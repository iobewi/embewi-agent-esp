//! Application-service persistent configuration.
//!
//! This is distinct from agent identity and from lifecycle/OTA state. The
//! current schema only owns the business service TCP port.

use config_space_manager::{Budget, ConfigSpace};
use esp_storage_manager::Key;
use log::{info, warn};

use crate::config::NvsConfigBackend;
use crate::storage::SharedStorage;

const LEGACY_NAMESPACE: Key = Key::from_str("system");
const LEGACY_KEY_APP_PORT: Key = Key::from_str("app_port");

const MAGIC: &[u8; 4] = b"APC1";
pub const DEFAULT_PORT: u16 = 8080;

pub const CONFIG_BUDGET: Budget = Budget::new(8);
pub type AppConfigSpace = ConfigSpace<NvsConfigBackend>;

pub async fn port(space: &AppConfigSpace) -> u16 {
    let Ok(Some(snapshot)) = space.load().await else {
        return DEFAULT_PORT;
    };
    let raw = snapshot.data;
    if raw.len() != 6 || &raw[..4] != MAGIC {
        warn!("app: stored config generation={} has an unsupported/corrupt schema", snapshot.generation);
        return DEFAULT_PORT;
    }
    u16::from_le_bytes([raw[4], raw[5]])
}

pub async fn save_port(space: &AppConfigSpace, port: u16) -> Result<(), ()> {
    let mut encoded = [0u8; 6];
    encoded[..4].copy_from_slice(MAGIC);
    encoded[4..].copy_from_slice(&port.to_le_bytes());
    space.commit(&encoded).await.map_err(|_| ())?;
    Ok(())
}

pub async fn migrate_legacy_config(
    storage: &'static SharedStorage,
    space: &AppConfigSpace,
) {
    match space.load().await {
        Ok(Some(_)) => return,
        Err(e) => {
            warn!("app: config-space load failed before legacy migration: {e:?}");
            return;
        }
        Ok(None) => {}
    }

    let legacy = storage.lock().await.get_u16(&LEGACY_NAMESPACE, &LEGACY_KEY_APP_PORT);
    let Some(port) = legacy else {
        return;
    };

    if save_port(space, port).await.is_err() {
        warn!("app: legacy port migration failed");
        return;
    }

    let mut storage = storage.lock().await;
    if storage.delete(&LEGACY_NAMESPACE, &LEGACY_KEY_APP_PORT).is_err() {
        warn!("app: migrated config but could not remove legacy app port");
    }
    info!("app: migrated legacy app port to config space");
}
