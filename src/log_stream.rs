//! ESP_LOGx streaming to the Core over an outbound client WebSocket
//! (contrat §5). Captures every log line through a custom [`log::Log`]
//! implementation installed in place of `esp_println`'s own (which exposes
//! no hook to duplicate lines through -- see [`install`]), buffers them in
//! a small ring, and a background task drains the ring into
//! `wss://ctrl_url_host:port/v1alpha1/logs`.
//!
//! The WS handshake is hand-rolled directly over a `tls::connect_client`
//! session instead of going through `edge_http::io::client::Connection`:
//! that type is tied to `edge_nal::TcpConnect`, which a TLS stream would
//! need four extra trait impls (`TcpConnect`/`TcpSplit`/`Readable`/
//! `TcpShutdown`) to satisfy, for no benefit over building the (tiny, fixed
//! -shape) upgrade request/response by hand. `edge_http::ws`'s header
//! helpers (`upgrade_request_headers`/`is_upgrade_accepted`) are still
//! reused -- they're plain functions, not tied to `Connection` either.
//! Frame-level I/O (`edge_ws`) is unchanged: it already worked over any
//! `embedded-io-async` stream, TLS session included.
//!
//! Best-effort by design (contrat §5, "on préfère perdre des logs plutôt
//! que bloquer le système"): a full ring drops the newest line rather than
//! blocking whoever is logging, and a disconnected/reconnecting socket
//! drains-and-discards rather than backlogging -- streaming *recent* logs
//! on reconnect beats replaying a stale backlog.

use alloc::ffi::CString;
use alloc::format;
use alloc::string::String as AllocString;
use core::cell::RefCell;
use core::fmt::Write as _;

use critical_section::Mutex;
use edge_http::ws::{is_upgrade_accepted, upgrade_request_headers, MAX_BASE64_KEY_LEN, MAX_BASE64_KEY_RESPONSE_LEN, NONCE_LEN};
use edge_ws::{FrameHeader, FrameType};
use embassy_net::Stack;
use embassy_time::{with_timeout, Duration, Timer};
use heapless::{Deque, String as HString};
use log::{Level, LevelFilter, Metadata, Record, info, warn};
use serde::Serialize;

use crate::agent;
use crate::storage::SharedStorage;
use crate::tls::TlsReferenceStatic;

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
        let fits = write!(line, "{}", record.args()).is_ok();
        // Too long to fit: drop rather than send a truncated/corrupt line.
        if !fits {
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

/// `ctrl_url` is `scheme://host[:port]` (the scheme itself is ignored --
/// contrat forces `wss://` regardless of what's stored, same as
/// `heartbeat.rs`). `None` on a malformed value rather than panicking this
/// task.
fn split_host_port(ctrl_url: &str) -> Option<(&str, u16)> {
    let rest = ctrl_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(ctrl_url);
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
    // buffers -- reused across every reconnection attempt. Raw TCP socket
    // buffers (the ciphertext in transit), not the WS frame payload itself
    // (`LINE_MAX`-bounded, well under 512B) -- sized down from 2048/1024
    // the same way `heartbeat.rs`'s were, for the same reason (see that
    // module's comment): shrinks this task's `Future`, and a larger TLS
    // record just crosses over more round trips instead of failing.
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 512];

    loop {
        let ctrl_url = agent::ctrl_url(storage).await;
        if let Some((host, port)) = split_host_port(&ctrl_url)
            && let Ok(host_c) = CString::new(host)
        {
            if let Err(e) = run_session(tls, stack, storage, &mut rx_buffer, &mut tx_buffer, &host_c, port).await {
                warn!("logs: session ended: {e}");
            }
        }
        drain_and_discard();
        Timer::after(RETRY_PERIOD).await;
    }
}

