//! What the application launcher knows besides the list of applications.
//!
//! Two things separate a launcher people keep using from one they abandon:
//! it puts what they actually run at the top, and it answers a small question
//! without making them open something else first. Both are arithmetic — one
//! over usage counts, one over the query itself — so both live here, pure and
//! tested, rather than tangled into the panel that draws them.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Where the usage counts live, under the user's data directory.
pub const USAGE_FILE: &str = "launcher-usage";

/// Most launches remembered. Beyond this the least useful entries are
/// dropped: a usage file that grows forever would eventually cost more to
/// read than the ranking is worth.
pub const MAX_TRACKED: usize = 500;

/// One KiB per retained entry is generous for desktop-file IDs while keeping
/// a damaged or special usage source cheap to reject.
const MAX_USAGE_BYTES: u64 = MAX_TRACKED as u64 * 1024;

/// Launches beyond this stop increasing an entry's rank, so an editor opened
/// ten thousand times cannot make everything else unreachable for a month
/// after you stop using it.
const COUNT_CAP: u32 = 50;
static USAGE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn read_usage(path: &Path) -> io::Result<String> {
    // O_NONBLOCK matters for a path unexpectedly replaced with a FIFO: the
    // regular-file check can then reject it without waiting for a writer.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "launcher usage source is not a regular file: {}",
                path.display()
            ),
        ));
    }
    if metadata.len() > MAX_USAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "launcher usage file exceeds its size limit",
        ));
    }

    // Keep the descriptor bounded too: metadata may be stale by the time the
    // read starts. The sentinel byte distinguishes an exact-limit file.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_USAGE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_USAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "launcher usage file exceeds its size limit",
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn atomic_write_usage(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let sequence = USAGE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{USAGE_FILE}.tmp-{}-{sequence}",
        std::process::id()
    ));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        // `mode` is still filtered through the process umask. Set the final
        // private mode explicitly before the inode becomes visible at `path`.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

// -------------------------------------------------------------------------
// Frecency
// -------------------------------------------------------------------------

/// How often something was launched, and when it last was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub count: u32,
    /// Unix seconds.
    pub last_used: u64,
}

/// Launch history behind the launcher's ranking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageStore {
    entries: HashMap<String, Usage>,
}

impl UsageStore {
    /// Read the store from its file format: `count last_used id`, one per
    /// line. Malformed lines are skipped rather than failing the load — a
    /// corrupt ranking file must not cost the user their launcher.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut entries = HashMap::new();
        for line in text.lines() {
            let mut fields = line.splitn(3, ' ');
            let (Some(count), Some(last_used), Some(id)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let (Ok(count), Ok(last_used)) = (count.parse::<u32>(), last_used.parse::<u64>())
            else {
                continue;
            };
            // The id is last precisely because application names contain
            // spaces; everything after the second field is the id.
            if id.is_empty() {
                continue;
            }
            entries.insert(id.to_string(), Usage { count, last_used });
        }
        Self { entries }
    }

    /// Serialize, keeping only the [`MAX_TRACKED`] best-ranked entries.
    #[must_use]
    pub fn serialize(&self, now: u64) -> String {
        let mut kept: Vec<(&String, &Usage)> = self.entries.iter().collect();
        kept.sort_by(|(left_id, left), (right_id, right)| {
            score(right, now)
                .cmp(&score(left, now))
                .then(left_id.cmp(right_id))
        });
        kept.truncate(MAX_TRACKED);
        // Sorted by id on disk so a diff of the file is readable and a save
        // that changed nothing produces the same bytes.
        kept.sort_by(|(left_id, _), (right_id, _)| left_id.cmp(right_id));
        let mut out = String::new();
        for (id, usage) in kept {
            use std::fmt::Write as _;
            let _ = writeln!(out, "{} {} {id}", usage.count, usage.last_used);
        }
        out
    }

    /// Note that `id` was just launched.
    pub fn record(&mut self, id: &str, now: u64) {
        let usage = self.entries.entry(id.to_string()).or_default();
        usage.count = usage.count.saturating_add(1);
        usage.last_used = now;
    }

