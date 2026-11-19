import json
import os
import errno
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import probe

class Contracts(unittest.TestCase):
    def test_exact_capability_diagnostic_not_generic_error(self):
        exact='ERROR Error executing keyboard shortcut: "'+probe.EXPECTED_REASON+'"'
        self.assertTrue(probe.require_capability_log(exact))
        self.assertFalse(probe.require_capability_log('starting window_switcher'))
        for text in ('could not start compositor',exact+' backend reported compositor transition'):
            with self.assertRaises(AssertionError):probe.require_capability_log(text)
    def test_client_identity_and_focus_must_stay_unchanged(self):
        row=dict(id=1,pid=2,name='A',is_focused=True,is_on_view=True,tags=1)
        probe.require_unchanged_clients([row],[dict(row)])
        for key,value in [('id',3),('pid',3),('name','B'),('is_focused',False),('tags',2),('is_on_view',False)]:
            with self.assertRaises(AssertionError):probe.require_unchanged_clients([row],[dict(row,**{key:value})])
    def test_real_lock_contract_exact_pid_owner_and_type(self):
        with tempfile.TemporaryDirectory() as d:
            lock=Path(d)/'lock';lock.write_text(f'{os.getpid():>10}\n')
            probe.require_server_lock(lock,os.getpid(),os.getuid())
            for pid,uid in ((os.getpid()+1,os.getuid()),(os.getpid(),os.getuid()+1)):
                with self.assertRaises(RuntimeError):probe.require_server_lock(lock,pid,uid)
            link=Path(d)/'link';link.symlink_to(lock)
            with self.assertRaises(OSError):probe.require_server_lock(link,os.getpid(),os.getuid())
            fifo=Path(d)/'fifo';os.mkfifo(fifo)
            with self.assertRaises(RuntimeError):probe.require_server_lock(fifo,os.getpid(),os.getuid())
    def test_preexisting_display_paths_are_not_modified(self):
        with tempfile.TemporaryDirectory() as d:
            lock,endpoint=Path(d)/'lock',Path(d)/'socket'
            with patch.object(probe,'display_paths',return_value=(lock,endpoint)):
                for path in (lock,endpoint):
                    path.symlink_to(Path(d)/'missing')
                    with self.assertRaises(RuntimeError):probe.require_unused_display('199')
                    self.assertTrue(path.is_symlink());path.unlink()
    def test_abstract_collision_fails_closed(self):
        with patch.object(probe.os.path,'lexists',return_value=False),patch.object(probe.socket,'socket') as sock:
            with self.assertRaises(RuntimeError):probe.require_unused_display('199')
            sock.return_value.__enter__.return_value.connect.side_effect=OSError(errno.ECONNREFUSED,'absent')
            probe.require_unused_display('199')
            sock.return_value.__enter__.return_value.connect.side_effect=OSError(errno.EACCES,'denied')
            with self.assertRaises(OSError):probe.require_unused_display('199')
    def test_native_press_needs_pid_key_and_state(self):
        line='123 42 key 9 51 48 Value(Pressed)'
        self.assertTrue(probe.received([line],48,123))
        self.assertFalse(probe.received([line],48,124))
        self.assertFalse(probe.received([line],47,123))
        self.assertFalse(probe.received([line.replace('Pressed','Released')],48,123))
        self.assertFalse(probe.received(['123 42 keyboard_enter 9'],48,123))
    def test_deadline_is_not_swallowed_as_retryable_oserror(self):
        def fail():raise probe.DeadlineError('stop')
        with self.assertRaises(probe.DeadlineError):probe.wait(fail,'deadline')
    def test_identity_is_unique_and_pid_title_both_required(self):
        row={'pid':123,'id':4,'name':'HostFocusA'}
        self.assertEqual(probe.matching_row([row],123,'HostFocusA'),row)
        self.assertIsNone(probe.matching_row([row],124,'HostFocusA'))
        self.assertIsNone(probe.matching_row([row],123,'wrong'))
        with self.assertRaises(RuntimeError):probe.matching_row([row,row],123,'HostFocusA')
    def test_host_identity_is_server_attested_exact_pid_and_xid(self):
        identity={'raw_xid':42,'server_pid':123,'source':'XResQueryClientIds'}
        probe.require_host_owner(identity,'42',123)
        for bad in ({**identity,'server_pid':124},{**identity,'raw_xid':43},{**identity,'source':'WM_PID_property'}):
            with self.assertRaises(RuntimeError):probe.require_host_owner(bad,'42',123)
    def test_host_window_search_requires_unique_numeric_match(self):
        self.assertEqual(probe.single_host_match('42\n'),'42')
        self.assertIsNone(probe.single_host_match(''))
        for text in ('42\n43\n','garbage\n'):
            with self.assertRaises(RuntimeError):probe.single_host_match(text)
    def test_transient_active_is_not_called_persistent_residual(self):
        now=[0.0];states=iter([True,True,False,False,False])
        def sleep(dt):now[0]+=dt
        samples,ok=probe.observe_inactive(lambda:{'active':next(states)},timeout=.25,clock=lambda:now[0],sleep=sleep)
        self.assertTrue(ok);self.assertEqual(len(samples),5)
        self.assertEqual([s['ui']['active'] for s in samples],[True,True,False,False,False])
    def test_persistent_active_has_bounded_failure_evidence(self):
        now=[0.0]
        def sleep(dt):now[0]+=dt
        samples,ok=probe.observe_inactive(lambda:{'active':True},timeout=.2,clock=lambda:now[0],sleep=sleep)
        self.assertFalse(ok);self.assertTrue(samples);self.assertLessEqual(now[0],.25)
    def test_auth_private_random(self):
        with tempfile.TemporaryDirectory() as d:
            a,b=Path(d)/'a',Path(d)/'b';probe.auth_file(a);probe.auth_file(b)
            self.assertEqual(a.stat().st_mode&0o777,0o600)
            self.assertNotEqual(a.read_bytes(),b.read_bytes())
            self.assertIn(b'MIT-MAGIC-COOKIE-1',a.read_bytes())
    def test_config_requires_existing_required_field(self):
        self.assertIn('show_bar = false',probe.edit_setting('[status_bar]\nshow_bar = true\n','status_bar','show_bar','false'))
        with self.assertRaises(RuntimeError):probe.edit_setting('[x]\n','status_bar','show_bar','false')
    def test_missing_dependency_produces_failure_not_reproduction(self):
        with tempfile.TemporaryDirectory() as d:
            out=Path(d)/'evidence'
            argv=['probe','--expected-source','0'*40,'--source-root',d,'--build-receipt',d+'/missing','--bin-dir',d,
                  '--client-bin',d+'/missing','--evidence-dir',str(out)]
            with patch('sys.argv',argv),patch.object(probe.shutil,'which',return_value=None),patch('builtins.print'):
                self.assertEqual(probe.main(),1)
            r=json.loads((out/'result.json').read_text())
            self.assertEqual(r['status'],'failure');self.assertEqual(r['phase'],'setup')
            self.assertNotIn('normal_release_control',r)

if __name__=='__main__':unittest.main()
