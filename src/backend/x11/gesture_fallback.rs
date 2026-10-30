//! Gesture-only libinput access for X servers whose input driver omits XI2.4.
//!
//! Only seat-local touchpad event nodes can be opened. Xorg keeps pointer,
//! buttons and scrolling; this context never grabs a device or injects them.

use crate::{
    backend::gesture_swipe::{SwipeInput, next_swipe_id},
    config::CONFIG,
};
use input::{
    Event, Libinput, LibinputInterface,
    event::{
        EventTrait,
        gesture::{
            GestureEndEvent, GestureEvent, GestureEventCoordinates, GestureEventTrait,
            GestureSwipeEvent,
        },
    },
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::{CStr, CString, OsStr},
    fs::OpenOptions,
    io,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

type SessionActive = unsafe extern "C" fn(*const libc::c_char) -> libc::c_int;
type MonitorFlush = unsafe extern "C" fn(*mut libc::c_void) -> libc::c_int;
type MonitorUnref = unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void;

/// Use logind's public API and wake fd, never its private state-file format.
struct Session {
    _library: libloading::Library,
    id: CString,
    seat: String,
    monitor: *mut libc::c_void,
    fd: RawFd,
    active: SessionActive,
    flush: MonitorFlush,
    unref: MonitorUnref,
}

impl Session {
    fn new() -> Result<Self, String> {
        // The library remains owned until after the monitor is destroyed.
        unsafe {
            let library = libloading::Library::new("libsystemd.so.0").map_err(|e| e.to_string())?;
            let pid_session = *library
                .get::<unsafe extern "C" fn(libc::pid_t, *mut *mut libc::c_char) -> libc::c_int>(
                    b"sd_pid_get_session\0",
                )
                .map_err(|e| e.to_string())?;
            let get_seat = *library.get::<unsafe extern "C" fn(*const libc::c_char, *mut *mut libc::c_char) -> libc::c_int>(b"sd_session_get_seat\0").map_err(|e| e.to_string())?;
            let get_type = *library.get::<unsafe extern "C" fn(*const libc::c_char, *mut *mut libc::c_char) -> libc::c_int>(b"sd_session_get_type\0").map_err(|e| e.to_string())?;
            let active = *library
                .get::<SessionActive>(b"sd_session_is_active\0")
                .map_err(|e| e.to_string())?;
            let new_monitor = *library.get::<unsafe extern "C" fn(*const libc::c_char, *mut *mut libc::c_void) -> libc::c_int>(b"sd_login_monitor_new\0").map_err(|e| e.to_string())?;
            let get_fd = *library
                .get::<unsafe extern "C" fn(*mut libc::c_void) -> libc::c_int>(
                    b"sd_login_monitor_get_fd\0",
                )
                .map_err(|e| e.to_string())?;
            let flush = *library
                .get::<MonitorFlush>(b"sd_login_monitor_flush\0")
                .map_err(|e| e.to_string())?;
            let unref = *library
                .get::<MonitorUnref>(b"sd_login_monitor_unref\0")
                .map_err(|e| e.to_string())?;
            let mut value = std::ptr::null_mut();
            if pid_session(0, &mut value) < 0 || value.is_null() {
                return Err("No local logind session for touchpad gesture fallback".into());
            }
            let id = CStr::from_ptr(value).to_owned();
            libc::free(value.cast());
            value = std::ptr::null_mut();
            if get_type(id.as_ptr(), &mut value) < 0 || value.is_null() {
                return Err("Cannot identify the local X11 login session".into());
            }
            let x11_session = CStr::from_ptr(value) == c"x11";
            libc::free(value.cast());
            value = std::ptr::null_mut();
            if !x11_session {
                return Err(
                    "The host compositor owns touchpad gestures outside a local X11 login".into(),
                );
            }
            if get_seat(id.as_ptr(), &mut value) < 0 || value.is_null() {
                return Err("Gesture fallback requires a local seat".into());
            }
            let seat = CStr::from_ptr(value).to_string_lossy().into_owned();
            libc::free(value.cast());
            let mut monitor = std::ptr::null_mut();
            if new_monitor(c"session".as_ptr(), &mut monitor) < 0 || monitor.is_null() {
                return Err("Cannot watch login session activity".into());
            }
            let fd = get_fd(monitor);
            if fd < 0 {
                unref(monitor);
                return Err("Cannot watch login session descriptor".into());
            }
            Ok(Self {
                _library: library,
                id,
                seat,
                monitor,
                fd,
                active,
                flush,
                unref,
            })
        }
    }

    fn is_active(&self) -> bool {
        unsafe { (self.active)(self.id.as_ptr()) > 0 }
    }

    fn flush(&self) {
        unsafe {
            (self.flush)(self.monitor);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            (self.unref)(self.monitor);
        }
    }
}

fn allowed_touchpad(device: &udev::Device, seat: &str) -> bool {
    access_policy(
        device.property_value("ID_INPUT_TOUCHPAD"),
        device.property_value("ID_INPUT_KEYBOARD"),
        device.property_value("ID_SEAT"),
        device.sysname(),
        seat,
    )
}

fn access_policy(
    touchpad: Option<&OsStr>,
    keyboard: Option<&OsStr>,
    device_seat: Option<&OsStr>,
    name: &OsStr,
    seat: &str,
) -> bool {
    touchpad == Some(OsStr::new("1"))
        && keyboard != Some(OsStr::new("1"))
        && device_seat.unwrap_or(OsStr::new("seat0")) == OsStr::new(seat)
        && name.to_str().is_some_and(|name| {
            name.strip_prefix("event").is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
            })
        })
}

