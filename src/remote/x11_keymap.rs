//! Stable fingerprint for the core X11 keyboard and modifier maps.
//!
//! The LAN MVP forwards core keycodes so the host X server can perform its
//! normal XKB processing and autorepeat.  Raw keycodes are only meaningful
//! when both peers use the same map, so input negotiation compares this digest
//! and fails closed to view-only on a mismatch.

use super::RemoteResult;
use sha2::{Digest, Sha256};
use std::io;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;

const DOMAIN: &[u8] = b"jwm-remote/v1/x11-core-keymap";

pub fn fingerprint_display(display: Option<&str>) -> RemoteResult<[u8; 32]> {
    let (conn, _) = x11rb::connect(display)?;
    fingerprint(&conn)
}

pub fn fingerprint<C: Connection>(conn: &C) -> RemoteResult<[u8; 32]> {
    let setup = conn.setup();
    let count = setup
        .max_keycode
        .checked_sub(setup.min_keycode)
        .and_then(|range| range.checked_add(1))
        .ok_or_else(|| invalid_data("X11 keyboard keycode range is invalid"))?;
    let keyboard = conn
        .get_keyboard_mapping(setup.min_keycode, count)?
        .reply()?;
    let modifiers = conn.get_modifier_mapping()?.reply()?;

    let mut digest = Sha256::new();
    digest.update(DOMAIN);
    digest.update([setup.min_keycode, setup.max_keycode]);
    digest.update([keyboard.keysyms_per_keycode]);
    for keysym in keyboard.keysyms {
        digest.update(keysym.to_be_bytes());
    }
    digest.update([modifiers.keycodes_per_modifier()]);
    digest.update(&modifiers.keycodes);
    Ok(digest.finalize().into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
