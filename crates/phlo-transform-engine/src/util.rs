//! Small internal helpers.

use std::cmp::Ordering;

use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Current UTC time as RFC 3339 with a fixed nine-digit fraction, or an
/// empty string if formatting fails. `time`'s `Rfc3339` formatter trims
/// trailing zeros (`.5Z`, `.52Z`, `Z`), which makes the string's byte and
/// chronological orders diverge within a second — and state rows and
/// audit evidence order by this string. Full precision keeps them
/// identical.
pub fn now_rfc3339() -> String {
    let now = OffsetDateTime::now_utc();
    let (date, time) = (now.date(), now.time());
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        time.hour(),
        time.minute(),
        time.second(),
        time.nanosecond()
    )
}

/// Order two RFC 3339 timestamps chronologically. Both sides are parsed
/// so variable-width fractions written before the format was fixed —
/// `.5Z` (500ms) sorting after `.52Z` (520ms) as bytes — still order
/// correctly. A value that fails to parse falls back to byte order so a
/// corrupt string still orders deterministically.
pub fn cmp_rfc3339(a: &str, b: &str) -> Ordering {
    match (
        OffsetDateTime::parse(a, &Rfc3339),
        OffsetDateTime::parse(b, &Rfc3339),
    ) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => a.cmp(b),
    }
}

/// Lower-case hex SHA-256 of a string.
pub fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}
