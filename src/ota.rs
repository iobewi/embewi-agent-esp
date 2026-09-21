//! OTA A/B updates (contrat §3/§4/§6). The Core streams a raw `.bin` into
//! whichever `ota_0`/`ota_1` slot isn't currently booted; the bootloader
//! (`esp-bootloader-esp-idf`) does the real rollback -- this module only
//! ever *asks* it to switch slots, it never re-implements that decision.
//!
//! Mirrors `firmware-c`'s `embewi_ota.c`/`embewi_selfcheck.c` state machine
//! (same `stage`/`slot`/`digest`/`deployment_id`/`size` staged-NVS layout,
//! same `embewi_ota_plan`/`embewi_ota_is_final` pure resume logic for
//! `Content-Range`), reimplemented against `esp-bootloader-esp-idf`'s Rust
//! API and embassy tasks instead of ESP-IDF's C one and FreeRTOS tasks.
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
use embassy_time::{Duration, Timer};
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{AppPartitionSubType, PARTITION_TABLE_MAX_LEN};
use esp_nvs::Key;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent;
use crate::storage::{SharedStorage, StorageError};

/// Contrat §4: `POST /ota/prepare`'s `partition_layout` field must match
/// this exactly, or the write is refused before a single byte transfers.
/// Bump only if `partitions.csv`'s slot layout ever changes shape.
pub const PARTITION_LAYOUT: &str = "embewi-ab-v1";
/// Contrat §3: how long a `pending_verify` self-check gets before this
/// device forces its own reset -- unconfirmed past this, the bootloader's
/// own rollback takes over on the next boot. Same value `firmware-c` uses
/// (`EMBEWI_PENDING_DEADLINE_MS`).
const SELFCHECK_DEADLINE: Duration = Duration::from_secs(15);

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

async fn current_ota_state(storage: &SharedStorage) -> Option<OtaImageState> {
    let mut storage = storage.lock().await;
    storage
        .with_raw_flash(|flash| {
            let mut buffer = table_buffer();
            let mut updater = OtaUpdater::new(flash, &mut buffer).ok()?;
            let mut ota_data = updater.ota_data().ok()?;
            ota_data.current_ota_state().ok()
        })
        .flatten()
}

async fn set_current_ota_state(storage: &SharedStorage, state: OtaImageState) -> bool {
    let mut storage = storage.lock().await;
    storage
        .with_raw_flash(|flash| {
            let mut buffer = table_buffer();
            let Ok(mut updater) = OtaUpdater::new(flash, &mut buffer) else {
                return false;
            };
            let Ok(mut ota_data) = updater.ota_data() else {
                return false;
            };
            ota_data.set_current_ota_state(state).is_ok()
        })
        .unwrap_or(false)
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
    storage
        .with_raw_flash(|flash| {
            let mut buffer = table_buffer();
            let Ok(mut updater) = OtaUpdater::new(flash, &mut buffer) else {
                return refuse("busy");
            };
            let Ok((region, slot)) = updater.next_partition() else {
                return refuse("busy");
            };
            if req.size as usize > region.partition_size() {
                return refuse("size_too_large");
            }
            PrepareResponse { accepted: true, target_slot: Some(slot_name(slot)), reason: None }
        })
        .unwrap_or_else(|| refuse("busy"))
}

/// In-RAM write session (see the module doc comment for why this doesn't
/// need to survive a reboot). One at a time, matching `firmware-c`'s own
/// single static session -- this device only ever serves one HTTP
/// connection at a time anyway.
struct WriteSession {
    slot: AppPartitionSubType,
    written: u32,
    hasher: Sha256,
}

static WRITE_SESSION: Mutex<CriticalSectionRawMutex, Option<WriteSession>> = Mutex::new(None);

pub async fn write_in_progress() -> bool {
    WRITE_SESSION.lock().await.is_some()
}

pub async fn write_written() -> u32 {
    WRITE_SESSION.lock().await.as_ref().map_or(0, |s| s.written)
}

/// `PUT /v1alpha1/ota/write`'s resume decision (contrat §4's
/// `Content-Range` protocol) and `Content-Range` header parsing live in
/// `ota-logic` (workspace crate, `crates/ota-logic`) instead of here --
/// pure enough to unit-test with a plain `cargo test`, no ESP32 hardware
/// involved. Re-exported so callers keep writing `ota::Plan`/
/// `ota::write_plan` as if it were still defined in this module.
pub use ota_logic::{Plan, parse_content_range, write_is_final, write_plan};

