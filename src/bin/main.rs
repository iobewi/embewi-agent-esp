#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embedded_io_async::{Read, Write};
use esp_backtrace as _;
use esp_hal::Async;
use esp_hal::clock::CpuClock;
use esp_hal::peripherals::WIFI;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagTx};
use esp_radio::wifi::{
    AuthenticationMethod, Config as WifiConfig, Interface, WifiController, scan::ScanConfig,
    sta::StationConfig,
};
use log::{info, warn};
use static_cell::StaticCell;

use embewi_agent_esp::improv::{self, Command, ImprovError, ParsedCommand, Parser, State};

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

type UsbTx = UsbSerialJtagTx<'static, Async>;

struct Wifi {
    controller: WifiController<'static>,
    stack: Stack<'static>,
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Sizes recommended by esp-radio's docs for Wi-Fi.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // Wi-Fi credentials are provisioned at runtime by ESP Web Tools over Improv
    // Serial; the radio only comes up once the browser needs it.
    let (mut rx, mut tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split();
    let mut wifi_peripheral = Some(peripherals.WIFI);
    let mut wifi: Option<Wifi> = None;
    let mut state = State::Authorized;
    let mut parser = Parser::new();
    let mut buf = [0u8; 64];

    info!("Improv: listening on USB-Serial-JTAG");

    loop {
        let n = match rx.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                warn!("USB read failed: {e:?}");
                continue;
            }
        };
        for &byte in &buf[..n] {
            if let Some(command) = parser.feed(byte) {
                handle_command(
                    command,
                    &mut tx,
                    &mut state,
                    &mut wifi,
                    &mut wifi_peripheral,
                    spawner,
                )
                .await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}

async fn send(tx: &mut UsbTx, frame: &[u8]) {
    if let Err(e) = tx.write_all(frame).await {
        warn!("USB write failed: {e:?}");
    }
}

/// Brings the radio up on first use, started in station mode so it can scan.
/// Failures are logged and return `None` rather than panicking, which would
/// halt the chip and take Improv down with it.
fn ensure_wifi<'a>(
    wifi: &'a mut Option<Wifi>,
    peripheral: &mut Option<WIFI<'static>>,
    spawner: Spawner,
) -> Option<&'a mut Wifi> {
    if wifi.is_none() {
        let (mut controller, interfaces) =
            match esp_radio::wifi::new(peripheral.take()?, Default::default()) {
                Ok(parts) => parts,
                Err(e) => {
                    warn!("Wi-Fi init failed: {e:?}");
                    return None;
                }
            };
        if let Err(e) = controller.set_config(&WifiConfig::Station(StationConfig::default())) {
            warn!("Wi-Fi start failed: {e:?}");
            return None;
        }

        static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
        let seed = esp_hal::time::Instant::now().duration_since_epoch().as_micros() as u64;
        let (stack, runner) = embassy_net::new(
            interfaces.station,
            embassy_net::Config::dhcpv4(Default::default()),
            RESOURCES.init(StackResources::new()),
            seed,
        );
        spawner.spawn(net_task(runner).unwrap());

        *wifi = Some(Wifi { controller, stack });
    }
    wifi.as_mut()
}

async fn handle_command(
    command: ParsedCommand,
    tx: &mut UsbTx,
    state: &mut State,
    wifi: &mut Option<Wifi>,
    wifi_peripheral: &mut Option<WIFI<'static>>,
    spawner: Spawner,
) {
    match command {
        ParsedCommand::GetCurrentState => send(tx, &improv::state_frame(*state)).await,
        ParsedCommand::GetDeviceInfo => {
            let frame = improv::rpc_response_frame(
                Command::GetDeviceInfo,
                &[
                    b"embewi-agent-esp",
                    env!("CARGO_PKG_VERSION").as_bytes(),
                    b"ESP32-C3",
                    b"embewi-agent-esp",
                ],
            );
            send(tx, &frame).await;
        }
        ParsedCommand::GetWifiNetworks => {
            if let Some(wifi) = ensure_wifi(wifi, wifi_peripheral, spawner) {
                match wifi.controller.scan_async(&ScanConfig::default().with_max(20)).await {
                    Ok(access_points) => {
                        let mut seen: Vec<String> = Vec::new();
                        for ap in access_points {
                            let ssid = ap.ssid.as_str();
                            if ssid.is_empty() || seen.iter().any(|s| s == ssid) {
                                continue;
                            }
                            seen.push(String::from(ssid));
                            let rssi = alloc::format!("{}", ap.signal_strength);
                            let secured = !matches!(
                                ap.auth_method,
                                None | Some(AuthenticationMethod::None)
                            );
                            let auth: &[u8] = if secured { b"YES" } else { b"NO" };
                            let frame = improv::rpc_response_frame(
                                Command::GetWifiNetworks,
                                &[ssid.as_bytes(), rssi.as_bytes(), auth],
                            );
                            send(tx, &frame).await;
                        }
                    }
                    Err(e) => warn!("Wi-Fi scan failed: {e:?}"),
                }
            }
            // An empty entry terminates the list.
            send(tx, &improv::rpc_response_frame(Command::GetWifiNetworks, &[])).await;
        }
        ParsedCommand::GetNetworkState => {
            let mut flags: u8 = 0x02; // supports Wi-Fi
            if wifi.as_ref().is_some_and(|w| w.stack.config_v4().is_some()) {
                flags |= 0x01; // online
            }
            let flags = alloc::format!("{flags}");
            send(
                tx,
                &improv::rpc_response_frame(Command::GetNetworkState, &[flags.as_bytes()]),
            )
            .await;
        }
        ParsedCommand::WifiSettings(settings) => {
            info!("Improv: connecting to SSID={}", settings.ssid);
            *state = State::Provisioning;
            send(tx, &improv::state_frame(*state)).await;

            let connected = match ensure_wifi(wifi, wifi_peripheral, spawner) {
                Some(wifi) => {
                    let config = WifiConfig::Station(
                        StationConfig::default()
                            .with_ssid(settings.ssid.as_str())
                            .with_password(settings.password),
                    );
                    wifi.controller.set_config(&config).is_ok()
                        && wifi.controller.connect_async().await.is_ok()
                }
                None => false,
            };

            if connected {
                let stack = wifi.as_ref().expect("connected implies initialized").stack;
                stack.wait_config_up().await;
                *state = State::Provisioned;
                send(tx, &improv::state_frame(*state)).await;
                send(tx, &improv::rpc_response_frame(Command::WifiSettings, &[])).await;
                info!("Improv: Wi-Fi connected, ip = {:?}", stack.config_v4());
            } else {
                *state = State::Authorized;
                send(tx, &improv::error_frame(ImprovError::UnableToConnect)).await;
                send(tx, &improv::state_frame(*state)).await;
                warn!("Improv: Wi-Fi connect failed");
            }
        }
        ParsedCommand::Unsupported(cmd) => {
            info!("Improv: unsupported command 0x{cmd:02X}");
            send(tx, &improv::error_frame(ImprovError::UnknownRpc)).await;
        }
    }
}
