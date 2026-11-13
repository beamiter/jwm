# Atomic workspace selector, protocol version 1

Implementation: `src/ipc.rs`, `src/jwm/ipc_handler.rs` and
`scripts/task_workspace.py`. This is an explicitly approved experimental
workspace selector, not an application or terminal restoration mechanism.

`get_task_workspace_state` is a read-only query returning:

```json
{"version":1,"monitor_identity":"opaque token or null","workspaces":[]}
```

Workspaces retain existing `get_workspaces` semantics; `focused` means active
on the selected monitor. `connector` is the existing stable output key, not
necessarily the literal physical connector name. The token covers a random
process nonce, selected monitor SlotMap key including its generation, and
that stable output key. It is a concurrency precondition, not a credential.

`task_workspace_view` accepts exactly:

```json
{"version":1,"connector":"DP-1","monitor":0,"monitor_identity":"token from query","expected_tag_mask":1,"tag_mask":4}
```

The handler synchronously validates:

- known version, no unknown fields, typed nonempty connector;
- exactly one configured tag bit in both masks (zero never means previous tag);
- one unique live monitor matching the stable output key;
- currently selected monitor's number, generation token and active mask;
- no system UI lock, backend session lock or lock on that monitor;
- the existing command path also rejects pending shutdown.

It calls `view_on_monitor` with the validated key without yielding to the
compositor event loop. This eliminates the client preview/check-to-view focus
race; it does not promise that focus cannot change *after* completion. Clients
must still verify the result and must not blindly retry a timed-out mutation.

Scope remains tag selection only, no application/session launch, attachment,
window placement or arbitrary command execution. There is no fallback to the
unconditional legacy `view` command.

## Identity boundary

This is not physical-display attestation. If a backend reconnects a physical
display while preserving both JWM monitor key generation and stable output
identity, the token is unchanged. It is not an independent hotplug epoch.
The process nonce differs after normal compositor restart; the key generation
differs when JWM removes/reuses the monitor slot. Two distinct physical-output
focus racing and backend-specific hotplug behavior still need live acceptance.

## Validation and release limits

Run the focused checks from the repository root:

```sh
python3 scripts/test_task_workspace.py -v
cargo test --locked --lib --no-default-features task_workspace -- --test-threads=1
cargo check --locked --lib --no-default-features
```

The initial local validation passed 17 Python tests and 5 Rust tests with the
minimal feature set. Native default/X11 feature validation was blocked by missing
ALSA and X11 protocol development metadata in that build environment. Real
compositor restart, physical multi-monitor races and hardware hotplug acceptance
remain required before treating this as a supported automated restoration API.
