//! Per-keycode description of the core X11 keyboard and modifier maps.
//!
//! The LAN MVP forwards core keycodes so the host X server can perform its
//! normal XKB processing and autorepeat. A raw keycode is only meaningful when
//! both peers turn it into the same keysym, so input negotiation compares the
//! two maps and fails closed to pointer-only on a mismatch.
//!
//! Comparing whole maps by hash made that brittle on real machines. Tools that
//! type text through XTEST bind spare keycodes to the characters they need and
//! do not always put them back, and the core table can widen as a side
//! effect. Two desktops with the same layout then disagreed, and keyboard
//! control silently dropped to pointer-only.
//!
//! What matters is narrower: a keycode bound on both sides must mean the same
//! thing on both. A keycode unbound on either side cannot type a wrong
//! character -- the viewer's keyboard does not produce it, or the host ignores
//! it. So each side describes every keycode separately, and only keycodes both
//! have bound are compared. Modifier maps are compared exactly.

use super::RemoteResult;
use sha2::{Digest, Sha256};
use std::fmt;
use std::io;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;

const KEY_DOMAIN: &[u8] = b"jwm-remote/v2/x11-core-key";
const MODIFIER_DOMAIN: &[u8] = b"jwm-remote/v2/x11-modifiers";
const NO_SYMBOL: u32 = 0;
const MIN_X11_KEYCODE: u8 = 8;
/// Bytes of each keycode's digest. A collision only matters between two
/// keycodes that already disagree, and 2^-24 per key is far below the rate
/// at which a person notices a wrong character.
pub const KEY_DIGEST_LEN: usize = 3;
const MODIFIER_DIGEST_LEN: usize = 32;
/// Largest encoded table: minimum keycode, count, modifiers, 248 keycodes.
pub const MAX_ENCODED_KEYMAP_LEN: usize =
    2 + MODIFIER_DIGEST_LEN + (256 - MIN_X11_KEYCODE as usize) * KEY_DIGEST_LEN;
/// Fewer shared keycodes than this cannot be a real keyboard, so agreement on
/// them proves nothing.
const MIN_SHARED_KEYCODES: usize = 32;
/// Differing keycodes named in a diagnostic before the rest are counted.
const SHOWN_DIFFERING_KEYCODES: usize = 8;
const UNBOUND: [u8; KEY_DIGEST_LEN] = [0; KEY_DIGEST_LEN];

/// One side's keymap, as compared during negotiation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeymapTable {
    min_keycode: u8,
    /// Digest of each keycode's base symbols from `min_keycode` up;
    /// [`UNBOUND`] for a keycode with none.
    keys: Vec<[u8; KEY_DIGEST_LEN]>,
    modifiers: [u8; MODIFIER_DIGEST_LEN],
}

/// The outcome of comparing two keymaps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeymapMatch {
    Compatible,
    /// Shift, Control, Lock or a ModN modifier sits on different keycodes.
    ModifiersDiffer,
    /// Keycodes bound on both sides to different symbols.
    KeysDiffer(Vec<u8>),
    /// Too few keycodes are bound on both sides to compare meaningfully.
    TooFewShared(usize),
}

impl fmt::Display for KeymapMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compatible => f.write_str("keymaps agree"),
            Self::ModifiersDiffer => f.write_str("modifier keys differ"),
            Self::KeysDiffer(keycodes) => {
                let shown = keycodes
                    .iter()
                    .take(SHOWN_DIFFERING_KEYCODES)
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{} keys differ (keycodes {shown}", keycodes.len())?;
                if keycodes.len() > SHOWN_DIFFERING_KEYCODES {
                    f.write_str(", ...")?;
                }
                f.write_str(")")
            }
            Self::TooFewShared(shared) => {
                write!(f, "only {shared} keys are bound on both sides")
            }
        }
    }
}

impl KeymapTable {
    /// The table a view-only viewer sends: no keyboard to describe.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            min_keycode: 0,
            keys: Vec::new(),
            modifiers: [0; MODIFIER_DIGEST_LEN],
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Read the current core keyboard and modifier maps from `conn`.
    pub fn read<C: Connection>(conn: &C) -> RemoteResult<Self> {
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
        Ok(Self::from_core(
            setup.min_keycode,
            count,
            keyboard.keysyms_per_keycode,
            &keyboard.keysyms,
            modifiers.keycodes_per_modifier(),
            &modifiers.keycodes,
        ))
    }

