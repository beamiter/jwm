//! X11 input injection for the remote-control host.
//!
//! This deliberately opens its own X11 connection.  The connection is
//! independent of whichever transport (`x11rb` or `xcb`) the running JWM uses,
//! while XTEST still injects into the same X server-wide input stream.

use std::error::Error;
use std::fmt;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, Mapping, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const REQUIRED_XTEST_VERSION: (u8, u16) = (2, 1);
const NO_DELAY_MS: u32 = 0;
const CORE_DEVICE: u8 = 0;
const ABSOLUTE_MOTION: u8 = 0;

/// One input operation received from a remote viewer.
///
/// Keycodes are X11 server keycodes, not keysyms or Linux evdev codes.  Peers
/// must therefore negotiate/verify compatible keymaps before forwarding them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Pointer { x: u16, y: u16 },
    Key { keycode: u8, pressed: bool },
    Button { button: u8, pressed: bool },
    ReleaseAll,
}

/// Errors reported while opening X11 or injecting an input operation.
#[derive(Debug)]
pub enum InputError {
    Connect(x11rb::rust_connection::ConnectError),
    Connection(x11rb::errors::ConnectionError),
    Reply(x11rb::errors::ReplyError),
    ScreenUnavailable { screen: usize },
    UnsupportedXtestVersion { major: u8, minor: u16 },
    InvalidKeycode { keycode: u8, min: u8, max: u8 },
    InvalidButton { button: u8 },
    UnmappedButton { button: u8 },
    CoordinateOutOfRange { axis: char, value: u16 },
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => write!(f, "could not connect to X11: {error}"),
            Self::Connection(error) => write!(f, "X11 connection error: {error}"),
            Self::Reply(error) => write!(f, "X11 reply error: {error}"),
            Self::ScreenUnavailable { screen } => {
                write!(f, "X11 screen {screen} is not present in the server setup")
            }
            Self::UnsupportedXtestVersion { major, minor } => write!(
                f,
                "XTEST {major}.{minor} is too old; version {}.{} or newer is required",
                REQUIRED_XTEST_VERSION.0, REQUIRED_XTEST_VERSION.1
            ),
            Self::InvalidKeycode { keycode, min, max } => write!(
                f,
                "X11 keycode {keycode} is outside the server range {min}..={max}"
            ),
            Self::InvalidButton { button } => {
                write!(f, "X11 button number {button} is invalid")
            }
            Self::UnmappedButton { button } => write!(
                f,
                "logical X11 button {button} is not present in the host pointer mapping"
            ),
            Self::CoordinateOutOfRange { axis, value } => write!(
                f,
                "pointer {axis} coordinate {value} exceeds XTEST's i16 range"
            ),
        }
    }
}

impl Error for InputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Connect(error) => Some(error),
            Self::Connection(error) => Some(error),
            Self::Reply(error) => Some(error),
            Self::ScreenUnavailable { .. }
            | Self::UnsupportedXtestVersion { .. }
            | Self::InvalidKeycode { .. }
            | Self::InvalidButton { .. }
            | Self::UnmappedButton { .. }
            | Self::CoordinateOutOfRange { .. } => None,
        }
    }
}

impl From<x11rb::rust_connection::ConnectError> for InputError {
    fn from(error: x11rb::rust_connection::ConnectError) -> Self {
        Self::Connect(error)
    }
}

impl From<x11rb::errors::ConnectionError> for InputError {
    fn from(error: x11rb::errors::ConnectionError) -> Self {
        Self::Connection(error)
    }
}

impl From<x11rb::errors::ReplyError> for InputError {
    fn from(error: x11rb::errors::ReplyError) -> Self {
        Self::Reply(error)
    }
}

/// An independent X11/XTEST connection used by the remote-control host.
pub struct InputInjector {
    conn: RustConnection,
    root: Window,
    min_keycode: u8,
    max_keycode: u8,
    pressed: PressedState,
}

