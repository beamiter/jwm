//! Backend-independent lock screen and application launcher state.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchEntry {
    pub name: String,
    pub command: Vec<String>,
    search: String,
}

#[derive(Debug)]
pub enum SystemUiState {
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
        offset: usize,
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
                offset,
            } => Self::Info {
                title: title.clone(),
                lines: lines.clone(),
                offset: *offset,
            },
            // Never duplicate credentials into another allocation.
            Self::Locked { message, .. } => Self::Locked {
                password: String::new(),
                message: message.clone(),
            },
        }
    }
}

impl Default for SystemUiState {
    fn default() -> Self {
        Self::Inactive
    }
}

impl SystemUiState {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Inactive)
    }
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::Locked { .. })
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
        Self::Info {
            title: title.into(),
            lines,
            offset: 0,
        }
    }

    pub fn push_char(&mut self, ch: char) {
        match self {
            Self::Launcher { query, .. } => query.push(ch),
            Self::Locked { password, message } => {
                password.push(ch);
                message.clear();
            }
            Self::Inactive => return,
            Self::Info { .. } => return,
        }
        self.refresh_matches();
    }

    pub fn backspace(&mut self) {
        match self {
            Self::Launcher { query, .. } => {
                query.pop();
            }
            Self::Locked { password, message } => {
                password.pop();
                message.clear();
            }
            Self::Inactive => return,
            Self::Info { .. } => return,
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
        } else if let Self::Info { lines, offset, .. } = self {
            let max = lines.len().saturating_sub(1);
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
                offset,
            } => {
                let mut out = format!("{title}\n\n");
                for line in lines.iter().skip(*offset).take(28) {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str("\nEsc  close    \u{f062}/\u{f063}  scroll");
                out
            }
        }
    }

    fn refresh_matches(&mut self) {
        let Self::Launcher {
            query,
            entries,
            matches,
            selected,
        } = self
        else {
            return;
        };
        let needle = query.to_lowercase();
        let mut scored: Vec<(usize, usize)> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| fuzzy_score(&entry.search, &needle).map(|score| (i, score)))
            .collect();
        scored.sort_by_key(|&(i, score)| (Reverse(score), entries[i].name.to_lowercase()));
        *matches = scored.into_iter().map(|(i, _)| i).collect();
        *selected = 0;
    }
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
}
