//! Shared bounded, lazy wallpaper decode slots. Dropping a queued slot never spawns work.
use super::wallpaper::{WallpaperImageData, WallpaperMode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
/// Counting gate bounding how many wallpaper images decode concurrently.
/// Each decode does `image::open` + a Lanczos3 downscale (heavy CPU, transient
/// full-image allocation); rapid wallpaper changes or per-monitor setup would
/// otherwise run unbounded decodes at once.
struct DecodeGate {
    /// The number of currently-available decode permits.
    available: Mutex<usize>,
}

impl DecodeGate {
    const fn new(permits: usize) -> Self {
        Self {
            available: Mutex::new(permits),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, usize> {
        self.available.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn try_acquire(&self) -> Option<DecodePermit<'_>> {
        let mut available = self.lock();
        if *available == 0 {
            return None;
        }
        *available -= 1;
        Some(DecodePermit { gate: self })
    }
}

/// The process-wide decode gate shared by wallpaper loads and side previews.
fn decode_gate() -> &'static DecodeGate {
    static GATE: OnceLock<DecodeGate> = OnceLock::new();
    GATE.get_or_init(|| {
        let max = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(2);
        DecodeGate::new(max)
    })
}

/// RAII permit for a [`DecodeGate`]; returns the permit on drop (covering
/// early returns and panics).
struct DecodePermit<'a> {
    gate: &'a DecodeGate,
}

impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        let mut available = self.gate.lock();
        *available += 1;
    }
}

type WallpaperDecode = Box<dyn FnOnce(&AtomicBool) -> Option<WallpaperImageData> + Send>;
type DecodeWorker = Box<dyn FnOnce() + Send>;

enum WallpaperLoadState {
    Queued(WallpaperDecode),
    Running(mpsc::Receiver<WallpaperImageData>),
    Failed,
}

/// A per-request CPU decode queue slot. Polling starts a worker only after a
/// permit is available, so replaced requests never leave threads waiting on
/// the gate. Each monitor retains its own slot and result; no global latest-
/// request policy is applied to real wallpapers.
pub(crate) struct DeferredWallpaperLoad {
    state: Mutex<WallpaperLoadState>,
    cancelled: Arc<AtomicBool>,
}

impl DeferredWallpaperLoad {
    pub(crate) fn new(
        decode: impl FnOnce(&AtomicBool) -> Option<WallpaperImageData> + Send + 'static,
    ) -> Self {
        Self {
            state: Mutex::new(WallpaperLoadState::Queued(Box::new(decode))),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn try_recv(&self) -> Result<WallpaperImageData, mpsc::TryRecvError> {
        self.try_recv_with(decode_gate(), |worker| {
            std::thread::Builder::new()
                .name("jwm-wallpaper".into())
                .spawn(worker)
                .map(|_| ())
        })
    }

    fn try_recv_with(
        &self,
        gate: &'static DecodeGate,
        spawn: impl FnOnce(DecodeWorker) -> std::io::Result<()>,
    ) -> Result<WallpaperImageData, mpsc::TryRecvError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, WallpaperLoadState::Queued(_)) {
            let Some(permit) = gate.try_acquire() else {
                return Err(mpsc::TryRecvError::Empty);
            };
            let WallpaperLoadState::Queued(decode) =
                std::mem::replace(&mut *state, WallpaperLoadState::Failed)
            else {
                unreachable!();
            };
            let (tx, rx) = mpsc::channel();
            let cancelled = Arc::clone(&self.cancelled);
            let worker = Box::new(move || {
                let _permit = permit;
                if !cancelled.load(Ordering::Acquire)
                    && let Some(image) = decode(&cancelled)
                    && !cancelled.load(Ordering::Acquire)
                {
                    let _ = tx.send(image);
                }
            });
            match spawn(worker) {
                Ok(()) => *state = WallpaperLoadState::Running(rx),
                Err(error) => log::warn!("compositor: could not start wallpaper decoder: {error}"),
            }
        }
        match &*state {
            WallpaperLoadState::Running(rx) => rx.try_recv(),
            WallpaperLoadState::Failed => Err(mpsc::TryRecvError::Disconnected),
            WallpaperLoadState::Queued(_) => unreachable!(),
        }
    }
}

impl Drop for DeferredWallpaperLoad {
    fn drop(&mut self) {
        // A running file decode cannot be interrupted; it still owns its
        // permit until it exits. Queued work is dropped without spawning.
        self.cancelled.store(true, Ordering::Release);
    }
}

