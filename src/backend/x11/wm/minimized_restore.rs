//! Strict wire codec for JWM's private minimized-client restart property.
//!
//! `_JWM_MINIMIZED_RESTORE_V1` is stored as exactly 18 CARDINAL/32 values:
//!
//! ```text
//!  0 version (= 1)
//!  1 flags
//!  2 tags
//!  3 monitor number (two's-complement i32 bits)
//!  4..=7 visible x/y/w/h
//!  8..=11 remembered floating x/y/w/h, or all zero
//! 12..=15 pre-fullscreen x/y/w/h, or all zero
//! 16 minimized order low 32 bits
//! 17 minimized order high 32 bits
//! ```
//!
//! Coordinates use their two's-complement bit pattern so negative-origin
//! output layouts round-trip exactly through X11's unsigned CARDINAL type.

use crate::backend::api::{
    MAX_MINIMIZED_RESTORE_ORDER, MinimizedRestoreRect, MinimizedRestoreState,
};

pub(crate) const MINIMIZED_RESTORE_V1_WORD_COUNT: usize = 18;
pub(crate) const MINIMIZED_RESTORE_V1_LONG_LENGTH: u32 = 18;

const VERSION: u32 = 1;
const FLAG_FLOATING: u32 = 1 << 0;
const FLAG_DRAG_FLOATING: u32 = 1 << 1;
const FLAG_HAS_FLOATING_RECT: u32 = 1 << 2;
const FLAG_PIP: u32 = 1 << 3;
const FLAG_OLD_STATE: u32 = 1 << 4;
const FLAG_HAS_FULLSCREEN_RESTORE: u32 = 1 << 5;
const FLAG_PIP_RESTORE_STICKY: u32 = 1 << 6;
const KNOWN_FLAGS: u32 = FLAG_FLOATING
    | FLAG_DRAG_FLOATING
    | FLAG_HAS_FLOATING_RECT
    | FLAG_PIP
    | FLAG_OLD_STATE
    | FLAG_HAS_FULLSCREEN_RESTORE
    | FLAG_PIP_RESTORE_STICKY;

#[inline]
fn encode_i32_bits(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}

#[inline]
fn decode_i32_bits(value: u32) -> i32 {
    i32::from_ne_bytes(value.to_ne_bytes())
}

fn encode_rect(rect: MinimizedRestoreRect) -> Option<[u32; 4]> {
    let w = u32::try_from(rect.w).ok()?;
    let h = u32::try_from(rect.h).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some([encode_i32_bits(rect.x), encode_i32_bits(rect.y), w, h])
}

fn decode_rect(words: &[u32]) -> Option<MinimizedRestoreRect> {
    let [x, y, w, h] = words else {
        return None;
    };
    let w = i32::try_from(*w).ok()?;
    let h = i32::try_from(*h).ok()?;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(MinimizedRestoreRect {
        x: decode_i32_bits(*x),
        y: decode_i32_bits(*y),
        w,
        h,
    })
}

fn decode_optional_rect(words: &[u32], present: bool) -> Option<Option<MinimizedRestoreRect>> {
    if present {
        return decode_rect(words).map(Some);
    }
    words.iter().all(|word| *word == 0).then_some(None)
}

