//! Embewi's OTA adapter (contrat §3/§4/§6): the Core streams a raw `.bin`
//! into whichever `ota_0`/`ota_1` slot isn't currently booted.
//!
//! This module is **not** the OTA engine -- it is the ESP/Embewi-specific
//! glue around one. The transaction state machine (staged/activating,
//! post-reboot reconciliation) and the streaming, digest-verified,
//! resumable write session both live in
//! [`atomic_ota`](https://github.com/iobewi/atomic-ota), a generic,
//! `no_std` crate with no ESP32/`esp-hal`/Embassy/HTTP dependency of its
//! own -- see that crate's own doc comment for what it owns and why. What
//! stays here is everything that engine needs a concrete backend for, plus
//! whatever is genuinely specific to this device and this contract:
//!
//! ```text
//! Embewi OTA adapter (this module)
//! ├── HTTP / contrat v1alpha1        (ota_write.rs; prepare/activate below)
//! ├── ESP flash backend              (EspArtifactStorage, same
//! │                                    sector-buffered NorFlash writes
//! │                                    this module always used)
//! ├── NVS transaction metadata       (NvsTransactionMetadata, same
//! │                                    five-key staged/digest/slot/
//! │                                    deployment_id/size layout this
//! │                                    module always used)
//! ├── EWBT / bootloader adapter      (otadata_confirm/reject/activate,
//! │                                    via embewi_boot_core -- see below)
//! └── watchdog / self-check          (arm_boot_watchdog, selfcheck_task)
//!
//! atomic_ota (external crate)
//! ├── transaction state machine      (TransactionRecord/TransactionState)
//! ├── post-reboot reconciliation     (reconcile, driven from on_boot)
//! └── streaming WriteSession         (digest-verified, resumable)
//! ```
//!
//! `otadata` itself -- which slot is active, what to write to activate,
//! confirm or reject one -- is `embewi_boot_core` (`crates/embewi-boot-core`),
//! the same crate `embewi-boot` (`boot/`) uses to decide what to boot. This
//! module never re-implements that decision or that format: every write goes
//! through [`execute_otadata_write`], the same erase/body/commit protocol the
//! bootloader executes, each step read back before the next. `otadata`
//! entries written the ESP-IDF way (as `esp-bootloader-esp-idf`, still used
//! here only for partition-table parsing and the OTA image writes
//! themselves, would write) are deliberately not understood by this format --
//! no legacy mode, matching `embewi-boot`. `embewi_boot_core` is its own
//! thing, unrelated to `atomic_ota`: two separate state machines (bootloader
//! slot-trust vs. this adapter's OTA transaction), stitched together by
//! `atomic_ota::reconcile`'s `BackendOutcome` input.
//!
//! Mirrors `firmware-c`'s `embewi_ota.c`/`embewi_selfcheck.c` state machine
//! (same `stage`/`slot`/`digest`/`deployment_id`/`size` staged-NVS layout),
//! reimplemented against embassy tasks instead of ESP-IDF's C one and
//! FreeRTOS tasks -- the resume-decision and reconciliation *logic* itself
//! now lives in `atomic_ota`, not reimplemented here.
//!
//! Staged state is persisted to NVS (not just kept in RAM) because, unlike
//! a Core restart, the reconcile in contrat §6 also has to survive *this*
//! device rebooting between `/ota/write` and `/ota/activate` -- the write
//! session itself (running SHA-256, byte offset) doesn't need to, since a
//! reboot there always restarts the Core from `start=0` (contrat's own
//! `[RÉSERVE]` on inter-reboot write resume).

use alloc::boxed::Box;
use alloc::string::String;
use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use embewi_boot_core as boot_core;
use boot_core::Decoded;
use esp_bootloader_esp_idf::partitions::{AppPartitionSubType, DataPartitionSubType, PARTITION_TABLE_MAX_LEN, PartitionType};
use esp_nvs::Key;
use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::agent;
use atomic_ota::{Action, BackendOutcome, TransactionState};

use crate::storage::{SharedStorage, Storage, StorageError};

/// Contrat §4: `POST /ota/prepare`'s `partition_layout` field must match
/// this exactly, or the write is refused before a single byte transfers.
/// Bump only if `partitions.csv`'s slot layout ever changes shape.
pub const PARTITION_LAYOUT: &str = "embewi-ab-v1";
/// Contrat §3: how long a `pending_verify` self-check gets before this
/// device forces its own reset -- unconfirmed past this, the bootloader's
/// own rollback takes over on the next boot. Same value `firmware-c` uses
/// (`EMBEWI_PENDING_DEADLINE_MS`).
const SELFCHECK_DEADLINE: Duration = Duration::from_secs(15);
/// How long the anti-freeze watchdog (see [`arm_boot_watchdog`]) gets before
/// it force-resets the device. Longer than `SELFCHECK_DEADLINE` so the
/// graceful, logged software timeout in `selfcheck_task` fires first in the
/// ordinary case; this is the hardware backstop for when even that doesn't
/// run -- a hang before the self-check's own `select` is ever reached, or
/// one inside the embassy executor itself, neither of which a purely
/// software deadline (which depends on that same executor) can catch.
const WATCHDOG_DEADLINE: esp_hal::time::Duration = esp_hal::time::Duration::from_secs(20);

const NAMESPACE: Key = Key::from_str("ota");
const KEY_STAGE: Key = Key::from_str("stage");
const KEY_SLOT: Key = Key::from_str("slot");
const KEY_DIGEST: Key = Key::from_str("digest");
const KEY_DEPLOYMENT_ID: Key = Key::from_str("dep_id");
const KEY_SIZE: Key = Key::from_str("size");
const KEY_ACTIVE_DIGEST: Key = Key::from_str("act_digest");
const KEY_ACTIVE_DEPLOYMENT_ID: Key = Key::from_str("act_dep_id");

fn slot_name(slot: AppPartitionSubType) -> &'static str {
    match slot {
        AppPartitionSubType::Factory => "factory",
        AppPartitionSubType::Ota0 => "ota_0",
        AppPartitionSubType::Ota1 => "ota_1",
        _ => "ota_?",
    }
}

fn slot_from_name(name: &str) -> Option<AppPartitionSubType> {
    match name {
        "ota_0" => Some(AppPartitionSubType::Ota0),
        "ota_1" => Some(AppPartitionSubType::Ota1),
        _ => None,
    }
}

/// A scratch buffer for `esp_bootloader_esp_idf::partitions::read_partition_table`
/// -- heap-allocated (`Box`), not a local array: at `PARTITION_TABLE_MAX_LEN`
/// (0xC00 = 3072 bytes) it would trip `#![deny(clippy::large_stack_frames)]`
/// on an embassy task's already-tight stack.
fn table_buffer() -> Box<[u8; PARTITION_TABLE_MAX_LEN]> {
    Box::new([0u8; PARTITION_TABLE_MAX_LEN])
}

// --- otadata (embewi_boot_core) ---------------------------------------------
//
// `otadata` semantics (which slot is active, what to write for a transition)
// live in `embewi_boot_core`, shared with `embewi-boot`. What's here only
// finds the partition and drives the flash for it -- `esp-bootloader-esp-idf`
// is used purely as a partition-table *parser* (its own `OtaUpdater`/`Ota`,
// which read and write `otadata` in the older, non-committed format, are not
// used anywhere in this file).

const SLOT_COUNT: u8 = 2;
/// `otadata`'s two entries sit at the start of each of its two 4 KiB sectors.
const OTADATA_SECTOR: u32 = 0x1000;

fn slot_index(slot: AppPartitionSubType) -> Option<u8> {
    match slot {
        AppPartitionSubType::Ota0 => Some(0),
        AppPartitionSubType::Ota1 => Some(1),
        _ => None,
    }
}

fn slot_from_index(i: u8) -> Option<AppPartitionSubType> {
    match i {
        0 => Some(AppPartitionSubType::Ota0),
        1 => Some(AppPartitionSubType::Ota1),
        _ => None,
    }
}

