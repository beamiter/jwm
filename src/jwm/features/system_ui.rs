//! Backend-independent modal system UI state.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchEntry {
    pub name: String,
    pub command: Vec<String>,
    search: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorLayoutEntry {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
pub enum SystemUiState {
    #[default]
    Inactive,
    Launcher {
        query: String,
        entries: Vec<LaunchEntry>,
        matches: Vec<usize>,
        selected: usize,
    },
    Info {
        title: String,
        lines: Vec<String>,
        query: String,
        matches: Vec<usize>,
        offset: usize,
    },
    MonitorLayout {
        entries: Vec<MonitorLayoutEntry>,
        selected: usize,
        reference: usize,
        message: String,
    },
    Locked {
        password: String,
        message: String,
    },
}

impl Clone for SystemUiState {
    fn clone(&self) -> Self {
        match self {
            Self::Inactive => Self::Inactive,
            Self::Launcher {
                query,
                entries,
                matches,
                selected,
            } => Self::Launcher {
                query: query.clone(),
                entries: entries.clone(),
                matches: matches.clone(),
                selected: *selected,
            },
            Self::Info {
                title,
                lines,
                query,
                matches,
                offset,
            } => Self::Info {
                title: title.clone(),
                lines: lines.clone(),
                query: query.clone(),
                matches: matches.clone(),
                offset: *offset,
            },
            Self::MonitorLayout {
                entries,
                selected,
                reference,
                message,
            } => Self::MonitorLayout {
                entries: entries.clone(),
                selected: *selected,
                reference: *reference,
                message: message.clone(),
            },
            // Never duplicate credentials into another allocation.
            Self::Locked { message, .. } => Self::Locked {
                password: String::new(),
                message: message.clone(),
            },
        }
    }
}

impl SystemUiState {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Inactive)
    }
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::Locked { .. })
    }

    pub fn is_monitor_layout(&self) -> bool {
        matches!(self, Self::MonitorLayout { .. })
    }

    pub fn cancel(&mut self) {
        if let Self::Locked { password, .. } = self {
            // Keep the optimizer from eliding the overwrite before dropping.
            unsafe { password.as_bytes_mut().fill(0) };
        }
        *self = Self::Inactive;
    }

    pub fn open_launcher() -> Self {
        let entries = discover_applications();
        let matches = (0..entries.len()).collect();
        Self::Launcher {
            query: String::new(),
            entries,
            matches,
            selected: 0,
        }
    }

    pub fn lock() -> Self {
        Self::Locked {
            password: String::new(),
            message: String::new(),
        }
    }

    pub fn info(title: impl Into<String>, lines: Vec<String>) -> Self {
        let matches = (0..lines.len()).collect();
        Self::Info {
            title: title.into(),
            lines,
            query: String::new(),
            matches,
            offset: 0,
        }
    }

    #[must_use]
    pub fn monitor_layout(mut entries: Vec<MonitorLayoutEntry>) -> Self {
        normalize_monitor_positions(&mut entries);
        Self::MonitorLayout {
            entries,
            selected: 0,
            reference: 1,
            message: String::new(),
        }
    }

    pub fn cycle_monitor(&mut self, delta: isize) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        if entries.len() < 2 {
            return;
        }
        let previous = *selected;
        *selected = cycle_index(*selected, entries.len(), delta);
        if *reference == *selected {
            *reference = previous;
        }
        message.clear();
    }

    pub fn cycle_monitor_reference(&mut self, delta: isize) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        if entries.len() < 2 {
            return;
        }
        loop {
            *reference = cycle_index(*reference, entries.len(), delta);
            if *reference != *selected {
                break;
            }
        }
        message.clear();
    }

    pub fn place_monitor(&mut self, direction: MonitorDirection) {
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
        let Some(target) = entries.get_mut(*selected) else {
            return;
        };
        match direction {
            MonitorDirection::Left => {
                target.x = anchor.x - target.width;
                target.y = anchor.y;
            }
            MonitorDirection::Right => {
                target.x = anchor.x + anchor.width;
                target.y = anchor.y;
            }
            MonitorDirection::Above => {
                target.x = anchor.x;
                target.y = anchor.y - target.height;
            }
            MonitorDirection::Below => {
                target.x = anchor.x;
                target.y = anchor.y + anchor.height;
            }
        }
        normalize_monitor_positions(entries);
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
        let Self::MonitorLayout { entries, .. } = self else {
            return None;
        };
        let mut args = Vec::with_capacity(entries.len() * 4);
        for entry in entries {
            args.push("--output".into());
            args.push(entry.name.clone());
            args.push("--pos".into());
            args.push(format!("{}x{}", entry.x, entry.y));
        }
        Some(args)
    }

    pub fn monitor_layout_error(&mut self, error: impl Into<String>) {
        if let Self::MonitorLayout { message, .. } = self {
            *message = error.into();
        }
    }

    pub fn push_char(&mut self, ch: char) {
        match self {
            Self::Launcher { query, .. } | Self::Info { query, .. } => query.push(ch),
            Self::Locked { password, message } => {
                password.push(ch);
                message.clear();
            }
            Self::Inactive | Self::MonitorLayout { .. } => return,
        }
        self.refresh_matches();
    }

    pub fn backspace(&mut self) {
        match self {
            Self::Launcher { query, .. } | Self::Info { query, .. } => {
                query.pop();
            }
            Self::Locked { password, message } => {
                password.pop();
                message.clear();
            }
            Self::Inactive | Self::MonitorLayout { .. } => return,
        }
        self.refresh_matches();
    }

    pub fn move_selection(&mut self, delta: isize) {
        if let Self::Launcher {
            matches, selected, ..
        } = self
        {
            if matches.is_empty() {
                *selected = 0;
                return;
            }
            *selected = (*selected as isize + delta).rem_euclid(matches.len() as isize) as usize;
        } else if let Self::Info {
            matches, offset, ..
        } = self
        {
            let max = matches.len().saturating_sub(28);
            *offset = (*offset as isize + delta).clamp(0, max as isize) as usize;
        }
    }

    pub fn selected_command(&self) -> Option<Vec<String>> {
        let Self::Launcher {
            entries,
            matches,
            selected,
            ..
        } = self
        else {
            return None;
        };
        entries
            .get(*matches.get(*selected)?)
            .map(|entry| entry.command.clone())
    }

    pub fn take_password(&mut self) -> Option<String> {
        let Self::Locked { password, message } = self else {
            return None;
        };
        message.clear();
        Some(std::mem::take(password))
    }

    pub fn authentication_failed(&mut self) {
        if let Self::Locked { password, message } = self {
            unsafe { password.as_bytes_mut().fill(0) };
            password.clear();
            *message = "Authentication failed".into();
        }
    }

    pub fn overlay_text(&self) -> String {
        match self {
            Self::Inactive => String::new(),
            Self::Locked { password, message } => {
                let status = if message.is_empty() {
                    "Enter password to unlock"
                } else {
                    message
                };
                format!(
                    "\u{f023}  JWM LOCKED\n\n{status}\n\n\u{f084}  Password  {}",
                    "*".repeat(password.chars().count())
                )
            }
            Self::Launcher {
                query,
                entries,
                matches,
                selected,
            } => {
                let mut out = format!("\u{f135}  APPLICATIONS\n\n\u{f002}  {query}_\n\n");
                if matches.is_empty() {
                    out.push_str("  No matching applications");
                }
                let start = selected.saturating_sub(11);
                for (row, &index) in matches.iter().enumerate().skip(start).take(12) {
                    let marker = if row == *selected { "\u{f054}" } else { " " };
                    out.push_str(&format!("{marker} {}\n", entries[index].name));
                }
                out.push_str("\n\u{f2f6} Enter  launch    Esc  close    \u{f062}/\u{f063}  select");
                out
            }
            Self::Info {
                title,
                lines,
                query,
                matches,
                offset,
            } => {
                let mut out = format!("{title}\n\n\u{f002}  {query}_\n\n");
                if matches.is_empty() {
                    out.push_str("  No matching shortcuts\n");
                } else {
                    for &index in matches.iter().skip(*offset).take(28) {
                        out.push_str(&lines[index]);
                        out.push('\n');
                    }
                }
                out.push_str(
                    "\nType  search    Backspace  erase    Esc  close    \u{f062}/\u{f063}  scroll",
                );
                out
            }
            Self::MonitorLayout {
                entries,
                selected,
                reference,
                message,
            } => monitor_layout_overlay(entries, *selected, *reference, message),
        }
    }

    fn refresh_matches(&mut self) {
        match self {
            Self::Launcher {
                query,
                entries,
                matches,
                selected,
            } => {
                let needle = query.to_lowercase();
                let mut scored: Vec<(usize, usize)> = entries
                    .iter()
                    .enumerate()
                    .filter_map(|(i, entry)| {
                        fuzzy_score(&entry.search, &needle).map(|score| (i, score))
                    })
                    .collect();
                scored.sort_by_key(|&(i, score)| (Reverse(score), entries[i].name.to_lowercase()));
                *matches = scored.into_iter().map(|(i, _)| i).collect();
                *selected = 0;
            }
            Self::Info {
                query,
                lines,
                matches,
                offset,
                ..
            } => {
                let needle = query.to_lowercase();
                let mut scored: Vec<(usize, usize)> = lines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, line)| {
                        fuzzy_score(&line.to_lowercase(), &needle).map(|score| (i, score))
                    })
                    .collect();
                scored.sort_by_key(|&(i, score)| (Reverse(score), i));
                *matches = scored.into_iter().map(|(i, _)| i).collect();
                *offset = 0;
            }
            Self::Inactive | Self::Locked { .. } | Self::MonitorLayout { .. } => {}
        }
    }
}

