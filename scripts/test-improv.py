#!/usr/bin/env python3
"""Hardware gate for Embewi's Improv Serial path.

The ESP logs and Improv binary frames share the same USB-Serial-JTAG stream.
This client therefore resynchronizes on valid IMPROV frames while preserving
plain-text log output.

Dependency:
    python3 -m pip install pyserial

Examples:
    scripts/test-improv.py --port COM7 status
    scripts/test-improv.py --port COM7 scan
    scripts/test-improv.py --port COM7 gate --ssid MyWifi --password 'secret'
"""

from __future__ import annotations

import argparse
import sys
import time
from dataclasses import dataclass

try:
    import serial
    from serial.tools import list_ports
except ImportError:
    print("pyserial is required: python3 -m pip install pyserial", file=sys.stderr)
    raise SystemExit(2)

HEADER = b"IMPROV"
VERSION = 1

TYPE_STATE = 0x01
TYPE_ERROR = 0x02
TYPE_RPC = 0x03
TYPE_RPC_RESPONSE = 0x04

CMD_WIFI_SETTINGS = 0x01
CMD_CURRENT_STATE = 0x02
CMD_DEVICE_INFO = 0x03
CMD_WIFI_NETWORKS = 0x04
CMD_NETWORK_STATE = 0x07

STATE_NAMES = {
    0x00: "Stopped",
    0x02: "Authorized",
    0x03: "Provisioning",
    0x04: "Provisioned",
}

ERROR_NAMES = {
    0x02: "UnknownRpc",
    0x03: "UnableToConnect",
}


@dataclass
class Frame:
    frame_type: int
    payload: bytes


def checksum(data: bytes) -> int:
    return sum(data) & 0xFF


def make_frame(frame_type: int, payload: bytes) -> bytes:
    out = bytearray(HEADER)
    out += bytes((VERSION, frame_type, len(payload)))
    out += payload
    out.append(checksum(out))
    out.append(0x0A)
    return bytes(out)


def rpc(command: int, body: bytes = b"") -> bytes:
    return make_frame(TYPE_RPC, bytes((command, len(body))) + body)


def wifi_settings(ssid: str, password: str) -> bytes:
    ssid_b = ssid.encode()
    pass_b = password.encode()
    if len(ssid_b) > 255 or len(pass_b) > 255:
        raise ValueError("SSID/password too long for Improv framing")
    body = bytes((len(ssid_b),)) + ssid_b + bytes((len(pass_b),)) + pass_b
    return rpc(CMD_WIFI_SETTINGS, body)


def parse_rpc_strings(payload: bytes) -> tuple[int, list[str]]:
    if len(payload) < 3:
        raise ValueError("short RPC response")
    command = payload[0]
    data_len = payload[1]
    end = 2 + data_len
    if end > len(payload):
        raise ValueError("truncated RPC response")

    data = payload[2:end]
    values: list[str] = []
    pos = 0
    while pos < len(data):
        length = data[pos]
        pos += 1
        if pos + length > len(data):
            raise ValueError("truncated RPC string")
        values.append(data[pos : pos + length].decode(errors="replace"))
        pos += length
    return command, values


def choose_port(explicit: str | None) -> str:
    if explicit:
        return explicit

    ports = list(list_ports.comports())
    esp = [
        p
        for p in ports
        if p.vid == 0x303A
        or "Espressif" in (p.manufacturer or "")
        or "USB JTAG" in (p.description or "")
    ]
    candidates = esp or ports
    if len(candidates) == 1:
        return candidates[0].device

    if not candidates:
        raise SystemExit("No serial port found. Pass --port COMx or --port /dev/ttyACMx.")

    print("Multiple serial ports found; choose one with --port:", file=sys.stderr)
    for p in candidates:
        print(
            f"  {p.device:16} {p.description} "
            f"VID:PID={p.vid or 0:04x}:{p.pid or 0:04x}",
            file=sys.stderr,
        )
    raise SystemExit(2)