/// The slot this device is currently running: among `Valid`/`Pending`
/// entries (the only states a slot that's actually executing can be in --
/// `New`/`Invalid`/`Aborted` never are), the one with the highest sequence.
/// Matches `embewi-boot`'s own candidate selection (`plan_boot`, and
/// `embewi_boot_core::activate`'s own choice of which sector to protect):
/// **not** "the first `Valid` entry found" -- a stale `Valid` entry can
/// legitimately survive in the other sector after a successful `confirm`
/// (nothing clears it, same as `plan_boot` never does), so two entries can
/// both read `Valid` at once. Picking the wrong one here would report the
/// slot that's actually running as the OTA *target*, letting `/ota/write`
/// overwrite the image this device is executing from.
fn otadata_active_slot(entries: &[boot_core::Raw; SLOT_COUNT as usize]) -> Option<u8> {
    entries
        .iter()
        .filter_map(|raw| match boot_core::decode(raw) {
            Decoded::Ok(e) if e.state == boot_core::state::VALID || e.state == boot_core::state::PENDING_VERIFY => {
                Some(e.seq)
            }
            _ => None,
        })
        .max()
        .map(|seq| boot_core::slot_of(seq, SLOT_COUNT))
}

/// Reads the two raw `otadata` entries and that partition's flash offset
/// (for callers that go on to write there). Free function (not a method) so
/// it can be called from inside another `with_raw_flash` closure, since
/// `PartitionTable`/`PartitionEntry` can't be returned out of one (they
/// borrow `buffer`, which lives only for the call).
fn read_otadata_raw(
    flash: &mut esp_storage::FlashStorage<'static>,
    buffer: &mut [u8; PARTITION_TABLE_MAX_LEN],
) -> Option<(u32, [boot_core::Raw; SLOT_COUNT as usize])> {
    let table = esp_bootloader_esp_idf::partitions::read_partition_table(flash, buffer).ok()?;
    let otadata = table.find_partition(PartitionType::Data(DataPartitionSubType::Ota)).ok().flatten()?;
    let base = otadata.offset();
    let mut entries = [boot_core::BLANK; SLOT_COUNT as usize];
    for (i, raw) in entries.iter_mut().enumerate() {
        ReadNorFlash::read(flash, base + i as u32 * OTADATA_SECTOR, raw).ok()?;
    }
    Some((base, entries))
}

/// `storage` must already be locked (a sync helper, callable from inside an
/// already-`.lock().await`ed section without deadlocking on it again).
fn read_otadata_locked(storage: &mut Storage) -> Option<[boot_core::Raw; SLOT_COUNT as usize]> {
    storage.with_raw_flash(|flash| read_otadata_raw(flash, &mut table_buffer()).map(|(_, e)| e)).flatten()
}

/// Where a new OTA image is currently allowed to go: the slot `otadata`
/// does *not* call `Valid`. `None` if that can't be determined -- `otadata`
/// unreadable, or with no `Valid` entry yet (a device `embewi-boot` hasn't
/// seeded, or one caught mid self-check with nothing confirmed at all,
/// neither of which should reach an HTTP handler in practice).
struct WriteTarget {
    slot: AppPartitionSubType,
    /// Absolute flash offset of the partition -- resolved here, once, and
    /// from here on cached by callers (`write_begin` puts it in the
    /// `WriteSession`) instead of re-parsing the partition table on every
    /// chunk. Safe to cache for a whole session: nothing else can move
    /// `otadata`'s active slot while a write is in flight (`activate` only
    /// ever runs after a session has already finished, once
    /// `staged.stage == Written`).
    offset: u32,
    size: usize,
}

fn write_target_locked(storage: &mut Storage) -> Option<WriteTarget> {
    storage
        .with_raw_flash(|flash| {
            let mut buffer = table_buffer();
            let (_, entries) = read_otadata_raw(flash, &mut buffer)?;
            let slot = slot_from_index(1 - otadata_active_slot(&entries)?)?;
            let table = esp_bootloader_esp_idf::partitions::read_partition_table(flash, &mut *buffer).ok()?;
            let app = table.find_partition(PartitionType::App(slot)).ok().flatten()?;
            Some(WriteTarget { slot, offset: app.offset(), size: app.len() as usize })
        })
        .flatten()
}

/// One `otadata` entry update, executed exactly as `embewi-boot` does it:
/// erase the sector, program the body (everything but the commit word),
/// program the commit word in its own command -- each step read back and
/// checked before the next. `storage` must already be locked.
fn execute_otadata_write(storage: &mut Storage, write: boot_core::Write) -> Result<(), OtadataError> {
    storage
        .with_raw_flash(|flash| -> Option<()> {
            let mut buffer = table_buffer();
            let (base, _) = read_otadata_raw(flash, &mut buffer)?;
            let base = base + u32::from(write.sector) * OTADATA_SECTOR;
            let [erase, body, commit] = write.ops();
            let mut back = [0u8; boot_core::ENTRY_SIZE];

            let boot_core::Op::Erase { .. } = erase else { return None };
            NorFlash::erase(flash, base, base + OTADATA_SECTOR).ok()?;
            ReadNorFlash::read(flash, base, &mut back).ok()?;
            if back != boot_core::BLANK {
                return None;
            }

            let boot_core::Op::Program { offset, len, data, .. } = body else { return None };
            NorFlash::write(flash, base + u32::from(offset), &data[..usize::from(len)]).ok()?;
            ReadNorFlash::read(flash, base, &mut back).ok()?;
            if back != write.entry.body() {
                return None;
            }

            let boot_core::Op::Program { offset, len, data, .. } = commit else { return None };
            NorFlash::write(flash, base + u32::from(offset), &data[..usize::from(len)]).ok()?;
            ReadNorFlash::read(flash, base, &mut back).ok()?;
            if back != write.entry.encode() || boot_core::decode(&back) != Decoded::Ok(write.entry) {
                return None;
            }
            Some(())
        })
        .flatten()
        .ok_or(OtadataError::Verify)
}

/// Why an `otadata` transition ([`otadata_confirm`]/[`otadata_reject`]/
/// [`otadata_activate`]) didn't happen.
enum OtadataError {
    /// The partition or its entries couldn't be read.
    Unavailable,
    /// `embewi_boot_core` found nothing to act on (no `Pending` entry for
    /// confirm/reject, no `Valid` entry to activate against) -- a boot-chain
    /// anomaly, not something to paper over.
    NoTransition,
    /// A write step didn't read back as expected.
    Verify,
}

/// The running image, self-checked and passing: `Pending` -> `Valid`.
async fn otadata_confirm(storage: &SharedStorage) -> Result<(), OtadataError> {
    let mut storage = storage.lock().await;
    let entries = read_otadata_locked(&mut storage).ok_or(OtadataError::Unavailable)?;
    let write = boot_core::confirm(entries).ok_or(OtadataError::NoTransition)?;
    execute_otadata_write(&mut storage, write)
}

/// The running image, self-checked and failing: `Pending` -> `Invalid`, so
/// the next boot falls back at once (`embewi-boot`'s `plan_boot` treats an
/// `Invalid`/`Aborted` entry as dead, never a candidate).
async fn otadata_reject(storage: &SharedStorage) -> Result<(), OtadataError> {
    let mut storage = storage.lock().await;
    let entries = read_otadata_locked(&mut storage).ok_or(OtadataError::Unavailable)?;
    let write = boot_core::reject(entries).ok_or(OtadataError::NoTransition)?;
    execute_otadata_write(&mut storage, write)
}

