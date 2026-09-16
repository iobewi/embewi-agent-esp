//! Improv Serial service: answers ESP Web Tools over USB-Serial-JTAG so that
//! Wi-Fi credentials can be entered from the browser instead of being baked
//! into the firmware.

use embedded_io_async::{Read, Write};
use esp_hal::Async;
use esp_hal::usb_serial_jtag::{UsbSerialJtagRx, UsbSerialJtagTx};
use log::{info, warn};

use crate::improv::{self, Command, ImprovError, ParsedCommand, Parser, State};
use crate::wifi::WifiManager;

const NAME: &str = "embewi-agent-esp";
const CHIP: &str = "ESP32-C3";

type Rx = UsbSerialJtagRx<'static, Async>;
type Tx = UsbSerialJtagTx<'static, Async>;

/// Serves Improv Serial forever.
pub async fn run(mut rx: Rx, mut tx: Tx, mut wifi: WifiManager) -> ! {
    let mut parser = Parser::new();
    let mut state = State::Authorized;
    let mut buffer = [0u8; 64];

    info!("Improv: listening on USB-Serial-JTAG");

    loop {
        let read = match rx.read(&mut buffer).await {
            Ok(read) => read,
            Err(e) => {
                warn!("USB read failed: {e:?}");
                continue;
            }
        };
        for &byte in &buffer[..read] {
            if let Some(command) = parser.feed(byte) {
                handle(command, &mut tx, &mut state, &mut wifi).await;
            }
        }
    }
}

async fn send(tx: &mut Tx, frame: &[u8]) {
    if let Err(e) = tx.write_all(frame).await {
        warn!("USB write failed: {e:?}");
    }
}

async fn handle(
    command: ParsedCommand,
    tx: &mut Tx,
    state: &mut State,
    wifi: &mut WifiManager,
) {
    match command {
        ParsedCommand::GetCurrentState => send(tx, &improv::state_frame(*state)).await,
        ParsedCommand::GetDeviceInfo => {
            let frame = improv::rpc_response_frame(
                Command::GetDeviceInfo,
                &[
                    NAME.as_bytes(),
                    env!("CARGO_PKG_VERSION").as_bytes(),
                    CHIP.as_bytes(),
                    NAME.as_bytes(),
                ],
            );
            send(tx, &frame).await;
        }
        ParsedCommand::GetWifiNetworks => {
            for network in wifi.scan().await {
                let signal_strength = alloc::format!("{}", network.signal_strength);
                let secured: &[u8] = if network.secured { b"YES" } else { b"NO" };
                let frame = improv::rpc_response_frame(
                    Command::GetWifiNetworks,
                    &[network.ssid.as_bytes(), signal_strength.as_bytes(), secured],
                );
                send(tx, &frame).await;
            }
            // An empty entry terminates the list.
            send(tx, &improv::rpc_response_frame(Command::GetWifiNetworks, &[])).await;
        }
        ParsedCommand::GetNetworkState => {
            let mut flags: u8 = 0x02; // supports Wi-Fi
            if wifi.is_online() {
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

            if wifi.connect(&settings.ssid, settings.password).await {
                *state = State::Provisioned;
                send(tx, &improv::state_frame(*state)).await;
                send(tx, &improv::rpc_response_frame(Command::WifiSettings, &[])).await;
            } else {
                *state = State::Authorized;
                send(tx, &improv::error_frame(ImprovError::UnableToConnect)).await;
                send(tx, &improv::state_frame(*state)).await;
            }
        }
        ParsedCommand::Unsupported(command) => {
            info!("Improv: unsupported command 0x{command:02X}");
            send(tx, &improv::error_frame(ImprovError::UnknownRpc)).await;
        }
    }
}
