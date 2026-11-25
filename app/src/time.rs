//! Times as people read them: in local time (the system's zone, summer
//! time included) or UTC, as `14:02`, `Thursday 15 October 2026`,
//! `15 Oct 14:02`, and lengths like `17 hours`.

/// Seconds east of UTC that local time is at `unix_secs` (summer time
/// included), from the system's time zone. 0 where that can't be found.
#[cfg(unix)]
fn local_offset(unix_secs: u64) -> i64 {
    let t = unix_secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

/// Windows: the same, by converting the moment to local time with the
/// system's zone rules and seeing how far it moved.
#[cfg(windows)]
fn local_offset(unix_secs: u64) -> i64 {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime};
    let Some(ticks) = unix_secs.checked_add(11_644_473_600).and_then(|t| t.checked_mul(10_000_000)) else { return 0 };
    let utc_ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        let mut local_ft: FILETIME = std::mem::zeroed();
        if FileTimeToSystemTime(&utc_ft, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
            || SystemTimeToFileTime(&local, &mut local_ft) == 0
        {
            return 0;
        }
        let local_ticks = ((local_ft.dwHighDateTime as u64) << 32) | local_ft.dwLowDateTime as u64;
        (local_ticks as i64 - ticks as i64) / 10_000_000
    }
}

#[cfg(not(any(unix, windows)))]
fn local_offset(_unix_secs: u64) -> i64 {
    0
}

/// The local zone's short name right now (`BST`, `CET`...), or its
/// offset from UTC when it has none.
#[cfg(unix)]
pub fn zone_name() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let t = now as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() || tm.tm_zone.is_null() {
        return offset_name(local_offset(now));
    }
    let name = unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned();
    if name.is_empty() || name.starts_with(['+', '-']) { offset_name(tm.tm_gmtoff as i64) } else { name }
}

/// Elsewhere (Windows): the offset, since Windows' zone names are long
/// ("GMT Summer Time", "AUS Eastern Daylight Time").
#[cfg(not(unix))]
pub fn zone_name() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    offset_name(local_offset(now))
}

fn offset_name(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.abs() / 60;
    format!("UTC{sign}{}:{:02}", minutes / 60, minutes % 60)
}

/// A moment broken into its parts, in local time or UTC.
pub struct When {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u64,
    pub minute: u64,
    /// 0 is Monday.
    pub weekday: u64,
    /// Days since 1970 — for telling whether two moments share a day.
    pub day_number: i64,
}

pub fn when(unix_secs: u64, utc: bool) -> When {
    let unix_secs = unix_secs.min(253_402_000_000);
    let shifted = if utc { unix_secs as i64 } else { unix_secs as i64 + local_offset(unix_secs) };
    let day_number = shifted.div_euclid(86_400);
    let secs = shifted.rem_euclid(86_400) as u64;
    let (year, month, day) = civil_from_days(day_number);
    When { year, month, day, hour: secs / 3600, minute: secs % 3600 / 60, weekday: (day_number + 3).rem_euclid(7) as u64, day_number }
}

/// Now as a log line's start, `14:02:07.381`, local time.
pub fn log_stamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let w = when(now.as_secs(), false);
    let shifted = now.as_secs() as i64 + local_offset(now.as_secs());
    format!("{:02}:{:02}:{:02}.{:03}", w.hour, w.minute, shifted.rem_euclid(60), now.subsec_millis())
}

/// `log`, each line starting with the time (`log_stamp`): a log's lines
/// lined up with the other side's, or a packet capture.
pub fn stamped(log: swarm_core::node::Logger) -> swarm_core::node::Logger {
    std::sync::Arc::new(move |line: String| {
        match line.strip_prefix('\n') {
            Some(rest) => log(format!("\n{} {rest}", log_stamp())),
            None => log(format!("{} {line}", log_stamp())),
        }
    })
}

