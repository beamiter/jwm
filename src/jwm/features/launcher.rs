//! What the application launcher knows besides the list of applications.
//!
//! Two things separate a launcher people keep using from one they abandon:
//! it puts what they actually run at the top, and it answers a small question
//! without making them open something else first. Both are arithmetic — one
//! over usage counts, one over the query itself — so both live here, pure and
//! tested, rather than tangled into the panel that draws them.

use std::collections::HashMap;

/// Where the usage counts live, under the user's data directory.
pub const USAGE_FILE: &str = "launcher-usage";

/// Most launches remembered. Beyond this the least useful entries are
/// dropped: a usage file that grows forever would eventually cost more to
/// read than the ranking is worth.
pub const MAX_TRACKED: usize = 500;

/// Launches beyond this stop increasing an entry's rank, so an editor opened
/// ten thousand times cannot make everything else unreachable for a month
/// after you stop using it.
const COUNT_CAP: u32 = 50;

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
        std::fs::read_to_string(usage_path())
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Write the store back. Failures are logged and dropped — losing a
    /// ranking update is not worth interrupting a launch over.
    pub fn save(&self, now: u64) {
        let path = usage_path();
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::debug!("launcher: {}: {error}", parent.display());
            return;
        }
        if let Err(error) = std::fs::write(&path, self.serialize(now)) {
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
// Terminal applications
// -------------------------------------------------------------------------

/// The argv to actually run for a launcher choice.
///
/// A desktop entry with `Terminal=true` — an editor, a system monitor, a
/// package manager front end — draws no window of its own. Spawned directly
/// it exits instantly and looks like a launcher that did nothing, so it is
/// handed a terminal. `-e` is the option every terminal in the prober's list
/// understands.
#[must_use]
pub fn launch_command(termcmd: &[String], command: &[String], terminal: bool) -> Vec<String> {
    if !terminal || command.is_empty() || termcmd.is_empty() {
        return command.to_vec();
    }
    let mut argv = termcmd.to_vec();
    argv.push("-e".to_string());
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
    fn a_terminal_application_is_given_a_terminal() {
        let term = vec!["alacritty".to_string()];
        let htop = vec!["htop".to_string()];
        assert_eq!(
            launch_command(&term, &htop, true),
            ["alacritty", "-e", "htop"]
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
