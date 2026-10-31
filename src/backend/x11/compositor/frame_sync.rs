//! Opt-in extended X11 frame feedback. X11 I/O belongs to the worker; the
//! compositor consumes immutable counter snapshots and reports successful swaps.
//! A swap timestamp is the protocol's permitted nearby presentation time, NOT
//! physical scanout or a companion CompositorPresented retirement certificate.
use crate::backend::update_notifier::AsyncUpdateNotifier;
use arc_swap::ArcSwap;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        sync::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

const LIMIT: usize = 4096;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub window: u32,
    pub generation: u64,
    pub counter_id: u32,
    pub counter: u64,
}
#[derive(Default)]
pub(super) struct Snapshot {
    pub revision: u64,
    pub frames: HashMap<u32, Frame>,
}
enum Command {
    Track(u32),
    Forget(u32),
    Swapped(Vec<Frame>, u64, u64),
}
pub(super) struct FrameSync {
    tx: Option<mpsc::SyncSender<Command>>,
    wake: AsyncUpdateNotifier,
    snapshot: Arc<ArcSwap<Snapshot>>,
    stop: Arc<AtomicBool>,
    observed: u64,
    // Worker owns its connection. Drop wakes it, but never waits on a possibly
    // frozen display server from the WM/render thread.
    _worker: std::thread::JoinHandle<()>,
}
impl FrameSync {
    pub(super) fn start(
        root: u32,
        owner: u32,
        windows: Vec<u32>,
        signal: calloop::LoopSignal,
    ) -> Option<Self> {
        if std::env::var("JWM_X11_FRAME_SYNC").as_deref() != Ok("1") {
            return None;
        }
        let wake = AsyncUpdateNotifier::new().ok()?;
        let worker_wake = wake.clone();
        let snapshot = Arc::new(ArcSwap::from_pointee(Snapshot::default()));
        let output = snapshot.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (tx, rx) = mpsc::sync_channel(256);
        let worker = std::thread::Builder::new()
            .name("jwm-x11-frame-sync".into())
            .spawn(move || {
                if let Err(error) = worker(
                    (root, owner),
                    windows,
                    rx,
                    &worker_wake,
                    &worker_stop,
                    &output,
                    &signal,
                ) {
                    log::warn!("X11 frame feedback stopped: {error}");
                }
                let revision = output.load().revision.saturating_add(1);
                output.store(Arc::new(Snapshot {
                    revision,
                    ..Default::default()
                }));
                signal.wakeup();
            })
            .ok()?;
        Some(Self {
            tx: Some(tx),
            wake,
            snapshot,
            stop,
            observed: 0,
            _worker: worker,
        })
    }
    fn send(&mut self, command: Command) {
        if self
            .tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(command).is_err())
        {
            // Lost lifecycle/submission controls cannot become success. Stop
            // advertising the capability instead of dropping a proof silently.
            self.stop.store(true, Ordering::Release);
            self.tx = None;
        }
        self.wake.notify();
    }
    pub(super) fn track(&mut self, window: u32) {
        self.send(Command::Track(window));
    }
    pub(super) fn forget(&mut self, window: u32) {
        self.send(Command::Forget(window));
    }
    pub(super) fn pending(&self) -> bool {
        self.snapshot.load().revision != self.observed
    }
    pub(super) fn needs_composition(&self, scene: &[(u32, i32, i32, u32, u32)]) -> bool {
        let snapshot = self.snapshot.load();
        scene
            .iter()
            .any(|entry| snapshot.frames.contains_key(&entry.0))
    }
    pub(super) fn take_snapshot(&mut self) -> Arc<Snapshot> {
        let snapshot = self.snapshot.load_full();
        self.observed = snapshot.revision;
        snapshot
    }
    pub(super) fn swapped(&mut self, frames: Vec<Frame>, drawn: u64, swapped: u64) {
        if !frames.is_empty() {
            self.send(Command::Swapped(frames, drawn, swapped));
        }
    }
}
impl Drop for FrameSync {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.tx = None;
        self.wake.notify();
    }
}

