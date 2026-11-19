#!/usr/bin/env python3
"""Strict native unsupported Alt-Tab capability regression. Own Xvfb only; never host desktop."""
import argparse, hashlib, json, os, secrets, socket, struct, tomllib
from pathlib import Path
import errno, shutil, signal, stat, subprocess, tempfile, time, traceback

EXPECTED_REASON='wayland-winit does not yet support a visible Alt-Tab panel; no window was switched'

def require_capability_log(text):
    if 'could not start compositor' in text or 'backend reported compositor transition' in text:
        raise AssertionError('old misleading compositor error occurred')
    return ('Error executing keyboard shortcut: "'+EXPECTED_REASON+'"') in text

def require_unchanged_clients(before, after):
    def state(rows):
        return sorted((r['id'],r['pid'],r['name'],r['is_focused'],r['is_on_view'],r['tags']) for r in rows)
    if state(before)!=state(after):raise AssertionError('client identity/focus/view changed')

class DeadlineError(RuntimeError): pass

# Dedicated runner display, outside Smithay's automatic 0..32 allocation range.
DISPLAY_NUMBER = '199'

def display_paths(number):
    return Path('/tmp/.X'+number+'-lock'), Path('/tmp/.X11-unix/X'+number)

def require_unused_display(number):
    lock, endpoint = display_paths(number)
    for path in (lock, endpoint):
        if os.path.lexists(path):
            raise RuntimeError('display already occupied: '+str(path))
    with socket.socket(socket.AF_UNIX) as peer:
        peer.settimeout(2)
        try:peer.connect('\0'+str(endpoint))
        except OSError as error:
            if error.errno not in (errno.ENOENT, errno.ECONNREFUSED):raise
        else:raise RuntimeError('display abstract endpoint already occupied')

def require_server_lock(path, pid, uid):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    try:
        info=os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid!=uid:
            raise RuntimeError('wrong Xvfb lock owner/type')
        raw=os.read(fd,32)
        if raw != f'{pid:>10}\n'.encode():
            raise RuntimeError('wrong Xvfb lock PID')
    finally:os.close(fd)

def edit_setting(text,section,key,value,required=True):
    lines=text.splitlines(); inside=False; changed=False
    for i,line in enumerate(lines):
        stripped=line.strip()
        if stripped.startswith('['): inside=stripped=='['+section+']'
        if inside and stripped.startswith(key+' ='):
            lines[i]=key+' = '+value; changed=True
    if required and not changed: raise RuntimeError('missing config: '+section+'.'+key)
    return '\n'.join(lines)+'\n'

def auth_file(path):
    fields=(b'',b'',b'MIT-MAGIC-COOKIE-1',secrets.token_bytes(16))
    data=struct.pack('!H',65535)+b''.join(struct.pack('!H',len(v))+v for v in fields)
    with path.open('xb') as f:
        os.chmod(path,0o600);f.write(data)

def matching_row(rows,pid,title):
    matches=[r for r in rows if r.get('pid')==pid and r.get('name')==title]
    if len(matches)>1:raise RuntimeError('ambiguous native client identity')
    return matches[0] if matches else None

def single_host_match(text):
    matches=text.splitlines()
    if len(matches)>1 or (matches and not matches[0].isdigit()):raise RuntimeError('ambiguous/malformed host window')
    return matches[0] if matches else None

def require_host_owner(identity,xid,pid):
    if identity!={'raw_xid':int(xid),'server_pid':pid,'source':'XResQueryClientIds'}:
        raise RuntimeError('wrong host window owner')

def observe_inactive(read, timeout=2, clock=time.monotonic, sleep=time.sleep):
    start=clock();samples=[];stable=0
    while clock()-start<timeout:
        status=read();samples.append({'seconds':clock()-start,'ui':status})
        stable=stable+1 if not status['active'] else 0
        sleep(.05)
    return samples,stable>=3

def wait(fn, label, timeout=8):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        try:
            value=fn()
            if value:return value
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
        time.sleep(.03)
    raise AssertionError(label+' timed out')

def keys(path):
    return path.read_text().splitlines() if path.exists() else []

