//! Embewi storage model layered on top of `esp-storage-manager`.
//!
//! This module owns application namespaces, keys and boot-snapshot semantics.
//! Physical flash ownership, cached NVS coordination and bounded raw-flash
//! access live in the reusable `esp-storage-manager` crate.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_hal::peripherals::FLASH;
use esp_storage_manager::{FlashStorage, Key, NvsPartition, StorageManager};

pub use esp_storage_manager::StorageError;

/// Outcome of [`Storage::cfg_set`] when NVS itself did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSetResult {
    Stored,
    Deleted,
    Rejected,
}

/// `Storage` shared between asynchronous agent tasks.
pub type SharedStorage = Mutex<CriticalSectionRawMutex, Storage>;

/// Default ESP-IDF NVS partition used by this firmware.
const PARTITION_OFFSET: usize = 0x9000;
const PARTITION_SIZE: usize = 0x6000;

const SYSTEM_NAMESPACE: Key = Key::from_str("system");
const KEY_LOCKED: Key = Key::from_str("locked");

const CFG_NAMESPACE: Key = Key::from_str("cfg");
const KEY_CFG_GENERATION: Key = Key::from_str("_gen");
/// `esp-nvs` keys are capped at 15 bytes.
const MAX_CFG_KEY_LEN: usize = 15;

pub struct Storage {
    backend: StorageManager,
    /// McuConfigMap snapshot taken once at boot. It intentionally does not
    /// change when live NVS values are updated later through the admin API.
    active_cfg: BTreeMap<String, String>,
    active_cfg_generation: u32,
}

impl Storage {
    pub fn new(flash: FLASH<'static>) -> Self {
        let mut storage = Self {
            backend: StorageManager::new(
                flash,
                NvsPartition::new(PARTITION_OFFSET, PARTITION_SIZE),
            ),
            active_cfg: BTreeMap::new(),
            active_cfg_generation: 0,
        };
        storage.active_cfg_generation = storage.cfg_generation();
        storage.active_cfg = storage.cfg_entries();
        storage
    }

    /// Bounded raw access used by the OTA adapter. The agent preserves the
    /// invariant that raw users only touch partitions outside the NVS range.
    pub fn with_raw_flash<R>(
        &mut self,
        f: impl FnOnce(&mut FlashStorage<'static>) -> R,
    ) -> Option<R> {
        Some(self.backend.with_raw_flash(f))
    }

    pub fn get_string(&mut self, namespace: &Key, key: &Key) -> Option<String> {
        self.backend.get_string(namespace, key)
    }

    /// Opaque blob access used by config-space-manager's NVS backend.
    /// Missing data is distinct from an NVS infrastructure failure.
    pub fn read_blob(
        &mut self,
        namespace: &Key,
        key: &Key,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.backend.read_blob(namespace, key)
    }

    pub fn set_blob(
        &mut self,
        namespace: &Key,
        key: &Key,
        value: &[u8],
    ) -> Result<(), StorageError> {
        self.backend.set_blob(namespace, key, value)
    }

    pub fn nvs_statistics(
        &mut self,
    ) -> Result<esp_storage_manager::NvsStatistics, StorageError> {
        self.backend.nvs_statistics()
    }

    pub fn set_string(
        &mut self,
        namespace: &Key,
        key: &Key,
        value: &str,
    ) -> Result<(), StorageError> {
        self.backend.set_string(namespace, key, value)
    }

    pub fn get_u8(&mut self, namespace: &Key, key: &Key) -> Option<u8> {
        self.backend.get_u8(namespace, key)
    }

    pub fn set_u8(&mut self, namespace: &Key, key: &Key, value: u8) -> Result<(), StorageError> {
        self.backend.set_u8(namespace, key, value)
    }

    pub fn get_bool(&mut self, namespace: &Key, key: &Key) -> Option<bool> {
        self.backend.get_bool(namespace, key)
    }

    pub fn set_bool(
        &mut self,
        namespace: &Key,
        key: &Key,
        value: bool,
    ) -> Result<(), StorageError> {
        self.backend.set_bool(namespace, key, value)
    }

    pub fn get_u16(&mut self, namespace: &Key, key: &Key) -> Option<u16> {
        self.backend.get_u16(namespace, key)
    }

    pub fn set_u16(&mut self, namespace: &Key, key: &Key, value: u16) -> Result<(), StorageError> {
        self.backend.set_u16(namespace, key, value)
    }

    pub fn get_u32(&mut self, namespace: &Key, key: &Key) -> Option<u32> {
        self.backend.get_u32(namespace, key)
    }

    pub fn set_u32(&mut self, namespace: &Key, key: &Key, value: u32) -> Result<(), StorageError> {
        self.backend.set_u32(namespace, key, value)
    }

    pub fn delete(&mut self, namespace: &Key, key: &Key) -> Result<(), StorageError> {
        self.backend.delete(namespace, key)
    }

    pub fn is_locked(&mut self) -> bool {
        self.get_bool(&SYSTEM_NAMESPACE, &KEY_LOCKED).unwrap_or(false)
    }

    pub fn lock(&mut self) -> Result<(), StorageError> {
        self.set_bool(&SYSTEM_NAMESPACE, &KEY_LOCKED, true)
    }

    /// Round-trips the same canary used before the extraction. Health state
    /// itself is now maintained by the reusable storage backend.
    pub fn self_check(&mut self) -> bool {
        const CANARY: Key = Key::from_str("canary");
        self.backend.self_check(&SYSTEM_NAMESPACE, &CANARY)
    }

    pub fn is_healthy(&self) -> bool {
        self.backend.is_healthy()
    }


    pub fn cfg_generation(&mut self) -> u32 {
        self.get_u32(&CFG_NAMESPACE, &KEY_CFG_GENERATION).unwrap_or(0)
    }

    pub fn active_cfg_generation(&self) -> u32 {
        self.active_cfg_generation
    }

    pub fn active_cfg(&self) -> &BTreeMap<String, String> {
        &self.active_cfg
    }

    pub fn cfg_set(&mut self, key: &str, value: &str) -> Result<ConfigSetResult, StorageError> {
        if key.is_empty() || key.len() > MAX_CFG_KEY_LEN || key.starts_with('_') {
            return Ok(ConfigSetResult::Rejected);
        }
        let key = Key::from_str(key);
        if value.is_empty() {
            self.delete(&CFG_NAMESPACE, &key)?;
            Ok(ConfigSetResult::Deleted)
        } else {
            self.set_string(&CFG_NAMESPACE, &key, value)?;
            Ok(ConfigSetResult::Stored)
        }
    }

    pub fn cfg_bump_generation(&mut self) -> Result<u32, StorageError> {
        let next = self.cfg_generation().wrapping_add(1);
        self.set_u32(&CFG_NAMESPACE, &KEY_CFG_GENERATION, next)?;
        Ok(next)
    }

    pub fn cfg_entries(&mut self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (key, value) in self.backend.string_entries(&CFG_NAMESPACE) {
            if key.as_str().starts_with('_') {
                continue;
            }
            out.insert(String::from(key.as_str()), value);
        }
        out
    }
}