    /// How strongly `id` should be preferred, 0 when it has never been used.
    #[must_use]
    pub fn score(&self, id: &str, now: u64) -> u32 {
        self.entries.get(id).map_or(0, |usage| score(usage, now))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read the store from disk. A missing or unreadable file is an empty
    /// history, not an error: the launcher works without a ranking.
    #[must_use]
    pub fn load() -> Self {
        Self::load_from_path(&usage_path())
    }

    fn load_from_path(path: &Path) -> Self {
        read_usage(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Write the store back. Failures are logged and dropped — losing a
    /// ranking update is not worth interrupting a launch over.
    pub fn save(&self, now: u64) {
        let path = usage_path();
        let serialized = self.serialize(now);
        if let Err(error) = atomic_write_usage(&path, serialized.as_bytes()) {
            log::debug!("launcher: {}: {error}", path.display());
        }
    }
}

/// Frecency: recency in coarse buckets, multiplied by a capped launch count.
///
/// Buckets rather than a continuous decay because the ranking only has to be
/// stable and explicable — "used today" beating "used last month" is the
/// whole requirement, and a bucket boundary is far easier to reason about
/// than a half-life when a row moves and the user wonders why.
fn score(usage: &Usage, now: u64) -> u32 {
    let age = now.saturating_sub(usage.last_used);
    let weight = match age {
        0..3_600 => 100,
        3_600..86_400 => 70,
        86_400..604_800 => 50,
        604_800..2_592_000 => 30,
        _ => 10,
    };
    usage.count.min(COUNT_CAP) * weight
}

fn usage_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("jwm").join(USAGE_FILE)
}

/// Seconds since the Unix epoch, for callers that only need "now".
#[must_use]
pub fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

// -------------------------------------------------------------------------
// Arithmetic
// -------------------------------------------------------------------------

/// Evaluate `query` as arithmetic, or return `None` if it is not.
///
/// An operator is required: a query of `42` is somebody looking for an
/// application, not asking what 42 is. Division by zero returns `None` too —
/// an `inf` in the panel answers nothing.
#[must_use]
pub fn evaluate(query: &str) -> Option<f64> {
    let trimmed = query.trim().trim_start_matches('=').trim();
    if trimmed.is_empty() || !trimmed.contains(['+', '-', '*', '/', '%', '^']) {
        return None;
    }
    let tokens = tokenize(trimmed)?;
    let mut parser = Parser { tokens, at: 0 };
    let value = parser.expression(0)?;
    if parser.at != parser.tokens.len() || !value.is_finite() {
        return None;
    }
    Some(value)
}

/// Render a result without the noise of binary floating point: `0.30000000004`
/// is a correct answer to `0.1+0.2` and a useless thing to show.
#[must_use]
pub fn format_result(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let rounded = format!("{value:.10}");
    let trimmed = rounded.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

// -------------------------------------------------------------------------
// Windows
// -------------------------------------------------------------------------

/// Longest window title shown. The card sizes itself to its widest line, so
/// one pathological title would otherwise push the panel off the screen.
const MAX_TITLE_CHARS: usize = 52;

/// Longest class shown beside a title, for the same reason: `WM_CLASS` is
/// the client's to set and the X server hands over up to a kilobyte of it.
const MAX_CLASS_CHARS: usize = 24;

/// The prefix that asks for windows and nothing else.
///
/// A leading slash cannot collide with the calculator: `evaluate` parses no
/// expression that begins with an operator, so `/1+1` filters windows rather
/// than computing two.
pub const WINDOWS_PREFIX: char = '/';

/// One already-open window, flattened out of the window manager's client
/// table so the ranking is a unit test rather than a live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntry {
    /// The handle a focus takes.
    pub id: u64,
    pub title: String,
    pub class: String,
    pub instance: String,
    /// Zero-based tag index; `None` for a sticky window, which is on all of
    /// them.
    pub tag: Option<usize>,
    pub monitor: i32,
    /// Showing on its own monitor: on one of that monitor's active tags, and
    /// not minimised. Deliberately not "on the screen the user is looking
    /// at" — a window can be plainly visible on the other head, and calling
    /// that hidden is what made rows claim a tag the user was already on.
    pub visible: bool,
    /// On the monitor the user is looking at.
    pub on_selected_monitor: bool,
    pub minimized: bool,
}

/// What the ranker needs about one application, borrowed rather than copied.
#[derive(Debug, Clone, Copy)]
pub struct AppCandidate<'a> {
    /// Lowercased name and command, the haystack the entry already keeps.
    pub search: &'a str,
    /// Precomputed case-insensitive display-name key used only as the final
    /// deterministic tie-breaker. Borrowing it keeps sorting allocation-free.
    pub sort_key: &'a str,
    /// This entry's frecency, looked up by the caller.
    pub usage: u32,
}

/// A row of the merged list: an index into the windows, or into the
/// applications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherRow {
    Window(usize),
    App(usize),
}

/// What the query is asking for, decided once so the panel and the ranker
/// cannot disagree about which mode is showing.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryMode {
    /// Arithmetic: the answer replaces the list.
    Answer(f64),
    /// A leading slash: open windows only, matched on the rest.
    Windows(String),
    /// Applications, joined by open windows once something is typed.
    Search(String),
}

