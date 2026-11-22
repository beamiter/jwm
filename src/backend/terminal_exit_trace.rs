//! Opt-in diagnostic only. Disabled sessions perform no file IO.
//! Pre-open at backend startup; buffer markers until after render returns.
//! Diagnostic runs incur clock/lock/buffer and post-submit write overhead.
use std::cell::Cell;
use std::io::{BufWriter, Write};
thread_local! { static ACTIVE: Cell<bool> = const { Cell::new(false) }; }

#[cfg_attr(not(feature = "backend-wayland-nested"), allow(dead_code))]
pub(crate) fn render_scope(active: bool) {
    ACTIVE.with(|value| value.set(active));
}
#[cfg_attr(not(feature = "backend-wayland-nested"), allow(dead_code))]
pub(crate) fn mark_submit(event: &'static str) {
    if ACTIVE.with(Cell::get) {
        mark(event);
    }
}

use std::sync::{Mutex, OnceLock};

type Trace = Option<Mutex<BufWriter<std::fs::File>>>;
static TRACE: OnceLock<Trace> = OnceLock::new();

#[cfg_attr(not(feature = "backend-wayland-nested"), allow(dead_code))]
pub(crate) fn init() {
    TRACE.get_or_init(|| {
        let path = std::env::var_os("JWM_TERMINAL_EXIT_TRACE")?;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .ok()
            .map(|file| Mutex::new(BufWriter::with_capacity(4096, file)))
    });
}

pub(crate) fn mark(event: &'static str) {
    let Some(Some(file)) = TRACE.get() else {
        return;
    };
    let Ok(mut file) = file.lock() else { return };
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime initializes the valid timespec pointer on success.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0 {
        let ns = i128::from(time.tv_sec) * 1_000_000_000 + i128::from(time.tv_nsec);
        let _ = writeln!(file, "{ns} {event}");
        if event.starts_with("render_return_") {
            let _ = file.flush();
        }
    }
}
