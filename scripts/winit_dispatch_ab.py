#!/usr/bin/env python3
"""Single fixed baseline/product comparison. No retries or scheduler overrides."""
import argparse
import ctypes as C
import ctypes.util
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import time
import unittest

BASE = '04e76ac4e971efce03a1fe61ae1f4083585c2e22'
PRODUCT = '23fca55be3f57a9795c1e25dd99cc226b17128e9'

def proc_sample(pid):
    fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    assert fields[0] not in ('Z', 'X'), 'process no longer alive'
    return {'wall': time.monotonic(), 'ticks': int(fields[11])+int(fields[12]),
            'starttime': int(fields[19])}

def validate_idle(samples, hz):
    assert len(samples) == 11 and hz > 0, 'missing idle samples'
    assert len({s['starttime'] for s in samples}) == 1, 'PID reused'
    assert all(a['wall'] < b['wall'] and a['ticks'] <= b['ticks'] for a,b in zip(samples, samples[1:])), 'non-monotonic idle evidence'
    wall = samples[-1]['wall']-samples[0]['wall']
    assert wall >= 10, 'idle sample shorter than 10s'
    return 100*(samples[-1]['ticks']-samples[0]['ticks'])/hz/wall

def load_context():
    result = {'cpu_count': os.cpu_count(), 'wall': time.monotonic()}
    for key, path in [('cpu_pressure','/proc/pressure/cpu'),('loadavg','/proc/loadavg')]:
        try: result[key] = Path(path).read_text().strip()
        except OSError as error: result[key] = {'unavailable': str(error)}
    return result

def sample_idle(pid, record):
    record['load_before'] = load_context()
    samples = record.setdefault('samples', [])
    samples.append(proc_sample(pid))
    for _ in range(10):
        time.sleep(1)
        samples.append(proc_sample(pid))
    record['load_after'] = load_context()
    hz = os.sysconf('SC_CLK_TCK')
    record.update(clock_ticks_per_second=hz, one_core_cpu_percent=validate_idle(samples, hz), cadence='unmeasured')

def find_owned_jwm(wrapper, binary):
    todo = [wrapper.pid]
    matches = []
    while todo:
        pid = todo.pop()
        try:
            if Path(f'/proc/{pid}/exe').resolve() == binary.resolve(): matches.append(pid)
            todo.extend(map(int, Path(f'/proc/{pid}/task/{pid}/children').read_text().split()))
        except FileNotFoundError:
            continue
    assert len(matches) == 1, 'cannot identify unique owned JWM process'
    return matches[0]

class Data(C.Union):
    _fields_ = [('b', C.c_char*20), ('s', C.c_short*10), ('l', C.c_long*5)]
class ClientMessage(C.Structure):
    _fields_ = [('type', C.c_int), ('serial', C.c_ulong), ('send_event', C.c_int),
                ('display', C.c_void_p), ('window', C.c_ulong), ('message_type', C.c_ulong),
                ('format', C.c_int), ('data', Data)]
class Event(C.Union):
    _fields_ = [('client', ClientMessage), ('pad', C.c_long*24)]

def send_wm_delete(display_name, window):
    lib = C.CDLL(C.util.find_library('X11'))
    signatures = {'XOpenDisplay': ([C.c_char_p], C.c_void_p),
        'XInternAtom': ([C.c_void_p,C.c_char_p,C.c_int],C.c_ulong),
        'XGetWMProtocols': ([C.c_void_p,C.c_ulong,C.POINTER(C.POINTER(C.c_ulong)),C.POINTER(C.c_int)],C.c_int),
        'XSendEvent': ([C.c_void_p,C.c_ulong,C.c_int,C.c_long,C.POINTER(Event)],C.c_int),
        'XSync': ([C.c_void_p,C.c_int],C.c_int), 'XFree': ([C.c_void_p],C.c_int),
        'XCloseDisplay': ([C.c_void_p],C.c_int)}
    for name,(args,result) in signatures.items():
        fn=getattr(lib,name); fn.argtypes=args; fn.restype=result
    display=lib.XOpenDisplay(display_name.encode())
    assert display, 'cannot connect owned display'
    try:
        protocols=lib.XInternAtom(display,b'WM_PROTOCOLS',0)
        delete=lib.XInternAtom(display,b'WM_DELETE_WINDOW',0)
        atoms=C.POINTER(C.c_ulong)(); count=C.c_int()
        assert lib.XGetWMProtocols(display,window,C.byref(atoms),C.byref(count)), 'missing WM_PROTOCOLS'
        try: assert delete in [atoms[i] for i in range(count.value)], 'WM_DELETE_WINDOW unsupported'
        finally: lib.XFree(atoms)
        event=Event(); event.client.type=33; event.client.send_event=1
        event.client.display=display; event.client.window=window
        event.client.message_type=protocols; event.client.format=32
        event.client.data.l[0]=delete; event.client.data.l[1]=0
        started=time.monotonic()
        assert lib.XSendEvent(display,window,0,0,C.byref(event)), 'WM_DELETE send failed'
        lib.XSync(display,0)
        return started
    finally: lib.XCloseDisplay(display)