def received(lines, code, pid):
    return any(len(p:=line.split())>=7 and p[0]==str(pid) and p[2]=='key' and p[5]==str(code) and 'Pressed' in p[6] for line in lines)

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--bin-dir',type=Path,required=True)
    p.add_argument('--client-bin',type=Path,required=True)
    p.add_argument('--source-root',type=Path,required=True)
    p.add_argument('--expected-source',required=True)
    p.add_argument('--build-receipt',type=Path,required=True)
    p.add_argument('--backend',choices=['wayland-winit'],default='wayland-winit')
    p.add_argument('--evidence-dir',type=Path,required=True)
    a=p.parse_args(); assert len(a.expected_source)==40 and all(c in '0123456789abcdef' for c in a.expected_source),'exact source SHA required'; out=a.evidence_dir.resolve();out.mkdir(parents=True,exist_ok=False)
    report={'status':'failed','backend':a.backend,'scope':'private Xvfb native Wayland unsupported Alt-Tab; no host-loss panel test','failures':[]}
    (out/'result.json').write_text(json.dumps(report))
    children=[]
    temp=tempfile.TemporaryDirectory(prefix='jwm-alttab-host-')
    tmp=temp.name
    work=Path(tmp)
    env={'PATH':'/usr/bin:/bin','LANG':'C.UTF-8','LC_ALL':'C.UTF-8'}
    for k,leaf in [('HOME','home'),('XDG_RUNTIME_DIR','runtime'),('XDG_CONFIG_HOME','config'),('XDG_STATE_HOME','state'),('XDG_CACHE_HOME','cache'),('XDG_DATA_HOME','data')]:
        (work/leaf).mkdir(mode=0o700);env[k]=str(work/leaf)
    env.update(LIBGL_ALWAYS_SOFTWARE='1',WINIT_UNIX_BACKEND='x11',WINIT_X11_SCALE_FACTOR='1',DBUS_SYSTEM_BUS_ADDRESS='unix:path='+str(work/'absent-bus'))
    log=(out/'session.log').open('w');extra_logs=[];xserver=None;sequence=0
    def verify_server():
        if xserver is None or xserver.poll() is not None:raise RuntimeError('owned Xvfb is not live')
        require_server_lock(display_paths(DISPLAY_NUMBER)[0],xserver.pid,os.getuid())
        with socket.socket(socket.AF_UNIX) as peer:
            peer.settimeout(2);peer.connect('/tmp/.X11-unix/X'+env['DISPLAY'][1:])
            pid,uid,_=struct.unpack('3i',peer.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
        if pid!=xserver.pid or uid!=os.getuid():raise RuntimeError('not owned Xvfb peer')
        if Path('/proc',str(pid),'exe').resolve()!=Path('/usr/bin/Xvfb').resolve():raise RuntimeError('wrong Xvfb executable')
    def run(*argv):
        nonlocal sequence
        sequence+=1;record={'argv':list(map(str,argv)),'status':'failed'}
        try:
            verify_server()
            r=subprocess.run(record['argv'],env=env,capture_output=True,text=True,timeout=8)
            record.update(stdout=r.stdout,stderr=r.stderr,returncode=r.returncode)
            verify_server()
            if r.returncode:raise RuntimeError('command failed: '+str(argv[0]))
            record['status']='passed';return r.stdout
        except BaseException as error:
            record['error']=repr(error);raise
        finally:(out/f'command-{sequence:03}.json').write_text(json.dumps(record,indent=2))
    def spawn(argv,child_env=None,**kw):
        c=subprocess.Popen(list(map(str,argv)),env=child_env or env,stdout=kw.pop('stdout',log),stderr=log,start_new_session=True,**kw);children.append(c);return c
    def msg(name,values=None):
        argv=[a.bin_dir.resolve()/'jwm-tool','msg',name,'--raw']
        if values is not None:argv+=['--args',json.dumps(values)]
        r=json.loads(run(*argv));assert r.get('success',True),r;return r.get('data',r)
    identities={}
    def row(pid):return matching_row(msg('get_windows'),pid,identities[pid])
    def ui():return msg('get_system_ui')
    def focus_host(xid):
        verify_host(xid,*host_owners[xid])
        run('xdotool','windowfocus',xid)
        wait(lambda:int(run('xdotool','getwindowfocus').strip())==int(xid),'X input focus acknowledgement')
        time.sleep(.15)
    def expired(signum, frame):
        raise DeadlineError('fixture deadline or termination signal')
    signal.signal(signal.SIGALRM, expired)
    signal.signal(signal.SIGTERM, expired)
    signal.alarm(180)
    try:
        report['phase']='setup'
        for tool in ('Xvfb','xdotool','xdpyinfo','dbus-daemon'):assert shutil.which(tool),'missing '+tool
        for binary in (a.bin_dir/'jwm',a.bin_dir/'jwm-tool',a.client_bin):
            assert binary.is_file() and os.access(binary,os.X_OK),str(binary)
        root=a.source_root.resolve(strict=True)
        genv={'PATH':'/usr/bin:/bin','HOME':'/nonexistent','GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null'}
        def git(*args):return subprocess.check_output(['/usr/bin/git','-c','core.fsmonitor=false','-c','core.hooksPath=/dev/null',*args],cwd=root,env=genv,text=True,timeout=8).strip()
        assert git('rev-parse','HEAD')==a.expected_source and not git('status','--porcelain','--untracked-files=all'),'source mismatch/dirty'
        assert a.bin_dir.resolve()==root/'target/release'
        assert a.client_bin.resolve()==root/'target/native/debug/examples/native_shell_client'
        receipt=json.loads(a.build_receipt.read_text());assert receipt['source']==a.expected_source and receipt['build_exit_code']==0
        for name,path in [('jwm',a.bin_dir/'jwm'),('jwm-tool',a.bin_dir/'jwm-tool'),('native_shell_client',a.client_bin)]:
            assert hashlib.sha256(path.read_bytes()).hexdigest()==receipt['binaries'][name],name+' digest mismatch'
        assert hashlib.sha256(Path(__file__).with_name('host_identity.py').read_bytes()).hexdigest()==receipt['host_identity_sha256']
        report['build_receipt']=receipt
        authority=work/'authority';auth_file(authority);env['XAUTHORITY']=str(authority)
        env['DISPLAY']=':'+DISPLAY_NUMBER
        report['display_setup']={'display':env['DISPLAY'],'mode':'explicit-server-created-lock'}
        require_unused_display(DISPLAY_NUMBER)
        xserver=spawn(['/usr/bin/Xvfb',env['DISPLAY'],'-auth',authority,'-screen','0','1280x800x24','-nolisten','tcp','-noreset'])
        def server_ready():
            if xserver.poll() is not None:raise RuntimeError('owned Xvfb exited during startup')
            if not all(os.path.lexists(path) for path in display_paths(DISPLAY_NUMBER)):return False
            verify_server();return True
        wait(server_ready,'owned Xvfb socket and server-created lock')
        verify_server()
        wrong=work/'wrong-authority';auth_file(wrong)
        denied=subprocess.run(['/usr/bin/xdpyinfo'],env={**env,'XAUTHORITY':str(wrong)},capture_output=True,text=True,timeout=8)
        verify_server();assert denied.returncode!=0,'Xvfb accepted wrong cookie'
        report['wrong_cookie_rc']=denied.returncode
        run('/usr/bin/xdpyinfo');report['owner']={'pid':xserver.pid,'uid':os.getuid(),'display':env['DISPLAY'],'peer_verified':True,'server_lock_verified':True}
        binary=a.bin_dir.resolve()/'jwm' 
        run(binary,'--backend',a.backend,'--gen-config')
        cfg=work/'config/jwm/config_wayland.toml';text=cfg.read_text()
        for section,key,value,required in [('status_bar','show_bar','false',True),('status_bar','name','"/bin/true"',True),('desktop_shell','enabled','false',False),('animation','enabled','false',True),('behavior','eva_companion_enabled','false',True)]:
            text=edit_setting(text,section,key,value,required)
        if 'desktop_shell' not in tomllib.loads(text):text+='\n[desktop_shell]\nenabled = false\n'
        assert tomllib.loads(text)['desktop_shell']['enabled'] is False
        (out/'config_wayland.toml').write_text(text)
        cfg.write_text(text);run(binary,'--backend',a.backend,'--check-config')
        buslog=(out/'bus-address.log').open('w');extra_logs.append(buslog)
        spawn(['/usr/bin/dbus-daemon','--session','--nofork','--print-address=1'],stdout=buslog)
        env['DBUS_SESSION_BUS_ADDRESS']=wait(lambda:(out/'bus-address.log').read_text().splitlines()[0] if (out/'bus-address.log').read_text().strip() else None,'owned session bus')
        wm=spawn([binary,'--backend',a.backend])
        def search(title):
            r=subprocess.run(['xdotool','search','--onlyvisible','--name',title],env=env,capture_output=True,text=True,timeout=3)
            return single_host_match(r.stdout if r.returncode==0 else '')
        host_owners={}
        def verify_host(xid,child,exe,label):
            assert child.poll() is None,'owned host child exited'
            identity=json.loads(run('/usr/bin/python3','-I',Path(__file__).with_name('host_identity.py'),xid))
            require_host_owner(identity,xid,child.pid)
            assert Path('/proc',str(child.pid),'exe').resolve()==Path(exe).resolve(),'wrong host executable'
            report.setdefault('host_identities',{})[label]=identity
        outer=wait(lambda:search('JWM '),'nested host output')
        verify_host(outer,wm,binary,'nested-output');host_owners[outer]=(wm,binary,'nested-output');focus_host(outer)
        run('xdotool','mousemove','0','0')
        wayland=wait(lambda:next((f.name for f in (work/'runtime').glob('wayland-*') if f.is_socket()),None),'native socket')
        clients=[]
        for title in ('HostFocusA','HostFocusB'):
            event_log=out/(title+'.events');nenv=dict(env,WAYLAND_DISPLAY=wayland,JWM_TEST_EVENT_LOG=str(event_log));nenv.pop('DISPLAY',None)
            c=spawn([a.client_bin.resolve(),'--title',title],nenv)
            identities[c.pid]=title
            wait(lambda:row(c.pid),'managed native client');clients.append((c,event_log))
        # No outside window or host-loss gesture: this backend has no panel renderer.
        focus_host(outer);first=clients[0]
        msg('focus_window',{'id':row(first[0].pid)['id']})
        wait(lambda:row(first[0].pid)['is_focused'],'A focused')
        report['phase']='native-input-control'
        initial=msg('get_windows');report['initial_windows']=initial
        assert ui()=={'active':False,'kind':None},'UI active before gesture'
        a0=len(keys(first[1]));b0=len(keys(clients[1][1]))
        run('xdotool','key','b')
        wait(lambda:received(keys(first[1])[a0:],48,first[0].pid),'initial native b receipt')
        assert not received(keys(clients[1][1])[b0:],48,clients[1][0].pid),'initial b leaked to B'
        report['native_key_control']=True
        report['phase']='unsupported-capability'
        event_offsets=[len(keys(c[1])) for c in clients]
        log_offset=(out/'session.log').stat().st_size
        timeline=[];report['timeline']=timeline
        report['receipt_offsets']={'initial_A':a0,'initial_B':b0}
        started=time.monotonic()
        def record(label):timeline.append({'stage':label,'elapsed_s':time.monotonic()-started})
        run('xdotool','keydown','Alt_L');record('Alt-down')
        run('xdotool','key','Tab');record('Tab')
        wait(lambda:require_capability_log((out/'session.log').read_bytes()[log_offset:].decode('utf-8',errors='replace')),'exact unsupported capability error')
        record('exact-error-observed')
        run('xdotool','keyup','Alt_L');record('Alt-up')
        samples,_=observe_inactive(ui,timeout=.5)
        report['ui_samples']=samples
        assert samples and all(x['ui']=={'active':False,'kind':None} for x in samples),'UI became active'
        after=msg('get_windows');report['windows_after_gesture']=after;require_unchanged_clients(initial,after)
        assert int(run('xdotool','getwindowfocus').strip())==int(outer),'host focus changed'
        # Do not issue any focus repair between the gesture and the fresh receipt.
        a1=len(keys(first[1]));b1=len(keys(clients[1][1]))
        run('xdotool','key','b')
        wait(lambda:received(keys(first[1])[a1:],48,first[0].pid),'post-rejection native b receipt')
        assert not received(keys(clients[1][1])[b1:],48,clients[1][0].pid),'post-rejection b leaked to B'
        for (child,path),offset in zip(clients,event_offsets):
            assert not any(' keyboard_enter ' in line or ' keyboard_leave ' in line for line in keys(path)[offset:]),'native focus changed'
        final=msg('get_windows');require_unchanged_clients(initial,final)
        assert int(run('xdotool','getwindowfocus').strip())==int(outer),'final host focus changed'
        delta=(out/'session.log').read_bytes()[log_offset:].decode('utf-8',errors='replace')
        assert require_capability_log(delta)
        (out/'capability-error.log').write_text(delta)
        record('post-rejection-b-observed')
        report.update(timeline=timeline,final_windows=final,exact_error=EXPECTED_REASON,
                      post_rejection_native_key=True,focus_unchanged=True,
                      receipt_offsets={'initial_A':a0,'initial_B':b0,'post_A':a1,'post_B':b1},
                      grab_scope='No XGrab probe; zero grab calls covered by production-interface unit tests, not this desktop run')
        report['status']='failure' if report['failures'] else 'success'
    except BaseException:
        report['failures'].append(traceback.format_exc());report['status']='failure'
    finally:
        signal.alarm(0)
        for child in reversed(children):
            try:
                if child.poll() is None:
                    try:
                        os.killpg(child.pid,signal.SIGTERM);child.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid,signal.SIGKILL);child.wait(timeout=3)
            except ProcessLookupError:pass
            except BaseException as error:report['failures'].append('cleanup: '+repr(error))
        for extra in extra_logs:
            try:extra.close()
            except BaseException as error:report['failures'].append('extra log cleanup: '+repr(error))
        try:log.close()
        except BaseException as error:report['failures'].append('log cleanup: '+repr(error))
        try:temp.cleanup()
        except BaseException as error:report['failures'].append('directory cleanup: '+repr(error))
        report['status']='failure' if report['failures'] else 'success'
        (out/'result.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2));return int(report['status']!='success')
if __name__=='__main__':raise SystemExit(main())
