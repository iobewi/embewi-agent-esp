//! Inbound TLS for the admin server (`http/mod.rs`'s `/v1alpha1/*` + config
//! page). `POST /v1alpha1/tls/cert` pushes a certificate+key PEM pair
//! (contrat), persisted to NVS; the admin server upgrades to HTTPS as soon
//! as a valid pair is loaded, and stays on plain HTTP until then -- a
//! device needs at least one clear-text access to push its first
//! certificate (same bootstrap reasoning as the HTTP config page itself).
//!
//! Built on `mbedtls-rs` (crates.io, `esp-rs` org): no esp-hal-specific
//! glue crate needed, it only wants an RNG (`EspCryptoRng` below) and a
//! stream implementing `embedded-io-async`. See the git history for the
//! research that found this -- it compiles and links directly against this
//! project's stable `esp-hal ~1.1.0`/`esp-radio 0.18.0`, no bleeding-edge
//! dependency chain required (unlike the frozen `spike/mbedtls-rs-bleeding-edge`
//! branch from before this crate was published).
//!
//! Outbound TLS (`heartbeat.rs`/`log_stream.rs`, verifying the Core's
//! certificate) is also here: `POST /v1alpha1/tls/ca` pushes the CA PEM to
//! trust, and [`connect_client`] does the TCP connect + handshake against
//! it. Contrat §5's `clock_unsynced` relaxed-validation channel ("chiffrement
//! actif, chaîne/CN toujours vérifiés, seule la fenêtre de validité
//! temporelle est suspendue") is this build's *unconditional* behavior
//! right now, not something that tightens back up once SNTP syncs: date
//! checking needs MbedTLS's `hook-wall-clock` feature plus installing a
//! real wall clock (`crate::time`), neither of which is wired yet. Until
//! then this is honestly "always relaxed on dates", not "relaxed only
//! while unsynced".

use alloc::ffi::CString;
use core::convert::Infallible;

use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_nvs::Key;
use log::warn;
use mbedtls_rs::io::{ErrorType, Read};
use mbedtls_rs::{
    Certificate, ClientSessionConfig, Credentials, PrivateKey, ServerSessionConfig, Session, SessionConfig,
    SessionError, Tls, TlsReference, X509,
};
use picoserve::mem::BorrowedBuffer;
use rand_core::{TryCryptoRng, TryRng};
use static_cell::StaticCell;

use crate::storage::SharedStorage;

const NAMESPACE: Key = Key::from_str("tls");
const KEY_CERT: Key = Key::from_str("cert");
const KEY_KEY: Key = Key::from_str("key");
const KEY_CA: Key = Key::from_str("ca");

/// Wraps `esp_hal::rng::Rng` to assert the `CryptoRng` marker mbedtls-rs
/// requires -- esp-hal itself only implements the plain, non-crypto `TryRng`
/// (`rand_core` 0.10), staying silent on whether its output is CSPRNG-grade.
/// ESP32-C3's HW RNG draws from an internal RF/thermal noise source
/// (Espressif's own TRNG whitepaper) and is the same source
/// `agent::generate_token()` already trusts for the Bearer token, so
/// asserting it here is consistent, not a new risk.
struct EspCryptoRng(esp_hal::rng::Rng);

impl TryRng for EspCryptoRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        Ok(self.0.random())
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        let mut bytes = [0u8; 8];
        self.0.read(&mut bytes);
        Ok(u64::from_le_bytes(bytes))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        self.0.read(dst);
        Ok(())
    }
}

impl TryCryptoRng for EspCryptoRng {}

static RNG: StaticCell<EspCryptoRng> = StaticCell::new();
static TLS: StaticCell<Tls<'static>> = StaticCell::new();

/// Named alias so callers threading the reference through struct fields
/// (e.g. `WifiManager`) don't need to write out `TlsReference<'static>`.
pub type TlsReferenceStatic = TlsReference<'static>;

/// Initializes the global MbedTLS instance. Must be called exactly once, at
/// boot, before any `Session` is created -- see `Tls::new`'s own
/// "only one active instance" invariant.
pub fn init() -> TlsReference<'static> {
    let rng = RNG.init(EspCryptoRng(esp_hal::rng::Rng::new()));
    let tls = TLS.init(Tls::new(rng).expect("tls::init() called more than once"));
    tls.reference()
}

