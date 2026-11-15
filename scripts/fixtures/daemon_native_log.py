"""Read only native log files demonstrably open by the verified fixture JWM.

Pinned read descriptors retain the same inode through cleanup or rotation. No
wildcard scan or later PID lookup is used after the fixture terminates children.
"""
import hashlib
import os
from pathlib import Path
import re
import stat
import tempfile
import time


class NativeLogEvidence:
    def __init__(self, pid, expected_identity, executable, identity, started_at,
                 directories=None, proc_root=Path('/proc')):
        self.pid = pid
        self.expected_identity = expected_identity
        self.executable = Path(executable)
        self.identity = identity
        self.started_at = started_at
        self.proc = proc_root / str(pid)
        self.uid = os.geteuid()
        self.directories = directories if directories is not None else {
            Path('/var/tmp') / f'jwm-logs-{self.uid}',
            Path(tempfile.gettempdir()) / f'jwm-logs-{self.uid}',
        }
        self.pinned = {}
        self.errors = []

    def discover(self):
        # The caller constructs this only after checking daemon ancestry and
        # executable digest. Recheck live identity before and after inspection.
        try:
            if (self.identity(self.pid) != self.expected_identity
                    or (self.proc / 'exe').resolve() != self.executable):
                raise ValueError('verified JWM process identity changed')
            for entry in (self.proc / 'fd').iterdir():
                directory_fd = source_fd = None
                try:
                    source = Path(os.readlink(entry))
                    if source.parent not in self.directories:
                        continue
                    match = re.fullmatch(r'jwm_jwm_bar_global_(\d+)_p' + str(self.pid)
                                         + r'(?:[_.][A-Za-z0-9_.-]+)?\.log', source.name)
                    if not match or not self.started_at - 1 <= int(match[1]) <= time.time() + 1:
                        continue
                    directory_fd = os.open(source.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
                    directory = os.fstat(directory_fd)
                    if directory.st_uid != self.uid or stat.S_IMODE(directory.st_mode) != 0o700:
                        raise ValueError('unsafe native log directory')
                    source_fd = os.open(source.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                                        dir_fd=directory_fd)
                    metadata = os.fstat(source_fd)
                    active = entry.stat()
                    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != self.uid
                            or metadata.st_nlink != 1
                            or (metadata.st_dev, metadata.st_ino) != (active.st_dev, active.st_ino)):
                        raise ValueError('native log is not the verified open regular file')
                    if (self.identity(self.pid) != self.expected_identity
                            or (self.proc / 'exe').resolve() != self.executable):
                        raise ValueError('verified JWM process identity changed')
                    key = (metadata.st_dev, metadata.st_ino)
                    if key not in self.pinned:
                        self.pinned[key] = (source.name, source_fd)
                        source_fd = None
                except (OSError, ValueError) as error:
                    self.errors.append(str(error))
                finally:
                    if source_fd is not None:
                        os.close(source_fd)
                    if directory_fd is not None:
                        os.close(directory_fd)
        except (OSError, ValueError) as error:
            self.errors.append(str(error))

    def archive(self, evidence, phase):
        records = []
        for index, (name, fd) in enumerate(self.pinned.values()):
            try:
                # Bound diagnostics independently of the logger's 10 MB rotation.
                size = os.fstat(fd).st_size
                start = max(0, size - 10_000_000)
                data = os.pread(fd, size - start, start)
                destination = evidence / f'{phase}-native-{index}-{name}'
                destination.write_bytes(data)
                records.append({'status': 'saved', 'file': destination.name, 'bytes': len(data),
                                'source_bytes': size, 'tail_offset': start,
                                'sha256': hashlib.sha256(data).hexdigest()})
            except OSError as error:
                records.append({'status': 'error', 'error': str(error)})
        return {'files': records, 'errors': list(self.errors)}

    def close(self):
        for _, fd in self.pinned.values():
            os.close(fd)
        self.pinned.clear()
