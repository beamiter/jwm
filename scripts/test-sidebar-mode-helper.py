#!/usr/bin/env python3
"""Policy and ambiguous-response regressions for the live-compatible helper."""
import contextlib
import importlib.machinery
import io
import json
import os
import tempfile
from pathlib import Path
import subprocess
import types
import unittest
from unittest.mock import patch

loader = importlib.machinery.SourceFileLoader("sidebar_mode", str(Path(__file__).with_name("jwm-toggle-sidebar-mode")))
helper = types.ModuleType(loader.name)
loader.exec_module(helper)


class SidebarModeTests(unittest.TestCase):
    def invoke(self, config, timeout=False, conflict=False):
        calls = []
        with tempfile.TemporaryDirectory(prefix="sidebar-helper-test-") as directory:
            path = Path(directory) / "config.toml"
            path.write_text('# original comment\n[appearance]\nfont = "Sans" # preserve\n')
            path.chmod(0o600)

            def execute(command, **_kwargs):
                calls.append(command)
                if command[2] == "get_desktop_shell":
                    data = {"success": True, "data": {"config": config}}
                elif command[2] == "get_config_status":
                    data = {"success": True, "data": {"path": str(path)}}
                elif timeout:
                    raise subprocess.TimeoutExpired(command, 5)
                else:
                    data = {"success": True}
                    if conflict:
                        path.write_text('# concurrent user edit\n[appearance]\nfont = "Lato"\n')
                return subprocess.CompletedProcess(command, 0, json.dumps(data), "")

            with patch("sys.argv", ["jwm-toggle-sidebar-mode"]), patch.object(helper.subprocess, "run", execute), patch.dict(os.environ, {"XDG_RUNTIME_DIR": directory}), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                result = helper.main()
            self.last_config_text = path.read_text()
            self.last_config_mode = path.stat().st_mode & 0o777
        return result, calls

    def test_legacy_mixed_mode_keeps_disabled_edge_and_timing(self):
        result, calls = self.invoke({"enabled": True, "left": "disabled", "right": "always", "bottom": "hover", "reveal_delay_ms": 233, "hide_delay_ms": 999, "trigger_pixels": 4})
        self.assertEqual(result, 0)
        values = json.loads(calls[-1][-1])["values"]
        self.assertEqual(values, {"desktop_shell.enabled": True, "desktop_shell.left": "disabled", "desktop_shell.right": "always", "desktop_shell.bottom": "always", "desktop_shell.reveal_delay_ms": 233, "desktop_shell.hide_delay_ms": 999, "desktop_shell.trigger_pixels": 4})

    def test_all_auto_hidden_edges_become_pinned(self):
        result, calls = self.invoke({"enabled": True, "left": "hover", "right": "hover", "bottom": "hover"})
        self.assertEqual(result, 0)
        values = json.loads(calls[-1][-1])["values"]
        self.assertTrue(all(values[f"desktop_shell.{edge}"] == "always" for edge in ("left", "right", "bottom")))

    def test_all_pinned_edges_become_auto_hidden(self):
        result, calls = self.invoke({"enabled": True, "left": "always", "right": "always", "bottom": "always"})
        self.assertEqual(result, 0)
        values = json.loads(calls[-1][-1])["values"]
        self.assertTrue(all(values[f"desktop_shell.{edge}"] == "hover" for edge in ("left", "right", "bottom")))

    def test_disabled_shell_is_never_enabled_by_the_shortcut(self):
        result, calls = self.invoke({"enabled": False, "left": "hover", "right": "hover", "bottom": "hover"})
        self.assertEqual(result, 1)
        self.assertEqual(len(calls), 1)

    def test_all_disabled_edges_stay_disabled(self):
        result, calls = self.invoke({"enabled": True, "left": "disabled", "right": "disabled", "bottom": "disabled"})
        self.assertEqual(result, 0)
        self.assertEqual(len(calls), 1)

    def test_ambiguous_write_timeout_does_not_retry_or_toggle_again(self):
        result, calls = self.invoke({"enabled": True, "left": "hover", "right": "hover", "bottom": "hover"}, timeout=True)
        self.assertEqual(result, 1)
        self.assertEqual(len(calls), 3)
        self.assertEqual(calls[-1][2], "set_config_batch")

    def test_success_keeps_comments_permissions_and_unrelated_values(self):
        result, _calls = self.invoke({"enabled": True, "left": "hover", "right": "hover", "bottom": "hover"})
        self.assertEqual(result, 0)
        self.assertIn('font = "Sans" # preserve', self.last_config_text)
        self.assertIn('# original comment', self.last_config_text)
        self.assertIn('left = "always"', self.last_config_text)
        self.assertEqual(self.last_config_mode, 0o600)

    def test_concurrent_user_edit_is_never_overwritten(self):
        result, calls = self.invoke({"enabled": True, "left": "hover", "right": "hover", "bottom": "hover"}, conflict=True)
        self.assertEqual(result, 1)
        self.assertEqual(len(calls), 3)
        self.assertEqual(self.last_config_text, '# concurrent user edit\n[appearance]\nfont = "Lato"\n')

    def test_multiline_string_cannot_be_mistaken_for_a_table(self):
        with tempfile.TemporaryDirectory(prefix="sidebar-helper-literal-") as directory:
            path = Path(directory) / "config.toml"
            text = '[appearance]\nnote = """\n[desktop_shell]\nleft = "hover"\n"""\n'
            path.write_text(text)
            with self.assertRaises(ValueError):
                helper.prepare_config(path, {"desktop_shell.enabled": True, "desktop_shell.left": "always"})
            self.assertEqual(path.read_text(), text)

    def test_unknown_mode_cannot_write_partial_configuration(self):
        result, calls = self.invoke({"enabled": True, "left": "future-mode", "right": "hover", "bottom": "hover"})
        self.assertEqual(result, 1)
        self.assertEqual(len(calls), 1)


if __name__ == "__main__":
    unittest.main()
