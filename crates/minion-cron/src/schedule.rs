//! Cron expressions read in a named timezone (FR-18, SDD §5.7).
//!
//! Two things are deliberately not left to chance here. The expression is
//! required to have exactly five fields — `cron` accepts an optional seconds
//! field, and silently honouring a six-field string would make `0 9 * * 1` and
//! `0 0 9 * * 1` mean different things to different users. And every instant is
//! converted through the job's IANA timezone, so `0 9 * * *` in
//! `America/Mexico_City` stays at 09:00 local across a DST shift instead of
//! drifting an hour in UTC.

use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use cron::Schedule;

use minion_core::error::{Error, Result};

/// Read an IANA timezone name.
pub fn timezone(name: &str) -> Result<Tz> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(Error::Config(
            "a timezone is required; try UTC or America/Mexico_City".to_string(),
        ));
    }
    trimmed.parse::<Tz>().map_err(|_| {
        Error::Config(format!(
            "`{trimmed}` is not an IANA timezone; try UTC or America/Mexico_City"
        ))
    })
}

/// Parse a five-field cron expression.
///
/// Two translations happen here, and both matter.
///
/// The `cron` crate's own grammar puts seconds first, so a five-field schedule
/// is prefixed with `0 ` — every SDD expression fires on the zeroth second of
/// its minute, which is what a five-field cron means everywhere else.
///
/// And the crate numbers the days of the week the Quartz way (`1` = Sunday,
/// `2` = Monday), where every other cron numbers them the Vixie way (`0` and `7`
/// = Sunday, `1` = Monday). Left alone, `0 9 * * 1` would fire on Sunday. The
/// day-of-week field is therefore rewritten into the crate's numbering, so the
/// expression a user types means what a user expects. Names (`mon`, `fri`) and a
/// bare `*` need no rewrite: names agree, and a wildcard offset is uniform.
pub fn expression(raw: &str) -> Result<Schedule> {
    let trimmed = raw.trim();
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(Error::Config(format!(
            "`{raw}` has {} fields; a schedule is 5: minute hour day-of-month month day-of-week",
            fields.len()
        )));
    }
    let normalized = format!(
        "0 {} {} {} {} {}",
        fields[0],
        fields[1],
        fields[2],
        fields[3],
        translate_day_of_week(fields[4])?
    );
    Schedule::from_str(&normalized)
        .map_err(|err| Error::Config(format!("`{raw}` is not a valid cron expression: {err}")))
}

/// Rewrite a Vixie day-of-week field into the numbering the parser expects.
fn translate_day_of_week(field: &str) -> Result<String> {
    let mut rewritten = Vec::new();
    for element in field.split(',') {
        rewritten.push(translate_day_of_week_element(element.trim())?);
    }
    Ok(rewritten.join(","))
}

fn translate_day_of_week_element(element: &str) -> Result<String> {
    if element.is_empty() {
        return Err(Error::Config(
            "the day-of-week field has an empty element".to_string(),
        ));
    }
    // `?` and `*` mean "any day"; so does `*/n`, whose offset is uniform, so the
    // set of days it selects is the same in either numbering.
    if element == "?" || element == "*" {
        return Ok(element.to_string());
    }
    let (body, step) = match element.split_once('/') {
        Some((body, step)) => (body, Some(step)),
        None => (element, None),
    };
    let with_step = |mapped: String| match step {
        Some(step) => format!("{mapped}/{step}"),
        None => mapped,
    };

    if body.chars().any(|c| c.is_ascii_alphabetic()) {
        // `mon` is Monday in both conventions.
        return Ok(with_step(body.to_string()));
    }
    if body == "*" {
        return Ok(with_step("*".to_string()));
    }

    let mapped = match body.split_once('-') {
        Some((from, to)) => {
            let (from, to) = (day_number(from)?, day_number(to)?);
            if from > to {
                return Err(Error::Config(format!(
                    "the day-of-week range `{element}` wraps around, which cron does not support"
                )));
            }
            format!("{from}-{to}")
        }
        None => day_number(body)?.to_string(),
    };
    Ok(with_step(mapped))
}

/// Map a Vixie day number onto the parser's ordinal.
fn day_number(raw: &str) -> Result<u64> {
    let value: u64 = raw.trim().parse().map_err(|_| {
        Error::Config(format!(
            "`{raw}` is not a day of the week; use 0-7 (Sunday is 0 or 7) or a name"
        ))
    })?;
    if value > 7 {
        return Err(Error::Config(format!(
            "day of week {value} is out of range; use 0-7, where 0 and 7 are Sunday"
        )));
    }
    Ok(if value == 0 || value == 7 {
        1
    } else {
        value + 1
    })
}

/// The first occurrence strictly after `after`.
///
/// `None` means the expression never fires again, which FR-18 says must be
/// rejected at creation time rather than stored as a job that does nothing.
pub fn next_after(schedule: &Schedule, zone: Tz, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    schedule
        .after(&after.with_timezone(&zone))
        .next()
        .map(|at| at.with_timezone(&Utc))
}

/// The first occurrence strictly after `after`, or an error naming the expression.
pub fn next_after_or_error(
    schedule: &Schedule,
    zone: Tz,
    after: DateTime<Utc>,
    raw: &str,
) -> Result<DateTime<Utc>> {
    next_after(schedule, zone, after).ok_or_else(|| {
        Error::Config(format!(
            "`{raw}` has no occurrence after {}; it would never fire",
            minion_core::job::stamp(after)
        ))
    })
}

