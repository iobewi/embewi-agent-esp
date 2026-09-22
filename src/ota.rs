//! OTA A/B updates (contrat §3/§4/§6). The Core streams a raw `.bin` into
//! whichever `ota_0`/`ota_1` slot isn't currently booted.
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
//! no legacy mode, matching `embewi-boot`.
//!
//! Mirrors `firmware-c`'s `embewi_ota.c`/`embewi_selfcheck.c` state machine
//! (same `stage`/`slot`/`digest`/`deployment_id`/`size` staged-NVS layout,
//! same `embewi_ota_plan`/`embewi_ota_is_final` pure resume logic for
//! `Content-Range`), reimplemented against embassy tasks instead of
//! ESP-IDF's C one and FreeRTOS tasks.
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
use sha2::{Digest, Sha256};

use crate::agent;
use ota_logic::{BootAction, BootImage, StagedKind, boot_action};

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

pub async fn staged(storage: &SharedStorage) -> Staged {
    let mut storage = storage.lock().await;
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

/// Persists the staged-OTA record. Fails as soon as one field can't be
/// written: the record is only trustworthy when this returns `Ok`.
async fn save_staged(
    storage: &SharedStorage,
    stage: Stage,
    slot: &str,
    digest: &str,
    deployment_id: &str,
    size: u32,
) -> Result<(), StorageError> {
    let mut storage = storage.lock().await;
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

pub async fn clear_staged(storage: &SharedStorage) -> Result<(), StorageError> {
    save_staged(storage, Stage::None, "", "", "", 0).await
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
async fn current_ota_image(storage: &SharedStorage) -> BootImage {
    let mut storage = storage.lock().await;
    let Some(entries) = read_otadata_locked(&mut storage) else {
        return BootImage::Other;
    };
    let is = |wanted: u32| {
        entries.iter().any(|raw| matches!(boot_core::decode(raw), Decoded::Ok(e) if e.state == wanted))
    };
    if is(boot_core::state::PENDING_VERIFY) {
        BootImage::PendingVerify
    } else if is(boot_core::state::VALID) {
        BootImage::Valid
    } else {
        BootImage::Other
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

/// Flash sector size, and the unit [`WriteSession`] buffers before ever
/// touching flash -- see its own doc comment.
const OTA_SECTOR: u32 = 0x1000;

/// In-RAM write session (see the module doc comment for why this doesn't
/// need to survive a reboot). One at a time, matching `firmware-c`'s own
/// single static session -- this device only ever serves one HTTP
/// connection at a time anyway.
///
/// Buffers one flash sector in RAM and only ever erases/programs it once
/// full (or once, partially, at [`write_finish`]) -- not once per incoming
/// HTTP chunk. `esp-storage`'s `embedded_storage::Storage::write` (the
/// convenience trait the previous version of this session used) does a
/// read-modify-erase-rewrite of the *whole* sector on every call that isn't
/// itself sector-aligned; with a 1 KiB HTTP read buffer, writing one 4 KiB
/// sector meant up to four full erase+reprogram cycles of that same sector
/// instead of one.
struct WriteSession {
    slot: AppPartitionSubType,
    /// Absolute flash offset of the target partition, resolved once in
    /// [`write_begin`] ([`WriteTarget`]) and cached for the whole session --
    /// no partition-table re-parse per chunk. Nothing beyond `session.flushed
    /// <= params.total <= target.size` (checked once, at `write_begin`) is
    /// needed to stay inside the partition: no need to also carry its size.
    partition_offset: u32,
    /// One sector, filled from `sector_filled` on each chunk until full.
    /// Heap-allocated (`Box`): 4 KiB is too large to risk on an embassy
    /// task's stack (`#![deny(clippy::large_stack_frames)]`), and this way
    /// nothing changes if `OTA_SECTOR` ever grows.
    sector: Box<[u8; OTA_SECTOR as usize]>,
    /// Bytes of `sector` filled so far (buffered, not yet on flash).
    sector_filled: u32,
    /// Bytes *durably* written to flash -- what `write_written`/resume/the
    /// `partial` response report, distinct from bytes merely accepted into
    /// the session. `flushed + sector_filled` is every byte accepted so far.
    flushed: u32,
    /// Hashed exactly up to `flushed`, never ahead of it: hashing happens
    /// at flush time, not at receive time, specifically so a resume after a
    /// dropped connection -- which re-sends from `flushed`, the only bytes
    /// actually on flash -- never needs to un-hash bytes the (streaming,
    /// one-way) hasher already consumed but that never reached flash.
    hasher: Sha256,
    /// When `write_begin` opened this session -- purely diagnostic, logged
    /// by `write_finish` (contrat §4's own `written`/digest reply carries
    /// no timing field).
    started_at: Instant,
    /// How many sectors have been erased+programmed so far -- purely
    /// diagnostic, alongside `started_at`.
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

/// Bytes durably on flash -- see [`WriteSession::flushed`]'s doc comment.
/// This is what the JSON `written` field reports to the client: the point
/// it's safe to resume *after a dropped connection* from.
pub async fn write_written() -> u32 {
    WRITE_SESSION.lock().await.as_ref().map_or(0, |s| s.flushed)
}

/// Bytes accepted into the session so far -- `flushed` plus whatever's
/// buffered in the current, not-yet-full sector. Distinct from
/// `write_written` and used only for `write_plan`'s Continue-vs-Resync
/// decision: consecutive chunks of one *uninterrupted* PUT sequence declare
/// their `start` as "how much I've sent so far", which -- unless the
/// connection actually dropped -- is this, not `write_written` (which lags
/// behind it by up to one sector). Conflating the two would spuriously
/// 416 a live transfer whose chunk size doesn't happen to be a multiple of
/// the flash sector size.
pub async fn write_received() -> u32 {
    WRITE_SESSION.lock().await.as_ref().map_or(0, |s| s.flushed + s.sector_filled)
}

/// `PUT /v1alpha1/ota/write`'s resume decision (contrat §4's
/// `Content-Range` protocol) and `Content-Range` header parsing live in
/// `ota-logic` (workspace crate, `crates/ota-logic`) instead of here --
/// pure enough to unit-test with a plain `cargo test`, no ESP32 hardware
/// involved. Re-exported so callers keep writing `ota::Plan`/
/// `ota::write_plan` as if it were still defined in this module.
pub use ota_logic::{Plan, is_valid_digest, parse_content_range, range_len, write_is_final, write_plan};

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
    *WRITE_SESSION.lock().await = Some(WriteSession {
        slot: target.slot,
        partition_offset: target.offset,
        sector: Box::new([0u8; OTA_SECTOR as usize]),
        sector_filled: 0,
        flushed: 0,
        hasher: Sha256::new(),
        started_at: Instant::now(),
        sectors_flushed: 0,
        params,
    });
    Ok(())
}

/// Erases and programs the sector at `sector_index` (0-based within the
/// partition) with `session.sector[..len]`, `storage` already locked. One
/// erase, one program: the whole point of buffering a sector before ever
/// calling this.
/// Flashes `session.sector[..len]` (`len` is the number of *real* image
/// bytes in it) at `sector_index`, padded to the flash word size.
///
/// `NorFlash::write` (unlike the auto-RMW `embedded_storage::Storage` trait
/// this replaces) requires both the offset and the length to be a multiple
/// of the flash word size (4 bytes here) -- always true for a full sector
/// (`OTA_SECTOR` is 4096), but the final, partial sector at `write_finish`
/// rarely lands on a word boundary.
///
/// Invariant this keeps: padding is a hardware-write-granularity detail,
/// never part of the OTA payload. Concretely, for a logical image of `N`
/// bytes (`session.params.total`):
///
/// ```text
/// flash program length = align_up(N, 4)   (this function, at most +3 bytes)
/// digest                covers exactly [0, N)      -- hasher.update gets `len`, never `padded`
/// write_received/write_written are bounded by N    -- `flushed`/`received` advance by `len`, never `padded`
/// align_up(N, 4) <= partition_size                 -- checked below; total <= target.size at write_begin,
///                                                      and partition sizes are themselves sector-aligned
/// ```
///
/// So no *logical* OTA byte ever crosses `total`; only the NOR adapter may
/// program the minimal hardware-required alignment padding, and that
/// padding is explicitly zeroed (never leftover, indeterminate buffer
/// content) before it's written.
fn flush_sector(storage: &mut Storage, session: &mut WriteSession, sector_index: u32, len: u32) -> bool {
    let base = session.partition_offset + sector_index * OTA_SECTOR;
    let padded = (len as usize).div_ceil(4) * 4;
    debug_assert!(padded <= OTA_SECTOR as usize, "padding must never cross the sector it belongs to");
    session.sector[len as usize..padded].fill(0);
    storage
        .with_raw_flash(|flash| -> Option<()> {
            NorFlash::erase(flash, base, base + OTA_SECTOR).ok()?;
            NorFlash::write(flash, base, &session.sector[..padded]).ok()
        })
        .flatten()
        .is_some()
}

/// Appends `data` to the session, flushing whole sectors to flash as they
/// fill (see [`WriteSession`]'s doc comment). Handles `data` of any length,
/// not just the HTTP handler's own read-buffer size -- it may span several
/// sectors in one call.
pub async fn write_chunk(storage: &SharedStorage, mut data: &[u8]) -> bool {
    let mut session_guard = WRITE_SESSION.lock().await;
    let Some(session) = session_guard.as_mut() else {
        return false;
    };

    // Never accept more than the size the session declared.
    let received = session.flushed + session.sector_filled;
    if u32::try_from(data.len()).ok().and_then(|len| received.checked_add(len)).is_none_or(|end| end > session.params.total)
    {
        return false;
    }

    let mut storage = storage.lock().await;
    while !data.is_empty() {
        let space = (OTA_SECTOR - session.sector_filled) as usize;
        let take = space.min(data.len());
        session.sector[session.sector_filled as usize..session.sector_filled as usize + take]
            .copy_from_slice(&data[..take]);
        session.sector_filled += take as u32;
        data = &data[take..];

        if session.sector_filled == OTA_SECTOR {
            let sector_index = session.flushed / OTA_SECTOR;
            if !flush_sector(&mut storage, session, sector_index, OTA_SECTOR) {
                return false;
            }
            session.hasher.update(&session.sector[..OTA_SECTOR as usize]);
            session.flushed += OTA_SECTOR;
            session.sector_filled = 0;
            session.sectors_flushed += 1;
        }
    }
    true
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
    let Some(mut session) = WRITE_SESSION.lock().await.take() else {
        return Err(WriteFinishError::NotWriting);
    };

    // The image's own size rarely lands on a sector boundary: flush
    // whatever's left buffered (a final, partial sector) before checking
    // completeness (see `flush_sector`'s own comment on the padding this
    // needs).
    if session.sector_filled > 0 {
        let mut storage_guard = storage.lock().await;
        let sector_index = session.flushed / OTA_SECTOR;
        let len = session.sector_filled;
        if !flush_sector(&mut storage_guard, &mut session, sector_index, len) {
            return Err(WriteFinishError::Incomplete);
        }
        drop(storage_guard);
        session.hasher.update(&session.sector[..len as usize]);
        session.flushed += len;
        session.sector_filled = 0;
        session.sectors_flushed += 1;
    }

    if session.flushed != session.params.total {
        warn!("ota: session ended at {} of {} octets", session.flushed, session.params.total);
        return Err(WriteFinishError::Incomplete);
    }

    let digest_bytes = session.hasher.finalize();
    let mut digest = String::from("sha256:");
    for b in digest_bytes {
        let _ = write!(digest, "{b:02x}");
    }

    if !digest.eq_ignore_ascii_case(&session.params.digest) {
        warn!("ota: digest mismatch, attendu={} calculé={digest}", session.params.digest);
        return Err(WriteFinishError::DigestMismatch);
    }

    save_staged(
        storage,
        Stage::Written,
        slot_name(session.slot),
        &digest,
        &session.params.deployment_id,
        session.flushed,
    )
    .await
    .map_err(WriteFinishError::Storage)?;
    let elapsed = session.started_at.elapsed();
    info!(
        "ota: write OK {} octets ({} secteurs erase+program) en {}ms slot={} -> staged=written",
        session.flushed,
        session.sectors_flushed,
        elapsed.as_millis(),
        slot_name(session.slot)
    );
    Ok(WriteFinishOk { written: session.flushed, digest })
}

/// `POST /v1alpha1/ota/activate` (contrat §4): points the bootloader at the
/// staged slot and arms `OtaImageState::New` (which it promotes to
/// `PendingVerify` on the next boot). Reads the target slot from the NVS
/// `staged` state, not the in-RAM write session -- matches `firmware-c`'s
/// own fallback ("Reprise après reboot de l'agent entre write et
/// activate"), and works identically whether or not this device rebooted
/// since `/ota/write` finished.
pub async fn activate(storage: &SharedStorage, deployment_id: &str) -> Result<&'static str, ActivateError> {
    let staged = staged(storage).await;
    if staged.stage != Stage::Written {
        return Err(ActivateError::NotStaged);
    }
    // Activate exactly what was staged: the request names a deployment, it
    // doesn't get to rename the staged one.
    if staged.deployment_id != deployment_id {
        return Err(ActivateError::DeploymentMismatch);
    }
    let target = slot_from_name(&staged.slot).ok_or(ActivateError::NotStaged)?;

    // Record the intent first: if NVS refuses it, nothing has changed yet
    // and the caller gets an error instead of a reboot into a slot whose
    // staged record disagrees with `otadata`.
    save_staged(storage, Stage::Activating, &staged.slot, &staged.digest, &staged.deployment_id, staged.size)
        .await
        .map_err(ActivateError::Storage)?;

    let ok = otadata_activate(storage, target).await.is_ok();
    if !ok {
        // Best effort: back to `Written` so a retry of `activate` is possible.
        if save_staged(storage, Stage::Written, &staged.slot, &staged.digest, &staged.deployment_id, staged.size)
            .await
            .is_err()
        {
            warn!("ota: activate failed and the staged record couldn't be restored to `written`");
        }
        return Err(ActivateError::NotStaged);
    }

    info!("ota: activate dep={deployment_id} -> slot={} prêt, reboot imminent", staged.slot);
    Ok(slot_name(target))
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
/// next boot ([`BootAction::FinishInterruptedValidation`]).
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
/// du projet"). The decision itself is [`ota_logic::boot_action`], a pure
/// table unit-tested on the host; this only gathers its inputs and applies
/// the outcome. This is the only place `agent::State` is driven from
/// `Booting`.
pub async fn on_boot(storage: &'static SharedStorage, spawner: Spawner) {
    let staged = staged(storage).await;
    let image = current_ota_image(storage).await;
    let booted = active_slot(storage).await;
    let booted_is_staged = (!booted.is_empty()).then(|| booted == staged.slot);
    let kind = match staged.stage {
        Stage::None => StagedKind::None,
        Stage::Written => StagedKind::Written,
        Stage::Activating => StagedKind::Activating,
    };
    let action = boot_action(kind, image, booted_is_staged);
    info!("ota: boot slot={booted:?} staged={} image={image:?} -> {action:?}", staged.stage.as_str());

    match action {
        BootAction::SelfCheck => {
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
        BootAction::RollbackUnaccounted => {
            warn!("ota: PENDING_VERIFY image not accounted for by the staged record, rolling back");
            agent::set_state(agent::State::Rollback);
            mark_invalid_and_reboot(storage).await;
        }
        BootAction::Nothing | BootAction::KeepWritten => {}
        BootAction::ClearStale => {
            warn!("ota: stale staged record ({}), clearing", staged.stage.as_str());
            if clear_staged(storage).await.is_err() {
                warn!("ota: stale staged record couldn't be cleared");
            }
        }
        BootAction::FinishInterruptedValidation => {
            warn!("ota: finishing a validation interrupted before its bookkeeping");
            // Runs the same path as a live validation; `Degraded` (set by
            // it on failure) must not be overwritten below.
            finish_validation(storage, &staged).await;
            if agent::state() == agent::State::Degraded {
                return;
            }
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