/// `14:02`.
pub fn clock(unix_secs: u64, utc: bool) -> String {
    let w = when(unix_secs, utc);
    format!("{:02}:{:02}", w.hour, w.minute)
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/// `Thursday 15 October 2026`.
pub fn day_heading(unix_secs: u64, utc: bool) -> String {
    let w = when(unix_secs, utc);
    format!("{} {} {} {}", DAYS[w.weekday as usize], w.day, MONTHS[w.month as usize - 1], w.year)
}

/// `15 Oct 14:02` — for posts, which aren't grouped by day.
pub fn date_time(unix_secs: u64, utc: bool) -> String {
    let w = when(unix_secs, utc);
    format!("{} {} {:02}:{:02}", w.day, &MONTHS[w.month as usize - 1][..3], w.hour, w.minute)
}

/// `14:02` if it's today, `Thu 15 Oct 14:02` if not.
pub fn moment(unix_secs: u64, now: u64, utc: bool) -> String {
    let (then, today) = (when(unix_secs, utc), when(now, utc));
    if then.day_number == today.day_number {
        format!("{:02}:{:02}", then.hour, then.minute)
    } else {
        format!("{} {}", &DAYS[then.weekday as usize][..3], date_time(unix_secs, utc))
    }
}

/// A length of time as people say it: `a few seconds`, `5 minutes`,
/// `17 hours`, and from two days on in the largest unit with the next
/// one down: `8 days 9 hours`, `2 months 5 days`, `1 year 3 months`
/// (a month a twelfth of a year of 365 days).
pub fn how_long(seconds: u64) -> String {
    const DAY: u64 = 86_400;
    const MONTH: u64 = 365 * DAY / 12;
    const YEAR: u64 = 365 * DAY;
    let plural = |n: u64, unit: &str| if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") };
    let two = |big: u64, big_unit: &str, small: u64, small_unit: &str| {
        if small == 0 { plural(big, big_unit) } else { format!("{} {}", plural(big, big_unit), plural(small, small_unit)) }
    };
    match seconds {
        0..60 => "a few seconds".to_string(),
        60..3600 => plural(seconds / 60, "minute"),
        3600..172_800 => plural(seconds / 3600, "hour"),
        s if s < MONTH => two(s / DAY, "day", s % DAY / 3600, "hour"),
        s if s < YEAR => two(s / MONTH, "month", s % MONTH / DAY, "day"),
        s => two(s / YEAR, "year", s % YEAR / MONTH, "month"),
    }
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer stamps its messages and posts with any time it likes;
    /// every one of them can be shown, however absurd.
    #[test]
    fn any_time_at_all_can_be_shown() {
        for t in [0, 1, u64::MAX, u64::MAX / 2, i64::MAX as u64, i64::MAX as u64 + 1, 253_402_300_800, 1u64 << 40] {
            for utc in [true, false] {
                let _ = (clock(t, utc), day_heading(t, utc), date_time(t, utc), moment(t, 1_792_069_320, utc));
            }
            assert!(!day_heading(t, true).contains('-'), "{}", day_heading(t, true));
        }
    }

    #[test]
    fn dates_come_out_right() {
        let t = 1_792_069_320;
        assert_eq!(clock(t, true), "13:02");
        assert_eq!(day_heading(t, true), "Thursday 15 October 2026");
        assert_eq!(date_time(t, true), "15 Oct 13:02");
    }

    #[test]
    fn lengths_of_time_read_naturally() {
        assert_eq!(how_long(5), "a few seconds");
        assert_eq!(how_long(60), "1 minute");
        assert_eq!(how_long(17 * 3600 + 5), "17 hours");
        assert_eq!(how_long(3 * 86_400), "3 days");
        assert_eq!(how_long(201 * 3600), "8 days 9 hours");
        assert_eq!(how_long(45 * 86_400), "1 month 14 days");
        assert_eq!(how_long(365 * 86_400), "1 year");
        assert_eq!(how_long(800 * 86_400), "2 years 2 months");
        let t = 1_792_069_320;
        assert_eq!(moment(t, t + 60, true), "13:02");
        assert_eq!(moment(t, t + 86_400, true), "Thu 15 Oct 13:02");
    }
}

#[cfg(test)]
mod log_stamp_tests {
    #[test]
    fn a_stamp_is_hours_to_milliseconds_and_a_line_keeps_its_newline_first() {
        let s = super::log_stamp();
        assert_eq!(s.len(), 12, "{s}");
        assert!(s.chars().enumerate().all(|(i, c)| if [2, 5].contains(&i) { c == ':' } else if i == 8 { c == '.' } else { c.is_ascii_digit() }), "{s}");
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let into = got.clone();
        let log = super::stamped(std::sync::Arc::new(move |l: String| into.lock().unwrap().push(l)));
        log("\nhello".into());
        log("there".into());
        let got = got.lock().unwrap();
        assert!(got[0].starts_with('\n') && got[0].ends_with(" hello") && got[0].len() == 1 + 12 + 6);
        assert!(got[1].ends_with(" there") && got[1].len() == 12 + 6);
    }
}
