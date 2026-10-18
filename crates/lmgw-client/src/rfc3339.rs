//! RFC 3339 times as unix milliseconds, for the dates lmgw writes (an
//! enrolment's `not_after` and `notice_after`) without a date crate or a
//! clock.

/// `2027-10-14T14:03:11Z` (a fraction of a second and a `±HH:MM` offset
/// allowed, `T` and `Z` in either case) as milliseconds since the unix
/// epoch; `None` for anything else, a day the month does not have included.
/// A leap second (`:60`) reads as the second before it.
pub(crate) fn unix_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let digits = b.get(from..from + len)?;
        digits
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| digits.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    };
    let at = |i: usize, c: u8| b.get(i).is_some_and(|x| x.eq_ignore_ascii_case(&c));
    if !(at(4, b'-') && at(7, b'-') && at(10, b'T') && at(13, b':') && at(16, b':')) {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut i = 19;
    let mut millis = 0;
    if at(i, b'.') {
        let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        // Milliseconds: the first three digits, the rest truncated.
        let ms = b[i + 1..i + 1 + digits.min(3)]
            .iter()
            .fold(0, |n, d| n * 10 + i64::from(d - b'0'));
        millis = ms * 10_i64.pow(3 - digits.min(3) as u32);
        i += 1 + digits;
    }
    let offset_s = if at(i, b'Z') && b.len() == i + 1 {
        0
    } else if b.len() == i + 6 && matches!(b[i], b'+' | b'-') && at(i + 3, b':') {
        let (h, m) = (num(i + 1, 2)?, num(i + 4, 2)?);
        if h > 23 || m > 59 {
            return None;
        }
        let o = h * 3600 + m * 60;
        if b[i] == b'-' {
            -o
        } else {
            o
        }
    } else {
        return None;
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second.min(59) - offset_s;
    Some(secs * 1000 + millis)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_as_unix_milliseconds() {
        assert_eq!(unix_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_ms("2027-10-14T14:03:11Z"), Some(1_823_522_591_000));
        assert_eq!(unix_ms("2027-10-14t14:03:11z"), Some(1_823_522_591_000));
        assert_eq!(
            unix_ms("2027-10-14T16:03:11+02:00"),
            Some(1_823_522_591_000)
        );
        assert_eq!(unix_ms("2027-10-14T14:03:11.25Z"), Some(1_823_522_591_250));
        assert_eq!(
            unix_ms("2027-10-14T14:03:11.123456Z"),
            Some(1_823_522_591_123)
        );
        assert_eq!(unix_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        assert_eq!(unix_ms("1969-12-31T23:59:59Z"), Some(-1000));
        assert_eq!(unix_ms("9999-12-31T23:59:59Z"), Some(253_402_300_799_000));
        for bad in [
            "",
            "2027-10-14",
            "2027-10-14T14:03:11",
            "2027-10-14 14:03:11Z",
            "2023-02-29T00:00:00Z",
            "2027-13-01T00:00:00Z",
            "2027-10-14T24:00:00Z",
            "2027-10-14T14:03:11.Z",
            "2027-10-14T14:03:11+0200",
            "2027-10-14T14:03:11Zjunk",
            "２０２７-10-14T14:03:11Z",
        ] {
            assert_eq!(unix_ms(bad), None, "{bad}");
        }
    }
}
