//! ESP_LOGx streaming to the Core over an outbound client WebSocket
//! (contrat §5). Captures every log line through a custom [`log::Log`]
//! implementation installed in place of `esp_println`'s own (which exposes
//! no hook to duplicate lines through -- see [`install`]), buffers them in
//! a small ring, and a background task drains the ring into
//! `wss://ctrl_url_host:port/v1alpha1/logs` (plain `ws://` for now -- same
//! TLS gap as `heartbeat.rs`, tracked there, not solved twice).
//!
//! Best-effort by design (contrat §5, "on préfère perdre des logs plutôt
//! que bloquer le système"): a full ring drops the newest line rather than
//! blocking whoever is logging, and a disconnected/reconnecting socket
//! drains-and-discards rather than backlogging -- streaming *recent* logs
//! on reconnect beats replaying a stale backlog.

use alloc::format;
use alloc::string::String as AllocString;
use core::cell::RefCell;
use core::fmt::Write as _;
use core::net::SocketAddr;

use critical_section::Mutex;
use edge_http::io::client::Connection;
use edge_http::ws::{MAX_BASE64_KEY_LEN, MAX_BASE64_KEY_RESPONSE_LEN, NONCE_LEN};
use edge_nal::{AddrType, Dns};
use edge_nal_embassy::{Dns as EmbassyDns, Tcp, TcpBuffers};
use edge_ws::{FrameHeader, FrameType};
use embassy_net::Stack;
use embassy_time::{with_timeout, Duration, Timer};
use heapless::{Deque, String as HString};
use log::{Level, LevelFilter, Metadata, Record, info, warn};
use serde::Serialize;

use crate::agent;
use crate::storage::SharedStorage;

const LINE_MAX: usize = 160;
const RING_CAPACITY: usize = 24;
const RETRY_PERIOD: Duration = Duration::from_secs(5);
const DRAIN_PERIOD: Duration = Duration::from_millis(200);

static RING: Mutex<RefCell<Deque<HString<LINE_MAX>, RING_CAPACITY>>> =
    Mutex::new(RefCell::new(Deque::new()));

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Mirrors .cargo/config.toml's ESP_LOG="warn,embewi_agent_esp=info"
        // -- hardcoded rather than re-parsed at runtime, since
        // esp_println's own env-filter machinery is generated into that
        // crate's own OUT_DIR and isn't reusable from outside it (see this
        // module's doc comment: there's no hook to chain onto its logger,
        // so this one replaces it, filter included).
        if metadata.target().starts_with("embewi_agent_esp") {
            metadata.level() <= Level::Info
        } else {
            metadata.level() <= Level::Warn
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Replaces esp_println's logger entirely -- this is the only
        // place these lines get printed locally, not a second sink next
        // to it.
        esp_println::println!("{} - {}", record.level(), record.args());

        let mut line: HString<LINE_MAX> = HString::new();
        // Too long to fit: drop rather than send a truncated/corrupt line.
        if write!(line, "{}", record.args()).is_err() {
            return;
        }
        critical_section::with(|cs| {
            let mut ring = RING.borrow(cs).borrow_mut();
            if ring.is_full() {
                return;
            }
            let _ = ring.push_back(line);
        });
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

/// Installs this module's logger in place of `esp_println`'s own. Call
/// once, at boot, instead of `esp_println::logger::init_logger_from_env()`.
pub fn install() {
    unsafe {
        let _ = log::set_logger_racy(&LOGGER);
        log::set_max_level_racy(LevelFilter::Info);
    }
}

fn pop_line() -> Option<HString<LINE_MAX>> {
    critical_section::with(|cs| RING.borrow(cs).borrow_mut().pop_front())
}

fn drain_and_discard() {
    critical_section::with(|cs| RING.borrow(cs).borrow_mut().clear());
}

#[derive(Serialize)]
struct LogFrame<'a> {
    ts: u64,
    node: &'a str,
    workload: &'static str,
    level: &'static str,
    msg: &'a str,
}

/// `ctrl_url` is `http://host[:port]` (contrat's own field, no scheme
/// dedicated to WS) -- split into what `edge_http`/DNS need. `None` if the
/// host is empty (shouldn't happen once `ctrl_url` itself is non-empty,
/// but a malformed value shouldn't panic this task).
fn split_host_port(ctrl_url: &str) -> Option<(&str, u16)> {
    let rest = ctrl_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(ctrl_url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    match host_port.split_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host, port.parse().unwrap_or(80))),
        _ if !host_port.is_empty() => Some((host_port, 80)),
        _ => None,
    }
}