class ImprovSerial:
    def __init__(self, port: str, timeout: float = 0.1):
        self.ser = serial.Serial(port, 115200, timeout=timeout)
        # Avoid modem-control surprises on USB serial adapters.
        self.ser.dtr = False
        self.ser.rts = False
        self.buf = bytearray()
        self.log_buf = bytearray()
        self.log_history = bytearray()
        self.frames: list[Frame] = []

    def close(self) -> None:
        self.flush_logs()
        self.ser.close()

    def send(self, data: bytes) -> None:
        self.ser.write(data)
        self.ser.flush()

    def _emit_log(self, data: bytes) -> None:
        self.log_history += data
        if len(self.log_history) > 65536:
            del self.log_history[:-65536]
        self.log_buf += data
        while b"\n" in self.log_buf:
            line, _, rest = self.log_buf.partition(b"\n")
            self.log_buf = bytearray(rest)
            text = line.decode(errors="replace").rstrip("\r")
            if text:
                print(f"[device] {text}")

    def flush_logs(self) -> None:
        if self.log_buf:
            text = self.log_buf.decode(errors="replace").strip()
            if text:
                print(f"[device] {text}")
            self.log_buf.clear()

    def poll(self) -> None:
        chunk = self.ser.read(512)
        if chunk:
            self.buf += chunk

        while True:
            pos = self.buf.find(HEADER)
            if pos < 0:
                # Keep a possible partial header suffix for the next read.
                keep = min(len(self.buf), len(HEADER) - 1)
                if len(self.buf) > keep:
                    self._emit_log(bytes(self.buf[:-keep]))
                    del self.buf[:-keep]
                return

            if pos:
                self._emit_log(bytes(self.buf[:pos]))
                del self.buf[:pos]

            if len(self.buf) < 11:
                return

            if self.buf[6] != VERSION:
                self._emit_log(bytes((self.buf[0],)))
                del self.buf[0]
                continue

            payload_len = self.buf[8]
            total = 11 + payload_len
            if len(self.buf) < total:
                return

            checksum_index = 9 + payload_len
            newline_index = checksum_index + 1
            candidate = bytes(self.buf[:total])
            if candidate[newline_index] != 0x0A or checksum(candidate[:checksum_index]) != candidate[checksum_index]:
                self._emit_log(bytes((self.buf[0],)))
                del self.buf[0]
                continue

            self.frames.append(Frame(candidate[7], candidate[9:checksum_index]))
            del self.buf[:total]

    def wait_frame(self, predicate, timeout: float = 10.0) -> Frame:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.poll()
            for i, frame in enumerate(self.frames):
                if predicate(frame):
                    return self.frames.pop(i)
            time.sleep(0.02)
        raise TimeoutError("timed out waiting for Improv response")

    def wait_log(self, needle: str, timeout: float = 20.0) -> bool:
        wanted = needle.encode()
        if wanted in self.log_history:
            return True

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.poll()
            if wanted in self.log_history:
                return True
            time.sleep(0.02)
        return False


def rpc_response(client: ImprovSerial, command: int, timeout: float = 10.0) -> list[str]:
    frame = client.wait_frame(
        lambda f: f.frame_type == TYPE_RPC_RESPONSE
        and len(f.payload) >= 1
        and f.payload[0] == command,
        timeout,
    )
    _, values = parse_rpc_strings(frame.payload)
    return values


def get_network_state(client: ImprovSerial) -> tuple[int, str | None]:
    client.send(rpc(CMD_NETWORK_STATE))
    values = rpc_response(client, CMD_NETWORK_STATE)
    if not values:
        raise RuntimeError("GetNetworkState returned no flags")
    flags = int(values[0])
    url = values[1] if len(values) > 1 else None
    return flags, url


def scan(client: ImprovSerial) -> list[tuple[str, int, bool]]:
    client.send(rpc(CMD_WIFI_NETWORKS))
    networks: list[tuple[str, int, bool]] = []
    while True:
        values = rpc_response(client, CMD_WIFI_NETWORKS, timeout=20.0)
        if not values:
            return networks
        if len(values) != 3:
            raise RuntimeError(f"unexpected scan response: {values!r}")
        networks.append((values[0], int(values[1]), values[2].upper() == "YES"))


