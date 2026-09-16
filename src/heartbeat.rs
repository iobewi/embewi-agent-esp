//! Heartbeat outbound flow (contrat §5). Device-initiated POST every 5s to
//! `ctrl_url + /v1alpha1/heartbeat`, so it keeps working even for a device
//! that never accepts inbound connections. Silent (skips the tick) while
//! `ctrl_url` is empty -- nothing provisioned yet, nothing to talk to.
//!
//! Plain HTTP for now, not HTTPS: the contract forces `https://` regardless
//! of the scheme stored in `ctrl_url` (§5), but this agent has no TLS
//! client yet -- same gap as the inbound server's HTTPS requirement (see
//! `http/mod.rs`'s doc comment and the frozen `spike/mbedtls-rs-bleeding-edge`
//! branch). Uses `ctrl_url` exactly as provisioned instead of silently
//! claiming a security property it can't deliver.

use alloc::format;

use embassy_net::Stack;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use reqwless::client::HttpClient;
use reqwless::headers::ContentType;
use reqwless::request::{Method, RequestBuilder};
use serde::Serialize;
use static_cell::StaticCell;

use crate::agent;
use crate::storage::SharedStorage;

const PERIOD: Duration = Duration::from_secs(5);
/// Sentinel meaning "no temperature sensor wired up" (contrat §5) -- always
/// reported for now, this agent doesn't read the SoC's internal sensor yet.
const TEMP_UNAVAILABLE: f32 = -127.0;

#[derive(Serialize)]
struct Heartbeat<'a> {
    node_id: &'a str,
    ip: &'a str,
    ts: u64,
    state: &'static str,
    ota_validated: bool,
    config_generation: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    uptime_ms: u64,
    heap_free: u32,
    temp_celsius: f32,
}

#[embassy_executor::task]
pub async fn run(stack: Stack<'static>, storage: &'static SharedStorage) -> ! {
    static TCP_STATE: StaticCell<TcpClientState<1, 1024, 1024>> = StaticCell::new();
    let tcp_state = TCP_STATE.init(TcpClientState::new());
    let tcp_client = TcpClient::new(stack, tcp_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp_client, &dns);

    let mut last_ctrl_url_empty = None;
    loop {
        let ctrl_url = agent::ctrl_url(storage).await;
        if ctrl_url.is_empty() {
            // Logged once on the empty<->non-empty transition, not every
            // tick: contrat §2's "silence is worse than wrong" is about the
            // outbound wire, not the serial console, and five seconds of
            // log spam for the entire, possibly permanent, unprovisioned
            // lifetime of a device would drown out everything else.
            if last_ctrl_url_empty != Some(true) {
                info!("heartbeat: ctrl_url not provisioned, staying quiet");
            }
            last_ctrl_url_empty = Some(true);
        } else {
            if last_ctrl_url_empty != Some(false) {
                info!("heartbeat: ctrl_url={ctrl_url}, sending every {PERIOD:?}");
            }
            last_ctrl_url_empty = Some(false);
            send(&mut client, stack, storage, &ctrl_url).await;
        }
        Timer::after(PERIOD).await;
    }
}

async fn send(
    client: &mut HttpClient<'_, TcpClient<'_, 1, 1024, 1024>, DnsSocket<'_>>,
    stack: Stack<'static>,
    storage: &'static SharedStorage,
    ctrl_url: &str,
) {
    let node_id = agent::node_id(storage).await;
    let ip = stack
        .config_v4()
        .map(|c| format!("{}", c.address.address()))
        .unwrap_or_default();

    // Canal de détresse (contrat §5): tant que SNTP n'a pas convergé, `ts`
    // ne vaut que l'uptime et le heartbeat porte `reason: "clock_unsynced"`
    // pour que le Core distingue ce cas d'un vrai silence.
    let (ts, reason) = match crate::time::now() {
        Some(ts) => (ts, None),
        None => (Instant::now().as_secs(), Some("clock_unsynced")),
    };

    let body = Heartbeat {
        node_id: &node_id,
        ip: &ip,
        ts,
        state: "running",
        // No OTA subsystem yet: the device was never put into
        // pending_verify by an OTA cycle, so it's as validated as it'll
        // ever be until that phase lands.
        ota_validated: true,
        config_generation: 0,
        reason,
        uptime_ms: Instant::now().as_millis(),
        heap_free: esp_alloc::HEAP.free() as u32,
        temp_celsius: TEMP_UNAVAILABLE,
    };
    let Ok(json) = serde_json::to_vec(&body) else {
        return;
    };

    let url = format!("{ctrl_url}/v1alpha1/heartbeat");
    let mut rx_buf = [0u8; 256];
    let request = match client.request(Method::POST, &url).await {
        Ok(request) => request,
        Err(e) => {
            warn!("heartbeat: connect to {ctrl_url} failed: {e:?}");
            return;
        }
    };
    match request
        .body(&json[..])
        .content_type(ContentType::ApplicationJson)
        .send(&mut rx_buf)
        .await
    {
        Ok(response) => info!("heartbeat: POST {url} -> {:?}", response.status),
        Err(e) => warn!("heartbeat: POST {url} failed: {e:?}"),
    }
}
