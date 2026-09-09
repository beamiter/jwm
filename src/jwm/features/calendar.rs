//! Month-grid calendar for the shell's clock card.
//!
//! Everything here is pure and takes the date as an argument rather than
//! reading the clock, so the grid, the leap-year edges, and the month/year
//! stepping are unit tested against fixed dates instead of "today".

use chrono::{Datelike, NaiveDate, Timelike};

/// Column headers, Monday first — the ISO week, which is what most of the
/// world reading this uses.
const WEEKDAYS: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Which month the card is showing, plus the day to highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarView {
    pub year: i32,
    /// 1..=12.
    pub month: u32,
    /// The real current date, highlighted when it falls in the shown month.
    pub today: NaiveDate,
}

impl CalendarView {
    #[must_use]
    pub fn new(today: NaiveDate) -> Self {
        Self {
            year: today.year(),
            month: today.month(),
            today,
        }
    }

    /// Step by whole months, rolling the year over.
    pub fn shift_month(&mut self, delta: i32) {
        // Work in months-since-year-0 so December + 1 lands on January.
        let total = self.year * 12 + i32::try_from(self.month).unwrap_or(1) - 1 + delta;
        self.year = total.div_euclid(12);
        self.month = (total.rem_euclid(12) + 1) as u32;
    }

    pub fn shift_year(&mut self, delta: i32) {
        self.year += delta;
    }

    /// Jump back to the month containing today.
    pub fn reset(&mut self) {
        self.year = self.today.year();
        self.month = self.today.month();
    }

    #[must_use]
    pub fn title(&self) -> String {
        let name = MONTHS
            .get((self.month as usize).saturating_sub(1))
            .copied()
            .unwrap_or("");
        format!("{name} {}", self.year)
    }
}

/// Days in a month, leap years included.
#[must_use]
pub fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1);
    let next_first = NaiveDate::from_ymd_opt(next_year, next_month, 1);
    match (first, next_first) {
        (Some(first), Some(next)) => {
            u32::try_from(next.signed_duration_since(first).num_days()).unwrap_or(0)
        }
        _ => 0,
    }
}

/// Column index (0 = Monday) the first of the month falls on.
#[must_use]
pub fn leading_blanks(year: i32, month: u32) -> usize {
    NaiveDate::from_ymd_opt(year, month, 1)
        .map_or(0, |date| date.weekday().num_days_from_monday() as usize)
}

/// The month laid out as one string per week, with today marked by brackets.
///
/// Day cells are four columns wide — ` dd `, or `[dd]` around today — and
/// the header's three-column weekday names plus their one-column separators
/// ride the same four-column stride, so the grid lines up in the monospace
/// font the panel renders with. Leading cells before the 1st are blank;
/// trailing cells past the month's end are not drawn at all.
#[must_use]
pub fn month_grid(view: &CalendarView) -> Vec<String> {
    let days = days_in_month(view.year, view.month);
    if days == 0 {
        return Vec::new();
    }
    let today_here = view.today.year() == view.year && view.today.month() == view.month;

    let mut rows = vec![WEEKDAYS.map(|day| format!(" {day}")).join(" ")];
    let mut week = String::new();
    for _ in 0..leading_blanks(view.year, view.month) {
        week.push_str("    ");
    }
    for day in 1..=days {
        use std::fmt::Write as _;
        if today_here && day == view.today.day() {
            let _ = write!(week, "[{day:>2}]");
        } else {
            let _ = write!(week, " {day:>2} ");
        }
        // Seven cells to a row; the header already sits above them.
        let filled = leading_blanks(view.year, view.month) + day as usize;
        if filled.is_multiple_of(7) {
            rows.push(std::mem::take(&mut week));
        }
    }
    if !week.trim().is_empty() {
        rows.push(week);
    }
    rows
}

/// Transparent margin the text rasterizer leaves around every overlay
/// texture (`compositor_font::TEXT_PAD`, private to the backend — the same
/// tiny copy `system_ui` keeps). Glyph columns start this far into the
/// texture, so it comes out of a pointer offset before the column math.
const TEXT_PAD: f32 = 2.0;

