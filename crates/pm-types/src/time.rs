//! Exchange timestamps as integer Unix nanoseconds.
//!
//! Venue stamps carry up to 9 fractional digits, which is why ordering uses
//! integers rather than a float or a microsecond type.

pub const NS_PER_SEC: i64 = 1_000_000_000;
pub const NS_PER_MS: i64 = 1_000_000;

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// RFC 3339 (`2026-10-21T03:46:16.483269046Z`, `…+00:00`, or a space instead of `T`) → Unix ns.
/// Impossible dates or clock values, offsets other than `±HH[:MM]`, and times outside the
/// `i64` ns range give `None`; malformed input never panics.
pub fn parse_rfc3339_ns(text: &str) -> Option<i64> {
    let s = text.trim();
    if s.len() < 19 || !s.is_ascii() {
        return None;
    }
    let (date, rest) = s.split_at(10);
    let sep = rest.as_bytes()[0];
    if sep != b'T' && sep != b't' && sep != b' ' {
        return None;
    }
    let rest = &rest[1..];
    let mut parts = date.split('-');
    let (y, m, d) = (
        num(parts.next()?)?,
        num(parts.next()?)?,
        num(parts.next()?)?,
    );
    if rest.len() < 8 {
        return None;
    }
    let (clock, mut tail) = rest.split_at(8);
    let mut hms = clock.split(':');
    let (hh, mm, ss) = (num(hms.next()?)?, num(hms.next()?)?, num(hms.next()?)?);
    if !(1..=12).contains(&m)
        || !(1..=days_in_month(y, m)).contains(&d)
        || hh > 23
        || mm > 59
        || ss > 59
    {
        return None;
    }
    let mut frac_ns = 0i64;
    if let Some(after_dot) = tail.strip_prefix('.') {
        let digits = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let frac = &after_dot[..digits];
        for i in 0..9 {
            let digit = frac.as_bytes().get(i).map_or(0, |b| (b - b'0') as i64);
            frac_ns = frac_ns * 10 + digit;
        }
        tail = &after_dot[digits..];
    }
    let offset_secs = match tail {
        "Z" | "z" | "" => 0,
        _ => {
            let sign = match tail.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (oh, om) = tail[1..].split_once(':').unwrap_or((&tail[1..], "00"));
            let (oh, om) = (num(oh)?, num(om)?);
            if tail[1..].len() > 5 || oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    // Components are range-checked and the year has at most 4 digits, so only the ns
    // conversion can overflow (years after 2262).
    let secs = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss - offset_secs;
    secs.checked_mul(NS_PER_SEC)?.checked_add(frac_ns)
}

/// Unix ns → `YYYY-MM-DDTHH:MM:SSZ` (whole seconds, as `goodTillTime` expects).
pub fn format_rfc3339_secs(ns: i64) -> String {
    let secs = ns.div_euclid(NS_PER_SEC);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Unix ns → `YYYY-MM-DDTHH:MM:SS.ffffffZ`, for journals.
pub fn format_rfc3339_micros(ns: i64) -> String {
    let base = format_rfc3339_secs(ns);
    format!("{}.{:06}Z", &base[..19], ns.rem_euclid(NS_PER_SEC) / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_venue_stamps() {
        let a = parse_rfc3339_ns("2026-10-21T03:46:16.483269046Z").unwrap();
        assert_eq!(a % NS_PER_SEC, 483_269_046);
        assert_eq!(format_rfc3339_secs(a), "2026-10-21T03:46:16Z");
        let b = parse_rfc3339_ns("2026-10-21T05:46:16.483269046+02:00").unwrap();
        assert_eq!(a, b);
        let c = parse_rfc3339_ns("2026-08-05 02:32:23.085700+00:00").unwrap();
        assert_eq!(format_rfc3339_micros(c), "2026-08-05T02:32:23.085700Z");
        assert_eq!(parse_rfc3339_ns("1970-01-01T00:00:00Z"), Some(0));
        assert!(parse_rfc3339_ns("not a time").is_none());
        assert!(parse_rfc3339_ns("2024-02-29T00:00:00Z").is_some());
        assert_eq!(
            parse_rfc3339_ns("2026-10-21T05:46:16+02"),
            parse_rfc3339_ns("2026-10-21T03:46:16Z")
        );
    }

    #[test]
    fn rejects_malformed_stamps_without_panicking() {
        for s in [
            "123456789éT00:00:00Z",
            "2026-10-21T03:46:1éZ",
            "3000-01-01T00:00:00Z",
            "2026-10-21T03:46:16+999999999999999999:00",
            "2026-02-30T00:00:00Z",
            "2025-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T99:99:99Z",
            "2026-10-21T05:46:16+0200",
            "2026-10-21T05:46:16+24:00",
            "2026-10-21T05:46:16+02:60",
            "2026-10-21T05:46:16+",
        ] {
            assert_eq!(parse_rfc3339_ns(s), None, "{s}");
        }
    }
}