#[derive(Clone, Copy)]
struct Client {
    alarm: u32,
    counter_id: u32,
    generation: u64,
    value: u64,
    acknowledged: Option<u64>,
}
#[derive(Default)]
struct Tracker {
    clients: HashMap<u32, Client>,
    next_generation: u64,
    revision: u64,
}
impl Tracker {
    fn insert(&mut self, window: u32, alarm: u32, counter_id: u32, value: u64) -> Option<()> {
        if self.clients.len() >= LIMIT || self.clients.contains_key(&window) {
            return None;
        }
        self.next_generation = self.next_generation.checked_add(1)?;
        self.clients.insert(
            window,
            Client {
                alarm,
                counter_id,
                generation: self.next_generation,
                value,
                acknowledged: None,
            },
        );
        self.revision = self.revision.checked_add(1)?;
        Some(())
    }
    fn observe(&mut self, alarm: u32, value: u64) {
        if let Some(client) = self.clients.values_mut().find(|c| c.alarm == alarm) {
            if value > client.value {
                client.value = value;
                self.revision = self.revision.saturating_add(1);
            }
        }
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            revision: self.revision,
            frames: self
                .clients
                .iter()
                .filter_map(|(&window, c)| {
                    (c.value % 2 == 0 && c.acknowledged != Some(c.value)).then_some((
                        window,
                        Frame {
                            window,
                            generation: c.generation,
                            counter_id: c.counter_id,
                            counter: c.value,
                        },
                    ))
                })
                .collect(),
        }
    }
    fn accepts(&self, frame: Frame) -> bool {
        self.clients.get(&frame.window).is_some_and(|c| {
            c.generation == frame.generation
                && c.counter_id == frame.counter_id
                && frame.counter % 2 == 0
                && frame.counter <= c.value
                && c.acknowledged.is_none_or(|last| frame.counter > last)
        })
    }
    fn acknowledged(&mut self, frame: Frame) {
        if self.accepts(frame) {
            self.clients.get_mut(&frame.window).unwrap().acknowledged = Some(frame.counter);
            self.revision = self.revision.saturating_add(1);
        }
    }
    fn remove(&mut self, window: u32) -> Option<Client> {
        let client = self.clients.remove(&window)?;
        self.revision = self.revision.saturating_add(1);
        Some(client)
    }
}

