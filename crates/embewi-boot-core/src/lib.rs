//! Pure decision logic of the Embewi A/B boot chain: the `otadata` entry format
//! and the transitions the bootloader (`embewi-boot`) and the agent apply to
//! it. No hardware access, no allocation -- it only says *what to write and
//! what to boot*, so every rule can be tested on the host, including a power
//! cut at every flash command, under an adversarial model of what a cut leaves
//! behind (see `tests`).
//!
//! # Entry format
//!
//! Two 4 KiB sectors, one 32-byte entry at the start of each, the same size and
//! position as ESP-IDF's `esp_ota_select_entry_t`, so the partition, the tooling
//! and the slot arithmetic stay. What differs is how an entry becomes *valid*.
//! ESP-IDF's CRC covers only `ota_seq`: a half-programmed state word can read as
//! another state (`New` = 0 as `Valid` = 2) behind a valid CRC, unless the flash
//! programs bytes strictly in address order -- a guarantee no flash datasheet
//! gives. Embewi entries therefore carry a **commit word**, programmed by a
//! separate flash command after the rest of the entry:
//!
//! ```text
//!  0  ota_seq         u32   } as ESP-IDF
//!  4  magic           "EWBT"   //!  8  format version  u32 = 1   | in ESP-IDF's `seq_label`, which it ignores
//! 12  ext_crc         u32       | crc32(seq, state, magic, version)
//! 16  reserved        u32 = erased
//! 20  commit word     u32 = COMMIT   <- programmed last, on its own
//! 24  ota_state       u32   } as ESP-IDF
//! 28  idf_crc         u32   } crc32(ota_seq), as ESP-IDF
//! ```
//!
//! An entry is accepted only if **every** field is exact. Anything else that is
//! not fully erased is *corrupt* and ignored -- including entries written the
//! ESP-IDF way, by design: no legacy mode. A cut before the commit word is
//! complete leaves an entry that is not accepted; a cut during it leaves either
//! that or the complete entry, whose body was already fully written. Nothing
//! depends on the order the flash programs cells *within* one command.
//!
//! # Rules
//!
//! * Only `Valid` is trusted. `New` is a candidate that has never run: booted
//!   at most once, after being marked `PendingVerify`.
//! * [`activate`] writes **one** entry (sequence and state together) into the
//!   sector that does not hold the last `Valid` entry, so the fallback image
//!   stays selectable through any interruption. `esp-bootloader-esp-idf`
//!   activates in two writes and picks the sector by raw sequence comparison
//!   (after a rollback it would erase the only good entry); the agent must use
//!   this crate instead.
//! * A blank `otadata` is a normal first boot, not an error: see [`plan_boot`].
#![cfg_attr(not(test), no_std)]

/// Size of one `otadata` entry.
pub const ENTRY_SIZE: usize = 32;
/// The entry lives at the start of each of the two `otadata` sectors.
pub const SECTOR_COUNT: usize = 2;

/// One entry as stored.
pub type Raw = [u8; ENTRY_SIZE];
/// A fully erased entry.
pub const BLANK: Raw = [0xFF; ENTRY_SIZE];

/// `ota_state` values (`esp_ota_img_states_t`). Only these five are accepted.
pub mod state {
    pub const NEW: u32 = 0;
    pub const PENDING_VERIFY: u32 = 1;
    pub const VALID: u32 = 2;
    pub const INVALID: u32 = 3;
    pub const ABORTED: u32 = 4;
}

pub const MAGIC: [u8; 4] = *b"EWBT";
pub const FORMAT_VERSION: u32 = 1;
/// Balanced bits: a partly-programmed word is never this value.
pub const COMMIT: u32 = 0x5AC3_A53C;

const OFF_SEQ: usize = 0;
const OFF_MAGIC: usize = 4;
const OFF_VERSION: usize = 8;
const OFF_EXT_CRC: usize = 12;
const OFF_RESERVED: usize = 16;
/// Offset of the commit word, and the only bytes the second program command writes.
pub const OFF_COMMIT: usize = 20;
const OFF_STATE: usize = 24;
const OFF_IDF_CRC: usize = 28;

/// zlib-compatible CRC-32 continued from `init` -- what the ROM's
/// `esp_rom_crc32_le(init, ..)` computes.
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

fn word(raw: &Raw, at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

/// A logical entry: which slot it selects and what is known about that image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub seq: u32,
    pub state: u32,
}

impl Entry {
    pub fn new(seq: u32, state: u32) -> Entry {
        Entry { seq, state }
    }

    fn ext_crc(&self) -> u32 {
        let mut input = [0u8; 16];
        input[0..4].copy_from_slice(&self.seq.to_le_bytes());
        input[4..8].copy_from_slice(&self.state.to_le_bytes());
        input[8..12].copy_from_slice(&MAGIC);
        input[12..16].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        crc32_le(u32::MAX, &input)
    }

    /// What the first program command writes: everything except the commit word,
    /// whose four bytes stay `0xFF` (programming `0xFF` changes nothing).
    pub fn body(&self) -> Raw {
        let mut raw = BLANK;
        raw[OFF_SEQ..OFF_SEQ + 4].copy_from_slice(&self.seq.to_le_bytes());
        raw[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC);
        raw[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        raw[OFF_EXT_CRC..OFF_EXT_CRC + 4].copy_from_slice(&self.ext_crc().to_le_bytes());
        raw[OFF_STATE..OFF_STATE + 4].copy_from_slice(&self.state.to_le_bytes());
        raw[OFF_IDF_CRC..OFF_IDF_CRC + 4].copy_from_slice(&crc32_le(u32::MAX, &self.seq.to_le_bytes()).to_le_bytes());
        raw
    }

    /// The entry as it reads once committed.
    pub fn encode(&self) -> Raw {
        let mut raw = self.body();
        raw[OFF_COMMIT..OFF_COMMIT + 4].copy_from_slice(&COMMIT.to_le_bytes());
        raw
    }
}

/// What a sector holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoded {
    /// Fully erased.
    Blank,
    /// Anything that is not a complete, exact Embewi entry: torn, garbage, or ESP-IDF-format.
    Corrupt,
    Ok(Entry),
}

