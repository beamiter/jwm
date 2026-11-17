//! v30 unified-control repair payloads. Layout reuse does not share authorization.
//! Dispatch only through control message kinds 44..=48 and the control lease state.
pub(crate) use super::pointer_repair_phase::{ActionReceipt, Scope, Token};
pub(crate) use super::pointer_repair_wire::{RepairStatus, RepairStatusKind};