/// What a click on the calendar card asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarClick {
    /// Step one month back.
    PrevMonth,
    /// Step one month forward.
    NextMonth,
    /// Return to the month containing today.
    Today,
    /// Nothing the calendar answers to; the click stays a no-op.
    None,
}

/// Map a pointer press on the calendar card onto what it means, from the
/// row's index in the overlay and the press's offset into the row's text
/// texture. The rule, pinned:
///
/// * The overlay's rows are fixed: 0 is the clock line, 1 is blank, 2 the
///   weekday header, and 3 on are the week rows — only week rows answer.
/// * On a week row the grid is seven 4-column cells (see [`month_grid`]). A
///   click on a LEADING cell before the 1st — where the previous month's
///   tail days would sit — steps a month back; a click on a TRAILING cell
///   past the month's end — drawn or not — steps a month forward; a click
///   on today's own cell returns the view to the current month, mirroring
///   the `t` key.
/// * Everything else — real day cells, the clock, the blank row, the
///   header, the texture's margins, anywhere past the grid — is
///   [`CalendarClick::None`], and the press keeps being the no-op a click
///   on the card always was.
///
/// `char_width_px` is the measured advance of one glyph in the panel's
/// monospace font (the caller measures; this stays backend-free), and the
/// cell boundaries follow from it — never from a hardcoded pixel geometry.
#[must_use]
pub fn click_action(
    visual_row: usize,
    text_x_px: f32,
    char_width_px: f32,
    view: &CalendarView,
) -> CalendarClick {
    // The clock, the blank row and the weekday header name nothing.
    let Some(week) = visual_row.checked_sub(3) else {
        return CalendarClick::None;
    };
    // The grid never draws more than six week rows; a row past them (or a
    // width that cannot measure) is nobody's cell.
    if week >= 6 || !char_width_px.is_finite() || char_width_px <= 0.0 {
        return CalendarClick::None;
    }
    let column = ((text_x_px - TEXT_PAD) / char_width_px).floor();
    if !column.is_finite() || column < 0.0 {
        return CalendarClick::None;
    }
    let cell = (column as usize) / 4;
    if cell > 6 {
        return CalendarClick::None;
    }
    let position = week * 7 + cell;
    let leading = leading_blanks(view.year, view.month);
    let days = days_in_month(view.year, view.month);
    if position < leading {
        return CalendarClick::PrevMonth;
    }
    let day = (position - leading + 1) as u32;
    if day > days {
        return CalendarClick::NextMonth;
    }
    let today_here = view.today.year() == view.year && view.today.month() == view.month;
    if today_here && day == view.today.day() {
        return CalendarClick::Today;
    }
    CalendarClick::None
}

