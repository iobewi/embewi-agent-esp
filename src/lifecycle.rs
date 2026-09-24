//! Persistent device lifecycle state.
//!
//! Lifecycle is an application-owned object, persisted through its dedicated
//! ConfigSpace. The component knows its schema; neither callers nor this module
//! address NVS namespaces/keys directly.

use config_space_manager::{Budget, ConfigSpace};

use crate::config::NvsConfigBackend;

const MAGIC: &[u8; 4] = b"LFC1";
const ENCODED_LEN: usize = 5;

/// The complete lifecycle object is four magic bytes plus one flags byte.
pub const CONFIG_BUDGET: Budget = Budget::new(ENCODED_LEN);
pub type LifecycleConfigSpace = ConfigSpace<NvsConfigBackend>;

#[derive(Debug)]
pub enum LifecycleError {
    Persistence,
}

fn decode(raw: &[u8]) -> Option<bool> {
    if raw.len() != ENCODED_LEN || &raw[..4] != MAGIC || raw[4] & !1 != 0 {
        return None;
    }
    Some(raw[4] & 1 != 0)
}

/// Whether one-shot provisioning has been permanently locked.
///
/// Missing state means a fresh device and is therefore unlocked. Persistence
/// failure or corrupt state fails closed: a storage fault must never reopen
/// provisioning on an already configured device.
pub async fn is_locked(space: &LifecycleConfigSpace) -> bool {
    match space.load().await {
        Ok(None) => false,
        Ok(Some(snapshot)) => decode(&snapshot.data).unwrap_or(true),
        Err(_) => true,
    }
}

/// Permanently locks one-shot provisioning.
pub async fn lock(space: &LifecycleConfigSpace) -> Result<(), LifecycleError> {
    let mut encoded = [0u8; ENCODED_LEN];
    encoded[..4].copy_from_slice(MAGIC);
    encoded[4] = 1;
    space
        .commit(&encoded)
        .await
        .map(|_| ())
        .map_err(|_| LifecycleError::Persistence)
}