pub(super) fn monotonic_micros() -> u64 {
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 || time.tv_sec < 0 {
        return 0;
    }
    (time.tv_sec as u64)
        .saturating_mul(1_000_000)
        .saturating_add(time.tv_nsec as u64 / 1000)
}
fn words(value: u64) -> [u32; 2] {
    [value as u32, (value >> 32) as u32]
}
fn messages(counter: u64, drawn: u64, swapped: u64) -> Option<[[u32; 5]; 2]> {
    if drawn == 0 || swapped < drawn || counter % 2 != 0 {
        return None;
    }
    let offset = i32::try_from(swapped - drawn).ok()?;
    let [lo, hi] = words(counter);
    let [tlo, thi] = words(drawn);
    // A zero measured interval stays unknown, as specified. Never invent a
    // nonzero presentation offset merely to satisfy a client gate.
    Some([
        [lo, hi, tlo, thi, 0],
        [lo, hi, offset as u32, 0, 0x8000_0000],
    ])
}
fn int64(value: u64) -> sync::Int64 {
    sync::Int64 {
        hi: (value >> 32) as i32,
        lo: value as u32,
    }
}
fn unsigned(value: sync::Int64) -> Option<u64> {
    (value.hi >= 0).then_some(((value.hi as u64) << 32) | value.lo as u64)
}
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
struct Atoms {
    supported: u32,
    counter: u32,
    protocols: u32,
    request: u32,
    drawn: u32,
    timings: u32,
    marker: u32,
    selection: u32,
    owner: u32,
}
impl Atoms {
    fn new(conn: &RustConnection, root: u32, owner: u32) -> Result<Self> {
        let atom =
            |name: &[u8]| -> Result<u32> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        let screen = conn
            .setup()
            .roots
            .iter()
            .position(|s| s.root == root)
            .ok_or("wrong X11 root")?;
        Ok(Self {
            supported: atom(b"_NET_SUPPORTED")?,
            counter: atom(b"_NET_WM_SYNC_REQUEST_COUNTER")?,
            protocols: atom(b"WM_PROTOCOLS")?,
            request: atom(b"_NET_WM_SYNC_REQUEST")?,
            drawn: atom(b"_NET_WM_FRAME_DRAWN")?,
            timings: atom(b"_NET_WM_FRAME_TIMINGS")?,
            marker: atom(b"_JWM_FRAME_SYNC_OWNER")?,
            selection: atom(format!("_NET_WM_CM_S{screen}").as_bytes())?,
            owner,
        })
    }
    fn advertise(&self, conn: &RustConnection, root: u32, enabled: bool) -> Result<()> {
        // These rare lifecycle transactions must not race a replacement WM.
        conn.grab_server()?.check()?;
        let result = self.advertise_locked(conn, root, enabled);
        let ungrabbed = conn.ungrab_server()?.check();
        result?;
        ungrabbed?;
        Ok(())
    }
    fn owns_compositor(&self, conn: &RustConnection) -> Result<bool> {
        Ok(conn.get_selection_owner(self.selection)?.reply()?.owner == self.owner)
    }
    fn advertise_locked(&self, conn: &RustConnection, root: u32, enabled: bool) -> Result<()> {
        let current = conn.get_selection_owner(self.selection)?.reply()?.owner;
        if enabled && current != self.owner {
            return Err("compositor authority changed".into());
        }
        let marker = conn
            .get_property(false, root, self.marker, xproto::AtomEnum::WINDOW, 0, 1)?
            .reply()?;
        let previous_owner = marker.value32().and_then(|mut v| v.next());
        if !enabled
            && (previous_owner != Some(self.owner) || (current != self.owner && current != 0))
        {
            return Ok(());
        }
        let reply = conn
            .get_property(false, root, self.supported, xproto::AtomEnum::ATOM, 0, 4096)?
            .reply()?;
        if reply.bytes_after != 0 {
            return Err("EWMH supported-property limit".into());
        }
        let previous: Vec<_> = reply.value32().into_iter().flatten().collect();
        let mut next: Vec<_> = previous
            .iter()
            .copied()
            .filter(|v| *v != self.drawn && *v != self.timings)
            .collect();
        if enabled {
            next.extend([self.drawn, self.timings]);
        }
        if next != previous {
            conn.change_property32(
                xproto::PropMode::REPLACE,
                root,
                self.supported,
                xproto::AtomEnum::ATOM,
                &next,
            )?
            .check()?;
            conn.flush()?;
        }
        if enabled {
            conn.change_property32(
                xproto::PropMode::REPLACE,
                root,
                self.marker,
                xproto::AtomEnum::WINDOW,
                &[self.owner],
            )?
            .check()?;
        } else {
            conn.delete_property(root, self.marker)?.check()?;
        }
        Ok(())
    }
}
fn track(conn: &RustConnection, atoms: &Atoms, tracker: &mut Tracker, window: u32) -> Result<()> {
    if tracker.clients.contains_key(&window) || tracker.clients.len() >= LIMIT {
        return Ok(());
    }
    conn.change_window_attributes(
        window,
        &xproto::ChangeWindowAttributesAux::new()
            .event_mask(xproto::EventMask::PROPERTY_CHANGE | xproto::EventMask::STRUCTURE_NOTIFY),
    )?
    .check()?;
    let protocols = conn
        .get_property(
            false,
            window,
            atoms.protocols,
            xproto::AtomEnum::ATOM,
            0,
            128,
        )?
        .reply()?;
    if !protocols
        .value32()
        .is_some_and(|mut p| p.any(|v| v == atoms.request))
    {
        return Ok(());
    }
    let counters = conn
        .get_property(false, window, atoms.counter, xproto::AtomEnum::ANY, 0, 2)?
        .reply()?;
    let values: Vec<_> = counters.value32().into_iter().flatten().collect();
    if counters.bytes_after != 0 || values.len() != 2 || values[1] == 0 {
        return Ok(());
    }
    let Some(value) = unsigned(conn.sync_query_counter(values[1])?.reply()?.counter_value) else {
        return Ok(());
    };
    let Some(next) = value.checked_add(1).filter(|v| *v <= i64::MAX as u64) else {
        return Ok(());
    };
    let alarm = conn.generate_id()?;
    conn.sync_create_alarm(
        alarm,
        &sync::CreateAlarmAux::new()
            .counter(values[1])
            .value_type(sync::VALUETYPE::ABSOLUTE)
            .value(int64(next))
            .test_type(sync::TESTTYPE::POSITIVE_COMPARISON)
            .delta(int64(1))
            .events(1u32),
    )?
    .check()?;
    if tracker.insert(window, alarm, values[1], value).is_none() {
        conn.sync_destroy_alarm(alarm)?.check()?;
    } else {
        log::debug!(
            "frame feedback watch window={window:#x} counter={} value={value} alarm={alarm:#x}",
            values[1]
        );
    }
    Ok(())
}
fn forget(conn: &RustConnection, tracker: &mut Tracker, window: u32) {
    if let Some(client) = tracker.remove(window) {
        log::debug!(
            "frame feedback forget window={window:#x} generation={}",
            client.generation
        );
        let _ = conn.sync_destroy_alarm(client.alarm);
    }
}

