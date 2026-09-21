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

/// Ported from `firmware-c`'s `embewi_ota_is_final`, with the `end + 1`
/// checked: `end == u32::MAX` must not wrap to 0 and match a `total` of 0.
pub fn write_is_final(has_range: bool, end: u32, total: u32) -> bool {
    !has_range || end.checked_add(1) == Some(total)
}

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

/// What the agent's persisted staged-OTA record says (`ota::Stage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedKind {
    None,
    Written,
    Activating,
}

/// What the bootloader says about the image that just booted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootImage {
    PendingVerify,
    Valid,
    /// Anything else (factory image with a blank `otadata`, `Invalid`, ...).
    Other,
}

/// What `ota::on_boot` must do -- see [`boot_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootAction {
    /// Nothing staged, nothing to reconcile.
    Nothing,
    /// The staged image is the pending one: run the bounded self-check.
    SelfCheck,
    /// The bootloader has an unconfirmed image the staged record doesn't
    /// account for: never promote it blindly, roll it back.
    RollbackUnaccounted,
    /// `mark_valid` was interrupted after the bootloader validated the
    /// image but before its digest/`deployment_id` were recorded: finish it.
    FinishInterruptedValidation,
    /// The record no longer describes anything real (activation aborted, or
    /// the bootloader fell back to the previous slot): forget it.
    ClearStale,
    /// `written` and waiting for `/ota/activate`: must survive the reboot.
    KeepWritten,
}

/// The `on_boot` decision table. `booted_is_staged` is whether the slot
/// actually running equals the staged record's slot, `None` if the running
/// slot couldn't be determined -- then nothing destructive is decided
/// (a flaky partition-table read must never roll back a good image).
pub fn boot_action(staged: StagedKind, image: BootImage, booted_is_staged: Option<bool>) -> BootAction {
    use BootAction::*;
    match (image, booted_is_staged) {
        (BootImage::PendingVerify, None) => SelfCheck,
        (BootImage::PendingVerify, Some(true)) if staged == StagedKind::Activating => SelfCheck,
        (BootImage::PendingVerify, _) => RollbackUnaccounted,
        (_, None) => Nothing,
        (_, Some(same)) => match staged {
            StagedKind::None => Nothing,
            StagedKind::Written if same => ClearStale,
            StagedKind::Written => KeepWritten,
            StagedKind::Activating if same && image == BootImage::Valid => FinishInterruptedValidation,
            StagedKind::Activating => ClearStale,
        },
    }
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
        assert!(!write_is_final(true, u32::MAX, 0));
        assert!(write_is_final(true, u32::MAX - 1, u32::MAX));
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

    use BootAction::*;
    use BootImage as I;
    use StagedKind as S;

    #[test]
    fn boot_written_survives_a_reboot() {
        assert_eq!(boot_action(S::Written, I::Other, Some(false)), KeepWritten);
        assert_eq!(boot_action(S::Written, I::Valid, Some(false)), KeepWritten);
    }

    #[test]
    fn boot_written_on_the_running_slot_is_stale() {
        assert_eq!(boot_action(S::Written, I::Valid, Some(true)), ClearStale);
    }

    #[test]
    fn boot_activating_pending_same_slot_self_checks() {
        assert_eq!(boot_action(S::Activating, I::PendingVerify, Some(true)), SelfCheck);
    }

    #[test]
    fn boot_activating_valid_same_slot_finishes_validation() {
        assert_eq!(boot_action(S::Activating, I::Valid, Some(true)), FinishInterruptedValidation);
    }

    #[test]
    fn boot_activating_on_another_slot_is_an_aborted_activation() {
        assert_eq!(boot_action(S::Activating, I::Valid, Some(false)), ClearStale);
        assert_eq!(boot_action(S::Activating, I::Other, Some(false)), ClearStale);
        assert_eq!(boot_action(S::Activating, I::Other, Some(true)), ClearStale);
    }

    #[test]
    fn boot_pending_image_the_record_does_not_explain_is_rolled_back() {
        assert_eq!(boot_action(S::None, I::PendingVerify, Some(true)), RollbackUnaccounted);
        assert_eq!(boot_action(S::None, I::PendingVerify, Some(false)), RollbackUnaccounted);
        assert_eq!(boot_action(S::Written, I::PendingVerify, Some(true)), RollbackUnaccounted);
        assert_eq!(boot_action(S::Activating, I::PendingVerify, Some(false)), RollbackUnaccounted);
    }

    #[test]
    fn boot_unknown_slot_is_never_destructive() {
        assert_eq!(boot_action(S::Activating, I::PendingVerify, None), SelfCheck);
        assert_eq!(boot_action(S::None, I::PendingVerify, None), SelfCheck);
        assert_eq!(boot_action(S::Written, I::Valid, None), Nothing);
        assert_eq!(boot_action(S::Activating, I::Valid, None), Nothing);
    }

    #[test]
    fn boot_nothing_staged_does_nothing() {
        assert_eq!(boot_action(S::None, I::Valid, Some(true)), Nothing);
        assert_eq!(boot_action(S::None, I::Other, Some(false)), Nothing);
    }
}