def validate_run(report, exit_code, candidate):
    assert exit_code in (0,1), 'fixture crashed or failed abnormally'
    assert exit_code == (0 if report.get('status')=='success' else 1), 'exit/report disagreement'
    assert report.get('status')=='success' or report.get('failure_class')=='behavior', 'nonbehavior baseline failure is not waivable'
    close = report.get('lifecycle',{}).get('close',{})
    assert close.get('exit_code')==0 and 0 <= close.get('seconds',float('inf')) <= 2, 'missing clean bounded host-close'
    assert close.get('protocol')=='WM_PROTOCOLS/WM_DELETE_WINDOW', 'close used wrong mechanism'
    idle = report.get('lifecycle',{}).get('idle',{})
    measured = validate_idle(idle.get('samples',[]), idle.get('clock_ticks_per_second',0))
    assert math.isclose(measured, idle.get('one_core_cpu_percent',float('nan')), rel_tol=1e-9, abs_tol=1e-9), 'idle CPU disagrees with raw samples'
    cases = report.get('cases',[])
    assert [case.get('case') for case in cases] == ['floating','pip','fullscreen'], 'missing/duplicate/out-of-order strict cases'
    assert all(case.get('frames') for case in cases), 'missing strict frames'
    lifecycle = report['lifecycle']
    assert lifecycle.get('windows_before') == [] and lifecycle.get('windows_after') == [], 'missing empty-client idle evidence'
    assert lifecycle.get('system_ui_before',{}).get('active') is False and lifecycle.get('system_ui_after',{}).get('active') is False, 'missing idle UI evidence'
    if candidate: assert exit_code==0 and report['status']=='success', 'candidate strict failed'

