//! Date parsing for `dm:`, `dc:`, `da:` and `rc:` filters, plus the FILETIME
//! helpers used throughout the crate.
//!
//! Dates are stored the way NTFS stores them: 100 ns ticks since 1601-01-01.

pub const FT_PER_SEC: i64 = 10_000_000;
pub const FT_PER_MIN: i64 = 60 * FT_PER_SEC;
pub const FT_PER_HOUR: i64 = 60 * FT_PER_MIN;
pub const FT_PER_DAY: i64 = 24 * FT_PER_HOUR;
pub const UNIX_EPOCH_FT: i64 = 116_444_736_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub y: i64,
    pub m: u32,
    pub d: u32,
    pub h: u32,
    pub mi: u32,
    pub s: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LocalClock {
    pub now: Civil,
    /// local FILETIME minus UTC FILETIME at the moment of sampling.
    pub offset: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedDate {
    /// Half-open range `[start, end)` in FILETIME ticks (local wall clock).
    Range(i64, i64),
    /// The entry has no date at all.
    Unknown,
    Invalid,
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let m = m as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn filetime_from_civil(c: Civil) -> i64 {
    let days = days_from_civil(c.y, c.m, c.d);
    days * 86400 * FT_PER_SEC
        + c.h as i64 * FT_PER_HOUR
        + c.mi as i64 * FT_PER_MIN
        + c.s as i64 * FT_PER_SEC
        + UNIX_EPOCH_FT
}

pub fn civil_from_filetime(ft: i64) -> Civil {
    let mut t = (ft - UNIX_EPOCH_FT).div_euclid(FT_PER_SEC);
    let secs_of_day = t.rem_euclid(86400);
    t = t.div_euclid(86400);
    let (y, m, d) = civil_from_days(t);
    Civil {
        y,
        m,
        d,
        h: (secs_of_day / 3600) as u32,
        mi: ((secs_of_day % 3600) / 60) as u32,
        s: (secs_of_day % 60) as u32,
    }
}

pub fn local_clock() -> LocalClock {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTimeAsFileTime};
        unsafe {
            let mut st: SYSTEMTIME = std::mem::zeroed();
            GetLocalTime(&mut st);
            let mut ft: FILETIME = std::mem::zeroed();
            GetSystemTimeAsFileTime(&mut ft);
            let utc = ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64;
            let now = Civil {
                y: st.wYear as i64,
                m: st.wMonth as u32,
                d: st.wDay as u32,
                h: st.wHour as u32,
                mi: st.wMinute as u32,
                s: st.wSecond as u32,
            };
            LocalClock {
                now,
                offset: filetime_from_civil(now) - utc,
            }
        }
    }
    #[cfg(not(windows))]
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        LocalClock {
            now: civil_from_filetime(secs * FT_PER_SEC + UNIX_EPOCH_FT),
            offset: 0,
        }
    }
}

impl LocalClock {
    pub fn civil_to_ft(&self, c: Civil) -> i64 {
        filetime_from_civil(c) - self.offset
    }

    pub fn day_start(&self, y: i64, m: u32, d: u32) -> i64 {
        self.civil_to_ft(Civil { y, m, d, h: 0, mi: 0, s: 0 })
    }

    fn add_months(&self, y: i64, m: u32, delta: i64) -> (i64, u32) {
        let total = y * 12 + (m as i64 - 1) + delta;
        (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32)
    }

    fn month_start(&self, y: i64, m: u32) -> i64 {
        self.day_start(y, m, 1)
    }

