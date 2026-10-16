//! XI2.4 swipe input shared by both X11 backends.
//!
//! The separate connection is intentional: rust-xcb's protocol definitions do
//! not include XI2.4. This is an in-process X client, not an input-device reader.
//! A synchronous swipe-only passive grab lets us replay unconfigured finger
//! counts intact. Never do user work between a Begin and AllowEvents + flush.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{
    self, ConnectionExt as _, EventMode, GrabMode22, GrabOwner, GrabType,
};
use x11rb::protocol::{Event, xproto};
use x11rb::rust_connection::RustConnection;

use crate::backend::api::BackendEvent;
use crate::backend::gesture_status::{RuntimeStatus, change_status};
use crate::backend::gesture_swipe::GestureSwipeTracker;
#[cfg(test)]
use crate::config::gesture_swipe_signature as config_signature;
use crate::config::{CONFIG, GestureSwipeConfig};

const ALL_DEVICES: u16 = 0;
const ALL_MASTER_DEVICES: u16 = 1;
// XI2 event masks are arrays of 32-bit words; End is bit 32, not bit 0
// in the first word. XIEventMask has no named gesture constants.
const SWIPE_MASK: [u32; 2] = [(1 << 30) | (1 << 31), 1];
const ANY_MODIFIER: u32 = 1 << 31;
const POLL_BUDGET: usize = 128;

type GestureResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct ActiveSwipe {
    source: u16,
    begin_time: u32,
    config_signature: String,
    tracker: GestureSwipeTracker,
}

/// Insert once into the backend's calloop loop, forwarding each BackendEvent
/// to its normal handler. Configuration changes are applied before sleeping;
/// disabled bindings retain the socket, so enabling again needs no new source.
pub(crate) struct GestureEventSource {
    conn: RustConnection,
    root: u32,
    grabbed: bool,
    config_signature: String,
    active: HashMap<u16, ActiveSwipe>,
    pending: VecDeque<BackendEvent>,
    wake_token: Option<Token>,
    failed: bool,
    gesture_devices: usize,
    devices_dirty: bool,
}

impl GestureEventSource {
    /// A missing XI2.4 server or conflicting grab is a nonfatal feature failure:
    /// the caller should log this error and continue running its normal backend.
    pub(crate) fn new() -> Result<Self, String> {
        change_status(|status| {
            *status = RuntimeStatus {
                state: "connecting".into(),
                ..RuntimeStatus::default()
            }
        });
        Self::connect().map_err(|error| {
            let reason = error.to_string();
            change_status(|status| {
                status.active = false;
                status.state = "unavailable".into();
                status.reason = Some(reason.clone());
            });
            reason
        })
    }

