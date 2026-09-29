//! Strict wire codec for JWM's private hand-float restart property.
//!
//! `_JWM_FLOATING_V1` is stored as exactly 6 CARDINAL/32 values:
//!
//! ```text
//!  0 version (= 1)
//!  1 flags (KNOWN_FLAGS = 0 for now)
//!  2..=5 floating x/y/w/h
//! ```
//!
//! A visible hand-floated window carries its float rectangle here so a
//! seamless X11 exec can re-admit the same layout membership the previous
//! process held. Coordinates use their two's-complement bit pattern so
//! negative-origin output layouts round-trip through CARDINAL.

use crate::backend::api::{FloatingRestoreState, MinimizedRestoreRect};

pub(crate) const FLOATING_RESTORE_V1_WORD_COUNT: usize = 6;
pub(crate) const FLOATING_RESTORE_V1_LONG_LENGTH: u32 = 6;

const VERSION: u32 = 1;
const KNOWN_FLAGS: u32 = 0;

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

/// Encode one hand-float snapshot. Rejects rectangles no X server could be
/// asked for rather than emitting a property the next process cannot adopt.
pub(crate) fn encode_floating_restore_v1(
    state: FloatingRestoreState,
) -> Option<[u32; FLOATING_RESTORE_V1_WORD_COUNT]> {
    let rect = encode_rect(state.floating_rect)?;
    Some([VERSION, KNOWN_FLAGS, rect[0], rect[1], rect[2], rect[3]])
}

/// Decode a property reply. Wrong type/format/length or unknown flags →
/// `None` so an untrusted client property cannot block window adoption.
pub(crate) fn decode_floating_restore_v1<A: Copy + Eq>(
    type_: A,
    cardinal: A,
    format: u8,
    bytes_after: u32,
    words: &[u32],
) -> Option<FloatingRestoreState> {
    if type_ != cardinal || format != 32 || bytes_after != 0 {
        return None;
    }
    if words.len() != FLOATING_RESTORE_V1_WORD_COUNT {
        return None;
    }
    if words[0] != VERSION {
        return None;
    }
    let flags = words[1];
    if flags & !KNOWN_FLAGS != 0 {
        return None;
    }
    let floating_rect = decode_rect(&words[2..6])?;
    Some(FloatingRestoreState { floating_rect })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> MinimizedRestoreRect {
        MinimizedRestoreRect { x, y, w, h }
    }

    #[test]
    fn floating_restore_v1_round_trips_and_rejects_garbage() {
        let state = FloatingRestoreState {
            floating_rect: rect(100, 80, 640, 480),
        };
        let words = encode_floating_restore_v1(state).expect("encode");
        let decoded = decode_floating_restore_v1(1u32, 1u32, 32, 0, &words).expect("decode");
        assert_eq!(decoded.floating_rect, state.floating_rect);

        let negative = FloatingRestoreState {
            floating_rect: rect(-100, 40, 800, 600),
        };
        let words = encode_floating_restore_v1(negative).expect("encode negative");
        let decoded = decode_floating_restore_v1(1u32, 1u32, 32, 0, &words).expect("decode");
        assert_eq!(decoded.floating_rect.x, -100);

        assert!(
            decode_floating_restore_v1(1u32, 1u32, 32, 0, &[1, 0, 1, 2, 3]).is_none(),
            "wrong length"
        );
        assert!(
            decode_floating_restore_v1(1u32, 1u32, 32, 0, &[2, 0, 1, 2, 3, 4]).is_none(),
            "wrong version"
        );
        assert!(
            decode_floating_restore_v1(1u32, 1u32, 32, 0, &[1, 1 << 0, 1, 2, 3, 4]).is_none(),
            "unknown flag"
        );
        assert!(
            encode_floating_restore_v1(FloatingRestoreState {
                floating_rect: rect(0, 0, 0, 0),
            })
            .is_none(),
            "zero size is not configurable"
        );
    }
}
