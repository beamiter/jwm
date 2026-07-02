/// Entry for Expose/Mission Control mode.
pub struct ExposeEntry {
    pub x11_win: u32,
    pub orig_x: f32,
    pub orig_y: f32,
    pub orig_w: f32,
    pub orig_h: f32,
    pub target_x: f32,
    pub target_y: f32,
    pub target_w: f32,
    pub target_h: f32,
    pub current_x: f32,
    pub current_y: f32,
    pub current_w: f32,
    pub current_h: f32,
    pub is_hovered: bool,
}

/// Snap preview state.
pub struct SnapPreview {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub opacity: f32,
    pub start: std::time::Instant,
    pub fading_out: bool,
}

/// Single tab in a window group.
pub struct WindowTab {
    pub x11_win: u32,
    pub title: String,
    pub is_active: bool,
}

pub struct ExposeTickResult {
    pub keep_animating: bool,
    pub clear_entries: bool,
}

/// Compute Expose/Mission Control targets for a set of window rectangles.
pub fn build_expose_entries(
    screen_w: f32,
    screen_h: f32,
    gap: f32,
    windows: &[(u32, i32, i32, u32, u32)],
) -> Vec<ExposeEntry> {
    let n = windows.len();
    if n == 0 {
        return Vec::new();
    }

    let screen_aspect = screen_w / screen_h.max(1.0);
    let cols = ((n as f32 * screen_aspect).sqrt()).ceil() as u32;
    let cols = cols.max(1);
    let rows = ((n as u32 + cols - 1) / cols).max(1);

    let cell_w = (screen_w - gap * (cols as f32 + 1.0)) / cols as f32;
    let cell_h = (screen_h - gap * (rows as f32 + 1.0)) / rows as f32;

    windows
        .iter()
        .enumerate()
        .map(|(i, &(win, x, y, w, h))| {
            let col = i as u32 % cols;
            let row = i as u32 / cols;

            let cell_x = gap + col as f32 * (cell_w + gap);
            let cell_y = gap + row as f32 * (cell_h + gap);

            let win_aspect = w as f32 / h.max(1) as f32;
            let cell_aspect = cell_w / cell_h.max(1.0);
            let (tw, th) = if win_aspect > cell_aspect {
                (cell_w, cell_w / win_aspect)
            } else {
                (cell_h * win_aspect, cell_h)
            };
            let tx = cell_x + (cell_w - tw) * 0.5;
            let ty = cell_y + (cell_h - th) * 0.5;

            ExposeEntry {
                x11_win: win,
                orig_x: x as f32,
                orig_y: y as f32,
                orig_w: w as f32,
                orig_h: h as f32,
                target_x: tx,
                target_y: ty,
                target_w: tw,
                target_h: th,
                current_x: x as f32,
                current_y: y as f32,
                current_w: w as f32,
                current_h: h as f32,
                is_hovered: false,
            }
        })
        .collect()
}

pub fn tick_expose_entries(
    entries: &mut Vec<ExposeEntry>,
    active: bool,
    opacity: &mut f32,
    dt: f32,
) -> ExposeTickResult {
    if entries.is_empty() {
        return ExposeTickResult {
            keep_animating: false,
            clear_entries: false,
        };
    }

    let ease_speed = 12.0_f32;
    let t = 1.0 - (-ease_speed * dt).exp();
    let mut any_moving = false;

    if active {
        *opacity = (*opacity + dt * 4.0).min(1.0);
        if *opacity < 1.0 {
            any_moving = true;
        }

        for entry in entries.iter_mut() {
            any_moving |= tick_entry_towards(
                entry,
                entry.target_x,
                entry.target_y,
                entry.target_w,
                entry.target_h,
                t,
            );
        }
    } else {
        for entry in entries.iter_mut() {
            any_moving |= tick_entry_towards(
                entry,
                entry.orig_x,
                entry.orig_y,
                entry.orig_w,
                entry.orig_h,
                t,
            );
        }

        let fade_speed = if any_moving { 8.0 } else { 20.0 };
        *opacity = (*opacity - dt * fade_speed).max(0.0);
        if *opacity > 0.0 {
            any_moving = true;
        }

        if *opacity <= 0.0 && !any_moving {
            return ExposeTickResult {
                keep_animating: false,
                clear_entries: true,
            };
        }
    }

    ExposeTickResult {
        keep_animating: any_moving || (*opacity > 0.0 && *opacity < 1.0),
        clear_entries: false,
    }
}

fn tick_entry_towards(
    entry: &mut ExposeEntry,
    target_x: f32,
    target_y: f32,
    target_w: f32,
    target_h: f32,
    t: f32,
) -> bool {
    let dx = target_x - entry.current_x;
    let dy = target_y - entry.current_y;
    let dw = target_w - entry.current_w;
    let dh = target_h - entry.current_h;

    if dx.abs() > 0.5 || dy.abs() > 0.5 || dw.abs() > 0.5 || dh.abs() > 0.5 {
        entry.current_x += dx * t;
        entry.current_y += dy * t;
        entry.current_w += dw * t;
        entry.current_h += dh * t;
        true
    } else {
        entry.current_x = target_x;
        entry.current_y = target_y;
        entry.current_w = target_w;
        entry.current_h = target_h;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expose_layout_preserves_aspect_and_count() {
        let windows = vec![(1, 10, 20, 800, 400), (2, 50, 60, 300, 600)];
        let entries = build_expose_entries(1920.0, 1080.0, 20.0, &windows);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].x11_win, 1);
        assert_eq!(entries[0].orig_x, 10.0);
        assert_eq!(entries[0].orig_y, 20.0);
        assert!(entries[0].target_w > entries[0].target_h);
        assert!(entries[1].target_h > entries[1].target_w);
    }

    #[test]
    fn expose_tick_moves_towards_target_and_fades_in() {
        let windows = vec![(1, 0, 0, 800, 400)];
        let mut entries = build_expose_entries(1920.0, 1080.0, 20.0, &windows);
        let mut opacity = 0.0;

        let result = tick_expose_entries(&mut entries, true, &mut opacity, 1.0 / 60.0);

        assert!(result.keep_animating);
        assert!(!result.clear_entries);
        assert!(opacity > 0.0);
        assert!(entries[0].current_x > 0.0 || entries[0].current_y > 0.0);
    }

    #[test]
    fn expose_tick_requests_clear_after_fade_out_finishes() {
        let mut entries = vec![ExposeEntry {
            x11_win: 1,
            orig_x: 0.0,
            orig_y: 0.0,
            orig_w: 100.0,
            orig_h: 100.0,
            target_x: 0.0,
            target_y: 0.0,
            target_w: 100.0,
            target_h: 100.0,
            current_x: 0.0,
            current_y: 0.0,
            current_w: 100.0,
            current_h: 100.0,
            is_hovered: false,
        }];
        let mut opacity = 0.01;

        let result = tick_expose_entries(&mut entries, false, &mut opacity, 1.0 / 60.0);

        assert!(!result.keep_animating);
        assert!(result.clear_entries);
    }
}
