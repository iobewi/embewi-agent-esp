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
//! it. Certificate validity dates (`notBefore`/`notAfter`) are checked:
//! `hook-wall-clock` is enabled and `crate::time` (SNTP) is installed as
//! MbedTLS's wall clock in [`init`]. The peer is therefore never talked to
//! before SNTP has converged ([`ClientTlsError::ClockUnsynced`]) -- this
//! build fails closed there instead of implementing contrat §5's relaxed
//! `clock_unsynced` channel (chain/CN verified, dates suspended), which
//! would accept an expired certificate.

use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::string::String;
use core::convert::Infallible;

use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_storage_manager::Key;
use log::warn;
use mbedtls_rs::io::{ErrorType, Read};
use mbedtls_rs::sys::hook::timer::{MbedtlsTimer, hook_timer};
use mbedtls_rs::sys::hook::wall_clock::{MbedtlsWallClock, hook_wall_clock};
use mbedtls_rs::sys::{mbedtls_ms_time_t, tm};
use mbedtls_rs::{
    Certificate, ClientSessionConfig, Credentials, PrivateKey, ServerSessionConfig, Session, SessionConfig,
    SessionError, Tls, TlsReference, X509,
};
use picoserve::mem::BorrowedBuffer;
use rand_core::{TryCryptoRng, TryRng};
use static_cell::StaticCell;

use crate::storage::{SharedStorage, Storage};

const NAMESPACE: Key = Key::from_str("tls");
const KEY_CA: Key = Key::from_str("ca");

/// The server cert/key pair lives in one of two NVS banks, A (0) and B (1);
/// `KEY_SLOT` says which one is live. Installing a new pair writes the
/// *inactive* bank completely, reads it back, and only then flips `KEY_SLOT`
/// with a single write -- so at every instant either the old pair or the
/// new one is fully usable, whatever fails or loses power in between.
const KEY_SLOT: Key = Key::from_str("slot");
const PAIR_KEYS: [(Key, Key); 2] =
    [(Key::from_str("cert_a"), Key::from_str("key_a")), (Key::from_str("cert_b"), Key::from_str("key_b"))];
/// Pre-A/B storage: a single pair, used as the live one until the first
/// `save_cert` on a device flashed before banks existed (then deleted).
const LEGACY_KEYS: (Key, Key) = (Key::from_str("cert"), Key::from_str("key"));

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

/// MbedTLS's wall clock (X.509 validity dates), backed by SNTP. `None`
/// before the first sync, which MbedTLS treats as "every certificate is
/// invalid" -- fails closed even if a caller forgot the
/// [`ClientTlsError::ClockUnsynced`] check.
struct SntpWallClock;

impl MbedtlsWallClock for SntpWallClock {
    fn instant(&self) -> Option<tm> {
        epoch_to_tm(crate::time::now()?)
    }
}

/// Monotonic clock MbedTLS wants alongside the wall clock (timeouts).
struct UptimeTimer;

impl MbedtlsTimer for UptimeTimer {
    fn now(&self) -> mbedtls_ms_time_t {
        embassy_time::Instant::now().as_millis() as mbedtls_ms_time_t
    }
}

/// Unix epoch seconds -> broken-down UTC time (Hinnant's civil-from-days).
/// `None` past year 9999 (X.509's own range), never wraps.
fn epoch_to_tm(epoch: u64) -> Option<tm> {
    let days = i64::try_from(epoch / 86_400).ok()?;
    let secs = (epoch % 86_400) as i32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as i32; // 1..=12
    let year = yoe + era * 400 + i64::from(month <= 2);
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let is_leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    const CUMULATIVE: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let yday = CUMULATIVE[(month - 1) as usize] + day - 1 + i32::from(is_leap && month > 2);
    Some(tm {
        tm_sec: secs % 60,
        tm_min: secs / 60 % 60,
        tm_hour: secs / 3_600,
        tm_mday: day,
        tm_mon: month - 1,
        tm_year: (year - 1900) as i32,
        tm_wday: ((days + 4).rem_euclid(7)) as i32, // 1970-01-01 was a Thursday
        tm_yday: yday,
        tm_isdst: 0,
    })
}

