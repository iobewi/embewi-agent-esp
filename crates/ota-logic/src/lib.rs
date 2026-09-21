//! Pure OTA write-session decision logic (contrat §4's `Content-Range`
//! resume protocol) -- split out of `embewi-agent-esp`'s `src/ota.rs` and
//! `src/http/ota_write.rs` into its own crate specifically so it can be
//! unit-tested with a plain `cargo test`, no ESP32 hardware or `no_std`
//! toolchain involved.
//!
//! Mirrors `firmware-c`'s own split: that project keeps this exact same
//! logic in `embewi_parse.c`/`embewi_parse.h`, host-tested in
//! `test/host/test_parse.c`, for the same reason -- it's the one part of
//! the OTA write path that's pure enough to test without a device, and
//! subtle enough (off-by-one on a resumed transfer) to be worth it.
//!
//! `#![no_std]` except under `cargo test` (the test harness itself needs
//! `std`) -- the firmware crate depends on this directly, so it must stay
//! usable from a `no_std` build.
#![cfg_attr(not(test), no_std)]

/// `PUT /v1alpha1/ota/write`'s resume decision, ported verbatim from
/// `firmware-c`'s `embewi_ota_plan`.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Begin,
    Resync,
    Continue,
}

pub fn write_plan(has_range: bool, start: u32, in_progress: bool, written: u32) -> Plan {
    if !has_range || start == 0 {
        return Plan::Begin;
    }
    if !in_progress || written != start {
        return Plan::Resync;
    }
    Plan::Continue
}

/// Ported verbatim from `firmware-c`'s `embewi_ota_is_final`.
pub fn write_is_final(has_range: bool, end: u32, total: u32) -> bool {
    !has_range || end + 1 == total
}

/// Parses a `Content-Range: bytes <start>-<end>/<total>` header value
/// (contrat §4). `None` on anything malformed -- the caller maps that to
/// `400 {"error":"bad_content_range"}`.
pub fn parse_content_range(value: &str) -> Option<(u32, u32, u32)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?, total.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_no_range_always_begins() {
        assert_eq!(write_plan(false, 0, false, 0), Plan::Begin);
        assert_eq!(write_plan(false, 42, true, 42), Plan::Begin);
    }

    #[test]
    fn plan_start_zero_always_begins() {
        assert_eq!(write_plan(true, 0, true, 1234), Plan::Begin);
        assert_eq!(write_plan(true, 0, false, 0), Plan::Begin);
    }

    #[test]
    fn plan_resyncs_when_nothing_in_progress() {
        assert_eq!(write_plan(true, 100, false, 0), Plan::Resync);
    }

    #[test]
    fn plan_resyncs_on_offset_mismatch() {
        assert_eq!(write_plan(true, 100, true, 50), Plan::Resync);
        assert_eq!(write_plan(true, 100, true, 200), Plan::Resync);
    }

    #[test]
    fn plan_continues_when_aligned() {
        assert_eq!(write_plan(true, 100, true, 100), Plan::Continue);
    }

    #[test]
    fn is_final_without_range_is_always_final() {
        // Legacy monolithic write: one PUT is the whole image.
        assert!(write_is_final(false, 0, 0));
        assert!(write_is_final(false, 999, 1));
    }

    #[test]
    fn is_final_with_range_checks_last_byte() {
        assert!(write_is_final(true, 999, 1000));
        assert!(!write_is_final(true, 499, 1000));
    }

    #[test]
    fn is_final_off_by_one_boundaries() {
        // The exact boundary that a resumed transfer's last chunk must hit.
        assert!(write_is_final(true, 0, 1));
        assert!(!write_is_final(true, 0, 2));
    }

    #[test]
    fn parses_a_well_formed_range() {
        assert_eq!(parse_content_range("bytes 0-99/1000"), Some((0, 99, 1000)));
        assert_eq!(parse_content_range("bytes 65536-131071/983040"), Some((65536, 131071, 983040)));
    }

    #[test]
    fn tolerates_incidental_whitespace_around_numbers() {
        assert_eq!(parse_content_range("bytes 0-99 / 1000"), Some((0, 99, 1000)));
    }

    #[test]
    fn rejects_missing_bytes_prefix() {
        assert_eq!(parse_content_range("0-99/1000"), None);
    }

    #[test]
    fn rejects_missing_slash() {
        assert_eq!(parse_content_range("bytes 0-99"), None);
    }

    #[test]
    fn rejects_missing_dash() {
        assert_eq!(parse_content_range("bytes 099/1000"), None);
    }

    #[test]
    fn rejects_non_numeric_fields() {
        assert_eq!(parse_content_range("bytes a-99/1000"), None);
        assert_eq!(parse_content_range("bytes 0-b/1000"), None);
        assert_eq!(parse_content_range("bytes 0-99/c"), None);
    }

    #[test]
    fn rejects_empty_string() {
        assert_eq!(parse_content_range(""), None);
    }
}
