#![no_std]
extern crate alloc;

pub mod agent;
pub mod heartbeat;
pub mod http;
pub mod improv;
pub mod log_stream;
pub mod ota;
pub mod provisioning;
pub mod stack_usage;
pub mod status;
pub mod storage;
pub mod time;
pub mod tls;
pub mod wifi;
// POC 1: native agent/workload split (see src/workload.rs's doc comment).
// Supersedes the earlier WASM/wasmi spike for now -- wasmi 2.0.0 doesn't
// build for ESP32-C3's riscv32imc target at all (hard `alloc::sync::Arc`/
// atomics dependency, absent RISC-V 'A' extension), and the architecture
// discussion moved to testing native-artefact isolation first.
pub mod workload;
