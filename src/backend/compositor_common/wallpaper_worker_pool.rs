//! Fixed, bounded wallpaper dispatch. File access remains inside submitted jobs.
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

pub(super) type Job = Box<dyn FnOnce() + Send>;
pub(super) const MAX_WORKERS: usize = 4;
// Every production job already owns a gate permit, and that gate has at
// most MAX_WORKERS permits. Even if no worker has received a submitted job,
// all permitted jobs fit: Full can only arise from non-production submitters.
const QUEUE_CAPACITY: usize = MAX_WORKERS;

pub(super) struct WorkerPool {
    sender: mpsc::SyncSender<Job>,
    #[cfg(test)]
    workers: usize,
}

impl WorkerPool {
    pub(super) fn start_with(mut spawn: impl FnMut(Job) -> io::Result<()>) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = 0;
        let mut last_error = None;
        for _ in 0..MAX_WORKERS {
            let receiver = Arc::clone(&receiver);
            match spawn(Box::new(move || {
                loop {
                    // Release the receiver lock before executing: another worker
                    // can receive immediately while this one processes its image.
                    let job = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(job) = job else { break };
                    // Unwinding drops the job's sender and decode permit, while
                    // this worker survives to drain subsequent requests.
                    let _ = catch_unwind(AssertUnwindSafe(job));
                }
            })) {
                Ok(()) => workers += 1,
                Err(error) => last_error = Some(error),
            }
        }
        if workers == 0 {
            return Err(last_error.unwrap_or_else(|| io::Error::other("no wallpaper workers")));
        }
        Ok(Self {
            sender,
            #[cfg(test)]
            workers,
        })
    }

    pub(super) fn submit(&self, job: Job) -> io::Result<()> {
        self.sender.try_send(job).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => {
                io::Error::new(io::ErrorKind::WouldBlock, "wallpaper worker queue is full")
            }
            mpsc::TrySendError::Disconnected(_) => {
                io::Error::new(io::ErrorKind::BrokenPipe, "wallpaper workers disconnected")
            }
        })
    }
}

/// A failed initial start leaves POOL empty so a later request can retry.
/// Concurrent initialization never waits on another initializer in a UI poll.
pub(super) fn get_or_start() -> io::Result<&'static WorkerPool> {
    static POOL: OnceLock<WorkerPool> = OnceLock::new();
    static STARTING: Mutex<()> = Mutex::new(());
    get_or_start_with(&POOL, &STARTING, |worker| {
        std::thread::Builder::new()
            .name("jwm-wallpaper".into())
            .spawn(worker)
            .map(|_| ())
    })
}

fn get_or_start_with<'a>(
    pool: &'a OnceLock<WorkerPool>,
    starting: &Mutex<()>,
    spawn: impl FnMut(Job) -> io::Result<()>,
) -> io::Result<&'a WorkerPool> {
    if let Some(pool) = pool.get() {
        return Ok(pool);
    }
    let _starting = starting
        .try_lock()
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "wallpaper workers are starting"))?;
    if let Some(pool) = pool.get() {
        return Ok(pool);
    }
    let started = WorkerPool::start_with(spawn)?;
    Ok(pool.get_or_init(|| started))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::Duration;

    #[test]
    fn fixed_workers_reuse_threads_and_survive_a_panicking_job() {
        let pool = WorkerPool::start_with(|worker| {
            std::thread::spawn(worker);
            Ok(())
        })
        .unwrap();
        assert_eq!(pool.workers, MAX_WORKERS);
        let (tx, rx) = mpsc::channel();
        let mut ids = HashSet::new();
        for _ in 0..100 {
            let tx = tx.clone();
            pool.submit(Box::new(move || {
                tx.send(std::thread::current().id()).unwrap();
            }))
            .unwrap();
            ids.insert(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        assert!(ids.len() <= MAX_WORKERS);
        let panic_tx = tx.clone();
        pool.submit(Box::new(move || {
            panic_tx.send(std::thread::current().id()).unwrap();
            panic!("injected wallpaper job panic");
        }))
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // A subsequent full wave still completes; the panic cannot kill a worker.
        for _ in 0..MAX_WORKERS {
            let tx = tx.clone();
            pool.submit(Box::new(move || {
                tx.send(std::thread::current().id()).unwrap();
            }))
            .unwrap();
        }
        for _ in 0..MAX_WORKERS {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }

    #[test]
    fn bounded_queue_rejects_without_waiting_and_drops_rejected_jobs() {
        // Capture worker entry points without running them: no consumer can
        // race the queue capacity assertion.
        let mut workers = Vec::new();
        let pool = WorkerPool::start_with(|worker| {
            workers.push(worker);
            Ok(())
        })
        .unwrap();
        for _ in 0..QUEUE_CAPACITY {
            pool.submit(Box::new(|| {})).unwrap();
        }
        let marker = Arc::new(());
        let held = Arc::clone(&marker);
        let error = pool.submit(Box::new(move || drop(held))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(Arc::strong_count(&marker), 1);
        drop(pool);
        for worker in workers {
            worker();
        }
    }

    #[test]
    fn initialization_contention_does_not_wait_or_publish_a_failed_pool() {
        let pool = OnceLock::new();
        let starting = Mutex::new(());
        let guard = starting.lock().unwrap();
        let result = get_or_start_with(&pool, &starting, |_| panic!("initializer already active"));
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::WouldBlock));
        assert!(pool.get().is_none());
        drop(guard);
        assert!(
            get_or_start_with(&pool, &starting, |_| Err(io::Error::other("no threads"))).is_err()
        );
        assert!(pool.get().is_none());
        let retry = get_or_start_with(&pool, &starting, |worker| {
            std::thread::spawn(worker);
            Ok(())
        })
        .unwrap();
        assert_eq!(retry.workers, MAX_WORKERS);
    }

    #[test]
    fn total_start_failure_can_retry_and_partial_start_failure_degrades() {
        assert!(WorkerPool::start_with(|_| Err(io::Error::other("no threads"))).is_err());
        let mut started = 0;
        let pool = WorkerPool::start_with(|worker| {
            if started == 0 {
                started += 1;
                std::thread::spawn(worker);
                Ok(())
            } else {
                Err(io::Error::other("remaining threads unavailable"))
            }
        })
        .unwrap();
        assert_eq!(pool.workers, 1);
        let (tx, rx) = mpsc::channel();
        pool.submit(Box::new(|| panic!("single worker must survive")))
            .unwrap();
        pool.submit(Box::new(move || {
            tx.send(()).unwrap();
        }))
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}
