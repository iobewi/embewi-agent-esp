//! Embewi TLS policy and persistence adapter.
//!
//! Reusable ESP/MbedTLS mechanics live in `esp-hal-mbedtls`: hardware RNG,
//! MbedTLS time hooks, cert/key validation, server configuration, outbound
//! Embassy DNS/TCP/TLS handshakes and the picoserve TLS socket adapter.
//!
//! This module deliberately retains only application policy:
//!
//! - NVS layout and transactional certificate-bank switching;
//! - persisted outbound CA;
//! - the requirement that SNTP has converged before outbound TLS;
//! - Embewi-facing error mapping.

use alloc::string::String;

use embassy_net::tcp::TcpSocket;
use esp_hal_mbedtls::mbedtls_rs::{Session, SessionConfig, SessionError};
use esp_storage_manager::Key;
use log::warn;

use crate::storage::{SharedStorage, Storage};

const NAMESPACE: Key = Key::from_str("tls");
const KEY_CA: Key = Key::from_str("ca");

const KEY_SLOT: Key = Key::from_str("slot");
const PAIR_KEYS: [(Key, Key); 2] =
    [(Key::from_str("cert_a"), Key::from_str("key_a")), (Key::from_str("cert_b"), Key::from_str("key_b"))];
const LEGACY_KEYS: (Key, Key) = (Key::from_str("cert"), Key::from_str("key"));

pub type TlsReferenceStatic = esp_hal_mbedtls::TlsReferenceStatic;
pub use esp_hal_mbedtls::embassy::PicoserveTlsSocket as TlsSocket;

/// Initializes the global MbedTLS instance using Embewi's SNTP-derived wall
/// clock. Before SNTP converges, the callback returns `None`, so certificate
/// date validation fails closed.
pub fn init() -> TlsReferenceStatic {
    esp_hal_mbedtls::init(crate::time::now)
}

pub enum SaveCertError {
    Invalid,
    Mismatch,
    Storage,
}

/// Validates and transactionally installs a new server certificate/private-key
/// pair. The inactive bank is written and read back before the one-byte slot
/// pointer is committed, so the previously active pair survives any earlier
/// failure or power cut.
pub async fn save_cert(storage: &SharedStorage, cert_pem: &str, key_pem: &str) -> Result<(), SaveCertError> {
    match esp_hal_mbedtls::validate_cert_key_pair(cert_pem, key_pem) {
        Ok(()) => {}
        Err(esp_hal_mbedtls::PairError::Mismatch) => return Err(SaveCertError::Mismatch),
        Err(esp_hal_mbedtls::PairError::InvalidCertificate | esp_hal_mbedtls::PairError::InvalidPrivateKey) => {
            return Err(SaveCertError::Invalid);
        }
    }

    // Exercise exactly the same high-level parser/config construction used
    // for incoming connections before anything is persisted.
    if esp_hal_mbedtls::server_config_from_pem(cert_pem, key_pem).is_err() {
        return Err(SaveCertError::Invalid);
    }

    let mut storage = storage.lock().await;
    let active = storage.get_u8(&NAMESPACE, &KEY_SLOT).filter(|slot| usize::from(*slot) < PAIR_KEYS.len());
    let target = active.map_or(0, |slot| 1 - slot);
    let (cert_key, key_key) = &PAIR_KEYS[usize::from(target)];

    storage.set_string(&NAMESPACE, cert_key, cert_pem).map_err(|_| SaveCertError::Storage)?;
    storage.set_string(&NAMESPACE, key_key, key_pem).map_err(|_| SaveCertError::Storage)?;
    if storage.get_string(&NAMESPACE, cert_key).as_deref() != Some(cert_pem)
        || storage.get_string(&NAMESPACE, key_key).as_deref() != Some(key_pem)
    {
        warn!("tls: candidate bank doesn't read back identical, keeping the active pair");
        return Err(SaveCertError::Storage);
    }

    storage.set_u8(&NAMESPACE, &KEY_SLOT, target).map_err(|_| SaveCertError::Storage)?;
    if storage.get_u8(&NAMESPACE, &KEY_SLOT) != Some(target) {
        return Err(SaveCertError::Storage);
    }

    let _ = storage.delete(&NAMESPACE, &LEGACY_KEYS.0);
    let _ = storage.delete(&NAMESPACE, &LEGACY_KEYS.1);
    Ok(())
}

