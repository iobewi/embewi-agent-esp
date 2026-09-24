//! Device lifecycle state persisted outside component configuration.
//!
//! This state is not part of ConfigManager: it controls whether the one-shot
//! provisioning surface is still available and therefore belongs to device
//! lifecycle, not to a configurable component schema.

use esp_storage_manager::Key;

use crate::storage::{SharedStorage, StorageError};

const NAMESPACE: Key = Key::from_str("system");
const KEY_LOCKED: Key = Key::from_str("locked");

pub async fn is_locked(storage: &SharedStorage) -> bool {
    storage
        .lock()
        .await
        .get_bool(&NAMESPACE, &KEY_LOCKED)
        .unwrap_or(false)
}

pub async fn lock(storage: &SharedStorage) -> Result<(), StorageError> {
    storage
        .lock()
        .await
        .set_bool(&NAMESPACE, &KEY_LOCKED, true)
}
