//! Embewi TLS policy and persistent configuration.
//!
//! Reusable ESP/MbedTLS mechanics live in `esp-hal-mbedtls`. This module
//! owns only the Embewi TLS configuration schema and policy. Persistence is
//! one opaque ConfigSpace value: CA + server certificate + private key are
//! replaced atomically by config-space-manager's backend.

use alloc::string::String;
use alloc::vec::Vec;

use config_space_manager::{Budget, ConfigSpace};
use embassy_net::tcp::TcpSocket;
use esp_hal_mbedtls::mbedtls_rs::{Session, SessionConfig, SessionError};
use log::warn;

use crate::config::NvsConfigBackend;

const MAGIC: &[u8; 4] = b"TLS1";
const HEADER_LEN: usize = 10;
const MAX_CA_LEN: usize = 2048;
const MAX_CERT_LEN: usize = 2048;
const MAX_KEY_LEN: usize = 2048;

/// Header plus the maximum encoded CA/certificate/private-key payload.
pub const CONFIG_BUDGET: Budget =
    Budget::new(HEADER_LEN + MAX_CA_LEN + MAX_CERT_LEN + MAX_KEY_LEN);

pub type TlsConfigSpace = ConfigSpace<NvsConfigBackend>;
pub type TlsReferenceStatic = esp_hal_mbedtls::TlsReferenceStatic;
pub use esp_hal_mbedtls::embassy::PicoserveTlsSocket as TlsSocket;

#[derive(Clone, Default)]
struct TlsConfig {
    ca_pem: String,
    cert_pem: String,
    key_pem: String,
}

impl TlsConfig {
    fn encode(&self) -> Option<Vec<u8>> {
        if self.ca_pem.len() > MAX_CA_LEN
            || self.cert_pem.len() > MAX_CERT_LEN
            || self.key_pem.len() > MAX_KEY_LEN
        {
            return None;
        }

        let ca_len = u16::try_from(self.ca_pem.len()).ok()?;
        let cert_len = u16::try_from(self.cert_pem.len()).ok()?;
        let key_len = u16::try_from(self.key_pem.len()).ok()?;
        let total = HEADER_LEN
            .checked_add(self.ca_pem.len())?
            .checked_add(self.cert_pem.len())?
            .checked_add(self.key_pem.len())?;
        if total > CONFIG_BUDGET.max_bytes() {
            return None;
        }

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&ca_len.to_le_bytes());
        out.extend_from_slice(&cert_len.to_le_bytes());
        out.extend_from_slice(&key_len.to_le_bytes());
        out.extend_from_slice(self.ca_pem.as_bytes());
        out.extend_from_slice(self.cert_pem.as_bytes());
        out.extend_from_slice(self.key_pem.as_bytes());
        Some(out)
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        if raw.len() < HEADER_LEN || &raw[..4] != MAGIC {
            return None;
        }
        let ca_len = u16::from_le_bytes([raw[4], raw[5]]) as usize;
        let cert_len = u16::from_le_bytes([raw[6], raw[7]]) as usize;
        let key_len = u16::from_le_bytes([raw[8], raw[9]]) as usize;
        if ca_len > MAX_CA_LEN || cert_len > MAX_CERT_LEN || key_len > MAX_KEY_LEN {
            return None;
        }

        let ca_end = HEADER_LEN.checked_add(ca_len)?;
        let cert_end = ca_end.checked_add(cert_len)?;
        let key_end = cert_end.checked_add(key_len)?;
        if key_end != raw.len() {
            return None;
        }

        Some(Self {
            ca_pem: String::from(core::str::from_utf8(&raw[HEADER_LEN..ca_end]).ok()?),
            cert_pem: String::from(core::str::from_utf8(&raw[ca_end..cert_end]).ok()?),
            key_pem: String::from(core::str::from_utf8(&raw[cert_end..key_end]).ok()?),
        })
    }
}

