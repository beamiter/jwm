#!/usr/bin/env python3
"""Unpublished Linux-only owned Xorg preflight. No JWM test and no desktop reuse."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import time
import traceback

import preflight


def require(value, message):
    if not value:
        raise RuntimeError(message)


def authority(path, secret):
    # FamilyWild permits displayfd-selected number; file remains owner-only.
    fields = (b'', b'', b'MIT-MAGIC-COOKIE-1', secret)
    data = struct.pack('!H', 65535)
    data += b''.join(struct.pack('!H', len(v)) + v for v in fields)
    with open(path, 'xb') as file:
        os.chmod(path, 0o600)
        file.write(data)


class OwnedXorg:
    def __init__(self, work, out, executable, config):
        self.work, self.out = work, out
        self.executable, self.config = executable, config
        self.proc = None
        self.fds = []
        self.deadline = time.monotonic() + 90
        self.display = None
        self.phase = "initialization"
        self.operation = "initialization"
        self.resource_path = None
        self.log = None
        self.command_count = 0
        self.env = {'PATH': '/usr/bin:/bin', 'LANG': 'C', 'LC_ALL': 'C'}
        for var, directory in [('HOME', 'home'), ('XDG_CONFIG_HOME', 'config'),
                               ('XDG_CACHE_HOME', 'cache'), ('XDG_DATA_HOME', 'data'),
                               ('XDG_STATE_HOME', 'state'), ('XDG_RUNTIME_DIR', 'runtime')]:
            path = work / directory
            path.mkdir(mode=0o700)
            self.env[var] = str(path)
        self.auth = work / 'authority'
        authority(self.auth, secrets.token_bytes(16))
        self.env['XAUTHORITY'] = str(self.auth)

    def remaining(self, cap=10):
        value = min(cap, self.deadline - time.monotonic())
        require(value > 0, 'total preflight deadline exceeded')
        return value

    def verify_owner(self):
        require(self.proc is not None and self.proc.poll() is None, 'owned Xorg exited')
        require(self.display is not None, 'no displayfd allocation')
        # This is the actual server's Unix socket peer identity, not an env label.
        self.phase = 'peer-credentials'
        self.resource_path = '/tmp/.X11-unix/X' + self.display
        with socket.socket(socket.AF_UNIX) as peer:
            peer.settimeout(self.remaining(2))
            peer.connect(self.resource_path)
            pid, uid, _ = struct.unpack('3i', peer.getsockopt(
                socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize('3i')))
        require(pid == self.proc.pid and uid == os.getuid(), 'X socket peer is not owned child')
        # Xorg 21.1.12 os/utils.c:770-775 sets nolock for -displayfd.
        # A pathname lock is not part of this mode's ownership contract.
        self.phase = 'server-executable'
        self.resource_path = str(Path('/proc', str(pid), 'exe'))
        require(Path('/proc', str(pid), 'exe').resolve() == self.executable,
                'X server executable identity changed')
        require(self.proc.poll() is None, 'owned Xorg exited during identity check')

    def command(self, args, *, env=None, success=True):
        self.command_count += 1
        record = {'argv': args, 'status': 'failed', 'returncode': None,
                  'stdout': '', 'stderr': ''}
        try:
            self.verify_owner()
            result = subprocess.run(args, env=env or self.env, capture_output=True,
                                    text=True, timeout=self.remaining())
            record.update(returncode=result.returncode, stdout=result.stdout, stderr=result.stderr)
            self.verify_owner()
            if success:
                require(result.returncode == 0, f'{args[0]} failed: {result.stderr}')
            record['status'] = 'completed'
            return result
        except subprocess.TimeoutExpired as error:
            record['error'] = repr(error)
            for key, value in [('stdout', error.stdout), ('stderr', error.stderr)]:
                record[key] = value.decode(errors='replace') if isinstance(value, bytes) else value or ''
            raise
        except BaseException as error:
            record['error'] = repr(error)
            raise
        finally:
            (self.out / f'command-{self.command_count:03d}.json').write_text(json.dumps(record, indent=2))

    def xr(self, *args):
        self.operation = 'xrandr'
        return self.command(['/usr/bin/xrandr', *args]).stdout

    def start(self):
        self.phase = 'displayfd-setup'
        read_fd, write_fd = os.pipe()
        self.fds.extend([read_fd, write_fd])
        self.log = open(self.out / 'server-stdio.log', 'xb')
        self.proc = subprocess.Popen(
            [str(self.executable), '-displayfd', str(write_fd),
             '-config', str(self.config), '-logfile', str(self.out / 'Xorg.log'),
             '-auth', str(self.auth), '-nolisten', 'tcp', '-noreset', '-novtswitch'],
            env=self.env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
            pass_fds=(write_fd,), start_new_session=True)
        os.close(write_fd); self.fds.remove(write_fd)
        self.phase = 'displayfd-readiness'
        self.resource_path = None
        allocation = b''
        startup_end = time.monotonic() + 15
        while b'\n' not in allocation:
            require(self.proc.poll() is None, 'Xorg exited before displayfd readiness')
            require(time.monotonic() < startup_end, 'displayfd readiness timeout')
            if select.select([read_fd], [], [], min(.2, self.remaining()))[0]:
                chunk = os.read(read_fd, 32)
                require(chunk, 'displayfd closed without allocation')
                allocation += chunk
                require(len(allocation) <= 16, 'invalid displayfd response')
        require(allocation.endswith(b'\n') and allocation[:-1].isdigit(),
                'displayfd returned malformed display')
        self.display = str(int(allocation[:-1]))
        os.close(read_fd); self.fds.remove(read_fd)
        self.env['DISPLAY'] = ':' + self.display
        self.verify_owner()
        # A server with access control disabled would falsely accept wrong cookies.
        bad_auth = self.work / 'wrong-authority'
        authority(bad_auth, secrets.token_bytes(16))
        self.operation = 'wrong-cookie-negative-control'
        denied = self.command(['/usr/bin/xdpyinfo'],
                              env={**self.env, 'XAUTHORITY': str(bad_auth)}, success=False)
        require(denied.returncode != 0, 'private server accepted wrong authorization cookie')
        self.operation = 'private-cookie-positive-control'
        good = self.command(['/usr/bin/xdpyinfo'])
        (self.out / 'xdpyinfo.txt').write_text(good.stdout)
        (self.out / 'ownership.json').write_text(json.dumps({
            'pid': self.proc.pid, 'uid': os.getuid(), 'display': self.display,
            'lock_contract': 'not used: Xorg displayfd explicitly disables lock creation',
            'binary': str(self.executable),
            'binary_sha256': hashlib.sha256(self.executable.read_bytes()).hexdigest(),
            'checks': ['displayfd inherited pipe', 'SO_PEERCRED PID+UID',
                       '/proc executable', 'wrong-cookie rejected',
                       'private-cookie accepted']}, indent=2))

    def close(self):
        errors = []
        for fd in self.fds:
            try:
                os.close(fd)
            except BaseException as error:
                errors.append(repr(error))
        self.fds.clear()
        try:
            if self.proc is not None:
                if self.proc.poll() is None:
                    os.killpg(self.proc.pid, signal.SIGTERM)
                    try:
                        self.proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(self.proc.pid, signal.SIGKILL)
                        self.proc.wait(timeout=5)
                else:
                    self.proc.wait()
        except BaseException as error:
            errors.append(repr(error))
        try:
            if self.log is not None:
                self.log.close()
        except BaseException as error:
            errors.append(repr(error))
        require(not errors, 'cleanup failed: ' + '; '.join(errors))


def inputs(out):
    executable = Path('/usr/lib/xorg/Xorg').resolve(strict=True)
    config = Path(__file__).with_name('xorg.conf').resolve(strict=True)
    shutil.copyfile(config, out / 'xorg.conf')
    return executable, config


def save_report(out, report):
    staged = out / 'preflight.json.new'
    staged.write_text(json.dumps(report, indent=2))
    staged.replace(out / 'preflight.json')


def error_detail(error, stage, server=None):
    # No environment, auth bytes, or frame locals are serialized.
    return {'stage': stage, 'error': repr(error),
            'filename': getattr(error, 'filename', None),
            'filename2': getattr(error, 'filename2', None),
            'phase': getattr(server, 'phase', None),
            'operation': getattr(server, 'operation', None),
            'resource_path': getattr(server, 'resource_path', None),
            'traceback': ''.join(traceback.format_exception(type(error), error, error.__traceback__))}


def execute(out):
    # Persist failure before setup, so abrupt termination never leaves a pass.
    report = {'status': 'failed', 'scope': 'owned dummy preflight only',
              'stage': 'setup', 'errors': []}
    save_report(out, report)
    server = None
    tmp = None
    candidate = None
    try:
        executable, config = inputs(out)
        tmp = tempfile.TemporaryDirectory(prefix='jwm-randr-owned-')
        server = OwnedXorg(Path(tmp.name), out, executable, config)
        report['stage'] = 'server-start'
        server.start()
        report['stage'] = 'output-preflight'
        candidate = preflight.run(out, server.xr)
        require(candidate.get('status') == 'passed', 'preflight did not pass')
    except BaseException as error:
        report['errors'].append(error_detail(error, report['stage'], server))
    finally:
        # Best effort for every cleanup step, including setup/constructor failures.
        if server is not None:
            try:
                server.close()
            except BaseException as error:
                report['errors'].append(error_detail(error, 'server-cleanup', server))
        if tmp is not None:
            try:
                tmp.cleanup()
            except BaseException as error:
                report['errors'].append(error_detail(error, 'directory-cleanup'))
        if not report['errors'] and candidate is not None:
            report.update(candidate)
            report['stage'] = 'complete'
        else:
            report['status'] = 'failed'
        save_report(out, report)
    return 0 if report['status'] == 'passed' else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    # Refuse overwriting existing evidence. If this fails, no safe destination
    # exists: exit nonzero without altering someone else's evidence directory.
    out = args.output.resolve()
    out.mkdir(mode=0o700, parents=False, exist_ok=False)
    def interrupted(signum, frame):
        raise RuntimeError(f'preflight interrupted by signal {signum}')
    previous = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        return execute(out)
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == '__main__':
    raise SystemExit(main())
