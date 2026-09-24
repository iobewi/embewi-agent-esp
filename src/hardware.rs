//! Hardware-owned persistent configuration.
//!
//! The component owns the schema inside its ConfigSpace. Direct NVS access
//! exists only for the one-way migration from the pre-ConfigSpace layout.

use config_space_manager::{Budget, ConfigSpace};
use esp_storage_manager::Key;
use log::{info, warn};

use crate::config::NvsConfigBackend;
use crate::storage::SharedStorage;

const LEGACY_NAMESPACE: Key = Key::from_str("hw");
const LEGACY_KEY_LED_GPIO: Key = Key::from_str("led_gpio");

const MAGIC: &[u8; 4] = b"HWC1";
const NONE: u8 = 0xff;

pub const CONFIG_BUDGET: Budget = Budget::new(8);
pub type HardwareConfigSpace = ConfigSpace<NvsConfigBackend>;

pub async fn led_gpio(space: &HardwareConfigSpace) -> Option<u8> {
    let Ok(Some(snapshot)) = space.load().await else {
        return None;
    };
    let raw = snapshot.data;
    if raw.len() != 5 || &raw[..4] != MAGIC {
        warn!("hardware: stored config generation={} has an unsupported/corrupt schema", snapshot.generation);
        return None;
    }
    (raw[4] != NONE).then_some(raw[4])
}

pub async fn save_led_gpio(
    space: &HardwareConfigSpace,
    gpio: Option<u8>,
) -> Result<(), ()> {
    let mut encoded = [0u8; 5];
    encoded[..4].copy_from_slice(MAGIC);
    encoded[4] = gpio.unwrap_or(NONE);
    space.commit(&encoded).await.map_err(|_| ())?;
    Ok(())
}

pub async fn migrate_legacy_config(
    storage: &'static SharedStorage,
    space: &HardwareConfigSpace,
) {
    match space.load().await {
        Ok(Some(_)) => return,
        Err(e) => {
            warn!("hardware: config-space load failed before legacy migration: {e:?}");
            return;
        }
        Ok(None) => {}
    }

    let legacy = storage.lock().await.get_u8(&LEGACY_NAMESPACE, &LEGACY_KEY_LED_GPIO);
    let Some(gpio) = legacy else {
        return;
    };

    if save_led_gpio(space, Some(gpio)).await.is_err() {
        warn!("hardware: legacy LED GPIO migration failed");
        return;
    }

    let mut storage = storage.lock().await;
    if storage.delete(&LEGACY_NAMESPACE, &LEGACY_KEY_LED_GPIO).is_err() {
        warn!("hardware: migrated config but could not remove legacy LED GPIO");
    }
    info!("hardware: migrated legacy LED GPIO to config space");
}