static WALL_CLOCK: SntpWallClock = SntpWallClock;
static TIMER: UptimeTimer = UptimeTimer;
static RNG: StaticCell<EspCryptoRng> = StaticCell::new();
static TLS: StaticCell<Tls<'static>> = StaticCell::new();

/// Named alias so callers threading the reference through struct fields
/// (e.g. `WifiManager`) don't need to write out `TlsReference<'static>`.
pub type TlsReferenceStatic = TlsReference<'static>;

/// Initializes the global MbedTLS instance. Must be called exactly once, at
/// boot, before any `Session` is created -- see `Tls::new`'s own
/// "only one active instance" invariant.
pub fn init() -> TlsReference<'static> {
    // SAFETY: called once at boot, before any MbedTLS X.509 use (documented
    // requirement of both hooks); both statics are `'static` and stateless.
    unsafe {
        hook_timer(Some(&TIMER));
        hook_wall_clock(Some(&WALL_CLOCK));
    }
    let rng = RNG.init(EspCryptoRng(esp_hal::rng::Rng::new()));
    let tls = TLS.init(Tls::new(rng).expect("tls::init() called more than once"));
    tls.reference()
}

/// Why [`save_cert`] refused or failed.
pub enum SaveCertError {
    /// A PEM couldn't be parsed.
    Invalid,
    /// Both parse, but the private key isn't the certificate's.
    Mismatch,
    /// NVS refused a write (or what it stored doesn't read back). The
    /// previously active pair is untouched.
    Storage,
}

/// Validates and installs a new cert/key pair (contrat, `POST
/// /v1alpha1/tls/cert`), transactionally: the pair must parse *and* match,
/// then it goes to the inactive bank, is read back byte for byte, and only
/// then does one write of `KEY_SLOT` make it live. On any error the pair
/// that was working before keeps working.
pub async fn save_cert(storage: &SharedStorage, cert_pem: &str, key_pem: &str) -> Result<(), SaveCertError> {
    check_pair(cert_pem, key_pem)?;
    if build_server_config(cert_pem, key_pem).is_err() {
        return Err(SaveCertError::Invalid);
    }

    let mut storage = storage.lock().await;
    let active = storage.get_u8(&NAMESPACE, &KEY_SLOT).filter(|slot| usize::from(*slot) < PAIR_KEYS.len());
    let target = active.map_or(0, |slot| 1 - slot);
    let (cert_key, key_key) = &PAIR_KEYS[usize::from(target)];

    storage.set_string(&NAMESPACE, cert_key, cert_pem).map_err(|_| SaveCertError::Storage)?;
    storage.set_string(&NAMESPACE, key_key, key_pem).map_err(|_| SaveCertError::Storage)?;
    // Byte-identical to what was just validated, hence valid too.
    if storage.get_string(&NAMESPACE, cert_key).as_deref() != Some(cert_pem)
        || storage.get_string(&NAMESPACE, key_key).as_deref() != Some(key_pem)
    {
        warn!("tls: candidate bank doesn't read back identical, keeping the active pair");
        return Err(SaveCertError::Storage);
    }

    // The commit point.
    storage.set_u8(&NAMESPACE, &KEY_SLOT, target).map_err(|_| SaveCertError::Storage)?;
    if storage.get_u8(&NAMESPACE, &KEY_SLOT) != Some(target) {
        return Err(SaveCertError::Storage);
    }

    // Reclaim the pre-A/B pair's space; harmless if this fails.
    let _ = storage.delete(&NAMESPACE, &LEGACY_KEYS.0);
    let _ = storage.delete(&NAMESPACE, &LEGACY_KEYS.1);
    Ok(())
}

/// The live pair: the bank `KEY_SLOT` names, or the legacy single pair on a
/// device that never ran `save_cert` since banks were introduced.
fn load_active_pair(storage: &mut Storage) -> Option<(String, String)> {
    let (cert_key, key_key) = match storage.get_u8(&NAMESPACE, &KEY_SLOT) {
        Some(slot) if usize::from(slot) < PAIR_KEYS.len() => &PAIR_KEYS[usize::from(slot)],
        _ => &LEGACY_KEYS,
    };
    Some((storage.get_string(&NAMESPACE, cert_key)?, storage.get_string(&NAMESPACE, key_key)?))
}

