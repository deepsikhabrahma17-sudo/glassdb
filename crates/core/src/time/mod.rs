//! Time: the clock commits are stamped with, and timestamps as text.
//!
//! A time is a count of microseconds since 1970-01-01 00:00:00 UTC. Text is
//! UTC unless it says otherwise, because Rust's standard library can't find
//! the local time zone. The calendar sums are written out here, since a
//! timestamp is the only date-shaped thing GlassDB has.

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MICROS_PER_SECOND: i64 = 1_000_000;
pub const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
pub const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
pub const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;

/// Where commit times come from. A seam, so tests can choose the time and a
/// browser, which has no system clock to ask, can supply its own.
pub trait Clock: Send + Sync {
    /// The time now, in microseconds since the Unix epoch.
    fn now(&self) -> i64;
}

/// The computer's own clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_micros() as i64,
            // A clock set before 1970. Commit times are held from going
            // backwards anyway, so the exact value doesn't matter.
            Err(_) => 0,
        }
    }
}

/// A clock that only moves when told to.
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(now: i64) -> ManualClock {
        ManualClock(AtomicI64::new(now))
    }

    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::Relaxed);
    }

    pub fn advance(&self, by: i64) {
        self.0.fetch_add(by, Ordering::Relaxed);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Days since 1970-01-01 of a calendar date. The trick is to count years from
/// March, which puts the leap day last, so months have a fixed pattern.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12; // March = 0
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The calendar date `days` after 1970-01-01: `days_from_civil` undone.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = (shifted_month + 2) % 12 + 1;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Walks through a timestamp's bytes.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn eat(&mut self, byte: u8) -> bool {
        let found = self.0.first() == Some(&byte);
        if found {
            self.0 = &self.0[1..];
        }
        found
    }

    fn at_digit(&self) -> bool {
        self.0.first().is_some_and(u8::is_ascii_digit)
    }

    /// Exactly `count` digits.
    fn digits(&mut self, count: usize) -> Option<i64> {
        let digits = self.0.get(..count)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.0 = &self.0[count..];
        Some(digits.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    }

    /// `HH:MM[:SS[.ffffff]]`, as microseconds after midnight.
    fn time(&mut self) -> Option<i64> {
        let hour = self.digits(2).filter(|h| *h < 24)?;
        if !self.eat(b':') {
            return None;
        }
        let minute = self.digits(2).filter(|m| *m < 60)?;
        let mut micros = hour * MICROS_PER_HOUR + minute * MICROS_PER_MINUTE;
        if self.eat(b':') {
            micros += self.digits(2).filter(|s| *s < 60)? * MICROS_PER_SECOND;
            if self.eat(b'.') {
                micros += self.fraction()?;
            }
        }
        Some(micros)
    }

    /// One to six digits after a decimal point, as microseconds.
    fn fraction(&mut self) -> Option<i64> {
        let mut micros = 0;
        let mut place = MICROS_PER_SECOND;
        while self.at_digit() {
            place /= 10;
            if place == 0 {
                return None;
            }
            micros += self.digits(1)? * place;
        }
        (place < MICROS_PER_SECOND).then_some(micros)
    }

    /// `Z`, `+HH:MM` or `-HH:MM`: how far ahead of UTC the time was written.
    /// Nothing at all means UTC.
    fn offset(&mut self) -> Option<i64> {
        if self.eat(b'Z') || self.0.is_empty() {
            return Some(0);
        }
        let sign = match self.eat(b'+') {
            true => 1,
            false if self.eat(b'-') => -1,
            false => return None,
        };
        let hour = self.digits(2).filter(|h| *h < 24)?;
        if !self.eat(b':') {
            return None;
        }
        let minute = self.digits(2).filter(|m| *m < 60)?;
        Some(sign * (hour * MICROS_PER_HOUR + minute * MICROS_PER_MINUTE))
    }
}

/// Read `YYYY-MM-DD[ HH:MM[:SS[.ffffff]]][ Z|+HH:MM|-HH:MM]`. The separator
/// may be a `T`. Returns `None` if it isn't one, or isn't a real date.
pub fn parse_timestamp(text: &str) -> Option<i64> {
    let mut cursor = Cursor(text.trim().as_bytes());
    let year = cursor.digits(4).filter(|y| *y > 0)?;
    if !cursor.eat(b'-') {
        return None;
    }
    let month = cursor.digits(2).filter(|m| (1..=12).contains(m))?;
    if !cursor.eat(b'-') {
        return None;
    }
    let day = cursor
        .digits(2)
        .filter(|d| (1..=days_in_month(year, month)).contains(d))?;

    let spaced = cursor.eat(b' ') || cursor.eat(b'T');
    let time = if spaced && cursor.at_digit() {
        cursor.time()?
    } else {
        0
    };
    cursor.eat(b' ');
    let offset = cursor.offset()?;
    cursor
        .0
        .is_empty()
        .then(|| days_from_civil(year, month, day) * MICROS_PER_DAY + time - offset)
}

/// `micros` as UTC text that `parse_timestamp` reads back exactly.
pub fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let rest = micros.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = rest / MICROS_PER_HOUR;
    let minute = rest % MICROS_PER_HOUR / MICROS_PER_MINUTE;
    let second = rest % MICROS_PER_MINUTE / MICROS_PER_SECOND;
    let fraction = rest % MICROS_PER_SECOND;
    let mut text = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    if fraction != 0 {
        text.push_str(&format!(".{fraction:06}"));
    }
    text
}