    /// Build a table from the raw core-protocol replies.
    fn from_core(
        min_keycode: u8,
        count: u8,
        keysyms_per_keycode: u8,
        keysyms: &[u32],
        keycodes_per_modifier: u8,
        modifier_keycodes: &[u8],
    ) -> Self {
        let width = usize::from(keysyms_per_keycode);
        let keys = (0..usize::from(count))
            .map(|index| {
                let row = keysyms
                    .get(index * width..(index + 1) * width)
                    .unwrap_or_default();
                key_digest(row)
            })
            .collect();
        Self {
            min_keycode,
            keys,
            modifiers: modifier_digest(keycodes_per_modifier, modifier_keycodes),
        }
    }

    /// Append the wire form: minimum keycode, count, modifiers, then digests.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.min_keycode);
        // A table holds at most 248 keycodes, so its count fits a byte.
        out.push(u8::try_from(self.keys.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(&self.modifiers);
        for key in &self.keys {
            out.extend_from_slice(key);
        }
    }

    pub fn decode(bytes: &[u8]) -> RemoteResult<Self> {
        let [min_keycode, count, rest @ ..] = bytes else {
            return Err(invalid_data("keymap description is truncated").into());
        };
        let count = usize::from(*count);
        if rest.len() != MODIFIER_DIGEST_LEN + count * KEY_DIGEST_LEN {
            return Err(invalid_data("keymap description has an invalid length").into());
        }
        if count == 0 {
            if *min_keycode != 0 || rest.iter().any(|&byte| byte != 0) {
                return Err(invalid_data("an empty keymap description must be all zero").into());
            }
            return Ok(Self::empty());
        }
        if *min_keycode < MIN_X11_KEYCODE || usize::from(*min_keycode) + count > 256 {
            return Err(invalid_data("keymap description has an invalid keycode range").into());
        }
        let (modifiers, keys) = rest.split_at(MODIFIER_DIGEST_LEN);
        Ok(Self {
            min_keycode: *min_keycode,
            keys: keys
                .chunks_exact(KEY_DIGEST_LEN)
                .map(|key| key.try_into().unwrap())
                .collect(),
            modifiers: modifiers.try_into().unwrap(),
        })
    }

    /// Whether keycodes from one map mean the same on the other.
    #[must_use]
    pub fn compare(&self, other: &Self) -> KeymapMatch {
        if self.modifiers != other.modifiers {
            return KeymapMatch::ModifiersDiffer;
        }
        let mut shared = 0;
        let mut differing = Vec::new();
        for keycode in MIN_X11_KEYCODE..=u8::MAX {
            let (Some(mine), Some(theirs)) = (self.key(keycode), other.key(keycode)) else {
                continue;
            };
            if mine == theirs {
                shared += 1;
            } else {
                differing.push(keycode);
            }
        }
        if !differing.is_empty() {
            KeymapMatch::KeysDiffer(differing)
        } else if shared < MIN_SHARED_KEYCODES {
            KeymapMatch::TooFewShared(shared)
        } else {
            KeymapMatch::Compatible
        }
    }

    /// Whether every keycode bound in `negotiated` still means the same here.
    ///
    /// For the viewer's own map during a session: the host verified those
    /// keycodes. A spare keycode bound since then is not one the keyboard
    /// produces, so it does not end the session.
    #[must_use]
    pub fn still_covers(&self, negotiated: &Self) -> bool {
        self.modifiers == negotiated.modifiers
            && (MIN_X11_KEYCODE..=u8::MAX).all(|keycode| match negotiated.key(keycode) {
                Some(digest) => self.key(keycode) == Some(digest),
                None => true,
            })
    }

    /// Digest of a bound keycode; `None` when unbound or outside the table.
    fn key(&self, keycode: u8) -> Option<[u8; KEY_DIGEST_LEN]> {
        let index = usize::from(keycode.checked_sub(self.min_keycode)?);
        self.keys
            .get(index)
            .copied()
            .filter(|digest| *digest != UNBOUND)
    }
}

/// Read the keymap of `display` (or `$DISPLAY`).
pub fn read_display(display: Option<&str>) -> RemoteResult<KeymapTable> {
    let (conn, _) = xbar_core::x11_connection::connect(display)?;
    KeymapTable::read(&conn)
}

/// Digest of one keycode's base symbols, or [`UNBOUND`].
///
/// The base is the first two levels of the first two groups -- the first four
/// columns of the core table, normalised as the core protocol defines: an
/// empty second group repeats the first, and a level-two `NoSymbol` repeats
/// level one. Later columns are left out on purpose: their meaning depends on
/// how wide the server made the table, which is exactly what differs between
/// otherwise identical desktops once a tool has rebound a spare keycode.
fn key_digest(row: &[u32]) -> [u8; KEY_DIGEST_LEN] {
    let column = |index: usize| row.get(index).copied().unwrap_or(NO_SYMBOL);
    let group = |first: u32, second: u32| (first, if second == NO_SYMBOL { first } else { second });
    let first = group(column(0), column(1));
    let second = if column(2) == NO_SYMBOL && column(3) == NO_SYMBOL {
        first
    } else {
        group(column(2), column(3))
    };
    if first == (NO_SYMBOL, NO_SYMBOL) && second == (NO_SYMBOL, NO_SYMBOL) {
        return UNBOUND;
    }
    let mut digest = Sha256::new();
    digest.update(KEY_DOMAIN);
    for keysym in [first.0, first.1, second.0, second.1] {
        digest.update(keysym.to_be_bytes());
    }
    let digest = digest.finalize();
    let mut key = [digest[0], digest[1], digest[2]];
    // A bound key must never read as unbound.
    if key == UNBOUND {
        key[KEY_DIGEST_LEN - 1] = 1;
    }
    key
}

/// Digest of the modifier map, independent of its padding width.
fn modifier_digest(keycodes_per_modifier: u8, keycodes: &[u8]) -> [u8; MODIFIER_DIGEST_LEN] {
    let mut digest = Sha256::new();
    digest.update(MODIFIER_DOMAIN);
    let width = usize::from(keycodes_per_modifier).max(1);
    for modifier in 0..8 {
        let mut bound: Vec<u8> = keycodes
            .get(modifier * width..(modifier + 1) * width)
            .unwrap_or_default()
            .iter()
            .copied()
            .filter(|&keycode| keycode != 0)
            .collect();
        bound.sort_unstable();
        bound.dedup();
        digest.update([u8::try_from(bound.len()).unwrap_or(u8::MAX)]);
        digest.update(&bound);
    }
    digest.finalize().into()
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESCAPE: u32 = 0xff1b;
    const SHIFT_L: u32 = 0xffe1;

    /// A 248-keycode US-like map: keycode `k` types keysym `0x100 + k`, with
    /// its shifted form one page up. `width` is the core table width.
    fn us_like(width: u8) -> Vec<u32> {
        let mut keysyms = Vec::new();
        for keycode in 8..=255_u32 {
            let mut row = vec![NO_SYMBOL; usize::from(width)];
            if keycode == 8 {
                // Unbound, like keycode 8 on an evdev keyboard.
            } else if keycode == 9 {
                row[0] = ESCAPE;
                row[2] = ESCAPE;
            } else if keycode < 200 {
                row[0] = 0x100 + keycode;
                row[1] = 0x200 + keycode;
                row[2] = 0x100 + keycode;
                row[3] = 0x200 + keycode;
                // Wider tables repeat the groups in later columns.
                let base = [row[0], row[1], row[2], row[3]];
                for (column, slot) in row.iter_mut().enumerate().skip(4) {
                    *slot = base[column % 4];
                }
            }
            keysyms.extend(row);
        }
        keysyms
    }

    /// Shift on keycodes 50 and 62, Control on 37: the evdev defaults.
    fn modifiers(width: u8) -> Vec<u8> {
        let mut map = vec![0_u8; 8 * usize::from(width)];
        map[0] = 50;
        map[1] = 62;
        map[2 * usize::from(width)] = 37;
        map
    }

    fn table(keysyms: &[u32], width: u8, modifier_width: u8) -> KeymapTable {
        KeymapTable::from_core(
            8,
            248,
            width,
            keysyms,
            modifier_width,
            &modifiers(modifier_width),
        )
    }

    fn rebind(keysyms: &mut [u32], width: u8, keycode: u32, row: &[u32]) {
        let start = (keycode as usize - 8) * usize::from(width);
        keysyms[start..start + usize::from(width)].fill(NO_SYMBOL);
        keysyms[start..start + row.len()].copy_from_slice(row);
    }

    #[test]
    fn the_same_layout_agrees_across_core_table_widths() {
        let narrow = table(&us_like(7), 7, 2);
        let wide = table(&us_like(15), 15, 4);
        assert_eq!(narrow.compare(&wide), KeymapMatch::Compatible);
    }

    /// The case found between two real desktops: spare keycodes rebound to
    /// CJK characters by a text-typing tool and never restored.
    #[test]
    fn leftover_bindings_on_spare_keycodes_do_not_disable_the_keyboard() {
        let viewer = table(&us_like(7), 7, 2);
        let mut polluted = us_like(15);
        for spare in [8, 210, 222] {
            rebind(
                &mut polluted,
                15,
                spare,
                &[0x1006ca1, 0xffe0, 0x100eeee, 0xffe0],
            );
        }
        let host = table(&polluted, 15, 2);
        assert_eq!(viewer.compare(&host), KeymapMatch::Compatible);
        assert_eq!(host.compare(&viewer), KeymapMatch::Compatible);
    }

    #[test]
    fn a_key_bound_differently_on_both_sides_disables_the_keyboard() {
        let viewer = table(&us_like(7), 7, 2);
        let mut swapped = us_like(7);
        // QWERTZ: the keycodes for y and z trade places.
        rebind(&mut swapped, 7, 29, &[0x100 + 52, 0x200 + 52]);
        rebind(&mut swapped, 7, 52, &[0x100 + 29, 0x200 + 29]);
        let host = table(&swapped, 7, 2);
        assert_eq!(viewer.compare(&host), KeymapMatch::KeysDiffer(vec![29, 52]));
        assert_eq!(
            viewer.compare(&host).to_string(),
            "2 keys differ (keycodes 29, 52)"
        );
    }

    #[test]
    fn modifiers_must_match_exactly_but_not_their_padding() {
        let keysyms = us_like(7);
        let viewer = table(&keysyms, 7, 2);
        assert_eq!(
            viewer.compare(&table(&keysyms, 7, 4)),
            KeymapMatch::Compatible
        );
        let mut moved = modifiers(2);
        moved[1] = 66;
        let host = KeymapTable::from_core(8, 248, 7, &keysyms, 2, &moved);
        assert_eq!(viewer.compare(&host), KeymapMatch::ModifiersDiffer);
    }

    #[test]
    fn core_shorthand_rows_normalise_to_their_full_form() {
        // "Escape" alone, "Escape NoSymbol Escape" and the full row agree.
        let full = key_digest(&[ESCAPE, ESCAPE, ESCAPE, ESCAPE]);
        assert_eq!(key_digest(&[ESCAPE]), full);
        assert_eq!(key_digest(&[ESCAPE, NO_SYMBOL, ESCAPE]), full);
        assert_ne!(key_digest(&[SHIFT_L]), full);
        assert_eq!(key_digest(&[]), UNBOUND);
        assert_eq!(key_digest(&[NO_SYMBOL; 7]), UNBOUND);
    }

    #[test]
    fn an_almost_empty_keymap_proves_nothing() {
        let mut sparse = vec![NO_SYMBOL; 248 * 7];
        rebind(&mut sparse, 7, 38, &[0x100 + 38, 0x200 + 38]);
        let sparse = table(&sparse, 7, 2);
        let viewer = table(&us_like(7), 7, 2);
        assert_eq!(viewer.compare(&sparse), KeymapMatch::TooFewShared(1));
    }

    #[test]
    fn a_spare_keycode_bound_mid_session_keeps_the_negotiated_map() {
        let negotiated = table(&us_like(7), 7, 2);
        let mut later = us_like(7);
        rebind(&mut later, 7, 222, &[0x1005236]);
        assert!(table(&later, 7, 2).still_covers(&negotiated));
        rebind(&mut later, 7, 38, &[0x1005236]);
        assert!(!table(&later, 7, 2).still_covers(&negotiated));
    }

    #[test]
    fn the_wire_form_round_trips_and_rejects_malformed_tables() {
        let keymap = table(&us_like(7), 7, 2);
        let mut wire = Vec::new();
        keymap.encode_into(&mut wire);
        assert_eq!(wire.len(), MAX_ENCODED_KEYMAP_LEN);
        assert_eq!(KeymapTable::decode(&wire).unwrap(), keymap);

        let mut empty = Vec::new();
        KeymapTable::empty().encode_into(&mut empty);
        assert!(KeymapTable::decode(&empty).unwrap().is_empty());

        assert!(KeymapTable::decode(&wire[..wire.len() - 1]).is_err());
        let mut below_range = wire.clone();
        below_range[0] = 7;
        assert!(KeymapTable::decode(&below_range).is_err());
        let mut past_255 = wire.clone();
        past_255[0] = 9;
        assert!(KeymapTable::decode(&past_255).is_err());
        let mut dirty_empty = empty.clone();
        dirty_empty[5] = 1;
        assert!(KeymapTable::decode(&dirty_empty).is_err());
    }
}
