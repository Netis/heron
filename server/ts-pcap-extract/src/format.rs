//! Inlined copy of pcap classic format constants and the minute-label
//! encode/decode used by `ts-capture::pcap_dump`. Duplicated rather than
//! shared via a dependency so this read crate doesn't pull in libpcap /
//! ZMQ / snap-via-ts-capture / retention. The on-disk format is the
//! actual contract; anchor tests below pin it.
//!
//! See also: `server/ts-capture/src/pcap_dump.rs` (writer side).

// ---- pcap classic format constants (frozen by external spec) ----

pub const PCAP_MAGIC: u32 = 0xa1b2_c3d4;
pub const PCAP_VERSION_MAJOR: u16 = 2;
pub const PCAP_VERSION_MINOR: u16 = 4;
/// Must match `ts-capture::pcap_dump::PCAP_SNAPLEN`. Both sides re-anchor
/// against the literal value in their own tests, so drift surfaces locally.
pub const PCAP_SNAPLEN: u32 = 262_144;

pub const MICROS_PER_MINUTE: i64 = 60 * 1_000_000;

/// Default `link_type` used by the global header when no candidate files
/// exist (so an empty-result extract is still a valid, openable .pcap).
/// `1` = DLT_EN10MB / Ethernet — overwhelmingly the link type for
/// server-side LLM HTTP capture.
pub const DEFAULT_EMPTY_LINK_TYPE: u32 = 1;

// ---- minute label encode / decode ----

/// Compact UTC label for a wall-clock minute. `minute_key = ts_us / 60_000_000`.
/// Returns `"YYYYMMDDTHHMM"`.
pub fn minute_label(minute_key: i64) -> String {
    let total_secs = minute_key * 60;
    let days = total_secs.div_euclid(86_400);
    let rem = total_secs.rem_euclid(86_400);
    let (h, m) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32);
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}{mo:02}{d:02}T{h:02}{m:02}")
}

/// Inverse of [`minute_label`]. Returns `None` for any string that doesn't
/// match the exact `YYYYMMDDTHHMM` shape, or whose fields don't form a
/// valid UTC instant.
pub fn parse_minute_label(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 13 || b[8] != b'T' {
        return None;
    }
    if !b[..8].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !b[9..].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let mo: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    let h: u32 = s[9..11].parse().ok()?;
    let mi: u32 = s[11..13].parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 {
        return None;
    }
    let days = ymd_to_days(y, mo, d);
    if days_to_ymd(days) != (y, mo, d) {
        return None;
    }
    Some(days * 1440 + i64::from(h) * 60 + i64::from(mi))
}

fn ymd_to_days(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 {
        i64::from(y) - 1
    } else {
        i64::from(y)
    };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m_shift = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * m_shift + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard — these literal values must match
    /// `ts-capture::pcap_dump`'s own tests for the same constants.
    #[test]
    fn pcap_consts_anchored() {
        assert_eq!(PCAP_MAGIC, 0xa1b2_c3d4);
        assert_eq!(PCAP_VERSION_MAJOR, 2);
        assert_eq!(PCAP_VERSION_MINOR, 4);
        assert_eq!(PCAP_SNAPLEN, 262_144);
        assert_eq!(DEFAULT_EMPTY_LINK_TYPE, 1);
    }

    /// Drift guard — these fixtures must match
    /// `ts-capture::pcap_dump::tests::minute_label_format`.
    #[test]
    fn minute_label_anchored() {
        assert_eq!(minute_label(0), "19700101T0000");
        assert_eq!(minute_label(29_633_130), "20260505T1330");
    }

    #[test]
    fn parse_round_trips() {
        for &k in &[0i64, 1, 60, 29_633_130, 100_000_000] {
            let label = minute_label(k);
            assert_eq!(parse_minute_label(&label), Some(k), "label = {label}");
        }
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert_eq!(parse_minute_label(""), None);
        assert_eq!(parse_minute_label("20260505T2530"), None); // hour 25
        assert_eq!(parse_minute_label("20260532T1330"), None); // day 32
        assert_eq!(parse_minute_label("20261305T1330"), None); // month 13
        assert_eq!(parse_minute_label("20260505X1330"), None); // missing T
    }
}
