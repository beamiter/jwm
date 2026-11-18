# Task workspace MVP (schema 1)

This first vertical slice associates a development task with a repository or
worktree directory, a JWM tag on a named output, and terminal/agent/browser/remote
references. The task can be listed, previewed against live JWM state, and explicitly
resumed by selecting its existing tag. It is an external stdlib-only CLI; it does
not add work to the compositor render loop.

## Try it

Create a local manifest (replace paths and connector with actual values):

```json
{
  "version": 1,
  "id": "rpl-stairs",
  "title": "RPL stairs debugging",
  "repository": "/home/me/repo/sonic",
  "workspace": {"connector": "DP-1", "tag_mask": 4},
  "resources": [
    {"kind": "terminal", "label": "Training logs", "terminal": "ember",
     "reference": "123-456-7", "snapshot": "/home/me/.local/state/ember/sessions.json"},
    {"kind": "agent", "label": "Code review", "reference": "review-session-17"},
    {"kind": "remote", "label": "Training host", "reference": "gpu-devbox"}
  ]
}
```

```sh
python3 scripts/task_workspace.py import /path/to/task.json
python3 scripts/task_workspace.py list
python3 scripts/task_workspace.py plan rpl-stairs
# Read the complete plan, then copy its approval_digest:
python3 scripts/task_workspace.py resume rpl-stairs --approve-plan DIGEST
```

Default registry: `$XDG_STATE_HOME/jwm/tasks`, or `~/.local/state/jwm/tasks`.
`--store /private/path` before the command selects a separate registry. Import
requires a private owner-controlled directory; publishes a complete fsynced file
without overwriting an existing task, and refuses unsupported schema/fields.
Task records contain context references, not credentials, command argv, or terminal
contents. Do not put secrets into references. No existing registry is migrated or
modified automatically. Version 2 and unknown fields fail closed.

The paired compositor patch exposes `jwm-tool msg get_task_workspace_state --raw`
with API version 1, workspaces and an opaque selected-monitor identity token.
Older servers fail closed; there is no fallback to unguarded `view`. `tag_mask`
is a bit mask, so 4 means the third tag, **not** tag number 4. Obtain output/tag
values from this query; do not guess based on screen order. The MVP deliberately
requires the user to focus the target monitor manually before resume, and never
moves a window or changes the layout. Missing or ambiguous outputs/tags block
resume. The preview digest includes the task, live active tags, target monitor,
and resource status. Changed previews must be approved again. After `task_workspace_view`, the
CLI queries the versioned state and monitor token to confirm the target tag; failed confirmation is not retried.

## Recovery boundaries and terminal integration

- `repository` may be a Git worktree but this slice only checks directory presence.
  It does not run Git, check out branches, reset changes, or start a process.
- Terminal references use stable `session_id` metadata found in the terminal
  session layer. The read-only adapter recognizes explicit version 1–4 `sessions`
  arrays. Each of Anvil/Forge/Ember/Frost can be named, but a different snapshot
  layout/version reports `snapshot_unavailable_or_unsupported`; it is not silently
  interpreted. Snapshot locations must be provided explicitly.
- `snapshot_reference_found` means exactly that. It does not establish a running
  process, PTY ownership, attachability, or an unexpired remote authorization.
- Agent, browser, remote and artifact references are returned for human navigation;
  no URLs are opened and no SSH, agent, shell, or terminal commands are executed.
- No arbitrary application memory, closed windows, terminal history, or layout is
  restored. The only executed action selects an existing tag after explicit approval.
- `task_workspace_view` checks version, selected monitor index, connector, opaque
  monitor identity and expected current single tag mask before changing state in
  the same synchronous compositor handler. The identity binds a process nonce
  and monitor key generation, so process restart or removal/reuse of that monitor
  slot invalidates previews. It is not an authenticated physical-display identity
  or hotplug epoch; invisible backend replacement preserving the same key and
  stable output identity is not detected.
  A mismatched precondition is refused instead of acting on another monitor.
