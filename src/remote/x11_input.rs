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
    /// Core pointer map, refreshed only after a `Mapping::POINTER` notice.
    ///
    /// Fetching it per batch cost one blocking round trip for every scroll
    /// notch, because a wheel notch is a button press/release edge.
    pointer_mapping: Option<Vec<u8>>,
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
            pointer_mapping: None,
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
    pub fn inject(&mut self, event: InputEvent, origin: (i16, i16)) -> Result<(), InputError> {
        self.inject_batch(std::slice::from_ref(&event), origin)
    }

    /// Validate a complete batch, queue its XTEST requests in order, then
    /// flush the connection once. No request is queued until every keycode,
    /// coordinate, button mapping, and `ReleaseAll` expansion is valid.
    /// `origin` is where the shared area starts in root coordinates.
    ///
    /// The viewer maps its window onto the shared area, so the coordinates it
    /// sends are relative to that area, while XTEST warps in root
    /// coordinates. Sharing one monitor of a multi-monitor root would
    /// otherwise land every click on the leftmost display.
    pub fn inject_batch(
        &mut self,
        events: &[InputEvent],
        origin: (i16, i16),
    ) -> Result<(), InputError> {
        if events.is_empty() {
            return Ok(());
        }

        let needs_pointer_mapping = events
            .iter()
            .any(|event| matches!(event, InputEvent::Button { .. } | InputEvent::ReleaseAll));
        let pointer_mapping = if needs_pointer_mapping {
            self.pointer_mapping()?.to_vec()
        } else {
            Vec::new()
        };
        let (prepared, next_pressed, uncertain_pressed) = prepare_batch(
            events,
            &self.pressed,
            self.min_keycode,
            self.max_keycode,
            &pointer_mapping,
            origin,
        )?;
        let conn = &self.conn;
        let root = self.root;
        execute_prepared_batch(
            &mut self.pressed,
            prepared,
            next_pressed,
            uncertain_pressed,
            |request| queue_prepared(conn, root, request),
            || conn.flush().map_err(InputError::from),
        )
    }

    /// Drain X11 notifications and report whether the keyboard or modifier
    /// mapping may have changed.  Callers re-fingerprint before deciding that
    /// keyboard forwarding is unsafe: some X11 tools temporarily replace and
    /// then restore a mapping, leaving a harmless notification queued here.
    pub fn take_keymap_change(&mut self) -> Result<bool, InputError> {
        let mut changed = false;
        while let Some(event) = self.conn.poll_for_event()? {
            if let Event::MappingNotify(event) = event {
                if event.request == Mapping::POINTER {
                    // Drop the cache rather than re-fetching here: the next
                    // batch that actually needs a button will pay for it.
                    self.pointer_mapping = None;
                } else {
                    changed = true;
                }
            }
        }
        Ok(changed)
    }

    /// True while this injector holds any key or button down on the host.
    #[must_use]
    pub fn has_pressed(&self) -> bool {
        !self.pressed.held.is_empty()
    }

    /// Best-effort unwind of every key/button pressed by this injector.
    ///
    /// All releases are attempted even after an error.  Successfully sent
    /// releases are removed from the tracked state so a later call can retry
    /// only what remains.
    pub fn release_all(&mut self) -> Result<(), InputError> {
        let mut first_error = None;
        for held in self.pressed.release_plan() {
            match self.send_release(held) {
                Ok(()) => self.pressed.release(held),
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

    fn pointer_mapping(&mut self) -> Result<&[u8], InputError> {
        if self.pointer_mapping.is_none() {
            self.pointer_mapping = Some(self.conn.get_pointer_mapping()?.reply()?.map);
        }
        Ok(self
            .pointer_mapping
            .as_deref()
            .expect("the pointer mapping was just populated"))
    }

    /// Release one tracked press without consulting the current pointer map.
    ///
    /// The physical button was pinned when the press was queued, so a remap
    /// that lands mid-press cannot leave the real button stuck down.
    fn send_release(&mut self, held: HeldInput) -> Result<(), InputError> {
        let request = release_request(held, self.min_keycode, self.max_keycode)?;
        queue_prepared(&self.conn, self.root, request)?;
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

/// Translate a shared-area coordinate into a root coordinate.
fn root_coordinate(axis: char, value: u16, origin: i16) -> Result<i16, InputError> {
    let translated = i32::from(coordinate(axis, value)?) + i32::from(origin);
    i16::try_from(translated).map_err(|_| InputError::CoordinateOutOfRange { axis, value })
}

fn physical_button(mapping: &[u8], logical: u8) -> Option<u8> {
    mapping
        .iter()
        .position(|mapped| *mapped == logical)
        .and_then(|index| u8::try_from(index + 1).ok())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreparedInput {
    type_: u8,
    detail: u8,
    root_x: i16,
    root_y: i16,
}

fn prepare_batch(
    events: &[InputEvent],
    pressed: &PressedState,
    min_keycode: u8,
    max_keycode: u8,
    pointer_mapping: &[u8],
    origin: (i16, i16),
) -> Result<(Vec<PreparedInput>, PressedState, PressedState), InputError> {
    let mut next_pressed = pressed.clone();
    // Until the one final flush succeeds, any prefix of the queued requests
    // may have reached the X server. Retain every key/button that any such
    // prefix could leave held so cleanup can release a conservative superset.
    let mut uncertain_pressed = pressed.clone();
    let mut prepared = Vec::with_capacity(events.len().saturating_add(pressed.held.len()));
    for &event in events {
        if event == InputEvent::ReleaseAll {
            for held in next_pressed.release_plan() {
                // Releases use the button pinned at press time, so they never
                // consult the pointer map and can never be dropped.
                prepared.push(release_request(held, min_keycode, max_keycode)?);
                next_pressed.release(held);
            }
            next_pressed.record_success(InputEvent::ReleaseAll, None);
            continue;
        }
        let Some(request) =
            prepare_input(event, min_keycode, max_keycode, pointer_mapping, origin)?
        else {
            continue;
        };
        let physical = matches!(event, InputEvent::Button { .. }).then_some(request.detail);
        prepared.push(request);
        next_pressed.record_success(event, physical);
        uncertain_pressed.record_possible_press(event, physical);
    }
    Ok((prepared, next_pressed, uncertain_pressed))
}

fn execute_prepared_batch<E>(
    pressed: &mut PressedState,
    prepared: Vec<PreparedInput>,
    next_pressed: PressedState,
    uncertain_pressed: PressedState,
    mut queue: impl FnMut(PreparedInput) -> Result<(), E>,
    mut flush: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    // Queueing and flushing have side effects with an uncertain failure
    // boundary. Install the conservative state first; validation errors return
    // before this helper and therefore still leave the live state untouched.
    *pressed = uncertain_pressed;
    for request in prepared {
        queue(request)?;
    }
    flush()?;
    *pressed = next_pressed;
    Ok(())
}

/// Build one XTEST request, or `None` when this host cannot express the event.
fn prepare_input(
    event: InputEvent,
    min_keycode: u8,
    max_keycode: u8,
    pointer_mapping: &[u8],
    origin: (i16, i16),
) -> Result<Option<PreparedInput>, InputError> {
    let (type_, detail, root_x, root_y) = match event {
        InputEvent::Pointer { x, y } => (
            MOTION_NOTIFY_EVENT,
            ABSOLUTE_MOTION,
            root_coordinate('x', x, origin.0)?,
            root_coordinate('y', y, origin.1)?,
        ),
        InputEvent::Key { keycode, pressed } => {
            if !(min_keycode..=max_keycode).contains(&keycode) {
                return Err(InputError::InvalidKeycode {
                    keycode,
                    min: min_keycode,
                    max: max_keycode,
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
            // Core events carry the logical button after the source server's
            // mapping, while XTEST expects a physical button and applies the
            // destination mapping. One batch snapshots the mapping before it
            // queues any sparse button edge.
            // An unmapped button is a property of *this host's* pointer map,
            // not a protocol violation: the peer's 12-button mouse or its
            // horizontal-scroll buttons 6/7 simply do not exist here. Skip the
            // event. Failing the batch used to disconnect the session, and
            // because the whole batch is validated before anything is queued,
            // it also discarded every pointer motion and any ReleaseAll
            // travelling alongside it.
            let Some(physical) = physical_button(pointer_mapping, button) else {
                return Ok(None);
            };
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
        InputEvent::ReleaseAll => unreachable!("ReleaseAll is expanded before preparation"),
    };
    Ok(Some(PreparedInput {
        type_,
        detail,
        root_x,
        root_y,
    }))
}

fn queue_prepared(
    conn: &RustConnection,
    root: Window,
    request: PreparedInput,
) -> Result<(), InputError> {
    // XTEST has no modifier-mask argument. Modifier keys arrive as normal
    // ordered Key events and therefore update server state naturally.
    conn.xtest_fake_input(
        request.type_,
        request.detail,
        NO_DELAY_MS,
        root,
        request.root_x,
        request.root_y,
        CORE_DEVICE,
    )?
    // Extension presence/version and value ranges were checked above. Avoid a
    // synchronous round-trip per event and consume asynchronous error cookies.
    .ignore_error();
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeldInput {
    Key(u8),
    /// A held button, with the physical button pinned at press time.
    ///
    /// `logical` is what the peer sent and what a later release event matches
    /// on; `physical` is what XTEST was actually told to press. Re-deriving
    /// `physical` at release time against a pointer map that changed during
    /// the press would release a different button and leave the real one down.
    Button {
        logical: u8,
        physical: u8,
    },
}

#[derive(Clone, Debug, Default)]
struct PressedState {
    // One unified order lets release_all unwind mixed key/button chords in the
    // exact reverse order in which their first presses succeeded.
    held: Vec<HeldInput>,
}

impl PressedState {
    /// Record the outcome of an event that was actually queued.
    ///
    /// `physical` carries the resolved button for a button press; it is
    /// ignored for keys and releases.
    fn record_success(&mut self, event: InputEvent, physical: Option<u8>) {
        let (held, pressed) = match event {
            InputEvent::Key { keycode, pressed } => (HeldInput::Key(keycode), pressed),
            InputEvent::Button { button, pressed } => (
                HeldInput::Button {
                    logical: button,
                    physical: physical.unwrap_or(button),
                },
                pressed,
            ),
            InputEvent::Pointer { .. } => return,
            InputEvent::ReleaseAll => {
                self.held.clear();
                return;
            }
        };

        if pressed {
            if !self.held.iter().any(|item| same_input(*item, held)) {
                self.held.push(held);
            }
        } else if let Some(index) = self.held.iter().position(|item| same_input(*item, held)) {
            self.held.remove(index);
        }
    }

    /// Forget one held input that has already been released.
    fn release(&mut self, held: HeldInput) {
        if let Some(index) = self.held.iter().position(|item| same_input(*item, held)) {
            self.held.remove(index);
        }
    }

    /// Held inputs in reverse press order, so chords unwind naturally.
    fn release_plan(&self) -> Vec<HeldInput> {
        self.held.iter().rev().copied().collect()
    }

    fn record_possible_press(&mut self, event: InputEvent, physical: Option<u8>) {
        if matches!(
            event,
            InputEvent::Key { pressed: true, .. } | InputEvent::Button { pressed: true, .. }
        ) {
            self.record_success(event, physical);
        }
    }
}

/// Identity for the held set: buttons are identified by their logical number,
/// because that is what a peer's release event names.
fn same_input(left: HeldInput, right: HeldInput) -> bool {
    match (left, right) {
        (HeldInput::Key(left), HeldInput::Key(right)) => left == right,
        (HeldInput::Button { logical: left, .. }, HeldInput::Button { logical: right, .. }) => {
            left == right
        }
        _ => false,
    }
}

/// Build the XTEST request that releases one already-pressed input.
fn release_request(
    held: HeldInput,
    min_keycode: u8,
    max_keycode: u8,
) -> Result<PreparedInput, InputError> {
    match held {
        HeldInput::Key(keycode) => {
            if !(min_keycode..=max_keycode).contains(&keycode) {
                return Err(InputError::InvalidKeycode {
                    keycode,
                    min: min_keycode,
                    max: max_keycode,
                });
            }
            Ok(PreparedInput {
                type_: KEY_RELEASE_EVENT,
                detail: keycode,
                root_x: 0,
                root_y: 0,
            })
        }
        HeldInput::Button { physical, .. } => Ok(PreparedInput {
            type_: BUTTON_RELEASE_EVENT,
            detail: physical,
            root_x: 0,
            root_y: 0,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, HeldInput, InputEvent, KEY_PRESS_EVENT,
        KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, PressedState, coordinate, execute_prepared_batch,
        physical_button, prepare_batch, supports_required_xtest,
    };

    #[test]
    fn pressed_state_tracks_unique_inputs_in_press_order() {
        let mut state = PressedState::default();
        let key = InputEvent::Key {
            keycode: 38,
            pressed: true,
        };
        state.record_success(key, None);
        state.record_success(
            InputEvent::Button {
                button: 1,
                pressed: true,
            },
            None,
        );
        state.record_success(key, None);

        assert_eq!(
            state.release_plan(),
            vec![
                HeldInput::Button {
                    logical: 1,
                    physical: 1,
                },
                HeldInput::Key(38),
            ]
        );
    }

    #[test]
    fn successful_release_removes_only_the_matching_input() {
        let mut state = PressedState::default();
        state.record_success(
            InputEvent::Key {
                keycode: 37,
                pressed: true,
            },
            None,
        );
        state.record_success(
            InputEvent::Button {
                button: 1,
                pressed: true,
            },
            None,
        );
        state.record_success(
            InputEvent::Key {
                keycode: 54,
                pressed: true,
            },
            None,
        );
        state.record_success(
            InputEvent::Button {
                button: 1,
                pressed: false,
            },
            None,
        );

        assert_eq!(
            state.release_plan(),
            vec![HeldInput::Key(54), HeldInput::Key(37)]
        );
    }

    #[test]
    fn release_all_state_transition_clears_everything() {
        let mut state = PressedState::default();
        state.record_success(
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            None,
        );
        state.record_success(
            InputEvent::Button {
                button: 3,
                pressed: true,
            },
            None,
        );
        state.record_success(InputEvent::ReleaseAll, None);

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
    fn pointer_coordinates_are_translated_into_root_space() {
        // The viewer maps its window onto the shared area, so it sends
        // area-relative coordinates. Sharing the right-hand monitor of a dual
        // 1920x1080 root would otherwise land every click on the left one.
        let pressed = PressedState::default();
        let events = [InputEvent::Pointer { x: 10, y: 20 }];
        let (prepared, _, _) =
            prepare_batch(&events, &pressed, 8, 255, &[1, 2, 3], (1920, 0)).unwrap();
        assert_eq!(prepared[0].root_x, 1930);
        assert_eq!(prepared[0].root_y, 20);

        // Whole-root sharing keeps the coordinates exactly as sent.
        let (prepared, _, _) =
            prepare_batch(&events, &pressed, 8, 255, &[1, 2, 3], (0, 0)).unwrap();
        assert_eq!((prepared[0].root_x, prepared[0].root_y), (10, 20));

        // A translation that leaves the X11 coordinate range fails closed
        // rather than wrapping into some unrelated part of the desktop.
        assert!(
            prepare_batch(
                &[InputEvent::Pointer { x: 30000, y: 0 }],
                &pressed,
                8,
                255,
                &[1, 2, 3],
                (30000, 0)
            )
            .is_err()
        );
    }

    #[test]
    fn an_unmapped_button_is_dropped_without_losing_the_rest_of_the_batch() {
        // The peer's pointer map is not this host's. A 12-button mouse, or a
        // horizontal scroll (libinput legacy buttons 6/7), names buttons that
        // simply do not exist here. Failing the batch used to end the session,
        // and because prevalidation is all-or-nothing it also discarded the
        // pointer motion travelling alongside the bad button.
        let pressed = PressedState::default();
        let events = [
            InputEvent::Pointer { x: 10, y: 20 },
            InputEvent::Button {
                button: 7,
                pressed: true,
            },
            InputEvent::Button {
                button: 7,
                pressed: false,
            },
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
        ];

        let (prepared, next_pressed, uncertain_pressed) =
            prepare_batch(&events, &pressed, 8, 255, &[1, 2, 3], (0, 0)).unwrap();
        let types_and_details: Vec<_> = prepared
            .iter()
            .map(|request| (request.type_, request.detail))
            .collect();
        assert_eq!(
            types_and_details,
            [(MOTION_NOTIFY_EVENT, 0), (KEY_PRESS_EVENT, 38)],
            "the unmappable button vanishes; everything else is preserved in order"
        );
        assert_eq!(
            next_pressed.release_plan(),
            [HeldInput::Key(38)],
            "a button that was never pressed must not be tracked as held"
        );
        assert_eq!(uncertain_pressed.release_plan(), [HeldInput::Key(38)]);
    }

    #[test]
    fn a_pointer_remap_mid_press_still_releases_the_physical_button() {
        // Press logical 1 while the map is [3,2,1] -> physical 3.
        let mut pressed = PressedState::default();
        let (_, next_pressed, _) = prepare_batch(
            &[InputEvent::Button {
                button: 1,
                pressed: true,
            }],
            &pressed,
            8,
            255,
            &[3, 2, 1],
            (0, 0),
        )
        .unwrap();
        pressed = next_pressed;
        assert_eq!(
            pressed.release_plan(),
            [HeldInput::Button {
                logical: 1,
                physical: 3,
            }]
        );

        // The pointer is remapped to identity while the button is still down.
        // The release must still target physical 3, or the real button stays
        // stuck down on the host with no way for the peer to lift it.
        let (prepared, after, _) = prepare_batch(
            &[InputEvent::ReleaseAll],
            &pressed,
            8,
            255,
            &[1, 2, 3],
            (0, 0),
        )
        .unwrap();
        assert_eq!(
            prepared
                .iter()
                .map(|request| (request.type_, request.detail))
                .collect::<Vec<_>>(),
            [(BUTTON_RELEASE_EVENT, 3)]
        );
        assert!(after.release_plan().is_empty());
    }

    #[test]
    fn logical_buttons_are_inverted_through_the_host_mapping() {
        assert_eq!(physical_button(&[3, 2, 1, 4, 5], 1).unwrap(), 3);
        assert_eq!(physical_button(&[3, 2, 1, 4, 5], 3).unwrap(), 1);
        assert!(physical_button(&[3, 2, 1], 4).is_none());
    }

    #[test]
    fn batch_prevalidation_preserves_edges_and_expands_release_all_in_reverse_order() {
        let mut pressed = PressedState::default();
        pressed.record_success(
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            None,
        );
        // Logical 1 resolves to physical 3 under the map used below; pinning
        // it here is what lets the ReleaseAll below emit physical 3.
        pressed.record_success(
            InputEvent::Button {
                button: 1,
                pressed: true,
            },
            Some(3),
        );
        let events = [
            InputEvent::Pointer { x: 120, y: 240 },
            InputEvent::Key {
                keycode: 39,
                pressed: true,
            },
            InputEvent::ReleaseAll,
            InputEvent::Button {
                button: 3,
                pressed: true,
            },
        ];

        let (prepared, next_pressed, uncertain_pressed) =
            prepare_batch(&events, &pressed, 8, 255, &[3, 2, 1, 4, 5], (0, 0)).unwrap();
        let types_and_details: Vec<_> = prepared
            .iter()
            .map(|request| (request.type_, request.detail))
            .collect();
        assert_eq!(
            types_and_details,
            [
                (MOTION_NOTIFY_EVENT, 0),
                (KEY_PRESS_EVENT, 39),
                (KEY_RELEASE_EVENT, 39),
                (BUTTON_RELEASE_EVENT, 3),
                (KEY_RELEASE_EVENT, 38),
                (BUTTON_PRESS_EVENT, 1),
            ]
        );
        assert_eq!(
            next_pressed.release_plan(),
            [HeldInput::Button {
                logical: 3,
                physical: 1,
            }]
        );
        assert_eq!(
            uncertain_pressed.release_plan(),
            [
                HeldInput::Button {
                    logical: 3,
                    physical: 1,
                },
                HeldInput::Key(39),
                HeldInput::Button {
                    logical: 1,
                    physical: 3,
                },
                HeldInput::Key(38),
            ],
            "the uncertain state must cover every held input in any request prefix"
        );
        assert_eq!(
            pressed.release_plan(),
            [
                HeldInput::Button {
                    logical: 1,
                    physical: 3,
                },
                HeldInput::Key(38),
            ],
            "prevalidation must not mutate the live pressed state"
        );
    }

    #[test]
    fn invalid_final_batch_event_cannot_commit_a_valid_pressed_prefix() {
        let pressed = PressedState::default();
        let events = [
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            InputEvent::Pointer {
                x: i16::MAX as u16 + 1,
                y: 0,
            },
        ];

        assert!(prepare_batch(&events, &pressed, 8, 255, &[], (0, 0)).is_err());
        assert!(pressed.release_plan().is_empty());
    }

    #[test]
    fn release_all_has_an_empty_final_state_but_retains_uncertain_cleanup() {
        let mut initial = PressedState::default();
        initial.record_success(
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            None,
        );
        initial.record_success(
            InputEvent::Button {
                button: 1,
                pressed: true,
            },
            None,
        );

        let (_, final_pressed, uncertain_pressed) = prepare_batch(
            &[InputEvent::ReleaseAll],
            &initial,
            8,
            255,
            &[1, 2, 3, 4, 5],
            (0, 0),
        )
        .unwrap();
        assert!(final_pressed.release_plan().is_empty());
        assert_eq!(uncertain_pressed.release_plan(), initial.release_plan());
    }

    #[test]
    fn queue_and_flush_failures_keep_a_conservative_cleanup_plan() {
        let mut initial = PressedState::default();
        initial.record_success(
            InputEvent::Key {
                keycode: 37,
                pressed: true,
            },
            None,
        );
        let events = [
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            InputEvent::Key {
                keycode: 38,
                pressed: false,
            },
            InputEvent::Button {
                button: 1,
                pressed: true,
            },
            InputEvent::ReleaseAll,
            InputEvent::Button {
                button: 3,
                pressed: true,
            },
        ];
        let (prepared, final_pressed, uncertain_pressed) =
            prepare_batch(&events, &initial, 8, 255, &[1, 2, 3, 4, 5], (0, 0)).unwrap();
        let uncertain_plan = uncertain_pressed.release_plan();
        let final_plan = final_pressed.release_plan();

        for fail_at in [1, 3] {
            let mut live = initial.clone();
            let mut queue_calls = 0;
            let queue_error = execute_prepared_batch(
                &mut live,
                prepared.clone(),
                final_pressed.clone(),
                uncertain_pressed.clone(),
                |_| {
                    queue_calls += 1;
                    if queue_calls == fail_at {
                        Err("synthetic queue failure")
                    } else {
                        Ok(())
                    }
                },
                || Ok(()),
            )
            .unwrap_err();
            assert_eq!(queue_error, "synthetic queue failure");
            assert_eq!(live.release_plan(), uncertain_plan);
        }

        let mut live = initial.clone();
        let flush_error = execute_prepared_batch(
            &mut live,
            prepared.clone(),
            final_pressed.clone(),
            uncertain_pressed.clone(),
            |_| Ok(()),
            || Err("synthetic flush failure"),
        )
        .unwrap_err();
        assert_eq!(flush_error, "synthetic flush failure");
        assert_eq!(live.release_plan(), uncertain_plan);

        let mut live = initial;
        let mut queued = Vec::new();
        let mut flushes = 0;
        execute_prepared_batch(
            &mut live,
            prepared.clone(),
            final_pressed,
            uncertain_pressed,
            |request| {
                queued.push(request);
                Ok::<(), &'static str>(())
            },
            || {
                flushes += 1;
                Ok::<(), &'static str>(())
            },
        )
        .unwrap();
        assert_eq!(queued, prepared);
        assert_eq!(flushes, 1);
        assert_eq!(live.release_plan(), final_plan);
    }
}