/// RNG callback handed to MbedTLS's key checks (`mbedtls_pk_check_pair`
/// requires one); same hardware source as [`EspCryptoRng`].
unsafe extern "C" fn mbedtls_rng(_ctx: *mut core::ffi::c_void, out: *mut u8, len: usize) -> core::ffi::c_int {
    // SAFETY: MbedTLS passes a writable buffer of `len` bytes.
    esp_hal::rng::Rng::new().read(unsafe { core::slice::from_raw_parts_mut(out, len) });
    0
}

/// Checks that `key_pem` is the private key of the (first, i.e. leaf)
/// certificate in `cert_pem`. `ServerSessionConfig::new` doesn't -- MbedTLS
/// leaves it to the application (`mbedtls_pk_check_pair`) -- and a mismatched
/// pair would otherwise install fine and then fail every handshake, locking
/// the admin API out over HTTPS.
fn check_pair(cert_pem: &str, key_pem: &str) -> Result<(), SaveCertError> {
    use mbedtls_rs::sys::{
        mbedtls_pk_check_pair, mbedtls_pk_context, mbedtls_pk_free, mbedtls_pk_init, mbedtls_pk_parse_key,
        mbedtls_x509_crt, mbedtls_x509_crt_free, mbedtls_x509_crt_init, mbedtls_x509_crt_parse,
    };

    /// Frees the MbedTLS contexts on every exit path.
    struct Contexts {
        crt: Box<mbedtls_x509_crt>,
        pk: Box<mbedtls_pk_context>,
    }
    impl Drop for Contexts {
        fn drop(&mut self) {
            // SAFETY: both were initialised in `check_pair` before any
            // other use, and are freed exactly once, here.
            unsafe {
                mbedtls_x509_crt_free(&mut *self.crt);
                mbedtls_pk_free(&mut *self.pk);
            }
        }
    }

    let cert_c = CString::new(cert_pem).map_err(|_| SaveCertError::Invalid)?;
    let key_c = CString::new(key_pem).map_err(|_| SaveCertError::Invalid)?;

    let mut ctx = Contexts { crt: Box::default(), pk: Box::default() };
    // SAFETY: freshly allocated contexts; the PEM buffers are NUL-terminated
    // and outlive the calls (length includes the NUL, as MbedTLS requires).
    unsafe {
        mbedtls_x509_crt_init(&mut *ctx.crt);
        mbedtls_pk_init(&mut *ctx.pk);

        let rc = mbedtls_x509_crt_parse(&mut *ctx.crt, cert_c.as_ptr().cast(), cert_c.count_bytes() + 1);
        if rc != 0 {
            warn!("tls: certificate parse failed: -0x{:04x}", -rc);
            return Err(SaveCertError::Invalid);
        }
        let rc = mbedtls_pk_parse_key(
            &mut *ctx.pk,
            key_c.as_ptr().cast(),
            key_c.count_bytes() + 1,
            core::ptr::null(),
            0,
            Some(mbedtls_rng),
            core::ptr::null_mut(),
        );
        if rc != 0 {
            warn!("tls: private key parse failed: -0x{:04x}", -rc);
            return Err(SaveCertError::Invalid);
        }
        let rc = mbedtls_pk_check_pair(&ctx.crt.pk, &*ctx.pk, Some(mbedtls_rng), core::ptr::null_mut());
        if rc != 0 {
            warn!("tls: private key doesn't match the certificate: -0x{:04x}", -rc);
            return Err(SaveCertError::Mismatch);
        }
    }
    Ok(())
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
    let (cert, key) = load_active_pair(&mut *storage.lock().await)?;
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
    /// SNTP hasn't converged yet, so certificate dates can't be checked:
    /// not a handshake failure, and retrying once the clock is set works.
    ClockUnsynced,
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
            Self::ClockUnsynced => write!(f, "clock not synchronized yet (SNTP)"),
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
    if !crate::time::is_set() {
        return Err(ClientTlsError::ClockUnsynced);
    }
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