impl InputInjector {
    /// Connect to `display` (or `$DISPLAY` when it is `None`) and verify XTEST.
    pub fn connect(display: Option<&str>) -> Result<Self, InputError> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_num)
            .ok_or(InputError::ScreenUnavailable { screen: screen_num })?;
        let root = screen.root;
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;

        let version = conn
            .xtest_get_version(REQUIRED_XTEST_VERSION.0, REQUIRED_XTEST_VERSION.1)?
            .reply()?;
        if !supports_required_xtest(version.major_version, version.minor_version) {
            return Err(InputError::UnsupportedXtestVersion {
                major: version.major_version,
                minor: version.minor_version,
            });
        }

        Ok(Self {
            conn,
            root,
            min_keycode,
            max_keycode,
            pressed: PressedState::default(),
        })
    }

    pub fn keymap_fingerprint(&self) -> super::RemoteResult<[u8; 32]> {
        super::x11_keymap::fingerprint(&self.conn)
    }

    /// Inject one operation and flush it to the X server.
    ///
    /// XTEST's `time` field is a delay in milliseconds, not the source X event
    /// timestamp, so interactive operations always use a zero delay here.
    pub fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        if event == InputEvent::ReleaseAll {
            return self.release_all();
        }

        self.send(event)?;
        self.pressed.record_success(event);
        Ok(())
    }

    /// Drain X11 notifications and report whether the keyboard or modifier
    /// mapping may have changed.  Callers re-fingerprint before deciding that
    /// keyboard forwarding is unsafe: some X11 tools temporarily replace and
    /// then restore a mapping, leaving a harmless notification queued here.
    pub fn take_keymap_change(&self) -> Result<bool, InputError> {
        let mut changed = false;
        while let Some(event) = self.conn.poll_for_event()? {
            if let Event::MappingNotify(event) = event
                && event.request != Mapping::POINTER
            {
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Best-effort unwind of every key/button pressed by this injector.
    ///
    /// All releases are attempted even after an error.  Successfully sent
    /// releases are removed from the tracked state so a later call can retry
    /// only what remains.
    pub fn release_all(&mut self) -> Result<(), InputError> {
        let mut first_error = None;
        for event in self.pressed.release_plan() {
            match self.send(event) {
                Ok(()) => self.pressed.record_success(event),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if first_error.is_none()
            && let Err(error) = self.conn.sync()
        {
            first_error = Some(InputError::Reply(error));
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn send(&self, event: InputEvent) -> Result<(), InputError> {
        let (type_, detail, root_x, root_y) = match event {
            InputEvent::Pointer { x, y } => (
                MOTION_NOTIFY_EVENT,
                ABSOLUTE_MOTION,
                coordinate('x', x)?,
                coordinate('y', y)?,
            ),
            InputEvent::Key { keycode, pressed } => {
                if !(self.min_keycode..=self.max_keycode).contains(&keycode) {
                    return Err(InputError::InvalidKeycode {
                        keycode,
                        min: self.min_keycode,
                        max: self.max_keycode,
                    });
                }
                (
                    if pressed {
                        KEY_PRESS_EVENT
                    } else {
                        KEY_RELEASE_EVENT
                    },
                    keycode,
                    0,
                    0,
                )
            }
            InputEvent::Button { button, pressed } => {
                if button == 0 {
                    return Err(InputError::InvalidButton { button });
                }
                // Core events carry the logical button after the source
                // server's mapping, while XTEST expects a physical button and
                // applies the destination mapping. Invert the current host
                // map on every sparse button edge so MappingNotify changes do
                // not silently double-map left-handed input.
                let mapping = self.conn.get_pointer_mapping()?.reply()?.map;
                let physical = physical_button(&mapping, button)?;
                (
                    if pressed {
                        BUTTON_PRESS_EVENT
                    } else {
                        BUTTON_RELEASE_EVENT
                    },
                    physical,
                    0,
                    0,
                )
            }
            InputEvent::ReleaseAll => return Ok(()),
        };

        // XTEST has no modifier-mask argument.  Modifier keys arrive as normal
        // ordered Key events and therefore update server state naturally.
        self.conn
            .xtest_fake_input(
                type_,
                detail,
                NO_DELAY_MS,
                self.root,
                root_x,
                root_y,
                CORE_DEVICE,
            )?
            // Extension presence/version and value ranges were checked above.
            // Avoid a synchronous X round-trip for every mouse movement, and do
            // not leave asynchronous errors queued on this injection-only link.
            .ignore_error();
        self.conn.flush()?;
        Ok(())
    }
}

impl Drop for InputInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

fn supports_required_xtest(major: u8, minor: u16) -> bool {
    major > REQUIRED_XTEST_VERSION.0
        || (major == REQUIRED_XTEST_VERSION.0 && minor >= REQUIRED_XTEST_VERSION.1)
}

fn coordinate(axis: char, value: u16) -> Result<i16, InputError> {
    i16::try_from(value).map_err(|_| InputError::CoordinateOutOfRange { axis, value })
}

fn physical_button(mapping: &[u8], logical: u8) -> Result<u8, InputError> {
    mapping
        .iter()
        .position(|mapped| *mapped == logical)
        .and_then(|index| u8::try_from(index + 1).ok())
        .ok_or(InputError::UnmappedButton { button: logical })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeldInput {
    Key(u8),
    Button(u8),
}

#[derive(Debug, Default)]
struct PressedState {
    // One unified order lets release_all unwind mixed key/button chords in the
    // exact reverse order in which their first presses succeeded.
    held: Vec<HeldInput>,
}

impl PressedState {
    fn record_success(&mut self, event: InputEvent) {
        let (held, pressed) = match event {
            InputEvent::Key { keycode, pressed } => (HeldInput::Key(keycode), pressed),
            InputEvent::Button { button, pressed } => (HeldInput::Button(button), pressed),
            InputEvent::Pointer { .. } => return,
            InputEvent::ReleaseAll => {
                self.held.clear();
                return;
            }
        };

        if pressed {
            if !self.held.contains(&held) {
                self.held.push(held);
            }
        } else if let Some(index) = self.held.iter().position(|item| *item == held) {
            self.held.remove(index);
        }
    }

    fn release_plan(&self) -> Vec<InputEvent> {
        self.held
            .iter()
            .rev()
            .map(|held| match *held {
                HeldInput::Key(keycode) => InputEvent::Key {
                    keycode,
                    pressed: false,
                },
                HeldInput::Button(button) => InputEvent::Button {
                    button,
                    pressed: false,
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{InputEvent, PressedState, coordinate, physical_button, supports_required_xtest};

    #[test]
    fn pressed_state_tracks_unique_inputs_in_press_order() {
        let mut state = PressedState::default();
        let key = InputEvent::Key {
            keycode: 38,
            pressed: true,
        };
        state.record_success(key);
        state.record_success(InputEvent::Button {
            button: 1,
            pressed: true,
        });
        state.record_success(key);

        assert_eq!(
            state.release_plan(),
            vec![
                InputEvent::Button {
                    button: 1,
                    pressed: false,
                },
                InputEvent::Key {
                    keycode: 38,
                    pressed: false,
                },
            ]
        );
    }

    #[test]
    fn successful_release_removes_only_the_matching_input() {
        let mut state = PressedState::default();
        state.record_success(InputEvent::Key {
            keycode: 37,
            pressed: true,
        });
        state.record_success(InputEvent::Button {
            button: 1,
            pressed: true,
        });
        state.record_success(InputEvent::Key {
            keycode: 54,
            pressed: true,
        });
        state.record_success(InputEvent::Button {
            button: 1,
            pressed: false,
        });

        assert_eq!(
            state.release_plan(),
            vec![
                InputEvent::Key {
                    keycode: 54,
                    pressed: false,
                },
                InputEvent::Key {
                    keycode: 37,
                    pressed: false,
                },
            ]
        );
    }

    #[test]
    fn release_all_state_transition_clears_everything() {
        let mut state = PressedState::default();
        state.record_success(InputEvent::Key {
            keycode: 38,
            pressed: true,
        });
        state.record_success(InputEvent::Button {
            button: 3,
            pressed: true,
        });
        state.record_success(InputEvent::ReleaseAll);

        assert!(state.release_plan().is_empty());
    }

    #[test]
    fn xtest_version_and_coordinate_boundaries_are_explicit() {
        assert!(!supports_required_xtest(2, 0));
        assert!(supports_required_xtest(2, 1));
        assert!(supports_required_xtest(3, 0));
        assert_eq!(coordinate('x', i16::MAX as u16).unwrap(), i16::MAX);
        assert!(coordinate('y', i16::MAX as u16 + 1).is_err());
    }

    #[test]
    fn logical_buttons_are_inverted_through_the_host_mapping() {
        assert_eq!(physical_button(&[3, 2, 1, 4, 5], 1).unwrap(), 3);
        assert_eq!(physical_button(&[3, 2, 1, 4, 5], 3).unwrap(), 1);
        assert!(physical_button(&[3, 2, 1], 4).is_err());
    }
}