/// Occurrences `o` with `from <= o <= until`, at most `cap` of them.
///
/// Used by catch-up: the occurrences between the job's recorded `next_run_at`
/// and now. The iterator starts one second before `from` because `cron`'s
/// `after` is exclusive, and occurrences sit on whole seconds.
pub fn occurrences(
    schedule: &Schedule,
    zone: Tz,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    cap: usize,
) -> Vec<DateTime<Utc>> {
    if cap == 0 || from > until {
        return Vec::new();
    }
    let start = from - Duration::seconds(1);
    let mut found = Vec::new();
    for at in schedule.after(&start.with_timezone(&zone)) {
        let at = at.with_timezone(&Utc);
        if at > until || found.len() >= cap {
            break;
        }
        found.push(at);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn a_five_field_expression_is_accepted_and_a_six_field_one_is_not() {
        assert!(expression("0 9 * * 1").is_ok());
        // The seconds field is what `cron` would otherwise accept silently.
        assert!(expression("0 0 9 * * 1").is_err());
        assert!(expression("nonsense").is_err());
        assert!(expression("* * * *").is_err());
    }

    #[test]
    fn the_zone_is_required_and_must_be_iana() {
        assert!(timezone("America/Mexico_City").is_ok());
        assert!(timezone("UTC").is_ok());
        assert!(timezone("Mars/Olympus").is_err());
        assert!(timezone("  ").is_err());
    }

    /// The numbers must mean what a Vixie cron means by them: `1` is Monday and
    /// `0`/`7` are Sunday. The parser underneath numbers them the Quartz way
    /// (`1` = Sunday), so without the rewrite every schedule would be a day out.
    #[test]
    fn the_day_of_week_numbers_follow_the_usual_cron_convention() {
        let zone = timezone("UTC").unwrap();
        // 2026-03-04 is a Wednesday.
        let from = at(2026, 3, 4, 12, 0);
        let weekday = |raw: &str| {
            let next = next_after(&expression(raw).unwrap(), zone, from).unwrap();
            next.with_timezone(&zone).date_naive().to_string()
        };

        assert_eq!(weekday("0 9 * * 1"), "2026-03-09", "1 is Monday");
        assert_eq!(weekday("0 9 * * mon"), "2026-03-09", "mon is Monday");
        assert_eq!(weekday("0 9 * * 0"), "2026-03-08", "0 is Sunday");
        assert_eq!(weekday("0 9 * * 7"), "2026-03-08", "7 is Sunday too");
        assert_eq!(weekday("0 9 * * 5"), "2026-03-06", "5 is Friday");
        assert_eq!(
            weekday("0 9 * * 1-5"),
            "2026-03-05",
            "1-5 is Thursday, the first weekday after Wednesday"
        );
        assert_eq!(weekday("0 9 * * 0,6"), "2026-03-07", "0,6 is Saturday");
    }

    #[test]
    fn an_impossible_day_number_is_rejected() {
        assert!(expression("0 9 * * 8").is_err());
        assert!(expression("0 9 * * six").is_err());
        assert!(
            expression("0 9 * * 6-1").is_err(),
            "wrap-around ranges are refused"
        );
    }

    /// The exit criterion of M4: a weekly job, read in a real timezone, lands on
    /// the next Monday at 09:00 local.
    #[test]
    fn a_weekly_expression_resolves_in_its_own_zone() {
        let schedule = expression("0 9 * * 1").unwrap();
        let zone = timezone("America/Mexico_City").unwrap();

        // 2026-03-04 is a Wednesday; the next Monday is 2026-03-09.
        let next = next_after(&schedule, zone, at(2026, 3, 4, 12, 0)).unwrap();

        assert_eq!(
            next.with_timezone(&zone).to_string(),
            "2026-03-09 09:00:00 CST"
        );
    }

    /// A nonexistent local time (the spring-forward gap) must still produce an
    /// answer: `chrono-tz` resolves it to the instant the clock jumped.
    #[test]
    fn a_local_time_inside_the_spring_forward_gap_still_resolves() {
        let schedule = expression("30 2 * * *").unwrap();
        let zone = timezone("America/New_York").unwrap();

        // 2026-03-08 02:00–03:00 local does not exist in New York.
        let next = next_after(&schedule, zone, at(2026, 3, 8, 5, 0)).unwrap();

        assert_eq!(
            next.with_timezone(&zone).to_string(),
            "2026-03-09 02:30:00 EDT"
        );
    }

    #[test]
    fn missed_occurrences_are_the_window_between_two_instants() {
        let schedule = expression("0 9 * * *").unwrap();
        let zone = timezone("UTC").unwrap();

        let found = occurrences(
            &schedule,
            zone,
            at(2026, 3, 2, 9, 0),
            at(2026, 3, 5, 9, 0),
            10,
        );

        assert_eq!(
            found.len(),
            4,
            "the 2nd, 3rd, 4th and 5th all count: {found:?}"
        );
        assert_eq!(found[0], at(2026, 3, 2, 9, 0));
        assert_eq!(found[3], at(2026, 3, 5, 9, 0));
    }

    #[test]
    fn the_safety_cap_bounds_the_window() {
        let schedule = expression("0 9 * * *").unwrap();
        let zone = timezone("UTC").unwrap();

        let found = occurrences(
            &schedule,
            zone,
            at(2026, 1, 1, 9, 0),
            at(2026, 12, 31, 9, 0),
            3,
        );

        assert_eq!(found.len(), 3);
    }

    #[test]
    fn an_expression_with_no_future_occurrence_is_an_error_when_asked_for_one() {
        let schedule = expression("0 9 30 2 *").unwrap();
        let zone = timezone("UTC").unwrap();

        // February 30th never exists, so `next_after` returns nothing.
        let err =
            next_after_or_error(&schedule, zone, at(2026, 3, 1, 0, 0), "0 9 30 2 *").unwrap_err();

        assert!(err.to_string().contains("never fire"), "was: {err}");
    }
}
