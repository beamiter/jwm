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
        let total = self.year * 12 + (self.month as i32 - 1) + delta;
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
        (Some(first), Some(next)) => next.signed_duration_since(first).num_days() as u32,
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
/// Cells are three columns wide so the grid lines up in the monospace font
/// the panel renders with.
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
        if today_here && day == view.today.day() {
            week.push_str(&format!("[{day:>2}]"));
        } else {
            week.push_str(&format!(" {day:>2} "));
        }
        // Seven cells to a row; the header already sits above them.
        let filled = leading_blanks(view.year, view.month) + day as usize;
        if filled % 7 == 0 {
            rows.push(std::mem::take(&mut week));
        }
    }
    if !week.trim().is_empty() {
        rows.push(week);
    }
    rows
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
}
