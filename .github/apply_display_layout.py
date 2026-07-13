from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one marker, found {count}: {old[:80]!r}")
    path.write_text(text.replace(old, new, 1))


system_ui = Path("src/jwm/features/system_ui.rs")

replace_once(
    system_ui,
    """#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorDirection {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Debug, Default)]
""",
    """#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorDirection {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorAlignment {
    Start,
    Center,
    End,
}

#[derive(Debug, Default)]
""",
)

replace_once(
    system_ui,
    """        normalize_monitor_positions(entries);
        message.clear();
    }

    #[must_use]
    pub fn monitor_layout_xrandr_args(&self) -> Option<Vec<String>> {
""",
    """        normalize_monitor_positions(entries);
        message.clear();
    }

    /// Move the selected monitor along the cross axis while preserving its
    /// attached side relative to the reference monitor.
    pub fn fine_tune_monitor(&mut self, direction: MonitorDirection, pixels: i32) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        let Some(anchor) = entries.get(*reference).cloned() else {
            return;
        };
        let Some(target_snapshot) = entries.get(*selected).cloned() else {
            return;
        };
        let Some(attachment) = monitor_attachment(&target_snapshot, &anchor) else {
            *message = "Place the target with an arrow key before fine tuning".into();
            return;
        };
        let Some(target) = entries.get_mut(*selected) else {
            return;
        };
        let pixels = pixels.max(1);
        let adjusted = match (attachment, direction) {
            (MonitorDirection::Left | MonitorDirection::Right, MonitorDirection::Above) => {
                target.y = target.y.saturating_sub(pixels);
                true
            }
            (MonitorDirection::Left | MonitorDirection::Right, MonitorDirection::Below) => {
                target.y = target.y.saturating_add(pixels);
                true
            }
            (MonitorDirection::Above | MonitorDirection::Below, MonitorDirection::Left) => {
                target.x = target.x.saturating_sub(pixels);
                true
            }
            (MonitorDirection::Above | MonitorDirection::Below, MonitorDirection::Right) => {
                target.x = target.x.saturating_add(pixels);
                true
            }
            (MonitorDirection::Left | MonitorDirection::Right, _) => {
                *message = "Left/right attachment is locked; fine-tune with Up/Down".into();
                false
            }
            (MonitorDirection::Above | MonitorDirection::Below, _) => {
                *message = "Above/below attachment is locked; fine-tune with Left/Right".into();
                false
            }
        };
        if adjusted {
            normalize_monitor_positions(entries);
            message.clear();
        }
    }

    pub fn align_monitor_start(&mut self) {
        self.align_monitor(MonitorAlignment::Start);
    }

    pub fn align_monitor_center(&mut self) {
        self.align_monitor(MonitorAlignment::Center);
    }

    pub fn align_monitor_end(&mut self) {
        self.align_monitor(MonitorAlignment::End);
    }

    fn align_monitor(&mut self, alignment: MonitorAlignment) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        let Some(anchor) = entries.get(*reference).cloned() else {
            return;
        };
        let Some(target_snapshot) = entries.get(*selected).cloned() else {
            return;
        };
        let Some(attachment) = monitor_attachment(&target_snapshot, &anchor) else {
            *message = "Place the target with an arrow key before aligning".into();
            return;
        };
        let Some(target) = entries.get_mut(*selected) else {
            return;
        };
        match attachment {
            MonitorDirection::Left | MonitorDirection::Right => {
                target.y = aligned_position(anchor.y, anchor.height, target.height, alignment);
            }
            MonitorDirection::Above | MonitorDirection::Below => {
                target.x = aligned_position(anchor.x, anchor.width, target.width, alignment);
            }
        }
        normalize_monitor_positions(entries);
        message.clear();
    }

    #[must_use]
    pub fn monitor_layout_xrandr_args(&self) -> Option<Vec<String>> {
""",
)

replace_once(
    system_ui,
    """fn monitor_layout_overlay(
""",
    """fn monitor_attachment(
    target: &MonitorLayoutEntry,
    anchor: &MonitorLayoutEntry,
) -> Option<MonitorDirection> {
    if target.x.saturating_add(target.width) == anchor.x {
        Some(MonitorDirection::Left)
    } else if target.x == anchor.x.saturating_add(anchor.width) {
        Some(MonitorDirection::Right)
    } else if target.y.saturating_add(target.height) == anchor.y {
        Some(MonitorDirection::Above)
    } else if target.y == anchor.y.saturating_add(anchor.height) {
        Some(MonitorDirection::Below)
    } else {
        None
    }
}

fn aligned_position(
    anchor_start: i32,
    anchor_size: i32,
    target_size: i32,
    alignment: MonitorAlignment,
) -> i32 {
    match alignment {
        MonitorAlignment::Start => anchor_start,
        MonitorAlignment::Center => {
            anchor_start.saturating_add(anchor_size.saturating_sub(target_size) / 2)
        }
        MonitorAlignment::End => anchor_start
            .saturating_add(anchor_size)
            .saturating_sub(target_size),
    }
}

fn monitor_attachment_summary(
    entries: &[MonitorLayoutEntry],
    selected: usize,
    reference: usize,
) -> Option<String> {
    let target = entries.get(selected)?;
    let anchor = entries.get(reference)?;
    let attachment = monitor_attachment(target, anchor)?;
    let (side, axis, offset) = match attachment {
        MonitorDirection::Left => (
            "left of",
            "vertical",
            target.y.saturating_sub(anchor.y),
        ),
        MonitorDirection::Right => (
            "right of",
            "vertical",
            target.y.saturating_sub(anchor.y),
        ),
        MonitorDirection::Above => (
            "above",
            "horizontal",
            target.x.saturating_sub(anchor.x),
        ),
        MonitorDirection::Below => (
            "below",
            "horizontal",
            target.x.saturating_sub(anchor.x),
        ),
    };
    Some(format!(
        "{} {side} {}; {axis} offset {offset:+} px",
        target.name, anchor.name
    ))
}

fn monitor_layout_overlay(
""",
)