/// Encode one semantic snapshot. Invalid in-memory combinations are rejected
/// rather than emitting a property that the next JWM process cannot adopt.
pub(crate) fn encode_minimized_restore_v1(
    state: MinimizedRestoreState,
) -> Option<[u32; MINIMIZED_RESTORE_V1_WORD_COUNT]> {
    if state.minimized_order == 0
        || state.minimized_order > MAX_MINIMIZED_RESTORE_ORDER
        || (state.is_drag_floating && !state.is_floating)
        || (state.is_pip && (!state.is_floating || state.floating_rect.is_none()))
        || (state.is_pip && state.fullscreen_restore_rect.is_some())
        || (!state.is_pip && state.pip_restore_sticky)
        || (state.fullscreen_restore_rect.is_some() && !state.is_floating)
    {
        return None;
    }

    let visible = encode_rect(state.visible_rect)?;
    let floating = match state.floating_rect {
        Some(rect) => encode_rect(rect)?,
        None => [0; 4],
    };
    let fullscreen_restore = match state.fullscreen_restore_rect {
        Some(rect) => encode_rect(rect)?,
        None => [0; 4],
    };

    let mut flags = 0;
    if state.is_floating {
        flags |= FLAG_FLOATING;
    }
    if state.is_drag_floating {
        flags |= FLAG_DRAG_FLOATING;
    }
    if state.floating_rect.is_some() {
        flags |= FLAG_HAS_FLOATING_RECT;
    }
    if state.is_pip {
        flags |= FLAG_PIP;
    }
    if state.old_state {
        flags |= FLAG_OLD_STATE;
    }
    if state.fullscreen_restore_rect.is_some() {
        flags |= FLAG_HAS_FULLSCREEN_RESTORE;
    }
    if state.pip_restore_sticky {
        flags |= FLAG_PIP_RESTORE_STICKY;
    }

    let order_low = u32::try_from(state.minimized_order & u64::from(u32::MAX)).ok()?;
    let order_high = u32::try_from(state.minimized_order >> 32).ok()?;
    Some([
        VERSION,
        flags,
        state.tags,
        encode_i32_bits(state.monitor_num),
        visible[0],
        visible[1],
        visible[2],
        visible[3],
        floating[0],
        floating[1],
        floating[2],
        floating[3],
        fullscreen_restore[0],
        fullscreen_restore[1],
        fullscreen_restore[2],
        fullscreen_restore[3],
        order_low,
        order_high,
    ])
}

