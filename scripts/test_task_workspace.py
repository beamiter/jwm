import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import task_workspace as tw


class TaskTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.task = dict(version=1, id='rpl', title='RPL debug', repository=str(self.root),
                         workspace=dict(connector='DP-1', tag_mask=4), resources=[])
        self.ws = [dict(connector='DP-1', tag_mask=1, monitor=0, focused=True),
                   dict(connector='DP-1', tag_mask=4, monitor=0, focused=False)]

    def state(self, workspaces=None, identity='process-generation-output'):
        return dict(version=1, workspaces=self.ws if workspaces is None else workspaces,
                    monitor_identity=identity)

    def test_import_durable_private_no_overwrite(self):
        dest = tw.save_new(self.task, self.root / 'tasks')
        self.assertEqual(tw.load(dest), self.task)
        self.assertEqual(dest.stat().st_mode & 0o777, 0o600)
        with self.assertRaises(FileExistsError):
            tw.save_new(self.task, self.root / 'tasks')

    def test_reject_unknown_version_fields_path_and_mask(self):
        for key, value in [('version', 2), ('version', True), ('id', '../oops'), ('repository', 'relative')]:
            bad = copy.deepcopy(self.task)
            bad[key] = value
            with self.assertRaises(ValueError):
                tw.validate(bad)
        for mask in (0, 3, -1, True, 2**32):
            bad = copy.deepcopy(self.task)
            bad['workspace']['tag_mask'] = mask
            with self.assertRaises(ValueError):
                tw.validate(bad)
        bad = dict(self.task, command='rm -rf /')
        with self.assertRaises(ValueError):
            tw.validate(bad)

    def test_store_symlink_and_public_dir_rejected(self):
        link = self.root / 'link'
        link.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(ValueError):
            tw.save_new(self.task, link)
        public = self.root / 'public'
        public.mkdir(mode=0o755)
        public.chmod(0o755)
        with self.assertRaises(ValueError):
            tw.save_new(self.task, public)

    def test_bounded_json_and_duplicate_keys(self):
        f = self.root / 'bad'
        for value in ('{"version":1,"version":2}', 'x' * (tw.LIMIT + 1)):
            f.write_text(value)
            with self.assertRaises(ValueError):
                tw.load(f)

    def test_plan_only_no_subprocess(self):
        with patch.object(tw.subprocess, 'run', side_effect=AssertionError('not allowed')):
            p = tw.plan(self.task, self.state())
        self.assertEqual(p['blockers'], [])
        self.assertEqual(p['action']['command'], 'task_workspace_view')
        self.assertEqual(p['action']['args']['expected_tag_mask'], 1)
        self.assertEqual(p['action']['args']['tag_mask'], 4)

    def test_missing_ambiguous_monitor_and_repo_block(self):
        for state in ([], self.ws + [self.ws[1]], [dict(self.ws[0], connector='DP-2'), self.ws[1]]):
            self.assertTrue(tw.plan(self.task, self.state(state))['blockers'])
        self.task['repository'] += '/missing'
        self.assertIn('repository_missing', tw.plan(self.task, self.state())['blockers'])

    def test_successful_resume_and_no_launch(self):
        plan = tw.plan(self.task, self.state())
        calls = []
        def ipc(cmd, args=None):
            calls.append((cmd, args))
            if cmd == 'get_task_workspace_state':
                return self.state() if len(calls) == 1 else self.state([dict(self.ws[1], focused=True)])
        result = tw.resume(self.task, plan['approval_digest'], ipc)
        self.assertEqual(result['status'], 'workspace_selected')
        self.assertEqual(calls, [('get_task_workspace_state', None), ('task_workspace_view', plan['action']['args']), ('get_task_workspace_state', None)])

    def test_changed_plan_never_writes(self):
        approved = tw.plan(self.task, self.state())['approval_digest']
        self.task['title'] = 'different'
        calls = []
        def ipc(cmd, args=None):
            calls.append(cmd)
            return self.state()
        with self.assertRaises(ValueError):
            tw.resume(self.task, approved, ipc)
        self.assertEqual(calls, ['get_task_workspace_state'])

    def test_unconfirmed_view_not_retried(self):
        approved = tw.plan(self.task, self.state())['approval_digest']
        calls = []
        def ipc(cmd, args=None):
            calls.append(cmd)
            return self.state()
        with self.assertRaisesRegex(ValueError, 'not confirmed'):
            tw.resume(self.task, approved, ipc)
        self.assertEqual(calls.count('task_workspace_view'), 1)

    def test_terminal_reference_never_means_live(self):
        f = self.root / 'sessions.json'
        r = dict(kind='terminal', label='train', reference='123-456-7', terminal='ember', snapshot=str(f))
        self.task['resources'] = [r]
        f.write_text(json.dumps(dict(version=4, sessions=[dict(session_id=r['reference'])])))
        self.assertEqual(tw.plan(self.task, self.state())['resources'][0]['status'], 'snapshot_reference_found')
        f.write_text(json.dumps(dict(version=99, sessions=[])))
        self.assertEqual(tw.resource_status(r)['status'], 'snapshot_unavailable_or_unsupported')
        f.write_text(json.dumps(dict(version=4, sessions=[dict(session_id=r['reference'])] * 2)))
        self.assertEqual(tw.resource_status(r)['status'], 'ambiguous')

    def test_ipc_argv_and_failure(self):
        plan = tw.plan(self.task, self.state())
        class Reply:
            stdout = b'{"success":true,"data":[]}'
        with patch.object(tw.subprocess, 'run', return_value=Reply()) as run:
            tw.ipc('task_workspace_view', plan['action']['args'])
            argv = run.call_args.args[0]
            self.assertEqual(argv, ['jwm-tool', 'msg', 'task_workspace_view', '--raw', '--args', json.dumps(plan['action']['args'])])
            self.assertNotIn('shell', run.call_args.kwargs)
        Reply.stdout = b'{"success":false,"error":"locked"}'
        with patch.object(tw.subprocess, 'run', return_value=Reply()):
            with self.assertRaises(ValueError):
                tw.ipc('task_workspace_view')

    def test_fifo_symlink_and_malformed_types(self):
        fifo = self.root / 'fifo'
        os.mkfifo(fifo)
        with self.assertRaises(ValueError):
            tw.load(fifo)
        link = self.root / 'link'
        link.symlink_to(fifo)
        with self.assertRaises(OSError):
            tw.load(link)
        self.task['resources'] = [dict(kind=[], label='x', reference='x')]
        with self.assertRaises(ValueError):
            tw.validate(self.task)

    def test_cli_subprocess_roundtrip(self):
        import subprocess
        import sys
        script = str(Path(tw.__file__).resolve())
        store = self.root / 'tasks'
        manifest = self.root / 'task.json'
        manifest.write_text(json.dumps(self.task))
        tool = self.root / 'jwm-tool'
        state = self.root / 'state.json'
        state.write_text(json.dumps(self.ws))
        tool.write_text("#!/usr/bin/env python3\nimport json,sys,os\np=os.environ['TEST_STATE']\ns=json.load(open(p))\nif sys.argv[2]=='task_workspace_view':\n a=json.loads(sys.argv[sys.argv.index('--args')+1])\n expected={'version':1,'connector':'DP-1','monitor':0,'monitor_identity':'process-generation-output','expected_tag_mask':1,'tag_mask':4}\n assert a==expected, a\n assert sum(w['focused'] for w in s)==1\n assert next(w for w in s if w['focused'])['tag_mask']==a['expected_tag_mask']\n s=[dict(w,focused=w['tag_mask']==4) for w in s]\n json.dump(s,open(p,'w'))\nprint(json.dumps({'success':True,'data':{'version':1,'workspaces':s,'monitor_identity':'process-generation-output'}}))\n")
        tool.chmod(0o700)
        env = dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ['PATH'], TEST_STATE=str(state))
        def run(*args):
            result = subprocess.run([sys.executable, script, '--store', str(store), *args], env=env, capture_output=True, text=True, check=True)
            return json.loads(result.stdout)
        run('import', str(manifest))
        self.assertEqual(run('list')[0]['id'], 'rpl')
        preview = run('plan', 'rpl')
        self.assertEqual(run('resume', 'rpl', '--approve-plan', preview['approval_digest'])['status'], 'workspace_selected')

    def test_verification_timeout_and_focus_race_report_uncertainty(self):
        import subprocess
        approved = tw.plan(self.task, self.state())['approval_digest']
        for outcome in (subprocess.TimeoutExpired('get_task_workspace_state', 5),
                        [dict(self.ws[1], connector='DP-2', focused=True)],
                        [dict(w, focused=True) for w in self.ws],
                        [dict(self.ws[1], monitor=False, focused=True)]):
            calls = []
            def ipc(cmd, args=None):
                calls.append(cmd)
                if len(calls) == 1:
                    return self.state()
                if cmd == 'get_task_workspace_state':
                    if isinstance(outcome, Exception):
                        raise outcome
                    return self.state(outcome)
            with self.assertRaisesRegex(ValueError, 'view sent, outcome not confirmed'):
                tw.resume(self.task, approved, ipc)
            self.assertEqual(calls.count('task_workspace_view'), 1)

    def test_atomic_api_required_and_no_fallback(self):
        for state in (self.ws, dict(version=2, workspaces=self.ws)):
            with self.assertRaises(ValueError):
                tw.plan(self.task, state)
        for state in (self.state(identity=None),
                      self.state([dict(w, focused=True) for w in self.ws]),
                      self.state([self.ws[0], dict(self.ws[1], monitor=1)])):
            self.assertTrue(tw.plan(self.task, state)['blockers'])
        approved = tw.plan(self.task, self.state())['approval_digest']
        calls = []
        def rejected(cmd, args=None):
            calls.append(cmd)
            if cmd == 'get_task_workspace_state':
                return self.state()
            raise ValueError('atomic precondition rejected')
        with self.assertRaisesRegex(ValueError, 'no retry'):
            tw.resume(self.task, approved, rejected)
        self.assertEqual(calls, ['get_task_workspace_state', 'task_workspace_view'])

    def test_identity_restart_and_generation_changes(self):
        approved = tw.plan(self.task, self.state())['approval_digest']
        calls = []
        def changed(cmd, args=None):
            calls.append(cmd)
            return self.state(identity='different-process-or-generation')
        with self.assertRaisesRegex(ValueError, 'plan changed'):
            tw.resume(self.task, approved, changed)
        self.assertEqual(calls, ['get_task_workspace_state'])
        calls.clear()
        def postchanged(cmd, args=None):
            calls.append(cmd)
            return self.state() if len(calls) == 1 else self.state([dict(self.ws[1], focused=True)], identity='different')
        with self.assertRaisesRegex(ValueError, 'monitor identity changed'):
            tw.resume(self.task, approved, postchanged)
        self.assertEqual(calls.count('task_workspace_view'), 1)

    def test_private_records_deep_json_and_escaped_size(self):
        f = self.root / 'record'
        f.write_text('{}')
        f.chmod(0o644)
        with self.assertRaises(ValueError):
            tw.load(f, private=True)
        f.write_text('[' * 2000 + ']' * 2000)
        with self.assertRaises(ValueError):
            tw.load(f)
        self.task['resources'] = [dict(kind='agent', label='x', reference=str(i) + '中' * 4000) for i in range(64)]
        with self.assertRaisesRegex(ValueError, 'serialized task'):
            tw.save_new(self.task, self.root / 'tasks')
        self.assertFalse((self.root / 'tasks' / 'rpl.json').exists())