/// Why [`save_cert`]/[`save_ca`] refused or failed.
pub enum SaveCertError {
    /// A PEM couldn't be parsed.
    Invalid,
    /// NVS refused a write.
    Storage,
}

/// Validates and persists a new cert/key pair (contrat, `POST
/// /v1alpha1/tls/cert`). `Invalid` if a PEM couldn't be parsed -- nothing is
/// saved in that case, so a bad push can't silently break a previously
/// working certificate. An NVS failure is reported, not swallowed.
pub async fn save_cert(storage: &SharedStorage, cert_pem: &str, key_pem: &str) -> Result<(), SaveCertError> {
    if build_server_config(cert_pem, key_pem).is_err() {
        return Err(SaveCertError::Invalid);
    }
    let mut storage = storage.lock().await;
    storage.set_string(&NAMESPACE, &KEY_CERT, cert_pem).map_err(|_| SaveCertError::Storage)?;
    storage.set_string(&NAMESPACE, &KEY_KEY, key_pem).map_err(|_| SaveCertError::Storage)
}

/// Builds the server TLS config from whatever is currently stored in NVS,
/// `None` if nothing's configured or it fails to parse. Reparsed on every
/// call (every incoming connection) rather than cached in a `static`:
/// `mbedtls-rs`'s certificate/key handles wrap raw C pointers and are not
/// `Send`, so they can't live in a `Mutex`-guarded `static` shared across
/// tasks -- and re-parsing a PEM pair per connection is cheap next to the
/// TLS handshake itself, which dwarfs it. This is also why there's no
/// separate "load from NVS at boot" step: this *is* that load, just run
/// lazily on first (and every) connection instead of once upfront.
pub async fn server_config(storage: &SharedStorage) -> Option<SessionConfig<'static>> {
    let (cert, key) = {
        let mut storage = storage.lock().await;
        (storage.get_string(&NAMESPACE, &KEY_CERT), storage.get_string(&NAMESPACE, &KEY_KEY))
    };
    let (cert, key) = (cert?, key?);
    build_server_config(&cert, &key).ok()
}

fn build_server_config(cert_pem: &str, key_pem: &str) -> Result<SessionConfig<'static>, ()> {
    let cert_c = CString::new(cert_pem).map_err(|_| ())?;
    let key_c = CString::new(key_pem).map_err(|_| ())?;
    let certificate = Certificate::new(X509::PEM(&cert_c)).map_err(|e| {
        warn!("tls: certificate parse failed: {e}");
    })?;
    let private_key = PrivateKey::new(X509::PEM(&key_c), None).map_err(|e| {
        warn!("tls: private key parse failed: {e}");
    })?;
    Ok(SessionConfig::Server(ServerSessionConfig::new(Credentials { certificate, private_key })))
}

/// Wraps a connected [`Session`] as a [`picoserve::io::Socket`], so
/// `http/mod.rs`'s `serve_connection` (generic over `Socket`) can serve
/// HTTPS exactly the same way it serves plain HTTP over a bare `TcpSocket`.
///
/// Doesn't use `Session::split` (which itself drives the handshake and is
/// therefore `async`/fallible) -- `picoserve::io::Socket::split` is sync and
/// infallible, so the handshake must already be done by the time this is
/// constructed. Instead both halves share the one `Session` behind a
/// `Mutex`, locked per read/write call; picoserve only ever uses one half at
/// a time on a single connection, so this never contends.
// `'tls` (the global `Tls`/`SessionConfig` instance, always `'static` in
// practice) and `'buf` (the `TcpSocket`'s own rx/tx buffers, borrowed from
// `http::run`'s stack and *not* `'static`) are kept as separate lifetime
// parameters throughout -- collapsing them into one would wrongly force the
// per-connection TCP buffers to outlive the whole program.
pub struct TlsSocket<'tls, 'buf> {
    session: Mutex<CriticalSectionRawMutex, Session<'tls, TcpSocket<'buf>>>,
}