pub(crate) fn load_wallpaper(
    path: &str,
    max_w: u32,
    max_h: u32,
    mode: WallpaperMode,
) -> DeferredWallpaperLoad {
    let path = path.to_owned();
    DeferredWallpaperLoad::new(move |cancelled| {
        let img = super::image_source::open_regular_image(&path)
            .map_err(|e| log::warn!("wallpaper: failed to decode '{}': {}", path, e))
            .ok()?;
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        let img = super::wallpaper::prepare_wallpaper_image(img, max_w, max_h, mode);
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        let rgba = img.to_rgba8();
        Some(WallpaperImageData {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
            mode,
        })
    })
}
#[cfg(test)]
mod deferred_load_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    // Leaked tiny gates give injected worker closures the same 'static permit
    // lifetime as the process-wide production gate, without shared test state.
    fn gate(permits: usize) -> &'static DecodeGate {
        Box::leak(Box::new(DecodeGate::new(permits)))
    }

    fn image(id: u32) -> WallpaperImageData {
        WallpaperImageData {
            rgba: vec![id as u8; 4],
            width: id,
            height: 1,
            mode: WallpaperMode::Fit,
        }
    }

    #[test]
    fn saturated_loads_wait_in_slots_and_all_monitors_eventually_complete() {
        let gate = gate(4);
        let loads: Vec<_> = (1..=16)
            .map(|id| DeferredWallpaperLoad::new(move |_| Some(image(id))))
            .collect();
        let mut workers = VecDeque::new();
        let mut completed = vec![false; loads.len()];
        for (i, load) in loads.iter().enumerate() {
            assert!(matches!(
                load.try_recv_with(gate, |worker| {
                    workers.push_back(worker);
                    Ok(())
                }),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert!(
                workers.len() <= 4,
                "request {i} must not spawn a waiting thread"
            );
        }
        assert_eq!(workers.len(), 4);
        assert_eq!(*gate.lock(), 0);
        for _ in 0..16 {
            workers.pop_front().expect("a worker can make progress")();
            for (i, load) in loads.iter().enumerate() {
                if completed[i] {
                    continue;
                }
                match load.try_recv_with(gate, |worker| {
                    workers.push_back(worker);
                    Ok(())
                }) {
                    Ok(data) => {
                        assert_eq!(data.width, i as u32 + 1);
                        assert_eq!(data.rgba, vec![i as u8 + 1; 4]);
                        completed[i] = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => panic!("lost monitor {i}"),
                }
                assert!(workers.len() <= 4);
            }
        }
        assert!(completed.into_iter().all(|done| done));
        assert!(workers.is_empty());
        assert_eq!(*gate.lock(), 4);
    }

    #[test]
    fn replacing_or_dropping_a_queued_load_does_not_spawn_or_cancel_other_monitors() {
        let gate = gate(1);
        let held = gate.try_acquire().unwrap();
        let marker = Arc::new(());
        let weak = Arc::downgrade(&marker);
        let mut slot = Some(DeferredWallpaperLoad::new(move |_| {
            drop(marker);
            panic!("replaced decode ran")
        }));
        let independent = DeferredWallpaperLoad::new(|_| Some(image(2)));
        assert!(matches!(
            slot.as_ref()
                .unwrap()
                .try_recv_with(gate, |_| panic!("no permit")),
            Err(mpsc::TryRecvError::Empty)
        ));
        slot = Some(DeferredWallpaperLoad::new(|_| Some(image(3))));
        assert!(weak.upgrade().is_none());
        drop(slot);
        drop(held);
        let result = independent
            .try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap();
        assert_eq!(result.width, 2);
        assert_eq!(*gate.lock(), 1);
    }

    #[test]
    fn wallpaper_decode_failure_does_not_retry_from_polling_alone() {
        let gate = gate(1);
        let decoded = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&decoded);
        let load = DeferredWallpaperLoad::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            None
        });
        assert!(matches!(
            load.try_recv_with(gate, |worker| {
                worker();
                Ok(())
            }),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        for _ in 0..10 {
            assert!(matches!(
                load.try_recv_with(gate, |_| panic!("poll must not retry")),
                Err(mpsc::TryRecvError::Disconnected)
            ));
        }
        assert_eq!(decoded.load(Ordering::Relaxed), 1);
        // Explicit refresh replaces the failed slot with a fresh request.
        let replacement = DeferredWallpaperLoad::new(|_| Some(image(7)));
        assert_eq!(
            replacement
                .try_recv_with(gate, |worker| {
                    worker();
                    Ok(())
                })
                .unwrap()
                .width,
            7
        );
    }

    #[test]
    fn wallpaper_superseded_failure_cannot_poison_current_request() {
        let gate = gate(2);
        let mut old_worker = None;
        let old = DeferredWallpaperLoad::new(|_| None);
        assert!(matches!(
            old.try_recv_with(gate, |worker| {
                old_worker = Some(worker);
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        // Production drops the old receiver before installing its replacement.
        drop(old);
        let current = DeferredWallpaperLoad::new(|_| Some(image(8)));
        old_worker.take().unwrap()();
        let result = current.try_recv_with(gate, |worker| {
            worker();
            Ok(())
        });
        assert_eq!(result.unwrap().width, 8);
    }

    #[test]
    fn spawn_failure_disconnects_and_returns_the_permit() {
        let gate = gate(1);
        let load = DeferredWallpaperLoad::new(|_| panic!("failed spawn must not decode"));
        assert!(matches!(
            load.try_recv_with(gate, |_| Err(std::io::Error::other(
                "injected spawn failure"
            ))),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(*gate.lock(), 1);
        assert!(matches!(
            load.try_recv_with(gate, |_| panic!("failed request must not retry")),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        let next = DeferredWallpaperLoad::new(|_| Some(image(4)));
        assert_eq!(
            next.try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap()
            .width,
            4
        );
    }

    #[test]
    fn dropping_a_started_request_cancels_only_it_and_keeps_its_permit_until_exit() {
        let gate = gate(1);
        let decoded = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&decoded);
        let load = DeferredWallpaperLoad::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Some(image(1))
        });
        let mut worker = None;
        assert!(matches!(
            load.try_recv_with(gate, |job| {
                worker = Some(job);
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(load);
        assert_eq!(*gate.lock(), 0);
        worker.take().unwrap()();
        assert_eq!(decoded.load(Ordering::Relaxed), 0);
        assert_eq!(*gate.lock(), 1);

        let (started_tx, started_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let load = DeferredWallpaperLoad::new(move |cancelled| {
            started_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            cancel_tx.send(cancelled.load(Ordering::Acquire)).unwrap();
            Some(image(2))
        });
        let mut thread = None;
        assert!(matches!(
            load.try_recv_with(gate, |job| {
                thread = Some(std::thread::spawn(job));
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        drop(load);
        assert_eq!(*gate.lock(), 0);
        continue_tx.send(()).unwrap();
        assert!(
            cancel_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        thread.take().unwrap().join().unwrap();
        assert_eq!(*gate.lock(), 1);
    }
}
