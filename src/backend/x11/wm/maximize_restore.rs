//! Strict wire codec for JWM's private maximize-restore restart property.
//!
//! `_JWM_MAXIMIZE_RESTORE_V1` is stored as exactly 6 CARDINAL/32 values:
//!
//! ```text
//!  0 version (= 1)
//!  1 flags
//!  2..=5 restore x/y/w/h
//! ```
//!
//! A visible maximized floating window carries its pre-maximize rectangle
//! here so a seamless X11 exec can adopt the same restore slot the previous
//! process held. Coordinates use their two's-complement bit pattern so
//! negative-origin output layouts round-trip through CARDINAL.

use crate::backend::api::{MaximizeRestoreState, MinimizedRestoreRect};

pub(crate) const MAXIMIZE_RESTORE_V1_WORD_COUNT: usize = 6;
pub(crate) const MAXIMIZE_RESTORE_V1_LONG_LENGTH: u32 = 6;

const VERSION: u32 = 1;
const FLAG_PROMOTED: u32 = 1 << 0;
const KNOWN_FLAGS: u32 = FLAG_PROMOTED;

#[inline]
fn encode_i32_bits(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}

#[inline]
fn decode_i32_bits(value: u32) -> i32 {
    i32::from_ne_bytes(value.to_ne_bytes())
}

fn encode_rect(rect: MinimizedRestoreRect) -> Option<[u32; 4]> {
    if !rect.is_configurable() {
        return None;
    }
    Some([
        encode_i32_bits(rect.x),
        encode_i32_bits(rect.y),
        rect.w as u32,
        rect.h as u32,
    ])
}

fn decode_rect(words: &[u32]) -> Option<MinimizedRestoreRect> {
    let [x, y, w, h] = words else {
        return None;
    };
    let rect = MinimizedRestoreRect {
        x: decode_i32_bits(*x),
        y: decode_i32_bits(*y),
        w: *w as i32,
        h: *h as i32,
    };
    rect.is_configurable().then_some(rect)
}

/// Encode one maximize-restore snapshot. Rejects rectangles no X server could
/// be asked for rather than emitting a property the next process cannot adopt.
pub(crate) fn encode_maximize_restore_v1(
    state: MaximizeRestoreState,
) -> Option<[u32; MAXIMIZE_RESTORE_V1_WORD_COUNT]> {
    let rect = encode_rect(state.restore_rect)?;
    let mut flags = 0;
    if state.promoted {
        flags |= FLAG_PROMOTED;
    }
    Some([VERSION, flags, rect[0], rect[1], rect[2], rect[3]])
}

/// Decode a property reply. Wrong type/format/length or unknown flags →
/// `None` so an untrusted client property cannot block window adoption.
pub(crate) fn decode_maximize_restore_v1<A: Copy + Eq>(
    type_: A,
    cardinal: A,
    format: u8,
    bytes_after: u32,
    words: &[u32],
) -> Option<MaximizeRestoreState> {
    if type_ != cardinal || format != 32 || bytes_after != 0 {
        return None;
    }
    if words.len() != MAXIMIZE_RESTORE_V1_WORD_COUNT {
        return None;
    }
    if words[0] != VERSION {
        return None;
    }
    let flags = words[1];
    if flags & !KNOWN_FLAGS != 0 {
        return None;
    }
    let restore_rect = decode_rect(&words[2..6])?;
    Some(MaximizeRestoreState {
        restore_rect,
        promoted: flags & FLAG_PROMOTED != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> MinimizedRestoreRect {
        MinimizedRestoreRect { x, y, w, h }
    }

    #[test]
    fn maximize_restore_v1_round_trips_and_rejects_garbage() {
        let state = MaximizeRestoreState {
            restore_rect: rect(100, 80, 640, 480),
            promoted: false,
        };
        let words = encode_maximize_restore_v1(state).expect("encode");
        let decoded = decode_maximize_restore_v1(1u32, 1u32, 32, 0, &words).expect("decode");
        assert_eq!(decoded.restore_rect, state.restore_rect);
        assert!(!decoded.promoted);

        let promoted = MaximizeRestoreState {
            restore_rect: rect(-100, 40, 800, 600),
            promoted: true,
        };
        let words = encode_maximize_restore_v1(promoted).expect("encode promoted");
        let decoded = decode_maximize_restore_v1(1u32, 1u32, 32, 0, &words).expect("decode");
        assert!(decoded.promoted);
        assert_eq!(decoded.restore_rect.x, -100);

        assert!(
            decode_maximize_restore_v1(1u32, 1u32, 32, 0, &[1, 0, 1, 2, 3]).is_none(),
            "wrong length"
        );
        assert!(
            decode_maximize_restore_v1(1u32, 1u32, 32, 0, &[2, 0, 1, 2, 3, 4]).is_none(),
            "wrong version"
        );
        assert!(
            decode_maximize_restore_v1(1u32, 1u32, 32, 0, &[1, 1 << 7, 1, 2, 3, 4]).is_none(),
            "unknown flag"
        );
        assert!(
            encode_maximize_restore_v1(MaximizeRestoreState {
                restore_rect: rect(0, 0, 0, 0),
                promoted: false,
            })
            .is_none(),
            "zero size is not configurable"
        );
    }
}