replace_once(
    system_ui,
    """    out.push_str(&monitor_layout_preview(entries, selected, reference));
    out.push('\n');
    for (index, entry) in entries.iter().enumerate() {
""",
    """    out.push_str(&monitor_layout_preview(entries, selected, reference));
    out.push('\n');
    if let Some(summary) = monitor_attachment_summary(entries, selected, reference) {
        writeln!(out, "\nLock: {summary}").expect("writing to a String cannot fail");
    }
    for (index, entry) in entries.iter().enumerate() {
""",
)

replace_once(
    system_ui,
    """    out.push_str(
        "\nTab  target    [ / ]  reference    Arrow keys  place\nEnter  apply with xrandr    Esc  cancel",
    );
""",
    """    out.push_str(
        "\nTab  target    [ / ]  reference    Arrow  attach side\nShift+Arrow  10px adjust    Ctrl+Arrow  1px adjust\nHome / C / End  align start / center / end\nEnter  apply with xrandr    Esc  cancel",
    );
""",
)

replace_once(
    system_ui,
    """    #[test]
    fn monitor_layout_preview_marks_target_and_reference() {
""",
    """    #[test]
    fn monitor_layout_keeps_horizontal_attachment_while_adjusting_vertical_offset() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("eDP-1", 0, 0, 1920, 1080),
            monitor("HDMI-1", 0, 0, 2560, 1440),
        ]);

        state.place_monitor(MonitorDirection::Right);
        state.fine_tune_monitor(MonitorDirection::Below, 10);
        state.fine_tune_monitor(MonitorDirection::Below, 1);

        assert_eq!(
            state.monitor_layout_xrandr_args().unwrap(),
            [
                "--output", "eDP-1", "--pos", "2560x11", "--output", "HDMI-1",
                "--pos", "0x0",
            ]
        );
        assert!(state.overlay_text().contains("vertical offset +11 px"));
    }

    #[test]
    fn monitor_layout_centers_different_height_outputs_on_cross_axis() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("eDP-1", 0, 0, 1920, 1080),
            monitor("HDMI-1", 0, 0, 2560, 1440),
        ]);

        state.place_monitor(MonitorDirection::Right);
        state.align_monitor_center();

        assert_eq!(
            state.monitor_layout_xrandr_args().unwrap(),
            [
                "--output", "eDP-1", "--pos", "2560x180", "--output", "HDMI-1",
                "--pos", "0x0",
            ]
        );
    }

    #[test]
    fn monitor_layout_rejects_adjustment_that_breaks_locked_axis() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("one", 0, 0, 100, 100),
            monitor("two", 100, 0, 100, 100),
        ]);

        state.place_monitor(MonitorDirection::Left);
        let before = state.monitor_layout_xrandr_args();
        state.fine_tune_monitor(MonitorDirection::Left, 10);

        assert_eq!(state.monitor_layout_xrandr_args(), before);
        assert!(state.overlay_text().contains("fine-tune with Up/Down"));
    }

    #[test]
    fn monitor_layout_preview_marks_target_and_reference() {
""",
)

input_handler = Path("src/jwm/input_handler.rs")

replace_once(
    input_handler,
    """            if self.features.system_ui.is_monitor_layout() {
                if keysym == keys::KEY_Tab || keysym == keys::KEY_ISO_Left_Tab {
""",
    """            if self.features.system_ui.is_monitor_layout() {
                let adjustment_step = if clean_state.contains(Mods::CONTROL) {
                    Some(1)
                } else if clean_state.contains(Mods::SHIFT) {
                    Some(10)
                } else {
                    None
                };
                let arrow_direction = match keysym {
                    keys::KEY_Left => Some(MonitorDirection::Left),
                    keys::KEY_Right => Some(MonitorDirection::Right),
                    keys::KEY_Up => Some(MonitorDirection::Above),
                    keys::KEY_Down => Some(MonitorDirection::Below),
                    _ => None,
                };
                if keysym == keys::KEY_Tab || keysym == keys::KEY_ISO_Left_Tab {
""",
)

replace_once(
    input_handler,
    """                } else if keysym == keys::KEY_Left {
                    self.features
                        .system_ui
                        .place_monitor(MonitorDirection::Left);
                } else if keysym == keys::KEY_Right {
                    self.features
                        .system_ui
                        .place_monitor(MonitorDirection::Right);
                } else if keysym == keys::KEY_Up {
                    self.features
                        .system_ui
                        .place_monitor(MonitorDirection::Above);
                } else if keysym == keys::KEY_Down {
                    self.features
                        .system_ui
                        .place_monitor(MonitorDirection::Below);
                } else if keysym == keys::KEY_Return {
""",
    """                } else if let (Some(step), Some(direction)) =
                    (adjustment_step, arrow_direction)
                {
                    self.features.system_ui.fine_tune_monitor(direction, step);
                } else if let Some(direction) = arrow_direction {
                    self.features.system_ui.place_monitor(direction);
                } else if keysym == keys::KEY_Home {
                    self.features.system_ui.align_monitor_start();
                } else if keysym == keys::KEY_c {
                    self.features.system_ui.align_monitor_center();
                } else if keysym == keys::KEY_End {
                    self.features.system_ui.align_monitor_end();
                } else if keysym == keys::KEY_Return {
""",
)
