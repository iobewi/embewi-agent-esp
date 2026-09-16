//! Embewi contract v1alpha1 -- inbound API surface (Core -> ESP), §4.
//!
//! Only `GET /v1alpha1/info` is implemented so far. Fields that depend on
//! subsystems not built yet (OTA A/B -- `staged`, `active_slot`,
//! `firmware.digest`; McuConfigMap -- `config_generation`) are honest
//! placeholders, not guesses: `staged.state` really is `"none"` because no
//! OTA write has ever happened, `config_generation` really is `0` because
//! nothing has ever bumped it. They'll become real once those phases land.

use alloc::format;
use alloc::string::String;
use core::convert::Infallible;
use core::fmt::Write as _;

use esp_nvs::Key;
use picoserve::extract::FromRequestParts;
use picoserve::request::RequestParts;
use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::storage::SharedStorage;

/// Versions of the `/v1alpha1`-style protocol this agent answers, highest
/// first (contrat §4, "Découverte de version d'API").
pub const API_VERSIONS: &[&str] = &["v1alpha1"];
pub const FW_NAME: &str = "embewi-agent-esp";
pub const FW_VERSION: &str = env!("CARGO_PKG_VERSION");

const NAMESPACE: Key = Key::from_str("agent");
const KEY_NODE_ID: Key = Key::from_str("node_id");
const KEY_CTRL_URL: Key = Key::from_str("ctrl_url");
const KEY_TOKEN: Key = Key::from_str("token");

/// The device's `node_id` (contrat §1a): the NVS value if provisioned, else
/// a temporary MAC-derived ID (`embewi-AABBCC`) -- explicitly *not* a stable
/// identity, callers shouldn't persist it Core-side as a long-term key.
pub async fn node_id(storage: &SharedStorage) -> String {
    if let Some(id) = storage.lock().await.get_string(&NAMESPACE, &KEY_NODE_ID) {
        return id;
    }
    let mac = esp_hal::efuse::base_mac_address();
    let mac = mac.as_bytes();
    format!("embewi-{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5])
}

/// The Kubernetes controller URL (contrat §1a), empty if not yet
/// provisioned. Outbound flows (heartbeat/logs, once built) should treat an
/// empty `ctrl_url` as "nothing to talk to yet" and stay quiet, same as the
/// reference implementation.
pub async fn ctrl_url(storage: &SharedStorage) -> String {
    storage
        .lock()
        .await
        .get_string(&NAMESPACE, &KEY_CTRL_URL)
        .unwrap_or_default()
}

/// The current Bearer token, empty if none has been provisioned yet.
/// Deliberately readable, not just comparable: `http/mod.rs`'s save flow
/// shows it once, on the confirmation page served right before the device
/// locks and reboots.
pub async fn token(storage: &SharedStorage) -> String {
    storage
        .lock()
        .await
        .get_string(&NAMESPACE, &KEY_TOKEN)
        .unwrap_or_default()
}

/// 128-bit random token, hex-encoded (contrat §1a: "token vide → généré
/// aléatoirement par le device"). True randomness needs the RF subsystem up
/// (Wi-Fi) -- always the case here, since this is only ever called from the
/// HTTP config page, itself only reachable once on Wi-Fi.
fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    esp_hal::rng::Rng::new().read(&mut bytes);
    let mut token = String::with_capacity(32);
    for b in bytes {
        let _ = write!(token, "{b:02x}");
    }
    token
}

/// Saves `node_id`/`ctrl_url`. `presented_token` empty means "keep the
/// existing token, or generate a fresh one if there isn't one yet" (contrat
/// §1a) -- it never clears an existing token, matching `POST /token`'s own
/// refusal of an empty value (§4: "on ne désactive pas l'auth par
/// rotation"). `http/mod.rs`'s save flow always calls this with an empty
/// `presented_token` (the form has no token field at all -- it's a
/// one-shot save, there's nothing to rotate to yet), so in practice this
/// only ever generates on first provisioning.
pub async fn save_identity(storage: &SharedStorage, node_id: &str, ctrl_url: &str, presented_token: &str) {
    let mut storage = storage.lock().await;
    storage.set_string(&NAMESPACE, &KEY_NODE_ID, node_id);
    storage.set_string(&NAMESPACE, &KEY_CTRL_URL, ctrl_url);

    if !presented_token.is_empty() {
        storage.set_string(&NAMESPACE, &KEY_TOKEN, presented_token);
    } else if storage.get_string(&NAMESPACE, &KEY_TOKEN).is_none() {
        let token = generate_token();
        storage.set_string(&NAMESPACE, &KEY_TOKEN, &token);
    }
}

/// Extracts the raw Bearer token from the `Authorization` header, if any
/// (every inbound endpoint requires `Authorization: Bearer <token>`,
/// contrat §4). Never fails to extract: an absent/malformed header just
/// yields `None`, and [`is_authorized`] correctly refuses that.
pub struct Bearer(pub Option<String>);

impl<'r, State> FromRequestParts<'r, State> for Bearer {
    type Rejection = Infallible;

    async fn from_request_parts(
        _state: &'r State,
        request_parts: &RequestParts<'r>,
    ) -> Result<Self, Self::Rejection> {
        let token = request_parts
            .headers()
            .get("authorization")
            .and_then(|value| value.as_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(String::from);
        Ok(Bearer(token))
    }
}

/// Whether `presented` matches the stored Bearer token, compared in
/// constant time (contrat §1: "pas de fuite du token octet par octet").
/// `false` -- refusing every inbound call -- when no token has been
/// provisioned yet (contrat §1a).
pub async fn is_authorized(storage: &SharedStorage, presented: &str) -> bool {
    let Some(token) = storage.lock().await.get_string(&NAMESPACE, &KEY_TOKEN) else {
        return false;
    };
    token.as_bytes().ct_eq(presented.as_bytes()).into()
}

#[derive(Serialize)]
struct Firmware {
    name: &'static str,
    version: &'static str,
    /// Real once OTA A/B (§3/§4) computes it from the running partition.
    digest: &'static str,
}

#[derive(Serialize)]
struct Staged {
    state: &'static str,
}

/// `GET /v1alpha1/info` response body (contrat §4).
#[derive(Serialize)]
pub struct Info {
    node_id: String,
    api_versions: &'static [&'static str],
    chip: &'static str,
    firmware: Firmware,
    staged: Staged,
    state: &'static str,
    config_generation: u32,
    app_port: u16,
}

pub async fn info(storage: &SharedStorage) -> Info {
    Info {
        node_id: node_id(storage).await,
        api_versions: API_VERSIONS,
        chip: esp_metadata_generated::chip_pretty!(),
        firmware: Firmware { name: FW_NAME, version: FW_VERSION, digest: "" },
        // No OTA subsystem yet (roadmap: after WebSocket) -- always "none"
        // until a write actually lands on the inactive slot.
        staged: Staged { state: "none" },
        state: "running",
        // No McuConfigMap subsystem yet -- never bumped, so genuinely 0.
        config_generation: 0,
        // This HTTP server *is* the app service for now (single binary,
        // no separate workload process) -- 80, matching http/mod.rs.
        app_port: 80,
    }
}
