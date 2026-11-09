#!/usr/bin/env python3
"""Bounded offline FFmpeg/libx264 experiment. No display, sockets or GPU probes."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import resource
import selectors
import shutil
import subprocess
import sys
import time

CAP = 64 * 1024 * 1024
SCENES = ('idle', 'sparse_change', 'scroll', 'full_change', 'resize')

def require(ok, message):
    if not ok:
        raise ValueError(message)

def bounded_read(path, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        import stat
        require(stat.S_ISREG(os.fstat(fd).st_mode), 'input must be a regular file')
        with os.fdopen(fd, 'rb', closefd=False) as handle:
            data = handle.read(limit + 1)
        require(len(data) <= limit, 'input exceeded bounded size')
        return data
    finally:
        os.close(fd)

def load_fixture(root):
    root = root.resolve(strict=True)
    path = root / 'manifest.json'
    require(not path.is_symlink() and path.stat().st_size <= 1024 * 1024, 'invalid manifest file')
    manifest = json.loads(bounded_read(path, 1024 * 1024))
    require(manifest.get('schema_version') == 1 and manifest.get('workload_version') == 1
            and manifest.get('producer') == 'jwm-production-replay'
            and manifest.get('pixel_format') == 'rgb24', 'not a production replay fixture')
    config = manifest['config']
    require(1 <= config['width'] <= 1920 and 1 <= config['height'] <= 1080
            and 1 <= config['frames'] <= 300 and config['warmup'] == 0, 'invalid replay config')
    baseline = manifest['tile_baseline']
    require(baseline.get('config') == config and baseline.get('workload_version') == 1
            and baseline.get('scope') == 'offline_synthetic_tile_codec_only', 'tile baseline/config mismatch')
    entries = manifest['frames']
    require(len(entries) == config['frames'] * 5, 'fixture frame count mismatch')
    total = 0
    seen = set()
    for item in entries:
        scene, index = item['scene'], item['index']
        require(scene in SCENES and type(index) is int and 0 <= index < config['frames'], 'invalid scene/index')
        require((scene, index) not in seen, 'duplicate frame'); seen.add((scene, index))
        resized = scene == 'resize' and (index // 3) % 2 == 1
        w = max(1, config['width'] - 17) if resized else config['width']
        h = max(1, config['height'] - 9) if resized else config['height']
        require((item['width'], item['height'], item['geometry_epoch']) ==
                (w, h, index // 3 if scene == 'resize' else 0), 'unexpected frame geometry/epoch')
        name = item['file']
        require(isinstance(name, str) and Path(name).name == name and name.endswith('.rgb'), 'unsafe fixture path')
        file = root / name
        size = w * h * 3
        require(not file.is_symlink() and file.is_file() and file.stat().st_size == size, 'wrong RGB file size/type')
        total += size
        require(total <= CAP, 'RGB fixture exceeds 64 MiB')
        require(hashlib.sha256(bounded_read(file, size)).hexdigest() == item['sha256'], 'RGB checksum mismatch')
    return manifest

def segments(entries):
    groups = []
    for scene in SCENES:
        for item in sorted((e for e in entries if e['scene'] == scene), key=lambda e: e['index']):
            scope = (scene, item['width'], item['height'], item['geometry_epoch'])
            if not groups or groups[-1][0] != scope:
                groups.append((scope, []))
            groups[-1][1].append(item)
    require(len(groups) <= 25, 'at most 25 independent segments per run')
    return groups

def child_limits():
    resource.setrlimit(resource.RLIMIT_FSIZE, (CAP, CAP))
    resource.setrlimit(resource.RLIMIT_CPU, (30, 30))

def run_process(command, stderr_path, timeout=30):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.monotonic_ns()
    samples, chunks, buffer = [], [], b''
    with stderr_path.open('xb') as err:
        proc = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=err, preexec_fn=child_limits)
        selector = selectors.DefaultSelector()
        selector.register(proc.stdout, selectors.EVENT_READ)
        try:
            while selector.get_map():
                require((time.monotonic_ns() - start) < timeout * 1e9, 'FFmpeg timeout')
                for key, _ in selector.select(0.1):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    observed = time.monotonic_ns() - start
                    chunks.append({'at_ns': observed, 'bytes': len(chunk)})
                    buffer += chunk
                    require(sum(c['bytes'] for c in chunks) < 1024 * 1024, 'excessive packet metadata')
                    while b'\n' in buffer:
                        line, buffer = buffer.split(b'\n', 1)
                        if line and not line.startswith(b'#'):
                            samples.append({'observed_ns': observed, 'read_batch': len(chunks) - 1,
                                            'framecrc': line.decode('ascii')})
            remaining = timeout - (time.monotonic_ns() - start) / 1e9
            require(remaining > 0, 'FFmpeg timeout during finalization')
            require(proc.wait(timeout=remaining) == 0, 'FFmpeg failed; inspect bounded stderr log')
            require(stderr_path.stat().st_size <= 256 * 1024, 'excessive FFmpeg diagnostics')
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            selector.close()
            proc.stdout.close()
    elapsed = time.monotonic_ns() - start
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    return {'wall_ns': elapsed, 'child_user_s': after.ru_utime - before.ru_utime,
            'child_system_s': after.ru_stime - before.ru_stime,
            'child_rss_high_water_kib': after.ru_maxrss,
            'rss_scope': 'cumulative maximum across this script children; not per-run RSS',
            'packet_observations': samples, 'read_batches': chunks,
            'startup_to_first_packet_observed_ns': samples[0]['observed_ns'] if samples else None,
            'post_first_packet_observation_intervals_ns':
                [b['observed_ns'] - a['observed_ns'] for a, b in zip(samples, samples[1:])],
            'observation_warning': 'pipe/muxer/process buffering and scheduling included; these are NOT frame encode or end-to-end latency'}

def rgb_error(original, decoded):
    require(len(original) == len(decoded) and bool(original), 'decoded RGB length mismatch')
    squared, maximum, changed = 0, 0, 0
    for a, b in zip(original, decoded):
        delta = abs(a - b); squared += delta * delta; maximum = max(maximum, delta); changed += delta != 0
    mse = squared / len(original)
    return {'rgb_mse': mse, 'rgb_psnr_db': 10 * math.log10(255 * 255 / mse) if mse else None,
            'lossless': mse == 0, 'max_channel_error': maximum, 'changed_channel_fraction': changed / len(original)}

def run(args):
    require(1 <= args.trials <= 3 and 0 <= args.warmup_runs <= 1 and 0 <= args.crf <= 51, 'invalid trial/warmup/CRF limit')
    manifest = load_fixture(args.fixture)
    groups = segments(manifest['frames'])
    ffmpeg = shutil.which('ffmpeg'); require(ffmpeg is not None, 'FFmpeg must already be installed')
    require(not args.output.exists(), 'output directory must not exist')
    require(not any(char in str(args.output.resolve()) for char in "[]|:'\\"), 'output path contains FFmpeg tee metacharacters')
    args.output.mkdir()
    version = subprocess.run([ffmpeg, '-hide_banner', '-version'], capture_output=True, text=True, check=True, timeout=5).stdout
    report = {'schema_version': 1, 'scope': 'offline_software_h264_not_gpu_or_desktop_latency',
              'config': manifest['config'], 'fixture_manifest_sha256': hashlib.sha256(bounded_read(args.fixture / 'manifest.json', 1024 * 1024)).hexdigest(),
              'ffmpeg_version': version, 'machine': platform.machine(), 'python_version': platform.python_version(), 'logical_cpu_count': os.cpu_count(), 'affinity_cpu_count': len(os.sched_getaffinity(0)), 'codec': 'libx264', 'crf': args.crf, 'threads': 1,
              'warmup_runs': args.warmup_runs, 'trials': args.trials,
              'tile_baseline': manifest['tile_baseline'],
              'comparability': 'identical production RGB generator; CRF and JPEG quality are NOT matched quality; tile timings exclude RGB materialization, FFmpeg includes conversion/IPC; tile bytes include tile headers, H264 bytes include SPS/PPS/AnnexB',
              'runs': []}
    common = [ffmpeg, '-hide_banner', '-loglevel', 'error', '-nostdin', '-n', '-protocol_whitelist', 'file,pipe', '-threads', '1', '-filter_threads', '1']
    total_start = time.monotonic()
    for ordinal, (scope, entries) in enumerate(groups):
        require(time.monotonic() - total_start < 180, 'aggregate runtime budget exceeded')
        scene, width, height, epoch = scope
        raw = b''.join(bounded_read(args.fixture / item['file'], item['width'] * item['height'] * 3) for item in entries)
        require(all(hashlib.sha256(raw[i*width*height*3:(i+1)*width*height*3]).hexdigest() == item['sha256'] for i, item in enumerate(entries)), 'fixture changed after validation')
        source = args.output / f'segment-{ordinal}.rgb'
        with source.open('xb') as output:
            output.write(raw)
        for trial in range(-args.warmup_runs, args.trials):
            prefix = args.output / f'{ordinal}-{trial}'
            bitstream = prefix.with_suffix('.h264')
            # Each geometry segment starts fresh/IDR. No B frames or lookahead.
            command = common + ['-f', 'rawvideo', '-pixel_format', 'rgb24', '-video_size', f'{width}x{height}', '-framerate', '60', '-i', str(source.resolve()),
                       '-map', '0:v', '-vf', 'pad=ceil(iw/2)*2:ceil(ih/2)*2,format=yuv420p',
                       '-c:v', 'libx264', '-preset', 'ultrafast', '-tune', 'zerolatency', '-crf', str(args.crf),
                       '-bf', '0', '-g', '60', '-keyint_min', '60', '-sc_threshold', '0',
                       '-x264-params', 'threads=1:rc-lookahead=0:sync-lookahead=0:bframes=0:repeat-headers=1:aud=1',
                       '-f', 'tee', f'[f=h264]{bitstream.resolve()}|[f=framecrc:flush_packets=1]pipe:1']
            timing = run_process(command, prefix.with_suffix('.encode.log'))
            require(len(timing['packet_observations']) == len(entries), 'packet/frame count mismatch')
            decoded = prefix.with_suffix('.decoded.rgb')
            decode_command = common + ['-i', str(bitstream.resolve()), '-threads', '1', '-vf', f'format=rgb24,crop={width}:{height}:0:0', '-f', 'rawvideo', str(decoded.resolve())]
            decode_timing = run_process(decode_command, prefix.with_suffix('.decode.log'))
            decoded_rgb = bounded_read(decoded, CAP)
            errors = [rgb_error(raw[i:i+width*height*3], decoded_rgb[i:i+width*height*3]) for i in range(0,len(raw),width*height*3)]
            require(len(decoded_rgb) == len(raw), 'decoded frame count mismatch')
            report['runs'].append({'scene':scene, 'geometry_epoch':epoch, 'width':width,'height':height,
                'frames':len(entries), 'trial':trial, 'warmup':trial<0,'encode_command':command,'decode_command':decode_command,
                'encoded_bytes':bitstream.stat().st_size,'encode_process':timing,'decode_process':decode_timing,'per_frame_quality':errors})
            # Retain all measurements/logs but bound workspace: discard only this
            # script's explicitly owned intermediate files, never input fixtures.
            bitstream.unlink(); decoded.unlink()
        source.unlink()
    report['aggregate_wall_s'] = time.monotonic()-total_start
    with (args.output / 'report.json').open('x') as output:
        json.dump(report, output, indent=2, allow_nan=False)
    print(args.output / 'report.json')

def self_test():
    require(rgb_error(b'abc',b'abc')['lossless'], 'equal RGB')
    require(rgb_error(b'\x00',b'\xff')['rgb_mse'] == 65025, 'RGB error')
    e = [{'scene':'resize','index':i,'width':33 if i<3 else 16,'height':25 if i<3 else 16,'geometry_epoch':i//3} for i in range(4)]
    require([len(group[1]) for group in segments(e)] == [3,1], 'resize segmentation')
    for action in (lambda: require(False, 'expected'), lambda: rgb_error(b'a', b'bc'), lambda: segments([{'scene':'resize','index':i,'width':1,'height':1,'geometry_epoch':i} for i in range(26)])):
        try: action()
        except ValueError: pass
        else: raise AssertionError('expected validation rejection')
    print('self-test passed (pure; no processes/files/sockets)')

if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test',action='store_true')
    parser.add_argument('--fixture',type=Path)
    parser.add_argument('--output',type=Path)
    parser.add_argument('--trials',type=int,default=1)
    parser.add_argument('--warmup-runs',type=int,default=0)
    parser.add_argument('--crf',type=int,default=23)
    args=parser.parse_args()
    if args.self_test: self_test()
    else:
        parser.error('--fixture and --output required') if args.fixture is None or args.output is None else None
        try: run(args)
        except (ValueError,OSError,subprocess.SubprocessError,KeyError,TypeError) as error:
            print(f'benchmark failed: {error}',file=sys.stderr);sys.exit(1)
