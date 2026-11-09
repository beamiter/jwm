#!/usr/bin/env python3
"""Bounded offline analysis of complete production-tile and software-H264 evidence."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import sys
from fractions import Fraction

SCENES = ('idle', 'sparse_change', 'scroll', 'full_change', 'resize')
IDENTITY = {'scene', 'index', 'width', 'height', 'geometry_epoch', 'sha256'}
METRICS = {'sse', 'channel_samples', 'changed_count', 'max_channel_error'}
MANIFEST_CAP = 4 * 1024 * 1024
REPORT_CAP = 8 * 1024 * 1024
TOTAL_CAP = 36 * 1024 * 1024
RGB_CAP = 64 * 1024 * 1024


def require(ok, message):
    if not ok:
        raise ValueError(message)


def integer(value, low, high):
    require(type(value) is int and low <= value <= high, 'invalid bounded integer')
    return value


def keys(value, expected):
    require(type(value) is dict and set(value) == set(expected), 'unknown or missing fields')


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate JSON key')
        result[key] = value
    return result


def parse(data):
    def bad_constant(_):
        raise ValueError('non-finite JSON constant')
    result = json.loads(data, object_pairs_hook=unique_object, parse_constant=bad_constant)
    def visit(value, depth=0):
        require(depth <= 32, 'excessive JSON nesting')
        if isinstance(value, float):
            require(math.isfinite(value), 'non-finite number')
        elif isinstance(value, dict):
            for child in value.values():
                visit(child, depth + 1)
        elif isinstance(value, list):
            for child in value:
                visit(child, depth + 1)
    visit(result)
    return result


def read(path, cap):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        require(stat.S_ISREG(os.fstat(fd).st_mode), 'input is not a regular file')
        with os.fdopen(fd, 'rb', closefd=False) as stream:
            data = stream.read(cap + 1)
        require(len(data) <= cap, 'input file exceeds bound')
        return data
    finally:
        os.close(fd)


def config(value):
    keys(value, {'width', 'height', 'frames', 'warmup', 'seed', 'quality'})
    integer(value['width'], 1, 1920)
    integer(value['height'], 1, 1080)
    integer(value['frames'], 1, 300)
    integer(value['warmup'], 0, 0)
    integer(value['seed'], 0, 2**64 - 1)
    integer(value['quality'], 1, 100)


def identities(entries, cfg, files=False):
    require(type(entries) is list and len(entries) == 5 * cfg['frames'], 'incomplete frame timeline')
    result, total = [], 0
    for ordinal, entry in enumerate(entries):
        keys(entry, IDENTITY | ({'file'} if files else set()))
        scene, index = SCENES[ordinal // cfg['frames']], ordinal % cfg['frames']
        resized = scene == 'resize' and (index // 3) % 2 == 1
        width = max(1, cfg['width'] - 17) if resized else cfg['width']
        height = max(1, cfg['height'] - 9) if resized else cfg['height']
        epoch = index // 3 if scene == 'resize' else 0
        require(entry['scene'] == scene, 'noncanonical scene order')
        for name, expected in [('index', index), ('width', width), ('height', height), ('geometry_epoch', epoch)]:
            require(type(entry[name]) is int and entry[name] == expected, 'noncanonical index/dimensions/epoch')
        require(type(entry['sha256']) is str and re.fullmatch('[0-9a-f]{64}', entry['sha256']), 'invalid RGB hash')
        if files:
            require(entry['file'] == f'{scene}-{index:04}.rgb', 'noncanonical fixture filename')
        total += width * height * 3
        require(total <= RGB_CAP, 'RGB timeline exceeds 64 MiB')
        result.append({name: entry[name] for name in IDENTITY})
    return result


def validate_identity(record, expected):
    for key in ('index', 'width', 'height', 'geometry_epoch'):
        require(type(record[key]) is int, 'boolean/noninteger frame identity')
    require({key: record[key] for key in IDENTITY} == expected, 'frame identity mismatch')


def expected_commands(ffmpeg, source, bitstream, decoded, width, height, crf):
    common = [ffmpeg, '-hide_banner', '-loglevel', 'error', '-nostdin', '-n',
              '-protocol_whitelist', 'file,pipe', '-threads', '1', '-filter_threads', '1']
    encode = common + ['-f', 'rawvideo', '-pixel_format', 'rgb24', '-video_size', f'{width}x{height}',
        '-framerate', '60', '-i', source, '-map', '0:v', '-vf', 'pad=ceil(iw/2)*2:ceil(ih/2)*2,format=yuv420p',
        '-c:v', 'libx264', '-preset', 'ultrafast', '-tune', 'zerolatency', '-crf', str(crf), '-bf', '0',
        '-g', '60', '-keyint_min', '60', '-sc_threshold', '0', '-x264-params',
        'threads=1:rc-lookahead=0:sync-lookahead=0:bframes=0:repeat-headers=1:aud=1',
        '-f', 'tee', f'[f=h264]{bitstream}|[f=framecrc:flush_packets=1]pipe:1']
    decode = common + ['-i', bitstream, '-threads', '1', '-vf', f'format=rgb24,crop={width}:{height}:0:0', '-f', 'rawvideo', decoded]
    return encode, decode


def validate_commands(run, crf):
    encode, decode = run['encode_command'], run['decode_command']
    for command in (encode, decode):
        require(type(command) is list and 1 <= len(command) <= 100
                and all(type(arg) is str and 0 < len(arg) <= 8192 for arg in command), 'invalid command evidence')
        require(command.count('-i') == 1, 'ambiguous input command')
        require(command.index('-i') + 1 < len(command), 'missing input command operand')
    expected = expected_commands(encode[0], encode[encode.index('-i') + 1],
        decode[decode.index('-i') + 1], decode[-1], run['width'], run['height'], crf)
    require((encode, decode) == expected, 'codec commands contradict fixed measured profile')

def metric(record, identity):
    n = identity['width'] * identity['height'] * 3
    require(integer(record['channel_samples'], 1, RGB_CAP) == n, 'channel count mismatch')
    changed = integer(record['changed_count'], 0, n)
    maximum = integer(record['max_channel_error'], 0, 255)
    sse = integer(record['sse'], 0, 65025 * n)
    require((changed == 0) == (maximum == 0) == (sse == 0), 'inconsistent zero distortion')
    if changed:
        require(maximum**2 + changed - 1 <= sse <= changed * maximum**2,
                'inconsistent SSE/changed channels/max error')
    return {name: record[name] for name in METRICS}


def point(candidate, codec, setting, trial, scene, frames, encoded_bytes):
    sse = sum(frame['sse'] for frame in frames)
    samples = sum(frame['channel_samples'] for frame in frames)
    return {'candidate': candidate, 'codec': codec, 'setting': setting, 'trial': trial,
            'scene': scene, 'encoded_bytes': encoded_bytes, 'sse': sse,
            'channel_samples': samples, 'mse': sse / samples,
            'changed_count': sum(frame['changed_count'] for frame in frames),
            'max_channel_error': max(frame['max_channel_error'] for frame in frames),
            'worst_frame_mse': max(frame['sse'] / frame['channel_samples'] for frame in frames),
            'per_frame': frames}


def validate_baseline(baseline, cfg):
    keys(baseline, {'schema_version', 'workload_version', 'scope', 'build_commit', 'debug_assertions', 'config', 'scenes'})
    require(type(baseline['schema_version']) is int and baseline['schema_version'] == 1
            and type(baseline['workload_version']) is int and baseline['workload_version'] == 1
            and baseline['config'] == cfg and baseline['scope'] == 'offline_synthetic_tile_codec_only', 'baseline schema/config')
    config(baseline['config'])
    require(type(baseline['debug_assertions']) is bool and type(baseline['build_commit']) is str, 'baseline build evidence')
    require(type(baseline['scenes']) is list and len(baseline['scenes']) == 5, 'baseline scene count')
    for name, scene in zip(SCENES, baseline['scenes']):
        stages = {'plan', 'encode', 'atlas_pack', 'jpeg', 'commit', 'decode'}
        keys(scene, {'scene', 'frames', 'emitted_frames', 'suppressed_frames', 'encoded_bytes', 'dirty_tiles', 'keyframes'} | stages)
        require(scene['scene'] == name and integer(scene['frames'], 1, 300) == cfg['frames'], 'baseline frames')
        for key in ('emitted_frames', 'suppressed_frames', 'keyframes'):
            integer(scene[key], 0, cfg['frames'])
        require(scene['emitted_frames'] + scene['suppressed_frames'] == cfg['frames'], 'baseline accounting')
        integer(scene['encoded_bytes'], 0, RGB_CAP * 300)
        integer(scene['dirty_tiles'], 0, 2**63 - 1)
        for stage in stages:
            timing = scene[stage]
            keys(timing, {'samples', 'raw_us', 'mean_us', 'p50_us', 'p95_us', 'p99_us'})
            count = integer(timing['samples'], 0, cfg['frames'])
            require(type(timing['raw_us']) is list and len(timing['raw_us']) == count, 'timing sample count')
            require(all(type(v) in (int, float) and math.isfinite(v) and v >= 0 for v in timing['raw_us']), 'invalid timing')


def validate_packets(process, count):
    keys(process, {'wall_ns', 'child_user_s', 'child_system_s', 'child_rss_high_water_kib',
                   'rss_scope', 'packet_observations', 'read_batches', 'startup_to_first_packet_observed_ns',
                   'post_first_packet_observation_intervals_ns', 'observation_warning'})
    packets = process['packet_observations']
    require(type(packets) is list and len(packets) == count, 'packet count mismatch')
    require(type(process['read_batches']) is list and len(process['read_batches']) <= 16384, 'read batch limit')
    for index, sample in enumerate(packets):
        keys(sample, {'observed_ns', 'read_batch', 'framecrc'})
        integer(sample['observed_ns'], 0, 60_000_000_000)
        integer(sample['read_batch'], 0, len(process['read_batches']) - 1)
        require(type(sample['framecrc']) is str and len(sample['framecrc']) <= 4096, 'invalid framecrc')
        columns = [value.strip() for value in sample['framecrc'].split(',')]
        require(len(columns) >= 6 and tuple(map(int, columns[:4])) == (0, index, index, 1)
                and int(columns[4]) > 0 and re.fullmatch('0x[0-9a-fA-F]{8}', columns[5]), 'framecrc ordering mismatch')

def tile_points(manifest, label):
    keys(manifest, {'schema_version', 'workload_version', 'producer', 'pixel_format', 'config',
                    'frames', 'tile_baseline', 'baseline_order', 'quality_order', 'tile_quality'})
    require(type(manifest['schema_version']) is int and type(manifest['workload_version']) is int
            and manifest['schema_version'] == 1 and manifest['workload_version'] == 1
            and manifest['producer'] == 'jwm-production-replay' and manifest['pixel_format'] == 'rgb24',
            'unknown fixture schema/workload')
    cfg = manifest['config']; config(cfg)
    validate_baseline(manifest['tile_baseline'], cfg)
    ids = identities(manifest['frames'], cfg, files=True)
    quality = manifest['tile_quality']
    keys(quality, {'schema_version', 'scope', 'frames'})
    require(type(quality['schema_version']) is int and quality['schema_version'] == 1 and quality['scope'] == 'offline_production_tile_rgb_distortion',
            'unknown tile quality evidence')
    require(type(quality['frames']) is list and len(quality['frames']) == len(ids), 'incomplete tile quality')
    baseline = manifest['tile_baseline']
    require(baseline['config'] == cfg and baseline['scope'] == 'offline_synthetic_tile_codec_only'
            and baseline['schema_version'] == 1 and baseline['workload_version'] == 1, 'tile baseline mismatch')
    require([item['scene'] for item in baseline['scenes']] == list(SCENES), 'baseline scene order')
    records = []
    previous = None
    next_sequence = 1
    for record, identity in zip(quality['frames'], ids):
        keys(record, IDENTITY | METRICS | {'emitted', 'encoded_bytes', 'displayed_sequence'})
        validate_identity(record, identity)
        if previous is None or previous['scene'] != record['scene']:
            next_sequence = 1
        measured = metric(record, identity)
        require(type(record['emitted']) is bool, 'invalid emission flag')
        size = integer(record['encoded_bytes'], 0, RGB_CAP)
        sequence = integer(record['displayed_sequence'], 0, 2**64 - 1)
        if not record['emitted']:
            require(size == 0 and previous is not None and previous['scene'] == record['scene']
                    and previous['geometry_epoch'] == record['geometry_epoch']
                    and previous['displayed_sequence'] == sequence, 'invalid suppressed reconstruction')
        else:
            require(size > 0 and sequence == next_sequence, 'invalid emitted sequence/bytes')
            next_sequence += 1
        if not record['emitted'] and previous['sha256'] == record['sha256']:
            require(all(previous[k] == record[k] for k in METRICS), 'unchanged suppressed reconstruction metrics differ')
        records.append({'frame_identity': identity, **measured, 'emitted': record['emitted'],
                        'encoded_bytes': size, 'displayed_sequence': sequence})
        previous = record
    points = []
    for scene, baseline_scene in zip(SCENES, baseline['scenes']):
        subset = [record for record in records if record['frame_identity']['scene'] == scene]
        size = sum(record['encoded_bytes'] for record in subset)
        require(sum(r['emitted'] for r in subset) == baseline_scene['emitted_frames']
                and sum(not r['emitted'] for r in subset) == baseline_scene['suppressed_frames'], 'baseline emission counts mismatch')
        require(size == baseline_scene['encoded_bytes'] and len(subset) == baseline_scene['frames'],
                'tile quality and baseline totals differ')
        points.append(point(label, 'jpeg_tiles', cfg['quality'], None, scene, subset, size))
    return ids, points


def groups(ids):
    result = []
    for item in ids:
        scope = (item['scene'], item['width'], item['height'], item['geometry_epoch'])
        if not result or result[-1][0] != scope:
            result.append((scope, []))
        result[-1][1].append(item)
    require(len(result) <= 25, 'too many independent H264 segments')
    return result


def h264_points(report, ids, label, manifests):
    keys(report, {'schema_version', 'scope', 'config', 'fixture_manifest_sha256', 'ffmpeg_version',
                 'machine', 'python_version', 'logical_cpu_count', 'affinity_cpu_count', 'codec', 'crf',
                 'threads', 'warmup_runs', 'trials', 'tile_baseline', 'comparability', 'runs',
                 'aggregate_wall_s', 'quality_evidence_version', 'input_frames'})
    require(type(report['schema_version']) is int and type(report['quality_evidence_version']) is int
            and report['schema_version'] == 1 and report['quality_evidence_version'] == 1
            and report['scope'] == 'offline_software_h264_not_gpu_or_desktop_latency'
            and report['codec'] == 'libx264' and type(report['threads']) is int and report['threads'] == 1, 'unknown H264 evidence/profile')
    config(report['config'])
    require(identities(report['input_frames'], report['config']) == ids, 'H264 input identity mismatch')
    digest = report['fixture_manifest_sha256']
    require(digest in manifests and manifests[digest]['config'] == report['config'], 'missing exact H264 fixture manifest')
    require(report['tile_baseline'] == manifests[digest]['tile_baseline'], 'embedded baseline mismatch')
    crf = integer(report['crf'], 0, 51)
    trials = integer(report['trials'], 1, 3)
    warmup = integer(report['warmup_runs'], 0, 1)
    segments = groups(ids)
    expected = [(scope, entries, trial) for scope, entries in segments for trial in range(-warmup, trials)]
    require(type(report['runs']) is list and len(report['runs']) == len(expected), 'incomplete H264 runs')
    collected = {(trial, scene): ([], 0) for trial in range(trials) for scene in SCENES}
    for run, (scope, entries, trial) in zip(report['runs'], expected):
        keys(run, {'scene', 'geometry_epoch', 'width', 'height', 'frames', 'trial', 'warmup',
                   'encode_command', 'decode_command', 'encoded_bytes', 'encode_process',
                   'decode_process', 'per_frame_quality'})
        for name in ('width', 'height', 'geometry_epoch'):
            integer(run[name], 0, 2**64 - 1)
        require((run['scene'], run['width'], run['height'], run['geometry_epoch']) == scope
                and type(run['trial']) is int and run['trial'] == trial
                and type(run['warmup']) is bool and run['warmup'] == (trial < 0)
                and type(run['frames']) is int and run['frames'] == len(entries), 'H264 run order/scope mismatch')
        validate_commands(run, crf)
        size = integer(run['encoded_bytes'], 1, RGB_CAP)
        validate_packets(run['encode_process'], len(entries))
        require(sum(int(packet['framecrc'].split(',')[4]) for packet in run['encode_process']['packet_observations']) == size, 'framecrc packet bytes disagree with bitstream size')
        validate_packets(run['decode_process'], 0)
        qualities = run['per_frame_quality']
        require(type(qualities) is list and len(qualities) == len(entries), 'incomplete H264 quality')
        checked = []
        for record, identity in zip(qualities, entries):
            keys(record, METRICS | {'frame_identity', 'rgb_mse', 'rgb_psnr_db', 'lossless', 'changed_channel_fraction'})
            keys(record['frame_identity'], IDENTITY)
            validate_identity(record['frame_identity'], identity)
            measured = metric(record, identity)
            mse = measured['sse'] / measured['channel_samples']
            require(type(record['lossless']) is bool and record['lossless'] == (mse == 0), 'lossless mismatch')
            require(type(record['rgb_mse']) in (int, float) and math.isclose(record['rgb_mse'], mse, rel_tol=1e-12, abs_tol=1e-12), 'MSE mismatch')
            require(type(record['changed_channel_fraction']) in (int, float) and math.isclose(record['changed_channel_fraction'], measured['changed_count'] / measured['channel_samples'], rel_tol=1e-12, abs_tol=1e-12), 'changed fraction mismatch')
            psnr = record['rgb_psnr_db']
            require(psnr is None if mse == 0 else type(psnr) in (int, float) and math.isclose(psnr, 10 * math.log10(65025 / mse), rel_tol=1e-12), 'PSNR mismatch')
            checked.append({'frame_identity': identity, **measured})
        if trial >= 0:
            old, count = collected[(trial, scope[0])]
            old.extend(checked)
            collected[(trial, scope[0])] = (old, count + size)
    return [point(f'{label}/trial-{trial}', 'libx264', crf, trial, scene, *collected[(trial, scene)])
            for trial in range(trials) for scene in SCENES]


def frontier(points):
    return [p for p in points if not any(
        q['encoded_bytes'] <= p['encoded_bytes'] and q['sse'] * p['channel_samples'] <= p['sse'] * q['channel_samples']
        and (q['encoded_bytes'] < p['encoded_bytes'] or q['sse'] * p['channel_samples'] < p['sse'] * q['channel_samples'])
        for q in points)]


def select(points, max_mse=None, max_bytes=None):
    result = {}
    if max_mse is not None:
        threshold = Fraction(str(max_mse))
        require(0 <= threshold <= 65025, 'invalid MSE threshold')
        eligible = [p for p in points if Fraction(p['sse'], p['channel_samples']) <= threshold]
        result['max_mse'] = {'threshold': str(max_mse), 'candidates': [p['candidate'] for p in eligible
            if p['encoded_bytes'] == min((q['encoded_bytes'] for q in eligible), default=-1)],
            'status': 'measured_match' if eligible else 'no_measured_match'}
    if max_bytes is not None:
        integer(max_bytes, 0, 2**63 - 1)
        eligible = [p for p in points if p['encoded_bytes'] <= max_bytes]
        best = min((Fraction(p['sse'], p['channel_samples']) for p in eligible), default=None)
        result['max_bytes'] = {'threshold': max_bytes, 'candidates': [p['candidate'] for p in eligible
            if Fraction(p['sse'], p['channel_samples']) == best],
            'status': 'measured_match' if eligible else 'no_measured_match'}
    return result


def analyze(manifest_inputs, report_inputs, max_mse=None, max_bytes=None):
    require(1 <= len(manifest_inputs) <= 3 and 1 <= len(report_inputs) <= 3, 'requires 1..3 manifests and 1..3 H264 reports')
    points, source, known, qualities, crfs = [], None, {}, set(), set()
    for label, digest, manifest in manifest_inputs:
        ids, candidate_points = tile_points(manifest, label)
        require(source is None or source == ids, 'different RGB timeline across candidates')
        source = ids
        quality = manifest['config']['quality']
        require(quality not in qualities and digest not in known, 'duplicate tile candidate')
        qualities.add(quality); known[digest] = manifest
        points.extend(candidate_points)
    for label, report in report_inputs:
        require(report['crf'] not in crfs, 'duplicate H264 candidate')
        crfs.add(report['crf'])
        points.extend(h264_points(report, source, label, known))
    scenes = []
    for scene in SCENES:
        subset = [p for p in points if p['scene'] == scene]
        scenes.append({'scene': scene, 'points': subset,
                       'pareto_candidates': [p['candidate'] for p in frontier(subset)],
                       'selection': select(subset, max_mse, max_bytes)})
    return {'schema_version': 1, 'scope': 'offline_rgb_rate_distortion_not_latency',
            'identity_frames': source, 'scenes': scenes,
            'limits': {'manifests': 3, 'h264_reports': 3, 'trials_per_report': 3},
            'warning': 'Measured points only; pooled RGB SSE is not perceptual/text quality. Bytes exclude transport; no timing ranking. Evidence is locally supplied, not authenticated.'}


def synthetic_inputs(frames=1):
    """Pure structurally complete fixtures, not simulated codec measurements."""
    cfg = dict(width=1, height=1, frames=frames, warmup=0, seed=1, quality=75)
    entries, evidence, scenes = [], [], []
    for scene in SCENES:
        for index in range(frames):
            identity = dict(scene=scene, index=index, width=1, height=1,
                            geometry_epoch=index // 3 if scene == 'resize' else 0, sha256='a' * 64)
            entries.append(dict(identity, file=f'{scene}-{index:04}.rgb'))
            evidence.append(dict(identity, sse=0, channel_samples=3, changed_count=0,
                                 max_channel_error=0, emitted=True, encoded_bytes=1, displayed_sequence=index + 1))
        stats = dict(samples=0, raw_us=[], mean_us=None, p50_us=None, p95_us=None, p99_us=None)
        scenes.append(dict(scene=scene, frames=frames, emitted_frames=frames, suppressed_frames=0,
                           encoded_bytes=frames, dirty_tiles=frames, keyframes=1,
                           **{stage: dict(stats) for stage in ('plan', 'encode', 'atlas_pack', 'jpeg', 'commit', 'decode')}))
    baseline = dict(schema_version=1, workload_version=1, scope='offline_synthetic_tile_codec_only',
                    build_commit='synthetic-self-test', debug_assertions=True, config=cfg, scenes=scenes)
    manifest = dict(schema_version=1, workload_version=1, producer='jwm-production-replay', pixel_format='rgb24',
                    config=cfg, frames=entries, tile_baseline=baseline, baseline_order='self-test only', quality_order='self-test only',
                    tile_quality=dict(schema_version=1, scope='offline_production_tile_rgb_distortion', frames=evidence))
    return manifest


def synthetic_report(manifest, digest):
    ids = identities(manifest['frames'], manifest['config'], files=True)
    def process(count):
        return dict(wall_ns=1, child_user_s=0, child_system_s=0, child_rss_high_water_kib=1,
                    rss_scope='self-test', packet_observations=[dict(observed_ns=i, read_batch=0,
                    framecrc=f'0, {i}, {i}, 1, 1, 0x00000000') for i in range(count)],
                    read_batches=[dict(at_ns=0, bytes=1)] if count else [],
                    startup_to_first_packet_observed_ns=0 if count else None,
                    post_first_packet_observation_intervals_ns=[1] * max(0, count - 1), observation_warning='self-test')
    runs = []
    for (scene, width, height, epoch), entries in groups(ids):
        quality = [dict(frame_identity=entry, sse=0, channel_samples=3, changed_count=0,
                        max_channel_error=0, rgb_mse=0, rgb_psnr_db=None, lossless=True,
                        changed_channel_fraction=0) for entry in entries]
        encode, decode = expected_commands('/ffmpeg', '/source.rgb', '/stream.h264', '/decoded.rgb', width, height, 23)
        runs.append(dict(scene=scene, width=width, height=height, geometry_epoch=epoch, frames=len(entries),
                         trial=0, warmup=False, encode_command=encode, decode_command=decode,
                         encoded_bytes=len(entries), encode_process=process(len(entries)), decode_process=process(0),
                         per_frame_quality=quality))
    return dict(schema_version=1, scope='offline_software_h264_not_gpu_or_desktop_latency',
                config=manifest['config'], fixture_manifest_sha256=digest, ffmpeg_version='self-test',
                machine='self-test', python_version='self-test', logical_cpu_count=1, affinity_cpu_count=1,
                codec='libx264', crf=23, threads=1, warmup_runs=0, trials=1, tile_baseline=manifest['tile_baseline'],
                comparability='self-test', runs=runs, aggregate_wall_s=0, quality_evidence_version=1, input_frames=ids)

def self_test():
    identity = {'width': 1, 'height': 1}
    metric({'sse': 65025, 'channel_samples': 3, 'changed_count': 1, 'max_channel_error': 255}, identity)
    a = {'candidate': 'a', 'sse': 1, 'channel_samples': 3, 'encoded_bytes': 10}
    b = {'candidate': 'b', 'sse': 2, 'channel_samples': 3, 'encoded_bytes': 20}
    c = {'candidate': 'c', 'sse': 0, 'channel_samples': 3, 'encoded_bytes': 30}
    require(frontier([a, b, c]) == [a, c], 'Pareto domination')
    require(select([a, b], max_mse='0')['max_mse']['status'] == 'no_measured_match', 'no interpolation')
    require(select([a, b, c], max_bytes=20)['max_bytes']['candidates'] == ['a'], 'fixed bytes selection')
    require(select([a, b, c], max_mse='0')['max_mse']['candidates'] == ['c'], 'lossless selection')
    for call in [lambda: parse('{"x":1,"x":2}'), lambda: parse('{"x":NaN}'),
                 lambda: parse('{"x":1e999}'), lambda: integer(True, 0, 1),
                 lambda: metric({'sse': 0, 'channel_samples': 3, 'changed_count': 1, 'max_channel_error': 1}, identity),
                 lambda: keys({'unknown': 1}, {'sse'}), lambda: analyze([], [])]:
        try:
            call()
        except (ValueError, KeyError, TypeError):
            pass
        else:
            raise AssertionError('expected rejection')
    import copy
    manifest = synthetic_inputs()
    digest = hashlib.sha256(json.dumps(manifest).encode()).hexdigest()
    report = synthetic_report(manifest, digest)
    result = analyze([('tile-0', digest, manifest)], [('h264-0', report)])
    require(len(result['scenes']) == 5 and all(len(s['points']) == 2 for s in result['scenes']), 'complete evidence roundtrip')
    # All 1500 offered frames fit the explicitly supported manifest bound.
    maximum = synthetic_inputs(300)
    tile_points(maximum, 'max')
    require(len(json.dumps(maximum, indent=2).encode()) < MANIFEST_CAP, 'maximum frame manifest size')
    bad_reports = []
    for mutation in [lambda r: r['runs'].pop(), lambda r: r['runs'].append(r['runs'][0]),
                     lambda r: r['input_frames'].reverse(),
                     lambda r: r['runs'][0]['per_frame_quality'][0]['frame_identity'].update(sha256='b' * 64),
                     lambda r: r['runs'][0]['per_frame_quality'][0].update(sse=1),
                     lambda r: r['runs'][0]['encode_process']['packet_observations'][0].update(framecrc='0, 1, 0, 1, 1, 0x00000000'),
                     lambda r: r['runs'][0]['per_frame_quality'][0]['frame_identity'].update(width=True),
                     lambda r: r['runs'][0].update(width=True),
                     lambda r: r.update(threads=True),
                     lambda r: r['runs'][0]['encode_command'].__setitem__(27, 'wrong'),
                     lambda r: r['runs'][0].update(encode_command=['/ffmpeg', '-i']),
                     lambda r: r['runs'][0].update(decode_command=['/ffmpeg', '-i']),
                     lambda r: r.pop('quality_evidence_version')]:
        bad = copy.deepcopy(report); mutation(bad); bad_reports.append(bad)
    bad_manifests = []
    for mutation in [lambda m: m['tile_quality']['frames'].pop(),
                     lambda m: m['frames'].reverse(),
                     lambda m: m['tile_quality']['frames'][0].update(displayed_sequence=2),
                     lambda m: m['tile_quality']['frames'][0].update(width=True),
                     lambda m: m['tile_baseline']['scenes'][0].update(emitted_frames=0, suppressed_frames=1),
                     lambda m: m['tile_quality']['frames'][0].update(channel_samples=4),
                     lambda m: m['tile_quality']['frames'][0].update(emitted=False, encoded_bytes=0)]:
        bad = copy.deepcopy(manifest); mutation(bad); bad_manifests.append(bad)
    for bad in bad_reports:
        try: analyze([('tile-0', digest, manifest)], [('h264-0', bad)])
        except (ValueError, KeyError, TypeError): pass
        else: raise AssertionError('invalid H264 evidence accepted')
    for bad in bad_manifests:
        try: tile_points(bad, 'bad')
        except (ValueError, KeyError, TypeError): pass
        else: raise AssertionError('invalid tile evidence accepted')
    for inputs in [([('tile-0', digest, manifest)] * 2, [('h264-0', report)]),
                   ([('tile-0', digest, manifest)], [('h264-0', report)] * 2)]:
        try: analyze(*inputs)
        except ValueError: pass
        else: raise AssertionError('duplicate candidate accepted')
    print('self-test passed (pure; full evidence, negative joins, 1500-frame manifest; no files/processes/sockets)')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--fixture-manifest', type=Path, action='append', default=[])
    parser.add_argument('--h264-report', type=Path, action='append', default=[])
    parser.add_argument('--max-mse')
    parser.add_argument('--max-bytes', type=int)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.self_test:
        self_test(); return
    require(args.output is not None and 1 <= len(args.fixture_manifest) <= 3
            and 1 <= len(args.h264_report) <= 3, 'explicit output and 1..3 inputs of each kind required')
    manifests, reports, provenance, total = [], [], [], 0
    for kind, paths, cap in [('tile', args.fixture_manifest, MANIFEST_CAP), ('h264', args.h264_report, REPORT_CAP)]:
        for index, path in enumerate(paths):
            data = read(path, cap); total += len(data)
            require(total <= TOTAL_CAP, 'aggregate input bound exceeded')
            digest = hashlib.sha256(data).hexdigest()
            label = f'{kind}-{index}'
            provenance.append({'candidate': label, 'sha256': digest, 'bytes': len(data)})
            if kind == 'tile':
                manifests.append((label, digest, parse(data)))
            else:
                reports.append((label, parse(data)))
    result = analyze(manifests, reports, args.max_mse, args.max_bytes)
    result['input_provenance'] = provenance
    encoded = json.dumps(result, indent=2, allow_nan=False).encode()
    require(len(encoded) <= TOTAL_CAP, 'output evidence bound exceeded')
    with args.output.open('xb') as output:
        output.write(encoded)
    print(args.output)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, RecursionError, OverflowError, ZeroDivisionError) as error:
        print(f'rate-distortion analysis failed: {error}', file=sys.stderr)
        sys.exit(1)