/// Validate the complete X11 reply envelope and decode its V1 payload.
///
/// A property is accepted only when it is CARDINAL/32, was fetched in full,
/// and has the exact versioned length. The caller should map every malformed
/// or absent property to `Ok(None)`; only transport failures are errors.
pub(crate) fn decode_minimized_restore_v1<A: Copy + Eq>(
    actual_type: A,
    cardinal_type: A,
    format: u8,
    bytes_after: u32,
    words: &[u32],
) -> Option<MinimizedRestoreState> {
    if actual_type != cardinal_type
        || format != 32
        || bytes_after != 0
        || words.len() != MINIMIZED_RESTORE_V1_WORD_COUNT
        || words[0] != VERSION
    {
        return None;
    }

    let flags = words[1];
    if flags & !KNOWN_FLAGS != 0 {
        return None;
    }
    let is_floating = flags & FLAG_FLOATING != 0;
    let is_drag_floating = flags & FLAG_DRAG_FLOATING != 0;
    if is_drag_floating && !is_floating {
        return None;
    }

    let visible_rect = decode_rect(&words[4..8])?;
    let floating_rect = decode_optional_rect(&words[8..12], flags & FLAG_HAS_FLOATING_RECT != 0)?;
    let fullscreen_restore_rect =
        decode_optional_rect(&words[12..16], flags & FLAG_HAS_FULLSCREEN_RESTORE != 0)?;
    let minimized_order = u64::from(words[16]) | (u64::from(words[17]) << 32);
    if minimized_order == 0 || minimized_order > MAX_MINIMIZED_RESTORE_ORDER {
        return None;
    }
    let is_pip = flags & FLAG_PIP != 0;
    if is_pip && (!is_floating || floating_rect.is_none()) {
        return None;
    }
    let pip_restore_sticky = flags & FLAG_PIP_RESTORE_STICKY != 0;
    if (is_pip && fullscreen_restore_rect.is_some()) || (!is_pip && pip_restore_sticky) {
        return None;
    }
    if fullscreen_restore_rect.is_some() && !is_floating {
        return None;
    }

    Some(MinimizedRestoreState {
        tags: words[2],
        monitor_num: decode_i32_bits(words[3]),
        visible_rect,
        is_floating,
        is_drag_floating,
        floating_rect,
        is_pip,
        pip_restore_sticky,
        old_state: flags & FLAG_OLD_STATE != 0,
        fullscreen_restore_rect,
        minimized_order,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDINAL: u32 = 6;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> MinimizedRestoreRect {
        MinimizedRestoreRect { x, y, w, h }
    }

    fn snapshot() -> MinimizedRestoreState {
        MinimizedRestoreState {
            tags: 0b101,
            monitor_num: -2,
            visible_rect: rect(-3840, -120, 1920, 1080),
            is_floating: true,
            is_drag_floating: true,
            floating_rect: Some(rect(-3700, 48, 900, 700)),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: true,
            fullscreen_restore_rect: Some(rect(-3600, 64, 1280, 800)),
            minimized_order: 0x0123_4567_89ab_cdef,
        }
    }

    fn pip_snapshot() -> MinimizedRestoreState {
        MinimizedRestoreState {
            is_pip: true,
            pip_restore_sticky: true,
            old_state: false,
            fullscreen_restore_rect: None,
            ..snapshot()
        }
    }

    fn decode(words: &[u32]) -> Option<MinimizedRestoreState> {
        decode_minimized_restore_v1(CARDINAL, CARDINAL, 32, 0, words)
    }

    #[test]
    fn semantic_snapshot_round_trips_with_negative_coordinates_and_u64_order() {
        let state = snapshot();
        let words = encode_minimized_restore_v1(state).expect("valid snapshot");
        assert_eq!(words.len(), MINIMIZED_RESTORE_V1_WORD_COUNT);
        assert_eq!(words[3], encode_i32_bits(-2));
        assert_eq!(words[4], encode_i32_bits(-3840));
        assert_eq!(words[16], 0x89ab_cdef);
        assert_eq!(words[17], 0x0123_4567);
        assert_eq!(decode(&words), Some(state));

        let pip = pip_snapshot();
        let pip_words = encode_minimized_restore_v1(pip).expect("valid PiP snapshot");
        assert_ne!(pip_words[1] & FLAG_PIP_RESTORE_STICKY, 0);
        assert_eq!(decode(&pip_words), Some(pip));
    }

    #[test]
    fn full_i32_coordinate_domain_is_bit_preserving() {
        for value in [i32::MIN, -1, 0, 1, i32::MAX] {
            assert_eq!(decode_i32_bits(encode_i32_bits(value)), value);
        }
    }

    #[test]
    fn absent_optional_rectangles_have_a_canonical_zero_encoding() {
        let mut state = snapshot();
        state.is_drag_floating = false;
        state.is_pip = false;
        state.floating_rect = None;
        state.fullscreen_restore_rect = None;
        let words = encode_minimized_restore_v1(state).expect("valid snapshot");
        assert_eq!(&words[8..16], &[0; 8]);
        assert_eq!(decode(&words), Some(state));
    }

    #[test]
    fn reply_envelope_must_be_exact_and_complete() {
        let words = encode_minimized_restore_v1(snapshot()).expect("valid snapshot");
        assert!(decode_minimized_restore_v1(7, CARDINAL, 32, 0, &words).is_none());
        assert!(decode_minimized_restore_v1(CARDINAL, CARDINAL, 8, 0, &words).is_none());
        assert!(decode_minimized_restore_v1(CARDINAL, CARDINAL, 32, 4, &words).is_none());
        assert!(decode(&words[..MINIMIZED_RESTORE_V1_WORD_COUNT - 1]).is_none());
        let mut extended = words.to_vec();
        extended.push(0);
        assert!(decode(&extended).is_none());
    }

    #[test]
    fn version_flags_and_cross_field_invariants_are_strict() {
        let words = encode_minimized_restore_v1(snapshot()).expect("valid snapshot");

        let mut wrong_version = words;
        wrong_version[0] = 2;
        assert!(decode(&wrong_version).is_none());

        let mut unknown_flag = words;
        unknown_flag[1] |= 1 << 31;
        assert!(decode(&unknown_flag).is_none());

        let mut drag_without_float = words;
        drag_without_float[1] &= !FLAG_FLOATING;
        drag_without_float[1] |= FLAG_DRAG_FLOATING;
        assert!(decode(&drag_without_float).is_none());

        let pip_words = encode_minimized_restore_v1(pip_snapshot()).expect("valid PiP snapshot");

        let mut pip_without_float = pip_words;
        pip_without_float[1] &= !(FLAG_FLOATING | FLAG_DRAG_FLOATING);
        assert!(decode(&pip_without_float).is_none());

        let mut pip_without_floating_rect = pip_words;
        pip_without_floating_rect[1] &= !FLAG_HAS_FLOATING_RECT;
        pip_without_floating_rect[8..12].fill(0);
        assert!(decode(&pip_without_floating_rect).is_none());

        let mut pip_and_fullscreen = pip_words;
        pip_and_fullscreen[1] |= FLAG_HAS_FULLSCREEN_RESTORE;
        pip_and_fullscreen[12..16].copy_from_slice(&words[12..16]);
        assert!(decode(&pip_and_fullscreen).is_none());

        let mut sticky_restore_without_pip = words;
        sticky_restore_without_pip[1] |= FLAG_PIP_RESTORE_STICKY;
        assert!(decode(&sticky_restore_without_pip).is_none());

        let mut zero_order = words;
        zero_order[16] = 0;
        zero_order[17] = 0;
        assert!(decode(&zero_order).is_none());

        let mut allocator_poisoning_order = words;
        allocator_poisoning_order[16] = u32::MAX;
        allocator_poisoning_order[17] = u32::MAX;
        assert!(decode(&allocator_poisoning_order).is_none());
    }

    #[test]
    fn every_present_rectangle_requires_bounded_positive_dimensions() {
        let words = encode_minimized_restore_v1(snapshot()).expect("valid snapshot");
        for dimension in [6usize, 7, 10, 11, 14, 15] {
            let mut zero = words;
            zero[dimension] = 0;
            assert!(decode(&zero).is_none(), "dimension word {dimension}");

            let mut too_large = words;
            too_large[dimension] = (i32::MAX as u32) + 1;
            assert!(decode(&too_large).is_none(), "dimension word {dimension}");
        }
    }

    #[test]
    fn absent_optional_rectangles_reject_noncanonical_payload_words() {
        let mut state = snapshot();
        state.is_pip = false;
        state.is_drag_floating = false;
        state.floating_rect = None;
        state.fullscreen_restore_rect = None;
        let words = encode_minimized_restore_v1(state).expect("valid snapshot");
        for index in 8..16 {
            let mut malformed = words;
            malformed[index] = 1;
            assert!(decode(&malformed).is_none(), "optional word {index}");
        }
    }

    #[test]
    fn encoder_refuses_states_that_cannot_form_a_valid_v1_property() {
        let mut state = snapshot();
        state.minimized_order = 0;
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = snapshot();
        state.minimized_order = u64::MAX;
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = snapshot();
        state.is_floating = false;
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = snapshot();
        state.is_drag_floating = false;
        state.is_floating = false;
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = pip_snapshot();
        state.floating_rect = None;
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = pip_snapshot();
        state.fullscreen_restore_rect = Some(rect(0, 0, 100, 100));
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = snapshot();
        state.pip_restore_sticky = true;
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = snapshot();
        state.visible_rect.w = 0;
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = snapshot();
        state.floating_rect = Some(rect(0, 0, -1, 1));
        assert!(encode_minimized_restore_v1(state).is_none());

        let mut state = snapshot();
        state.fullscreen_restore_rect = Some(rect(0, 0, 1, 0));
        assert!(encode_minimized_restore_v1(state).is_none());
    }
}
