# Adopting the 0.3 lifecycle core

Version 0.3 is additive: the 0.2 model, presentation, provider, transport, and
renderer APIs remain available. The new lifecycle surface replaces retry and
cadence state that was duplicated across the JWM bar repositories.

## Managed transport

The runtime can now own opening and recovering the existing WM-owned ring:

```rust
use xbar_core::{BarRuntime, ModelConfig, TransportRecoveryConfig};

let recovery = TransportRecoveryConfig::with_default_retry(shared_path)?;
let mut runtime = BarRuntime::with_managed_transport(ModelConfig::default(), recovery)?;
```

The first `poll_transport()` attempts to open the path. An unsuccessful open
is returned as `RuntimeIssue::AdapterFailed { operation: "open", .. }` and the
next attempt is bounded by the configured retry interval. Read or command
failures drop the handle, reduce `WindowManagerUnavailable`, clear stale
geometry, and enter the same recovery state.

For an existing runtime, install or replace only the recovery policy with
`set_transport_recovery`. This does not replace an already-open handle. It is
therefore safe to combine an eager startup open (for initial notifier
registration) with managed recovery afterward.

Delete frontend copies of:

- `TRANSPORT_RETRY_INTERVAL` when the two-second core default is appropriate;
- `last_transport_retry` / `next_transport_retry`;
- `ensure_transport`, `try_open_transport`, and equivalent helpers;
- code that calls `set_transport(None)` after a transport issue—the runtime
  already removed the broken handle.

`RuntimeUpdate::transport_failed()` remains useful for logging or telemetry;
it classifies only actual open/read/write failures, not command gating or a
full bounded queue.

## Portable service cadence

Frameworks with a 50–250 ms timer can replace separate poll and provider
deadlines with one schedule:

```rust
use xbar_core::RuntimeSchedule;

let mut schedule = RuntimeSchedule::default();

// On each framework timer/idle turn:
let update = schedule.service(&mut runtime);
```

Every call polls transport; the first call also ticks providers, and later
provider ticks occur once per monotonic second by default. Missed intervals
coalesce into one tick rather than causing catch-up bursts. Use
`RuntimeSchedule::new(interval)` for a different cadence, or keep explicit
`tick()` and `poll_transport()` calls when a native loop already has separate
timerfd and notifier sources. `BarRuntime::service()` is the unscheduled
single-pass form that always performs both operations.

## Native notifier generations

An eventfd notifier observes one concrete shared-ring generation. Periodic
polling remains the correctness path after a reconnect, while
`transport_generation()` lets a native event loop detect handle replacement
and register a new notifier for lower latency. `transport_status()` reports:

- `Disabled`: neither a handle nor a recovery policy exists;
- `Recovering`: a managed path is waiting for its next open attempt;
- `Connected`: the handle is open but has not produced a fresh snapshot;
- `Ready`: the current WM projection is authoritative.

Commands remain rejected in `Connected`, preventing a reopened transport from
using stale monitor state.

## Issues

`RuntimeIssue` now implements `Display` and `Error`, and exposes `adapter()`.
`RuntimeUpdate` adds `is_empty`, `has_issues`, `has_adapter_issue`, and
`transport_failed`. Frontends can log `{issue}` directly instead of carrying
their own issue-to-string match that must be updated whenever the core grows a
new issue variant.
