//! Tiny HTTP config server, reachable once the device is on Wi-Fi -- see the
//! "Visit Device" link ESP Web Tools shows after Improv Wi-Fi provisioning
//! succeeds (`src/provisioning.rs`). Built with `picoserve`, an async
//! no_std HTTP server for `embassy-net`. Markup lives alongside this file
//! (`index.html`, `reboot.html`, `locked.html`); styling is `web/style.css`,
//! shared with the flashing page (see the `STYLE_CSS` constant below).
//!
//! Deliberately doesn't use picoserve's `AppBuilder`/`State` extractor
//! machinery: that's for routers whose *type* needs to be nameable (passed
//! as a task parameter, pooled across connections), which needs nightly
//! Rust (`#![feature(impl_trait_in_assoc_type)]`) to spell out. Ours doesn't
//! need that -- it's built fresh, once, inside this one task, and its
//! handlers just capture `storage` directly as a plain closure variable, so
//! this crate stays on stable Rust.

use alloc::format;
use alloc::string::String;
use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::{Rtc, RwdtStage, RwdtStageAction};
use log::warn;
use picoserve::extract::Form;
use picoserve::io::Socket;
use picoserve::response::{File, Response, StatusCode};
use picoserve::routing::{PathRouter, get, get_service};
use static_cell::StaticCell;

use crate::agent;
use crate::storage::SharedStorage;

const INDEX_TEMPLATE: &str = include_str!("index.html");
const REBOOT_PAGE: &str = include_str!("reboot.html");
const LOCKED_PAGE: &str = include_str!("locked.html");
// Shared with web/index.html (the flashing page), so both look consistent
// -- one canonical file instead of a copy that could drift.
const STYLE_CSS: &str = include_str!("../../web/style.css");

/// Highest usable GPIO number on this chip. Update when this firmware
/// targets a chip other than ESP32-C3.
const MAX_GPIO: u8 = 21;
/// Sent by the form in place of a real pin number to mean "no status LED".
const LED_DISABLED: u8 = 255;

#[derive(serde::Deserialize)]
struct HardwareForm {
    led_gpio: u8,
}