- Preview binds the observed active state and identity. Resume rechecks the preview
  digest, submits the conditional action once, and verifies token/monitor/tag again.
  There is no retry or rollback after timeout, rejection or uncertain verification.
  Later focus changes can still make the post-check fail. This is not a transaction
  over applications, files, sessions, or other IPC requests.
- Keep this an explicitly approved experimental workflow pending real desktop
  acceptance tests. A passing fake-tool test is not proof of hardware behavior.

## Read-only Agent observations

`python3 scripts/task_workspace.py status TASK_ID` queries `get_agent_sessions`
once when the task contains an explicitly bound Agent. It never switches tags,
reveals windows, attaches sessions, launches Agents, opens SSH, or changes resume
approval. Without a binding, an Agent remains `reference_only`; labels, repository
paths and titles are never used to guess associations. Existing manifests continue
to work unchanged. `plan` and `resume` still use static references, not these live
observations, so polling status does not invalidate approval digests.

To opt in, add `agent_identity` to an Agent resource in a **new** manifest and
import it under a new task ID (import does not overwrite). Copy these exact values
from a reviewed `jwm-tool msg get_agent_sessions --raw` response: `wm_instance_id`
comes from the snapshot, all other fields from the intended session. The reference
must equal `session_id`. This operation does not automatically bind or update an
existing task. For example, the following is illustrative, not a usable identity:

```json
{"kind":"agent", "label":"Review", "reference":"review-1",
 "agent_identity":{
   "session_id":"review-1", "project_id":"project-1", "provider":"ember",
   "host_id":"host-1", "host_incarnation":"boot-1", "pane_id":"pane-1",
   "window_token":"window-1", "wm_instance_id":"wm-1"
 }}
```

All eight identity fields must match. A reused session ID, a restarted host or
compositor, changed pane/window, absent session, or duplicate session ID yields
`unknown` with a reason, never a guessed match. The CLI forwards `disconnected`
only when the registry reports it for the exact identity. The compositor already
maps expired leases and missing windows to `disconnected`; the CLI does not infer
liveness from timestamps. Unsupported/malformed replies and IPC interruption
return `unknown` with `evidence: unavailable`, without retries. Unbound resources
retain `evidence: static_reference`; matched and unmatched valid registry queries
use `evidence: registry_observation`.

`agent_counts` counts only explicitly bound Agents, by observed state. The
`agent_snapshot_available` field is null when no query was needed, false when the
query failed validation or transport, and true for a usable snapshot (including
an empty one). A successful registry query is not proof a referenced Agent exists.
These are point-in-time provider reports, not verified task outcomes or GPU/remote
health, and may change immediately after the query. A reported `succeeded` does
not prove the whole task succeeded. No real remote continuity or restoration
performance is claimed by the simulated tests.

## Validation

```sh
python3 scripts/test_task_workspace.py -v
python3 -m py_compile scripts/task_workspace.py scripts/test_task_workspace.py
```

Tests cover private durable create/no overwrite, invalid versions/fields/paths,
JSON bounds and duplicate fields, missing/ambiguous outputs, changed-plan refusal,
read-only preview, terminal metadata interpretation, explicit argv IPC, successful
selection and unconfirmed selection without retry, API incompatibility, multiple
active tags, monitor generation/restart changes, and no unsafe fallback. The paired
Rust compositor patch adds the guarded command and its state query.
A fake-tool subprocess integration test exercises the actual CLI as well.
Live JWM GUI/terminal resume and real hardware task switching remain unverified.
No <=10 second or <=2 action performance result is claimed.

## Next vertical slices

1. Real multi-monitor desktop acceptance: focus races, lock, hotplug, compositor
   restart, multi-tag selection and failed/uncertain IPC. No auto-retry or undo yet.
2. Terminal-owned discovery/attach API: expose session identity, lifecycle,
   attachment support and failure reasons. Keep per-app persistence owners intact.
3. Task picker/overview UI using this versioned task model and per-resource
   status; display stale/unknown status rather than launching duplicates.
4. Explicit approved restore actions and receipts, then browser/remote references.