    fn connect() -> GestureResult<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let version = conn.xinput_xi_query_version(2, 4)?.reply()?;
        change_status(|status| {
            status.server_version = Some((version.major_version, version.minor_version));
            status.available = (version.major_version, version.minor_version) >= (2, 4);
        });
        if (version.major_version, version.minor_version) < (2, 4) {
            return Err(
                io::Error::other("XInput 2.4 is unavailable; touchpad swipes disabled").into(),
            );
        }
        // Diagnostics only: keep the AllMasterDevices grab available for
        // hotplug even when no capable physical touchpad is currently present.
        let devices = conn.xinput_xi_query_device(ALL_DEVICES)?.reply()?;
        let capable = devices
            .infos
            .iter()
            .filter(|device| {
                device.enabled
                    && device.type_ == xinput::DeviceType::SLAVE_POINTER
                    && device
                        .classes
                        .iter()
                        .any(|class| matches!(&class.data, xinput::DeviceClassData::Gesture(_)))
            })
            .count();
        change_status(|status| status.gesture_devices = capable);
        if capable == 0 {
            log::info!(
                "XInput 2.4 is available, but no gesture-capable touchpad is currently exposed; waiting for hotplug"
            );
        } else {
            log::info!("XInput 2.4 swipe input available on {capable} gesture-capable device(s)");
        }
        conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: ALL_DEVICES,
                mask: vec![xinput::XIEventMask::HIERARCHY | xinput::XIEventMask::DEVICE_CHANGED],
            }],
        )?
        .check()?;
        let mut source = Self {
            conn,
            root,
            grabbed: false,
            config_signature: String::new(),
            active: HashMap::new(),
            pending: VecDeque::new(),
            wake_token: None,
            failed: false,
            gesture_devices: capable,
            devices_dirty: false,
        };
        source.sync_config()?;
        Ok(source)
    }

    fn sync_config(&mut self) -> GestureResult<()> {
        let config = CONFIG.load();
        let counts = configured_counts(&config.behavior().gesture_swipe);
        let signature = config.gesture_swipe_signature();
        if self.config_signature == signature {
            return Ok(());
        }
        self.config_signature = signature;
        self.active.clear();
        self.pending.clear();
        let enabled = counts.iter().any(|enabled| *enabled);
        if enabled && !self.grabbed {
            let reply = self
                .conn
                .xinput_xi_passive_grab_device(
                    x11rb::CURRENT_TIME,
                    self.root,
                    x11rb::NONE,
                    0,
                    ALL_MASTER_DEVICES,
                    GrabType::GESTURE_SWIPE_BEGIN,
                    GrabMode22::SYNC,
                    xproto::GrabMode::ASYNC,
                    GrabOwner::NO_OWNER,
                    &SWIPE_MASK,
                    &[ANY_MODIFIER],
                )?
                .reply()?;
            if !reply.modifiers.is_empty() {
                return Err(
                    io::Error::other("XInput swipe grab conflicts with another client").into(),
                );
            }
            self.grabbed = true;
        } else if !enabled && self.grabbed {
            self.conn
                .xinput_xi_passive_ungrab_device(
                    self.root,
                    0,
                    ALL_MASTER_DEVICES,
                    GrabType::GESTURE_SWIPE_BEGIN,
                    &[ANY_MODIFIER],
                )?
                .check()?;
            // Removing a passive grab does not release an already active one.
            // Include masters with a Begin queued but not yet seen by us.
            let devices = self
                .conn
                .xinput_xi_query_device(ALL_MASTER_DEVICES)?
                .reply()?;
            for device in devices.infos {
                if device.type_ == xinput::DeviceType::MASTER_POINTER {
                    self.conn
                        .xinput_xi_ungrab_device(x11rb::CURRENT_TIME, device.deviceid)?
                        .check()?;
                }
            }
            self.grabbed = false;
        }
        self.conn.flush()?;
        self.publish_status();
        Ok(())
    }

    fn publish_status(&self) {
        change_status(|status| {
            status.gesture_devices = self.gesture_devices;
            status.active = self.grabbed && !self.failed;
            status.state = if self.failed {
                "failed"
            } else if !self.grabbed {
                "disabled"
            } else if self.gesture_devices == 0 {
                "awaiting_device"
            } else {
                "ready"
            }
            .into();
            status.reason = if self.failed {
                status.reason.take()
            } else if !self.grabbed {
                Some("No valid swipe bindings configured".into())
            } else if self.gesture_devices == 0 {
                Some("No gesture-capable touchpad exposed by the X server".into())
            } else {
                None
            };
        });
    }

    fn refresh_devices(&mut self) -> GestureResult<()> {
        if !self.devices_dirty {
            return Ok(());
        }
        let devices = self.conn.xinput_xi_query_device(ALL_DEVICES)?.reply()?;
        self.gesture_devices = devices
            .infos
            .iter()
            .filter(|device| {
                device.enabled
                    && device.type_ == xinput::DeviceType::SLAVE_POINTER
                    && device
                        .classes
                        .iter()
                        .any(|class| matches!(&class.data, xinput::DeviceClassData::Gesture(_)))
            })
            .count();
        self.devices_dirty = false;
        self.publish_status();
        Ok(())
    }

    fn map_event(&mut self, event: Event) -> GestureResult<Option<BackendEvent>> {
        match event {
            Event::XinputGestureSwipeBegin(event) => {
                if event.response_type & 0x80 != 0 {
                    return Ok(None);
                }
                // Snapshot only immutable, local configuration before thawing.
                let config = CONFIG.load();
                let counts = configured_counts(&config.behavior().gesture_swipe);
                let claim = self.grabbed && accepts_count(&counts, event.detail);
                let mode = if claim {
                    EventMode::ASYNC_DEVICE
                } else {
                    EventMode::REPLAY_DEVICE
                };
                let allow = self.conn.xinput_xi_allow_events(
                    event.time,
                    event.deviceid,
                    mode,
                    0,
                    self.root,
                )?;
                // Flush before checking errors or allocating a tracker. In
                // particular, no callback or config mutation runs while frozen.
                self.conn.flush()?;
                allow.check()?;
                if claim {
                    if self.active.get(&event.deviceid).is_some_and(|active| {
                        active.source == event.sourceid && active.begin_time == event.time
                    }) {
                        return Ok(None);
                    }
                    let mut tracker = GestureSwipeTracker::default();
                    tracker.begin(event.detail, true);
                    self.active.insert(
                        event.deviceid,
                        ActiveSwipe {
                            source: event.sourceid,
                            begin_time: event.time,
                            config_signature: config.gesture_swipe_signature(),
                            tracker,
                        },
                    );
                } else {
                    self.active.remove(&event.deviceid);
                }
            }
            Event::XinputGestureSwipeUpdate(event) => {
                if event.response_type & 0x80 != 0 {
                    return Ok(None);
                }
                if let Some(active) = self.active.get_mut(&event.deviceid)
                    && active.source == event.sourceid
                    && active.tracker.fingers == event.detail
                {
                    active
                        .tracker
                        .update(fixed_to_f64(event.delta_x), fixed_to_f64(event.delta_y));
                }
            }
            Event::XinputGestureSwipeEnd(event) => {
                if event.response_type & 0x80 != 0 {
                    return Ok(None);
                }
                // A replayed Begin produces an End but owns no tracker.
                if self.active.get(&event.deviceid).is_some_and(|active| {
                    active.source == event.sourceid && active.tracker.fingers == event.detail
                }) && let Some(mut active) = self.active.remove(&event.deviceid)
                {
                    let config = CONFIG.load();
                    let behavior = config.behavior();
                    let cancelled = u32::from(event.flags) & 1 != 0
                        || active.config_signature != config.gesture_swipe_signature();
                    if let Some((fingers, direction)) = active
                        .tracker
                        .finish(cancelled, behavior.gesture_swipe_threshold)
                        && behavior.gesture_swipe.iter().any(|binding| {
                            binding.fingers == fingers
                                && binding.direction.eq_ignore_ascii_case(direction)
                        })
                    {
                        return Ok(Some(BackendEvent::GestureSwipeAction {
                            fingers,
                            direction,
                            config_signature: active.config_signature.clone(),
                        }));
                    }
                }
            }
            // Device removal, reassignment and capability changes invalidate
            // an in-progress sequence. A subsequent Begin establishes new state.
            Event::XinputHierarchy(_) | Event::XinputDeviceChanged(_) => {
                self.active.clear();
                self.devices_dirty = true;
            }
            Event::Error(error) => {
                return Err(
                    io::Error::other(format!("XInput swipe protocol error: {error:?}")).into(),
                );
            }
            _ => {}
        }
        Ok(None)
    }

    /// Bounded draining also handles events already buffered during reply reads.
    /// True means dispatch is needed (pending output or fairness continuation).
    fn prefetch(&mut self) -> GestureResult<bool> {
        if !self.pending.is_empty() {
            return Ok(true);
        }
        for _ in 0..POLL_BUDGET {
            let Some(event) = self.conn.poll_for_event()? else {
                if self.devices_dirty {
                    self.refresh_devices()?;
                    // The reply read can buffer new events and empty the fd.
                    // Recheck here before allowing the event loop to sleep.
                    continue;
                }
                return Ok(false);
            };
            if let Some(event) = self.map_event(event)? {
                self.pending.push_back(event);
                return Ok(true);
            }
        }
        Ok(true)
    }

    fn fail_open(&mut self, error: impl std::fmt::Display) {
        if self.failed {
            return;
        }
        self.failed = true;
        self.active.clear();
        self.pending.clear();
        // Disconnect the server immediately to release a possibly frozen grab,
        // but retain ownership of the fd until calloop has unregistered it.
        // SAFETY: the connection owns this live socket throughout this call.
        unsafe {
            libc::shutdown(self.conn.stream().as_raw_fd(), libc::SHUT_RDWR);
        }
        change_status(|status| {
            status.active = false;
            status.state = "failed".into();
            status.reason = Some(error.to_string());
        });
        log::warn!("Disabling X11 touchpad swipe input: {error}");
    }
}

