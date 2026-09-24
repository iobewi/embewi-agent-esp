//! Admin HTTP surface, reachable once the device is on Wi-Fi. Built with
//! `picoserve`, an async no_std HTTP server for `embassy-net`. Split into
//! two independent tasks/routers, spawned by `wifi.rs` -- never both in the
//! same boot, see that module for the `is_locked()` dispatch:
//!
//! - [`config`]: the one-shot provisioning UI (`index.html`/`confirm.html`/
//!   `locked.html`), reachable only before the device is locked.
//! - [`api`]: the contrat v1alpha1 JSON API (`/v1alpha1/*`), reachable only
//!   after the device is locked.
//!
//! **Why two tasks, not one router with both.** Every local variable an
//! `async fn` holds across an `.await` point -- including a `picoserve`
//! router, which owns every route's handler closure -- becomes part of the
//! fixed-size `Future` `#[embassy_executor::task]` bakes into a `'static`
//! `TaskStorage` (confirmed on our own binary: `http::run::POOL` was a
//! constant ~9 KiB reserved in `.bss`, `RAM`, for the *whole program's
//! lifetime*, regardless of whether the config page was ever hit again).
//! One task whose router held both surfaces paid for the config page's
//! closures forever, even on a device that had been locked for months. Two
//! smaller tasks each only pay for their own router. It's also a real
//! security improvement, not just a memory one: once locked, the
//! provisioning handlers and the templates behind them are *structurally*
//! unreachable -- there's no router path to them at all, not just a
//! runtime `is_locked()` check guarding one.
//!
//! **Two separate planes, two separate ports.** This is the
//! admin/Kubernetes-facing plane -- always on [`ADMIN_PORT_HTTP`]/
//! [`ADMIN_PORT_HTTPS`], never on `agent::app_port`. `app_port` (`POST
//! /v1alpha1/app/port`) is the TCP port of a *separate* business-layer
//! service the contract describes ("le port TCP du service applicatif
//! embarqué... ex. serveur REST de l'application métier"), which doesn't
//! exist as a distinct process in this single-binary agent yet -- until it
//! does, `app_port` is just an NVS value `GET /info` reports, with nothing
//! actually listening on it. Binding the admin plane to `app_port` instead
//! of a fixed port was a real bug: `app_port`'s own contractual range
//! (1024-65535) then made it impossible to bring the admin plane back
//! below 1024 once moved, and conflated a device management concern with
//! an application concern that must stay independent.
//!
//! Deliberately doesn't use picoserve's `AppBuilder`/`State` extractor
//! machinery: that's for routers whose *type* needs to be nameable (passed
//! as a task parameter, pooled across connections), which needs nightly
//! Rust (`#![feature(impl_trait_in_assoc_type)]`) to spell out. Ours
//! doesn't need that -- each router is built fresh, once, inside its own
//! task, and handlers just capture `storage` directly as a plain closure
//! variable, so this crate stays on stable Rust.

use alloc::string::String;

use embassy_executor::Spawner;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::{Rtc, RwdtStage, RwdtStageAction};
use log::warn;
use picoserve::io::Socket;
use picoserve::response::{ContentBody, ContentHeaders, Response, StatusCode};
use picoserve::routing::PathRouter;

use crate::storage::SharedStorage;

pub mod api;
pub mod config;

/// The one admin task spawned by `ApplicationSupervisor`: picks [`config::serve`] or
/// [`api::serve`] based on whether the device is locked yet, and never
/// switches mid-boot (a successful provisioning save reboots the device,
/// so the next boot's `run` re-reads `is_locked()` fresh). Deliberately one
/// task calling two plain functions rather than two
/// `#[embassy_executor::task]`s -- see this module's doc comment for why
/// that matters for RAM, confirmed on our own binary: two separate tasks
/// reserved ~16.8 KiB combined (`config::run::POOL` + `api::run::POOL`,
/// both permanently, since a `#[task]`'s `TaskStorage` exists in `.bss`
/// whether or not it's ever spawned this boot); one task whose body
/// branches between the two reserves the size of whichever branch is
/// larger (~8.9 KiB here), because the two `.await`s sit in mutually
/// exclusive arms of the same generated state machine instead of two
/// independent statics.
#[embassy_executor::task]
pub async fn run(
    stack: Stack<'static>,
    storage: &'static SharedStorage,
    agent_config: &'static crate::agent::AgentConfigSpace,
    app_config: &'static crate::app_config::AppConfigSpace,
    hardware_config: &'static crate::hardware::HardwareConfigSpace,
    tls_config: &'static crate::tls::TlsConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    lifecycle_config: &'static crate::lifecycle::LifecycleConfigSpace,
    ota_config: &'static crate::ota::OtaConfigSpace,
    spawner: Spawner,
    lpwr: LPWR<'static>,
    tls: crate::tls::TlsReferenceStatic,
) -> ! {
    if crate::lifecycle::is_locked(lifecycle_config).await {
        api::serve(stack, storage, agent_config, app_config, tls_config, runtime_config, ota_config, spawner, lpwr, tls).await
    } else {
        config::serve(stack, agent_config, hardware_config, tls_config, lifecycle_config, spawner, lpwr, tls).await
    }
}