/// Starts (or restarts) a write session against whichever slot the
/// bootloader would currently hand out next. Always re-derived fresh here
/// rather than cached from `/ota/prepare`: `firmware-c`'s `write_begin`
/// does the same (see its own comment for why) -- prepare is a compat
/// pre-check, not a reservation.
pub async fn write_begin(storage: &SharedStorage) -> bool {
    let slot = {
        let mut storage = storage.lock().await;
        let found = storage.with_raw_flash(|flash| {
            let mut buffer = table_buffer();
            let mut updater = OtaUpdater::new(flash, &mut buffer).ok()?;
            let (_, slot) = updater.next_partition().ok()?;
            Some(slot)
        });
        let Some(Some(slot)) = found else {
            return false;
        };
        slot
    };
    *WRITE_SESSION.lock().await = Some(WriteSession { slot, written: 0, hasher: Sha256::new() });
    true
}

pub async fn write_chunk(storage: &SharedStorage, data: &[u8]) -> bool {
    let mut session_guard = WRITE_SESSION.lock().await;
    let Some(session) = session_guard.as_mut() else {
        return false;
    };

    let mut storage = storage.lock().await;
    let written = session.written;
    let expected_slot = session.slot;
    let wrote = storage
        .with_raw_flash(|flash| {
            let mut buffer = table_buffer();
            let Ok(mut updater) = OtaUpdater::new(flash, &mut buffer) else {
                return false;
            };
            let Ok((mut region, slot)) = updater.next_partition() else {
                return false;
            };
            if slot != expected_slot {
                // The booted/next-update partition changed under us mid-session --
                // shouldn't happen (nothing else calls `activate_next_partition`
                // while a write is in flight), but writing to the wrong slot would
                // silently corrupt it, so refuse rather than guess.
                return false;
            }
            embedded_storage::Storage::write(&mut region, written, data).is_ok()
        })
        .unwrap_or(false);
    if !wrote {
        return false;
    }
    session.hasher.update(data);
    session.written += data.len() as u32;
    true
}

pub struct WriteFinishOk {
    pub written: u32,
    pub digest: String,
}

pub enum WriteFinishError {
    NotWriting,
    DigestMismatch,
    /// The image was written and verified, but the staged record couldn't
    /// be persisted -- it must not be reported as `written`.
    Storage(StorageError),
}

/// Closes the write session: compares the digest computed *while writing*
/// (never a post-hoc flash re-read, per contrat §4) against what the Core
/// expects, and on a match persists the staged state (contrat §6).
pub async fn write_finish(
    storage: &SharedStorage,
    expected_digest: &str,
    deployment_id: &str,
) -> Result<WriteFinishOk, WriteFinishError> {
    let Some(session) = WRITE_SESSION.lock().await.take() else {
        return Err(WriteFinishError::NotWriting);
    };

    let digest_bytes = session.hasher.finalize();
    let mut digest = String::from("sha256:");
    for b in digest_bytes {
        let _ = write!(digest, "{b:02x}");
    }

    if !expected_digest.is_empty() && !digest.eq_ignore_ascii_case(expected_digest) {
        warn!("ota: digest mismatch, attendu={expected_digest} calculé={digest}");
        return Err(WriteFinishError::DigestMismatch);
    }

    save_staged(storage, Stage::Written, slot_name(session.slot), &digest, deployment_id, session.written)
        .await
        .map_err(WriteFinishError::Storage)?;
    info!("ota: write OK {} octets slot={} -> staged=written", session.written, slot_name(session.slot));
    Ok(WriteFinishOk { written: session.written, digest })
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
    let target = slot_from_name(&staged.slot).ok_or(ActivateError::NotStaged)?;

    // Record the intent first: if NVS refuses it, nothing has changed yet
    // and the caller gets an error instead of a reboot into a slot whose
    // staged record disagrees with `otadata`.
    save_staged(storage, Stage::Activating, &staged.slot, &staged.digest, deployment_id, staged.size)
        .await
        .map_err(ActivateError::Storage)?;

    let ok = {
        let mut storage = storage.lock().await;
        storage
            .with_raw_flash(|flash| {
                let mut buffer = table_buffer();
                let mut updater = OtaUpdater::new(flash, &mut buffer).ok()?;
                let mut ota_data = updater.ota_data().ok()?;
                Some(
                    ota_data.set_current_app_partition(target).is_ok()
                        && ota_data.set_current_ota_state(OtaImageState::New).is_ok(),
                )
            })
            .flatten()
            .unwrap_or(false)
    };
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
    /// The staged record couldn't be persisted; nothing was activated.
    Storage(StorageError),
}

