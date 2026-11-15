#!/usr/bin/env python3
"""Private acceptance-only process observer; never used by the product."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys


def invocations(directory):
    return set(Path(directory).glob('*.json'))


def new_invocation(directory, before):
    paths = invocations(directory) - before
    if len(paths) > 1:
        raise RuntimeError(f'multiple media command invocations: {sorted(map(str, paths))}')
    return next(iter(paths), None)


def successful_result(path):
    record = json.loads(Path(path).read_text())
    code = record.get('returncode')
    if code is None:
        return None
    if code != 0:
        raise RuntimeError(f'media command failed: {record}')
    return record


def main():
    directory, log_path, output_mode, *command = sys.argv[1:]
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f'{os.getpid()}.json'
    record = {'observer_pid': os.getpid(), 'command': command}

    def publish():
        temporary = path.with_suffix('.tmp')
        temporary.write_text(json.dumps(record) + '\n')
        temporary.replace(path)

    # No new process group or environment: D-Bus and decoder identity remain
    # those of the real executable. Record its PID separately from this observer.
    with open(log_path, 'a') as log:
        try:
            child = subprocess.Popen(command, stderr=log,
                                     stdout=log if output_mode == 'both' else None)
        except OSError as error:
            record.update(returncode=127, error=str(error))
            publish()
            return 127
        record['child_pid'] = child.pid
        publish()

        def forward(signum, _frame):
            try:
                child.send_signal(signum)
            except ProcessLookupError:
                pass

        for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            signal.signal(signum, forward)
        code = child.wait()
        record['returncode'] = code
        publish()
        return code if code >= 0 else 128 - code


if __name__ == '__main__':
    sys.exit(main())