/// Read the query.
#[must_use]
pub fn parse_query(query: &str) -> QueryMode {
    if let Some(value) = evaluate(query) {
        return QueryMode::Answer(value);
    }
    match query.trim_start().strip_prefix(WINDOWS_PREFIX) {
        Some(rest) => QueryMode::Windows(rest.trim().to_lowercase()),
        None => QueryMode::Search(query.trim().to_lowercase()),
    }
}

/// How well a window matches, taking the best of its title, class and
/// instance.
///
/// The three are scored separately and never concatenated: a haystack of
/// `"github firefox"` would let `foxgithub` match something that is in
/// neither field.
fn window_score(entry: &WindowEntry, needle: &str) -> Option<usize> {
    [&entry.title, &entry.class, &entry.instance]
        .into_iter()
        .filter_map(|field| fuzzy_score(&field.to_lowercase(), needle))
        .max()
}

/// Rank open windows and applications into one list.
///
/// The rules, in the order they apply:
///
/// 1. What was typed decides. This already held among applications; it now
///    holds across both kinds, so a better-matching application still beats a
///    worse-matching window.
/// 2. On an equal score the window comes first. Focusing something that
///    already exists is one keystroke to undo; a second copy of a browser or
///    an IDE may not be.
/// 3. Windows keep the order they were given, which the caller makes
///    most-recently-focused first.
/// 4. Applications keep their frecency then their name, exactly as before.
///
/// An empty query lists applications only. The launcher's promise is that the
/// top row is one Enter from the application you use most, and a list of
/// windows on top of it would take that away.
#[must_use]
pub fn rank_rows(
    mode: &QueryMode,
    apps: &[AppCandidate<'_>],
    windows: &[WindowEntry],
) -> Vec<LauncherRow> {
    let (needle, want_apps, want_windows) = match mode {
        QueryMode::Answer(_) => return Vec::new(),
        QueryMode::Windows(needle) => (needle.as_str(), false, true),
        QueryMode::Search(needle) => (needle.as_str(), true, !needle.is_empty()),
    };

    let mut ranked_windows: Vec<(usize, usize)> = if want_windows {
        windows
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| window_score(entry, needle).map(|score| (index, score)))
            .collect()
    } else {
        Vec::new()
    };
    // A stable sort, so equal scores keep the caller's order.
    ranked_windows.sort_by_key(|&(_, score)| Reverse(score));

    let mut ranked_apps: Vec<(usize, usize)> = if want_apps {
        apps.iter()
            .enumerate()
            .filter_map(|(index, app)| fuzzy_score(app.search, needle).map(|score| (index, score)))
            .collect()
    } else {
        Vec::new()
    };
    ranked_apps.sort_by_key(|&(index, score)| {
        (
            Reverse(score),
            Reverse(apps[index].usage),
            apps[index].sort_key,
        )
    });

    let mut rows = Vec::with_capacity(ranked_windows.len() + ranked_apps.len());
    let (mut window_at, mut app_at) = (0, 0);
    while window_at < ranked_windows.len() || app_at < ranked_apps.len() {
        let window = ranked_windows.get(window_at);
        let app = ranked_apps.get(app_at);
        let take_window = match (window, app) {
            (Some(_), None) => true,
            (None, Some(_)) => false,
            // Ties go to the window: rule 2.
            (Some((_, window_score)), Some((_, app_score))) => window_score >= app_score,
            (None, None) => break,
        };
        if take_window {
            rows.push(LauncherRow::Window(ranked_windows[window_at].0));
            window_at += 1;
        } else {
            rows.push(LauncherRow::App(ranked_apps[app_at].0));
            app_at += 1;
        }
    }
    rows
}

/// One window row: a marker, what the window calls itself, and where it is
/// when that is not "right here".
///
/// The row answers "will Enter move me somewhere" before it is pressed.
#[must_use]
pub fn window_row(entry: &WindowEntry) -> String {
    let title = one_line(&entry.title);
    let class = one_line(&entry.class);
    let name = ellipsize(
        if title.is_empty() { &class } else { &title },
        MAX_TITLE_CHARS,
    );
    // The class is worth showing only when the title does not already say it,
    // and it is capped like the title: a client controls its own WM_CLASS,
    // and a thousand-character one would widen the card without limit.
    let class = if class.is_empty() || name.to_lowercase().contains(&class.to_lowercase()) {
        String::new()
    } else {
        format!("  \u{2014}  {}", ellipsize(&class, MAX_CLASS_CHARS))
    };
    let mut where_it_is = Vec::new();
    if entry.minimized {
        where_it_is.push("minimised".to_string());
    } else if !entry.visible {
        match entry.tag {
            Some(tag) => where_it_is.push(format!("tag {}", tag + 1)),
            None => where_it_is.push("hidden".to_string()),
        }
    } else if !entry.on_selected_monitor {
        // Visible, but on the other head. Naming its tag would name the one
        // the user is already looking at.
        where_it_is.push(format!("screen {}", entry.monitor));
    }
    let elsewhere = if where_it_is.is_empty() {
        String::new()
    } else {
        format!("  [{}]", where_it_is.join(", "))
    };
    // fa-window-maximize, inside FontAwesome 4.7.
    format!("\u{f2d0}  {name}{class}{elsewhere}")
}