// Shared with web/index.html (the flashing page), so both look consistent
// -- one canonical file instead of a copy that could drift.
const STYLE_CSS: &str = include_str!("../../web/style.css");

/// Fixed ports for the admin plane -- see this module's doc comment for why
/// they're never `agent::app_port`. Conventional HTTP/HTTPS split (matches
/// `firmware-c`'s own `esp_https_server` default of `:443`), not a single
/// port serving both: plain HTTP until a certificate is configured
/// (bootstrap), `:443` once one is -- `:80` stops listening at that point
/// rather than continuing to serve the admin API in clear text next to it.
const ADMIN_PORT_HTTP: u16 = 80;
const ADMIN_PORT_HTTPS: u16 = 443;

/// Bound on the TLS handshake specifically, well short of the 45 s socket
/// idle timeout below (which covers the *whole* connection, handshake
/// included, but is meant to tolerate a slow legitimate client mid-request,
/// not a stalled/adversarial handshake). This server handles one connection
/// at a time (this module's own doc comment): without this, a client that
/// opens the TCP connection and then stalls the handshake (a partial/absent
/// `ClientHello`, or bytes dripped just fast enough to keep resetting the
/// socket's own idle timer) can hold the only admin connection for up to
/// that 45 s on every single attempt, locking out legitimate Core traffic.
/// 5 s is generous for a real ECDHE handshake on this CPU (~0.5-1 s
/// observed) with LAN-grade margin.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Escapes `&`/`<`/`>`/`"` so admin-supplied text reflected back into
/// `value="..."` attributes can't break out of the attribute or inject
/// markup.
pub(super) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Named explicitly (not `impl IntoResponse`): every `/v1alpha1/*` handler
/// branches between this and [`json_error`]/[`unauthorized`], and separate
/// `impl Trait` return sites never unify even when the concrete type
/// matches -- picoserve's own `Response::ok`/`::new` already resolve to
/// this same `Response<ContentHeaders, ContentBody<String>>` either way.
pub(super) type JsonResponse = Response<ContentHeaders, ContentBody<String>>;

pub(super) fn json_ok(body: String) -> JsonResponse {
    Response::ok(body).with_content_type("application/json")
}

pub(super) fn json_error(status: StatusCode, body: &str) -> JsonResponse {
    Response::new(status, String::from(body)).with_content_type("application/json")
}

/// Shared by every `/v1alpha1/*` handler (contrat §4b: `401
/// {"error":"unauthorized"}`).
pub(super) fn unauthorized() -> JsonResponse {
    json_error(StatusCode::UNAUTHORIZED, "{\"error\":\"unauthorized\"}")
}

/// Gives the response time to actually reach the socket before resetting --
/// calling a reset directly from the request handler would cut the
/// connection before picoserve ever writes the confirmation page.
///
/// Uses the RTC watchdog (`ResetSystem`, the broadest of the three reset
/// scopes esp-hal exposes) instead of `esp_hal::system::software_reset()`.
/// That function only does a "digital core" reset, which on this chip
/// leaves the native USB-Serial-JTAG peripheral's link state untouched: the
/// host still sees the old USB session, the freshly-booted firmware expects
/// a new one, and Improv Serial stops responding correctly until a real
/// (EN-pin/RTS-triggered) reset -- exactly what ESP Web Tools itself always
/// does when it resets the board, which is why that path never showed this.
#[embassy_executor::task]
pub(super) async fn reboot_after_delay(lpwr: LPWR<'static>) -> ! {
    Timer::after(Duration::from_millis(500)).await;
    let mut rtc = Rtc::new(lpwr);
    rtc.rwdt
        .set_timeout(RwdtStage::Stage0, esp_hal::time::Duration::from_millis(100));
    rtc.rwdt.set_stage_action(RwdtStage::Stage0, RwdtStageAction::ResetSystem);
    rtc.rwdt.enable();
    loop {
        Timer::after(Duration::from_secs(10)).await;
    }
}