#[embassy_executor::task]
pub async fn run(stack: Stack<'static>, storage: &'static SharedStorage) -> ! {
    // Local, not `static`: `edge_nal_embassy::Pool`'s internal `Cell`s make
    // it `!Sync`, so it can't live in a `static`. A plain local works fine
    // here -- `run` never returns, so it lives exactly as long as a
    // `'static` one would have, without needing the bound.
    let buffers: TcpBuffers<1> = TcpBuffers::new();
    let tcp = Tcp::new(stack, &buffers);
    let dns = EmbassyDns::new(stack);

    loop {
        let ctrl_url = agent::ctrl_url(storage).await;
        if let Some((host, port)) = split_host_port(&ctrl_url)
            && let Err(e) = run_session(&tcp, &dns, host, port, storage).await
        {
            warn!("logs: session ended: {e}");
        }
        drain_and_discard();
        Timer::after(RETRY_PERIOD).await;
    }
}

async fn run_session(
    tcp: &Tcp<'_>,
    dns: &EmbassyDns<'_>,
    host: &str,
    port: u16,
    storage: &'static SharedStorage,
) -> Result<(), AllocString> {
    let ip = dns
        .get_host_by_name(host, AddrType::IPv4)
        .await
        .map_err(|e| format!("DNS {host} failed: {e:?}"))?;
    let addr = SocketAddr::new(ip, port);

    let mut buf = [0u8; 1024];
    let mut conn: Connection<'_, Tcp<'_>> = Connection::new(&mut buf, tcp, addr);

    let mut nonce = [0u8; NONCE_LEN];
    esp_hal::rng::Rng::new().read(&mut nonce);
    let mut key_buf = [0u8; MAX_BASE64_KEY_LEN];
    // `origin: None` isn't just "omit the header" here: edge_http's
    // `upgrade_request_headers` maps it to an empty `("", "")` tuple and
    // still emits it, producing a bare `:` line that confuses strict
    // server-side header parsers -- confirmed via `nc`, capturing the raw
    // bytes this device actually sent. Passing a real value sidesteps it.
    let origin = format!("http://{host}");
    conn.initiate_ws_upgrade_request(
        Some(host),
        Some(&origin),
        "/v1alpha1/logs",
        None,
        &nonce,
        &mut key_buf,
    )
    .await
    .map_err(|e| format!("upgrade request failed: {e:?}"))?;
    conn.initiate_response()
        .await
        .map_err(|e| format!("upgrade response failed: {e:?}"))?;
    let mut resp_buf = [0u8; MAX_BASE64_KEY_RESPONSE_LEN];
    if !conn
        .is_ws_upgrade_accepted(&nonce, &mut resp_buf)
        .map_err(|e| format!("upgrade check failed: {e:?}"))?
    {
        return Err(AllocString::from("server did not accept the WS upgrade"));
    }
    conn.complete()
        .await
        .map_err(|e| format!("upgrade completion failed: {e:?}"))?;
    let (mut socket, _) = conn.release();

    info!("logs: connected to {host}:{port}");
    loop {
        // Bounded read for whatever the server sent, mainly keepalive
        // Pings: a write-only client that never reads never answers them,
        // and real WS servers (confirmed against Python's `websockets`)
        // drop the connection over that alone, RFC 6455 requiring a Pong
        // in response. `DRAIN_PERIOD` doubles as the read timeout, so this
        // doesn't delay draining the ring by more than that either way.
        match with_timeout(DRAIN_PERIOD, FrameHeader::recv(&mut socket)).await {
            Ok(Ok(mut header)) => {
                let mut payload = [0u8; 125]; // RFC 6455: control frames are <= 125 bytes
                let payload = header
                    .recv_payload(&mut socket, &mut payload)
                    .await
                    .map_err(|e| format!("recv_payload failed: {e:?}"))?;
                match header.frame_type {
                    FrameType::Ping => {
                        header.frame_type = FrameType::Pong;
                        header.mask_key = Some(esp_hal::rng::Rng::new().random());
                        header
                            .send(&mut socket)
                            .await
                            .map_err(|e| format!("pong send failed: {e:?}"))?;
                        header
                            .send_payload(&mut socket, payload)
                            .await
                            .map_err(|e| format!("pong payload send failed: {e:?}"))?;
                    }
                    FrameType::Close => {
                        return Err(AllocString::from("server closed the connection"));
                    }
                    _ => {}
                }
            }
            Ok(Err(e)) => return Err(format!("recv failed: {e:?}")),
            Err(_timeout) => {} // nothing from the server this round, as usual
        }

        while let Some(line) = pop_line() {
            let node_id = agent::node_id(storage).await;
            let ts = crate::time::now().unwrap_or(0);
            let frame =
                LogFrame { ts, node: &node_id, workload: agent::FW_NAME, level: "raw", msg: &line };
            let Ok(json) = serde_json::to_vec(&frame) else {
                continue;
            };
            let mask_key = esp_hal::rng::Rng::new().random();
            edge_ws::io::send(&mut socket, FrameType::Text(false), Some(mask_key), &json)
                .await
                .map_err(|e| format!("send failed: {e:?}"))?;
        }
    }
}