fn page(current: Option<u8>, message: Option<(&str, bool)>) -> String {
    let mut options = String::new();
    let _ = write!(
        options,
        "<option value=\"{LED_DISABLED}\"{}>D\u{e9}sactiv\u{e9}e</option>",
        if current.is_none() { " selected" } else { "" }
    );
    for gpio in 0..=MAX_GPIO {
        let _ = write!(
            options,
            "<option value=\"{gpio}\"{}>{gpio}</option>",
            if current == Some(gpio) { " selected" } else { "" }
        );
    }

    let message = match message {
        Some((text, is_error)) => {
            let class = if is_error { "message error" } else { "message" };
            format!("<p class=\"{class}\">{text}</p>")
        }
        None => String::new(),
    };

    INDEX_TEMPLATE
        .replace("{{OPTIONS}}", &options)
        .replace("{{MESSAGE}}", &message)
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
async fn reboot_after_delay(lpwr: LPWR<'static>) -> ! {
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

#[embassy_executor::task]
pub async fn run(
    stack: Stack<'static>,
    storage: &'static SharedStorage,
    spawner: Spawner,
    lpwr: LPWR<'static>,
) -> ! {
    // `lpwr` (a non-`Copy` owned peripheral) must move into
    // `reboot_after_delay` exactly once, but the `/reboot` handler closure
    // below can run more than once -- parking it behind a lock it can
    // `take()` from is simpler than trying to move it directly.
    static LPWR_CELL: StaticCell<Mutex<CriticalSectionRawMutex, Option<LPWR<'static>>>> =
        StaticCell::new();
    let lpwr_cell = &*LPWR_CELL.init(Mutex::new(Some(lpwr)));

    let router = picoserve::Router::new()
        .route("/style.css", get_service(File::css(STYLE_CSS)))
        .route(
            "/",
            get(move || async move {
                let mut storage = storage.lock().await;
                if storage.is_locked() {
                    return Response::new(StatusCode::LOCKED, String::from(LOCKED_PAGE))
                        .with_content_type("text/html; charset=utf-8");
                }
                let current = storage.load_led_gpio();
                Response::ok(page(current, None)).with_content_type("text/html; charset=utf-8")
            })
            .post(move |Form(form): Form<HardwareForm>| async move {
                let mut storage = storage.lock().await;
                if storage.is_locked() {
                    return Response::new(StatusCode::LOCKED, String::from(LOCKED_PAGE))
                        .with_content_type("text/html; charset=utf-8");
                }
                let gpio = (form.led_gpio != LED_DISABLED).then_some(form.led_gpio);
                if gpio.is_some_and(|gpio| gpio > MAX_GPIO) {
                    return Response::new(
                        StatusCode::BAD_REQUEST,
                        page(None, Some(("Broche hors plage pour cette puce.", true))),
                    )
                    .with_content_type("text/html; charset=utf-8");
                }
                storage.save_led_gpio(gpio);
                Response::ok(page(gpio, Some(("Enregistr\u{e9}.", false))))
                    .with_content_type("text/html; charset=utf-8")
            }),
        )
        .route(
            "/lock",
            picoserve::routing::post(move || async move {
                storage.lock().await.lock();
                Response::ok(LOCKED_PAGE).with_content_type("text/html; charset=utf-8")
            }),
        )
        .route(
            "/reboot",
            picoserve::routing::post(move || async move {
                // `take()`s `None` on a second concurrent hit -- one pending
                // reboot is enough, and there's only one `lpwr` to give out.
                if let Some(lpwr) = lpwr_cell.lock().await.take()
                    && let Ok(token) = reboot_after_delay(lpwr)
                {
                    spawner.spawn(token);
                }
                Response::ok(REBOOT_PAGE).with_content_type("text/html; charset=utf-8")
            }),
        )
        // Embewi contract v1alpha1 (contrat §4). First endpoint of the
        // inbound API; more will grow alongside it under this same prefix.
        .route(
            "/v1alpha1/info",
            get(move |agent::Bearer(token): agent::Bearer| async move {
                if !agent::is_authorized(storage, token.as_deref().unwrap_or("")).await {
                    return Response::new(
                        StatusCode::UNAUTHORIZED,
                        String::from("{\"error\":\"unauthorized\"}"),
                    )
                    .with_content_type("application/json");
                }
                let body = serde_json::to_string(&agent::info(storage).await).unwrap_or_default();
                Response::ok(body).with_content_type("application/json")
            }),
        );

    let config = picoserve::Config::const_default().keep_connection_alive();
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 1024];
    let mut http_buffer = [0u8; 2048];

    // Accepts the raw TCP connection ourselves instead of calling picoserve's
    // `listen_and_serve` (which is hardcoded to `embassy_net::tcp::TcpSocket`
    // internally). `serve_connection` below only needs a
    // `picoserve::io::Socket`, so this is the one place a TLS layer will
    // plug in later -- wrap `socket` in a `Socket`-implementing TLS stream
    // before handing it to `serve_connection`, and nothing in the router or
    // handlers above has to change.
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        if let Err(e) = socket.accept(80).await {
            warn!("HTTP: accept failed: {e:?}");
            continue;
        }
        socket.set_keep_alive(Some(Duration::from_secs(30)));
        socket.set_timeout(Some(Duration::from_secs(45)));

        if let Err(e) = serve_connection(&router, &config, &mut http_buffer, socket).await {
            warn!("HTTP: connection error: {e:?}");
        }
    }
}

/// Serves one already-connected socket to completion. Generic over
/// [`picoserve::io::Socket`] rather than a concrete transport, so plugging in
/// TLS later is a matter of handing this a TLS-wrapped socket instead of a
/// bare [`TcpSocket`] -- the router and every handler above are unaware of
/// the transport either way.
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
