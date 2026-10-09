"""Pure/fake safety regressions. Never connect to a display/socket or spawn input/recorders."""
import io
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from runner import demo_windows, input_driver, run_demo, session_guard


class RecordingAdmissionTests(unittest.TestCase):
    def test_highest_tag_occupied_selects_another_empty_tag(self):
        ipc = Mock()
        ipc.query.return_value = [{"id": 99, "class": "PrivateApp", "tags": 4}]
        self.assertEqual(run_demo.choose_unused_tag(ipc, 3, 0, None), 2)

    def test_all_tags_occupied_refuses(self):
        ipc = Mock()
        ipc.query.return_value = [{"id": 99, "tags": 7}]
        with self.assertRaisesRegex(RuntimeError, "no unused"):
            run_demo.choose_unused_tag(ipc, 3, 0, None)

    def test_global_floating_user_window_is_not_a_bar(self):
        window = {"id": 99, "class": "PrivateApp", "tags": 7, "is_floating": True}
        self.assertEqual(run_demo.occupied_user_tags([window], 3), 7)

    def test_sticky_user_window_occupies_every_tag(self):
        self.assertEqual(run_demo.occupied_user_tags([{"id": 99, "tags": 1, "is_sticky": True}], 3), 7)

    def test_only_explicit_owned_status_bar_is_exempt(self):
        self.assertEqual(run_demo.occupied_user_tags([{"id": 99, "tags": 7, "is_status_bar": True}], 3), 0)
        self.assertEqual(run_demo.occupied_user_tags([{"id": 99, "class": "JwmDemo", "tags": 7}], 3), 7)

    def test_recording_recheck_rejects_visible_window_on_other_monitor(self):
        ipc = Mock()
        ipc.query.return_value = [{"id": 99, "tags": 1, "is_on_view": True}]
        with self.assertRaisesRegex(RuntimeError, "recording refused"):
            run_demo.ensure_recording_isolated(ipc, SimpleNamespace(control_sockets={42: "owned"}), 4)

    def test_recording_recheck_uses_owned_ids_not_demo_class(self):
        ipc = Mock()
        ipc.query.return_value = [{"id": 99, "class": "JwmDemo", "tags": 4, "is_on_view": True}]
        with self.assertRaisesRegex(RuntimeError, "recording refused"):
            run_demo.ensure_recording_isolated(ipc, SimpleNamespace(control_sockets={42: "owned"}), 4)

    def test_owned_demo_and_explicit_bar_are_allowed(self):
        ipc = Mock()
        ipc.query.return_value = [{"id": 42, "tags": 4, "is_on_view": True}, {"id": 7, "tags": 7, "is_status_bar": True}, {"id": 99, "tags": 1, "is_on_view": False}]
        run_demo.ensure_recording_isolated(ipc, SimpleNamespace(control_sockets={42: "owned"}), 4)

    def test_null_window_snapshot_is_refused(self):
        ipc = Mock()
        ipc.query.return_value = None
        with self.assertRaisesRegex(RuntimeError, "invalid window snapshot"):
            run_demo.choose_unused_tag(ipc, 3, 0, None)

    def test_malformed_window_entries_are_refused(self):
        ipc = Mock()
        for value in ({}, [None], [{"id": 1}], [{"id": True, "tags": 1}], [{"id": 1, "tags": True}], [{"id": 1, "tags": -1}], [{"id": 1, "tags": 1 << 32}], [{"id": 1, "tags": 1, "is_status_bar": "yes"}]):
            with self.subTest(value=value):
                ipc.query.return_value = value
                with self.assertRaisesRegex(RuntimeError, "recording refused"):
                    run_demo.choose_unused_tag(ipc, 3, 0, None)

    def test_empty_window_snapshot_is_allowed(self):
        ipc = Mock()
        ipc.query.return_value = []
        self.assertEqual(run_demo.choose_unused_tag(ipc, 3, 0, None), 4)
        run_demo.ensure_recording_isolated(ipc, SimpleNamespace(control_sockets={}), 4)

    def test_unknown_visibility_is_refused(self):
        ipc = Mock()
        ipc.query.return_value = [{"id": 99, "tags": 1}]
        with self.assertRaisesRegex(RuntimeError, "recording refused"):
            run_demo.ensure_recording_isolated(ipc, SimpleNamespace(control_sockets={}), 4)

    def test_nonboolean_visibility_is_refused(self):
        ipc = Mock()
        ipc.query.return_value = [{"id": 99, "tags": 1, "is_on_view": 0}]
        with self.assertRaisesRegex(RuntimeError, "recording refused"):
            run_demo.ensure_recording_isolated(ipc, SimpleNamespace(control_sockets={}), 4)

    def test_main_selects_unused_tag_before_entering_scene(self):
        trace = []
        class Ipc:
            def query(self, name):
                if name == "get_version": return {"backend": "x11rb", "build_profile": "release"}
                if name == "get_config": return {"tags_length": 3}
                if name == "get_windows": return [{"id": 99, "tags": 4}]
                raise AssertionError(name)
            def command(self, *args): pass
        class Stub:
            def __init__(self, *args): pass
            def close(self): pass
        class Guard(Stub):
            original_tag = 1
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def update(self, *args): pass
        scene = {"id": "fixture", "title": "fixture", "status": "ready", "actions": []}
        args = SimpleNamespace(voice=False, assemble=False, generate_assets=False, tts_command=None, preflight=False, backend="x11rb", resolution=None, fps=60)
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            (base / "manifest").mkdir()
            (base / "manifest/scenes.toml").write_text('[[scene]]\nid="fixture"\ntitle="fixture"\nstatus="ready"\n')
            replacements = dict(parse_args=lambda: args, JwmIpc=Ipc, preflight=lambda ipc: SimpleNamespace(ok=True, screen="100x100", as_dict=lambda: {}), load_scenes=lambda args: [scene], demo_binary=lambda args: Path("unused"), BASE=base, ROOT=base, SessionGuard=Guard, select_tag=lambda ipc, tag: trace.append(tag), focused_workspace=lambda ipc: {"tag_mask": 2, "layout": "tile", "m_fact": 0.5, "n_master": 1}, DemoWindows=Stub, XdotoolInput=Stub, Recorder=Stub, restore_workspace_baseline=lambda *args: None, restore_effect_baseline=lambda *args: None, run_scene=lambda *args: {"scene": "fixture", "success": True, "video": "synthetic.mp4"})
            with patch.multiple(run_demo, **replacements), patch.object(run_demo.signal, "signal"), patch("sys.stdout", new=io.StringIO()):
                self.assertEqual(run_demo.main(), 0)
        self.assertEqual(trace, [2])


