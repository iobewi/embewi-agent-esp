//! Pure decision logic of the Embewi A/B boot chain: the `otadata` entry codec
//! and the transitions the bootloader (`embewi-boot`) and the agent apply to
//! it. No hardware access, no allocation -- it only says *what to write and
//! what to boot*, so every rule can be tested on the host, including a power
//! cut at every flash write and erase (see `tests/`).
//!
//! The on-flash layout is ESP-IDF's `otadata` (two 4 KiB sectors, one 32-byte
//! `esp_ota_select_entry_t` at the start of each), kept on purpose: the agent
//! already uses it through `esp-bootloader-esp-idf` and the tooling knows it.
//!
//! ```text
//!   ota_seq u32 | seq_label [u8; 20] | ota_state u32 | crc u32 (of ota_seq)
//! ```
//!
//! Rules that differ from a naive reading of that format, each one closing a
//! real power-cut hole (all covered by tests):
//!
//! * Only `Valid` is trusted. `New`, `Undefined` (erased state) and any unknown
//!   state are *unproven*: booted at most once, after being marked
//!   `PendingVerify`.
//! * The agent must activate with [`activate`], **one** write of a complete
//!   entry. `esp-bootloader-esp-idf` does it in two (sequence, then state):
//!   between them the entry carries the state left in its sector, which is
//!   harmless when that is `Undefined` (fresh sector) but not when it is a
//!   stale `Valid` -- a cut there boots an image nobody has verified. See
//!   `tests::naive_two_step_activation_can_boot_an_unverified_image`.
//! * [`activate`] never overwrites the sector holding the last `Valid` entry
//!   (raw sequence comparison would, after a rollback left an `Aborted` entry
//!   with the highest sequence), and writes sequence and state in one entry.
//! * A blank `otadata` is a normal first boot, not an error: see [`plan_boot`].
#![cfg_attr(not(test), no_std)]

/// Size of one `otadata` entry.
pub const ENTRY_SIZE: usize = 32;
/// The entry lives at the start of each of the two `otadata` sectors.
pub const SECTOR_COUNT: usize = 2;

/// `ota_state` values (`esp_ota_img_states_t`).
pub mod state {
    pub const NEW: u32 = 0;
    pub const PENDING_VERIFY: u32 = 1;
    pub const VALID: u32 = 2;
    pub const INVALID: u32 = 3;
    pub const ABORTED: u32 = 4;
    /// The erased value; ESP-IDF's `ESP_OTA_IMG_UNDEFINED`.
    pub const UNDEFINED: u32 = 0xFFFF_FFFF;
}

/// zlib-compatible CRC-32 continued from `init` -- what the ROM's
/// `esp_rom_crc32_le(init, ..)` computes, and what `otadata` uses
/// (`init = u32::MAX`, over the 4 bytes of `ota_seq`).
pub fn crc32_le(init: u32, data: &[u8]) -> u32 {
    let mut crc = !init;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// One 32-byte `otadata` entry, fields as stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub seq: u32,
    pub label: [u8; 20],
    pub state: u32,
    pub crc: u32,
}

impl Entry {
    /// An erased entry.
    pub const BLANK: Entry = Entry { seq: u32::MAX, label: [0xFF; 20], state: u32::MAX, crc: u32::MAX };

    /// A complete, self-consistent entry -- what gets programmed in one write.
    pub fn new(seq: u32, state: u32) -> Entry {
        Entry { seq, label: [0xFF; 20], state, crc: crc32_le(u32::MAX, &seq.to_le_bytes()) }
    }

    pub fn decode(raw: &[u8; ENTRY_SIZE]) -> Entry {
        let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let mut label = [0u8; 20];
        label.copy_from_slice(&raw[4..24]);
        Entry { seq: word(0), label, state: word(24), crc: word(28) }
    }

    pub fn encode(&self) -> [u8; ENTRY_SIZE] {
        let mut raw = [0u8; ENTRY_SIZE];
        raw[0..4].copy_from_slice(&self.seq.to_le_bytes());
        raw[4..24].copy_from_slice(&self.label);
        raw[24..28].copy_from_slice(&self.state.to_le_bytes());
        raw[28..32].copy_from_slice(&self.crc.to_le_bytes());
        raw
    }