async fn run_session(
    tls: TlsReferenceStatic,
    stack: Stack<'static>,
    storage: &'static SharedStorage,
    rx_buffer: &mut [u8],
    tx_buffer: &mut [u8],
    host: &core::ffi::CStr,
    port: u16,
) -> Result<(), AllocString> {
    let mut session = crate::tls::connect_client(tls, stack, storage, rx_buffer, tx_buffer, host, port)
        .await
        .map_err(|e| format!("connect to {host:?}:{port} failed: {e}"))?;

    let host_str = host.to_str().unwrap_or("");
    let mut nonce = [0u8; NONCE_LEN];
    esp_hal::rng::Rng::new().read(&mut nonce);
    let mut key_buf = [0u8; MAX_BASE64_KEY_LEN];
    // `origin: None` isn't just "omit the header" here: edge_http's
    // `upgrade_request_headers` maps it to an empty `("", "")` tuple and
    // still emits it, producing a bare `:` line that confuses strict
    // server-side header parsers -- confirmed via `nc`, capturing the raw
    // bytes this device actually sent. Passing a real value sidesteps it.
    let origin = format!("https://{host_str}");
    let headers = upgrade_request_headers(Some(host_str), Some(&origin), None, &nonce, &mut key_buf);

    let mut request = AllocString::from("GET /v1alpha1/logs HTTP/1.1\r\n");
    for (name, value) in headers {
        if !name.is_empty() {
            let _ = write!(request, "{name}: {value}\r\n");
        }
    }
    request.push_str("\r\n");
    session
        .write(request.as_bytes())
        .await
        .map_err(|e| format!("upgrade request failed: {e}"))?;
    session.flush().await.map_err(|e| format!("upgrade request flush failed: {e}"))?;

    // Reads the upgrade response into a fixed buffer -- large enough for
    // any real server's response headers, and there's no body to worry
    // about spilling past `\r\n\r\n` (the server waits for the handshake to
    // complete before sending any WS frame).
    let mut resp_buf = [0u8; 512];
    let mut filled = 0;
    let header_end = loop {
        if filled >= resp_buf.len() {
            return Err(AllocString::from("upgrade response headers too large"));
        }
        let n = session
            .read(&mut resp_buf[filled..])
            .await
            .map_err(|e| format!("upgrade response read failed: {e}"))?;
        if n == 0 {
            return Err(AllocString::from("connection closed during upgrade"));
        }
        filled += n;
        if let Some(pos) = resp_buf[..filled].windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let mut httparse_headers = [httparse::EMPTY_HEADER; 24];
    let mut response = httparse::Response::new(&mut httparse_headers);
    response
        .parse(&resp_buf[..header_end])
        .map_err(|e| format!("upgrade response parse failed: {e:?}"))?;
    let code = response.code.ok_or_else(|| AllocString::from("upgrade response has no status code"))?;
    let header_pairs: heapless::Vec<(&str, &str), 24> = response
        .headers
        .iter()
        .filter_map(|h| core::str::from_utf8(h.value).ok().map(|v| (h.name, v)))
        .collect();
    let mut accept_buf = [0u8; MAX_BASE64_KEY_RESPONSE_LEN];
    if !is_upgrade_accepted(code, header_pairs, &nonce, &mut accept_buf) {
        return Err(AllocString::from("server did not accept the WS upgrade"));
    }

    info!("logs: connected to {host_str}:{port}");
    loop {
        // Bounded read for whatever the server sent, mainly keepalive
        // Pings: a write-only client that never reads never answers them,
        // and real WS servers (confirmed against Python's `websockets`)
        // drop the connection over that alone, RFC 6455 requiring a Pong
        // in response. `DRAIN_PERIOD` doubles as the read timeout, so this
        // doesn't delay draining the ring by more than that either way.
        match with_timeout(DRAIN_PERIOD, FrameHeader::recv(&mut session)).await {
            Ok(Ok(mut header)) => {
                let mut payload = [0u8; 125]; // RFC 6455: control frames are <= 125 bytes
                let payload = header
                    .recv_payload(&mut session, &mut payload)
                    .await
                    .map_err(|e| format!("recv_payload failed: {e:?}"))?;
                match header.frame_type {
                    FrameType::Ping => {
                        header.frame_type = FrameType::Pong;
                        header.mask_key = Some(esp_hal::rng::Rng::new().random());
                        header
                            .send(&mut session)
                            .await
                            .map_err(|e| format!("pong send failed: {e:?}"))?;
                        header
                            .send_payload(&mut session, payload)
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
            edge_ws::io::send(&mut session, FrameType::Text(false), Some(mask_key), &json)
                .await
                .map_err(|e| format!("send failed: {e:?}"))?;
        }
    }
}
