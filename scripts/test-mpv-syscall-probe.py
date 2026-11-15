#!/usr/bin/env python3
"""No browser/ptrace required: guard and argv contracts for private tracing."""
from pathlib import Path
import unittest
from unittest.mock import patch
from fixtures.mpv_syscall_probe import guarded_exec, launcher_prefix


class ProbeTests(unittest.TestCase):
    def test_missing_tool_is_error(self):
        with patch('fixtures.mpv_syscall_probe.shutil.which', return_value=None):
            with self.assertRaisesRegex(RuntimeError, 'requires strace'):
                launcher_prefix(Path('/tmp/private evidence'))

    def check_guard(self, tracer, pid=123, parent=45):
        with patch('fixtures.mpv_syscall_probe.Path.read_text', return_value=f'TracerPid:\t{tracer}\n'), \
             patch('fixtures.mpv_syscall_probe.os.getpid', return_value=pid), \
             patch('fixtures.mpv_syscall_probe.os.getppid', return_value=parent), \
             patch('fixtures.mpv_syscall_probe.os.execv') as execute:
            if tracer and pid == 123 and parent == 45:
                guarded_exec(['123', '45', '/usr/bin/mpv', '--input-ipc-client=fd://0', 'two words'])
                execute.assert_called_once_with('/usr/bin/mpv', ['/usr/bin/mpv', '--input-ipc-client=fd://0', 'two words'])
            else:
                with self.assertRaisesRegex(RuntimeError, 'refusing untraced execution'):
                    guarded_exec(['123', '45', '/usr/bin/mpv'])
                execute.assert_not_called()

    def test_no_tracer_is_error(self):
        self.check_guard(0)

    def test_changed_identity_is_error(self):
        self.check_guard(99, pid=124)
        self.check_guard(99, parent=46)

    def test_guard_preserves_program_and_arguments(self):
        self.check_guard(99)


if __name__ == '__main__':
    unittest.main()
