//! GitHub's timestamps (RFC 3339) and short ages, without a date library.

/// Days from 1970-01-01 to the given civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since the Unix epoch for `2026-09-08T23:21:26Z`, with optional
/// fractional seconds and a `Z` or `+hh:mm`/`-hh:mm` offset. `None` for
/// anything else, including GitHub's zero time `0001-01-01T00:00:00Z`.
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &r[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) && &o[3..4] == ":" => {
            let oh: i64 = o[1..3].parse().ok()?;
            let om: i64 = o[4..6].parse().ok()?;
            let v = oh * 3600 + om * 60;
            if o.starts_with('+') { v } else { -v }
        }
        _ => return None,
    };
    if y < 1970 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset)
}

/// A short age: `40s`, `5m`, `3h`, `12d`, `8mo`, `2y`. A time in the
/// future (clock skew) reads `now`.
pub fn age_label(then: i64, now: i64) -> String {
    let s = now - then;
    if s < 0 {
        return "now".into();
    }
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        86_400..2_592_000 => format!("{}d", s / 86_400),
        2_592_000..31_536_000 => format!("{}mo", s / 2_592_000),
        _ => format!("{}y", s / 31_536_000),
    }
}

/// `HH:MM` in UTC for a Unix time.
pub fn utc_clock(secs: i64) -> String {
    let day = secs.rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, day % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_times() {
        // `date -u -d 2026-09-08T23:21:26Z +%s`
        assert_eq!(parse_time("2026-09-08T23:21:26Z"), Some(1_788_909_686));
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2024-02-29T12:00:00.123Z"), Some(1_709_208_000));
        assert_eq!(parse_time("2024-02-29T14:00:00+02:00"), Some(1_709_208_000));
        assert_eq!(parse_time("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("2024-13-01T00:00:00Z"), None);
        assert_eq!(parse_time("2024-01-01T00:00:00"), None);
    }

    #[test]
    fn ages_and_clock() {
        assert_eq!(age_label(100, 140), "40s");
        assert_eq!(age_label(0, 3599), "59m");
        assert_eq!(age_label(0, 7200), "2h");
        assert_eq!(age_label(0, 86_400 * 12), "12d");
        assert_eq!(age_label(0, 86_400 * 70), "2mo");
        assert_eq!(age_label(0, 86_400 * 800), "2y");
        assert_eq!(age_label(10, 0), "now");
        assert_eq!(utc_clock(1_788_909_686), "23:21");
    }
}
