#!/usr/bin/env python3
"""Run the real viewer TCP/presentation test only against one Xvfb child owned here.

CI only. No ambient DISPLAY is used or modified and no external server is killed.
"""
import os
import selectors
import signal
import subprocess
import sys
import time


def launch():
    child = subprocess.Popen(
        ["Xvfb", "-displayfd", "1", "-screen", "0", "1280x800x24", "-nolisten", "tcp", "-ac", "-noreset"],
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


def run_test(command, env):
    # Only this child and its descendants belong to the newly created session.
    child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        stdout, stderr = child.communicate(timeout=600)
        return subprocess.CompletedProcess(command, child.returncode, stdout, stderr)
    except BaseException:
        # On timeout or interruption, kill only the group created above.
        # The session leader is our own Popen child; never use ambient process IDs.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.communicate(timeout=10)
        raise


TESTS = (
    "remote::host::host_viewer_integration::actual_viewer_tcp_pointer_presented_scope_and_revocation",
    "remote::host::host_keyboard_viewer_integration::actual_viewer_tcp_keyboard_presented_scope_and_revocation",
)


def run_owned_test(test_name):
    child = None
    try:
        # Each exact test owns a fresh server: a failed pointer test may leave
        # an XTEST button down, which must not contaminate keyboard evidence.
        child, display = launch()
        env = os.environ.copy()
        env.pop("DISPLAY", None)
        env.pop("WAYLAND_DISPLAY", None)
        env.update(JWM_REMOTE_TEST_VIEWER_OWNED="1", JWM_REMOTE_TEST_VIEWER_DISPLAY=display)
        command = ["cargo", "test", "--locked", "--no-default-features", "--features",
                   "remote-wayland,backend-wayland-udev", "--lib", test_name,
                   "--", "--ignored", "--exact", "--test-threads=1", "--nocapture"]
        result = run_test(command, env)
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        if result.returncode:
            return result.returncode
        if "1 passed; 0 failed; 0 ignored" not in result.stdout:
            raise RuntimeError("mandatory real viewer test did not execute exactly once")
        return 0
    finally:
        if child is not None:
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)


def main():
    failures = []
    for test_name in TESTS:
        try:
            code = run_owned_test(test_name)
        except (RuntimeError, OSError, subprocess.SubprocessError) as error:
            # run_test already tears down only its owned process group on timeout.
            # Continue independent tests, but never turn missing evidence green.
            print(f"FAILED {test_name}: {error}", file=sys.stderr)
            code = 1
        if code:
            failures.append((test_name, code))
        print(f"viewer exact result: {test_name}: {code}", file=sys.stderr)
    if failures:
        print(f"viewer failures: {failures}", file=sys.stderr)
        return failures[0][1] if failures[0][1] > 0 else 1
    return 0


def selftest():
    import unittest
    from unittest import mock

    class OwnedViewerTests(unittest.TestCase):
        def run_owned(self, result):
            child = mock.Mock()
            child.poll.return_value = None
            with mock.patch.object(sys.modules[__name__], "launch", return_value=(child, ":901")), mock.patch.object(sys.modules[__name__], "run_test", return_value=result) as run, mock.patch.dict(os.environ, {"DISPLAY": ":user", "WAYLAND_DISPLAY": "user"}):
                value = main()
            self.assertEqual(child.terminate.call_count, 2)
            self.assertEqual(run.call_count, 2)
            env = run.call_args.args[1]
            self.assertNotIn("DISPLAY", env)
            self.assertNotIn("WAYLAND_DISPLAY", env)
            self.assertEqual(env["JWM_REMOTE_TEST_VIEWER_DISPLAY"], ":901")
            return value

        def test_timeout_kills_only_owned_test_group(self):
            child = mock.Mock(pid=987654)
            child.communicate.side_effect = [subprocess.TimeoutExpired("cargo", 600), ("", "")]
            with mock.patch.object(subprocess, "Popen", return_value=child) as popen, mock.patch.object(os, "killpg") as kill:
                with self.assertRaises(subprocess.TimeoutExpired):
                    run_test(["cargo", "test"], {})
            self.assertTrue(popen.call_args.kwargs["start_new_session"])
            kill.assert_called_once_with(child.pid, signal.SIGKILL)
            self.assertEqual(child.communicate.call_count, 2)

        def test_success_requires_actual_execution(self):
            self.assertEqual(self.run_owned(mock.Mock(returncode=0, stdout="1 passed; 0 failed; 0 ignored", stderr="")), 0)

        def test_failure_is_not_hidden(self):
            self.assertEqual(self.run_owned(mock.Mock(returncode=7, stdout="", stderr="")), 7)

        def test_empty_filter_is_failure(self):
            self.assertNotEqual(self.run_owned(mock.Mock(returncode=0, stdout="0 passed; 0 failed; 0 ignored", stderr="")), 0)

        def test_pointer_failure_does_not_skip_keyboard_or_hide_failure(self):
            with mock.patch.object(sys.modules[__name__], "run_owned_test", side_effect=[7, 0]) as run:
                self.assertEqual(main(), 7)
            self.assertEqual([call.args[0] for call in run.call_args_list], list(TESTS))

        def test_timeout_still_attempts_independent_keyboard(self):
            with mock.patch.object(sys.modules[__name__], "run_owned_test", side_effect=[subprocess.TimeoutExpired("cargo", 600), 0]) as run:
                self.assertEqual(main(), 1)
            self.assertEqual(run.call_count, 2)

    suite = unittest.defaultTestLoader.loadTestsFromTestCase(OwnedViewerTests)
    return 0 if unittest.TextTestRunner().run(suite).wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(selftest() if sys.argv[1:] == ["--self-test"] else main())