/// Arms `target` slot as `New` (contrat's `/ota/activate`): one committed
/// write, into whichever sector does not hold the last `Valid` entry, so the
/// slot that's still known-good stays selectable through any interruption.
async fn otadata_activate(storage: &SharedStorage, target: AppPartitionSubType) -> Result<(), OtadataError> {
    let mut storage = storage.lock().await;
    let entries = read_otadata_locked(&mut storage).ok_or(OtadataError::Unavailable)?;
    let target = slot_index(target).ok_or(OtadataError::Unavailable)?;
    let write = boot_core::activate(entries, SLOT_COUNT, target).map_err(|_| OtadataError::NoTransition)?;
    execute_otadata_write(&mut storage, write)
}

/// Contrat §4: `staged.state` ∈ `none | written | activating`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    None,
    Written,
    Activating,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::None => "none",
            Stage::Written => "written",
            Stage::Activating => "activating",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Stage::Written,
            2 => Stage::Activating,
            _ => Stage::None,
        }
    }
}

/// What's sitting in the inactive slot right now (contrat §4/§6's `staged`
/// object) -- see the module doc comment for why this lives in NVS.
pub struct Staged {
    pub stage: Stage,
    pub slot: String,
    pub digest: String,
    pub deployment_id: String,
    pub size: u32,
}

/// `storage` must already be locked -- see [`read_otadata_locked`]'s own
/// doc comment for why this crate names that variant `_locked` rather than
/// overloading. The single source of truth for what "staged" means on
/// flash; [`staged`] (self-locking, for async callers) and
/// [`NvsTransactionMetadata`] (sync, for `atomic_ota`) both read through
/// this, never duplicate it.
fn staged_locked(storage: &mut Storage) -> Staged {
    Staged {
        stage: Stage::from_u8(storage.get_u8(&NAMESPACE, &KEY_STAGE).unwrap_or(0)),
        slot: storage.get_string(&NAMESPACE, &KEY_SLOT).unwrap_or_default(),
        digest: storage.get_string(&NAMESPACE, &KEY_DIGEST).unwrap_or_default(),
        deployment_id: storage
            .get_string(&NAMESPACE, &KEY_DEPLOYMENT_ID)
            .unwrap_or_default(),
        size: storage.get_u32(&NAMESPACE, &KEY_SIZE).unwrap_or(0),
    }
}

pub async fn staged(storage: &SharedStorage) -> Staged {
    staged_locked(&mut *storage.lock().await)
}

/// Persists the staged-OTA record. Fails as soon as one field can't be
/// written: the record is only trustworthy when this returns `Ok`.
/// `storage` must already be locked -- see [`staged_locked`].
fn save_staged_locked(
    storage: &mut Storage,
    stage: Stage,
    slot: &str,
    digest: &str,
    deployment_id: &str,
    size: u32,
) -> Result<(), StorageError> {
    // `stage` is what `staged()` consumers switch on. Clearing drops it
    // first (a failure later leaves "nothing staged" beside stale details,
    // which is harmless); any other stage is published last, so a failure
    // never pairs a new stage with stale details.
    let clearing = stage == Stage::None;
    if clearing {
        storage.set_u8(&NAMESPACE, &KEY_STAGE, stage as u8)?;
    }
    storage.set_string(&NAMESPACE, &KEY_SLOT, slot)?;
    storage.set_string(&NAMESPACE, &KEY_DIGEST, digest)?;
    storage.set_string(&NAMESPACE, &KEY_DEPLOYMENT_ID, deployment_id)?;
    storage.set_u32(&NAMESPACE, &KEY_SIZE, size)?;
    if !clearing {
        storage.set_u8(&NAMESPACE, &KEY_STAGE, stage as u8)?;
    }
    Ok(())
}

async fn save_staged(
    storage: &SharedStorage,
    stage: Stage,
    slot: &str,
    digest: &str,
    deployment_id: &str,
    size: u32,
) -> Result<(), StorageError> {
    save_staged_locked(&mut *storage.lock().await, stage, slot, digest, deployment_id, size)
}

pub async fn clear_staged(storage: &SharedStorage) -> Result<(), StorageError> {
    save_staged(storage, Stage::None, "", "", "", 0).await
}

/// The one artifact kind v1 ever stages. A transaction's own identity is
/// `deployment_id` (`atomic_ota::TransactionRecord::id`) -- never
/// duplicated as this artifact's id, per `atomic-ota`'s own `transaction`
/// module doc comment on why the two must stay distinct even with a single
/// artifact today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactKind {
    Firmware,
}

/// Where an artifact goes -- deliberately narrower than
/// `AppPartitionSubType` (no `Factory`: that slot is never an OTA
/// write/activation target).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Ota0,
    Ota1,
}

impl Target {
    fn to_subtype(self) -> AppPartitionSubType {
        match self {
            Target::Ota0 => AppPartitionSubType::Ota0,
            Target::Ota1 => AppPartitionSubType::Ota1,
        }
    }

    fn from_subtype(subtype: AppPartitionSubType) -> Option<Target> {
        match subtype {
            AppPartitionSubType::Ota0 => Some(Target::Ota0),
            AppPartitionSubType::Ota1 => Some(Target::Ota1),
            _ => None,
        }
    }

    fn to_slot_name(self) -> &'static str {
        slot_name(self.to_subtype())
    }

    fn from_slot_name(name: &str) -> Option<Target> {
        Target::from_subtype(slot_from_name(name)?)
    }
}

/// `atomic_ota`'s transaction record, with this agent's concrete identity
/// types: the transaction id is `deployment_id`, the (one, for now)
/// artifact is always [`ArtifactKind::Firmware`].
type OtaTransaction = atomic_ota::TransactionRecord<String, ArtifactKind, Target>;

/// Parses the `sha256:<64 hex>` string NVS stores back into
/// `atomic_ota::Digest`'s raw bytes. Only used reading a staged record
/// back (`NvsTransactionMetadata::load`) -- writing one always has the raw
/// hasher output already in hand (`write_finish`), never round-trips
/// through this.
fn parse_digest(value: &str) -> Option<atomic_ota::Digest> {
    let hex = value.strip_prefix("sha256:")?;
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(atomic_ota::Digest(bytes))
}