def run_gate(client: ImprovSerial, ssid: str, password: str) -> None:
    print("1/4 GetNetworkState")
    flags, url = get_network_state(client)
    print(f"  flags=0x{flags:02x} online={bool(flags & 0x01)} wifi={bool(flags & 0x02)} url={url or '-'}")
    if not (flags & 0x01):
        raise RuntimeError("device is not online before reprovision")
    if not (flags & 0x02):
        raise RuntimeError("device does not advertise Wi-Fi support")

    print("2/4 GetWifiNetworks")
    networks = scan(client)
    for name, rssi, secured in networks:
        print(f"  {rssi:4d} dBm  {'secured' if secured else 'open':7}  {name}")
    if not networks:
        raise RuntimeError("scan returned no networks")
    if not any(name == ssid for name, _, _ in networks):
        raise RuntimeError(f"target SSID {ssid!r} not present in scan")

    # Clear any old frames before the stateful operation.
    client.frames.clear()

    print("3/4 Reprovision with the same credentials")
    client.send(wifi_settings(ssid, password))

    provisioning = client.wait_frame(
        lambda f: f.frame_type == TYPE_STATE and f.payload == b"\x03",
        timeout=5.0,
    )
    assert provisioning.payload == b"\x03"
    print("  state=Provisioning")

    provisioned = client.wait_frame(
        lambda f: f.frame_type == TYPE_STATE and f.payload == b"\x04",
        timeout=30.0,
    )
    assert provisioned.payload == b"\x04"
    print("  state=Provisioned")

    values = rpc_response(client, CMD_WIFI_SETTINGS, timeout=5.0)
    print(f"  nextUrl={values[0] if values else '-'}")

    print("4/4 Supervisor one-shot")
    needle = "supervisor: IP services already started, ignoring duplicate readiness"
    if not client.wait_log(needle, timeout=10.0):
        raise RuntimeError(
            "duplicate IP-ready log not observed; cannot prove supervisor one-shot on serial"
        )
    print("  duplicate IP readiness explicitly ignored")

    flags, url = get_network_state(client)
    if not (flags & 0x01):
        raise RuntimeError("device went offline after reprovision")
    print(f"  final online=yes url={url or '-'}")
    print("\nPASS: Improv scan + reprovision + supervisor one-shot")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", help="serial port; auto-detected when unambiguous")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("status")
    sub.add_parser("scan")

    gate = sub.add_parser("gate")
    gate.add_argument("--ssid", required=True)
    gate.add_argument("--password", required=True)

    provision = sub.add_parser("provision")
    provision.add_argument("--ssid", required=True)
    provision.add_argument("--password", required=True)

    args = parser.parse_args()
    port = choose_port(args.port)
    print(f"Opening {port}")
    client = ImprovSerial(port)
    try:
        # Give native USB a short moment after opening and drain chatter.
        deadline = time.monotonic() + 0.5
        while time.monotonic() < deadline:
            client.poll()

        if args.command == "status":
            flags, url = get_network_state(client)
            print(f"flags=0x{flags:02x} online={bool(flags & 1)} wifi={bool(flags & 2)} url={url or '-'}")
        elif args.command == "scan":
            for name, rssi, secured in scan(client):
                print(f"{rssi:4d} dBm  {'secured' if secured else 'open':7}  {name}")
        elif args.command == "provision":
            client.send(wifi_settings(args.ssid, args.password))
            frame = client.wait_frame(
                lambda f: f.frame_type in (TYPE_STATE, TYPE_ERROR),
                timeout=30.0,
            )
            if frame.frame_type == TYPE_ERROR:
                code = frame.payload[0] if frame.payload else -1
                raise RuntimeError(f"Improv error: {ERROR_NAMES.get(code, hex(code))}")
            print(f"state={STATE_NAMES.get(frame.payload[0], hex(frame.payload[0]))}")
        elif args.command == "gate":
            run_gate(client, args.ssid, args.password)
        return 0
    finally:
        client.close()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (TimeoutError, RuntimeError, ValueError, serial.SerialException) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        raise SystemExit(1)
