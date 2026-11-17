#!/usr/bin/env python3
"""Task context registry and conservative JWM workspace resume (schema 1)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile

LIMIT = 1024 * 1024
TERMINALS = {'anvil', 'forge', 'ember', 'frost'}


def require(ok, message):
    if not ok:
        raise ValueError(message)


def text(value, label):
    require(isinstance(value, str) and 0 < len(value) <= 4096 and
            not any(ord(c) < 32 for c in value), label + ': invalid text')
    return value


def load(path, private=False):
    fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode), 'JSON source must be a regular non-symlink file')
        require(not private or (info.st_uid == os.getuid() and info.st_mode & 0o077 == 0), 'task record must be private and owned by you')
        raw = stream.read(LIMIT + 1)
    require(len(raw) <= LIMIT, 'JSON exceeds 1 MiB')
    depth, quoted, escaped = 0, False, False
    for byte in raw:
        if quoted:
            if escaped:
                escaped = False
            elif byte == 92:
                escaped = True
            elif byte == 34:
                quoted = False
        elif byte == 34:
            quoted = True
        elif byte in (91, 123):
            depth += 1
            require(depth <= 64, 'JSON nesting too deep')
        elif byte in (93, 125):
            depth -= 1
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, 'duplicate JSON key: ' + key)
            result[key] = value
        return result
    try:
        return json.loads(raw, object_pairs_hook=pairs)
    except RecursionError as error:
        raise ValueError('JSON nesting too deep') from error


def validate(task):
    require(isinstance(task, dict), 'task must be an object')
    require(set(task) == {'version', 'id', 'title', 'repository', 'workspace', 'resources'},
            'unknown or missing task fields')
    require(type(task['version']) is int and task['version'] == 1, 'unsupported task version')
    require(isinstance(task['id'], str) and re.fullmatch(r'[a-z0-9][a-z0-9_-]{0,63}', task['id']), 'invalid task id')
    text(task['title'], 'title')
    require(Path(text(task['repository'], 'repository')).is_absolute(), 'repository must be absolute')
    workspace = task['workspace']
    require(isinstance(workspace, dict) and set(workspace) == {'connector', 'tag_mask'}, 'invalid workspace fields')
    text(workspace['connector'], 'connector')
    mask = workspace['tag_mask']
    require(type(mask) is int and 0 < mask < 2**32 and mask & (mask - 1) == 0, 'tag_mask must be a single tag bit')
    require(isinstance(task['resources'], list) and len(task['resources']) <= 64, 'resources must contain at most 64 entries')
    ids = set()
    for resource in task['resources']:
        require(isinstance(resource, dict), 'resource must be object')
        kind = resource.get('kind')
        allowed = {'kind', 'label', 'reference'} | ({'terminal', 'snapshot'} if kind == 'terminal' else set())
        require(isinstance(kind, str) and kind in {'terminal', 'agent', 'browser', 'remote', 'artifact'} and set(resource) == allowed, 'unsupported resource fields')
        text(resource['label'], 'resource label')
        text(resource['reference'], 'resource reference')
        key = (kind, resource['reference'])
        require(key not in ids, 'duplicate resource')
        ids.add(key)
        if kind == 'terminal':
            require(isinstance(resource['terminal'], str) and resource['terminal'] in TERMINALS, 'unsupported terminal')
            require(re.fullmatch(r'[A-Za-z0-9_-]{1,128}', resource['reference']), 'invalid session id')
            require(Path(text(resource['snapshot'], 'snapshot')).is_absolute(), 'snapshot must be absolute')
    return task


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def private_store(root):
    root = Path(root)
    info = root.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and info.st_mode & 0o077 == 0,
            'task directory must be owned by you, private (0700), and not a symlink')
    return root


def save_new(task, root):
    validate(task)
    root = Path(root)
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    private_store(root)
    path = root / (task['id'] + '.json')
    encoded = json.dumps(task, indent=2) + '\n'
    require(len(encoded.encode()) <= LIMIT, 'serialized task exceeds 1 MiB')
    fd, temporary = tempfile.mkstemp(prefix='.task-', dir=root)
    try:
        with os.fdopen(fd, 'w') as stream:
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        # Same-directory hard link publishes complete contents without replacing
        # an existing task. Crash residue is hidden and ignored by list.
        os.link(temporary, path, follow_symlinks=False)
        directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        os.unlink(temporary)
    return path


def resource_status(resource):
    result = dict(resource)
    result['status'] = 'reference_only'
    if resource['kind'] == 'terminal':
        try:
            snapshot = load(resource['snapshot'])
            # Ember/Forge-style session metadata is not proof of a live process.
            require(type(snapshot.get('version')) is int and snapshot['version'] in (1, 2, 3, 4), 'unknown terminal snapshot version')
            sessions = snapshot.get('sessions')
            require(isinstance(sessions, list), 'invalid sessions array')
            matches = [s for s in sessions if isinstance(s, dict) and s.get('session_id') == resource['reference']]
            result['status'] = 'snapshot_reference_found' if len(matches) == 1 else ('ambiguous' if matches else 'missing')
        except (OSError, ValueError, TypeError, AttributeError):
            result['status'] = 'snapshot_unavailable_or_unsupported'
    return result


def ipc(command, args=None):
    argv = ['jwm-tool', 'msg', command, '--raw']
    if args is not None:
        argv += ['--args', json.dumps(args)]
    completed = subprocess.run(argv, check=True, capture_output=True, timeout=5)
    require(len(completed.stdout) <= LIMIT, 'IPC reply too large')
    response = json.loads(completed.stdout)
    require(isinstance(response, dict), 'invalid JWM IPC response')
    require(response.get('success') is True, 'JWM IPC failed: ' + str(response.get('error', 'unknown error'))[:1024])
    return response.get('data')


def plan(task, state):
    validate(task)
    require(isinstance(state, dict) and type(state.get('version')) is int and state['version'] == 1,
            'atomic task workspace API version 1 required; no legacy view fallback')
    workspaces = state.get('workspaces')
    identity = state.get('monitor_identity')
    require(isinstance(workspaces, list), 'invalid workspace reply')
    require(all(isinstance(w, dict) for w in workspaces), 'invalid workspace entry')
    target = task['workspace']
    matches = [w for w in workspaces if w.get('connector') == target['connector'] and w.get('tag_mask') == target['tag_mask']]
    active = [w for w in workspaces if w.get('focused') is True]
    blockers = []
    if not Path(task['repository']).is_dir():
        blockers.append('repository_missing')
    if len(matches) != 1:
        blockers.append('workspace_missing_or_ambiguous')
    if len(active) != 1 or any(w.get('connector') != target['connector'] for w in active):
        blockers.append('focus_target_monitor_and_select_single_tag')
    if not isinstance(identity, str) or not identity or len(identity) > 4096:
        blockers.append('monitor_identity_unavailable')
    if len(matches) == 1 and len(active) == 1 and matches[0].get('monitor') != active[0].get('monitor'):
        blockers.append('monitor_identity_mismatch')
    for item in matches + active:
        mask = item.get('tag_mask')
        if (type(item.get('monitor')) is not int or not -(2**31) <= item['monitor'] < 2**31 or
                type(mask) is not int or not 0 < mask < 2**32 or mask & (mask - 1)):
            blockers.append('invalid_workspace_identity')
            break
    result = {'version': 1, 'task_id': task['id'], 'task_digest': digest(task),
              'repository': task['repository'], 'workspace': target,
              'active_workspaces': [{'connector': w.get('connector'), 'tag_mask': w.get('tag_mask'), 'monitor': w.get('monitor')} for w in active],
              'target_monitor': matches[0].get('monitor') if len(matches) == 1 else None,
              'resources': [resource_status(r) for r in task['resources']], 'blockers': blockers,
              'action': {'command': 'task_workspace_view', 'args': {
                  'version': 1, 'connector': target['connector'],
                  'monitor': matches[0].get('monitor') if len(matches) == 1 else None,
                  'monitor_identity': identity,
                  'expected_tag_mask': active[0].get('tag_mask') if len(active) == 1 else None,
                  'tag_mask': target['tag_mask']}},
              'limits': ['No app launch, session attach, layout restore, remote connection or command execution.',
                         'Snapshot reference is not a live-session guarantee.',
                         'Atomic preconditions protect selection; later desktop changes can still prevent confirmation.']}
    result['approval_digest'] = digest(result)
    return result


def resume(task, approval, call=ipc):
    proposed = plan(task, call('get_task_workspace_state'))
    require(not proposed['blockers'], 'resume blocked: ' + ', '.join(proposed['blockers']))
    require(approval == proposed['approval_digest'], 'plan changed or approval digest mismatch; preview again')
    try:
        call('task_workspace_view', proposed['action']['args'])
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        raise ValueError('view attempted; outcome may be uncertain; no retry performed: ' + str(error)) from error
    try:
        state = call('get_task_workspace_state')
        require(isinstance(state, dict) and type(state.get('version')) is int and state['version'] == 1,
                'invalid workspace verification version')
        require(state.get('monitor_identity') == proposed['action']['args']['monitor_identity'],
                'monitor identity changed after command')
        after = state.get('workspaces')
        require(isinstance(after, list) and all(isinstance(w, dict) for w in after), 'invalid workspace verification')
        focused = [w for w in after if w.get('focused') is True]
        require(len(focused) == 1 and type(focused[0].get('monitor')) is int and
                type(focused[0].get('tag_mask')) is int and focused[0].get('connector') == task['workspace']['connector'] and
                focused[0].get('tag_mask') == task['workspace']['tag_mask'] and
                focused[0].get('monitor') == proposed['action']['args']['monitor'], 'target focus not confirmed')
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        raise ValueError('view sent, outcome not confirmed; no retry performed: ' + str(error)) from error
    return {'task_id': task['id'], 'status': 'workspace_selected', 'resources': proposed['resources']}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--store', default=str(Path(os.environ.get('XDG_STATE_HOME', str(Path.home() / '.local/state'))) / 'jwm/tasks'))
    sub = parser.add_subparsers(dest='command', required=True)
    create = sub.add_parser('import', help='validate and save a new task; never overwrite')
    create.add_argument('manifest')
    sub.add_parser('list')
    for name in ('plan', 'resume'):
        cmd = sub.add_parser(name)
        cmd.add_argument('id')
        if name == 'resume':
            cmd.add_argument('--approve-plan', required=True)
    args = parser.parse_args(argv)
    if args.command == 'import':
        result = {'saved': str(save_new(load(args.manifest), args.store))}
    elif args.command == 'list':
        result = []
        for path in sorted(private_store(args.store).glob('*.json')):
            try:
                task = validate(load(path, private=True))
                require(task['id'] == path.stem, 'task file/id mismatch')
                result.append({'id': task['id'], 'title': task['title'], 'repository': task['repository']})
            except (ValueError, OSError) as error:
                result.append({'file': path.name, 'error': str(error)})
    else:
        require(re.fullmatch(r'[a-z0-9][a-z0-9_-]{0,63}', args.id), 'invalid task id')
        task = validate(load(private_store(args.store) / (args.id + '.json'), private=True))
        require(task['id'] == args.id, 'task file/id mismatch')
        result = plan(task, ipc('get_task_workspace_state')) if args.command == 'plan' else resume(task, args.approve_plan)
    print(json.dumps(result, indent=2, ensure_ascii=True))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print('task-workspace: ' + str(error), file=sys.stderr)
        sys.exit(1)
