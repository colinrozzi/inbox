//! Minimal RFC 2822 date formatter. Takes Unix epoch milliseconds and
//! produces strings like `Mon, 11 May 2026 00:35:00 +0000`. UTC only.

use alloc::format;
use alloc::string::String;

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn format_date(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let days = (secs / 86400) as i64;
    let sod = secs % 86400;
    let hour = sod / 3600;
    let minute = (sod / 60) % 60;
    let second = sod % 60;

    // 1970-01-01 was Thursday (index 4 in DAY_NAMES).
    let dow = ((days + 4).rem_euclid(7)) as usize;
    let (year, month, day) = days_to_ymd(days);

    format!(
        "{}, {} {} {:04} {:02}:{:02}:{:02} +0000",
        DAY_NAMES[dow],
        day,
        MONTH_NAMES[(month - 1) as usize],
        year,
        hour,
        minute,
        second
    )
}

/// Civil-from-days algorithm (Howard Hinnant). Treats months Mar..Feb as the
/// civil year, then unshifts at the end. Handles leap years exactly.
fn days_to_ymd(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468; // shift origin to 0000-03-01
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe.wrapping_sub(doe / 1460).wrapping_add(doe / 36524).wrapping_sub(doe / 146096)) / 365;
    let y = era * 400 + yoe as i64;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_final = if m <= 2 { y + 1 } else { y };
    (y_final, m, d)
}

