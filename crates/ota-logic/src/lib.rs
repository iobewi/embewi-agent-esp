//! Pure `Content-Range` transport logic for `PUT /v1alpha1/ota/write`
//! (contrat §4) -- split out of `embewi-agent-esp`'s `src/ota.rs` into its
//! own crate specifically so it can be unit-tested with a plain `cargo
//! test`, no ESP32 hardware or `no_std` toolchain involved.
//!
//! The resume *decision* this header used to drive (`Plan`/`write_plan`/
//! `write_is_final`) and the post-reboot staged-transaction reconciliation
//! (`boot_action`) have since moved to
//! [`atomic-ota`](https://github.com/iobewi/atomic-ota) -- generic,
//! transport-agnostic, `no_std`, host-tested there instead. What's left
//! here is genuinely `Content-Range`-shaped and has no business in a
//! transport-agnostic engine: parsing the header's `bytes
//! <start>-<end>/<total>` syntax, and the wire-format shape of the
//! `sha256:<hex>` digest header. `embewi-agent-esp`'s `ota.rs` adapts these
//! into the decoded numbers `atomic_ota::resume_plan`/`is_complete` take.
//!
//! `#![no_std]` except under `cargo test` (the test harness itself needs
//! `std`) -- the firmware crate depends on this directly, so it must stay
//! usable from a `no_std` build.
#![cfg_attr(not(test), no_std)]

/// Number of bytes a `Content-Range: bytes start-end/total` chunk carries
/// (`end - start + 1`), `None` if the range is inverted or the length
/// doesn't fit. The caller compares it to `Content-Length`.
pub fn range_len(start: u32, end: u32) -> Option<u32> {
    end.checked_sub(start)?.checked_add(1)
}

/// `sha256:` followed by exactly 64 hex digits (either case) -- the only
/// digest shape `PUT /ota/write` accepts (contrat §4).
pub fn is_valid_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Parses a `Content-Range: bytes <start>-<end>/<total>` header value
/// (contrat §4). `None` on anything malformed *or* semantically impossible
/// -- the caller maps that to `400 {"error":"bad_content_range"}`. Beyond
/// syntax, the range must satisfy `start <= end < total` (which also rules
/// out `total == 0`), so nothing downstream ever sees an inverted range or
/// one reaching past the declared total.
pub fn parse_content_range(value: &str) -> Option<(u32, u32, u32)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u32 = start.trim().parse().ok()?;
    let end: u32 = end.trim().parse().ok()?;
    let total: u32 = total.trim().parse().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn rejects_inverted_or_out_of_range() {
        assert_eq!(parse_content_range("bytes 10-5/100"), None);
        assert_eq!(parse_content_range("bytes 0-100/100"), None);
        assert_eq!(parse_content_range("bytes 0-0/0"), None);
        assert_eq!(parse_content_range("bytes 0-99/0"), None);
    }

    #[test]
    fn accepts_the_exact_last_byte() {
        assert_eq!(parse_content_range("bytes 99-99/100"), Some((99, 99, 100)));
        assert_eq!(parse_content_range("bytes 0-0/1"), Some((0, 0, 1)));
    }

    #[test]
    fn u32_max_end_cannot_wrap() {
        // `end + 1` used to wrap to 0 and match `total == 0`.
        assert_eq!(parse_content_range("bytes 0-4294967295/0"), None);
    }

    #[test]
    fn range_len_is_inclusive_and_checked() {
        assert_eq!(range_len(0, 99), Some(100));
        assert_eq!(range_len(5, 5), Some(1));
        assert_eq!(range_len(6, 5), None);
        assert_eq!(range_len(0, u32::MAX), None);
    }

    #[test]
    fn digest_shape() {
        let hex = "0123456789abcdefABCDEF0123456789abcdefABCDEF0123456789abcdefABCD";
        assert_eq!(hex.len(), 64);
        assert!(is_valid_digest(&format!("sha256:{hex}")));
        assert!(!is_valid_digest(""));
        assert!(!is_valid_digest("sha256:"));
        assert!(!is_valid_digest(&format!("sha256:{}", &hex[..63])));
        assert!(!is_valid_digest(&format!("sha256:{hex}0")));
        assert!(!is_valid_digest(&format!("sha1:{hex}")));
        assert!(!is_valid_digest(&format!("sha256:{}g", &hex[..63])));
    }
}