async fn load_config(space: &TlsConfigSpace) -> Option<TlsConfig> {
    match space.load().await {
        Ok(Some(snapshot)) => match TlsConfig::decode(&snapshot.data) {
            Some(config) => Some(config),
            None => {
                warn!(
                    "tls: stored config generation={} has an unsupported/corrupt schema",
                    snapshot.generation
                );
                None
            }
        },
        Ok(None) => Some(TlsConfig::default()),
        Err(e) => {
            warn!("tls: config-space load failed: {e:?}");
            None
        }
    }
}

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

/// Validates and atomically replaces the server certificate/private-key pair.
/// The current CA is preserved in the same ConfigSpace value.
pub async fn save_cert(
    space: &TlsConfigSpace,
    cert_pem: &str,
    key_pem: &str,
) -> Result<(), SaveCertError> {
    match esp_hal_mbedtls::validate_cert_key_pair(cert_pem, key_pem) {
        Ok(()) => {}
        Err(esp_hal_mbedtls::PairError::Mismatch) => return Err(SaveCertError::Mismatch),
        Err(
            esp_hal_mbedtls::PairError::InvalidCertificate
            | esp_hal_mbedtls::PairError::InvalidPrivateKey,
        ) => return Err(SaveCertError::Invalid),
    }

    if esp_hal_mbedtls::server_config_from_pem(cert_pem, key_pem).is_err() {
        return Err(SaveCertError::Invalid);
    }

    let mut config = load_config(space).await.ok_or(SaveCertError::Storage)?;
    config.cert_pem = String::from(cert_pem);
    config.key_pem = String::from(key_pem);
    let encoded = config.encode().ok_or(SaveCertError::Storage)?;
    space
        .commit(&encoded)
        .await
        .map_err(|_| SaveCertError::Storage)?;
    Ok(())
}

/// Builds the current server TLS config. Missing cert/key means the admin
/// surface remains on plain HTTP during bootstrap.
pub async fn server_config(space: &TlsConfigSpace) -> Option<SessionConfig<'static>> {
    let config = load_config(space).await?;
    if config.cert_pem.is_empty() || config.key_pem.is_empty() {
        return None;
    }
    esp_hal_mbedtls::server_config_from_pem(&config.cert_pem, &config.key_pem).ok()
}

/// Validates and atomically replaces the CA trusted by outbound TLS
/// connections while preserving the current server certificate/key.
pub async fn save_ca(space: &TlsConfigSpace, ca_pem: &str) -> Result<(), SaveCertError> {
    if !esp_hal_mbedtls::validate_ca_pem(ca_pem) {
        return Err(SaveCertError::Invalid);
    }

    let mut config = load_config(space).await.ok_or(SaveCertError::Storage)?;
    config.ca_pem = String::from(ca_pem);
    let encoded = config.encode().ok_or(SaveCertError::Storage)?;
    space
        .commit(&encoded)
        .await
        .map_err(|_| SaveCertError::Storage)?;
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
pub async fn connect_client<'h, 'buf>(
    tls: TlsReferenceStatic,
    stack: embassy_net::Stack<'static>,
    space: &TlsConfigSpace,
    rx_buffer: &'buf mut [u8],
    tx_buffer: &'buf mut [u8],
    host: &'h core::ffi::CStr,
    port: u16,
) -> Result<Session<'h, TcpSocket<'buf>>, ClientTlsError> {
    if !crate::time::is_set() {
        return Err(ClientTlsError::ClockUnsynced);
    }

    let config = load_config(space).await.ok_or(ClientTlsError::NoCa)?;
    if config.ca_pem.is_empty() {
        return Err(ClientTlsError::NoCa);
    }

    esp_hal_mbedtls::embassy::connect_client(
        tls,
        stack,
        rx_buffer,
        tx_buffer,
        host,
        port,
        &config.ca_pem,
    )
    .await
    .map_err(|e| match e {
        esp_hal_mbedtls::embassy::ClientConnectError::BadCa => ClientTlsError::BadCa,
        esp_hal_mbedtls::embassy::ClientConnectError::Dns => ClientTlsError::Dns,
        esp_hal_mbedtls::embassy::ClientConnectError::Tcp(e) => ClientTlsError::Tcp(e),
        esp_hal_mbedtls::embassy::ClientConnectError::Handshake(e) => {
            ClientTlsError::Handshake(e)
        }
    })
}