struct TouchpadOnly {
    seat: String,
}
impl LibinputInterface for TouchpadOnly {
    fn open_restricted(&mut self, path: &Path, _flags: i32) -> Result<OwnedFd, i32> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| error.raw_os_error().unwrap_or(libc::EACCES))?;
        let metadata = file.metadata().map_err(|_| libc::EACCES)?;
        let device = udev::Device::from_devnum(udev::DeviceType::Character, metadata.rdev())
            .map_err(|_| libc::EACCES)?;
        if !allowed_touchpad(&device, &self.seat) {
            return Err(libc::EACCES);
        }
        Ok(file.into())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(fd);
    }
}

struct Swipe {
    id: u64,
    fingers: u32,
    signature: String,
}

pub(super) struct GestureFallback {
    context: Libinput,
    monitor: udev::MonitorSocket,
    session: Session,
    devices: HashMap<PathBuf, input::Device>,
    swipes: HashMap<String, Swipe>,
    pending: VecDeque<SwipeInput>,
    native: HashSet<String>,
    disabled: HashSet<String>,
    enabled: bool,
    awake: bool,
    dirty: bool,
    pub(super) denied: usize,
    pub(super) candidates: usize,
    pub(super) needs_wake: bool,
}

impl GestureFallback {
    pub(super) fn new() -> Result<Self, String> {
        let session = Session::new()?;
        let context = Libinput::new_from_path(TouchpadOnly {
            seat: session.seat.clone(),
        });
        let monitor = udev::MonitorBuilder::new()
            .and_then(|builder| builder.match_subsystem("input"))
            .and_then(|builder| builder.listen())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            context,
            monitor,
            session,
            devices: HashMap::new(),
            swipes: HashMap::new(),
            pending: VecDeque::new(),
            native: HashSet::new(),
            disabled: HashSet::new(),
            enabled: false,
            awake: false,
            dirty: true,
            denied: 0,
            candidates: 0,
            needs_wake: false,
        })
    }

    pub(super) fn fds(&self) -> [RawFd; 3] {
        [
            self.context.as_raw_fd(),
            self.monitor.as_raw_fd(),
            self.session.fd,
        ]
    }

    pub(super) fn device_count(&self) -> usize {
        self.devices.len()
    }
    pub(super) fn session_active(&self) -> bool {
        self.awake
    }

    pub(super) fn stop(&mut self) -> VecDeque<SwipeInput> {
        self.remove_devices();
        self.awake = false;
        std::mem::take(&mut self.pending)
    }

    pub(super) fn set_native_devices(
        &mut self,
        native: HashSet<String>,
        disabled: HashSet<String>,
    ) {
        if self.native != native || self.disabled != disabled {
            self.native = native;
            self.disabled = disabled;
            self.dirty = true;
        }
    }

    pub(super) fn cancel(&mut self) {
        for (_, swipe) in self.swipes.drain() {
            self.pending.push_back(SwipeInput::End {
                id: swipe.id,
                cancelled: true,
                time: 0,
            });
        }
    }

    pub(super) fn reset(&mut self) {
        self.remove_devices();
        self.dirty = true;
    }

    fn remove_devices(&mut self) {
        self.cancel();
        for (_, device) in self.devices.drain() {
            self.context.path_remove_device(device);
        }
    }

    fn rescan(&mut self) -> io::Result<()> {
        self.dirty = false;
        self.denied = 0;
        self.candidates = 0;
        let mut enumerator = udev::Enumerator::new()?;
        enumerator.match_subsystem("input")?;
        enumerator.match_property("ID_INPUT_TOUCHPAD", "1")?;
        let mut wanted = HashSet::new();
        for device in enumerator.scan_devices()?.take(64) {
            if !allowed_touchpad(&device, &self.session.seat) {
                continue;
            }
            let Some(path) = device.devnode() else {
                continue;
            };
            let name = device
                .parent()
                .and_then(|parent| parent.attribute_value("name").map(OsStr::to_owned))
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if self.native.contains(&name) || self.disabled.contains(&name) {
                continue;
            }
            self.candidates += 1;
            wanted.insert(path.to_owned());
            if self.devices.contains_key(path) {
                continue;
            }
            if let Some(pad) = self.context.path_add_device(&path.to_string_lossy()) {
                if pad.has_capability(input::DeviceCapability::Gesture) {
                    self.devices.insert(path.to_owned(), pad);
                } else {
                    self.context.path_remove_device(pad);
                }
            } else {
                self.denied += 1;
            }
        }
        let removed: Vec<_> = self
            .devices
            .keys()
            .filter(|path| !wanted.contains(*path))
            .cloned()
            .collect();
        if !removed.is_empty() {
            self.cancel();
        }
        for path in removed {
            if let Some(device) = self.devices.remove(&path) {
                self.context.path_remove_device(device);
            }
        }
        Ok(())
    }

    pub(super) fn poll(&mut self, enabled: bool) -> io::Result<VecDeque<SwipeInput>> {
        self.session.flush();
        if self.monitor.iter().take(128).count() > 0 {
            self.dirty = true;
        }
        let awake = enabled && self.session.is_active();
        if awake != self.awake || enabled != self.enabled {
            self.remove_devices();
            self.awake = awake;
            self.enabled = enabled;
            self.dirty = true;
        }
        if self.dirty && self.awake {
            self.rescan()?;
        }
        self.context.dispatch()?;
        self.needs_wake = false;
        for count in 0..128 {
            let Some(event) = self.context.next() else {
                break;
            };
            self.needs_wake = count == 127;
            let Event::Gesture(GestureEvent::Swipe(event)) = event else {
                continue;
            };
            let device = event.device().sysname().to_owned();
            // Removed/reopened devices can still have buffered events. Their
            // old Begin must never acquire the new policy or session owner.
            if !self
                .devices
                .values()
                .any(|current| *current == event.device())
            {
                continue;
            }
            let cfg = CONFIG.load();
            let time = event.time();
            match event {
                GestureSwipeEvent::Begin(event) => {
                    let fingers = event.finger_count() as u32;
                    if !self.awake
                        || !cfg.behavior().gesture_swipe.iter().any(|binding| {
                            binding.fingers == fingers
                                && crate::config::gesture_swipe_binding_is_valid(binding)
                        })
                    {
                        continue;
                    }
                    if let Some(old) = self.swipes.remove(&device) {
                        self.pending.push_back(SwipeInput::End {
                            id: old.id,
                            cancelled: true,
                            time,
                        });
                    }
                    let id = next_swipe_id();
                    let signature = cfg.gesture_swipe_signature();
                    self.swipes.insert(
                        device,
                        Swipe {
                            id,
                            fingers,
                            signature: signature.clone(),
                        },
                    );
                    self.pending.push_back(SwipeInput::Begin {
                        id,
                        fingers,
                        config_signature: signature,
                    });
                }
                GestureSwipeEvent::Update(event) => {
                    if let Some(swipe) = self.swipes.get(&device) {
                        self.pending.push_back(SwipeInput::Update {
                            id: swipe.id,
                            dx: event.dx(),
                            dy: event.dy(),
                            time,
                        });
                    }
                }
                GestureSwipeEvent::End(event) => {
                    if let Some(swipe) = self.swipes.remove(&device) {
                        let cancelled = event.cancelled()
                            || !self.awake
                            || swipe.fingers != event.finger_count() as u32
                            || swipe.signature != cfg.gesture_swipe_signature();
                        self.pending.push_back(SwipeInput::End {
                            id: swipe.id,
                            cancelled,
                            time,
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(std::mem::take(&mut self.pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_access_is_limited_to_touchpad_events_on_its_own_seat() {
        let one = Some(OsStr::new("1"));
        let event = OsStr::new("event4");
        assert!(access_policy(one, None, None, event, "seat0"));
        assert!(!access_policy(None, None, None, event, "seat0"));
        assert!(!access_policy(one, one, None, event, "seat0"));
        assert!(!access_policy(one, None, None, event, "seat1"));
        assert!(access_policy(
            one,
            None,
            Some(OsStr::new("seat1")),
            event,
            "seat1"
        ));
        for name in ["mouse0", "event", "event4/../event5", "event4-keyboard"] {
            assert!(!access_policy(one, None, None, OsStr::new(name), "seat0"));
        }
    }
}
