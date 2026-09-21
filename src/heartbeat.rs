//! Heartbeat outbound flow (contrat §5). Device-initiated POST every 5s to
//! `ctrl_url + /v1alpha1/heartbeat`, so it keeps working even for a device
//! that never accepts inbound connections. Silent (skips the tick) while
//! `ctrl_url` is empty -- nothing provisioned yet, nothing to talk to.
//!
//! HTTPS via `tls::connect_client` (contrat §5: "le scheme est forcé en
//! https:// indépendamment du scheme stocké dans ctrl_url"). Hand-rolled
//! HTTP/1.1 request/response instead of `reqwless`: `reqwless`'s own TLS
//! support is hard-wired to a different crate (`embedded-tls`), and running
//! two separate TLS stacks in the same firmware just to keep `reqwless`
//! costs more flash than writing this one POST by hand costs in code.
//! Stays silent (no heartbeat sent, tick skipped) until a CA is configured
//! via `POST /v1alpha1/tls/ca` -- see `tls::ClientTlsError::NoCa`.

use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;

use embassy_net::Stack;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use serde::Serialize;

use crate::agent;
use crate::storage::SharedStorage;
use crate::tls::TlsReferenceStatic;

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
    #[serde(skip_serializing_if = "String::is_empty")]
    deployment_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    firmware_digest: String,
    ota_validated: bool,
    config_generation: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    uptime_ms: u64,
    heap_free: u32,
    temp_celsius: f32,
    task_hwm_min: u32,
}

/// `ctrl_url` is `scheme://host[:port]` (the scheme itself is ignored --
/// contrat forces HTTPS regardless of what's stored). `None` on a malformed
/// value rather than panicking this task.
fn split_host_port(ctrl_url: &str) -> Option<(&str, u16)> {
    let rest = ctrl_url.split_once("://").map(|(_, rest)| rest).unwrap_or(ctrl_url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    match host_port.split_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host, port.parse().unwrap_or(443))),
        _ if !host_port.is_empty() => Some((host_port, 443)),
        _ => None,
    }
}

#[embassy_executor::task]
pub async fn run(stack: Stack<'static>, storage: &'static SharedStorage, tls: TlsReferenceStatic) -> ! {
    // Declared once outside the reconnect loop, like `http::run`'s own
    // buffers -- reused across every tick's short-lived connection. These
    // are the raw TCP socket's own buffers (the ciphertext in transit),
    // not the application JSON buffer (`send`'s own 64-byte read further
    // down) -- 512B/512B is generous for a POST whose body is a few
    // hundred bytes either way; a TLS record larger than this just crosses
    // it over more `read`/`write` round trips instead of failing, so this
    // is a throughput/RAM tradeoff, not a correctness one, and throughput
    // doesn't matter for a 5s-period heartbeat. Being local (not `static`)
    // is what makes reducing them actually shrink this task's `Future`.
    let mut rx_buffer = [0u8; 512];
    let mut tx_buffer = [0u8; 512];

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
            if let Some((host, port)) = split_host_port(&ctrl_url)
                && let Ok(host_c) = CString::new(host)
            {
                send(tls, stack, storage, &mut rx_buffer, &mut tx_buffer, &host_c, port).await;
            }
        }
        Timer::after(PERIOD).await;
    }
}

async fn send(
    tls: TlsReferenceStatic,
    stack: Stack<'static>,
    storage: &'static SharedStorage,
    rx_buffer: &mut [u8],
    tx_buffer: &mut [u8],
    host: &core::ffi::CStr,
    port: u16,
) {
    let mut session = match crate::tls::connect_client(tls, stack, storage, rx_buffer, tx_buffer, host, port).await {
        Ok(session) => session,
        Err(e) => {
            warn!("heartbeat: connect to {host:?}:{port} failed: {e}");
            return;
        }
    };

    let node_id = agent::node_id(storage).await;
    let ip = stack.config_v4().map(|c| format!("{}", c.address.address())).unwrap_or_default();

    // `connect_client` refuses to connect before SNTP has converged (TLS
    // date validation fails closed), so the clock is set by here and the
    // contrat §5 `clock_unsynced` distress channel can no longer be sent:
    // an unsynced device is silent, and the log below says why.
    let ts = crate::time::now().unwrap_or_default();
    let reason = None;

    let state = agent::state();
    let body = Heartbeat {
        node_id: &node_id,
        ip: &ip,
        ts,
        state: state.as_str(),
        deployment_id: crate::ota::active_deployment_id(storage).await,
        firmware_digest: crate::ota::active_digest(storage).await,
        // contrat §3: distinguishes `pending_verify` (false) from every
        // other state -- the only state a heartbeat is still sent from
        // before `mark_valid` has run.
        ota_validated: state != agent::State::PendingVerify,
        config_generation: storage.lock().await.cfg_generation(),
        reason,
        uptime_ms: Instant::now().as_millis(),
        heap_free: esp_alloc::HEAP.free() as u32,
        temp_celsius: TEMP_UNAVAILABLE,
        task_hwm_min: crate::stack_usage::free_bytes(),
    };
    let Ok(json) = serde_json::to_string(&body) else {
        return;
    };

    let host_str = host.to_str().unwrap_or("");
    let request = format!(
        "POST /v1alpha1/heartbeat HTTP/1.1\r\n\
         Host: {host_str}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {json}",
        json.len()
    );
    if let Err(e) = session.write(request.as_bytes()).await {
        warn!("heartbeat: POST to {host_str} failed: {e}");
        return;
    }
    if let Err(e) = session.flush().await {
        warn!("heartbeat: POST to {host_str} failed to flush: {e}");
        return;
    }

    // Only the status line is worth reading -- fire-and-forget, contrat
    // doesn't have the agent act on the response body.
    let mut buf = [0u8; 64];
    match session.read(&mut buf).await {
        Ok(n) => {
            let status = core::str::from_utf8(&buf[..n]).unwrap_or("").lines().next().unwrap_or("");
            info!("heartbeat: POST {host_str}/v1alpha1/heartbeat -> {status}");
        }
        Err(e) => warn!("heartbeat: reading response from {host_str} failed: {e}"),
    }
    let _ = session.close().await;
}