impl Drop for GestureEventSource {
    fn drop(&mut self) {
        // Release server-side grabs before acquiring a status lock, including
        // when construction or source registration failed partway through.
        unsafe {
            libc::shutdown(self.conn.stream().as_raw_fd(), libc::SHUT_RDWR);
        }
        change_status(|status| {
            status.active = false;
            if !self.failed {
                status.state = "stopped".into();
                status.reason = Some("X11 gesture input source is not running".into());
            }
        });
    }
}

fn configured_counts(bindings: &[GestureSwipeConfig]) -> [bool; 6] {
    let mut counts = [false; 6];
    for binding in bindings {
        if crate::config::gesture_swipe_binding_is_valid(binding) {
            counts[binding.fingers as usize] = true;
        }
    }
    counts
}

fn accepts_count(counts: &[bool; 6], fingers: u32) -> bool {
    counts.get(fingers as usize).copied().unwrap_or(false)
}

fn fixed_to_f64(value: i32) -> f64 {
    f64::from(value) / 65536.0
}

impl EventSource for GestureEventSource {
    type Event = BackendEvent;
    type Metadata = ();
    type Ret = ();
    type Error = io::Error;
    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn process_events<F>(
        &mut self,
        _readiness: Readiness,
        _token: Token,
        mut callback: F,
    ) -> Result<PostAction, Self::Error>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        if self.failed {
            return Ok(PostAction::Remove);
        }
        if let Err(error) = self.sync_config() {
            self.fail_open(error);
            return Ok(PostAction::Remove);
        }
        for _ in 0..POLL_BUDGET {
            if let Some(event) = self.pending.pop_front() {
                callback(event, &mut ());
                continue;
            }
            match self.conn.poll_for_event() {
                Ok(Some(event)) => match self.map_event(event) {
                    Ok(Some(event)) => callback(event, &mut ()),
                    Ok(None) => {}
                    Err(error) => {
                        self.fail_open(error);
                        return Ok(PostAction::Remove);
                    }
                },
                Ok(None) => {
                    if let Err(error) = self.refresh_devices() {
                        self.fail_open(error);
                        return Ok(PostAction::Remove);
                    }
                    break;
                }
                Err(error) => {
                    self.fail_open(error);
                    return Ok(PostAction::Remove);
                }
            }
        }
        Ok(PostAction::Continue)
    }

    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        let wake = if self.failed {
            true
        } else {
            match self.sync_config().and_then(|()| self.prefetch()) {
                Ok(wake) => wake,
                Err(error) => {
                    self.fail_open(error);
                    true
                }
            }
        };
        if wake {
            Ok(Some((
                Readiness::EMPTY,
                self.wake_token.ok_or(calloop::Error::InvalidToken)?,
            )))
        } else {
            Ok(None)
        }
    }

    fn register(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        let token = token_factory.token();
        self.wake_token = Some(token);
        // SAFETY: self.conn owns the socket until after unregister/source drop.
        let fd = unsafe { BorrowedFd::borrow_raw(self.conn.stream().as_raw_fd()) };
        unsafe { poll.register(fd, Interest::READ, Mode::Level, token) }
    }

    fn reregister(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        let token = token_factory.token();
        self.wake_token = Some(token);
        let fd = unsafe { BorrowedFd::borrow_raw(self.conn.stream().as_raw_fd()) };
        poll.reregister(fd, Interest::READ, Mode::Level, token)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        let fd = unsafe { BorrowedFd::borrow_raw(self.conn.stream().as_raw_fd()) };
        poll.unregister(fd)?;
        self.wake_token = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ArgumentConfig;

    fn binding(fingers: u32, direction: &str) -> GestureSwipeConfig {
        GestureSwipeConfig {
            fingers,
            direction: direction.into(),
            function: "loopview".into(),
            argument: ArgumentConfig::Int(1),
        }
    }

    #[test]
    fn only_explicit_valid_finger_counts_are_claimed() {
        let counts = configured_counts(&[
            binding(3, "left"),
            binding(5, "up"),
            binding(2, "left"),
            binding(4, "diagonal"),
        ]);
        for fingers in [0, 1, 2, 4, 6, u32::MAX] {
            assert!(!accepts_count(&counts, fingers));
        }
        assert!(accepts_count(&counts, 3));
        assert!(accepts_count(&counts, 5));
        assert!(accepts_count(&configured_counts(&[binding(4, "Left")]), 4));
        assert!(!configured_counts(&[]).into_iter().any(|value| value));
    }

    #[test]
    fn configuration_signature_tracks_commands_arguments_and_threshold() {
        let original = binding(3, "left");
        let signature = config_signature(std::slice::from_ref(&original), 80.0);
        let mut changed = original.clone();
        changed.function = "view".into();
        assert_ne!(signature, config_signature(&[changed], 80.0));
        let mut changed = original.clone();
        changed.argument = ArgumentConfig::Int(-1);
        assert_ne!(signature, config_signature(&[changed], 80.0));
        assert_ne!(signature, config_signature(&[original], 90.0));
        assert_ne!(signature, config_signature(&[], 80.0));
    }

    #[test]
    fn mask_includes_end_in_second_word_and_fixed_point_is_exact() {
        for event in [30, 31, 32] {
            assert_ne!(SWIPE_MASK[event / 32] & (1 << (event % 32)), 0);
        }
        assert_eq!(fixed_to_f64(65536), 1.0);
        assert_eq!(fixed_to_f64(-32768), -0.5);
    }
}