fn format_digest(digest: &atomic_ota::Digest) -> String {
    let mut s = String::from("sha256:");
    for b in digest.0 {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Adapts the existing `staged`/NVS layout to `atomic_ota::TransactionMetadata`
/// -- same five keys, same on-flash bytes, no new layout. `storage` must
/// already be locked (constructed inside a `storage.lock().await` block):
/// `TransactionMetadata`'s methods are sync, `SharedStorage`'s own lock is
/// async, so the lock can only happen at the call site, same as
/// [`execute_otadata_write`]'s own `_locked` convention.
struct NvsTransactionMetadata<'a> {
    storage: &'a mut Storage,
}

impl atomic_ota::TransactionMetadata for NvsTransactionMetadata<'_> {
    type Error = StorageError;
    type Record = OtaTransaction;

    /// `Ok(None)` both for a genuinely empty `staged.stage` and for a
    /// non-empty one whose `slot`/`digest` don't parse -- the latter
    /// should never happen (`commit` never writes a non-empty stage
    /// without a valid slot/digest alongside it, in the same call), but if
    /// flash is corrupted regardless, "nothing usable staged" is the same
    /// refusal `activate`'s pre-`atomic_ota` form already gave via its own
    /// `slot_from_name(..).ok_or(NotStaged)`.
    fn load(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        let staged = staged_locked(self.storage);
        let state = match staged.stage {
            Stage::None => return Ok(None),
            Stage::Written => TransactionState::Staged,
            Stage::Activating => TransactionState::Activating,
        };
        let Some(target) = Target::from_slot_name(&staged.slot) else {
            return Ok(None);
        };
        let Some(digest) = parse_digest(&staged.digest) else {
            return Ok(None);
        };
        Ok(Some(atomic_ota::TransactionRecord {
            id: staged.deployment_id,
            state,
            artifacts: alloc::vec![atomic_ota::ArtifactRecord {
                id: ArtifactKind::Firmware,
                size: u64::from(staged.size),
                digest,
                target,
            }],
        }))
    }

    /// Guarantees old-or-new, never a mix -- but only by reusing the two
    /// transitions the NVS field-ordering discipline
    /// ([`save_staged_locked`]'s own doc comment) actually proves safe:
    /// `Empty -> anything` (stage published last) and `anything -> Empty`
    /// (stage cleared first). A `Some -> Some` commit whose identity or
    /// artifacts differ from what's already there would need to replace
    /// body fields *in place* while the stage marker stays non-empty
    /// throughout -- exactly the case that discipline does not cover (a
    /// crash mid-write could leave the old stage byte next to a mix of old
    /// and new body fields) -- so it is refused here rather than assumed
    /// safe. The caller (`write_finish`) must `commit(None)` first if it
    /// really means to replace a different, already-staged transaction.
    ///
    /// HTTP-adaptation debt, deliberately not paid down yet (no HTTP
    /// surface changes this step): this refusal surfaces to
    /// `write_finish`'s caller as `StorageError::Write`, which
    /// `WriteFinishError::Storage` maps to the same generic `500
    /// nvs_write_failed` any other NVS failure gets. It is really a state
    /// conflict, not a storage failure -- a future step giving it a
    /// distinct `409`-shaped response (once `ota_write.rs` is touched
    /// again, e.g. for the `ArtifactStorage` swap) should not need to
    /// change anything here beyond that mapping.
    fn commit(&mut self, record: Option<&Self::Record>) -> Result<(), Self::Error> {
        let current = self.load()?;
        if let (Some(a), Some(b)) = (&current, record) {
            if a.id != b.id || a.artifacts != b.artifacts {
                warn!(
                    "ota: refusing to replace a staged transaction in place (would mix old/new on a crash); clear to empty first"
                );
                return Err(StorageError::Write);
            }
        }
        match record {
            None => save_staged_locked(self.storage, Stage::None, "", "", "", 0),
            Some(r) => {
                let stage = match r.state {
                    TransactionState::Staged => Stage::Written,
                    TransactionState::Activating => Stage::Activating,
                    // `TransactionState` is `#[non_exhaustive]`: a variant
                    // this build doesn't recognize is never written.
                    _ => {
                        warn!("ota: refusing to commit a transaction state this build doesn't recognize");
                        return Err(StorageError::Write);
                    }
                };
                let Some(artifact) = r.artifacts.first() else {
                    warn!("ota: refusing to commit a transaction with no artifacts");
                    return Err(StorageError::Write);
                };
                let Ok(size) = u32::try_from(artifact.size) else {
                    return Err(StorageError::Write);
                };
                save_staged_locked(
                    self.storage,
                    stage,
                    artifact.target.to_slot_name(),
                    &format_digest(&artifact.digest),
                    &r.id,
                    size,
                )
            }
        }
    }
}

/// Digest of the currently-running, validated firmware -- empty until the
/// first successful OTA cycle (honest placeholder for a factory-flashed
/// image that's never been OTA'd, same reasoning `agent.rs` already
/// documents for the rest of `GET /info`).
pub async fn active_digest(storage: &SharedStorage) -> String {
    storage
        .lock()
        .await
        .get_string(&NAMESPACE, &KEY_ACTIVE_DIGEST)
        .unwrap_or_default()
}

/// The `deployment_id` of the currently-running, validated firmware --
/// empty until the first successful OTA cycle.
pub async fn active_deployment_id(storage: &SharedStorage) -> String {
    storage
        .lock()
        .await
        .get_string(&NAMESPACE, &KEY_ACTIVE_DEPLOYMENT_ID)
        .unwrap_or_default()
}

/// Contrat §4: `GET /info`'s `active_slot`. Reads the bootloader's own
/// the MMU to find the partition this code is *actually* running from --
/// deliberately not `OtaUpdater::selected_partition` (what `otadata` says
/// should boot next), confirmed on real hardware to diverge from this: a
/// silently-invalid image (bad app header, never even reaches
/// `pending_verify`) makes the bootloader fall back to the last-known-good
/// slot without updating `otadata` to match, so `selected_partition` kept
/// reporting the bad slot as "current" long after the device had already
/// recovered onto the other one.
pub async fn active_slot(storage: &SharedStorage) -> String {
    let mut storage = storage.lock().await;
    storage
        .with_raw_flash(|flash| {
            let mut buffer = table_buffer();
            let Ok(table) = esp_bootloader_esp_idf::partitions::read_partition_table(flash, &mut *buffer) else {
                return String::new();
            };
            match table.booted_partition() {
                Ok(Some(entry)) => String::from(entry.label_as_str()),
                _ => String::new(),
            }
        })
        .unwrap_or_default()
}

/// The image state relevant to the boot decision (contrat §3): a `Pending`
/// entry means a self-check is owed, regardless of which sector holds it;
/// otherwise a `Valid` entry means the running image is confirmed. Scanning
/// both entries for these two states (rather than resolving "the current
/// slot" the way `esp-bootloader-esp-idf`'s `Ota::current_slot()` did, by
/// comparing raw sequence numbers) is exactly what removes that hazard: it
/// needs no notion of "current slot" at all, just what `embewi_boot_core`
/// itself calls trustworthy.
async fn current_ota_image(storage: &SharedStorage) -> BackendOutcome {
    let mut storage = storage.lock().await;
    let Some(entries) = read_otadata_locked(&mut storage) else {
        return BackendOutcome::Other;
    };
    let is = |wanted: u32| {
        entries.iter().any(|raw| matches!(boot_core::decode(raw), Decoded::Ok(e) if e.state == wanted))
    };
    if is(boot_core::state::PENDING_VERIFY) {
        BackendOutcome::PendingConfirmation
    } else if is(boot_core::state::VALID) {
        BackendOutcome::Confirmed
    } else {
        BackendOutcome::Other
    }
}

/// `POST /v1alpha1/ota/prepare` request body (contrat §4). `artifact` and
/// `idf_version` are accepted but not declared here -- this agent isn't
/// ESP-IDF, so there's no meaningful running version to compare `idf_version`
/// against; serde ignores fields a struct doesn't declare, same as every
/// other `POST` body in `agent.rs`.
#[derive(Deserialize)]
pub struct PrepareRequest {
    pub size: u32,
    pub chip: String,
    pub partition_layout: String,
}

/// `POST /v1alpha1/ota/prepare` response body (contrat §4).
#[derive(Serialize)]
pub struct PrepareResponse {
    accepted: bool,
    target_slot: Option<&'static str>,
    reason: Option<&'static str>,
}

fn refuse(reason: &'static str) -> PrepareResponse {
    PrepareResponse { accepted: false, target_slot: None, reason: Some(reason) }
}

/// Validates compat *before* a single byte transfers (contrat §3: "un
/// binaire esp32-s3 flashé sur esp32 ne boote pas").
pub async fn prepare(storage: &SharedStorage, req: &PrepareRequest) -> PrepareResponse {
    if req.chip != esp_metadata_generated::chip_pretty!() {
        return refuse("chip_mismatch");
    }
    if req.partition_layout != PARTITION_LAYOUT {
        return refuse("layout_mismatch");
    }

    let mut storage = storage.lock().await;
    let Some(target) = write_target_locked(&mut storage) else {
        return refuse("busy");
    };
    if req.size as usize > target.size {
        return refuse("size_too_large");
    }
    PrepareResponse { accepted: true, target_slot: Some(slot_name(target.slot)), reason: None }
}

/// Flash sector size, and the unit [`EspArtifactStorage`] flushes to flash
/// -- see its own doc comment. The generic engine
/// (`atomic_ota::WriteSession`, held by [`WriteSession::engine`]) has no
/// notion of this at all: it only ever offers whatever undurable tail it's
/// holding and trusts the durable watermark this backend reports.
const OTA_SECTOR: u32 = 0x1000;

/// Nothing to report beyond pass/fail: the only thing that can go wrong
/// here is the underlying `NorFlash` erase/program call, which
/// [`EspArtifactStorage::flush_sector`]'s own `Option` already reduces to
/// that -- matches what `flush_sector`'s pre-`atomic_ota` `bool` return
/// carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlashWriteFailed;

