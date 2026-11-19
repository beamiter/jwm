//! Opt-in measurement only. Queue positions are recorded under the event lock.
//! No scheduling changes; CLOCK_MONOTONIC submit return is NOT presentation.
#![cfg_attr(not(feature = "backend-wayland-nested"), allow(dead_code))]
use std::{
    io::Write,
    sync::{Mutex, OnceLock},
};

#[derive(Default)]
struct Binding {
    next: u64,
    pending: Vec<(usize, u64)>,
}
impl Binding {
    fn enqueue(&mut self, position: usize) -> Option<u64> {
        if self.pending.len() >= 128 {
            return None;
        }
        self.next = self.next.checked_add(1)?;
        self.pending.push((position, self.next));
        Some(self.next)
    }
    fn pop(&mut self) -> Option<u64> {
        let found = self
            .pending
            .iter()
            .find(|(position, _)| *position == 0)
            .map(|(_, id)| *id);
        self.pending.retain(|(position, _)| *position > 0);
        for (position, _) in &mut self.pending {
            *position -= 1;
        }
        found
    }
}
struct Trace {
    file: std::fs::File,
    binding: Binding,
    cycle: u64,
    active: Option<u64>,
    rows: Vec<(i128, u64, u64, &'static str)>,
    overflow: bool,
}
static TRACE: OnceLock<Option<Mutex<Trace>>> = OnceLock::new();
fn with(f: impl FnOnce(&mut Trace)) {
    if let Some(Some(trace)) = TRACE.get() {
        if let Ok(mut trace) = trace.lock() {
            f(&mut trace);
        }
    }
}
impl Trace {
    fn mark(&mut self, id: u64, event: &'static str) {
        if self.rows.len() >= 4096 {
            self.overflow = true;
            return;
        }
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid writable timespec, inspected only after success.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
            self.overflow = true;
            return;
        }
        self.rows.push((
            i128::from(time.tv_sec) * 1_000_000_000 + i128::from(time.tv_nsec),
            id,
            self.cycle,
            event,
        ));
    }
    fn flush(&mut self) {
        for (ns, seq, cycle, event) in self.rows.drain(..) {
            let _ = writeln!(self.file, "{ns} {seq} 57 {cycle} {event}");
        }
        if self.overflow {
            let _ = writeln!(self.file, "INVALID overflow-or-clock-failure");
        }
        let _ = self.file.flush();
    }
}
pub(crate) fn init() {
    TRACE.get_or_init(|| {
        use std::os::unix::fs::OpenOptionsExt;
        let path = std::env::var_os("JWM_WINIT_QUEUE_TRACE")?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .ok()?;
        Some(Mutex::new(Trace {
            file,
            binding: Binding::default(),
            cycle: 0,
            active: None,
            rows: Vec::with_capacity(4096),
            overflow: false,
        }))
    });
}
pub(crate) fn cycle() {
    with(|t| {
        if let Some(next) = t.cycle.checked_add(1) {
            t.cycle = next;
        } else {
            t.overflow = true;
        }
    });
}
pub(crate) fn enqueue(position: usize, keycode: u8) {
    if keycode != 57 {
        return;
    }
    with(|t| {
        if let Some(id) = t.binding.enqueue(position) {
            t.mark(id, "enqueue");
        } else {
            t.overflow = true;
        }
    });
}
pub(crate) fn pop(keycode: Option<u8>) {
    with(|t| {
        t.active = t.binding.pop();
        if let Some(id) = t.active {
            if keycode != Some(57) {
                t.mark(id, "INVALID-binding");
                t.active = None;
            } else {
                t.mark(id, "dequeue");
            }
        }
    });
}
pub(crate) fn mark(event: &'static str) {
    with(|t| {
        if let Some(id) = t.active {
            t.mark(id, event);
        }
    });
}
pub(crate) fn sleep(event: &'static str) {
    with(|t| {
        let ids: Vec<_> = t.binding.pending.iter().map(|(_, id)| *id).collect();
        for id in ids {
            t.mark(id, event);
        }
    });
}
pub(crate) fn discarded() {
    with(|t| {
        let ids: Vec<_> = t.binding.pending.drain(..).map(|(_, id)| id).collect();
        for id in ids {
            t.mark(id, "discarded-before-dequeue");
        }
    });
}
pub(crate) fn finish_handler() {
    with(|t| {
        if let Some(id) = t.active.take() {
            t.mark(id, "handler_return");
            t.flush();
        }
    });
}
pub(crate) fn finish_run() {
    with(|t| t.flush());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_same_key_has_distinct_queue_identity() {
        let mut b = Binding::default();
        let first = b.enqueue(1).unwrap();
        let second = b.enqueue(3).unwrap();
        assert_ne!(first, second);
        assert_eq!(b.pop(), None);
        assert_eq!(b.pop(), Some(first));
        assert_eq!(b.pop(), None);
        assert_eq!(b.pop(), Some(second));
    }
    #[test]
    fn bounded_capacity_and_id_overflow_fail_closed() {
        let mut b = Binding {
            next: u64::MAX,
            pending: Vec::new(),
        };
        assert_eq!(b.enqueue(0), None);
        b = Binding::default();
        for position in 0..128 {
            assert!(b.enqueue(position).is_some());
        }
        assert_eq!(b.enqueue(128), None);
        let mut trace = Trace {
            file: std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .unwrap(),
            binding: b,
            cycle: 0,
            active: None,
            rows: vec![(0, 0, 0, "test"); 4096],
            overflow: false,
        };
        trace.mark(1, "enqueue");
        assert!(trace.overflow);
        assert_eq!(trace.rows.len(), 4096);
    }
    #[test]
    fn discarded_targets_cannot_bind_later_input() {
        let mut b = Binding::default();
        let old = b.enqueue(0).unwrap();
        b.pending.clear();
        let new = b.enqueue(0).unwrap();
        assert_ne!(old, new);
        assert_eq!(b.pop(), Some(new));
        assert_eq!(b.pop(), None);
    }
}
