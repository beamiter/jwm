import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

FILE = Path(__file__).with_name("portal_probe.py")
spec = importlib.util.spec_from_file_location("portal_probe", FILE)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
m.gio()


class Contract(unittest.TestCase):
    def test_cold_envs_disjoint(self):
        self.assertIsNone(m.env_sample(m.phase_env("x11"))["WAYLAND_DISPLAY"])
        self.assertIsNone(m.env_sample(m.phase_env("wayland"))["DISPLAY"])

    def test_transition_is_model_not_reset(self):
        before = m.env_sample(m.phase_env("foreign"))
        after = m.env_sample({**m.phase_env("foreign"), **m.phase_env("x11")})
        session = Mock()
        session.activate_observer.side_effect = [before, before, after]
        original, m.dbus = m.dbus, Mock()
        try:
            got = m.activation_observation(session, "foreign", "x11")
            self.assertEqual(got["before_import"]["XDG_CURRENT_DESKTOP"], "FixtureOther")
            self.assertEqual(got["after_import"]["XDG_CURRENT_DESKTOP"], "jwm")
            self.assertTrue(got["stale_wayland_display"])
            self.assertEqual([x.args[1] for x in m.dbus.call_args_list],
                             ["ReloadConfig", "UpdateActivationEnvironment"])
        finally:
            m.dbus = original

    def test_clean_environment_excludes_ambient_bus(self):
        with tempfile.TemporaryDirectory() as tmp:
            patch = dict(os.environ)
            try:
                os.environ.update(DBUS_SESSION_BUS_ADDRESS="must-not-connect",
                                  WAYLAND_DISPLAY="must-not-inherit",
                                  XDG_CURRENT_DESKTOP="must-not-inherit",
                                  XDG_DESKTOP_PORTAL_DIR="must-not-inherit")
                s = m.Session(Path(tmp), None, m.phase_env("x11"))
                for name in ("DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "XDG_DESKTOP_PORTAL_DIR"):
                    self.assertNotIn(name, s.env)
                self.assertEqual(s.env["XDG_CURRENT_DESKTOP"], "jwm")
            finally:
                os.environ.clear()
                os.environ.update(patch)

    def test_service_exec_is_lexically_quoted(self):
        dest = Path("/tmp/owned directory/receipt.jsonl")
        text = m.service_text("org.jwm.Probe", "observe", dest)
        self.assertNotIn("SystemdService", text)
        cmd = text.split("Exec=", 1)[1]
        ok, argv = m.GLib.shell_parse_argv(cmd)
        self.assertTrue(ok)
        self.assertEqual(argv[-2:], ["--journal", str(dest)])

    def test_json_cancellation_signature(self):
        self.assertEqual(m.var("(ua{sv})", (1, {})).unpack(), (1, {}))


if __name__ == "__main__":
    unittest.main()
