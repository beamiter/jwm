#!/usr/bin/env python3
"""Bounded, read-only benchmark context. No performance measurements."""
import argparse
import json
import os
from pathlib import Path
import re
import stat
import sys

CPU_BYTES = 65536
GPU_BYTES = 32
GPU_SLOTS = 32
MODEL_CHARS = 160
BACKENDS = ('x11', 'x11rb', 'xcb', 'wayland', 'wayland-udev',
            'wayland-x11', 'wayland-winit', 'remote-x11', 'unknown')


def read_bounded(path, limit, root):
    """Only regular files resolving inside root; never read unbounded content.

    Sysfs uses symlinked parent directories. Resolve those within the approved
    root, refuse a symlink at the final open, and refuse special files/FIFOs.
    The real /proc and /sys trees must be OS-managed, not attacker-controlled.
    """
    try:
        resolved = path.resolve(strict=True)
        resolved.relative_to(root.resolve(strict=True))
        fd = os.open(resolved, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
        with os.fdopen(fd, 'rb', buffering=0) as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                return None
            return stream.read(limit)
    except (OSError, ValueError, RuntimeError, AttributeError):
        return None


def cpu_model(proc_root):
    data = read_bounded(proc_root / 'cpuinfo', CPU_BYTES, proc_root)
    if data is None:
        return None
    # A final partial line at the byte bound must never be treated as a model.
    for line in data.split(b'\n')[:-1]:
        key, sep, value = line.partition(b':')
        if sep and key.strip() in (b'model name', b'Hardware'):
            value = value.strip()
            if not 1 <= len(value) <= MODEL_CHARS:
                return None
            if any(c < 32 or c > 126 for c in value):
                return None
            return value.decode('ascii')
    return None


def gpu_ids(sys_root):
    result = []
    # Fixed probes avoid unbounded directory enumeration. No PCI addresses,
    # serials, driver dumps, EDID, monitor names or display connections.
    for slot in range(GPU_SLOTS):
        ids = {}
        for field in ('vendor', 'device', 'subsystem_vendor', 'subsystem_device'):
            data = read_bounded(sys_root / 'class/drm' / ('card%d' % slot)
                                / 'device' / field, GPU_BYTES, sys_root)
            value = data.strip() if data else b''
            ids[field] = (value.decode('ascii').lower()
                          if re.fullmatch(rb'0x[0-9a-fA-F]{4}', value) else None)
        if ids['vendor'] is not None and ids['device'] is not None:
            result.append(ids)
    return result or None


def label(value):
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._+-]{0,95}', value):
        raise argparse.ArgumentTypeError('use 1-96 ASCII letters, digits, . _ + or -')
    return value


def display_size(value):
    match = re.fullmatch(r'([1-9][0-9]{0,4})x([1-9][0-9]{0,4})', value)
    if not match or any(int(v) > 32768 for v in match.groups()):
        raise argparse.ArgumentTypeError('use WIDTHxHEIGHT, each dimension 1-32768')
    return {'width': int(match[1]), 'height': int(match[2])}


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--backend', choices=BACKENDS, default=None,
                   help='operator-declared backend; not auto-detected')
    p.add_argument('--display-size', type=display_size,
                   help='operator-declared benchmark dimensions; no display access')
    p.add_argument('--build-label', type=label,
                   help='public revision or build tag, never a secret or personal identifier')
    p.add_argument('--include-gpu', action='store_true',
                   help='include bounded Linux sysfs PCI vendor/device identifiers')
    return p


def collect(args, proc_root=Path('/proc'), sys_root=Path('/sys')):
    linux = sys.platform.startswith('linux')
    try:
        count = os.cpu_count()
    except (OSError, NotImplementedError):
        count = None
    return {
        'schema_version': 1,
        'kind': 'benchmark_context_not_performance',
        'os': {'family': sys.platform},
        'cpu': {'logical_count': count, 'model': cpu_model(proc_root) if linux else None},
        'gpu': {'requested': args.include_gpu,
                'pci_ids': gpu_ids(sys_root) if args.include_gpu and linux else None},
        'declared': {'backend': args.backend, 'display_size': args.display_size,
                     'build_label': args.build_label},
    }


def main():
    print(json.dumps(collect(parser().parse_args()), indent=2, sort_keys=True))


if __name__ == '__main__':
    main()