    fn month_end(&self, y: i64, m: u32) -> i64 {
        let (ny, nm) = self.add_months(y, m, 1);
        self.day_start(ny, nm, 1)
    }
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

const MONTHS: [&str; 12] = [
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];

const MONTH_ABBR: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

pub fn parse_date(input: &str, clock: &LocalClock) -> ParsedDate {
    let s = input.trim().to_ascii_lowercase().replace('_', "");
    if s.is_empty() {
        return ParsedDate::Invalid;
    }
    if s == "unknown" {
        return ParsedDate::Unknown;
    }
    let now = clock.now;
    let (ty, tm, td) = (now.y, now.m, now.d);

    match s.as_str() {
        "today" => {
            let start = clock.day_start(ty, tm, td);
            return ParsedDate::Range(start, start + FT_PER_DAY);
        }
        "yesterday" => {
            let start = clock.day_start(ty, tm, td) - FT_PER_DAY;
            return ParsedDate::Range(start, start + FT_PER_DAY);
        }
        "tomorrow" => {
            let start = clock.day_start(ty, tm, td) + FT_PER_DAY;
            return ParsedDate::Range(start, start + FT_PER_DAY);
        }
        "thisweek" | "week" => {
            let dow = (days_from_civil(ty, tm, td) + 3).rem_euclid(7); // Monday = 0
            let start = clock.day_start(ty, tm, td) - dow * FT_PER_DAY;
            return ParsedDate::Range(start, start + 7 * FT_PER_DAY);
        }
        "lastweek" | "prevweek" | "previousweek" => {
            let dow = (days_from_civil(ty, tm, td) + 3).rem_euclid(7);
            let start = clock.day_start(ty, tm, td) - (dow + 7) * FT_PER_DAY;
            return ParsedDate::Range(start, start + 7 * FT_PER_DAY);
        }
        "nextweek" | "comingweek" => {
            let dow = (days_from_civil(ty, tm, td) + 3).rem_euclid(7);
            let start = clock.day_start(ty, tm, td) + (7 - dow) * FT_PER_DAY;
            return ParsedDate::Range(start, start + 7 * FT_PER_DAY);
        }
        "thismonth" | "month" => {
            return ParsedDate::Range(clock.month_start(ty, tm), clock.month_end(ty, tm));
        }
        "lastmonth" | "prevmonth" | "previousmonth" => {
            let (y, m) = clock.add_months(ty, tm, -1);
            return ParsedDate::Range(clock.month_start(y, m), clock.month_end(y, m));
        }
        "nextmonth" | "comingmonth" => {
            let (y, m) = clock.add_months(ty, tm, 1);
            return ParsedDate::Range(clock.month_start(y, m), clock.month_end(y, m));
        }
        "thisyear" | "year" => {
            return ParsedDate::Range(clock.day_start(ty, 1, 1), clock.day_start(ty + 1, 1, 1));
        }
        "lastyear" | "prevyear" | "previousyear" => {
            return ParsedDate::Range(
                clock.day_start(ty - 1, 1, 1),
                clock.day_start(ty, 1, 1),
            );
        }
        "nextyear" | "comingyear" => {
            return ParsedDate::Range(
                clock.day_start(ty + 1, 1, 1),
                clock.day_start(ty + 2, 1, 1),
            );
        }
        _ => {}
    }

    // <last|past|prev|next><n><unit>, e.g. last7days
    for prefix in ["last", "past", "prev", "previous", "this", "coming", "next"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if let Some((n, unit)) = split_amount(rest) {
                let sign = if prefix == "next" || prefix == "coming" {
                    1
                } else {
                    -1
                };
                let now_ft = clock.civil_to_ft(now);
                let delta = match unit {
                    "year" | "years" => {
                        let (y, m) = clock.add_months(ty, tm, sign * n as i64 * 12);
                        return ParsedDate::Range(
                            clock.day_start(y, m, 1),
                            clock.month_end(y, m),
                        );
                    }
                    "month" | "months" => {
                        let (y, m) = clock.add_months(ty, tm, sign * n as i64);
                        return ParsedDate::Range(
                            clock.day_start(y, m, 1),
                            clock.month_end(y, m),
                        );
                    }
                    "week" | "weeks" => sign * n as i64 * 7 * FT_PER_DAY,
                    "day" | "days" => sign * n as i64 * FT_PER_DAY,
                    "hour" | "hours" => sign * n as i64 * FT_PER_HOUR,
                    "minute" | "minutes" | "min" | "mins" => sign * n as i64 * FT_PER_MIN,
                    "second" | "seconds" | "sec" | "secs" => sign * n as i64 * FT_PER_SEC,
                    _ => continue,
                };
                let (start, end) = if delta < 0 {
                    (now_ft + delta, now_ft)
                } else {
                    (now_ft, now_ft + delta)
                };
                return ParsedDate::Range(start, end);
            }
        }
    }

    // Month names -> that month in the current year.
    for (i, name) in MONTHS.iter().enumerate() {
        if s == *name || s == MONTH_ABBR[i] {
            let m = i as u32 + 1;
            let y = if m > tm { ty - 1 } else { ty };
            return ParsedDate::Range(clock.month_start(y, m), clock.month_end(y, m));
        }
    }

    if s.as_bytes()[0].is_ascii_digit() {
        if let Some(r) = parse_numeric(&s, clock) {
            return r;
        }
    }

