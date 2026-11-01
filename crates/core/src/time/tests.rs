use proptest::prelude::*;

use super::*;

const NOON: i64 = 1_793_534_400_000_000; // 2026-11-01 12:00:00 UTC

#[test]
fn a_date_alone_is_midnight() {
    assert_eq!(
        parse_timestamp("2026-11-01").unwrap(),
        NOON - 12 * MICROS_PER_HOUR
    );
}

#[test]
fn time_parts_are_optional() {
    assert_eq!(parse_timestamp("2026-11-01 12:00").unwrap(), NOON);
    assert_eq!(parse_timestamp("2026-11-01 12:00:00").unwrap(), NOON);
    assert_eq!(parse_timestamp("2026-11-01T12:00:00").unwrap(), NOON);
    assert_eq!(
        parse_timestamp("2026-11-01 12:00:01").unwrap(),
        NOON + 1_000_000
    );
    assert_eq!(
        parse_timestamp("2026-11-01 12:00:00.5").unwrap(),
        NOON + 500_000
    );
    assert_eq!(
        parse_timestamp("2026-11-01 12:00:00.000001").unwrap(),
        NOON + 1
    );
}

#[test]
fn offsets_move_it_to_utc() {
    assert_eq!(parse_timestamp("2026-11-01 12:00Z").unwrap(), NOON);
    assert_eq!(parse_timestamp("2026-11-01 12:00 Z").unwrap(), NOON);
    assert_eq!(
        parse_timestamp("2026-11-01 17:30 +05:30").unwrap(),
        NOON,
        "India is ahead of UTC, so its 17:30 is 12:00 UTC"
    );
    assert_eq!(parse_timestamp("2026-11-01 17:30+05:30").unwrap(), NOON);
    assert_eq!(parse_timestamp("2026-11-01 07:00 -05:00").unwrap(), NOON);
}

#[test]
fn nonsense_is_refused() {
    for text in [
        "",
        "yesterday",
        "2026",
        "2026-11",
        "2026-13-01",
        "2026-00-01",
        "2026-11-00",
        "2026-11-31",
        "2026-02-29",
        "2026-11-01 24:00",
        "2026-11-01 12:60",
        "2026-11-01 12:00:60",
        "2026-11-01 12",
        "2026-11-01 12:00:00.",
        "2026-11-01 12:00:00.1234567",
        "2026-11-01 12:00 +5",
        "2026-11-01 12:00 +24:00",
        "2026-11-01 12:00 +05:60",
        "2026-11-01 12:00 trailing",
        "0000-01-01",
        "26-11-01",
    ] {
        assert_eq!(parse_timestamp(text), None, "{text}");
    }
}

#[test]
fn leap_years_follow_the_calendar() {
    assert!(parse_timestamp("2024-02-29").is_some());
    assert!(parse_timestamp("2000-02-29").is_some());
    assert!(parse_timestamp("1900-02-29").is_none());
    assert_eq!(
        parse_timestamp("2024-03-01").unwrap() - parse_timestamp("2024-02-29").unwrap(),
        24 * MICROS_PER_HOUR
    );
}

#[test]
fn the_unix_epoch_is_zero() {
    assert_eq!(parse_timestamp("1970-01-01").unwrap(), 0);
    assert_eq!(format_timestamp(0), "1970-01-01 00:00:00");
    assert_eq!(format_timestamp(-1), "1969-12-31 23:59:59.999999");
}

#[test]
fn printing_drops_a_zero_fraction() {
    assert_eq!(format_timestamp(NOON), "2026-11-01 12:00:00");
    assert_eq!(format_timestamp(NOON + 1), "2026-11-01 12:00:00.000001");
    assert_eq!(
        format_timestamp(NOON + 500_000),
        "2026-11-01 12:00:00.500000"
    );
}

#[test]
fn a_manual_clock_only_moves_when_told() {
    let clock = ManualClock::new(100);
    assert_eq!(clock.now(), 100);
    assert_eq!(clock.now(), 100);
    clock.advance(5);
    assert_eq!(clock.now(), 105);
    clock.set(7);
    assert_eq!(clock.now(), 7);
}

#[test]
fn the_system_clock_is_after_this_code_was_written() {
    assert!(SystemClock.now() > parse_timestamp("2026-01-01").unwrap());
}

proptest! {
    #[test]
    fn printing_and_parsing_round_trip(
        // Years 0001 to 9999.
        micros in -62_135_596_800_000_000i64..253_402_300_800_000_000i64,
    ) {
        prop_assert_eq!(parse_timestamp(&format_timestamp(micros)), Some(micros));
    }
}