/// Collapse a client-controlled string onto one line.
///
/// A window title comes straight from `_NET_WM_NAME` and may contain
/// anything. The panel's contract is one item per visual line — the
/// compositor divides the card's height by the item count to place the
/// selection highlight — so a title with a newline in it would put the
/// highlight on a different row than the one Enter activates. The
/// notifications module collapses control characters for the same reason.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_string()
}

fn ellipsize(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit - 1).collect();
    out.push('\u{2026}');
    out
}

// -------------------------------------------------------------------------
// Matching
// -------------------------------------------------------------------------

/// How well `needle` matches `haystack`, higher being better.
///
/// A substring match beats every subsequence match, and an earlier one beats
/// a later one, so typing the start of a name puts it on top. Failing that,
/// the characters must appear in order — which is what makes `vsc` find
/// Visual Studio Code.
#[must_use]
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<usize> {
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

// -------------------------------------------------------------------------
// Terminal applications
// -------------------------------------------------------------------------

/// The argv to actually run for a launcher choice.
///
/// A desktop entry with `Terminal=true` — an editor, a system monitor, a
/// package manager front end — draws no window of its own. Spawned directly
/// it exits instantly and looks like a launcher that did nothing, so it is
/// handed an execution-capable terminal prefix selected by the prober. The
/// prefix already carries that terminal's execution delimiter (`-e`, `--`,
/// or another declared protocol), so this function only preserves argv
/// boundaries while joining it to the desktop-entry command.
#[must_use]
pub fn launch_command(
    terminal_prefix: &[String],
    command: &[String],
    terminal: bool,
) -> Vec<String> {
    if !terminal || command.is_empty() || terminal_prefix.is_empty() {
        return command.to_vec();
    }
    let mut argv = terminal_prefix.to_vec();
    argv.extend_from_slice(command);
    argv
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Number(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Open,
    Close,
}

fn tokenize(text: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut at = 0;
    while at < bytes.len() {
        let ch = bytes[at];
        if ch.is_whitespace() {
            at += 1;
            continue;
        }
        if ch.is_ascii_digit() || ch == '.' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == '.') {
                at += 1;
            }
            let literal: String = bytes[start..at].iter().collect();
            tokens.push(Token::Number(literal.parse().ok()?));
            continue;
        }
        tokens.push(match ch {
            '+' => Token::Plus,
            '-' => Token::Minus,
            '*' | 'x' => Token::Star,
            '/' => Token::Slash,
            '%' => Token::Percent,
            '^' => Token::Caret,
            '(' => Token::Open,
            ')' => Token::Close,
            // Anything else means this was never an expression: an
            // application name, most likely.
            _ => return None,
        });
        at += 1;
    }
    (!tokens.is_empty()).then_some(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    /// Precedence climbing. `^` binds tighter than `*`, which binds tighter
    /// than `+`, and `^` associates to the right.
    fn expression(&mut self, min_binding: u8) -> Option<f64> {
        let mut left = self.unary()?;
        loop {
            let (binding, right_associative) = match self.tokens.get(self.at) {
                Some(Token::Plus | Token::Minus) => (1, false),
                Some(Token::Star | Token::Slash | Token::Percent) => (2, false),
                Some(Token::Caret) => (3, true),
                _ => break,
            };
            if binding < min_binding {
                break;
            }
            let operator = self.tokens[self.at];
            self.at += 1;
            let next_binding = if right_associative {
                binding
            } else {
                binding + 1
            };
            let right = self.expression(next_binding)?;
            left = match operator {
                Token::Plus => left + right,
                Token::Minus => left - right,
                Token::Star => left * right,
                Token::Slash | Token::Percent if right == 0.0 => return None,
                Token::Slash => left / right,
                Token::Percent => left % right,
                Token::Caret => left.powf(right),
                _ => return None,
            };
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<f64> {
        match self.tokens.get(self.at) {
            Some(Token::Minus) => {
                self.at += 1;
                Some(-self.unary()?)
            }
            Some(Token::Plus) => {
                self.at += 1;
                self.unary()
            }
            Some(Token::Number(value)) => {
                let value = *value;
                self.at += 1;
                Some(value)
            }
            Some(Token::Open) => {
                self.at += 1;
                let inner = self.expression(0)?;
                if self.tokens.get(self.at) != Some(&Token::Close) {
                    return None;
                }
                self.at += 1;
                Some(inner)
            }
            _ => None,
        }
    }
}

// -------------------------------------------------------------------------
// The window manager's half
// -------------------------------------------------------------------------

impl crate::jwm::Jwm {
    /// Flatten the open windows for the launcher, most-recently-focused first
    /// on the monitor the user is looking at.
    ///
    /// A snapshot rather than a live view: the panel holds it for as long as
    /// it is open, and a window that closes meanwhile is a quiet no-op when
    /// its row is activated.
    pub(crate) fn launcher_window_snapshot(&self) -> Vec<WindowEntry> {
        let cfg = crate::config::CONFIG.load();
        let tag_count = cfg.tags_length();
        let mut ordered: Vec<crate::core::models::MonitorKey> = Vec::new();
        // The monitor in front of the user first, so its windows rank ahead
        // of the ones on the other screen.
        ordered.extend(self.state.sel_mon);
        ordered.extend(
            self.state
                .monitor_order
                .iter()
                .copied()
                .filter(|key| Some(*key) != self.state.sel_mon),
        );

        let mut entries = Vec::new();
        for monitor_key in ordered {
            let Some(monitor) = self.state.monitors.get(monitor_key) else {
                continue;
            };
            let active = monitor.get_active_tags();
            let Some(stack) = self.state.monitor_stack.get(monitor_key) else {
                continue;
            };
            for &client_key in stack {
                let Some(client) = self.state.clients.get(client_key) else {
                    continue;
                };
                // A swallowed terminal is standing in for its child, and a
                // scratchpad parked on no tag at all cannot be revealed
                // without duplicating the scratchpad's own show logic.
                if client.state.is_swallowed || (client.state.tags == 0 && !client.state.is_sticky)
                {
                    continue;
                }
                let tag = if client.state.is_sticky {
                    None
                } else {
                    (0..tag_count).find(|index| client.state.tags & (1 << index) != 0)
                };
                entries.push(WindowEntry {
                    id: client.win.raw(),
                    title: client.name.clone(),
                    class: client.class.clone(),
                    instance: client.instance.clone(),
                    tag,
                    monitor: monitor.num,
                    visible: !client.state.is_hidden
                        && (client.state.is_sticky || client.state.tags & active != 0),
                    on_selected_monitor: Some(monitor_key) == self.state.sel_mon,
                    minimized: client.state.is_hidden,
                });
            }
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    const WEEK: u64 = 604_800;
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn recent_use_outranks_old_use() {
        let mut store = UsageStore::default();
        store.record("today", NOW - HOUR * 2);
        store.record("last-month", NOW - DAY * 40);
        store.record("last-month", NOW - DAY * 40);
        store.record("last-month", NOW - DAY * 40);
        assert!(store.score("today", NOW) > store.score("last-month", NOW));
    }

    #[test]
    fn frequent_use_outranks_a_single_use_at_the_same_age() {
        let mut store = UsageStore::default();
        for _ in 0..5 {
            store.record("often", NOW - HOUR);
        }
        store.record("once", NOW - HOUR);
        assert!(store.score("often", NOW) > store.score("once", NOW));
    }

    #[test]
    fn a_huge_count_cannot_hold_a_row_forever() {
        let mut store = UsageStore::default();
        for _ in 0..10_000 {
            store.record("ancient", NOW - DAY * 400);
        }
        for _ in 0..10 {
            store.record("current", NOW - HOUR);
        }
        // Ten launches this hour beat ten thousand a year ago. Uncapped they
        // would not: the count alone would be two hundred times larger, and
        // the top of the list would be frozen for as long as the user lives.
        assert!(store.score("current", NOW) > store.score("ancient", NOW));
    }

    #[test]
    fn something_never_launched_scores_nothing() {
        assert_eq!(UsageStore::default().score("unknown", NOW), 0);
    }

    #[test]
    fn the_store_survives_a_round_trip_including_names_with_spaces() {
        let mut store = UsageStore::default();
        store.record("Text Editor", NOW - HOUR);
        store.record("Text Editor", NOW - HOUR);
        store.record("firefox", NOW - WEEK);
        let restored = UsageStore::parse(&store.serialize(NOW));
        assert_eq!(restored, store);
        assert_eq!(
            restored.score("Text Editor", NOW),
            store.score("Text Editor", NOW)
        );
    }

    #[test]
    fn a_corrupt_line_costs_that_line_and_nothing_else() {
        let store = UsageStore::parse(
            "3 1800000000 good\nnonsense\n\n1 notanumber bad\n7 1800000000 also good\n",
        );
        assert_eq!(store.len(), 2);
        assert!(store.score("good", NOW) > 0);
        assert!(store.score("also good", NOW) > 0);
        assert_eq!(store.score("bad", NOW), 0);
    }

    #[test]
    fn usage_load_rejects_oversized_files_and_special_sources() {
        use std::os::unix::ffi::OsStrExt as _;

        let sequence = USAGE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "jwm-launcher-load-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();

        let oversized = root.join("oversized");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_USAGE_BYTES + 1)
            .unwrap();
        assert!(UsageStore::load_from_path(&oversized).is_empty());

        let fifo = root.join("fifo");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        // Opening a FIFO for a normal blocking read would hang here until a
        // writer appeared. The loader must reject it immediately instead.
        assert!(UsageStore::load_from_path(&fifo).is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saving_drops_the_least_useful_entries_first() {
        let mut store = UsageStore::default();
        for index in 0..MAX_TRACKED + 10 {
            // Older and less used as the index grows.
            let id = format!("app{index:04}");
            store.record(&id, NOW - index as u64 * DAY);
        }
        store.record("keeper", NOW);
        let kept = UsageStore::parse(&store.serialize(NOW));
        assert_eq!(kept.len(), MAX_TRACKED);
        assert!(
            kept.score("keeper", NOW) > 0,
            "the freshest entry was dropped"
        );
        assert_eq!(kept.score("app0509", NOW), 0, "the stalest entry was kept");
    }

    #[test]
    fn usage_save_replaces_a_symlink_without_touching_its_target() {
        let sequence = USAGE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "jwm-launcher-usage-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let victim = root.join("victim");
        fs::write(&victim, "unchanged").unwrap();
        let path = root.join(USAGE_FILE);
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        atomic_write_usage(&path, b"3 1800000000 firefox\n").unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "unchanged");
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "3 1800000000 firefox\n");
        fs::remove_dir_all(root).unwrap();
    }

    fn window(id: u64, title: &str, class: &str) -> WindowEntry {
        WindowEntry {
            id,
            title: title.into(),
            class: class.into(),
            instance: class.to_lowercase(),
            tag: Some(0),
            monitor: 0,
            visible: true,
            on_selected_monitor: true,
            minimized: false,
        }
    }

    fn app<'a>(search: &'a str, sort_key: &'a str, usage: u32) -> AppCandidate<'a> {
        AppCandidate {
            search,
            sort_key,
            usage,
        }
    }

    #[test]
    fn an_empty_query_lists_applications_only() {
        // The launcher's promise is that the top row is one Enter from the
        // application you use most; a window list on top of it would take
        // that away.
        let rows = rank_rows(
            &parse_query(""),
            &[app("firefox", "firefox", 0)],
            &[window(1, "GitHub", "firefox")],
        );
        assert_eq!(rows, [LauncherRow::App(0)]);
    }

    #[test]
    fn typing_brings_the_open_windows_in() {
        let rows = rank_rows(
            &parse_query("fire"),
            &[app("firefox", "firefox", 0)],
            &[window(1, "GitHub", "firefox")],
        );
        // Both match `fire` equally well through their class, and the window
        // wins the tie: focusing what exists is one keystroke to undo.
        assert_eq!(rows, [LauncherRow::Window(0), LauncherRow::App(0)]);
    }

    #[test]
    fn what_was_typed_still_decides_across_both_kinds() {
        // The application matches at position 0, the window only as a
        // subsequence; a worse-matching window must not jump the queue.
        let rows = rank_rows(
            &parse_query("gimp"),
            &[app("gimp image editor", "gimp", 0)],
            &[window(1, "graphics import", "xterm")],
        );
        assert_eq!(rows.first(), Some(&LauncherRow::App(0)));
    }

    #[test]
    fn equal_matches_and_usage_borrow_the_precomputed_name_sort_key() {
        let apps = [
            app("editor", "zeta editor", 0),
            app("editor", "alpha editor", 0),
        ];
        assert_eq!(
            rank_rows(&parse_query("editor"), &apps, &[]),
            [LauncherRow::App(1), LauncherRow::App(0)]
        );
    }

    #[test]
    fn a_window_matches_by_class_and_never_across_two_fields() {
        let entry = window(1, "GitHub", "firefox");
        assert!(window_score(&entry, "firefox").is_some(), "class");
        assert!(window_score(&entry, "github").is_some(), "title");
        // Concatenating the fields into one haystack would invent this match.
        assert!(window_score(&entry, "foxgithub").is_none());
    }

    #[test]
    fn equally_matching_windows_keep_the_order_they_were_given() {
        // The caller hands them over most-recently-focused first.
        let windows = [
            window(7, "one", "term"),
            window(8, "two", "term"),
            window(9, "three", "term"),
        ];
        let rows = rank_rows(&parse_query("term"), &[], &windows);
        assert_eq!(
            rows,
            [
                LauncherRow::Window(0),
                LauncherRow::Window(1),
                LauncherRow::Window(2)
            ]
        );
    }

    #[test]
    fn a_leading_slash_asks_for_windows_and_nothing_else() {
        let apps = [app("firefox", "firefox", 99)];
        let windows = [window(1, "GitHub", "firefox")];
        // Even with nothing after it: that is the on-demand window list.
        assert_eq!(parse_query("/"), QueryMode::Windows(String::new()));
        assert_eq!(
            rank_rows(&parse_query("/"), &apps, &windows),
            [LauncherRow::Window(0)]
        );
        assert_eq!(
            rank_rows(&parse_query("/git"), &apps, &windows),
            [LauncherRow::Window(0)]
        );
        assert!(rank_rows(&parse_query("/zzz"), &apps, &windows).is_empty());
    }

    #[test]
    fn the_slash_prefix_and_the_calculator_cannot_fight() {
        // `evaluate` parses nothing that starts with an operator, so a
        // leading slash is always a window filter.
        assert_eq!(parse_query("/1+1"), QueryMode::Windows("1+1".into()));
        assert!(matches!(parse_query("1920*0.6"), QueryMode::Answer(_)));
        // Arithmetic replaces the list entirely: no window, no application.
        assert!(
            rank_rows(
                &parse_query("1+1"),
                &[app("firefox", "firefox", 0)],
                &[window(1, "GitHub", "firefox")]
            )
            .is_empty()
        );
    }

    #[test]
    fn a_row_says_where_the_window_is_only_when_it_is_not_here() {
        let here = window(1, "GitHub", "firefox");
        let row = window_row(&here);
        assert!(row.contains("GitHub") && row.contains("firefox"));
        assert!(
            !row.contains('['),
            "a visible window needs no location: {row}"
        );

        let elsewhere = WindowEntry {
            visible: false,
            tag: Some(3),
            ..here.clone()
        };
        assert!(
            window_row(&elsewhere).contains("tag 4"),
            "tags are 1-based on screen"
        );

        let hidden = WindowEntry {
            visible: false,
            minimized: true,
            ..here.clone()
        };
        let row = window_row(&hidden);
        assert!(row.contains("minimised") && !row.contains("tag"));

        // Sticky: on every tag, so there is no one tag to name.
        let sticky = WindowEntry {
            visible: false,
            tag: None,
            ..here.clone()
        };
        assert!(window_row(&sticky).contains("hidden"));
    }

    #[test]
    fn a_row_does_not_repeat_the_class_the_title_already_says() {
        let entry = window(1, "Firefox — GitHub", "Firefox");
        let row = window_row(&entry);
        assert_eq!(row.matches("Firefox").count(), 1, "{row}");
    }

    #[test]
    fn a_pathological_title_or_class_cannot_widen_the_card_without_limit() {
        // Both are the client's to set: the title comes from _NET_WM_NAME and
        // the class from WM_CLASS, of which the X server hands over up to a
        // kilobyte.
        for entry in [
            window(1, &"x".repeat(400), "term"),
            window(2, "short", &"y".repeat(400)),
        ] {
            let row = window_row(&entry);
            assert!(
                row.chars().count() < MAX_TITLE_CHARS + MAX_CLASS_CHARS + 16,
                "{} chars: {row}",
                row.chars().count()
            );
            assert!(row.contains('\u{2026}'));
        }
    }

    #[test]
    fn a_title_with_a_newline_in_it_stays_one_row() {
        // The panel's contract is one item per visual line — the compositor
        // divides the card by the item count to place the highlight — so a
        // two-line title would put the highlight on a different row than the
        // one Enter activates.
        let entry = window(1, "GitHub\nPull requests\r\tmore", "term");
        let row = window_row(&entry);
        assert_eq!(row.lines().count(), 1, "{row:?}");
        assert!(row.contains("GitHub Pull requests"));
    }

    #[test]
    fn a_window_on_the_other_screen_is_not_called_hidden() {
        // It is plainly visible over there; naming its tag would name the one
        // the user is already looking at, and calling a sticky window hidden
        // is simply false.
        let elsewhere = WindowEntry {
            on_selected_monitor: false,
            monitor: 1,
            ..window(1, "Firefox", "firefox")
        };
        let row = window_row(&elsewhere);
        assert!(row.contains("screen 1"), "{row}");
        assert!(!row.contains("tag") && !row.contains("hidden"));

        // Off-tag on its own monitor still names the tag.
        let off_tag = WindowEntry {
            visible: false,
            tag: Some(2),
            ..elsewhere.clone()
        };
        assert!(window_row(&off_tag).contains("tag 3"));
    }

    #[test]
    fn a_window_row_uses_only_widely_available_glyphs() {
        for entry in [
            window(1, "a", "b"),
            WindowEntry {
                visible: false,
                minimized: true,
                ..window(2, "c", "d")
            },
        ] {
            for ch in window_row(&entry)
                .chars()
                .filter(|ch| ('\u{f000}'..'\u{f900}').contains(ch))
            {
                assert!((ch as u32) < 0xf600, "{ch:?} is outside FontAwesome 4");
            }
        }
    }

    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy_score("visual studio code", "vsc").is_some());
        assert!(fuzzy_score("firefox", "zzz").is_none());
    }

    #[test]
    fn a_terminal_application_is_given_a_terminal() {
        let term = vec!["alacritty".to_string(), "-e".to_string()];
        let htop = vec!["htop".to_string()];
        assert_eq!(
            launch_command(&term, &htop, true),
            ["alacritty", "-e", "htop"]
        );
        let terminator = vec!["terminator".to_string(), "-x".to_string()];
        let command_with_args = vec![
            "monitor".to_string(),
            "--interval".to_string(),
            "2".to_string(),
        ];
        assert_eq!(
            launch_command(&terminator, &command_with_args, true),
            ["terminator", "-x", "monitor", "--interval", "2"]
        );
        // A graphical application is spawned exactly as the desktop entry
        // asked, arguments and all.
        let editor = vec!["gedit".to_string(), "--new-window".to_string()];
        assert_eq!(launch_command(&term, &editor, false), editor);
        // Nothing to wrap it in: better the bare command than an argv that
        // starts with an empty program name.
        assert_eq!(launch_command(&[], &htop, true), htop);
        assert!(launch_command(&term, &[], true).is_empty());
    }

    #[test]
    fn arithmetic_respects_precedence_and_parentheses() {
        assert_eq!(evaluate("1+2*3"), Some(7.0));
        assert_eq!(evaluate("(1+2)*3"), Some(9.0));
        assert_eq!(evaluate("2^3^2"), Some(512.0), "^ associates to the right");
        assert_eq!(evaluate("10-2-3"), Some(5.0), "- associates to the left");
        assert_eq!(evaluate("-4+1"), Some(-3.0));
        assert_eq!(evaluate("7%4"), Some(3.0));
        assert_eq!(evaluate("1920 * 0.6"), Some(1152.0));
        assert_eq!(evaluate("=1+1"), Some(2.0), "a leading = is allowed");
    }

    #[test]
    fn a_query_without_an_operator_is_not_arithmetic() {
        // Otherwise every application name that happens to be a number would
        // vanish behind a calculator row.
        assert_eq!(evaluate("42"), None);
        assert_eq!(evaluate("firefox"), None);
        assert_eq!(evaluate(""), None);
        assert_eq!(evaluate("   "), None);
    }

    #[test]
    fn an_application_name_that_contains_an_operator_is_still_not_arithmetic() {
        assert_eq!(evaluate("gtk+"), None);
        assert_eq!(evaluate("c++"), None);
        assert_eq!(evaluate("re-search"), None);
        assert_eq!(evaluate("2+"), None);
        assert_eq!(evaluate("(1+2"), None);
        assert_eq!(evaluate("1+2)"), None);
    }

    #[test]
    fn division_by_zero_answers_nothing_rather_than_infinity() {
        assert_eq!(evaluate("1/0"), None);
        assert_eq!(evaluate("1%0"), None);
        assert_eq!(evaluate("0/0"), None);
    }

    #[test]
    fn results_are_shown_the_way_a_person_would_write_them() {
        assert_eq!(format_result(7.0), "7");
        assert_eq!(format_result(-3.0), "-3");
        assert_eq!(format_result(1152.0), "1152");
        assert_eq!(format_result(2.5), "2.5");
        // Binary floating point's answer to 0.1+0.2 is correct and useless.
        assert_eq!(format_result(evaluate("0.1+0.2").expect("value")), "0.3");
        assert_eq!(
            format_result(evaluate("10/3").expect("value")),
            "3.3333333333"
        );
    }
}