/// Accepts an entry only if every field is exact.
pub fn decode(raw: &Raw) -> Decoded {
    if *raw == BLANK {
        return Decoded::Blank;
    }
    let entry = Entry { seq: word(raw, OFF_SEQ), state: word(raw, OFF_STATE) };
    let known = matches!(
        entry.state,
        state::NEW | state::PENDING_VERIFY | state::VALID | state::INVALID | state::ABORTED
    );
    let exact = entry.seq != 0
        && entry.seq != u32::MAX
        && known
        && raw[OFF_MAGIC..OFF_MAGIC + 4] == MAGIC
        && word(raw, OFF_VERSION) == FORMAT_VERSION
        && word(raw, OFF_EXT_CRC) == entry.ext_crc()
        && word(raw, OFF_RESERVED) == u32::MAX
        && word(raw, OFF_COMMIT) == COMMIT
        && word(raw, OFF_IDF_CRC) == crc32_le(u32::MAX, &entry.seq.to_le_bytes());
    if exact { Decoded::Ok(entry) } else { Decoded::Corrupt }
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
    /// `New`: a candidate that has never run.
    Unproven,
}

impl Trust {
    fn of(state: u32) -> Trust {
        match state {
            state::VALID => Trust::Valid,
            state::PENDING_VERIFY => Trust::Pending,
            state::NEW => Trust::Unproven,
            _ => Trust::Dead, // INVALID, ABORTED (`decode` admits nothing else)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Blank,
    Corrupt,
    Ok { seq: u32, trust: Trust },
}

fn classify(raw: &Raw) -> Class {
    match decode(raw) {
        Decoded::Blank => Class::Blank,
        Decoded::Corrupt => Class::Corrupt,
        Decoded::Ok(e) => Class::Ok { seq: e.seq, trust: Trust::of(e.state) },
    }
}

/// Which OTA slot (0-based: `ota_0`, `ota_1`, ...) a sequence number selects.
pub fn slot_of(seq: u32, slot_count: u8) -> u8 {
    ((seq - 1) % u32::from(slot_count)) as u8
}

/// One entry update: the sector is erased, the body programmed, then -- in a
/// separate command -- the commit word. Three flash commands, any of which a
/// power cut can interrupt; only the last one makes the entry acceptable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Write {
    pub sector: u8,
    pub entry: Entry,
}

/// A single flash command of a [`Write`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Erase { sector: u8 },
    /// Program `data[..len]` at `offset` within the sector's entry.
    Program { sector: u8, offset: u8, len: u8, data: [u8; ENTRY_SIZE] },
}

impl Write {
    /// The commands, in order. An executor should read the body back and compare
    /// before issuing the last one (a body that didn't take must not be committed).
    pub fn ops(&self) -> [Op; 3] {
        let mut commit = [0u8; ENTRY_SIZE];
        commit[..4].copy_from_slice(&COMMIT.to_le_bytes());
        [
            Op::Erase { sector: self.sector },
            Op::Program { sector: self.sector, offset: 0, len: ENTRY_SIZE as u8, data: self.entry.body() },
            Op::Program { sector: self.sector, offset: OFF_COMMIT as u8, len: 4, data: commit },
        ]
    }
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
pub fn plan_boot(otadata: [Raw; SECTOR_COUNT], slot_count: u8, image_ok: &mut dyn FnMut(u8) -> bool) -> Plan {
    let class = [classify(&otadata[0]), classify(&otadata[1])];
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
    // (Corrupt includes anything not written the Embewi way: a device flashed with an ESP-IDF
    // `otadata` is re-seeded, which is the no-legacy policy, not an accident.)
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
pub fn activate(otadata: [Raw; SECTOR_COUNT], slot_count: u8, target: u8) -> Result<Write, ActivateError> {
    let class = [classify(&otadata[0]), classify(&otadata[1])];
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
pub fn confirm(otadata: [Raw; SECTOR_COUNT]) -> Option<Write> {
    pending(otadata).map(|(sector, seq)| Write { sector, entry: Entry::new(seq, state::VALID) })
}

/// The agent rejecting the image it runs (self-check failed): the `Pending`
/// entry becomes `Invalid`, so the next boot falls back at once.
pub fn reject(otadata: [Raw; SECTOR_COUNT]) -> Option<Write> {
    pending(otadata).map(|(sector, seq)| Write { sector, entry: Entry::new(seq, state::INVALID) })
}

fn pending(otadata: [Raw; SECTOR_COUNT]) -> Option<(u8, u32)> {
    let mut best: Option<(u8, u32)> = None;
    for (sector, entry) in otadata.iter().enumerate() {
        if let Class::Ok { seq, trust: Trust::Pending } = classify(entry) {
            if best.is_none_or(|(_, b)| seq > b) {
                best = Some((sector as u8, seq));
            }
        }
    }
    best
}

pub mod image;

#[cfg(test)]
mod image_tests;
#[cfg(test)]
mod tests;