/// Implements `atomic_ota::ArtifactStorage` over exactly the same
/// sector-buffered `NorFlash` mechanism the pre-`atomic_ota` `WriteSession`
/// always used: same sector size, same padding, same erase-then-program
/// pair per sector, same target partition. Nothing about *how* flash is
/// written changes here -- only *who* decides when to call it (the engine,
/// via the `pending` tail it offers on every call, instead of this crate's
/// own byte-accumulation loop).
///
/// Constructed fresh for each `append`/`finish` call, borrowing whichever
/// `Storage` lock is currently held: it cannot be stored inside the
/// persistent [`WriteSession`] the way `scratch` is, since `Storage`'s own
/// lock is async and reacquired per call -- see
/// `atomic_ota::WriteSession`'s own doc comment on why `append`/`finish`
/// take a backend by `&mut` instead of owning one.
struct EspArtifactStorage<'a> {
    storage: &'a mut Storage,
    partition_offset: u32,
    /// The session's own scratch buffer ([`WriteSession::scratch`]),
    /// borrowed for this call only -- not owned here, so no extra
    /// allocation happens per chunk.
    scratch: &'a mut [u8; OTA_SECTOR as usize],
}

impl EspArtifactStorage<'_> {
    /// Erases and programs the sector at absolute flash offset `at` with
    /// `self.scratch[..len]`, padded to the flash word size -- identical to
    /// the pre-`atomic_ota` free function of the same name: one erase, one
    /// program, the pad explicitly zeroed and never treated as image bytes.
    ///
    /// `NorFlash::write` (unlike the auto-RMW `embedded_storage::Storage`
    /// trait this replaced, back before `atomic_ota` existed) requires both
    /// the offset and the length to be a multiple of the flash word size (4
    /// bytes here) -- always true for a full sector (`OTA_SECTOR` is 4096),
    /// but the final, partial sector at `finish` rarely lands on a word
    /// boundary.
    ///
    /// Invariant this keeps: padding is a hardware-write-granularity
    /// detail, never part of the OTA payload -- the pad is explicitly
    /// zeroed (never leftover, indeterminate buffer content) and never
    /// counted by `atomic_ota::WriteSession`'s own `received`/`durable`/
    /// digest, which only ever see the logical `len` bytes this function
    /// was actually asked to write.
    fn flush_sector(&mut self, at: u32, len: usize) -> Option<()> {
        let padded = len.div_ceil(4) * 4;
        debug_assert!(padded <= OTA_SECTOR as usize, "padding must never cross the sector it belongs to");
        self.scratch[len..padded].fill(0);
        self.storage
            .with_raw_flash(|flash| -> Option<()> {
                NorFlash::erase(flash, at, at + OTA_SECTOR).ok()?;
                NorFlash::write(flash, at, &self.scratch[..padded]).ok()
            })
            .flatten()
    }
}

impl atomic_ota::ArtifactStorage for EspArtifactStorage<'_> {
    type Error = FlashWriteFailed;

    /// Flushes every *whole* sector `pending` contains, one erase+program
    /// per sector -- never more than once per sector, which is the entire
    /// reason a session buffers before ever touching flash (esp-storage's
    /// convenience `embedded_storage::Storage::write` does a
    /// read-modify-erase-rewrite of the whole sector on every call that
    /// isn't itself sector-aligned; with a 1 KiB HTTP read buffer, that
    /// meant up to four full erase+reprogram cycles of the same sector
    /// instead of one). Leaves any short-of-a-sector remainder in `pending`
    /// untouched -- `durable_offset` is therefore always sector-aligned
    /// here (only [`Self::finish`] ever sees or writes a partial sector).
    fn write(&mut self, durable_offset: u64, pending: &[u8]) -> Result<u64, Self::Error> {
        let mut consumed = 0u32;
        while pending.len() - consumed as usize >= OTA_SECTOR as usize {
            let chunk = &pending[consumed as usize..consumed as usize + OTA_SECTOR as usize];
            self.scratch.copy_from_slice(chunk);
            let sector_index = (durable_offset + u64::from(consumed)) / u64::from(OTA_SECTOR);
            let at = self.partition_offset + sector_index as u32 * OTA_SECTOR;
            self.flush_sector(at, OTA_SECTOR as usize).ok_or(FlashWriteFailed)?;
            consumed += OTA_SECTOR;
        }
        Ok(durable_offset + u64::from(consumed))
    }

    /// Flushes whatever short-of-a-sector tail is left (see
    /// [`Self::write`]'s own doc comment on why it's never more than that).
    fn finish(&mut self, durable_offset: u64, pending: &[u8]) -> Result<u64, Self::Error> {
        if pending.is_empty() {
            return Ok(durable_offset);
        }
        debug_assert!(pending.len() < OTA_SECTOR as usize, "finish must only ever see a short-of-a-sector tail");
        self.scratch[..pending.len()].copy_from_slice(pending);
        let sector_index = durable_offset / u64::from(OTA_SECTOR);
        let at = self.partition_offset + sector_index as u32 * OTA_SECTOR;
        self.flush_sector(at, pending.len()).ok_or(FlashWriteFailed)?;
        Ok(durable_offset + pending.len() as u64)
    }
}

/// In-RAM write session (see the module doc comment for why this doesn't
/// need to survive a reboot). One at a time, matching `firmware-c`'s own
/// single static session -- this device only ever serves one HTTP
/// connection at a time anyway.
struct WriteSession {
    slot: AppPartitionSubType,
    /// Absolute flash offset of the target partition, resolved once in
    /// [`write_begin`] ([`WriteTarget`]) and cached for the whole session --
    /// no partition-table re-parse per chunk.
    partition_offset: u32,
    /// One sector, filled from the engine's own undurable tail as
    /// [`EspArtifactStorage`] flushes it. Heap-allocated (`Box`): 4 KiB is
    /// too large to risk on an embassy task's stack
    /// (`#![deny(clippy::large_stack_frames)]`), and this way nothing
    /// changes if `OTA_SECTOR` ever grows. Allocated once here and reused
    /// for the whole session (borrowed by a fresh `EspArtifactStorage` on
    /// every call) -- never reallocated per chunk.
    scratch: Box<[u8; OTA_SECTOR as usize]>,
    /// The generic engine: received/durable byte counts, the undurable
    /// tail, and the streaming digest -- see `atomic_ota::artifact`'s own
    /// doc comment. Everything sector-shaped stays out here, in
    /// [`EspArtifactStorage`]; the engine itself has no notion of it.
    engine: atomic_ota::WriteSession,
    /// When `write_begin` opened this session -- purely diagnostic, logged
    /// by `write_finish` (contrat §4's own `written`/digest reply carries
    /// no timing field).
    started_at: Instant,
    /// How many sectors have been erased+programmed so far -- purely
    /// diagnostic, alongside `started_at`. Derived from how far
    /// `engine.durable()` moves on each call (always a whole number of
    /// sectors, except `write_finish`'s own final partial one).
    sectors_flushed: u32,
    /// Frozen at the first PUT: what the image is (`deployment_id`,
    /// `digest`) and how big (`total`). Every later PUT of the session must
    /// repeat them exactly ([`write_params_match`]) and `write_finish`
    /// uses these, never whatever the last request happened to carry.
    params: SessionParams,
}

/// The identity of one write session, fixed by its first PUT.
pub struct SessionParams {
    pub deployment_id: String,
    /// `sha256:<64 hex>`, as sent.
    pub digest: String,
    /// Full image size: `Content-Range`'s total, or `Content-Length` for a
    /// monolithic PUT.
    pub total: u32,
}

static WRITE_SESSION: Mutex<CriticalSectionRawMutex, Option<WriteSession>> = Mutex::new(None);

pub async fn write_in_progress() -> bool {
    WRITE_SESSION.lock().await.is_some()
}

