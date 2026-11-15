//! Calendar arithmetic for the three time formats of S3: the `x-amz-date` of a signature, the HTTP date of `Last-Modified` and `Date` headers, and the ISO 8601 of a listing.

/// Days since 1970-01-01 of a civil date (proleptic Gregorian; the days-from-civil algorithm of H. Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `(20130524T000000Z, 20130524)`: the `x-amz-date` value and the date of the credential scope, for unix seconds `t`.
pub fn amz_dates(t: i64) -> (String, String) {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let day = format!("{y:04}{m:02}{d:02}");
    (format!("{day}T{:02}{:02}{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60), day)
}

fn month(name: &str) -> Option<i64> {
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    M.iter().position(|m| m.eq_ignore_ascii_case(name)).map(|i| i as i64 + 1)
}

fn hms(s: &str) -> Option<i64> {
    let mut p = s.split(':');
    let (h, m, sec): (i64, i64, i64) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
    (p.next().is_none() && (0..24).contains(&h) && (0..60).contains(&m) && (0..61).contains(&sec)).then_some(h * 3600 + m * 60 + sec)
}

/// `Sun, 15 Nov 2026 09:50:49 GMT` (the IMF-fixdate of HTTP) to unix seconds.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let rest = s.split_once(", ")?.1;
    let mut p = rest.split(' ');
    let day: i64 = p.next()?.parse().ok()?;
    let mon = month(p.next()?)?;
    let year: i64 = p.next()?.parse().ok()?;
    let time = hms(p.next()?)?;
    (p.next()? == "GMT" && p.next().is_none() && (1..=31).contains(&day)).then(|| days_from_civil(year, mon, day) * 86_400 + time)
}

/// `2026-11-15T09:50:49.123Z` (the listing's `LastModified`) to unix seconds, the fraction dropped.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let time = hms(time.split('.').next()?)?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    (d.next().is_none() && (1..=12).contains(&m) && (1..=31).contains(&day)).then(|| days_from_civil(y, m, day) * 86_400 + time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_formats_agree_with_known_instants() {
        assert_eq!(amz_dates(1_369_353_600), ("20130524T000000Z".into(), "20130524".into()));
        assert_eq!(amz_dates(0), ("19700101T000000Z".into(), "19700101".into()));
        assert_eq!(amz_dates(951_782_400 + 86_399), ("20000229T235959Z".into(), "20000229".into()), "a leap day");
        assert_eq!(parse_http_date("Fri, 24 May 2013 00:00:00 GMT"), Some(1_369_353_600));
        assert_eq!(parse_http_date("Sun, 15 Nov 2026 09:50:49 GMT"), Some(1_794_736_249));
        assert_eq!(parse_iso8601("2013-05-24T00:00:00.000Z"), Some(1_369_353_600));
        assert_eq!(parse_iso8601("2026-11-15T09:50:49Z"), Some(1_794_736_249));
        for bad in ["", "yesterday", "Fri, 24 Foo 2013 00:00:00 GMT", "Fri, 24 May 2013 25:00:00 GMT", "Fri, 24 May 2013 00:00:00 UTC"] {
            assert_eq!(parse_http_date(bad), None, "{bad}");
        }
        for bad in ["", "2013-05-24", "2013-13-24T00:00:00Z", "2013-05-24T00:00:00"] {
            assert_eq!(parse_iso8601(bad), None, "{bad}");
        }
    }

    #[test]
    fn every_few_hours_of_four_years_round_trip() {
        for t in (0..4 * 366 * 86_400).step_by(3600 * 7) {
            let (stamp, day) = amz_dates(t);
            assert_eq!(&stamp[..8], day);
            let back = parse_iso8601(&format!("{}-{}-{}T{}:{}:{}Z", &stamp[0..4], &stamp[4..6], &stamp[6..8], &stamp[9..11], &stamp[11..13], &stamp[13..15])).unwrap();
            assert_eq!(back, t);
        }
    }
}