    fn classify(&self) -> Class {
        if *self == Entry::BLANK {
            Class::Blank
        } else if self.seq == 0 || self.seq == u32::MAX || self.crc != crc32_le(u32::MAX, &self.seq.to_le_bytes()) {
            // Torn or garbage: never a candidate. (`seq == 0` would underflow
            // the `seq - 1` slot mapping.)
            Class::Corrupt
        } else {
            Class::Ok { seq: self.seq, trust: Trust::of(self.state) }
        }
    }
}

/// How much an entry's state can be believed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trust {
    /// Confirmed by a running image (or the first-boot seed).
    Valid,
    /// Booted once and not yet confirmed.
    Pending,
    /// Rejected: a failed self-check, or a `Pending` entry that never confirmed.
    Dead,
    /// `New`, erased, or unknown: a candidate that has never run.
    Unproven,
}

impl Trust {
    fn of(state: u32) -> Trust {
        match state {
            state::VALID => Trust::Valid,
            state::PENDING_VERIFY => Trust::Pending,
            state::INVALID | state::ABORTED => Trust::Dead,
            _ => Trust::Unproven,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Blank,
    Corrupt,
    Ok { seq: u32, trust: Trust },
}

/// Which OTA slot (0-based: `ota_0`, `ota_1`, ...) a sequence number selects.
pub fn slot_of(seq: u32, slot_count: u8) -> u8 {
    ((seq - 1) % u32::from(slot_count)) as u8
}

/// One flash transaction: erase `sector`, then program `entry` (32 bytes) at
/// its start. Exactly what ESP-IDF's `write_otadata` does; a power cut can
/// interrupt it at any point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Write {
    pub sector: u8,
    pub entry: Entry,
}

/// Why the bootloader stopped instead of booting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Halt {
    /// First boot and the slot-0 image is not bootable either.
    NoImage,
    /// Entries exist but every one is rejected or points at an unbootable image.
    NoUsableEntry,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Boot {
    /// Boot this slot, after applying the plan's writes.
    Slot { slot: u8, sector: u8, seq: u32 },
    Halt(Halt),
}

/// What the bootloader must do this boot: apply `writes` in order, then `boot`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Plan {
    pub boot: Boot,
    writes: [Option<Write>; 4],
}

impl Plan {
    fn new(boot: Boot, writes: [Option<Write>; 4]) -> Plan {
        Plan { boot, writes }
    }

    pub fn writes(&self) -> impl Iterator<Item = Write> + '_ {
        self.writes.iter().flatten().copied()
    }
}

/// The bootloader's decision, from the two `otadata` entries.
///
/// `image_ok(slot)` says whether that slot holds a bootable image (header,
/// segments, and whatever integrity check the bootloader applies); it is only
/// called for slots this function is about to boot, so the expensive checks
/// run lazily.
///
/// Order of business:
/// 1. every `Pending` entry becomes `Aborted` -- an image that was booted and
///    never confirmed is rejected (this is the rollback);
/// 2. candidates are the `Valid` and `Unproven` entries, highest sequence first;
///    one whose image is not bootable is marked `Invalid` and skipped;
/// 3. an `Unproven` candidate is marked `Pending` *before* it is booted; a
///    `Valid` one boots as is;
/// 4. with no candidate at all: if there is no entry that ever existed (blank
///    or torn `otadata`, i.e. a first boot) and slot 0 is bootable, seed it as
///    `Valid`; otherwise halt explicitly rather than guess.
pub fn plan_boot(otadata: [Entry; SECTOR_COUNT], slot_count: u8, image_ok: &mut dyn FnMut(u8) -> bool) -> Plan {
    let class = [otadata[0].classify(), otadata[1].classify()];
    let mut writes = [None; 4];
    let mut count = 0;
    let mut push = |w: Write, writes: &mut [Option<Write>; 4]| {
        writes[count] = Some(w);
        count += 1;
    };

    // 1. Pending -> Aborted, and collect what is still a candidate.
    let mut candidates: [Option<(usize, u32, Trust)>; 2] = [None; 2];
    let mut rejected_any = false;
    for (sector, c) in class.iter().enumerate() {
        if let Class::Ok { seq, trust } = *c {
            match trust {
                Trust::Pending => {
                    push(Write { sector: sector as u8, entry: Entry::new(seq, state::ABORTED) }, &mut writes);
                    rejected_any = true;
                }
                Trust::Dead => rejected_any = true,
                Trust::Valid | Trust::Unproven => candidates[sector] = Some((sector, seq, trust)),
            }
        }
    }

    // 2./3. Highest sequence first.
    let mut order = candidates;
    order.sort_unstable_by_key(|c| core::cmp::Reverse(c.map(|(_, seq, _)| seq)));
    for candidate in order.into_iter().flatten() {
        let (sector, seq, trust) = candidate;
        let slot = slot_of(seq, slot_count);
        if !image_ok(slot) {
            push(Write { sector: sector as u8, entry: Entry::new(seq, state::INVALID) }, &mut writes);
            rejected_any = true;
            continue;
        }
        if trust == Trust::Unproven {
            push(Write { sector: sector as u8, entry: Entry::new(seq, state::PENDING_VERIFY) }, &mut writes);
        }
        return Plan::new(Boot::Slot { slot, sector: sector as u8, seq }, writes);
    }

    // 4. Nothing to boot. A first boot only if no entry was ever there.
    if !rejected_any && class.iter().all(|c| matches!(c, Class::Blank | Class::Corrupt)) {
        if image_ok(0) {
            push(Write { sector: 0, entry: Entry::new(1, state::VALID) }, &mut writes);
            return Plan::new(Boot::Slot { slot: 0, sector: 0, seq: 1 }, writes);
        }
        return Plan::new(Boot::Halt(Halt::NoImage), writes);
    }
    Plan::new(Boot::Halt(Halt::NoUsableEntry), writes)
}

