//! Fail-closed X11 client handoff before a seamless exec restart.
//!
//! The replacement JWM discovers root children that are either viewable or
//! advertise ICCCM `WM_STATE=IconicState`.  This module proves that predicate
//! while the old registry and X11 connection are still alive.  A true-Iconic
//! client whose public/private readback is inconclusive is selectively mapped
//! at its already-verified off-screen parking coordinate; unrelated Iconic
//! clients remain unmapped.

use crate::Jwm;
use crate::backend::api::{Backend, NetWmState};
use crate::backend::common_define::WindowId;
use crate::core::models::ClientKey;
use crate::jwm::statusbar::StatusBarBuilder;
use crate::jwm::types::{ICONIC_STATE, NORMAL_STATE};
use crate::jwm::window_state::x11_geometry_fully_left_of_desktop;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// Proof that every persistent X11 client passed restart admission.  The
/// private fields make it impossible to manufacture outside this module; the
/// application will carry this value across the destructive cleanup boundary.
#[derive(Debug)]
pub(crate) struct PreparedRestartClients {
    _mapped_fallbacks: Vec<ClientKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RestartClientSpec {
    hidden: bool,
    dock_eligible: bool,
    swallowed_parent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RestartClientProbe {
    in_root_tree: bool,
    override_redirect: bool,
    viewable: bool,
    safely_parked: bool,
    wm_state_normal: bool,
    wm_state_iconic: bool,
    ewmh_hidden: bool,
    ewmh_not_hidden: bool,
    v1_exact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestartDisposition {
    ReadyViewable,
    ReadyIconic,
    NeedsMappedFallback,
    DeferredSwallowed,
    Reject(&'static str),
}

/// Classify a complete, resource-free probe.  All I/O stays in the caller so
/// the safety table can be exhaustively unit tested.
fn classify_restart_client(
    spec: RestartClientSpec,
    probe: RestartClientProbe,
) -> RestartDisposition {
    if !probe.in_root_tree {
        return RestartDisposition::Reject("not a root child");
    }
    if probe.override_redirect {
        return RestartDisposition::Reject("became override-redirect");
    }

    if !spec.hidden {
        if !probe.wm_state_normal || !probe.ewmh_not_hidden {
            return RestartDisposition::Reject(
                "visible client is not exactly NormalState and non-Hidden",
            );
        }
        if spec.swallowed_parent {
            // Its checked batch MapWindow is deliberately the final handoff
            // stage, after every other operation that can cancel restart.
            return RestartDisposition::DeferredSwallowed;
        }
        if !probe.viewable {
            return RestartDisposition::Reject("non-minimized client is unmapped");
        }
        return RestartDisposition::ReadyViewable;
    }

    if spec.swallowed_parent {
        return RestartDisposition::Reject("swallowed parent is also minimized");
    }

    if !probe.safely_parked {
        return RestartDisposition::Reject("minimized client is not safely parked");
    }
    // A viewable off-screen window can make the replacement discover it, but
    // it cannot reconstruct the semantic restore target that only V1 carries.
    // Never confuse physical safety with state fidelity.
    if !probe.v1_exact {
        return RestartDisposition::Reject("minimized V1 readback is not exact");
    }
    if probe.viewable {
        if !probe.wm_state_iconic && !probe.ewmh_hidden {
            return RestartDisposition::Reject("minimized public state is not recoverable");
        }
        return RestartDisposition::ReadyViewable;
    }
    if probe.wm_state_iconic {
        RestartDisposition::ReadyIconic
    } else if !spec.dock_eligible {
        // Only Dock-eligible clients have a reversible true-Iconic coordinator
        // path for the selective map transaction. A directly discoverable
        // ICCCM Iconic client above needs no such fallback.
        RestartDisposition::Reject("Dock-ineligible minimized client needs a mapped fallback")
    } else if probe.ewmh_hidden {
        // QueryTree still contains the client, but setup's pre-manage
        // admission reads WM_STATE only. Mapping this one safely parked
        // client changes only discoverability; EWMH retains hidden semantics
        // and exact V1 retains its restore geometry.
        RestartDisposition::NeedsMappedFallback
    } else {
        RestartDisposition::Reject("unmapped minimized public state is not recoverable")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectiveMapOp<K> {
    Apply(K),
    Rollback(K),
}

#[derive(Debug)]
struct SelectiveMapFailure<K, E> {
    failed: K,
    error: E,
    rollback_errors: Vec<(K, E)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestartClientTransactionOp<K> {
    Fallback(SelectiveMapOp<K>),
    SwallowedHandoff,
}

#[derive(Debug)]
enum RestartClientTransactionFailure<K, E> {
    Fallback(SelectiveMapFailure<K, E>),
    Swallowed {
        error: E,
        rollback_errors: Vec<(K, E)>,
    },
}

/// Apply only the requested fallback maps. A failed item is responsible for
/// its own atomicity; every earlier successful item is re-iconified in reverse
/// order, and rollback continues after individual errors.
fn run_selective_map_transaction<K: Copy, E>(
    clients: &[K],
    mut operation: impl FnMut(SelectiveMapOp<K>) -> Result<(), E>,
) -> Result<Vec<K>, SelectiveMapFailure<K, E>> {
    let mut applied = Vec::with_capacity(clients.len());
    for &client in clients {
        if let Err(error) = operation(SelectiveMapOp::Apply(client)) {
            let mut rollback_errors = Vec::new();
            for &mapped in applied.iter().rev() {
                if let Err(rollback_error) = operation(SelectiveMapOp::Rollback(mapped)) {
                    rollback_errors.push((mapped, rollback_error));
                }
            }
            return Err(SelectiveMapFailure {
                failed: client,
                error,
                rollback_errors,
            });
        }
        applied.push(client);
    }
    Ok(applied)
}

/// Commit the complete discoverability handoff. Swallowed parents are always
/// the final operation; if that bounded batch fails, every earlier selective
/// map is re-iconified in reverse order before the error is returned.
fn run_restart_client_transaction<K: Copy, E>(
    clients: &[K],
    mut operation: impl FnMut(RestartClientTransactionOp<K>) -> Result<(), E>,
) -> Result<Vec<K>, RestartClientTransactionFailure<K, E>> {
    let mapped = run_selective_map_transaction(clients, |fallback| {
        operation(RestartClientTransactionOp::Fallback(fallback))
    })
    .map_err(RestartClientTransactionFailure::Fallback)?;

    if let Err(error) = operation(RestartClientTransactionOp::SwallowedHandoff) {
        let mut rollback_errors = Vec::new();
        for &client in mapped.iter().rev() {
            if let Err(rollback_error) = operation(RestartClientTransactionOp::Fallback(
                SelectiveMapOp::Rollback(client),
            )) {
                rollback_errors.push((client, rollback_error));
            }
        }
        return Err(RestartClientTransactionFailure::Swallowed {
            error,
            rollback_errors,
        });
    }

    Ok(mapped)
}

fn restart_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.into()))
}

fn format_transaction_failure<K: fmt::Debug>(
    failure: SelectiveMapFailure<K, Box<dyn Error>>,
) -> Box<dyn Error> {
    let mut message = format!(
        "selective restart fallback failed for {:?}: {}",
        failure.failed, failure.error
    );
    if !failure.rollback_errors.is_empty() {
        message.push_str("; rollback failures:");
        for (client, error) in failure.rollback_errors {
            message.push_str(&format!(" {client:?}: {error};"));
        }
    }
    restart_error(message)
}

fn format_restart_client_transaction_failure<K: fmt::Debug>(
    failure: RestartClientTransactionFailure<K, Box<dyn Error>>,
) -> Box<dyn Error> {
    match failure {
        RestartClientTransactionFailure::Fallback(failure) => format_transaction_failure(failure),
        RestartClientTransactionFailure::Swallowed {
            error,
            rollback_errors,
        } => {
            let suffix = if rollback_errors.is_empty() {
                String::new()
            } else {
                let failures = rollback_errors
                    .into_iter()
                    .map(|(client, error)| format!("{client:?}: {error}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("; selective fallback rollback failures: {failures}")
            };
            restart_error(format!(
                "swallowed-parent restart handoff failed: {error}{suffix}"
            ))
        }
    }
}

/// A property write is restart-safe only when the checked write succeeds and
/// an immediate query on the same backend returns the exact expected value.
/// A stale matching value therefore cannot turn a failed write into a proof.
fn checked_write_readback<T: PartialEq, E>(
    expected: T,
    write: impl FnOnce() -> Result<(), E>,
    read: impl FnOnce() -> Result<T, E>,
) -> Result<bool, E> {
    write()?;
    Ok(read()? == expected)
}

impl Jwm {
    fn rollback_restart_fallback(
        &self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn Error>> {
        self.request_iconify_for_hidden_dock_client(backend, client_key)
    }

    /// Map one already-hidden true-Iconic client without changing its JWM,
    /// EWMH, ICCCM, Dock, or V1 semantics. Parking is repaired before mapping;
    /// a policy-level verification failure re-arms this same client before the
    /// error escapes.
    fn map_restart_fallback(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn Error>> {
        self.retry_x11_minimized_client_park(backend, client_key)?;
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.win)
            .ok_or_else(|| restart_error("client vanished before restart fallback map"))?;

        let verification = (|| -> Result<(), Box<dyn Error>> {
            backend.compositor_cancel_window_iconify(win)?;
            let mut attributes = backend.window_ops().get_window_attributes(win)?;
            if !attributes.map_state_viewable {
                // A desynchronised coordinator can legitimately have no
                // retained Iconic plan to cancel. In that case the compositor
                // hook is a no-op, so perform the narrow checked MapWindow
                // here instead of falsely treating the fallback as complete.
                backend.window_ops().map_window(win)?;
                backend.window_ops().flush()?;
                attributes = backend.window_ops().get_window_attributes(win)?;
            }
            if !attributes.map_state_viewable {
                return Err(restart_error(format!(
                    "{win:?} was not viewable after selective restart MapWindow"
                )));
            }
            let geometry = backend.window_ops().get_geometry(win)?;
            if !x11_geometry_fully_left_of_desktop(geometry, self.desktop_left_edge()) {
                return Err(restart_error(format!(
                    "{win:?} left its safe parking region during restart MapWindow"
                )));
            }
            Ok(())
        })();

        if let Err(error) = verification {
            if let Err(rollback_error) = self.rollback_restart_fallback(backend, client_key) {
                return Err(restart_error(format!(
                    "{error}; failed to re-iconify {win:?}: {rollback_error}"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    /// Prove that every persistent X11 client can be rediscovered by the
    /// replacement process before any destructive cleanup begins.
    pub(crate) fn prepare_restart_clients(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<PreparedRestartClients, Box<dyn Error>> {
        if !backend.capabilities().supports_client_list {
            return Ok(PreparedRestartClients {
                _mapped_fallbacks: Vec::new(),
            });
        }

        let root_children: HashSet<WindowId> =
            backend.window_ops().scan_windows()?.into_iter().collect();
        let bar_clients: HashSet<ClientKey> = self
            .secondary_bars
            .values()
            .filter_map(|bar| bar.client_key)
            .collect();
        let swallowed_parents: HashSet<ClientKey> = self
            .state
            .client_order
            .iter()
            .filter_map(|&key| {
                self.state
                    .clients
                    .get(key)
                    .and_then(|client| client.swallowing)
            })
            .chain(self.state.client_order.iter().copied().filter(|&key| {
                self.state
                    .clients
                    .get(key)
                    .is_some_and(|client| client.state.is_swallowed)
            }))
            .collect();
        let client_keys = self.state.client_order.clone();
        let mut fallbacks = Vec::new();

        for client_key in client_keys {
            if bar_clients.contains(&client_key) {
                continue;
            }
            let Some(initial_client) = self.state.clients.get(client_key) else {
                continue;
            };
            let hidden = initial_client.state.is_hidden;
            if hidden {
                self.retry_x11_minimized_client_park(backend, client_key)?;
            }

            let Some(client) = self.state.clients.get(client_key) else {
                return Err(restart_error(
                    "managed client vanished during restart probe",
                ));
            };
            let win = client.win;
            let spec = RestartClientSpec {
                hidden,
                dock_eligible: StatusBarBuilder::is_minimized_dock_eligible(client),
                swallowed_parent: swallowed_parents.contains(&client_key),
            };
            let attributes = backend.window_ops().get_window_attributes(win)?;
            let geometry = backend.window_ops().get_geometry(win)?;
            let safely_parked =
                !hidden || x11_geometry_fully_left_of_desktop(geometry, self.desktop_left_edge());

            let (wm_state_normal, wm_state_iconic, ewmh_hidden, ewmh_not_hidden) = if hidden {
                // Normalize both public minimized representations, but keep
                // their proofs independent. A backend write is trusted only
                // when the same connection reads the exact value back.
                let properties = backend.property_ops();
                let wm_state_iconic = match checked_write_readback(
                    i64::from(ICONIC_STATE),
                    || properties.set_wm_state(win, i64::from(ICONIC_STATE)),
                    || properties.get_wm_state(win),
                ) {
                    Ok(exact) => exact,
                    Err(error) => {
                        log::warn!("restart WM_STATE normalization failed for {win:?}: {error}");
                        false
                    }
                };
                let ewmh_hidden = match checked_write_readback(
                    true,
                    || properties.set_net_wm_state_flag(win, NetWmState::Hidden, true),
                    || properties.has_net_wm_state_flag(win, NetWmState::Hidden),
                ) {
                    Ok(exact) => exact,
                    Err(error) => {
                        log::warn!("restart EWMH Hidden normalization failed for {win:?}: {error}");
                        false
                    }
                };
                (false, wm_state_iconic, ewmh_hidden, false)
            } else {
                // Visible clients are observationally verified. Restart is
                // not an excuse to rewrite an ambiguous public lifecycle.
                let wm_state_normal = backend
                    .property_ops()
                    .get_wm_state(win)
                    .map(|state| state == i64::from(NORMAL_STATE))
                    .unwrap_or(false);
                let ewmh_not_hidden = backend
                    .property_ops()
                    .has_net_wm_state_flag(win, NetWmState::Hidden)
                    .map(|hidden| !hidden)
                    .unwrap_or(false);
                (wm_state_normal, false, false, ewmh_not_hidden)
            };

            let v1_exact = if hidden {
                match self.expected_minimized_restore_state(client_key) {
                    Some(expected) => match checked_write_readback(
                        Some(expected),
                        || {
                            backend
                                .property_ops()
                                .set_minimized_restore_state(win, expected)
                        },
                        || backend.property_ops().get_minimized_restore_state(win),
                    ) {
                        Ok(exact) => exact,
                        Err(error) => {
                            log::warn!(
                                "restart V1 write/readback failed for {win:?}; restart preflight will fail closed: {error}"
                            );
                            false
                        }
                    },
                    None => false,
                }
            } else {
                true
            };

            let probe = RestartClientProbe {
                in_root_tree: root_children.contains(&win),
                override_redirect: attributes.override_redirect,
                viewable: attributes.map_state_viewable,
                safely_parked,
                wm_state_normal,
                wm_state_iconic,
                ewmh_hidden,
                ewmh_not_hidden,
                v1_exact,
            };
            match classify_restart_client(spec, probe) {
                RestartDisposition::ReadyViewable
                | RestartDisposition::ReadyIconic
                | RestartDisposition::DeferredSwallowed => {}
                RestartDisposition::NeedsMappedFallback => fallbacks.push(client_key),
                RestartDisposition::Reject(reason) => {
                    return Err(restart_error(format!(
                        "restart client {win:?} failed discovery preflight: {reason}"
                    )));
                }
            }
        }

        // Swallowed parents are ordinary NormalState clients whose managed
        // UnmapWindow intentionally makes the replacement scan miss them.
        // Their checked batch map is the final operation in this pure
        // transaction, after every selective off-screen fallback.
        let mapped_fallbacks =
            run_restart_client_transaction(&fallbacks, |operation| match operation {
                RestartClientTransactionOp::Fallback(SelectiveMapOp::Apply(client_key)) => {
                    self.map_restart_fallback(backend, client_key)
                }
                RestartClientTransactionOp::Fallback(SelectiveMapOp::Rollback(client_key)) => {
                    self.rollback_restart_fallback(backend, client_key)
                }
                RestartClientTransactionOp::SwallowedHandoff => self
                    .prepare_swallowed_parents_for_handoff(backend)
                    .map_err(|error| Box::new(error) as Box<dyn Error>),
            })
            .map_err(format_restart_client_transaction_failure)?;

        Ok(PreparedRestartClients {
            _mapped_fallbacks: mapped_fallbacks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn spec(hidden: bool, dock_eligible: bool, swallowed_parent: bool) -> RestartClientSpec {
        RestartClientSpec {
            hidden,
            dock_eligible,
            swallowed_parent,
        }
    }

    fn probe(viewable: bool) -> RestartClientProbe {
        RestartClientProbe {
            in_root_tree: true,
            override_redirect: false,
            viewable,
            safely_parked: true,
            wm_state_normal: true,
            wm_state_iconic: true,
            ewmh_hidden: true,
            ewmh_not_hidden: true,
            v1_exact: true,
        }
    }

    #[test]
    fn classification_preserves_exact_iconic_and_maps_only_inconclusive_iconic() {
        assert_eq!(
            classify_restart_client(spec(true, true, false), probe(false)),
            RestartDisposition::ReadyIconic
        );

        let mut missing_wm_state = probe(false);
        missing_wm_state.wm_state_iconic = false;
        assert_eq!(
            classify_restart_client(spec(true, true, false), missing_wm_state),
            RestartDisposition::NeedsMappedFallback
        );

        let mut missing_ewmh = probe(false);
        missing_ewmh.ewmh_hidden = false;
        assert_eq!(
            classify_restart_client(spec(true, true, false), missing_ewmh),
            RestartDisposition::ReadyIconic
        );

        let mut missing_both = missing_ewmh;
        missing_both.wm_state_iconic = false;
        assert!(matches!(
            classify_restart_client(spec(true, true, false), missing_both),
            RestartDisposition::Reject(_)
        ));

        let mut missing_v1 = probe(false);
        missing_v1.v1_exact = false;
        assert!(matches!(
            classify_restart_client(spec(true, true, false), missing_v1),
            RestartDisposition::Reject("minimized V1 readback is not exact")
        ));
    }

    #[test]
    fn classification_rejects_unsafe_or_non_adoptable_clients() {
        let mut missing = probe(true);
        missing.in_root_tree = false;
        assert!(matches!(
            classify_restart_client(spec(false, true, false), missing),
            RestartDisposition::Reject(_)
        ));

        let mut override_redirect = probe(true);
        override_redirect.override_redirect = true;
        assert!(matches!(
            classify_restart_client(spec(false, true, false), override_redirect),
            RestartDisposition::Reject(_)
        ));

        let visible = probe(false);
        assert!(matches!(
            classify_restart_client(spec(false, true, false), visible),
            RestartDisposition::Reject("non-minimized client is unmapped")
        ));

        let mut ambiguous_visible = probe(true);
        ambiguous_visible.wm_state_normal = false;
        assert!(matches!(
            classify_restart_client(spec(false, true, false), ambiguous_visible),
            RestartDisposition::Reject("visible client is not exactly NormalState and non-Hidden")
        ));

        assert_eq!(
            classify_restart_client(spec(true, false, false), probe(false)),
            RestartDisposition::ReadyIconic
        );
        let mut ineligible_needs_map = probe(false);
        ineligible_needs_map.wm_state_iconic = false;
        assert!(matches!(
            classify_restart_client(spec(true, false, false), ineligible_needs_map),
            RestartDisposition::Reject("Dock-ineligible minimized client needs a mapped fallback")
        ));
    }

    #[test]
    fn mapped_hidden_and_swallowed_clients_have_distinct_safe_paths() {
        let mut degraded = probe(true);
        degraded.wm_state_iconic = false;
        assert_eq!(
            classify_restart_client(spec(true, true, false), degraded),
            RestartDisposition::ReadyViewable
        );
        assert_eq!(
            classify_restart_client(spec(false, false, true), probe(false)),
            RestartDisposition::DeferredSwallowed
        );
    }

    #[test]
    fn selective_map_failure_reiconifies_prior_successes_in_reverse_order() {
        let mut operations = Vec::new();
        let failure = run_selective_map_transaction(&[1_u8, 2, 3, 4], |operation| {
            operations.push(operation);
            match operation {
                SelectiveMapOp::Apply(3) => Err("map failed"),
                _ => Ok(()),
            }
        })
        .unwrap_err();

        assert_eq!(failure.failed, 3);
        assert_eq!(failure.error, "map failed");
        assert!(failure.rollback_errors.is_empty());
        assert_eq!(
            operations,
            vec![
                SelectiveMapOp::Apply(1),
                SelectiveMapOp::Apply(2),
                SelectiveMapOp::Apply(3),
                SelectiveMapOp::Rollback(2),
                SelectiveMapOp::Rollback(1),
            ]
        );
    }

    #[test]
    fn selective_map_rollback_keeps_going_after_a_rollback_error() {
        let failure = run_selective_map_transaction(&[1_u8, 2, 3], |operation| match operation {
            SelectiveMapOp::Apply(3) => Err("primary"),
            SelectiveMapOp::Rollback(2) => Err("rollback two"),
            _ => Ok(()),
        })
        .unwrap_err();

        assert_eq!(failure.rollback_errors, vec![(2, "rollback two")]);
    }

    #[test]
    fn swallowed_handoff_is_last_and_rolls_back_fallbacks_in_reverse() {
        let mut operations = Vec::new();
        let failure = run_restart_client_transaction(&[1_u8, 2], |operation| {
            operations.push(operation);
            if operation == RestartClientTransactionOp::SwallowedHandoff {
                Err("swallowed map failed")
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert!(matches!(
            failure,
            RestartClientTransactionFailure::Swallowed {
                error: "swallowed map failed",
                rollback_errors,
            } if rollback_errors.is_empty()
        ));
        assert_eq!(
            operations,
            vec![
                RestartClientTransactionOp::Fallback(SelectiveMapOp::Apply(1)),
                RestartClientTransactionOp::Fallback(SelectiveMapOp::Apply(2)),
                RestartClientTransactionOp::SwallowedHandoff,
                RestartClientTransactionOp::Fallback(SelectiveMapOp::Rollback(2)),
                RestartClientTransactionOp::Fallback(SelectiveMapOp::Rollback(1)),
            ]
        );
    }

    #[test]
    fn checked_readback_requires_a_successful_write_and_an_exact_value() {
        assert_eq!(
            checked_write_readback(3_i64, || Ok::<_, &str>(()), || Ok(3)),
            Ok(true)
        );
        assert_eq!(
            checked_write_readback(3_i64, || Ok::<_, &str>(()), || Ok(1)),
            Ok(false)
        );

        let read_called = Cell::new(false);
        let failed = checked_write_readback(
            3_i64,
            || Err::<(), _>("checked write failed"),
            || {
                read_called.set(true);
                Ok(3)
            },
        );
        assert_eq!(failed, Err("checked write failed"));
        assert!(
            !read_called.get(),
            "a stale matching value must not rescue a failed write"
        );
    }
}