fn load_active_pair(storage: &mut Storage) -> Option<(String, String)> {
    let (cert_key, key_key) = match storage.get_u8(&NAMESPACE, &KEY_SLOT) {
        Some(slot) if usize::from(slot) < PAIR_KEYS.len() => &PAIR_KEYS[usize::from(slot)],
        _ => &LEGACY_KEYS,
    };
    Some((storage.get_string(&NAMESPACE, cert_key)?, storage.get_string(&NAMESPACE, key_key)?))
}

/// Builds the current server TLS config from the persisted live bank.
pub async fn server_config(storage: &SharedStorage) -> Option<SessionConfig<'static>> {
    let (cert, key) = load_active_pair(&mut *storage.lock().await)?;
    esp_hal_mbedtls::server_config_from_pem(&cert, &key).ok()
}

/// Validates and persists the CA trusted by outbound TLS connections.
pub async fn save_ca(storage: &SharedStorage, ca_pem: &str) -> Result<(), SaveCertError> {
    if !esp_hal_mbedtls::validate_ca_pem(ca_pem) {
        return Err(SaveCertError::Invalid);
    }

    let mut storage = storage.lock().await;
    storage.set_string(&NAMESPACE, &KEY_CA, ca_pem).map_err(|_| SaveCertError::Storage)?;
    if storage.get_string(&NAMESPACE, &KEY_CA).as_deref() != Some(ca_pem) {
        return Err(SaveCertError::Storage);
    }
    Ok(())
}

pub enum ClientTlsError {
    ClockUnsynced,
    NoCa,
    BadCa,
    Dns,
    Tcp(embassy_net::tcp::ConnectError),
    Handshake(SessionError),
}

impl core::fmt::Display for ClientTlsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ClockUnsynced => write!(f, "clock not synchronized yet (SNTP)"),
            Self::NoCa => write!(f, "no CA configured (POST /v1alpha1/tls/ca)"),
            Self::BadCa => write!(f, "stored CA failed to parse"),
            Self::Dns => write!(f, "DNS resolution failed"),
            Self::Tcp(e) => write!(f, "TCP connect failed: {e:?}"),
            Self::Handshake(e) => write!(f, "TLS handshake failed: {e}"),
        }
    }
}

/// Embewi policy wrapper around the reusable Embassy/TLS connector.
///
/// The generic crate receives the already-provisioned CA PEM. This wrapper
/// enforces SNTP convergence and owns the NVS lookup/error semantics.
pub async fn connect_client<'h, 'buf>(
    tls: TlsReferenceStatic,
    stack: embassy_net::Stack<'static>,
    storage: &SharedStorage,
    rx_buffer: &'buf mut [u8],
    tx_buffer: &'buf mut [u8],
    host: &'h core::ffi::CStr,
    port: u16,
) -> Result<Session<'h, TcpSocket<'buf>>, ClientTlsError> {
    if !crate::time::is_set() {
        return Err(ClientTlsError::ClockUnsynced);
    }

    let ca_pem = storage.lock().await.get_string(&NAMESPACE, &KEY_CA).ok_or(ClientTlsError::NoCa)?;
    esp_hal_mbedtls::embassy::connect_client(tls, stack, rx_buffer, tx_buffer, host, port, &ca_pem)
        .await
        .map_err(|e| match e {
            esp_hal_mbedtls::embassy::ClientConnectError::BadCa => ClientTlsError::BadCa,
            esp_hal_mbedtls::embassy::ClientConnectError::Dns => ClientTlsError::Dns,
            esp_hal_mbedtls::embassy::ClientConnectError::Tcp(e) => ClientTlsError::Tcp(e),
            esp_hal_mbedtls::embassy::ClientConnectError::Handshake(e) => ClientTlsError::Handshake(e),
        })
}