    ParsedDate::Invalid
}

fn split_amount(rest: &str) -> Option<(u32, &str)> {
    if rest.is_empty() {
        return None;
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        // "lastweek" style with no number is handled above; tolerate 1
        return Some((1, rest));
    }
    let n: u32 = digits.parse().ok()?;
    Some((n.max(1), &rest[digits.len()..]))
}

fn parse_numeric(s: &str, clock: &LocalClock) -> Option<ParsedDate> {
    let (date_part, time_part) = match s.split_once(['t', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let sep = if date_part.contains('-') {
        '-'
    } else if date_part.contains('/') {
        '/'
    } else if date_part.contains('.') {
        '.'
    } else {
        '\0'
    };
    let nums: Vec<i64> = if sep == '\0' {
        // compact: YYYYMMDD or YYYYMM
        let digits: String = date_part.chars().filter(|c| c.is_ascii_digit()).collect();
        match digits.len() {
            4 => vec![digits.parse().ok()?],
            6 => vec![
                digits[0..4].parse().ok()?,
                digits[4..6].parse().ok()?,
            ],
            8 => vec![
                digits[0..4].parse().ok()?,
                digits[4..6].parse().ok()?,
                digits[6..8].parse().ok()?,
            ],
            _ => return None,
        }
    } else {
        date_part
            .split(sep)
            .map(|p| p.parse::<i64>().ok())
            .collect::<Option<Vec<_>>>()?
    };
    if nums.is_empty() || nums[0] < 1601 {
        return None;
    }
    let (y, m, d) = match nums.len() {
        1 => (nums[0], 1, 1),
        2 => (nums[0], nums[1] as u32, 1),
        _ => (nums[0], nums[1] as u32, nums[2] as u32),
    };
    if m == 0 || m > 12 || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    let (mut h, mut mi, mut sec) = (0u32, 0u32, 0u32);
    if let Some(t) = time_part {
        let t = t.trim_end_matches('z');
        let cleaned: String = t.chars().filter(|c| c.is_ascii_digit() || *c == ':').collect();
        let parts: Vec<&str> = cleaned.split(':').filter(|p| !p.is_empty()).collect();
        if let Some(p) = parts.first() {
            h = p.parse().ok()?;
        }
        if let Some(p) = parts.get(1) {
            mi = p.parse().ok()?;
        }
        if let Some(p) = parts.get(2) {
            sec = p.parse().ok()?;
        }
        if h > 23 || mi > 59 || sec > 59 {
            return None;
        }
    }
    let start = clock.day_start(y, m, d)
        + h as i64 * FT_PER_HOUR
        + mi as i64 * FT_PER_MIN
        + sec as i64 * FT_PER_SEC;
    let end = if time_part.is_some() {
        start + FT_PER_SEC
    } else if nums.len() == 1 {
        clock.day_start(y + 1, 1, 1)
    } else if nums.len() == 2 {
        clock.month_end(y, m)
    } else {
        clock.day_start(y, m, d) + FT_PER_DAY
    };
    Some(ParsedDate::Range(start, end))
}

/// Render a FILETIME for the result list. Empty string for "no date".
pub fn format_filetime(ft: i64, clock: &LocalClock) -> String {
    if ft <= 0 {
        return String::new();
    }
    let c = civil_from_filetime(ft + clock.offset);
    if c.y < 1601 || c.y > 9999 {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        c.y, c.m, c.d, c.h, c.mi
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_civil() {
        for z in [-1000i64, 0, 1, 8000, 20000, 750000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn filetime_roundtrip() {
        let ft = filetime_from_civil(Civil {
            y: 2024,
            m: 5,
            d: 3,
            h: 12,
            mi: 34,
            s: 56,
        });
        let c = civil_from_filetime(ft);
        assert_eq!((c.y, c.m, c.d, c.h, c.mi), (2024, 5, 3, 12, 34));
    }

    #[test]
    fn parses_dates() {
        let clock = local_clock();
        assert!(matches!(
            parse_date("2024-05-03", &clock),
            ParsedDate::Range(..)
        ));
        assert!(matches!(parse_date("today", &clock), ParsedDate::Range(..)));
        assert!(matches!(
            parse_date("last7days", &clock),
            ParsedDate::Range(..)
        ));
        assert_eq!(parse_date("unknown", &clock), ParsedDate::Unknown);
        assert_eq!(parse_date("nonsense", &clock), ParsedDate::Invalid);
    }
}
