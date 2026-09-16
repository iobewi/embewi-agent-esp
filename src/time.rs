//! SNTP time sync (contrat §5): keeps a Unix epoch clock derived from the
//! embassy-time monotonic clock, refreshed periodically. Heartbeat/log
//! timestamps need real epoch seconds, not an uptime that resets to ~0 on
//! every boot -- see [`now`].

use core::cell::RefCell;
use core::net::{IpAddr, SocketAddr};

use critical_section::Mutex;
use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use log::{info, warn};
use sntpc::{NtpContext, get_time};
use sntpc_net_embassy::UdpSocketWrapper;
use sntpc_time_embassy::EmbassyTimestampGenerator;

const NTP_SERVER: &str = "pool.ntp.org";
const RESYNC_PERIOD: Duration = Duration::from_secs(3600);
const RETRY_PERIOD: Duration = Duration::from_secs(15);
/// Below this, the server's answer is obviously wrong (or we mis-parsed it)
/// rather than trusted -- mirrors the C agent's own `> 2023-11-14` sanity
/// check on the resulting epoch.
const PLAUSIBLE_EPOCH_FLOOR: u64 = 1_700_000_000;

/// riscv32imc has no atomic CAS/RMW (no 'A' extension), and no native 64-bit
/// atomics at all -- a `critical_section`-guarded cell instead of
/// `AtomicU64`/`AtomicBool`, same reasoning as `status.rs`'s `STATUS`.
#[derive(Clone, Copy)]
struct Sync {
    epoch_at_sync_s: u64,
    mono_at_sync_us: u64,
}

static SYNC: Mutex<RefCell<Option<Sync>>> = Mutex::new(RefCell::new(None));
static FIRST_SYNC: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Whether SNTP has converged at least once since boot.
pub fn is_set() -> bool {
    critical_section::with(|cs| SYNC.borrow(cs).borrow().is_some())
}

/// Current wall-clock time, Unix epoch seconds UTC, if [`is_set`]. `None`
/// before the first successful sync -- callers needing the "clock_unsynced"
/// distress signal from the contract (§5) should treat that as the trigger,
/// not a zero/placeholder timestamp.
pub fn now() -> Option<u64> {
    let sync = critical_section::with(|cs| *SYNC.borrow(cs).borrow())?;
    let elapsed_us = Instant::now().as_micros().saturating_sub(sync.mono_at_sync_us);
    Some(sync.epoch_at_sync_s + elapsed_us / 1_000_000)
}

/// Blocks until the first sync completes or `timeout` elapses. Returns
/// `true` immediately if already synced from an earlier call.
pub async fn wait(timeout: Duration) -> bool {
    if is_set() {
        return true;
    }
    with_timeout(timeout, FIRST_SYNC.wait()).await.is_ok()
}

/// Resyncs forever: `RESYNC_PERIOD` apart on success, `RETRY_PERIOD` after a
/// failure (DNS hiccup, server unreachable...). Never panics -- a failed
/// sync just means [`now`] keeps returning a stale (or absent) value, not a
/// crashed device; the contract's own "silence is worse than being wrong"
/// principle (§2) applies here too.
#[embassy_executor::task]
pub async fn sync_task(stack: Stack<'static>) -> ! {
    let mut first_sync_done = false;
    loop {
        match sync_once(stack).await {
            Ok(epoch) => {
                let sync = Sync { epoch_at_sync_s: epoch, mono_at_sync_us: Instant::now().as_micros() };
                critical_section::with(|cs| *SYNC.borrow(cs).borrow_mut() = Some(sync));
                if !first_sync_done {
                    first_sync_done = true;
                    FIRST_SYNC.signal(());
                }
                info!("SNTP: synced, ts={epoch}");
                Timer::after(RESYNC_PERIOD).await;
            }
            Err(e) => {
                warn!("SNTP: sync failed: {e:?}");
                Timer::after(RETRY_PERIOD).await;
            }
        }
    }
}

// Fields are read via the derived `Debug` impl (`warn!("... {e:?}")`), which
// rustc's dead-code lint doesn't count as a use.
#[derive(Debug)]
#[allow(dead_code)]
enum SyncError {
    Dns(embassy_net::dns::Error),
    NoAddress,
    Bind(embassy_net::udp::BindError),
    Ntp(sntpc::Error),
    Implausible(u64),
}

async fn sync_once(stack: Stack<'static>) -> Result<u64, SyncError> {
    let addrs = stack
        .dns_query(NTP_SERVER, DnsQueryType::A)
        .await
        .map_err(SyncError::Dns)?;
    let addr: IpAddr = (*addrs.first().ok_or(SyncError::NoAddress)?).into();

    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buffer = [0u8; 128];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_buffer = [0u8; 128];
    let mut socket =
        UdpSocket::new(stack, &mut rx_meta, &mut rx_buffer, &mut tx_meta, &mut tx_buffer);
    socket.bind(123).map_err(SyncError::Bind)?;
    let socket = UdpSocketWrapper::new(socket);

    let context = NtpContext::new(EmbassyTimestampGenerator::default());
    let result = get_time(SocketAddr::from((addr, 123)), &socket, context)
        .await
        .map_err(SyncError::Ntp)?;

    let epoch = result.sec();
    if epoch < PLAUSIBLE_EPOCH_FLOOR {
        return Err(SyncError::Implausible(epoch));
    }
    Ok(epoch)
}
