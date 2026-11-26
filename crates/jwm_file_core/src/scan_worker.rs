use crate::{Cancellation, DirectorySnapshot, FileProvider, ListError, ListOptions, Location};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanId(u64);

pub struct ScanReply {
    pub id: ScanId,
    pub result: Result<DirectorySnapshot, ListError>,
}

struct Request {
    id: ScanId,
    location: Location,
    options: ListOptions,
    cancel: Cancellation,
}

#[derive(Default)]
struct State {
    latest: u64,
    pending: Option<Request>,
    active: Option<Cancellation>,
    ready: Option<ScanReply>,
    stopping: bool,
}

/// Bounded dispatcher for a desktop browsing session: one scan in flight,
/// one replaceable pending request and one latest result. No filesystem work
/// runs in submit or take_reply.
pub struct ScanWorker {
    shared: Arc<(Mutex<State>, Condvar)>,
}

impl ScanWorker {
    pub fn start(provider: impl FileProvider + 'static) -> io::Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let inner = Arc::clone(&shared);
        thread::Builder::new()
            .name("jwm-file-scan".into())
            .spawn(move || run(&inner, provider))?;
        Ok(Self { shared })
    }

    pub fn submit(&self, location: Location, options: ListOptions) -> ScanId {
        let (state, wake) = &*self.shared;
        let mut state = state.lock().expect("scan mailbox poisoned");
        state.latest = state
            .latest
            .checked_add(1)
            .expect("scan generation exhausted");
        let id = ScanId(state.latest);
        if let Some(active) = &state.active {
            active.cancel();
        }
        state.ready = None;
        state.pending = Some(Request {
            id,
            location,
            options,
            cancel: Cancellation::default(),
        });
        wake.notify_one();
        id
    }

    pub fn take_reply(&self) -> Option<ScanReply> {
        self.shared
            .0
            .lock()
            .expect("scan mailbox poisoned")
            .ready
            .take()
    }
}

impl Drop for ScanWorker {
    fn drop(&mut self) {
        let (state, wake) = &*self.shared;
        let mut state = state.lock().expect("scan mailbox poisoned");
        state.stopping = true;
        state.pending = None;
        if let Some(active) = &state.active {
            active.cancel();
        }
        // Do not join: an OS call in a dead mount may not respond to cancellation.
        wake.notify_one();
    }
}

fn run(shared: &(Mutex<State>, Condvar), provider: impl FileProvider) {
    let (mailbox, wake) = shared;
    loop {
        let request = {
            let state = mailbox.lock().expect("scan mailbox poisoned");
            let mut state = wake
                .wait_while(state, |s| s.pending.is_none() && !s.stopping)
                .expect("scan mailbox poisoned");
            if state.stopping {
                return;
            }
            let request = state.pending.take().expect("a pending scan must exist");
            state.active = Some(request.cancel.clone());
            request
        };
        let result = provider.list(&request.location, request.options, &request.cancel);
        let mut state = mailbox.lock().expect("scan mailbox poisoned");
        state.active = None;
        if !state.stopping && request.id.0 == state.latest {
            state.ready = Some(ScanReply {
                id: request.id,
                result,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::{Duration, Instant};

    struct Controlled {
        started: Sender<Location>,
        unblock_first: Mutex<Receiver<()>>,
    }

    impl FileProvider for Controlled {
        fn list(
            &self,
            location: &Location,
            _: ListOptions,
            _: &Cancellation,
        ) -> Result<DirectorySnapshot, ListError> {
            self.started.send(location.clone()).unwrap();
            if *location == Location::Local("first".into()) {
                self.unblock_first
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
            }
            // Deliberately ignores cancellation, as a blocking filesystem can.
            Ok(DirectorySnapshot {
                location: location.clone(),
                entries: Vec::new(),
                skipped_entries: 0,
            })
        }
    }

    #[test]
    fn superseded_running_and_queued_requests_never_reach_the_view() {
        let (started, watching) = mpsc::channel();
        let (release, unblock_first) = mpsc::channel();
        let worker = ScanWorker::start(Controlled {
            started,
            unblock_first: Mutex::new(unblock_first),
        })
        .unwrap();
        let first = Location::Local("first".into());
        worker.submit(first.clone(), ListOptions::default());
        assert_eq!(
            watching.recv_timeout(Duration::from_secs(3)).unwrap(),
            first
        );
        for n in 0..100 {
            worker.submit(
                Location::Local(format!("skipped-{n}").into()),
                ListOptions::default(),
            );
        }
        let last = Location::Local("last".into());
        let id = worker.submit(last.clone(), ListOptions::default());
        release.send(()).unwrap();
        assert_eq!(watching.recv_timeout(Duration::from_secs(3)).unwrap(), last);
        let deadline = Instant::now() + Duration::from_secs(3);
        let reply = loop {
            if let Some(reply) = worker.take_reply() {
                break reply;
            }
            assert!(Instant::now() < deadline, "latest scan never completed");
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(reply.id, id);
        assert_eq!(reply.result.unwrap().location, last);
        assert!(worker.take_reply().is_none());
    }
}