fn worker(
    authority: (u32, u32),
    windows: Vec<u32>,
    rx: mpsc::Receiver<Command>,
    wake: &AsyncUpdateNotifier,
    stop: &AtomicBool,
    output: &ArcSwap<Snapshot>,
    signal: &calloop::LoopSignal,
) -> Result<()> {
    use nix::poll::{PollFd, PollFlags, poll};
    use std::os::fd::AsFd;
    let (conn, _) = x11rb::connect(None)?;
    let (root, owner) = authority;
    if !conn.setup().roots.iter().any(|screen| screen.root == root) {
        return Err("wrong X11 root".into());
    }
    conn.sync_initialize(3, 1)?.reply()?;
    let atoms = Atoms::new(&conn, root, owner)?;
    let mut tracker = Tracker::default();
    for window in windows {
        let _ = track(&conn, &atoms, &mut tracker, window);
    }
    // This connection only observes its own window event masks. Other WM /
    // renderer connections retain their independent selections.
    conn.change_window_attributes(
        root,
        &xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE),
    )?
    .check()?;
    atoms.advertise(&conn, root, true)?;
    let result = (|| -> Result<()> {
        let mut pending_event = None;
        while !stop.load(Ordering::Acquire) {
            wake.drain()?;
            let mut exhausted = true;
            for _ in 0..512 {
                let event = match pending_event.take() {
                    Some(event) => Some(event),
                    None => conn.poll_for_event()?,
                };
                let Some(event) = event else {
                    exhausted = false;
                    break;
                };
                match event {
                    Event::SyncAlarmNotify(e) if e.response_type & 0x80 == 0 => {
                        if e.state == sync::ALARMSTATE::DESTROYED {
                            let window = tracker
                                .clients
                                .iter()
                                .find_map(|(&window, c)| (c.alarm == e.alarm).then_some(window));
                            if let Some(window) = window {
                                forget(&conn, &mut tracker, window);
                            }
                        } else if let Some(value) = unsigned(e.counter_value) {
                            log::debug!(
                                "frame feedback observe alarm={:#x} value={value}",
                                e.alarm
                            );
                            tracker.observe(e.alarm, value);
                        }
                    }
                    Event::DestroyNotify(e) => forget(&conn, &mut tracker, e.window),
                    Event::UnmapNotify(e) => forget(&conn, &mut tracker, e.window),
                    Event::PropertyNotify(e) if e.window == root && e.atom == atoms.supported => {
                        atoms.advertise(&conn, root, true)?
                    }
                    Event::PropertyNotify(e)
                        if e.atom == atoms.counter || e.atom == atoms.protocols =>
                    {
                        forget(&conn, &mut tracker, e.window);
                        let _ = track(&conn, &atoms, &mut tracker, e.window);
                    }
                    _ => {}
                }
            }
            // Drain native lifecycle events before accepting delayed render
            // controls. A full batch is not evidence that an old XID survived.
            if exhausted {
                continue;
            }
            for _ in 0..256 {
                match rx.try_recv() {
                    Ok(Command::Track(window)) => {
                        let _ = track(&conn, &atoms, &mut tracker, window);
                    }
                    Ok(Command::Forget(window)) => forget(&conn, &mut tracker, window),
                    Ok(Command::Swapped(frames, drawn, swapped)) => {
                        if stop.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        if !atoms.owns_compositor(&conn)? {
                            return Err("compositor authority retired before feedback".into());
                        }
                        for frame in frames {
                            if !tracker.accepts(frame) {
                                log::debug!("frame feedback skip stale submission {frame:?}");
                                continue;
                            }
                            let current = conn
                                .get_property(
                                    false,
                                    frame.window,
                                    atoms.counter,
                                    xproto::AtomEnum::ANY,
                                    0,
                                    2,
                                )?
                                .reply();
                            if !current.ok().is_some_and(|p| {
                                p.bytes_after == 0
                                    && p.value32().is_some_and(|mut values| {
                                        values.nth(1) == Some(frame.counter_id)
                                    })
                            }) {
                                forget(&conn, &mut tracker, frame.window);
                                continue;
                            }
                            let Some(data) = messages(frame.counter, drawn, swapped) else {
                                continue;
                            };
                            let mut delivered = true;
                            for (atom, data) in [(atoms.drawn, data[0]), (atoms.timings, data[1])] {
                                if conn
                                    .send_event(
                                        false,
                                        frame.window,
                                        xproto::EventMask::NO_EVENT,
                                        xproto::ClientMessageEvent::new(
                                            32,
                                            frame.window,
                                            atom,
                                            data,
                                        ),
                                    )?
                                    .check()
                                    .is_err()
                                {
                                    delivered = false;
                                    break;
                                }
                            }
                            if delivered {
                                log::debug!(
                                    "frame feedback submitted {frame:?} drawn={drawn} swapped={swapped}"
                                );
                                tracker.acknowledged(frame);
                            } else {
                                forget(&conn, &mut tracker, frame.window);
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            if output.load().revision != tracker.revision {
                log::debug!(
                    "frame feedback publish revision={} ready={:?}",
                    tracker.revision,
                    tracker.snapshot().frames
                );
                output.store(Arc::new(tracker.snapshot()));
                signal.wakeup();
            }
            conn.flush()?;
            // Synchronous replies above can queue native events while draining
            // the socket. Preserve the first such event before polling the fd;
            // otherwise an already buffered counter change could sleep forever.
            pending_event = conn.poll_for_event()?;
            if pending_event.is_some() {
                continue;
            }
            // No recurring timer or X request when idle. Control publication
            // and actual XSync alarms wake the worker independently.
            let mut fds = [
                PollFd::new(conn.stream().as_fd(), PollFlags::POLLIN),
                PollFd::new(wake.as_fd(), PollFlags::POLLIN),
            ];
            poll(&mut fds, nix::poll::PollTimeout::NONE)?;
        }
        Ok(())
    })();
    let _ = atoms.advertise(&conn, root, false);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_even_observed_frames_of_the_same_window_generation_are_acknowledged() {
        let mut tracker = Tracker::default();
        tracker.insert(10, 20, 30, 1).unwrap();
        assert!(tracker.snapshot().frames.is_empty());
        tracker.observe(20, 4);
        let frame = tracker.snapshot().frames[&10];
        assert!(tracker.accepts(frame));
        assert!(!tracker.accepts(Frame {
            counter: 8,
            ..frame
        }));
        tracker.acknowledged(frame);
        assert!(!tracker.accepts(frame));
        tracker.remove(10);
        tracker.insert(10, 21, 31, 4).unwrap();
        assert!(!tracker.accepts(frame));
        tracker.observe(20, 12);
        assert_eq!(tracker.snapshot().frames[&10].counter, 4);
    }
    #[test]
    fn zero_unknown_and_failed_swap_timings_cannot_be_invented() {
        assert!(messages(4, 0, 20).is_none());
        assert!(messages(4, 20, 19).is_none());
        assert!(messages(5, 20, 30).is_none());
        assert!(messages(4, 1, i32::MAX as u64 + 2).is_none());
        assert_eq!(messages(4, 10, 10).unwrap()[1][2], 0);
        let messages = messages((3u64 << 32) | 4, (5u64 << 32) | 1, (5u64 << 32) | 33).unwrap();
        assert_eq!(messages[0], [4, 3, 1, 5, 0]);
        assert_eq!(messages[1], [4, 3, 32, 0, 0x8000_0000]);
    }
}