/// Bytes durably on flash -- see `atomic_ota::WriteSession::durable`'s own
/// doc comment. This is what the JSON `written` field reports to the
/// client: the point it's safe to resume *after a dropped connection* from.
pub async fn write_written() -> u32 {
    WRITE_SESSION.lock().await.as_ref().map_or(0, |s| s.engine.durable() as u32)
}

/// Bytes accepted into the session so far. Distinct from `write_written`
/// and used only for `write_plan`'s Continue-vs-Resync decision --
/// consecutive chunks of one *uninterrupted* PUT sequence declare their
/// `start` as "how much I've sent so far", which -- unless the connection
/// actually dropped -- is this, not `write_written` (which lags behind it
/// by up to one sector). Conflating the two would spuriously 416 a live
/// transfer whose chunk size doesn't happen to be a multiple of the flash
/// sector size.
pub async fn write_received() -> u32 {
    WRITE_SESSION.lock().await.as_ref().map_or(0, |s| s.engine.received() as u32)
}

/// `Content-Range` header parsing/shape-checking is transport-specific and
/// stays pure enough to unit-test with a plain `cargo test` (no ESP32
/// hardware involved) in `ota-logic` (workspace crate, `crates/ota-logic`).
/// Re-exported so callers keep writing `ota::parse_content_range` as if it
/// were still defined in this module.
pub use ota_logic::{is_valid_digest, parse_content_range, range_len};

/// `PUT /v1alpha1/ota/write`'s resume decision itself (contrat §4's
/// `Content-Range` protocol, decoupled from `Content-Range`'s own wire
/// format) is `atomic_ota::resume_plan`/`atomic_ota::is_complete` --
/// generic, `no_std`, host-tested in that crate. These two functions are
/// thin `u32`-to-`u64` adapters so callers keep writing `ota::Plan`/
/// `ota::write_plan`/`ota::write_is_final` unchanged; nothing about the
/// decision itself lives here anymore.
pub use atomic_ota::ResumePlan as Plan;

pub fn write_plan(has_range: bool, start: u32, in_progress: bool, written: u32) -> Plan {
    atomic_ota::resume_plan(has_range, u64::from(start), in_progress, u64::from(written))
}

pub fn write_is_final(has_range: bool, end: u32, total: u32) -> bool {
    atomic_ota::is_complete(has_range, u64::from(end), u64::from(total))
}

/// Whether a continuing PUT carries the same `deployment_id`, digest and
/// total as the session it claims to resume.
pub async fn write_params_match(params: &SessionParams) -> bool {
    WRITE_SESSION.lock().await.as_ref().is_some_and(|s| {
        s.params.deployment_id == params.deployment_id
            && s.params.digest.eq_ignore_ascii_case(&params.digest)
            && s.params.total == params.total
    })
}

pub enum BeginError {
    /// The next slot couldn't be resolved.
    Busy,
    /// The declared image doesn't fit the slot.
    TooLarge,
}

/// Starts (or restarts) a write session against whichever slot the
/// bootloader would currently hand out next. Always re-derived fresh here
/// rather than cached from `/ota/prepare`: `firmware-c`'s `write_begin`
/// does the same (see its own comment for why) -- prepare is a compat
/// pre-check, not a reservation.
pub async fn write_begin(storage: &SharedStorage, params: SessionParams) -> Result<(), BeginError> {
    let target = {
        let mut storage = storage.lock().await;
        write_target_locked(&mut storage).ok_or(BeginError::Busy)?
    };
    if params.total as usize > target.size {
        return Err(BeginError::TooLarge);
    }
    // `params.digest` is already validated (`is_valid_digest`, in
    // `ota_write.rs`, before `write_begin` is ever reached): parsing it
    // here cannot actually fail. Handled as a real error rather than a
    // panic regardless -- an internal invariant slipping should refuse the
    // write, not crash the whole path.
    let Some(expected_digest) = parse_digest(&params.digest) else {
        warn!("ota: write_begin got an unparseable digest past validation, refusing");
        return Err(BeginError::Busy);
    };
    *WRITE_SESSION.lock().await = Some(WriteSession {
        slot: target.slot,
        partition_offset: target.offset,
        scratch: Box::new([0u8; OTA_SECTOR as usize]),
        engine: atomic_ota::WriteSession::begin(u64::from(params.total), expected_digest),
        started_at: Instant::now(),
        sectors_flushed: 0,
        params,
    });
    Ok(())
}

/// Appends `data` to the session, flushing whole sectors to flash as they
/// fill (see [`WriteSession`]'s and [`EspArtifactStorage`]'s doc comments).
/// Handles `data` of any length, not just the HTTP handler's own
/// read-buffer size -- it may span several sectors in one call.
pub async fn write_chunk(storage: &SharedStorage, data: &[u8]) -> bool {
    let mut session_guard = WRITE_SESSION.lock().await;
    let Some(session) = session_guard.as_mut() else {
        return false;
    };

    // Never touch flash for a chunk the session would refuse anyway --
    // `engine.append` checks this too (`Error::TooLarge`), but checking it
    // here first avoids locking storage at all for a chunk that's already
    // doomed, same as the pre-`atomic_ota` code did.
    let received = session.engine.received();
    if u64::try_from(data.len()).ok().and_then(|len| received.checked_add(len)).is_none_or(|end| end > u64::from(session.params.total))
    {
        return false;
    }

    let mut storage_guard = storage.lock().await;
    let mut backend =
        EspArtifactStorage { storage: &mut storage_guard, partition_offset: session.partition_offset, scratch: &mut *session.scratch };
    let before = session.engine.durable();
    let ok = session.engine.append(&mut backend, data).is_ok();
    let after = session.engine.durable();
    session.sectors_flushed += ((after - before) as u32).div_ceil(OTA_SECTOR);
    ok
}

pub struct WriteFinishOk {
    pub written: u32,
    pub digest: String,
}

pub enum WriteFinishError {
    NotWriting,
    DigestMismatch,
    /// The session ended short of its declared total.
    Incomplete,
    /// The image was written and verified, but the staged record couldn't
    /// be persisted -- it must not be reported as `written`.
    Storage(StorageError),
}

/// Closes the write session: compares the digest computed *while writing*
/// (never a post-hoc flash re-read, per contrat §4) against the one the
/// session was opened with, and on a match persists the staged state
/// (contrat §6). Both the expected digest and the `deployment_id` come
/// from the session -- fixed by its first PUT, not by the last request.
pub async fn write_finish(storage: &SharedStorage) -> Result<WriteFinishOk, WriteFinishError> {
    let Some(session) = WRITE_SESSION.lock().await.take() else {
        return Err(WriteFinishError::NotWriting);
    };
    let WriteSession { slot, partition_offset, mut scratch, engine, started_at, mut sectors_flushed, params } = session;

    // The image's own size rarely lands on a sector boundary: `finish`
    // flushes whatever's left buffered (a final, partial sector) before
    // checking completeness and the digest (see `EspArtifactStorage`'s own
    // comment on the padding this needs).
    let before = engine.durable();
    let committed = {
        let mut storage_guard = storage.lock().await;
        let mut backend =
            EspArtifactStorage { storage: &mut storage_guard, partition_offset, scratch: &mut *scratch };
        match engine.finish(&mut backend) {
            Ok(committed) => committed,
            Err(atomic_ota::Error::DigestMismatch(computed)) => {
                warn!("ota: digest mismatch, attendu={} calculé={}", params.digest, format_digest(&computed));
                return Err(WriteFinishError::DigestMismatch);
            }
            Err(atomic_ota::Error::Incomplete { durable }) => {
                warn!("ota: session ended at {durable} of {} octets", params.total);
                return Err(WriteFinishError::Incomplete);
            }
            Err(e) => {
                warn!("ota: write finish failed ({e:?})");
                return Err(WriteFinishError::Incomplete);
            }
        }
    };
    sectors_flushed += ((committed.size - before) as u32).div_ceil(OTA_SECTOR);

    let digest = format_digest(&committed.digest);
    let Some(target) = Target::from_subtype(slot) else {
        // Can't happen (`write_target_locked` only ever hands out Ota0/
        // Ota1), kept as a real error rather than a panic.
        return Err(WriteFinishError::Storage(StorageError::Write));
    };
    let record = OtaTransaction::staged(
        params.deployment_id.clone(),
        atomic_ota::ArtifactRecord { id: ArtifactKind::Firmware, size: committed.size, digest: committed.digest, target },
    );
    {
        let mut storage_guard = storage.lock().await;
        let mut meta = NvsTransactionMetadata { storage: &mut storage_guard };
        atomic_ota::TransactionMetadata::commit(&mut meta, Some(&record)).map_err(WriteFinishError::Storage)?;
    }
    let elapsed = started_at.elapsed();
    info!(
        "ota: write OK {} octets ({} secteurs erase+program) en {}ms slot={} -> staged=written",
        committed.size,
        sectors_flushed,
        elapsed.as_millis(),
        slot_name(slot)
    );
    Ok(WriteFinishOk { written: committed.size as u32, digest })
}