impl<'tls, 'buf> TlsSocket<'tls, 'buf> {
    /// `session` must already be connected (its first `read`/`write`/
    /// `connect` call would otherwise negotiate the handshake implicitly,
    /// which is fine functionally but means the caller can't observe a
    /// handshake failure before picoserve starts trying to parse a
    /// request out of it).
    pub fn new(session: Session<'tls, TcpSocket<'buf>>) -> Self {
        Self { session: Mutex::new(session) }
    }
}

pub struct TlsHalf<'tls, 'buf, 'b> {
    session: &'b Mutex<CriticalSectionRawMutex, Session<'tls, TcpSocket<'buf>>>,
}

impl ErrorType for TlsHalf<'_, '_, '_> {
    type Error = SessionError;
}

impl Read for TlsHalf<'_, '_, '_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, SessionError> {
        self.session.lock().await.read(buf).await
    }
}

impl mbedtls_rs::io::Write for TlsHalf<'_, '_, '_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, SessionError> {
        self.session.lock().await.write(buf).await
    }

    async fn flush(&mut self) -> Result<(), SessionError> {
        self.session.lock().await.flush().await
    }
}

impl picoserve::io::Write for TlsHalf<'_, '_, '_> {
    async fn write_with<F: FnOnce(picoserve::mem::BorrowedCursor<'_>) -> R, R>(
        &mut self,
        f: F,
    ) -> Result<R, SessionError> {
        // No zero-copy path here (unlike the plain-`TcpSocket` impl this
        // mirrors): TLS records must be encrypted through MbedTLS before
        // touching the wire, so there's no raw send buffer to lend
        // directly. Same pattern picoserve's own `Vec<u8>` impl uses.
        let mut buffer = [0u8; 1024];
        let mut buffer = BorrowedBuffer::new(&mut buffer);
        let output = f(buffer.unfilled());
        self.session.lock().await.write(buffer.filled()).await?;
        Ok(output)
    }
}

impl<'tls, 'buf> picoserve::io::Socket<picoserve::EmbassyRuntime> for TlsSocket<'tls, 'buf> {
    type Error = SessionError;
    type ReadHalf<'b>
        = TlsHalf<'tls, 'buf, 'b>
    where
        Self: 'b;
    type WriteHalf<'b>
        = TlsHalf<'tls, 'buf, 'b>
    where
        Self: 'b;

    fn split(&mut self) -> (Self::ReadHalf<'_>, Self::WriteHalf<'_>) {
        (TlsHalf { session: &self.session }, TlsHalf { session: &self.session })
    }

    async fn abort<T: picoserve::time::Timer<picoserve::EmbassyRuntime>>(
        self,
        _timeouts: &picoserve::Timeouts,
        _timer: &T,
    ) -> Result<(), picoserve::Error<Self::Error>> {
        let mut session = self.session.into_inner();
        session.stream().abort();
        Ok(())
    }

    async fn shutdown<T: picoserve::time::Timer<picoserve::EmbassyRuntime>>(
        self,
        _timeouts: &picoserve::Timeouts,
        _timer: &T,
    ) -> Result<(), picoserve::Error<Self::Error>> {
        let mut session = self.session.into_inner();
        // Best-effort TLS close_notify (also flushes MbedTLS's own pending
        // output). Deliberately simpler than `TcpSocket`'s own `shutdown`
        // (no explicit drain-until-peer-FIN loop): this admin server only
        // ever serves one short-lived connection at a time, so a slightly
        // less graceful TCP teardown here costs nothing worth the extra
        // complexity of replicating that loop through an encrypted stream.
        let _ = session.close().await;
        session.stream().close();
        Ok(())
    }
}