/// `Monday, 27 July 2026 · 15:42` — the line above the grid.
#[must_use]
pub fn clock_line(now: &chrono::NaiveDateTime) -> String {
    let weekday = match now.weekday() {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    };
    let month = MONTHS
        .get((now.month() as usize).saturating_sub(1))
        .copied()
        .unwrap_or("");
    format!(
        "{weekday}, {} {month} {} \u{2022} {:02}:{:02}",
        now.day(),
        now.year(),
        now.hour(),
        now.minute()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    #[test]
    fn month_lengths_follow_the_calendar() {
        assert_eq!(days_in_month(2026, 1), 31);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 12), 31);
    }

    #[test]
    fn february_knows_about_leap_years() {
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        // Century rule: 1900 was not a leap year, 2000 was.
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
    }

    #[test]
    fn the_grid_starts_the_first_on_its_weekday() {
        // 1 July 2026 is a Wednesday: two blanks under Mo and Tu.
        assert_eq!(leading_blanks(2026, 7), 2);
        // 1 June 2026 is a Monday: no blanks.
        assert_eq!(leading_blanks(2026, 6), 0);
    }

    #[test]
    fn the_grid_covers_every_day_exactly_once() {
        let view = CalendarView::new(date(2026, 7, 27));
        let grid = month_grid(&view);
        let numbers: Vec<u32> = grid
            .iter()
            .skip(1) // weekday header
            .flat_map(|row| {
                row.split_whitespace()
                    .map(|cell| cell.trim_matches(['[', ']']).to_string())
                    .filter_map(|cell| cell.parse::<u32>().ok())
            })
            .collect();

        assert_eq!(numbers.len(), 31);
        assert_eq!(numbers.first(), Some(&1));
        assert_eq!(numbers.last(), Some(&31));
    }

    #[test]
    fn today_is_marked_only_in_its_own_month() {
        let mut view = CalendarView::new(date(2026, 7, 27));
        assert!(month_grid(&view).iter().any(|row| row.contains("[27]")));

        view.shift_month(1);
        assert!(!month_grid(&view).iter().any(|row| row.contains("[27]")));
    }

    #[test]
    fn stepping_months_rolls_the_year_over() {
        let mut view = CalendarView::new(date(2026, 12, 15));
        view.shift_month(1);
        assert_eq!((view.year, view.month), (2027, 1));

        view.shift_month(-1);
        assert_eq!((view.year, view.month), (2026, 12));

        view.shift_month(-12);
        assert_eq!((view.year, view.month), (2025, 12));
    }

    #[test]
    fn stepping_backwards_from_january_lands_in_december() {
        let mut view = CalendarView::new(date(2026, 1, 5));
        view.shift_month(-1);
        assert_eq!((view.year, view.month), (2025, 12));
    }

    #[test]
    fn reset_returns_to_the_month_containing_today() {
        let mut view = CalendarView::new(date(2026, 7, 27));
        view.shift_month(5);
        view.shift_year(2);
        view.reset();
        assert_eq!((view.year, view.month), (2026, 7));
    }

    #[test]
    fn titles_name_the_month_and_year() {
        let view = CalendarView::new(date(2026, 7, 27));
        assert_eq!(view.title(), "July 2026");
    }

    #[test]
    fn the_weekday_header_leads_the_grid() {
        let view = CalendarView::new(date(2026, 7, 27));
        let grid = month_grid(&view);
        assert!(grid[0].contains("Mo"));
        assert!(grid[0].trim_end().ends_with("Su"));
    }

    #[test]
    fn the_clock_line_spells_the_date_out() {
        let when = date(2026, 7, 27)
            .and_hms_opt(15, 42, 0)
            .expect("valid time");
        assert_eq!(clock_line(&when), "Monday, 27 July 2026 \u{2022} 15:42");
    }

    /// Clicks in these tests use the bitmap fallback's metrics: 12 px a
    /// glyph, so cell `c` of a week row spans the 48 px after `48 * c + 2`
    /// (the texture's margin), and the +26 lands mid-cell.
    fn cell_x(cell: usize) -> f32 {
        (48 * cell + 26) as f32
    }

    #[test]
    fn leading_blank_cells_step_a_month_back() {
        // July 2026 opens on a Wednesday: two leading blanks under Mo/Tu.
        let view = CalendarView::new(date(2026, 7, 27));
        assert_eq!(
            click_action(3, cell_x(0), 12.0, &view),
            CalendarClick::PrevMonth
        );
        assert_eq!(
            click_action(3, cell_x(1), 12.0, &view),
            CalendarClick::PrevMonth
        );
    }

    #[test]
    fn trailing_blank_cells_step_a_month_forward() {
        // 31 days from a Wednesday end on a Friday: the last week row's
        // Saturday and Sunday cells are the trailing edge, drawn or not.
        let view = CalendarView::new(date(2026, 7, 27));
        assert_eq!(
            click_action(7, cell_x(5), 12.0, &view),
            CalendarClick::NextMonth
        );
        assert_eq!(
            click_action(7, cell_x(6), 12.0, &view),
            CalendarClick::NextMonth
        );

        // February 2026 starts on a Sunday and fills 28 days: only the very
        // last cell of the grid is a trailing blank.
        let february = CalendarView {
            year: 2026,
            month: 2,
            today: date(2026, 7, 27),
        };
        assert_eq!(
            click_action(7, cell_x(6), 12.0, &february),
            CalendarClick::NextMonth
        );
        assert_eq!(
            click_action(7, cell_x(5), 12.0, &february),
            CalendarClick::None,
            "the 28th is a real day"
        );
    }

    #[test]
    fn clicking_todays_cell_returns_to_today() {
        let view = CalendarView::new(date(2026, 7, 27));
        // The 27th sits at position 2 + 26 = 28: first cell of week row 4.
        assert_eq!(
            click_action(7, cell_x(0), 12.0, &view),
            CalendarClick::Today
        );

        // A leap day is just another today: 29 February 2024, a Thursday,
        // lands at position 3 + 28 = 31 — cell 3 of week row 4.
        let leap = CalendarView {
            year: 2024,
            month: 2,
            today: date(2024, 2, 29),
        };
        assert_eq!(
            click_action(7, cell_x(3), 12.0, &leap),
            CalendarClick::Today
        );

        // The same cell in a month that is not today's names nothing.
        let mut august = view;
        august.shift_month(1);
        assert_eq!(
            click_action(7, cell_x(3), 12.0, &august),
            CalendarClick::None
        );
        assert_eq!(
            click_action(7, cell_x(3), 12.0, &view),
            CalendarClick::None,
            "the 30th is not today"
        );
    }

    #[test]
    fn ordinary_days_and_fixed_rows_keep_the_click_a_no_op() {
        let view = CalendarView::new(date(2026, 7, 27));
        // A real day cell: the 1st under We on the first week row, the 13th
        // leading the third.
        assert_eq!(click_action(3, cell_x(2), 12.0, &view), CalendarClick::None);
        assert_eq!(click_action(5, cell_x(0), 12.0, &view), CalendarClick::None);
        // The clock, the blank row and the weekday header never answer,
        // wherever on them the press lands.
        for row in 0..=2 {
            assert_eq!(
                click_action(row, cell_x(0), 12.0, &view),
                CalendarClick::None
            );
            assert_eq!(
                click_action(row, cell_x(3), 12.0, &view),
                CalendarClick::None
            );
        }
    }

    #[test]
    fn clicks_off_the_grid_or_unmeasurable_name_nothing() {
        let view = CalendarView::new(date(2026, 7, 27));
        // Past the seventh cell the grid ends, even on a week row.
        assert_eq!(click_action(7, cell_x(7), 12.0, &view), CalendarClick::None);
        assert_eq!(click_action(7, 9000.0, 12.0, &view), CalendarClick::None);
        // The texture's left margin belongs to no cell.
        assert_eq!(click_action(3, 1.9, 12.0, &view), CalendarClick::None);
        assert_eq!(click_action(3, -8.0, 12.0, &view), CalendarClick::None);
        // The first cell starts where its glyphs do.
        assert_eq!(click_action(3, 2.0, 12.0, &view), CalendarClick::PrevMonth);
        // The card never draws a seventh week row.
        assert_eq!(click_action(9, cell_x(0), 12.0, &view), CalendarClick::None);
        // A width that cannot measure maps nothing.
        assert_eq!(click_action(3, cell_x(0), 0.0, &view), CalendarClick::None);
        assert_eq!(
            click_action(3, cell_x(0), -12.0, &view),
            CalendarClick::None
        );
        assert_eq!(click_action(3, f32::NAN, 12.0, &view), CalendarClick::None);
    }

    #[test]
    fn edge_cells_span_month_and_year_boundaries() {
        // December's trailing edge steps into January of the next year;
        // January's leading edge back into December of the last. The mapper
        // only names the direction; the shift itself is pinned above.
        let december = CalendarView {
            year: 2026,
            month: 12,
            today: date(2026, 7, 27),
        };
        assert_eq!(
            click_action(7, cell_x(4), 12.0, &december),
            CalendarClick::NextMonth
        );
        let january = CalendarView {
            year: 2026,
            month: 1,
            today: date(2026, 7, 27),
        };
        assert_eq!(
            click_action(3, cell_x(0), 12.0, &january),
            CalendarClick::PrevMonth
        );
        // A leap February's tail: 29 days from a Thursday end on a Thursday,
        // leaving Friday through Sunday of the last week row to step forward.
        let leap = CalendarView {
            year: 2024,
            month: 2,
            today: date(2024, 7, 27),
        };
        assert_eq!(
            click_action(7, cell_x(4), 12.0, &leap),
            CalendarClick::NextMonth
        );
    }
}
