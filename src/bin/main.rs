#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Pin;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::UsbSerialJtag;
use static_cell::StaticCell;

use embewi_agent_esp::agent;
use embewi_agent_esp::app_config;
use embewi_agent_esp::hardware;
use embewi_agent_esp::status;
use embewi_agent_esp::tls;
use embewi_agent_esp::storage::Storage;
use config_space_manager::ConfigManager;
use embewi_agent_esp::config::NvsConfigBackend;
use embewi_agent_esp::wifi::{self, WifiManager};
use embewi_agent_esp::provisioning;
use embewi_agent_esp::runtime_config;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // First thing, before anything else touches the stack any deeper than
    // this: paints it for `stack_usage::free_bytes()`'s high-water-mark
    // measurement (contrat §5's `task_hwm_min`) -- the earlier this runs,
    // the more of the stack it captures as "unused" before real usage
    // grows past it. Doesn't capture the runtime's own pre-`main` prologue
    // (riscv-rt's own stack usage before jumping here), but that's a fixed,
    // small, one-time cost, not something that grows with this firmware's
    // own code.
    embewi_agent_esp::stack_usage::paint();

    // `log_stream::install()` replaces `esp_println::logger::init_logger_from_env()`:
    // it still prints locally at the same filter level (`.cargo/config.toml`'s
    // ESP_LOG, now hardcoded there instead of re-parsed -- see log_stream.rs's
    // doc comment for why), but also captures every line for the outbound
    // WebSocket log stream (contrat §5). `init_logger` (not `_from_env`, the
    // reason the old call used the latter) applies one flat level to every
    // crate, ignoring ESP_LOG's per-module syntax entirely -- that meant
    // smoltcp, embassy-net and esp-radio were all logging at "info" on the
    // same USB wire Improv uses, real bytes possibly queued behind that
    // chatter, a suspected contributor to an earlier Improv bug.
    embewi_agent_esp::log_stream::install();

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Sizes recommended by esp-radio's docs for Wi-Fi. This second pool is
    // carved out of the same DRAM region the linker otherwise reserves
    // entirely for the stack (see `stack_usage.rs`) -- grown from 36 KiB
    // once `task_hwm_min` (contrat §5's real stack high-water-mark,
    // exposed in the heartbeat) confirmed real usage was nowhere close to
    // the ~189 KiB the linker set aside by default. Deliberately not
    // claiming all of the headroom `task_hwm_min` showed free: that
    // number only reflects code paths actually exercised so far (a real
    // OTA cycle, concurrent admin+heartbeat+logs TLS, haven't all been
    // observed together yet), so this keeps a wide safety margin rather
    // than assuming the untested paths won't dig deeper.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 132 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // `esp_hal::init` above unconditionally disabled every watchdog on the
    // chip (no `Config` option to keep one running), and `TimerGroup::new`
    // just now reset the whole TIMG0 block anyway (its first use resets the
    // peripheral -- arming the watchdog any earlier than this would just
    // have that reset wipe it straight back out). From here on, re-armed:
    // a freeze anywhere through `ota::on_boot`'s decision still resets the
    // device instead of bricking it on a `pending_verify` image; see
    // `ota.rs`'s "anti-freeze watchdog" section for the rest of it.
    embewi_agent_esp::ota::arm_boot_watchdog();

    static STORAGE: StaticCell<Mutex<CriticalSectionRawMutex, Storage>> = StaticCell::new();
    let storage = STORAGE.init(Mutex::new(Storage::new(peripherals.FLASH)));

    // Components claim isolated persistent configuration capabilities at
    // boot. The manager knows capacities/ownership only; each component owns
    // the schema inside its opaque space. Claims are completed in a fixed
    // order before any application service is spawned.
    let config_backend = NvsConfigBackend::new(storage)
        .await
        .expect("NVS config backend unavailable");
    let mut config_manager = ConfigManager::new(config_backend);

    let hardware_config = config_manager
        .claim("hardware", hardware::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for hardware config");
    static HARDWARE_CONFIG: StaticCell<hardware::HardwareConfigSpace> = StaticCell::new();
    let hardware_config = &*HARDWARE_CONFIG.init(hardware_config);

    let app_config = config_manager
        .claim("app", app_config::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for app config");
    static APP_CONFIG: StaticCell<app_config::AppConfigSpace> = StaticCell::new();
    let app_config = &*APP_CONFIG.init(app_config);

    let agent_config = config_manager
        .claim("agent", agent::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for agent config");
    static AGENT_CONFIG: StaticCell<agent::AgentConfigSpace> = StaticCell::new();
    let agent_config = &*AGENT_CONFIG.init(agent_config);

    let lifecycle_config = config_manager
        .claim("lifecycle", embewi_agent_esp::lifecycle::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for lifecycle state");
    static LIFECYCLE_CONFIG: StaticCell<embewi_agent_esp::lifecycle::LifecycleConfigSpace> =
        StaticCell::new();
    let lifecycle_config = &*LIFECYCLE_CONFIG.init(lifecycle_config);

    let runtime_space = config_manager
        .claim("runtime", runtime_config::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for runtime config");
    static RUNTIME_CONFIG: StaticCell<runtime_config::RuntimeConfig> = StaticCell::new();
    let runtime_config = &*RUNTIME_CONFIG.init(
        runtime_config::RuntimeConfig::new(runtime_space)
            .await
            .expect("runtime config unavailable"),
    );

    let tls_config = config_manager
        .claim("tls", tls::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for TLS config");
    static TLS_CONFIG: StaticCell<tls::TlsConfigSpace> = StaticCell::new();
    let tls_config = &*TLS_CONFIG.init(tls_config);

    let wifi_config = config_manager
        .claim("wifi", wifi::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for Wi-Fi config");

    // Which GPIO (if any) drives the status LED is board-specific and now
    // belongs to the hardware ConfigSpace rather than application-owned NVS.
    if let Some(gpio) = hardware::led_gpio(hardware_config).await {
        let led_pin = match gpio {
            0 => peripherals.GPIO0.degrade(),
            1 => peripherals.GPIO1.degrade(),
            2 => peripherals.GPIO2.degrade(),
            3 => peripherals.GPIO3.degrade(),
            4 => peripherals.GPIO4.degrade(),
            5 => peripherals.GPIO5.degrade(),
            6 => peripherals.GPIO6.degrade(),
            7 => peripherals.GPIO7.degrade(),
            8 => peripherals.GPIO8.degrade(),
            9 => peripherals.GPIO9.degrade(),
            10 => peripherals.GPIO10.degrade(),
            11 => peripherals.GPIO11.degrade(),
            12 => peripherals.GPIO12.degrade(),
            13 => peripherals.GPIO13.degrade(),
            14 => peripherals.GPIO14.degrade(),
            15 => peripherals.GPIO15.degrade(),
            16 => peripherals.GPIO16.degrade(),
            17 => peripherals.GPIO17.degrade(),
            18 => peripherals.GPIO18.degrade(),
            19 => peripherals.GPIO19.degrade(),
            20 => peripherals.GPIO20.degrade(),
            21 => peripherals.GPIO21.degrade(),
            other => panic!("saved status LED GPIO {other} is out of range for this chip"),
        };
        spawner.spawn(status::led_task(peripherals.RMT, led_pin).unwrap());
    }

    // contrat §3: detects whether the image that just booted is an
    // unconfirmed OTA update (`PENDING_VERIFY`) and, if so, starts the
    // bounded self-check that validates it or rolls it back -- before
    // Wi-Fi/HTTP come up, since this is a purely local safety net that
    // must run regardless of network state.
    embewi_agent_esp::ota::on_boot(storage, spawner).await;

    // Admin server TLS: one global MbedTLS instance for the whole program.
    let tls = embewi_agent_esp::tls::init();

    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split();

    let mut supervisor = embewi_agent_esp::supervisor::ApplicationSupervisor::new(
        spawner,
        peripherals.LPWR,
        tls,
        agent_config,
        app_config,
        hardware_config,
        tls_config,
        runtime_config,
        lifecycle_config,
    );

    let mut wifi = WifiManager::new(peripherals.WIFI, spawner, wifi_config);
    if wifi.reconnect_saved().await {
        if let Some(stack) = wifi.ip_stack() {
            supervisor.on_ip_ready(stack, storage);
        }
    }

    provisioning::run(rx, tx, wifi, supervisor, storage).await
}
