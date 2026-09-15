//! Minimal Improv Serial implementation (<https://www.improv-wifi.com/serial/>).
//!
//! Byte-for-byte compatible with the reference `improv-wifi/sdk-cpp` codec and
//! ESPHome's `improv_serial` component (verified against their source), since
//! that's what the ESP Web Tools browser client is built and tested against.

use alloc::vec::Vec;
use alloc::string::String;

const HEADER: &[u8; 6] = b"IMPROV";
const VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum FrameType {
    CurrentState = 0x01,
    ErrorState = 0x02,
    Rpc = 0x03,
    RpcResponse = 0x04,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    #[allow(dead_code)]
    Stopped = 0x00,
    Authorized = 0x02,
    Provisioning = 0x03,
    Provisioned = 0x04,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ImprovError {
    UnableToConnect = 0x03,
    UnknownRpc = 0x02,
}

/// RPC command IDs. `GetCurrentState` doubles as "Identify" on the wire (both
/// are `0x02` in the reference implementation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    WifiSettings = 0x01,
    GetCurrentState = 0x02,
    GetDeviceInfo = 0x03,
    GetWifiNetworks = 0x04,
    GetNetworkState = 0x07,
}

impl Command {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0x01 => Command::WifiSettings,
            0x02 => Command::GetCurrentState,
            0x03 => Command::GetDeviceInfo,
            0x04 => Command::GetWifiNetworks,
            0x07 => Command::GetNetworkState,
            _ => return None,
        })
    }
}

pub struct WifiSettings {
    pub ssid: String,
    pub password: String,
}

pub enum ParsedCommand {
    WifiSettings(WifiSettings),
    GetCurrentState,
    GetDeviceInfo,
    GetWifiNetworks,
    GetNetworkState,
    Unsupported(u8),
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

fn frame(frame_type: FrameType, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + payload.len() + 2);
    out.extend_from_slice(HEADER);
    out.push(VERSION);
    out.push(frame_type as u8);
    out.push(payload.len() as u8);
    out.extend_from_slice(payload);
    out.push(checksum(&out));
    // The browser client discards unrecognized bytes up to the next newline;
    // terminating every frame (as ESPHome does) guarantees it resyncs.
    out.push(b'\n');
    out
}

pub fn state_frame(state: State) -> Vec<u8> {
    frame(FrameType::CurrentState, &[state as u8])
}

pub fn error_frame(error: ImprovError) -> Vec<u8> {
    frame(FrameType::ErrorState, &[error as u8])
}

/// Builds an RPC response frame answering `command`, carrying `strings` as
/// length-prefixed entries (SSID/RSSI/auth, device info, redirect URLs...).
///
/// Mirrors the reference implementation's Serial encoding, including its
/// trailing zero pad byte (a leftover of sharing this builder with the BLE
/// transport, which embeds its own checksum there instead).
pub fn rpc_response_frame(command: Command, strings: &[&[u8]]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(command as u8);
    payload.push(0); // patched below once the strings section length is known
    let strings_start = payload.len();
    for s in strings {
        payload.push(s.len() as u8);
        payload.extend_from_slice(s);
    }
    payload[1] = (payload.len() - strings_start) as u8;
    payload.push(0); // trailing pad byte, see doc comment above
    frame(FrameType::RpcResponse, &payload)
}

/// Byte-by-byte Improv Serial frame parser, mirroring
/// `improv::parse_improv_serial_byte` from the reference implementation: an
/// unexpected byte at any position silently resets synchronization instead of
/// erroring, so plain log lines interleaved on the same wire are tolerated.
pub struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn feed(&mut self, byte: u8) -> Option<ParsedCommand> {
        let position = self.buffer.len();

        let synced = match position {
            0 => byte == b'I',
            1 => byte == b'M',
            2 => byte == b'P',
            3 => byte == b'R',
            4 => byte == b'O',
            5 => byte == b'V',
            6 => byte == VERSION,
            7 | 8 => true, // type, data length
            _ => {
                let data_len = self.buffer[8] as usize;
                if position <= 8 + data_len {
                    true
                } else if position == 8 + data_len + 1 {
                    checksum(&self.buffer) == byte
                } else {
                    false
                }
            }
        };

        if !synced {
            self.buffer.clear();
            return None;
        }

        self.buffer.push(byte);
        if self.buffer.len() < 9 {
            return None;
        }

        let data_len = self.buffer[8] as usize;
        if self.buffer.len() != 9 + data_len + 1 {
            return None;
        }

        let frame_type = self.buffer[7];
        let result = (frame_type == FrameType::Rpc as u8)
            .then(|| Self::parse_rpc_payload(&self.buffer[9..9 + data_len]))
            .flatten();
        self.buffer.clear();
        result
    }

    fn parse_rpc_payload(data: &[u8]) -> Option<ParsedCommand> {
        let command_byte = *data.first()?;
        let data_length = *data.get(1)? as usize;
        if data.len() != 2 + data_length {
            return None;
        }

        let command = Command::from_byte(command_byte);
        if command != Some(Command::WifiSettings) {
            return Some(match command {
                Some(Command::GetCurrentState) => ParsedCommand::GetCurrentState,
                Some(Command::GetDeviceInfo) => ParsedCommand::GetDeviceInfo,
                Some(Command::GetWifiNetworks) => ParsedCommand::GetWifiNetworks,
                Some(Command::GetNetworkState) => ParsedCommand::GetNetworkState,
                _ => ParsedCommand::Unsupported(command_byte),
            });
        }

        let ssid_len = *data.get(2)? as usize;
        let ssid_start = 3;
        let ssid_end = ssid_start + ssid_len;
        let pass_len = *data.get(ssid_end)? as usize;
        let pass_start = ssid_end + 1;
        let pass_end = pass_start + pass_len;
        if pass_end > data.len() {
            return None;
        }

        Some(ParsedCommand::WifiSettings(WifiSettings {
            ssid: String::from_utf8_lossy(&data[ssid_start..ssid_end]).into_owned(),
            password: String::from_utf8_lossy(&data[pass_start..pass_end]).into_owned(),
        }))
    }
}