class AgentStatusTests(unittest.TestCase):
    def setUp(self):
        TaskTests.setUp(self)
        self.identity = dict(session_id='review-1', project_id='project-1', provider='ember',
                             host_id='host-1', host_incarnation='boot-1', pane_id='pane-1',
                             window_token='window-1', wm_instance_id='wm-1')
        self.task['resources'] = [dict(kind='agent', label='Review', reference='review-1',
                                      agent_identity=self.identity)]
        session = {k: v for k, v in self.identity.items() if k != 'wm_instance_id'}
        self.snapshot = dict(protocol_version=1, wm_instance_id='wm-1', revision=4,
                             sessions=[dict(session, revision=2, status='running', can_reveal=True)])

    state = TaskTests.state

    def observed(self, snapshot=None):
        calls = []
        def call(command):
            calls.append(command)
            return self.snapshot if snapshot is None else snapshot
        result = tw.task_status(self.task, call)
        self.assertEqual(calls, ['get_agent_sessions'])
        self.assertTrue(result['read_only'])
        return result

    def test_all_observed_registry_states(self):
        # The compositor, not this client, resolves expired leases/missing windows.
        for state in tw.AGENT_STATES:
            self.snapshot['sessions'][0].update(status=state, can_reveal=False)
            result = self.observed()
            self.assertEqual(result['resources'][0]['status'], state)
            self.assertEqual(result['agent_counts'][state], 1)
            self.assertEqual(result['resources'][0]['evidence'], 'registry_observation')

    def test_identity_conflict_every_component(self):
        original = copy.deepcopy(self.snapshot)
        for field in tw.AGENT_IDENTITY - {'session_id'}:
            self.snapshot = copy.deepcopy(original)
            if field == 'wm_instance_id':
                self.snapshot[field] = 'other'
                reason = 'compositor_identity_changed'
            else:
                self.snapshot['sessions'][0][field] = 'forge' if field == 'provider' else 'other'
                reason = 'session_identity_conflict'
            result = self.observed()['resources'][0]
            self.assertEqual((result['status'], result['reason']), ('unknown', reason))

    def test_missing_and_ambiguous_are_unknown_not_disconnected(self):
        session = self.snapshot['sessions'][0]
        for sessions, reason in [([], 'session_not_observed'), ([session, session], 'ambiguous_session')]:
            self.snapshot['sessions'] = sessions
            result = self.observed()['resources'][0]
            self.assertEqual((result['status'], result['reason']), ('unknown', reason))

    def test_ipc_interruptions_do_not_retry_or_mutate(self):
        import subprocess
        for error in (OSError('lost socket'), ValueError('unsupported'),
                      subprocess.TimeoutExpired('jwm-tool', 5), subprocess.CalledProcessError(1, 'jwm-tool')):
            with patch.object(tw, 'ipc', side_effect=AssertionError('unexpected IPC')):
                call = unittest.mock.Mock(side_effect=error)
                result = tw.task_status(self.task, call)
                call.assert_called_once_with('get_agent_sessions')
            self.assertFalse(result['agent_snapshot_available'])
            self.assertEqual(result['resources'][0]['status'], 'unknown')
            self.assertEqual(result['resources'][0]['evidence'], 'unavailable')

    def test_malformed_snapshot_fails_closed(self):
        for snapshot in ([], {}, dict(self.snapshot, protocol_version=True),
                         dict(self.snapshot, sessions=[None]), dict(self.snapshot, revision=True)):
            result = self.observed(snapshot)
            self.assertFalse(result['agent_snapshot_available'])
        for field, value in [('status', []), ('provider', 'unsupported'), ('can_reveal', 1), ('revision', 0)]:
            snapshot = copy.deepcopy(self.snapshot)
            snapshot['sessions'][0][field] = value
            self.assertFalse(self.observed(snapshot)['agent_snapshot_available'])

    def test_static_agent_never_guessed_or_queried(self):
        del self.task['resources'][0]['agent_identity']
        result = tw.task_status(self.task, unittest.mock.Mock(side_effect=AssertionError('must not query')))
        self.assertEqual(result['resources'][0]['status'], 'reference_only')
        self.assertIsNone(result['agent_snapshot_available'])
        self.assertEqual(sum(result['agent_counts'].values()), 0)

    def test_status_cli_queries_only_registry(self):
        import subprocess
        import sys
        store = self.root / 'tasks'
        tw.save_new(self.task, store)
        tool = self.root / 'jwm-tool'
        tool.write_text("#!/usr/bin/env python3\nimport json,sys\nassert sys.argv[1:]==['msg','get_agent_sessions','--raw'], sys.argv\nprint(" + repr(json.dumps(dict(success=True, data=self.snapshot))) + ")\n")
        tool.chmod(0o700)
        env = dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ['PATH'])
        output = subprocess.check_output([sys.executable, tw.__file__, '--store', str(store),
                                          'status', self.task['id']], env=env)
        self.assertEqual(json.loads(output)['resources'][0]['status'], 'running')
        self.assertEqual(tw.load(store / (self.task['id'] + '.json')), self.task)

    def test_bound_task_resume_keeps_original_ipc_sequence(self):
        approved = tw.plan(self.task, self.state())['approval_digest']
        calls = []
        def call(command, args=None):
            calls.append(command)
            if command == 'get_task_workspace_state':
                return self.state() if len(calls) == 1 else self.state([dict(self.ws[1], focused=True)])
        self.assertEqual(tw.resume(self.task, approved, call)['status'], 'workspace_selected')
        self.assertEqual(calls, ['get_task_workspace_state', 'task_workspace_view', 'get_task_workspace_state'])

    def test_binding_validation_and_resume_digest_independent_of_live_status(self):
        before = tw.plan(self.task, self.state())
        self.observed()
        self.snapshot['sessions'][0]['status'] = 'failed'
        self.observed()
        self.assertEqual(before, tw.plan(self.task, self.state()))
        for field in tw.AGENT_IDENTITY:
            invalid = copy.deepcopy(self.task)
            del invalid['resources'][0]['agent_identity'][field]
            with self.assertRaises(ValueError):
                tw.validate(invalid)
        invalid = copy.deepcopy(self.task)
        invalid['resources'][0]['reference'] = 'other'
        with self.assertRaises(ValueError):
            tw.validate(invalid)


