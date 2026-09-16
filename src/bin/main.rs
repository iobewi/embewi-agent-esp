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

use embewi_agent_esp::status;
use embewi_agent_esp::storage::Storage;
use embewi_agent_esp::wifi::WifiManager;
use embewi_agent_esp::provisioning;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // `init_logger` (not `_from_env`) applies one flat level to every crate,
    // ignoring ESP_LOG's per-module syntax entirely. That meant smoltcp,
    // embassy-net and esp-radio were all logging at "info" on the same USB
    // wire Improv uses, real bytes possibly queued behind that chatter --
    // suspected contributor to Improv responses missing the browser's tight
    // reconnect timeout once Wi-Fi/HTTP started running concurrently. See
    // .cargo/config.toml's ESP_LOG for the actual per-crate levels now used.
    esp_println::logger::init_logger_from_env();

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Sizes recommended by esp-radio's docs for Wi-Fi.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // Not yet wrapped in the shared Mutex: nothing else is running yet, so
    // this one-time boot read needs no locking.
    let mut boot_storage = Storage::new(peripherals.FLASH);

    // Which GPIO (if any) drives the status LED is board-specific, so it's
    // read from NVS instead of being hardcoded -- set at runtime from the
    // device's own HTTP config page (see src/http/ and web/), not at compile
    // time.
    if let Some(gpio) = boot_storage.load_led_gpio() {
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
            // src/http/ rejects anything else before it ever reaches NVS, so
            // this should be unreachable.
            other => panic!("saved status LED GPIO {other} is out of range for this chip"),
        };
        spawner.spawn(status::led_task(peripherals.RMT, led_pin).unwrap());
    }

    static STORAGE: StaticCell<Mutex<CriticalSectionRawMutex, Storage>> = StaticCell::new();
    let storage = STORAGE.init(Mutex::new(boot_storage));

    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split();
    let mut wifi = WifiManager::new(peripherals.WIFI, peripherals.LPWR, spawner);
    wifi.reconnect_saved(storage).await;

    provisioning::run(rx, tx, wifi, storage).await
}