/// Validates and persists the CA to trust for outbound TLS connections
/// (`heartbeat.rs`/`log_stream.rs`, contrat §5), pushed via `POST
/// /v1alpha1/tls/ca`. `Invalid` if the PEM couldn't be parsed -- same
/// don't-break-a-working-CA-on-a-bad-push reasoning as `save_cert`; a single
/// NVS entry is replaced atomically, so no A/B bank is needed here.
pub async fn save_ca(storage: &SharedStorage, ca_pem: &str) -> Result<(), SaveCertError> {
    let Ok(ca_c) = CString::new(ca_pem) else {
        return Err(SaveCertError::Invalid);
    };
    if let Err(e) = Certificate::new(X509::PEM(&ca_c)) {
        warn!("tls: CA parse failed: {e}");
        return Err(SaveCertError::Invalid);
    }
    let mut storage = storage.lock().await;
    storage.set_string(&NAMESPACE, &KEY_CA, ca_pem).map_err(|_| SaveCertError::Storage)?;
    if storage.get_string(&NAMESPACE, &KEY_CA).as_deref() != Some(ca_pem) {
        return Err(SaveCertError::Storage);
    }
    Ok(())
}

/// Why [`connect_client`] couldn't establish a connection.
pub enum ClientTlsError {
    /// No CA has been pushed via `POST /v1alpha1/tls/ca` yet.
    NoCa,
    /// The stored CA PEM no longer parses (shouldn't happen -- `save_ca`
    /// validates before persisting -- but NVS is fallible).
    BadCa,
    Dns,
    Tcp(embassy_net::tcp::ConnectError),
    Handshake(SessionError),
}

impl core::fmt::Display for ClientTlsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoCa => write!(f, "no CA configured (POST /v1alpha1/tls/ca)"),
            Self::BadCa => write!(f, "stored CA failed to parse"),
            Self::Dns => write!(f, "DNS resolution failed"),
            Self::Tcp(e) => write!(f, "TCP connect failed: {e:?}"),
            Self::Handshake(e) => write!(f, "TLS handshake failed: {e}"),
        }
    }
}

/// Resolves `host`, connects, and completes a TLS handshake verifying the
/// peer's certificate against the CA pushed via `POST /v1alpha1/tls/ca` --
/// the one piece `heartbeat.rs` and `log_stream.rs` share, everything
/// after (the actual HTTP/WS exchange) is protocol-specific and stays in
/// each of them.
///
/// `rx_buffer`/`tx_buffer` are the caller's own long-lived TCP buffers
/// (declared once outside its reconnect loop, exactly like `http::run`'s),
/// and `host` must outlive the returned `Session` -- in practice, a
/// `CString` the caller re-derives from `ctrl_url` once per reconnect
/// attempt and keeps alive for that attempt's whole scope (see the callers
/// for the exact shape; `mbedtls_ssl_set_hostname` actually copies it
/// internally, but `ClientSessionConfig`'s own lifetime bound doesn't know
/// that, so the caller still has to satisfy it).
pub async fn connect_client<'h, 'buf>(
    tls: TlsReferenceStatic,
    stack: embassy_net::Stack<'static>,
    storage: &SharedStorage,
    rx_buffer: &'buf mut [u8],
    tx_buffer: &'buf mut [u8],
    host: &'h core::ffi::CStr,
    port: u16,
) -> Result<Session<'h, TcpSocket<'buf>>, ClientTlsError> {
    let ca_pem = storage.lock().await.get_string(&NAMESPACE, &KEY_CA).ok_or(ClientTlsError::NoCa)?;
    let ca_c = CString::new(ca_pem).map_err(|_| ClientTlsError::BadCa)?;
    let ca_chain = Certificate::new(X509::PEM(&ca_c)).map_err(|_| ClientTlsError::BadCa)?;

    let host_str = host.to_str().map_err(|_| ClientTlsError::Dns)?;
    let dns = embassy_net::dns::DnsSocket::new(stack);
    let ip = dns
        .query(host_str, embassy_net::dns::DnsQueryType::A)
        .await
        .ok()
        .and_then(|addrs| addrs.into_iter().next())
        .ok_or(ClientTlsError::Dns)?;

    let mut socket = TcpSocket::new(stack, rx_buffer, tx_buffer);
    socket.connect((ip, port)).await.map_err(ClientTlsError::Tcp)?;

    let config = SessionConfig::Client(ClientSessionConfig { ca_chain: Some(ca_chain), server_name: Some(host), ..ClientSessionConfig::new() });
    let mut session = Session::new(tls, socket, &config).map_err(ClientTlsError::Handshake)?;
    session.connect().await.map_err(ClientTlsError::Handshake)?;
    Ok(session)
}