/// `POST /v1alpha1/ota/activate` (contrat §4): points the bootloader at the
/// staged slot and arms `OtaImageState::New` (which it promotes to
/// `PendingVerify` on the next boot). Reads the target slot from the NVS
/// `staged` state, not the in-RAM write session -- matches `firmware-c`'s
/// own fallback ("Reprise après reboot de l'agent entre write et
/// activate"), and works identically whether or not this device rebooted
/// since `/ota/write` finished.
pub async fn activate(storage: &SharedStorage, deployment_id: &str) -> Result<&'static str, ActivateError> {
    // Record the intent first: if NVS refuses it, nothing has changed yet
    // and the caller gets an error instead of a reboot into a slot whose
    // staged record disagrees with `otadata`. `atomic_ota::activate` checks
    // `Staged` + identity (against the *transaction's* id, i.e.
    // `deployment_id` -- never an artifact's own id) and durably commits
    // the transition to `Activating` before returning.
    let activating = {
        let mut storage_guard = storage.lock().await;
        let mut meta = NvsTransactionMetadata { storage: &mut storage_guard };
        atomic_ota::activate(&mut meta, &String::from(deployment_id)).map_err(|e| match e {
            atomic_ota::Error::NotStaged => ActivateError::NotStaged,
            atomic_ota::Error::IdentityMismatch => ActivateError::DeploymentMismatch,
            atomic_ota::Error::Backend(storage_err) => ActivateError::Storage(storage_err),
            // `atomic_ota::Error` is `#[non_exhaustive]`: refuse rather
            // than guess at an outcome this build doesn't recognize.
            _ => ActivateError::NotStaged,
        })?
    };
    // `atomic_ota::activate` already refused an empty artifact list.
    let target = activating.artifacts[0].target;
    let esp_target = target.to_subtype();

    let ok = otadata_activate(storage, esp_target).await.is_ok();
    if !ok {
        // Best effort: back to `Staged` so a retry of `activate` is possible.
        let reverted = activating.with_state(TransactionState::Staged);
        let mut storage_guard = storage.lock().await;
        let mut meta = NvsTransactionMetadata { storage: &mut storage_guard };
        if atomic_ota::TransactionMetadata::commit(&mut meta, Some(&reverted)).is_err() {
            warn!("ota: activate failed and the staged record couldn't be restored to `written`");
        }
        return Err(ActivateError::NotStaged);
    }

    info!("ota: activate dep={deployment_id} -> slot={} prêt, reboot imminent", target.to_slot_name());
    Ok(slot_name(esp_target))
}

pub enum ActivateError {
    /// Nothing staged, or `otadata` couldn't be updated (`409 not_staged`,
    /// as before).
    NotStaged,
    /// The staged image belongs to another deployment (`409`).
    DeploymentMismatch,
    /// The staged record couldn't be persisted; nothing was activated.
    Storage(StorageError),
}

/// Records the validated image's digest and `deployment_id` as the active
/// ones. Idempotent, so an interrupted validation can be finished at the
/// next boot ([`Action::FinishInterruptedActivation`]).
async fn promote_staged(storage: &SharedStorage, staged: &Staged) -> Result<(), StorageError> {
    let mut storage = storage.lock().await;
    storage.set_string(&NAMESPACE, &KEY_ACTIVE_DIGEST, &staged.digest)?;
    storage.set_string(&NAMESPACE, &KEY_ACTIVE_DEPLOYMENT_ID, &staged.deployment_id)
}

/// Promotes the staged record once the image is confirmed, then forgets it.
/// If a write fails the `activating` record is kept and the agent goes
/// `Degraded`: the image is valid and stays so (never rolled back over
/// bookkeeping), and the next boot -- bootloader `Valid`, same slot, still
/// `activating` -- completes this promotion.
async fn finish_validation(storage: &SharedStorage, staged: &Staged) {
    if promote_staged(storage, staged).await.is_err() {
        warn!("ota: validated image's digest/deployment_id couldn't be persisted, will retry at next boot");
        agent::set_state(agent::State::Degraded);
        return;
    }
    if clear_staged(storage).await.is_err() {
        warn!("ota: staged record couldn't be cleared after validation, will retry at next boot");
        agent::set_state(agent::State::Degraded);
        return;
    }
    agent::set_state(agent::State::Running);
    info!("ota: validation done (deployment_id={})", staged.deployment_id);
}

/// Confirms the just-self-checked image with the bootloader and cancels its
/// pending rollback. Only ever called after every self-check passes
/// (contrat §3: "mark_valid n'est appelé QUE si tous les checks passent").
async fn mark_valid(storage: &'static SharedStorage) {
    let staged = staged(storage).await;
    if let Err(e) = otadata_confirm(storage).await {
        // Couldn't even record validation -- don't claim `running` over an
        // image `embewi-boot` doesn't agree is confirmed.
        warn!("ota: couldn't confirm the running image (code {}), rolling back", e as u8);
        mark_invalid_and_reboot(storage).await;
    }
    // `otadata_confirm` only returns `Ok` once the committed `Valid` entry
    // has been read back and decoded exactly as written (`execute_otadata_write`).
    // Only now does the anti-freeze watchdog come off: a freeze anywhere
    // before this point -- including one the self-check race itself can't
    // catch -- still resets into a `Pending` entry embewi-boot rolls back.
    disable_boot_watchdog();
    finish_validation(storage, &staged).await;
}

/// The rollback path (contrat §3): marks the image invalid and resets.
/// Never returns -- on reboot, `embewi-boot` sees `Invalid`/`Aborted` (or a
/// stuck `Pending`, if even this much couldn't complete, itself turned
/// `Aborted` on the next boot) and falls back to the previous slot on its
/// own; this agent doesn't drive that part.
async fn mark_invalid_and_reboot(storage: &'static SharedStorage) -> ! {
    if let Err(e) = otadata_reject(storage).await {
        warn!("ota: couldn't record rejection (code {}), resetting anyway", e as u8);
    }
    warn!("ota: self-check failed, marking image invalid and rebooting for rollback");
    // The anti-freeze watchdog (armed for this whole pending_verify window,
    // see `arm_boot_watchdog`) is deliberately left running here, not
    // disabled: this reset is about to happen anyway, and if *this* call
    // itself somehow never returns, the watchdog is still the backstop.
    //
    // Gives the log line above time to actually reach the WebSocket log
    // stream/serial console before the reset cuts it off.
    Timer::after(Duration::from_millis(200)).await;
    esp_hal::system::software_reset();
}

