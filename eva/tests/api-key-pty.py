#!/usr/bin/env python3
"""Offline CLI checks against a real controlling terminal and fake credentials.

Run after building: python3 eva/tests/api-key-pty.py target/release/jwm-eva
No real credential, service restart, microphone or network request is used.
"""
import errno
import json
import os
from pathlib import Path
import pty
import select
import stat
import subprocess
import sys
import tempfile
import termios
import time
import tomllib
import unittest

BINARY = str(Path(sys.argv.pop(1)).resolve())
KEY = b"sk-offline-placeholder-not-a-real-api-key"


class Terminal:
    def __init__(self, config, storage=None, env=None, restart=False):
        argv = [BINARY, "--config", str(config), "config", "api-key"]
        if not restart:
            argv.append("--no-restart")
        if storage:
            argv += ["--storage", storage]
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(BINARY, argv, env or os.environ.copy())
        self.output = b""
        self.status = None

    def read(self, timeout=0.1):
        if select.select([self.fd], [], [], timeout)[0]:
            try:
                data = os.read(self.fd, 65536)
                self.output += data
                return bool(data)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
        return False

    def until(self, text):
        deadline = time.monotonic() + 10
        while text.encode() not in self.output and time.monotonic() < deadline:
            self.read()
        if text.encode() not in self.output:
            try:
                self.finish()
            finally:
                raise AssertionError("Expected prompt missing: " + self.output.decode(errors="replace"))

    def send(self, data):
        os.write(self.fd, data)

    def finish(self):
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                self.read()
                pid, status = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    self.status = os.waitstatus_to_exitcode(status)
                    while self.read(0):
                        pass
                    return self.status, termios.tcgetattr(self.fd)[3]
            raise AssertionError("CLI did not exit")
        finally:
            if self.status is None:
                os.kill(self.pid, 9)
                os.waitpid(self.pid, 0)
            os.close(self.fd)


class ApiKeyCliTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="eva-api-key-test-")
        self.directory = Path(self.tmp.name)
        self.config = self.directory / "eva.toml"

    def tearDown(self):
        self.tmp.cleanup()

    def enter(self, terminal, key=KEY):
        terminal.until("OpenAI API key")
        self.assertFalse(termios.tcgetattr(terminal.fd)[3] & termios.ECHO)
        terminal.send(key + b"\n")
        code, flags = terminal.finish()
        self.assertTrue(flags & termios.ECHO)
        self.assertTrue(flags & termios.ICANON)
        self.assertTrue(flags & termios.ISIG)
        self.assertNotIn(key, terminal.output)
        return code

    def test_first_run_default_file_and_rotation(self):
        terminal = Terminal(self.config)
        terminal.until("选择 [1/2")
        terminal.send(b"\n")
        self.assertEqual(self.enter(terminal), 0)
        settings = tomllib.loads(self.config.read_text())
        key_file = Path(settings["openai"]["api_key_file"])
        self.assertEqual(settings["openai"]["api_key_source"], "file")
        self.assertEqual(key_file.read_bytes(), KEY)
        self.assertEqual(stat.S_IMODE(key_file.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.config.stat().st_mode), 0o600)
        terminal = Terminal(self.config, "file")
        self.assertEqual(self.enter(terminal, KEY + b"-rotated"), 0)
        self.assertEqual(key_file.read_bytes(), KEY + b"-rotated")

    def test_cancel_and_empty_leave_existing_configuration_untouched(self):
        original = "# preserve comment\n[openai]\napi_key_source = 'env'\n"
        self.config.write_text(original)
        self.config.chmod(0o600)
        for cancel in (b"\x03", b"\x04", b"\n"):
            terminal = Terminal(self.config, "file")
            terminal.until("OpenAI API key")
            terminal.send(cancel)
            code, flags = terminal.finish()
            self.assertNotEqual(code, 0)
            self.assertTrue(flags & termios.ECHO)
            self.assertTrue(flags & termios.ISIG)
            self.assertEqual(self.config.read_text(), original)
            self.assertFalse((self.directory / "openai.key").exists())

    def test_concurrent_edit_is_preserved(self):
        self.config.write_text("[openai]\napi_key_source = 'env'\n")
        self.config.chmod(0o600)
        terminal = Terminal(self.config, "file")
        terminal.until("OpenAI API key")
        edited = "# edited while prompting\n[openai]\napi_key_source = 'env'\n"
        self.config.write_text(edited)
        self.assertNotEqual(self.enter(terminal), 0)
        self.assertEqual(self.config.read_text(), edited)
        self.assertFalse((self.directory / "openai.key").exists())

    def test_keyring_uses_stdin_and_suppresses_child_output(self):
        fake = self.directory / "bin"
        fake.mkdir()
        capture = self.directory / "captured"
        tool = fake / "secret-tool"
        tool.write_text("#!/usr/bin/python3\n"
                        "import os,sys,json\n"
                        "key=sys.stdin.buffer.read()\n"
                        "p=os.environ['EVA_TEST_CAPTURE']\n"
                        "fd=os.open(p,os.O_CREAT|os.O_WRONLY,0o600)\n"
                        "os.write(fd,key);os.close(fd)\n"
                        "open(p+'.argv','w').write(json.dumps(sys.argv[1:]))\n"
                        "sys.stdout.buffer.write(key);sys.stderr.buffer.write(key)\n")
        tool.chmod(0o700)
        env = dict(os.environ, PATH=str(fake), EVA_TEST_CAPTURE=str(capture))
        terminal = Terminal(self.config, "secret-service", env)
        self.assertEqual(self.enter(terminal), 0)
        self.assertEqual(capture.read_bytes(), KEY)
        self.assertEqual(json.loads(Path(str(capture) + ".argv").read_text()),
                         ["store", "--label=JWM Eva OpenAI API", "service", "jwm-eva", "provider", "openai"])
        config = tomllib.loads(self.config.read_text())
        self.assertEqual(config["openai"]["api_key_source"], "secret_service")
        self.assertNotIn("api_key_file", config["openai"])
        self.assertNotIn(KEY, self.config.read_bytes())

    def test_keyring_failure_leaves_configuration_untouched(self):
        fake = self.directory / "bin"
        fake.mkdir()
        tool = fake / "secret-tool"
        tool.write_text("#!/bin/sh\ncat >&2\nexit 1\n")
        tool.chmod(0o700)
        self.config.write_text("# untouched\n[openai]\napi_key_source = 'env'\n")
        self.config.chmod(0o600)
        original = self.config.read_bytes()
        env = dict(os.environ, PATH=str(fake) + ":/usr/bin")
        terminal = Terminal(self.config, "secret-service", env)
        self.assertNotEqual(self.enter(terminal), 0)
        self.assertEqual(self.config.read_bytes(), original)

    def test_missing_keyring_tool_fails_before_requesting_key(self):
        terminal = Terminal(self.config, "secret-service", dict(os.environ, PATH="/nonexistent"))
        code, _ = terminal.finish()
        self.assertNotEqual(code, 0)
        self.assertNotIn(b"OpenAI API key", terminal.output)
        self.assertIn(b"secret-tool", terminal.output)
        self.assertFalse(self.config.exists())

    def test_redirected_key_without_a_terminal_is_rejected(self):
        result = subprocess.run(
            [BINARY, "--config", str(self.config), "config", "api-key", "--storage", "file"],
            input=KEY, capture_output=True, start_new_session=True, timeout=10,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(KEY, result.stdout + result.stderr)
        self.assertFalse(self.config.exists())

    def service_environment(self, restart_status=0, active_status=0):
        fake = self.directory / "bin"
        fake.mkdir()
        tool = fake / "systemctl"
        tool.write_text(
            "#!/usr/bin/python3\n"
            "import json,os,sys\n"
            "with open(os.environ['EVA_SERVICE_LOG'],'a') as f:\n"
            " f.write(json.dumps(sys.argv[1:])+'\\n')\n"
            "sys.stdout.write('suppressed service output')\n"
            "sys.stderr.write('suppressed service error')\n"
            "status='EVA_RESTART_STATUS' if sys.argv[2]=='restart' else 'EVA_ACTIVE_STATUS'\n"
            "sys.exit(int(os.environ[status]))\n"
        )
        tool.chmod(0o700)
        log = self.directory / "service.jsonl"
        return dict(os.environ, PATH=str(fake), EVA_SERVICE_LOG=str(log),
                    EVA_RESTART_STATUS=str(restart_status), EVA_ACTIVE_STATUS=str(active_status)), log

    def test_default_restarts_then_confirms_active_without_forwarding_output(self):
        env, log = self.service_environment()
        terminal = Terminal(self.config, "file", env, restart=True)
        self.assertEqual(self.enter(terminal), 0)
        self.assertEqual([json.loads(line) for line in log.read_text().splitlines()], [
            ["--user", "restart", "jwm-eva.service"],
            ["--user", "is-active", "--quiet", "jwm-eva.service"],
        ])
        self.assertIn("已重启并处于运行状态".encode(), terminal.output)
        self.assertNotIn(b"suppressed service", terminal.output)
        self.assertEqual((self.directory / "openai.key").read_bytes(), KEY)

    def test_restart_failure_retains_saved_key_and_reports_partial_success(self):
        env, log = self.service_environment(restart_status=1)
        terminal = Terminal(self.config, "file", env, restart=True)
        self.assertNotEqual(self.enter(terminal), 0)
        self.assertEqual([json.loads(line) for line in log.read_text().splitlines()], [
            ["--user", "restart", "jwm-eva.service"],
        ])
        self.assertIn("API key 和配置已保存，但服务重启失败".encode(), terminal.output)
        self.assertNotIn("已重启并处于运行状态".encode(), terminal.output)
        self.assertEqual((self.directory / "openai.key").read_bytes(), KEY)
        self.assertEqual(tomllib.loads(self.config.read_text())["openai"]["api_key_source"], "file")

    def test_inactive_service_is_not_reported_as_success(self):
        env, log = self.service_environment(active_status=3)
        terminal = Terminal(self.config, "file", env, restart=True)
        self.assertNotEqual(self.enter(terminal), 0)
        self.assertEqual(len(log.read_text().splitlines()), 2)
        self.assertIn("未确认处于运行状态".encode(), terminal.output)
        self.assertNotIn("已重启并处于运行状态".encode(), terminal.output)
        self.assertEqual((self.directory / "openai.key").read_bytes(), KEY)

    def test_no_restart_and_cancel_never_contact_service_manager(self):
        env, log = self.service_environment()
        terminal = Terminal(self.config, "file", env)
        self.assertEqual(self.enter(terminal), 0)
        self.assertIn("已跳过服务重启".encode(), terminal.output)
        self.assertFalse(log.exists())
        terminal = Terminal(self.config, "file", env, restart=True)
        terminal.until("OpenAI API key")
        terminal.send(b"\x03")
        self.assertNotEqual(terminal.finish()[0], 0)
        self.assertFalse(log.exists())

    def test_secret_file_symlink_is_rejected_without_touching_target(self):
        target = self.directory / "other-file"
        target.write_bytes(b"untouched")
        (self.directory / "openai.key").symlink_to(target)
        terminal = Terminal(self.config, "file")
        code, _ = terminal.finish()
        self.assertNotEqual(code, 0)
        self.assertNotIn(b"OpenAI API key", terminal.output)
        self.assertEqual(target.read_bytes(), b"untouched")
        self.assertFalse(self.config.exists())


if __name__ == "__main__":
    unittest.main()
