#!/usr/bin/env python3
"""Native-window harness contracts without a display, session bus or app process."""

import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location(
    "native_apps", Path(__file__).with_name("test-native-apps.py")
)
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)


class NativeAppsHarnessTests(unittest.TestCase):
    def test_missing_probe_fails_before_starting_a_session(self):
        for tool in ("dbus-run-session", "Xvfb", "xdotool", "xprop", "import"):
            with self.subTest(tool=tool):
                with (
                    patch.object(HARNESS.shutil, "which",
                                 side_effect=lambda name: None if name == tool else "/bin/" + name),
                    patch.object(HARNESS.sys, "argv", ["test-native-apps.py"]),
                    patch.object(HARNESS.subprocess, "call") as call,
                    patch.object(HARNESS.subprocess, "Popen") as popen,
                ):
                    with self.assertRaisesRegex(SystemExit, f"Required tool unavailable: {tool}$"):
                        HARNESS.main()
                    call.assert_not_called()
                    popen.assert_not_called()

    def test_available_tools_pass_preflight_without_running_them(self):
        with patch.object(HARNESS.shutil, "which", return_value="/bin/tool") as which:
            HARNESS.require_tools()
        self.assertEqual([call.args[0] for call in which.call_args_list],
                         ["dbus-run-session", "Xvfb", "xdotool", "xprop", "import"])

    def test_private_server_argv_disables_reset_and_tcp(self):
        # Stop at the process boundary: never start a display or import Pillow.
        with (
            patch.object(HARNESS, "require_tools"),
            patch.object(HARNESS.sys, "argv", ["test-native-apps.py", "--under-bus"]),
            patch.dict(HARNESS.sys.modules, {"PIL": Mock()}),
            patch.object(HARNESS.subprocess, "Popen", side_effect=RuntimeError("offline spawn stop")) as popen,
        ):
            with self.assertRaisesRegex(RuntimeError, "offline spawn stop"):
                HARNESS.main()
        popen.assert_called_once()
        self.assertEqual(popen.call_args.args[0],
                         ["Xvfb", "-displayfd", "1", "-noreset", "-nolisten", "tcp",
                          "-screen", "0", "1280x900x24"])

    def test_unicode_title_uses_net_wm_name_and_skips_other_windows(self):
        command = Mock(side_effect=[
            "123\n456",
            '_NET_WM_NAME(UTF8_STRING) = "photo.png — JWM 图片查看器"',
            '_NET_WM_NAME(UTF8_STRING) = "Second note — JWM 备忘录"',
        ])
        self.assertEqual(HARNESS.find_window(command, "Second note.*JWM 备忘录"), "456")
        self.assertEqual(command.call_args_list[0].args,
                         ("xdotool", "search", "--onlyvisible", "--class", "org.jwm"))
        self.assertEqual(command.call_args_list[2].args,
                         ("xprop", "-id", "456", "_NET_WM_NAME"))

    def test_nonmatching_title_keeps_waiting(self):
        command = Mock(side_effect=["123", '_NET_WM_NAME(UTF8_STRING) = "Other app"'])
        self.assertIsNone(HARNESS.find_window(command, "JWM 备忘录"))

    def test_missing_xprop_is_not_hidden_by_window_lookup(self):
        command = Mock(side_effect=["123", FileNotFoundError("xprop")])
        with self.assertRaisesRegex(FileNotFoundError, "xprop"):
            HARNESS.find_window(command, "JWM 备忘录")


if __name__ == "__main__":
    unittest.main()
