//! Process-lifetime links from admitted operations to their original dialogue.
//! The map confers no execution rights and is never reconstructed from a new
//! voice activation. Historical operations without a link stay unclaimed.
use companion_voice::dialogue::{Event, Outcome, Record};
use std::{
    collections::BTreeMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    session: String,
    turn: String,
    call: String,
    plan: String,
    epoch: u64,
}
struct Binding {
    owner: Owner,
    instance: String,
    action_digest: String,
    request_digest: String,
    last: Option<Outcome>,
}
static BINDINGS: OnceLock<Mutex<BTreeMap<String, Binding>>> = OnceLock::new();
static EPOCH: AtomicU64 = AtomicU64::new(1);

pub(super) fn disconnected() {
    if EPOCH
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .is_err()
    {
        failed();
    }
    if let Some(bindings) = BINDINGS.get() {
        bindings.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}
pub(super) fn current(epoch: u64) -> bool {
    epoch == EPOCH.load(Ordering::Acquire)
}

pub(crate) struct Settlement<'a> {
    pub id: &'a str,
    pub plan: &'a str,
    pub instance: &'a str,
    pub action_digest: &'a str,
    pub result_digest: &'a str,
    pub outcome: Outcome,
}

fn failed() {
    super::LOST.store(true, Ordering::Release);
}
impl Owner {
    pub(super) fn new(session: &str, turn: String, call: &str, plan: &str) -> Option<Self> {
        if super::BRIDGE.get().is_none() || super::LOST.load(Ordering::Acquire) {
            return None;
        }
        Some(Self {
            session: session.into(),
            turn,
            call: call.into(),
            plan: plan.into(),
            epoch: EPOCH.load(Ordering::Acquire),
        })
    }
    pub(crate) fn with_plan(&self, plan: &str) -> Self {
        Self {
            plan: plan.into(),
            ..self.clone()
        }
    }
    fn record(&self, event: Event) -> Option<Record> {
        let record = Record {
            voice_session_id: self.session.clone(),
            turn_id: self.turn.clone(),
            observed_at_ns: companion_voice::boottime_ns()?,
            event,
        };
        record.valid().then_some(record)
    }
    pub(crate) fn register(
        &self,
        id: &str,
        instance: &str,
        action_digest: &str,
        request_digest: &str,
    ) {
        if self.epoch != EPOCH.load(Ordering::Acquire) {
            return;
        }
        let Some(record) = self.record(Event::OperationRegistered {
            call_id: self.call.clone(),
            operation_id: id.into(),
            plan_hash: self.plan.clone(),
            wm_instance_id: instance.into(),
            request_digest: request_digest.into(),
        }) else {
            failed();
            return;
        };
        let mut bindings = BINDINGS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.epoch != EPOCH.load(Ordering::Acquire) {
            return;
        }
        if let Some(bound) = bindings.get(id) {
            if bound.owner != *self
                || bound.instance != instance
                || bound.action_digest != action_digest
                || bound.request_digest != request_digest
            {
                failed();
            }
            return;
        }
        if bindings.len() >= 4096 {
            failed();
            return;
        }
        // Emit while holding this short metadata lock so a concurrent status
        // query cannot enqueue the settlement before its registration.
        super::emit(super::Event::Operation(self.epoch, record));
        bindings.insert(
            id.into(),
            Binding {
                owner: self.clone(),
                instance: instance.into(),
                action_digest: action_digest.into(),
                request_digest: request_digest.into(),
                last: None,
            },
        );
    }
}

pub(crate) fn tracked(id: &str) -> bool {
    BINDINGS.get().is_some_and(|map| {
        map.lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    })
}
pub(crate) fn settled(update: Settlement<'_>) {
    let Some(map) = BINDINGS.get() else { return };
    let mut bindings = map.lock().unwrap_or_else(|e| e.into_inner());
    let Some(binding) = bindings.get_mut(update.id) else {
        return;
    };
    if binding.owner.plan != update.plan
        || binding.instance != update.instance
        || binding.action_digest != update.action_digest
    {
        failed();
        return;
    }
    if let Some(previous) = binding.last {
        if previous == update.outcome || update.outcome == Outcome::Unknown {
            return;
        }
        if previous != Outcome::Unknown {
            failed();
            return;
        }
    }
    let Some(record) = binding.owner.record(Event::OperationSettled {
        operation_id: update.id.into(),
        request_digest: binding.request_digest.clone(),
        result_digest: update.result_digest.into(),
        outcome: update.outcome,
    }) else {
        failed();
        return;
    };
    super::emit(super::Event::Operation(binding.owner.epoch, record));
    binding.last = Some(update.outcome);
    // Terminal identities remain in the native execution ledger and the
    // shared journal. Only unresolved operations need a live callback link.
    if matches!(
        update.outcome,
        Outcome::Succeeded | Outcome::Failed | Outcome::Cancelled
    ) {
        bindings.remove(update.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_operation_owner_cannot_register_after_transport_epoch_changes() {
        let old = Owner {
            session: "old-session".into(),
            turn: "old-turn".into(),
            call: "old-call".into(),
            plan: "a".repeat(64),
            epoch: EPOCH.load(Ordering::Acquire),
        };
        disconnected();
        assert!(!current(old.epoch));
        old.register("old-operation", "wm", &"b".repeat(64), &"c".repeat(64));
        assert!(!tracked("old-operation"));
    }
}