def assert_cpu_guard(baseline, candidate):
    assert candidate <= 25 and candidate <= baseline+5, 'candidate idle CPU exceeds fixed 25% or baseline+5pp guard'


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--baseline-bin',type=Path); p.add_argument('--product-bin',type=Path)
    p.add_argument('--client-bin',type=Path); p.add_argument('--output',type=Path)
    p.add_argument('--self-test',action='store_true')
    args=p.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Offline)).wasSuccessful() else 1
    assert all((args.baseline_bin,args.product_bin,args.client_bin,args.output)), 'missing paths'
    args.output.mkdir(parents=True,exist_ok=False)
    summary={'order':['baseline','product'],'runs':[],'failures':[], 'cadence':'unmeasured', 'cpu_gate':'candidate <=25% of one core and <=baseline+5 percentage points'}
    fixture=Path(__file__).with_name('test-terminal-exit-wayland.py')
    summary['harness_head']=subprocess.check_output(['git','-C',str(fixture.parent.parent),'rev-parse','HEAD'],text=True).strip()
    summary['fixture_sha256']=hashlib.sha256(fixture.read_bytes()).hexdigest()
    summary['driver_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    for label, revision, bindir in [('baseline',BASE,args.baseline_bin),('product',PRODUCT,args.product_bin)]:
        row={'label':label,'expected_revision':revision}; summary['runs'].append(row)
        try:
            # Build workflow writes this only after checkout HEAD verification/build.
            row['actual_revision']=(bindir/'REVISION').read_text().strip()
            assert row['actual_revision']==revision, 'build revision mismatch'
            row['binary_sha256']=hashlib.sha256((bindir/'jwm').read_bytes()).hexdigest()
            row['tool_sha256']=hashlib.sha256((bindir/'jwm-tool').read_bytes()).hexdigest()
            row['client_sha256']=hashlib.sha256(args.client_bin.read_bytes()).hexdigest()
            with (args.output/(label+'.log')).open('w') as log:
                result=subprocess.Popen(['python3',str(fixture),'--backend','wayland-winit',
                    '--dispatch-lifecycle','--diagnose','--bin-dir',str(bindir.resolve()),
                    '--client-bin',str(args.client_bin.resolve()),'--evidence-dir',str((args.output/label).resolve())],
                    stdout=log,stderr=subprocess.STDOUT)
                try:
                    result.wait(timeout=150)
                except subprocess.TimeoutExpired:
                    row['timed_out']=True
                    # Fixture's opt-in SIGTERM handler enters its existing finally,
                    # preserving frames/report and terminating owned child groups.
                    result.terminate()
                    try: result.wait(timeout=20)
                    except subprocess.TimeoutExpired:
                        result.kill(); result.wait(timeout=5)
                    raise
            row['exit_code']=result.returncode
            report=json.loads((args.output/label/'report.json').read_text())
            row['report']=str(args.output/label/'report.json')
            row['failure_class']=report.get('failure_class')
            row['legacy_status']=report.get('status')
            row['legacy_failures']=report.get('failures')
            row['idle']=report.get('lifecycle',{}).get('idle',{})
            validate_run(report,result.returncode,label=='product')
        except (OSError,ValueError,AssertionError,subprocess.SubprocessError) as error:
            row.setdefault('failure_class','fixture-or-build-identity'); row['error']=str(error); summary['failures'].append(label+': '+str(error))
            if row.get('timed_out') or isinstance(error, subprocess.TimeoutExpired):
                summary['incomplete'] = True
                summary['stopped_after_timeout'] = label
                break
    if len(summary['runs']) == 2 and all('idle' in run for run in summary['runs']):
        try:
            values=[validate_idle(run['idle'].get('samples',[]),run['idle'].get('clock_ticks_per_second',0)) for run in summary['runs']]
            summary['cpu_percentage_point_delta']=values[1]-values[0]
            assert_cpu_guard(*values)
        except (AssertionError,KeyError,TypeError) as error:
            summary['failures'].append('CPU evidence/guard: '+str(error))
    summary['status']='failure' if summary['failures'] else 'success'
    summary['meaning']='candidate gates plus usable comparison only; baseline strict verdict is preserved in its report. Sequential AB load can vary; CPU guard is not a statistical performance claim.'
    (args.output/'comparison.json').write_text(json.dumps(summary,indent=2)+'\n')
    return bool(summary['failures'])

class Offline(unittest.TestCase):
    def test_idle_rejects_pid_reuse_short_or_reverse_samples(self):
        good=[{'wall':float(i),'ticks':i,'starttime':1} for i in range(11)]
        self.assertEqual(validate_idle(good,100),1.)
        for field,value in [('starttime',2),('wall',-.1),('ticks',-1)]:
            bad=[dict(row) for row in good]; bad[5][field]=value
            with self.assertRaises(AssertionError): validate_idle(bad,100)
        with self.assertRaises(AssertionError): validate_idle(good[:10],100)
    def test_baseline_waiver_is_only_behavior_not_product_or_missing_evidence(self):
        report={'status':'failure','failure_class':'behavior','cases':[{'case':name,'frames':[1]} for name in ['floating','pip','fullscreen']], 'lifecycle':{
            'windows_before':[], 'windows_after':[], 'system_ui_before':{'active':False}, 'system_ui_after':{'active':False},
            'idle':{'samples':[{'wall':float(i),'ticks':i,'starttime':1} for i in range(11)],'clock_ticks_per_second':100,'one_core_cpu_percent':1.},'close':{'exit_code':0,'seconds':.1,'protocol':'WM_PROTOCOLS/WM_DELETE_WINDOW'}}}
        validate_run(report,1,False)
        last = report['cases'].pop()
        with self.assertRaises(AssertionError): validate_run(report,1,False)
        report['cases'].append(last)
        report['lifecycle']['idle']['one_core_cpu_percent'] = 0.
        with self.assertRaises(AssertionError): validate_run(report,1,False)
        report['lifecycle']['idle']['one_core_cpu_percent'] = 1.
        with self.assertRaises(AssertionError): validate_run(report,1,True)
        report['failure_class']='fixture'
        with self.assertRaises(AssertionError): validate_run(report,1,False)
        report['failure_class']='behavior'; report['lifecycle']['close']['seconds']=2.01
        with self.assertRaises(AssertionError): validate_run(report,1,False)
        report['lifecycle']['close']['seconds']=.1; report['lifecycle']['idle']={}
        with self.assertRaises(AssertionError): validate_run(report,1,False)
    def test_fixed_cpu_guard_never_adapts_to_outcome(self):
        assert_cpu_guard(20,25)
        for baseline,candidate in [(20,25.01),(0,5.01)]:
            with self.assertRaises(AssertionError): assert_cpu_guard(baseline,candidate)

    def test_client_message_abi_has_xevent_capacity(self):
        self.assertEqual(C.sizeof(Data),5*C.sizeof(C.c_long))
        self.assertEqual(C.sizeof(Event),24*C.sizeof(C.c_long))

if __name__=='__main__': raise SystemExit(main())
