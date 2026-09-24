//! Embewi storage model layered on top of `esp-storage-manager`.
//!
//! This module owns application namespaces, keys and boot-snapshot semantics.
//! Physical flash ownership, cached NVS coordination and bounded raw-flash
//! access live in the reusable `esp-storage-manager` crate.

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

pub struct Storage {
    backend: StorageManager,
}

impl Storage {
    pub fn new(flash: FLASH<'static>) -> Self {
        Self {
            backend: StorageManager::new(
                flash,
                NvsPartition::new(PARTITION_OFFSET, PARTITION_SIZE),
            ),
        }
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



}