// --- anti-freeze watchdog ----------------------------------------------
//
// `esp_hal::init()` unconditionally disables every watchdog on the chip
// (there's no `Config` option to keep one running) as part of its normal
// hardware bring-up -- so a watchdog `embewi-boot` armed before jumping here
// does not survive into this agent; only the agent's own code can protect
// its post-`init()` startup. `arm_boot_watchdog` is called right after
// `TimerGroup::new(peripherals.TIMG0)`/`esp_rtos::start` (`src/bin/main.rs`)
// -- not any earlier: `TimerGroup::new`'s first use of TIMG0 resets the
// whole peripheral block (`PeripheralClockControl`'s refcount going 0 -> 1),
// which would silently wipe out a watchdog armed before that call. From
// there it covers everything through `on_boot`'s decision -- `Storage::new`,
// not just the bounded self-check race inside `on_boot` itself. `on_boot`
// disables it again within milliseconds for every outcome except
// a genuine `pending_verify` self-check, where [`feed_boot_watchdog`] keeps
// it running until the image is durably confirmed
// ([`mark_valid`]/[`disable_boot_watchdog`]).
//
// TIMG0's own watchdog (not RTC_CNTL's, which `http::run`'s
// `reboot_after_delay` already threads `LPWR` through for its own
// unrelated `/reboot`/`/ota/activate` use). `Wdt::new()` needs no peripheral
// value -- like `esp_hal::init()`'s own internal disabling code, it derives
// register access purely from the `TIMG0` type parameter -- so this needs
// no plumbing through `main`'s peripherals at all.
fn boot_watchdog() -> esp_hal::timer::timg::Wdt<esp_hal::peripherals::TIMG0<'static>> {
    esp_hal::timer::timg::Wdt::new()
}

/// Arms the anti-freeze watchdog. Call exactly once, right after
/// `TimerGroup::new(peripherals.TIMG0)` (see the module section comment
/// above for why not any earlier).
pub fn arm_boot_watchdog() {
    let mut wdt = boot_watchdog();
    wdt.set_timeout(esp_hal::timer::timg::MwdtStage::Stage0, WATCHDOG_DEADLINE);
    wdt.enable();
}

fn feed_boot_watchdog() {
    boot_watchdog().feed();
}

fn disable_boot_watchdog() {
    boot_watchdog().disable();
}

#[embassy_executor::task]
async fn selfcheck_task(storage: &'static SharedStorage) {
    // TEST/DEBUG ONLY (`fault-injection-freeze` feature, never in a
    // production image): starves the executor before the self-check's own
    // software deadline (below) can ever be polled -- the one failure mode
    // that deadline structurally can't catch, since it depends on the same
    // stuck executor. Only the hardware watchdog (`arm_boot_watchdog`,
    // already armed and fed by `on_boot` before this task was spawned) can
    // recover from this; if it doesn't, this loop runs forever.
    if cfg!(feature = "fault-injection-freeze") {
        warn!("ota: [fault-injection-freeze] spinning forever, only the hardware watchdog can save this boot");
        loop {
            core::hint::spin_loop();
        }
    }
    // contrat §3: bounded by a deadline, not just "run the checks" -- a
    // hung check must never leave the device stuck in `pending_verify`
    // forever. Uses a plain `embassy_time::Timer` race rather than a
    // hardware watchdog: `LPWR` (the RTC peripheral the watchdog lives on)
    // is already owned by `http::run`'s own reboot mechanism for the
    // config page/`/reboot`/`/ota/activate` (see that module's doc comment
    // for why it needs RTC specifically, not `system::software_reset()`);
    // a plain software reset here is a faithful port of what `firmware-c`
    // itself does in this exact spot (an `esp_timer` deadline calling
    // `esp_restart()`, not a TWDT trip).
    match select(async { storage.lock().await.self_check() }, Timer::after(SELFCHECK_DEADLINE)).await {
        Either::First(true) => {
            // TEST/DEBUG ONLY (`fault-injection` feature, never in a
            // production image): reset right here, after the self-check
            // passed but before `confirm` ever runs -- so `otadata` is left
            // exactly as a real crash mid-`pending_verify` would leave it
            // (still `Pending`), for embewi-boot's rollback to act on on the
            // next boot. Deterministic, unlike timing a physical power-cut
            // against a window that normally closes before Wi-Fi even
            // reconnects.
            if cfg!(feature = "fault-injection") {
                warn!("ota: [fault-injection] self-check passed, resetting BEFORE confirm to exercise rollback");
                Timer::after(Duration::from_millis(200)).await;
                esp_hal::system::software_reset();
            }
            mark_valid(storage).await
        }
        Either::First(false) => mark_invalid_and_reboot(storage).await,
        Either::Second(()) => {
            warn!("ota: self-check deadline exceeded, forcing a reset (bootloader will roll back)");
            esp_hal::system::software_reset();
        }
    }
}

/// Called once at boot (`src/bin/main.rs`): reconciles the persisted staged
/// record with what the bootloader actually booted (contrat §3's "cœur dur
/// du projet"). The decision itself is [`atomic_ota::reconcile`], a pure
/// table host-tested in that crate; this only gathers its inputs and
/// applies the outcome. This is the only place `agent::State` is driven
/// from `Booting`.
pub async fn on_boot(storage: &'static SharedStorage, spawner: Spawner) {
    let staged = staged(storage).await;
    let image = current_ota_image(storage).await;
    let booted = active_slot(storage).await;
    let booted_is_staged = (!booted.is_empty()).then(|| booted == staged.slot);
    let staged_state = match staged.stage {
        Stage::None => None,
        Stage::Written => Some(TransactionState::Staged),
        Stage::Activating => Some(TransactionState::Activating),
    };
    let action = atomic_ota::reconcile(staged_state, image, booted_is_staged);
    info!("ota: boot slot={booted:?} staged={} image={image:?} -> {action:?}", staged.stage.as_str());

    match action {
        Action::AwaitConfirmation => {
            agent::set_state(agent::State::PendingVerify);
            warn!("ota: image is PENDING_VERIFY, starting bounded self-check (deadline {SELFCHECK_DEADLINE:?})");
            // Anti-freeze backstop stays armed (see `arm_boot_watchdog`'s doc
            // comment): fed here for a fresh window covering the self-check
            // and the confirm that follows it. Every other outcome below
            // disables it instead.
            feed_boot_watchdog();
            if let Ok(token) = selfcheck_task(storage) {
                spawner.spawn(token);
            }
            return;
        }
        Action::RollbackUnaccounted => {
            warn!("ota: PENDING_VERIFY image not accounted for by the staged record, rolling back");
            agent::set_state(agent::State::Rollback);
            mark_invalid_and_reboot(storage).await;
        }
        Action::Nothing | Action::KeepStaged => {}
        Action::ClearStale => {
            warn!("ota: stale staged record ({}), clearing", staged.stage.as_str());
            if clear_staged(storage).await.is_err() {
                warn!("ota: stale staged record couldn't be cleared");
            }
        }
        Action::FinishInterruptedActivation => {
            warn!("ota: finishing a validation interrupted before its bookkeeping");
            // Runs the same path as a live validation; `Degraded` (set by
            // it on failure) must not be overwritten below.
            finish_validation(storage, &staged).await;
            if agent::state() == agent::State::Degraded {
                return;
            }
        }
        // `Action` is `#[non_exhaustive]`: atomic-ota is not at a stable API
        // yet, and a future variant must not silently fall into one of the
        // arms above. Nothing destructive on an outcome this build doesn't
        // recognize -- same policy as `running_matches_staged: None`.
        _ => {
            warn!("ota: reconcile returned an action this build doesn't recognize, doing nothing");
        }
    }

    // Not a pending_verify boot after all (or one that needed no further
    // action): the anti-freeze window `arm_boot_watchdog` opened at the top
    // of `main` is over.
    disable_boot_watchdog();

    // The NVS canary round-trip `/health` reports on (during
    // `pending_verify` the self-check task runs it instead).
    if !storage.lock().await.self_check() {
        warn!("ota: boot NVS self-check failed, /health will report storage=fail");
    }
    agent::set_state(agent::State::Running);
}