/// Drives the accept loop for one router (`config` or `api`) to completion
/// -- generic over [`PathRouter`] rather than duplicated per caller, but
/// still monomorphized (and so given its own, separately-sized `Future`)
/// once per concrete router type, which is the whole point of splitting
/// `config`/`api` into two tasks in the first place (see this module's doc
/// comment).
pub(super) async fn serve(
    stack: Stack<'static>,
    tls_config: &'static crate::tls::TlsConfigSpace,
    tls: crate::tls::TlsReferenceStatic,
    router: &picoserve::Router<impl PathRouter>,
) -> ! {
    let config = picoserve::Config::const_default().keep_connection_alive();
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 1024];
    let mut http_buffer = [0u8; 2048];

    // Accepts the raw TCP connection ourselves instead of calling picoserve's
    // `listen_and_serve` (which is hardcoded to `embassy_net::tcp::TcpSocket`
    // internally). `serve_connection` below only needs a
    // `picoserve::io::Socket`, so plugging in TLS never touches the router
    // or handlers.
    loop {
        // Decided once per accept: which port to listen on for *this*
        // connection attempt, and (if HTTPS) the config to serve it with --
        // a single `server_config` call reused for both, rather than
        // re-parsing the PEM pair again after accepting. A cert pushed
        // while this `accept()` is already blocked only takes effect from
        // the next loop iteration onward (see `ADMIN_PORT_HTTPS`'s doc
        // comment) -- acceptable: that push itself completes over the
        // current HTTP connection, which ends this iteration anyway.
        let tls_server_config = crate::tls::server_config(tls_config).await;
        let port = if tls_server_config.is_some() { ADMIN_PORT_HTTPS } else { ADMIN_PORT_HTTP };

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        if let Err(e) = socket.accept(port).await {
            warn!("HTTP: accept failed: {e:?}");
            continue;
        }
        socket.set_keep_alive(Some(Duration::from_secs(30)));
        socket.set_timeout(Some(Duration::from_secs(45)));

        match tls_server_config {
            Some(tls_config) => {
                let mut session = match esp_hal_mbedtls::mbedtls_rs::Session::new(tls, socket, &tls_config) {
                    Ok(session) => session,
                    Err(e) => {
                        warn!("HTTPS: session setup failed: {e}");
                        continue;
                    }
                };
                match with_timeout(HANDSHAKE_TIMEOUT, session.connect()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!("HTTPS: handshake failed: {e}");
                        continue;
                    }
                    Err(_) => {
                        warn!("HTTPS: handshake timed out after {HANDSHAKE_TIMEOUT:?}");
                        continue;
                    }
                }
                if let Err(e) =
                    serve_connection(router, &config, &mut http_buffer, crate::tls::TlsSocket::new(session)).await
                {
                    warn!("HTTPS: connection error: {e:?}");
                }
            }
            None => {
                if let Err(e) = serve_connection(router, &config, &mut http_buffer, socket).await {
                    warn!("HTTP: connection error: {e:?}");
                }
            }
        }
    }
}

/// Serves one already-connected socket to completion. Generic over
/// [`picoserve::io::Socket`] rather than a concrete transport, so plugging in
/// TLS is a matter of handing this a TLS-wrapped socket instead of a bare
/// [`TcpSocket`] -- the router and every handler are unaware of the
/// transport either way.
async fn serve_connection<S: Socket<picoserve::EmbassyRuntime>>(
    router: &picoserve::Router<impl PathRouter>,
    config: &picoserve::Config,
    http_buffer: &mut [u8],
    socket: S,
) -> Result<picoserve::DisconnectionInfo<picoserve::NoGracefulShutdown>, picoserve::Error<S::Error>>
{
    picoserve::Server::new(router, config, http_buffer)
        .serve(socket)
        .await
}