/// Promotes the just-validated staged image to "active" and cancels the
/// bootloader's pending rollback. Only ever called after every self-check
/// passes (contrat §3: "mark_valid n'est appelé QUE si tous les checks
/// passent").
async fn mark_valid(storage: &'static SharedStorage) {
    let staged = staged(storage).await;
    if !set_current_ota_state(storage, OtaImageState::Valid).await {
        // Couldn't even record validation -- don't claim `running` over an
        // image the bootloader doesn't agree is confirmed.
        mark_invalid_and_reboot(storage).await;
    }

    // The bootloader already considers the image valid at this point, so a
    // failed bookkeeping write can't be turned into a rollback -- it's
    // logged loudly instead of being silently lost.
    {
        let mut storage = storage.lock().await;
        if storage.set_string(&NAMESPACE, &KEY_ACTIVE_DIGEST, &staged.digest).is_err()
            || storage.set_string(&NAMESPACE, &KEY_ACTIVE_DEPLOYMENT_ID, &staged.deployment_id).is_err()
        {
            warn!("ota: validated image's digest/deployment_id couldn't be persisted");
        }
    }
    if clear_staged(storage).await.is_err() {
        warn!("ota: staged record couldn't be cleared after mark_valid");
    }
    agent::set_state(agent::State::Running);
    info!("ota: self-check OK, mark_valid done (deployment_id={})", staged.deployment_id);
}

/// The rollback path (contrat §3): marks the image invalid and resets.
/// Never returns -- on reboot, the bootloader sees `Invalid` (or a stuck
/// `PendingVerify`, if even this much couldn't complete) and falls back to
/// the previous slot on its own; this agent doesn't drive that part.
async fn mark_invalid_and_reboot(storage: &'static SharedStorage) -> ! {
    let _ = set_current_ota_state(storage, OtaImageState::Invalid).await;
    warn!("ota: self-check failed, marking image invalid and rebooting for rollback");
    // Gives the log line above time to actually reach the WebSocket log
    // stream/serial console before the reset cuts it off.
    Timer::after(Duration::from_millis(200)).await;
    esp_hal::system::software_reset();
}

#[embassy_executor::task]
async fn selfcheck_task(storage: &'static SharedStorage) {
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
        Either::First(true) => mark_valid(storage).await,
        Either::First(false) => mark_invalid_and_reboot(storage).await,
        Either::Second(()) => {
            warn!("ota: self-check deadline exceeded, forcing a reset (bootloader will roll back)");
            esp_hal::system::software_reset();
        }
    }
}

/// Called once at boot (`src/bin/main.rs`): detects whether the image that
/// just booted is unconfirmed (contrat §3's "cœur dur du projet") and, if
/// so, starts the bounded self-check that will either validate it or roll
/// it back. This is the only place `agent::State` is driven from `Booting`.
pub async fn on_boot(storage: &'static SharedStorage, spawner: Spawner) {
    if current_ota_state(storage).await == Some(OtaImageState::PendingVerify) {
        agent::set_state(agent::State::PendingVerify);
        warn!("ota: image is PENDING_VERIFY, starting bounded self-check (deadline {SELFCHECK_DEADLINE:?})");
        if let Ok(token) = selfcheck_task(storage) {
            spawner.spawn(token);
        }
    } else {
        agent::set_state(agent::State::Running);
        // A `written`/`activating` entry surviving from an interrupted
        // cycle (e.g. this device power-cycled before the bootloader ever
        // flipped the image to `PendingVerify`) no longer describes
        // anything real -- clear it so `GET /info` doesn't report a slot
        // that isn't actually staged for anything anymore.
        if clear_staged(storage).await.is_err() {
            warn!("ota: stale staged record couldn't be cleared at boot");
        }
    }
}
