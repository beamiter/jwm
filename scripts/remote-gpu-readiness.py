#!/usr/bin/env python3
"""Read-only inventory and opt-in synthetic encode probes; never captures a screen."""
import argparse
import glob
import json
import os
import re
import shutil
import signal
import subprocess
import tempfile
import time

ENCODERS = ('h264_nvenc', 'hevc_nvenc', 'av1_nvenc', 'h264_vaapi', 'hevc_vaapi', 'av1_vaapi')
LIMIT = 1024 * 1024


def run_bounded(argv, timeout=10, limit=LIMIT):
    """Bound wall time, retained output and descendant lifetime; no shell."""
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        try:
            child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=out,
                                     stderr=err, start_new_session=True)
        except OSError:
            return {'status': 'unavailable', 'stdout': '', 'stderr': ''}
        deadline = time.monotonic() + timeout
        status = None
        try:
            while True:
                if os.fstat(out.fileno()).st_size + os.fstat(err.fileno()).st_size > limit:
                    status = 'output_limit'
                    break
                if child.poll() is not None:
                    status = 'passed' if child.returncode == 0 else 'failed'
                    break
                if time.monotonic() >= deadline:
                    status = 'timeout'
                    break
                time.sleep(.01)
        finally:
            # A child may have exited while leaving a descendant holding resources.
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
        out.seek(0)
        err.seek(0)
        return {'status': status, 'stdout': out.read(limit).decode('utf-8', 'replace'),
                'stderr': err.read(limit).decode('utf-8', 'replace')}


def parse_encoders(text):
    result = set()
    for line in text.splitlines():
        match = re.match(r'^\s*V[A-Z.]{5}\s+(\w+)\s', line)
        if match and match.group(1) in ENCODERS:
            result.add(match.group(1))
    return result


def probe_command(ffmpeg, encoder, render_node=None):
    if encoder not in ENCODERS:
        raise ValueError('unsupported encoder')
    args = [ffmpeg, '-hide_banner', '-loglevel', 'error', '-nostdin', '-nostats']
    if encoder.endswith('_vaapi'):
        if render_node is None:
            raise ValueError('VAAPI requires an explicit render node')
        args += ['-vaapi_device', render_node]
    args += ['-f', 'lavfi', '-i', 'color=c=black:s=128x128:r=30', '-frames:v', '3', '-an']
    if encoder.endswith('_vaapi'):
        args += ['-vf', 'format=nv12,hwupload']
    else:
        args += ['-pix_fmt', 'yuv420p']
    return args + ['-c:v', encoder, '-f', 'null', '-']


def collect(ffmpeg, probe=False, render_nodes=None, runner=run_bounded):
    nodes = sorted(set(render_nodes if render_nodes is not None else glob.glob('/dev/dri/renderD*')))
    report = {'schema_version': 1, 'synthetic_only': True,
              'screen_capture_attempted': False, 'dmabuf_import_verified': False,
              'remote_codec_negotiation_verified': False, 'end_to_end_zero_copy_verified': False,
              'render_nodes': nodes, 'encoders': [],
              'limitations': ['Encoder listing is build support, not usable hardware.',
                              'A successful probe uses CPU-generated pixels and may upload them.',
                              'No capture, cross-GPU import, fences, color fidelity or remote playback is tested.']}
    listing = runner([ffmpeg, '-hide_banner', '-encoders']) if ffmpeg else {'status': 'unavailable'}
    report['encoder_inventory_status'] = listing['status']
    advertised = parse_encoders(listing.get('stdout', '')) if listing['status'] == 'passed' else set()
    for encoder in ENCODERS:
        item = {'encoder': encoder, 'advertised': encoder in advertised, 'probes': []}
        if not probe:
            item['probe_status'] = 'not_requested'
        elif encoder not in advertised:
            item['probe_status'] = 'not_advertised' if listing['status'] == 'passed' else 'inventory_unavailable'
        elif encoder.endswith('_vaapi') and not nodes:
            item['probe_status'] = 'no_render_node'
        else:
            targets = nodes if encoder.endswith('_vaapi') else [None]
            for node in targets:
                result = runner(probe_command(ffmpeg, encoder, node))
                # Only ffmpeg error text, bounded; no env, screen pixels or host identity.
                item['probes'].append({'render_node': node, 'status': result['status'],
                                       'diagnostic': result.get('stderr', '')[:2048]})
            item['probe_status'] = 'completed'
        report['encoders'].append(item)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe-encode', action='store_true', help='try 3 generated black frames per advertised codec/device')
    parser.add_argument('--render-node', action='append', help='VAAPI node, e.g. /dev/dri/renderD128; repeat for multiple GPUs')
    args = parser.parse_args()
    if args.render_node and any(not re.fullmatch(r'/dev/dri/renderD\d+', n) for n in args.render_node):
        parser.error('render nodes must be /dev/dri/renderD<number>')
    print(json.dumps(collect(shutil.which('ffmpeg'), args.probe_encode, args.render_node), indent=2))


if __name__ == '__main__':
    main()