fn cycle_index(index: usize, len: usize, delta: isize) -> usize {
    debug_assert!(len > 0);
    let distance = delta.unsigned_abs() % len;
    if delta.is_negative() {
        (index + len - distance) % len
    } else {
        (index + distance) % len
    }
}

fn normalize_monitor_positions(entries: &mut [MonitorLayoutEntry]) {
    let min_x = entries.iter().map(|entry| entry.x).min().unwrap_or(0);
    let min_y = entries.iter().map(|entry| entry.y).min().unwrap_or(0);
    if min_x == 0 && min_y == 0 {
        return;
    }
    for entry in entries {
        entry.x -= min_x;
        entry.y -= min_y;
    }
}

fn monitor_attachment(
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
        MonitorDirection::Left => ("left of", "vertical", target.y.saturating_sub(anchor.y)),
        MonitorDirection::Right => ("right of", "vertical", target.y.saturating_sub(anchor.y)),
        MonitorDirection::Above => ("above", "horizontal", target.x.saturating_sub(anchor.x)),
        MonitorDirection::Below => ("below", "horizontal", target.x.saturating_sub(anchor.x)),
    };
    Some(format!(
        "{} {side} {}; {axis} offset {offset:+} px",
        target.name, anchor.name
    ))
}

fn monitor_layout_overlay(
    entries: &[MonitorLayoutEntry],
    selected: usize,
    reference: usize,
    message: &str,
) -> String {
    let mut out = String::from("\u{f108}  DISPLAY LAYOUT\n\n");
    out.push_str(&monitor_layout_preview(entries, selected, reference));
    out.push('\n');
    if let Some(summary) = monitor_attachment_summary(entries, selected, reference) {
        writeln!(out, "\nLock: {summary}").expect("writing to a String cannot fail");
    }
    for (index, entry) in entries.iter().enumerate() {
        let target = if index == selected { '>' } else { ' ' };
        let anchor = if index == reference { '*' } else { ' ' };
        writeln!(
            out,
            "{target}{anchor} {}  {}x{}  @ {},{}",
            entry.name, entry.width, entry.height, entry.x, entry.y
        )
        .expect("writing to a String cannot fail");
    }
    if !message.is_empty() {
        writeln!(out, "\n! {message}").expect("writing to a String cannot fail");
    }
    out.push_str(
        "\nTab  target    [ / ]  reference    Arrow  attach side\nShift+Arrow  10px adjust    Ctrl+Arrow  1px adjust\nS / C / E  align start / center / end\nEnter  apply with xrandr    Esc  cancel",
    );
    out
}