class DoctorTests(unittest.TestCase):
    setUp = AgentStatusTests.setUp
    state = TaskTests.state

    def report(self, state=None, snapshot=None):
        calls = []
        def call(command):
            calls.append(command)
            if command == 'get_task_workspace_state':
                return self.state() if state is None else state
            if command == 'get_agent_sessions':
                return self.snapshot if snapshot is None else snapshot
            raise AssertionError('mutation forbidden')
        result = tw.doctor(self.task, call)
        self.assertEqual(calls, ['get_task_workspace_state', 'get_agent_sessions'])
        self.assertNotIn('approval_digest', result)
        self.assertNotIn('action', result)
        self.assertTrue(result['read_only'])
        return result

    def test_ready_is_only_workspace_not_recovery_authorization(self):
        before = copy.deepcopy(self.task)
        result = self.report()
        self.assertEqual(result['workspace']['status'], 'ready_for_fresh_plan')
        self.assertEqual(result['repository']['status'], 'directory_present')
        self.assertEqual(result['resources'][0]['status'], 'running')
        self.assertEqual(self.task, before)
        with self.assertRaisesRegex(ValueError, 'approval digest mismatch'):
            tw.resume(self.task, tw.digest(result), lambda command: self.state())

    def test_compositor_restart_never_rebinds(self):
        snapshot = dict(self.snapshot, wm_instance_id='restarted')
        result = self.report(snapshot=snapshot)
        self.assertEqual(result['resources'][0]['status'], 'unknown')
        self.assertEqual(result['findings'][0]['reason'], 'compositor_identity_changed')
        self.assertEqual(self.identity['wm_instance_id'], 'wm-1')

    def test_missing_repository_output_and_focus_have_guidance(self):
        self.task['repository'] += '/absent'
        result = self.report(state=self.state([]))
        self.assertEqual(result['repository']['status'], 'missing_or_inaccessible')
        self.assertEqual(result['workspace']['status'], 'blocked')
        reasons = [f['reason'] for f in result['findings']]
        for reason in ['repository_missing', 'workspace_missing_or_ambiguous',
                       'focus_target_monitor_and_select_single_tag']:
            self.assertIn(reason, reasons)
        self.assertTrue(all(f['next_step'] for f in result['findings']))

    def test_both_ipc_endpoints_down_still_reports_saved_context(self):
        import subprocess
        calls = []
        def call(command):
            calls.append(command)
            raise subprocess.TimeoutExpired(command, 5)
        result = tw.doctor(self.task, call)
        self.assertEqual(calls, ['get_task_workspace_state', 'get_agent_sessions'])
        self.assertEqual(result['repository']['status'], 'directory_present')
        self.assertEqual(result['workspace']['status'], 'unknown')
        self.assertEqual(result['resources'][0]['status'], 'unknown')
        self.assertFalse(result['agent_snapshot_available'])

    def test_repository_permission_error_is_a_report_not_a_crash(self):
        with patch.object(tw.Path, 'is_dir', side_effect=PermissionError('denied')):
            result = self.report()
        self.assertEqual(result['repository']['status'], 'missing_or_inaccessible')
        self.assertIn('repository_missing', result['workspace']['blockers'])
        self.assertEqual(result['resources'][0]['status'], 'running')

    def test_one_invalid_endpoint_does_not_hide_other_checks(self):
        result = self.report(state={'version': 2})
        self.assertEqual(result['workspace']['status'], 'unknown')
        self.assertEqual(result['resources'][0]['status'], 'running')
        result = self.report(snapshot={'protocol_version': 2})
        self.assertEqual(result['workspace']['status'], 'ready_for_fresh_plan')
        self.assertEqual(result['resources'][0]['status'], 'unknown')

    def test_static_references_do_not_gain_liveness(self):
        self.task['resources'] = [dict(kind='remote', label='GPU', reference='gpu-host')]
        call = unittest.mock.Mock(return_value=self.state())
        result = tw.doctor(self.task, call)
        call.assert_called_once_with('get_task_workspace_state')
        self.assertEqual(result['resources'][0]['status'], 'reference_only')
        self.assertEqual(result['findings'][0]['reason'], 'reference_only')

    def test_doctor_cli_with_missing_compositor_preserves_store(self):
        import subprocess
        import sys
        store = self.root / 'tasks'
        path = tw.save_new(self.task, store)
        before = path.read_bytes()
        tool = self.root / 'jwm-tool'
        log = self.root / 'calls'
        tool.write_text("#!/usr/bin/env python3\nimport sys\np=" + repr(str(log)) + "\nwith open(p,'a') as f: f.write(sys.argv[2]+'\\n')\nraise SystemExit(1)\n")
        tool.chmod(0o700)
        env = dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ['PATH'])
        output = subprocess.check_output([sys.executable, tw.__file__, '--store', str(store),
                                          'doctor', self.task['id']], env=env)
        self.assertEqual(json.loads(output)['workspace']['status'], 'unknown')
        self.assertEqual(log.read_text().splitlines(), ['get_task_workspace_state', 'get_agent_sessions'])
        self.assertEqual(path.read_bytes(), before)


if __name__ == '__main__':
    unittest.main()
