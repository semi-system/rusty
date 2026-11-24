//! Dates, date-times and durations of iCalendar (RFC 5545 sections 3.3.4 to 3.3.6) as seconds since the epoch. There is no time zone database here: a value with a zone (or none: floating) is read as if it were
//! UTC and the caller widens the bounds it computes by [`ZONE_SLACK`], so that a range query on the bounds never misses an event (it may return one the client then drops).

/// The largest distance between a zone and UTC (UTC+14 and UTC-12): the widening of the bounds of an event whose zone is not known.
pub const ZONE_SLACK: i64 = 14 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    pub secs: i64,
    /// Written in UTC (`Z`) or a date with no time: the value is exact. Otherwise the zone is unknown.
    pub exact: bool,
    /// A date with no time of day.
    pub date_only: bool,
}

/// Days from 1970-01-01 of a civil date (the algorithm of H. Hinnant); `None` for a date that does not exist.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || d < 1 {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap => 29,
        _ => 28,
    };
    if d > dim {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `YYYYMMDD` or `YYYYMMDDTHHMMSS` with an optional `Z`. A leap second (60) counts as 59.
pub fn parse_moment(value: &str) -> Option<Moment> {
    let (day, time) = match value.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (value, None),
    };
    if day.len() != 8 {
        return None;
    }
    let days = days_from_civil(num(day.get(..4)?)?, num(day.get(4..6)?)?, num(day.get(6..8)?)?)?;
    let Some(time) = time else { return Some(Moment { secs: days * 86_400, exact: true, date_only: true }) };
    let (clock, utc) = match time.strip_suffix('Z') {
        Some(c) => (c, true),
        None => (time, false),
    };
    if clock.len() != 6 {
        return None;
    }
    let (h, mi, s) = (num(clock.get(..2)?)?, num(clock.get(2..4)?)?, num(clock.get(4..6)?)?);
    if h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(Moment { secs: days * 86_400 + h * 3600 + mi * 60 + s.min(59), exact: utc, date_only: false })
}

/// `[+-]P` then weeks, or days and `T` hours, minutes and seconds (`P1W`, `PT1H30M`, `P1DT2H`).
pub fn parse_duration(value: &str) -> Option<i64> {
    let (sign, rest) = match value.as_bytes().first()? {
        b'-' => (-1, value.get(1..)?),
        b'+' => (1, value.get(1..)?),
        _ => (1, value),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (rest, None),
    };
    let mut total: i64 = 0;
    let mut seen = false;
    let mut digits = String::new();
    for c in date.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n = num(&digits)?;
        digits.clear();
        total = total.checked_add(n.checked_mul(match c {
            'W' => 7 * 86_400,
            'D' => 86_400,
            _ => return None,
        })?)?;
        seen = true;
    }
    if !digits.is_empty() {
        return None;
    }
    if let Some(time) = time {
        let mut any = false;
        for c in time.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
                continue;
            }
            let n = num(&digits)?;
            digits.clear();
            total = total.checked_add(n.checked_mul(match c {
                'H' => 3600,
                'M' => 60,
                'S' => 1,
                _ => return None,
            })?)?;
            any = true;
        }
        if !digits.is_empty() || !any {
            return None;
        }
        seen = true;
    }
    if !seen || total > 1000 * 366 * 86_400 {
        return None;
    }
    Some(sign * total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moments_of_the_known_dates() {
        assert_eq!(parse_moment("19700101T000000Z").unwrap().secs, 0);
        assert_eq!(parse_moment("20000229T120000Z").unwrap().secs, 951_825_600);
        assert_eq!(parse_moment("20261124T055052Z").unwrap().secs, 1_795_499_452);
        let d = parse_moment("20240101").unwrap();
        assert_eq!((d.secs, d.date_only, d.exact), (1_704_067_200, true, true));
        let f = parse_moment("20240101T093000").unwrap();
        assert_eq!((f.secs, f.exact), (1_704_067_200 + 9 * 3600 + 1800, false));
        assert_eq!(parse_moment("19691231T235959Z").unwrap().secs, -1);
        assert_eq!(parse_moment("20240101T235960Z").unwrap().secs, parse_moment("20240101T235959Z").unwrap().secs);
    }

    #[test]
    fn dates_and_clocks_that_do_not_exist_are_refused() {
        for bad in ["", "2024", "20240230", "20230229", "20241301", "20240100", "20240101T2400", "20240101T240000", "20240101T006000", "2024-01-01", "20240101T09300", "20240101T093000ZZ", "x0240101", "20240101t093000"] {
            assert!(parse_moment(bad).is_none(), "{bad}");
        }
        assert!(parse_moment("20240229").is_some());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT1H30M"), Some(5400));
        assert_eq!(parse_duration("P1W"), Some(604_800));
        assert_eq!(parse_duration("P1DT2H"), Some(93_600));
        assert_eq!(parse_duration("-PT15M"), Some(-900));
        assert_eq!(parse_duration("+P2D"), Some(172_800));
        for bad in ["", "P", "PT", "1H", "P1H", "PT1D", "P1", "PT1", "P1W2", "P99999999999999999999D", "PT-1H"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }
}