fn monitor_layout_preview(
    entries: &[MonitorLayoutEntry],
    selected: usize,
    reference: usize,
) -> String {
    const WIDTH: usize = 52;
    const HEIGHT: usize = 10;
    let max_x = entries
        .iter()
        .map(|entry| entry.x.saturating_add(entry.width.max(1)))
        .max()
        .unwrap_or(1)
        .max(1);
    let max_y = entries
        .iter()
        .map(|entry| entry.y.saturating_add(entry.height.max(1)))
        .max()
        .unwrap_or(1)
        .max(1);
    let mut canvas = vec![vec![' '; WIDTH]; HEIGHT];

    // Draw the selected output last so its outline remains visible when the
    // current layout contains mirrored/overlapping outputs.
    let order = (0..entries.len())
        .filter(|&i| i != selected)
        .chain((selected < entries.len()).then_some(selected));
    for index in order {
        let entry = &entries[index];
        let x0 = scale_preview(entry.x, max_x, WIDTH);
        let y0 = scale_preview(entry.y, max_y, HEIGHT);
        let mut x1 = scale_preview(entry.x.saturating_add(entry.width), max_x, WIDTH);
        let mut y1 = scale_preview(entry.y.saturating_add(entry.height), max_y, HEIGHT);
        x1 = x1.max((x0 + 5).min(WIDTH - 1)).min(WIDTH - 1);
        y1 = y1.max((y0 + 2).min(HEIGHT - 1)).min(HEIGHT - 1);
        let horizontal = if index == selected { '=' } else { '-' };
        let vertical = if index == selected { '#' } else { '|' };
        canvas[y0][x0..=x1].fill(horizontal);
        canvas[y1][x0..=x1].fill(horizontal);
        for row in canvas.iter_mut().take(y1 + 1).skip(y0) {
            row[x0] = vertical;
            row[x1] = vertical;
        }
        for &(x, y) in &[(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
            canvas[y][x] = '+';
        }
        let marker = if index == selected {
            '>'
        } else if index == reference {
            '*'
        } else {
            char::from_digit(u32::try_from((index + 1).min(9)).unwrap_or(9), 10).unwrap_or('?')
        };
        let label = format!("{marker}{}", entry.name);
        for (offset, ch) in label.chars().take(x1.saturating_sub(x0 + 1)).enumerate() {
            canvas[(y0 + 1).min(y1)][x0 + 1 + offset] = ch;
        }
    }

    canvas
        .into_iter()
        .map(|row| row.into_iter().collect::<String>().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn scale_preview(value: i32, max: i32, extent: usize) -> usize {
    let extent_max = extent.saturating_sub(1);
    let value = u64::try_from(value.max(0)).unwrap_or(0);
    let max = u64::try_from(max.max(1)).unwrap_or(1);
    let extent_max_u64 = u64::try_from(extent_max).unwrap_or(u64::MAX);
    usize::try_from(value.saturating_mul(extent_max_u64) / max).unwrap_or(extent_max)
}

fn fuzzy_score(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(pos) = haystack.find(needle) {
        return Some(10_000 - pos);
    }
    let mut at = 0;
    let mut score = 0;
    for ch in needle.chars() {
        let rel = haystack[at..].find(ch)?;
        at += rel + ch.len_utf8();
        score += 100usize.saturating_sub(rel);
    }
    Some(score)
}

fn discover_applications() -> Vec<LaunchEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut roots = vec![data_home.join("applications")];
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    roots.extend(
        data_dirs
            .split(':')
            .map(|p| Path::new(p).join("applications")),
    );
    for root in roots {
        scan_desktop_dir(&root, &mut entries, &mut seen);
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let Ok(items) = fs::read_dir(dir) else {
                continue;
            };
            for item in items.flatten() {
                let name = item.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || !seen.insert(name.clone()) {
                    continue;
                }
                let Ok(meta) = item.metadata() else { continue };
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
                entries.push(LaunchEntry {
                    search: name.to_lowercase(),
                    name: name.clone(),
                    command: vec![name],
                });
            }
        }
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

fn scan_desktop_dir(root: &Path, entries: &mut Vec<LaunchEntry>, seen: &mut HashSet<String>) {
    let Ok(items) = fs::read_dir(root) else {
        return;
    };
    for item in items.flatten() {
        let path = item.path();
        if path.is_dir() {
            scan_desktop_dir(&path, entries, seen);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(body) = fs::read_to_string(path) else {
            continue;
        };
        let mut in_entry = false;
        let mut name = None;
        let mut exec = None;
        let mut hidden = false;
        for line in body.lines() {
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
                continue;
            }
            if !in_entry {
                continue;
            }
            if let Some(v) = line.strip_prefix("Name=") {
                name.get_or_insert_with(|| v.to_string());
            }
            if let Some(v) = line.strip_prefix("Exec=") {
                exec = Some(v.to_string());
            }
            if matches!(line, "Hidden=true" | "NoDisplay=true") {
                hidden = true;
            }
        }
        let (Some(name), Some(exec)) = (name, exec) else {
            continue;
        };
        if hidden || !seen.insert(name.clone()) {
            continue;
        }
        let command = parse_exec(&exec);
        if command.is_empty() {
            continue;
        }
        entries.push(LaunchEntry {
            search: format!("{} {}", name.to_lowercase(), exec.to_lowercase()),
            name,
            command,
        });
    }
}

fn parse_exec(exec: &str) -> Vec<String> {
    // Desktop Exec quoting is deliberately small but handles the common quoted
    // argv form. Field codes are omitted because no files/URLs were supplied.
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in exec.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None
            } else {
                current.push(ch)
            };
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args.into_iter()
        .filter(|arg| !arg.starts_with('%'))
        .collect()
}

// Minimal dynamically-loaded PAM client. dlopen keeps builds working on
// machines that have the PAM runtime (needed to log in) but not libpam headers.
pub fn authenticate_current_user(password: &str) -> bool {
    unsafe { authenticate_pam(password).unwrap_or(false) }
}

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}
#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}
#[repr(C)]
struct PamConv {
    conv: Option<
        unsafe extern "C" fn(
            c_int,
            *mut *const PamMessage,
            *mut *mut PamResponse,
            *mut c_void,
        ) -> c_int,
    >,
    appdata_ptr: *mut c_void,
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "C" fn pam_conversation(
    n: c_int,
    messages: *mut *const PamMessage,
    responses: *mut *mut PamResponse,
    data: *mut c_void,
) -> c_int {
    if n <= 0 || messages.is_null() || responses.is_null() {
        return 19;
    }
    let password = &*(data as *const CString);
    let out = libc::calloc(n as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
    if out.is_null() {
        return 5;
    }
    for i in 0..n as isize {
        let message = *messages.offset(i);
        if message.is_null() {
            libc::free(out.cast());
            return 19;
        }
        let value = match (*message).msg_style {
            1 => password.as_ptr(),
            2 => b"\0".as_ptr().cast(),
            3 | 4 => b"\0".as_ptr().cast(),
            _ => {
                libc::free(out.cast());
                return 19;
            }
        };
        (*out.offset(i)).resp = libc::strdup(value);
    }
    *responses = out;
    0
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn authenticate_pam(password: &str) -> Result<bool, ()> {
    let lib = libc::dlopen(c"libpam.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if lib.is_null() {
        return Err(());
    }
    macro_rules! sym {
        ($name:literal, $ty:ty) => {{
            let p = libc::dlsym(lib, concat!($name, "\0").as_ptr().cast());
            if p.is_null() {
                libc::dlclose(lib);
                return Err(());
            }
            std::mem::transmute::<*mut c_void, $ty>(p)
        }};
    }
    type Start = unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        *const PamConv,
        *mut *mut c_void,
    ) -> c_int;
    type Auth = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    type End = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    let start: Start = sym!("pam_start", Start);
    let auth: Auth = sym!("pam_authenticate", Auth);
    let end: End = sym!("pam_end", End);
    let pw = libc::getpwuid(libc::getuid());
    if pw.is_null() {
        libc::dlclose(lib);
        return Err(());
    }
    let user = CStr::from_ptr((*pw).pw_name);
    let password = CString::new(password).map_err(|_| ())?;
    let conv = PamConv {
        conv: Some(pam_conversation),
        appdata_ptr: (&password as *const CString).cast_mut().cast(),
    };
    let mut handle = std::ptr::null_mut();
    let mut result = start(c"login".as_ptr(), user.as_ptr(), &conv, &mut handle);
    if result == 0 {
        result = auth(handle, 0);
    }
    if !handle.is_null() {
        end(handle, result);
    }
    libc::dlclose(lib);
    Ok(result == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_desktop_exec() {
        assert_eq!(
            parse_exec("foo --name 'two words' %U"),
            ["foo", "--name", "two words"]
        );
    }
    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy_score("visual studio code", "vsc").is_some());
        assert!(fuzzy_score("firefox", "zzz").is_none());
    }

    #[test]
    fn info_search_filters_shortcut_and_description() {
        let mut state = SystemUiState::info(
            "KEYS",
            vec![
                "Mod1+j  focus next".into(),
                "Mod1+Return  terminal".into(),
                "Mod1+b  toggle bar".into(),
            ],
        );
        for ch in "term".chars() {
            state.push_char(ch);
        }
        let text = state.overlay_text();
        assert!(text.contains("Mod1+Return  terminal"));
        assert!(!text.contains("focus next"));
        state.backspace();
        assert!(state.overlay_text().contains("ter_"));
    }

    fn monitor(name: &str, x: i32, y: i32, width: i32, height: i32) -> MonitorLayoutEntry {
        MonitorLayoutEntry {
            name: name.into(),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn monitor_layout_places_target_relative_to_reference() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("eDP-1", 0, 0, 1920, 1080),
            monitor("HDMI-1", 0, 0, 2560, 1440),
        ]);

        state.place_monitor(MonitorDirection::Left);

        assert_eq!(
            state.monitor_layout_xrandr_args().unwrap(),
            [
                "--output", "eDP-1", "--pos", "0x0", "--output", "HDMI-1", "--pos", "1920x0",
            ]
        );
    }

    #[test]
    fn monitor_layout_cycles_target_without_using_it_as_reference() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("one", 0, 0, 100, 100),
            monitor("two", 100, 0, 100, 100),
            monitor("three", 200, 0, 100, 100),
        ]);

        state.cycle_monitor(1);
        state.place_monitor(MonitorDirection::Below);

        let text = state.overlay_text();
        assert!(text.contains(" * one  100x100  @ 0,0"));
        assert!(text.contains(">  two  100x100  @ 0,100"));
    }

    #[test]
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
                "--output", "eDP-1", "--pos", "2560x11", "--output", "HDMI-1", "--pos", "0x0",
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
                "--output", "eDP-1", "--pos", "2560x180", "--output", "HDMI-1", "--pos", "0x0",
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
        let state = SystemUiState::monitor_layout(vec![
            monitor("eDP-1", 0, 0, 1920, 1080),
            monitor("HDMI-1", 1920, 0, 2560, 1440),
        ]);
        let text = state.overlay_text();

        assert!(text.contains("DISPLAY LAYOUT"));
        assert!(text.contains(">  eDP-1"));
        assert!(text.contains(" * HDMI-1"));
        assert!(text.contains("apply with xrandr"));
    }
}