/// Why [`activate`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActivateError {
    /// No `Valid` entry: there is no known-good image to fall back to, so
    /// nothing may be activated (it could not be rolled back).
    NoValidBase,
    /// The sequence number space is exhausted.
    SequenceExhausted,
}

/// The agent's `activate`: point the boot at `target` slot, as `New`.
///
/// One write, of a complete entry (sequence and state together). It goes into
/// the sector that does **not** hold the last `Valid` entry, so the fallback
/// image stays selectable through any interruption.
pub fn activate(otadata: [Entry; SECTOR_COUNT], slot_count: u8, target: u8) -> Result<Write, ActivateError> {
    let class = [otadata[0].classify(), otadata[1].classify()];
    let mut base_sector = None;
    let mut max_seq = 0;
    for (sector, c) in class.iter().enumerate() {
        if let Class::Ok { seq, trust } = *c {
            max_seq = max_seq.max(seq);
            if trust == Trust::Valid && base_sector.is_none_or(|(_, best)| seq > best) {
                base_sector = Some((sector, seq));
            }
        }
    }
    let (protected, _) = base_sector.ok_or(ActivateError::NoValidBase)?;

    // Smallest sequence above everything present that selects `target`.
    let mut seq = max_seq.checked_add(1).ok_or(ActivateError::SequenceExhausted)?;
    while slot_of(seq, slot_count) != target {
        seq = seq.checked_add(1).ok_or(ActivateError::SequenceExhausted)?;
    }
    Ok(Write { sector: 1 - protected as u8, entry: Entry::new(seq, state::NEW) })
}

/// The agent confirming the image it runs (self-check passed): the `Pending`
/// entry becomes `Valid`. `None` if nothing is pending -- in particular if the
/// bootloader did not mark the entry `Pending`, which is a boot-chain anomaly
/// the caller should report, not paper over.
pub fn confirm(otadata: [Entry; SECTOR_COUNT]) -> Option<Write> {
    pending(otadata).map(|(sector, seq)| Write { sector, entry: Entry::new(seq, state::VALID) })
}

/// The agent rejecting the image it runs (self-check failed): the `Pending`
/// entry becomes `Invalid`, so the next boot falls back at once.
pub fn reject(otadata: [Entry; SECTOR_COUNT]) -> Option<Write> {
    pending(otadata).map(|(sector, seq)| Write { sector, entry: Entry::new(seq, state::INVALID) })
}

fn pending(otadata: [Entry; SECTOR_COUNT]) -> Option<(u8, u32)> {
    let mut best: Option<(u8, u32)> = None;
    for (sector, entry) in otadata.iter().enumerate() {
        if let Class::Ok { seq, trust: Trust::Pending } = entry.classify() {
            if best.is_none_or(|(_, b)| seq > b) {
                best = Some((sector as u8, seq));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests;
