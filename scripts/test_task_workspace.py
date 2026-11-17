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


if __name__ == '__main__':
    unittest.main()
