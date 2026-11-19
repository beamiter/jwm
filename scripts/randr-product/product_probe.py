#!/usr/bin/env python3
"""UNPUBLISHED real X11RB baseline/fixed probe. Run only on disposable CI.
Requires clean exact source checkouts whose binaries were built in those trees.
Not executable in the local AF_UNIX-restricted environment; no skips as passes.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import subprocess
import tempfile
import time
import tomllib
import owned_xorg

BASE = '687cebf36585c7baeeb6850d851a1b8fef017865'
FIXED = '3dbca462cadf37c005261b04195a50b409210084'
A_TAG, B_TAG = 4, 64

def require(value, message):
    if not value: raise RuntimeError(message)


def wait(probe, label, timeout=10):
    end = time.monotonic()+timeout
    while time.monotonic()<end:
        value = probe()
        if value: return value
        time.sleep(.05)
    raise RuntimeError('timeout: '+label)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def randr(server, out, label, positions):
    # Coordinates are a tested part of the contract, not inferred from xrandr success.
    rows = server.xr('--verbose')
    monitors = server.xr('--listactivemonitors')
    (out/(label+'.randr')).write_text(rows)
    (out/(label+'.active-monitors')).write_text(monitors)
    ids = {}
    for block in re.split(r'(?m)(?=^\S+ (?:connected|disconnected))', rows)[1:]:
        name=block.split()[0]
        match=re.search(r'(?m)^\s*Identifier:\s*(0x[\da-fA-F]+)',block)
        if match and name in ('DUMMY0','DUMMY1'): ids[name]=int(match[1],16)
    require(len(ids)==2 and len(set(ids.values()))==2 and min(ids.values())>0,'real output identity prerequisite')
    active={}
    for line in monitors.splitlines()[1:]:
        tokens=line.split()
        require(tokens[-1] in ('DUMMY0','DUMMY1'),'unbacked or unexpected monitor')
        geom=re.search(r'(\d+)/\d+x(\d+)/\d+\+(\d+)\+(\d+)',line)
        require(geom,'unparsed monitor geometry')
        w,h,x,y=map(int,geom.groups())
        require(tokens[-1] not in active,'duplicate active output')
        active[tokens[-1]]=(x,y,w,h)
    require(active==positions,f'RandR positions {active} != {positions}')
    return ids


def checked_source(root, expected):
    # No checkout source is imported/executed here. Fixed system Git only.
    root=root.resolve(strict=True)
    env={'PATH':'/usr/bin:/bin','HOME':'/nonexistent','LANG':'C',
         'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null'}
    def git(*args):
        return subprocess.check_output(['/usr/bin/git','-c','core.fsmonitor=false',
                                        '-c','core.hooksPath=/dev/null',*args],cwd=root,
                                       env=env,text=True,timeout=10).strip()
    require(Path(git('rev-parse','--show-toplevel')).resolve()==root,'not exact checkout root')
    require(git('rev-parse','HEAD')==expected,'wrong source revision')
    require(not git('status','--porcelain','--untracked-files=all'),'source tree not clean')
    return root


def ipc_message(path,message):
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
        client.settimeout(3); client.connect(str(path))
        client.sendall(json.dumps(message,separators=(',',':')).encode()+b'\n')
        frame=bytearray()
        while b'\n' not in frame:
            part=client.recv(4096)
            require(part,'IPC closed before complete response'); frame.extend(part)
            require(len(frame)<=1024*1024,'IPC frame exceeds limit')
    result=json.loads(frame.split(b'\n',1)[0])
    require(result.get('success'),f'IPC error: {result.get("error")}')
    return result


def edit_setting(text,section,key,literal,required=True):
    lines=text.splitlines(); inside=False; changed=False
    for index,line in enumerate(lines):
        clean=line.strip()
        if clean.startswith('['): inside=clean=='['+section+']'
        if inside and clean.startswith(key+' ='):
            lines[index]=key+' = '+literal; changed=True
    require(changed or not required,'missing config setting '+section+'.'+key)
    return '\n'.join(lines)+'\n'


def layout_name(value):
    names={'LayoutEnum("tile")':'tile','LayoutEnum("float")':'float'}
    require(value in names,'unexpected exact layout serialization: '+repr(value))
    return names[value]


def match_ipc_client(rows,pid,title):
    matches=[w for w in rows if w.get('pid')==pid and
             all(w.get(k)==title for k in ('name','class','instance'))]
    require(len(matches)<=1,'ambiguous IPC client identity')
    if not matches: return None
    require(isinstance(matches[0].get('id'),int) and matches[0]['id']>0,'invalid IPC window ID')
    return matches[0]


def native_properties(text,pid,title):
    expected={'WM_NAME(STRING)':f'"{title}"',
              'WM_CLASS(STRING)':f'"{title}", "{title}"',
              '_NET_WM_PID(CARDINAL)':str(pid)}
    rows={}
    for line in text.splitlines():
        key,sep,value=line.partition(' = ')
        require(sep and key not in rows,'malformed or duplicate native property')
        rows[key]=value
    require(rows==expected,'native XID property identity mismatch')


def key_events(path,raw_xid):
    data=path.read_bytes()
    if data and not data.endswith(b'\n'): return None
    events=[json.loads(line) for line in data.splitlines()]
    for event in events:
        require(set(event)=={'window','send_event','text_hex'} and event['window']==raw_xid
                and event['send_event']==0,'receipt from unexpected native window/event')
        require(isinstance(event['text_hex'],str) and re.fullmatch(r'(?:[0-9a-f]{2})+',event['text_hex']),
                'invalid input receipt encoding')
    return events


def event_text(events):
    return b''.join(bytes.fromhex(e['text_hex']) for e in events)


class Session:
    def __init__(self, root, client, out, work, expected, same_tag, build):
        self.root=checked_source(root,expected)
        self.client,self.out,self.work=client,out,work
        self.build=build
        self.expected,self.same_tag=expected,same_tag
        self.children=[]; self.logs=[]; self.counter=0
        self.server=None; self.wm=None; self.phase='setup'
        self.bin=root/'target/release/jwm'
        self.receipts={}; self.windows={}; self.raw_xids={}; self.client_identities={}; self.client_processes={}

    def save(self, name, data):
        (self.out/(name+'.json')).write_text(json.dumps(data,indent=2))

    def run_command(self,args,env=None):
        return self.server.command([str(x) for x in args],env=env)

    def spawn(self,args,label):
        self.server.verify_owner()
        log=open(self.out/(label+'.log'),'xb'); self.logs.append(log)
        child=subprocess.Popen([str(x) for x in args],env=self.server.env,stdin=subprocess.DEVNULL,
                               stdout=log,stderr=log,start_new_session=True)
        self.children.append(child); return child

    def ipc(self,name,args=None):
        self.server.verify_owner()
        require(self.wm.poll() is None,'JWM process exited')
        self.counter+=1
        msg={'query':name,'args':{}} if args is None else {'command':name,'args':args}
        result=ipc_message(self.work/'runtime/jwm-ipc.sock',msg)
        self.save(f'ipc-{self.counter:03}',{'request':msg,'response':result})
        return result.get('data')

    def snapshot(self,label):
        result={q:self.ipc(q) for q in ('get_monitors','get_workspaces','get_windows')}
        result['input_focus']=int(self.run_command(['/usr/bin/xdotool','getwindowfocus']).stdout.strip())
        self.save(label,result); return result

    def monitors(self): return self.ipc('get_monitors')

    def select(self,name):
        monitors=self.monitors()
        target=[m for m in monitors if m['name']==name]
        require(len(target)==1,'target output missing')
        if not target[0]['focused']: self.ipc('focusmon',{'value':1})
        wait(lambda:any(m['name']==name and m['focused'] for m in self.monitors()),'focus target output')

    def verify_client(self,letter):
        require(self.client_processes[letter].poll() is None,'owned native client exited '+letter)
        identity=self.client_identities[letter]
        raw=self.raw_xids[letter]
        props=self.run_command(['/usr/bin/xprop','-id',str(raw),'WM_NAME','WM_CLASS','_NET_WM_PID']).stdout
        native_properties(props,identity['pid'],identity['title'])
        row=match_ipc_client(self.ipc('get_windows'),identity['pid'],identity['title'])
        require(row and row['id']==self.windows[letter],'IPC identity mapping changed')
        self.save(f'identity-{letter}-{self.counter:03}',{**identity,'raw_xid':raw,'ipc_id':row['id'],
                                                        'native_properties':props,'ipc_row':row})

    def input_receipt(self,label):
        # No focus command here: native recipient is checked against native XID,
        # whereas selected_id and every IPC action use the separate abstract ID.
        for letter in self.windows: self.verify_client(letter)
        focus=int(self.run_command(['/usr/bin/xdotool','getwindowfocus']).stdout.strip())
        require(focus==self.raw_xids['B'],'X input focus is not native survivor B')
        before={k:key_events(p,self.raw_xids[k]) for k,p in self.receipts.items()}
        require(all(v is not None for v in before.values()),'partial pre-input receipt')
        token=('probe'+secrets.token_hex(8)).encode()
        self.run_command(['/usr/bin/xdotool','type','--clearmodifiers','--delay','2',token.decode()])
        def delivered():
            events=key_events(self.receipts['B'],self.raw_xids['B'])
            return events is not None and event_text(events)==event_text(before['B'])+token
        wait(delivered,'actual native key receipt B'); time.sleep(.1)
        after={k:key_events(p,self.raw_xids[k]) for k,p in self.receipts.items()}
        require(after['A']==before['A'] and after['B'] is not None and
                event_text(after['B'])==event_text(before['B'])+token,'unexpected key recipient')
        self.save(label,{'focus_raw_xid':focus,'expected_raw_xid':self.raw_xids['B'],
                        'expected_ipc_id':self.windows['B'],'token':token.decode(),'before':before,'after':after})

    def start(self):
        checked_source(self.root,self.expected)
        head=self.expected
        require(self.build['source']==head and self.build['exit_code']==0,'missing successful exact build')
        require(Path(self.build['root'])==self.root and Path(self.build['binary'])==self.bin,'build path mismatch')
        require(self.bin.is_file() and digest(self.bin)==self.build['binary_sha256'],'built binary changed')
        require(digest(self.client)==self.build['client_sha256'],'fixture binary changed')
        self.save('identity',{'source':head,'binary_sha256':digest(self.bin),'client_sha256':digest(self.client),
                              'same_tag':self.same_tag})
        self.server=owned_xorg.OwnedXorg(self.work,self.out,Path('/usr/lib/xorg/Xorg').resolve(strict=True),
                                       Path(__file__).with_name('xorg.conf'))
        self.server.deadline=time.monotonic()+180
        self.server.start()
        self.server.xr('--addmode','DUMMY1','1280x720')
        self.server.xr('--output','DUMMY0','--mode','1280x720','--pos','0x0',
                       '--output','DUMMY1','--mode','1280x720','--pos','1280x0')
        self.ids=randr(self.server,self.out,'initial',{'DUMMY0':(0,0,1280,720),'DUMMY1':(1280,0,1280,720)})
        self.phase='JWM-setup'
        self.run_command([self.bin,'--backend','x11rb','--gen-config'])
        config=self.work/'config/jwm/config_x11.toml'; text=config.read_text()
        for section,key,value,required in [
            ('status_bar','show_bar','false',True),('status_bar','name','"/bin/true"',True),
            ('desktop_shell','enabled','false',False),('behavior','eva_companion_enabled','false',True),('behavior','compositor','false',True),
            ('behavior','focus_follows_new_window','true',True),
            ('behavior','wallpaper_effect','"off"',False),('animation','enabled','false',True)]:
            text=edit_setting(text,section,key,value,required=required)
        if 'desktop_shell' not in tomllib.loads(text): text+='\n[desktop_shell]\nenabled = false\n'
        require(tomllib.loads(text)['desktop_shell']['enabled'] is False,'companion shell not explicitly disabled')
        config.write_text(text); (self.out/'config_x11.toml').write_text(text)
        self.run_command([self.bin,'--backend','x11rb','--check-config'])
        self.wm=self.spawn(['/usr/bin/dbus-run-session','--',self.bin,'--backend','x11rb'],'jwm')
        wait(lambda:(self.work/'runtime/jwm-ipc.sock').exists(),'JWM IPC socket',30)
        wait(lambda:len(self.monitors())==2,'JWM two outputs')
        for m in self.monitors():
            require(m['output_id']==self.ids[m['name']],'IPC output not mapped to real RandR XID')
            require((m['x'],m['y'],m['w'],m['h'])==({'DUMMY0':(0,0,1280,720),'DUMMY1':(1280,0,1280,720)}[m['name']]),'JWM initial geometry')
        self.phase='fixture-setup'
        for letter,output,tag,layout,nmaster in [('A','DUMMY0',A_TAG,'tile',2),('B','DUMMY1',B_TAG,'float',4)]:
            self.select(output); self.ipc('view',{'tag':tag}); self.ipc('setlayout',{'layout':layout}); self.ipc('setnmaster',{'value':nmaster})
            ready=self.work/(letter+'.ready'); receipt=self.out/(letter+'.keys')
            title='org.jwm.randrfixture'+letter.lower()
            child=self.spawn([self.client,title,receipt,ready],letter)
            self.client_processes[letter]=child
            wait(lambda:ready.exists() and ready.stat().st_size>0,'client ready '+letter)
            xid,pid=map(int,ready.read_text().split()); require(pid==child.pid,'fixture PID mismatch')
            self.raw_xids[letter]=xid; self.receipts[letter]=receipt
            self.client_identities[letter]={'pid':pid,'title':title}
            native=self.run_command(['/usr/bin/xprop','-id',str(xid),'WM_NAME','WM_CLASS','_NET_WM_PID']).stdout
            native_properties(native,pid,title)
            row=wait(lambda:match_ipc_client(self.ipc('get_windows'),pid,title),'managed client '+letter)
            self.windows[letter]=row['id']
            self.save(letter+'-binding',{'raw_ready':ready.read_text(),'raw_xid':xid,'ipc_id':row['id'],
                                        'child_pid':child.pid,'native_properties':native,'ipc_row':row})
            rows=[row]
            monitor=next(m for m in self.monitors() if m['name']==output)
            require(rows[0]['monitor']==monitor['num'] and rows[0]['tags']==tag,'client setup placement')
            if letter=='A' and self.same_tag:
                self.ipc('focus_window',{'id':self.windows[letter]}); self.ipc('tag',{'tag':B_TAG})
        self.run_command(['/usr/bin/xdotool','mousemove','1500','300'])
        self.ipc('focus_window',{'id':self.windows['B']})
        wait(lambda:any(w['id']==self.windows['B'] and w['is_focused'] for w in self.ipc('get_windows')),'B IPC focus')
        self.before=self.snapshot('before')
        self.b=next(m for m in self.before['get_monitors'] if m['name']=='DUMMY1')
        a=next(m for m in self.before['get_monitors'] if m['name']=='DUMMY0')
        require(a['num']==0 and self.b['num']==1,'fixture requires A first, B second in initial protocol order')
        require((a['active_tags'],layout_name(a['layout']),a['n_master'])==(A_TAG,'tile',2),'A setup contract')
        require((self.b['active_tags'],layout_name(self.b['layout']),self.b['n_master'])==(B_TAG,'float',4),'B setup contract')
        require(self.b['selected_id']==self.windows['B'] and self.b['focused'],'B setup selection')
        self.b_workspace=next(w for w in self.before['get_workspaces'] if w['monitor']==self.b['num'] and w['tag_mask']==B_TAG)
        require((layout_name(self.b_workspace['layout']),self.b_workspace['n_master'])==('float',4),'B per-tag setup')
        self.input_receipt('before-input')

    def differences(self,snapshot):
        monitors=snapshot['get_monitors']; windows=snapshot['get_windows']; reasons=[]
        b=next(m for m in monitors if m['name']=='DUMMY1')
        for key in ('num','active_tags','layout','n_master','focused','selected_id'):
            if b[key]!=self.b[key]: reasons.append({'field':key,'expected':self.b[key],'actual':b[key]})
        for letter,xid in self.windows.items():
            rows=[w for w in windows if w['id']==xid]
            if len(rows)!=1: reasons.append({'field':'window-'+letter,'expected':'exactly once','actual':len(rows)})
            elif rows[0]['monitor']!=self.b['num']: reasons.append({'field':'monitor-'+letter,'expected':self.b['num'],'actual':rows[0]['monitor']})
        for letter,xid in self.windows.items():
            rows=[w for w in windows if w['id']==xid]
            if len(rows)==1:
                for key,expected in [('tags',B_TAG if letter=='B' or self.same_tag else A_TAG),
                                     ('is_on_view',letter=='B' or self.same_tag),
                                     ('is_focused',letter=='B')]:
                    if rows[0][key]!=expected: reasons.append({'field':key+'-'+letter,'expected':expected,'actual':rows[0][key]})
        spaces=[w for w in snapshot['get_workspaces'] if w['monitor']==self.b['num'] and w['tag_mask']==B_TAG]
        if len(spaces)!=1: reasons.append({'field':'B-workspace','expected':'exactly once','actual':len(spaces)})
        else:
            for key in ('layout','n_master','m_fact','gap'):
                if spaces[0][key]!=self.b_workspace[key]: reasons.append({'field':'workspace-'+key,'expected':self.b_workspace[key],'actual':spaces[0][key]})
        if snapshot['input_focus']!=self.raw_xids['B']: reasons.append({'field':'input_focus','expected':self.raw_xids['B'],'actual':snapshot['input_focus']})
        return reasons

    def probe(self):
        self.start(); self.phase='A-off'
        self.server.xr('--output','DUMMY0','--off','--output','DUMMY1','--mode','1280x720','--pos','0x0')
        require(randr(self.server,self.out,'A-off',{'DUMMY1':(0,0,1280,720)})==self.ids,'RandR identity changed')
        wait(lambda:len(self.monitors())==1,'JWM real removal event')
        time.sleep(.2)
        for letter in self.windows: self.verify_client(letter)
        after=self.snapshot('after-removal')
        b=after['get_monitors'][0]
        require(b['name']=='DUMMY1' and b['output_id']==self.ids['DUMMY1'],'real event mapping not established')
        require((b['x'],b['y'],b['w'],b['h'])==(0,0,1280,720),'real removal geometry not established')
        require(all(len([w for w in after['get_windows'] if w['id']==xid])==1 for xid in self.windows.values()),'client disappearance is not the expected baseline reproduction')
        differences=self.differences(after); self.save('removal-differences',differences)
        if self.expected==BASE:
            # Only the original survivor state/key deviation counts as baseline reproduction.
            require(any(d['field'] in ('num','active_tags','layout','n_master') for d in differences),
                    'baseline did not reproduce expected survivor-state bug')
            return {'status':'passed','result':'baseline-reproduced','differences':differences}
        require(not differences,'fixed survivor-state regression: '+json.dumps(differences))
        self.input_receipt('after-removal-input')
        self.phase='A-readded'
        # B remains at origin; A returns to the right to prevent pointer targeting A.
        self.server.xr('--output','DUMMY0','--mode','1280x720','--pos','1280x0',
                       '--output','DUMMY1','--mode','1280x720','--pos','0x0')
        require(randr(self.server,self.out,'restored',{'DUMMY0':(1280,0,1280,720),'DUMMY1':(0,0,1280,720)})==self.ids,'restored output ID changed')
        wait(lambda:len(self.monitors())==2,'JWM real add event'); time.sleep(.2)
        restored=self.snapshot('after-readd')
        require(len({m['num'] for m in restored['get_monitors']})==2,'duplicate protocol monitor numbers')
        for m in restored['get_monitors']:
            require(m['output_id']==self.ids[m['name']],'readd output mapping')
            require((m['x'],m['y'],m['w'],m['h'])==({'DUMMY0':(1280,0,1280,720),'DUMMY1':(0,0,1280,720)}[m['name']]),'readd JWM geometry')
        require(not self.differences(restored),'survivor changed after output readd')
        self.input_receipt('after-readd-input')
        return {'status':'passed','result':'fixed-real-events-passed'}

    def close(self):
        errors=[]
        for child in reversed(self.children):
            try:
                if child.poll() is None:
                    os.killpg(child.pid,signal.SIGTERM)
                    try: child.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid,signal.SIGKILL); child.wait(timeout=3)
            except BaseException as e: errors.append(repr(e))
        for log in self.logs:
            try: log.close()
            except BaseException as e: errors.append(repr(e))
        if self.server:
            try: self.server.close()
            except BaseException as e: errors.append(repr(e))
        require(not errors,'cleanup errors '+repr(errors))


def execute(root,client,out,sha,same_tag,build):
    out.mkdir(mode=0o700)
    report={'status':'failed','source':sha,'same_tag':same_tag,'errors':[]}
    (out/'result.json').write_text(json.dumps(report))
    session=None; candidate=None; tmp=None
    try:
        tmp=tempfile.TemporaryDirectory(prefix='jwm-randr-product-')
        session=Session(root,client,out,Path(tmp.name),sha,same_tag,build)
        candidate=session.probe()
    except BaseException as e:
        report['errors'].append(owned_xorg.error_detail(e,session.phase if session else 'setup',session.server if session else None))
    finally:
        if session is not None:
            try: session.close()
            except BaseException as e:
                report['errors'].append(owned_xorg.error_detail(e,'session-cleanup',session.server))
        if tmp is not None:
            try: tmp.cleanup()
            except BaseException as e:
                report['errors'].append(owned_xorg.error_detail(e,'directory-cleanup'))
        if candidate and not report['errors']: report.update(candidate)
        (out/'result.json').write_text(json.dumps(report,indent=2))

    return report


def run_pair(builds,client,out):
    # Only the reviewed same-process build_and_probe orchestrator calls this.
    reports=[]
    for label,sha in [('baseline',BASE),('fixed',FIXED)]:
        build=builds[label]
        for same in (False,True):
            report=execute(Path(build['root']),client,out/(label+('-same-tag' if same else '-different-tags')),sha,same,build)
            reports.append(report)
            (out/'summary.json').write_text(json.dumps(reports,indent=2))
            if report['status']!='passed': return 1
    return 0


if __name__=='__main__':
    raise SystemExit('Use the reviewed build_and_probe.py orchestrator; prebuilt binaries are not accepted.')