class StartupOwnershipTests(unittest.TestCase):
    def process(self):
        process = Mock()
        process.poll.return_value = None
        process.wait.return_value = 0
        return process

    def test_timeout_terminates_reaps_and_never_reads_stderr_directly(self):
        process = self.process()
        windows = demo_windows.DemoWindows(Path("unused"), None, Path("unused"))
        with patch.object(demo_windows.subprocess, "Popen", return_value=process), patch.object(demo_windows, "_read_ready_line", create=True, side_effect=TimeoutError("fixture")):
            with self.assertRaises(TimeoutError): windows.spawn(1)
        process.terminate.assert_called_once()
        process.wait.assert_called_once_with(timeout=2)
        process.stderr.read.assert_not_called()
        self.assertEqual(windows.processes, [])

    def test_invalid_json_terminates_owned_child(self):
        process = self.process()
        windows = demo_windows.DemoWindows(Path("unused"), None, Path("unused"))
        with patch.object(demo_windows.subprocess, "Popen", return_value=process), patch.object(demo_windows, "_read_ready_line", create=True, return_value="bad JSON"):
            with self.assertRaises(json.JSONDecodeError): windows.spawn(1)
        process.terminate.assert_called_once()
        process.wait.assert_called_once_with(timeout=2)

    def test_ready_partial_line_obeys_total_deadline(self):
        stream = Mock()
        with patch.object(demo_windows.time, "monotonic", side_effect=[0.0, 1.0, 6.0]), patch.object(demo_windows.select, "select", return_value=([stream], [], [])), patch("os.read", return_value=b"partial"):
            with self.assertRaises(TimeoutError): demo_windows._read_ready_line(stream, timeout=5)

    def test_ready_line_has_a_byte_limit(self):
        stream = Mock()
        with patch.object(demo_windows.time, "monotonic", return_value=0.0), patch.object(demo_windows.select, "select", return_value=([stream], [], [])), patch("os.read", return_value=b"12345"):
            with self.assertRaisesRegex(RuntimeError, "byte limit"): demo_windows._read_ready_line(stream, max_bytes=4)

    def test_failed_terminate_escalates_and_reaps(self):
        process = self.process()
        process.wait.side_effect = [subprocess.TimeoutExpired("fixture", 2), 0]
        demo_windows._stop_process(process)
        process.kill.assert_called_once()
        self.assertEqual(process.wait.call_count, 2)


class InputCleanupTests(unittest.TestCase):
    def check_failure(self, failing_action):
        driver = input_driver.XdotoolInput.__new__(input_driver.XdotoolInput)
        calls = []
        def run(*args):
            calls.append(args)
            if args[0] == failing_action: raise RuntimeError("synthetic input error")
        driver._run = run
        driver.smooth = lambda *args, **kwargs: None
        with self.assertRaises(RuntimeError): driver.drag((1, 1), (2, 2))
        self.assertIn(("keyup", "Alt_L"), calls)
        return calls
    def test_mousedown_failure_releases_modifier(self):
        self.assertIn(("mouseup", 1), self.check_failure("mousedown"))
    def test_mouseup_failure_still_releases_modifier(self):
        self.check_failure("mouseup")
    def test_keydown_failure_still_attempts_keyup(self):
        self.check_failure("keydown")


class RestoreCleanupTests(unittest.TestCase):
    def guard(self):
        guard = session_guard.SessionGuard.__new__(session_guard.SessionGuard)
        guard.ipc = Mock()
        guard.original_tag = 1
        guard.original_layout = "tile"
        guard.state_path = Mock()
        guard.lock_path = Mock()
        guard.lock_file = Mock()
        return guard
    def test_missing_configuration_is_restored_from_owned_backup(self):
        guard = self.guard()
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "missing.toml"
            backup = Path(directory) / "backup.toml"
            backup.write_text("synthetic original\n")
            guard.config_backups = [(source, backup)]
            guard.restore()
            self.assertEqual(source.read_bytes(), backup.read_bytes())
        guard.ipc.command.assert_any_call("reload_config")
    def test_restore_failure_releases_lock_but_preserves_recovery_state(self):
        guard = self.guard()
        guard.restore = Mock(side_effect=OSError("synthetic restore failure"))
        with patch.object(session_guard.fcntl, "flock") as flock:
            with self.assertRaises(OSError): guard.__exit__(None, None, None)
        flock.assert_called_once_with(guard.lock_file, session_guard.fcntl.LOCK_UN)
        guard.lock_file.close.assert_called_once()
        guard.state_path.unlink.assert_not_called()


if __name__ == "__main__":
    unittest.main()
