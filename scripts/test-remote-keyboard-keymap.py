#!/usr/bin/env python3
"""Run the real keymap guard only against two Xvfb children owned here.

CI only. No ambient DISPLAY is used or modified and no external server is killed.
"""
import os
import selectors
import subprocess
import sys
import time


def launch():
    child = subprocess.Popen(
        ["Xvfb", "-displayfd", "1", "-screen", "0", "640x480x24", "-nolisten", "tcp", "-ac"],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=None,
    )
    try:
        os.set_blocking(child.stdout.fileno(), False)
        deadline = time.monotonic() + 20
        announced = bytearray()
        with selectors.DefaultSelector() as ready:
            ready.register(child.stdout, selectors.EVENT_READ)
            while b"\n" not in announced:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not ready.select(remaining):
                    raise RuntimeError("owned Xvfb did not announce a display within 20 seconds")
                chunk = os.read(child.stdout.fileno(), 32 - len(announced))
                if not chunk or len(announced) + len(chunk) >= 32:
                    raise RuntimeError("invalid owned Xvfb display announcement")
                announced.extend(chunk)
            number = bytes(announced).strip()
        if not number.isdigit() or child.poll() is not None:
            raise RuntimeError("owned Xvfb did not start")
        return child, ":" + number.decode("ascii")
    except BaseException:
        child.kill()
        child.wait(timeout=5)
        raise


def main():
    children = []
    try:
        first, a = launch()
        children.append(first)
        second, b = launch()
        children.append(second)
        if a == b:
            raise RuntimeError("Xvfb children returned the same display")
        env = os.environ.copy()
        env.pop("DISPLAY", None)
        env.update(JWM_KEYMAP_TEST_OWNED="1", JWM_KEYMAP_TEST_DISPLAY_A=a, JWM_KEYMAP_TEST_DISPLAY_B=b)
        command = ["cargo", "test", "--locked", "--no-default-features", "--features",
                   "remote-wayland,backend-wayland-udev", "--lib",
                   "remote::keyboard_keymap::tests::keyboard_keymap_owned_x11_nonce_and_history",
                   "--", "--ignored", "--exact", "--test-threads=1", "--nocapture"]
        return subprocess.run(command, env=env, timeout=600, check=False).returncode
    finally:
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)


def selftest():
    import unittest
    from unittest import mock

    class OwnedServerTests(unittest.TestCase):
        def children(self):
            first, second = mock.Mock(), mock.Mock()
            first.poll.return_value = second.poll.return_value = None
            return first, second

        def test_only_owned_displays_and_exact_ignored_filter(self):
            first, second = self.children()
            with mock.patch.object(sys.modules[__name__], "launch", side_effect=[(first, ":901"), (second, ":902")]), mock.patch.object(subprocess, "run", return_value=mock.Mock(returncode=0)) as run, mock.patch.dict(os.environ, {"DISPLAY": ":user"}):
                self.assertEqual(main(), 0)
            command = run.call_args.args[0]
            env = run.call_args.kwargs["env"]
            self.assertNotIn("DISPLAY", env)
            self.assertEqual(env["JWM_KEYMAP_TEST_DISPLAY_A"], ":901")
            self.assertEqual(env["JWM_KEYMAP_TEST_DISPLAY_B"], ":902")
            self.assertIn("--ignored", command)
            self.assertIn("--exact", command)
            first.terminate.assert_called_once()
            second.terminate.assert_called_once()

        def test_partial_startup_reaps_only_started_child(self):
            first, _ = self.children()
            with mock.patch.object(sys.modules[__name__], "launch", side_effect=[(first, ":901"), RuntimeError("startup failed")]):
                with self.assertRaises(RuntimeError):
                    main()
            first.terminate.assert_called_once()
            first.wait.assert_called_once()

        def test_test_failure_status_is_not_hidden(self):
            first, second = self.children()
            with mock.patch.object(sys.modules[__name__], "launch", side_effect=[(first, ":901"), (second, ":902")]), mock.patch.object(subprocess, "run", return_value=mock.Mock(returncode=101)):
                self.assertEqual(main(), 101)

    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(OwnedServerTests))
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(selftest() if sys.argv[1:] == ["--self-test"] else main())
