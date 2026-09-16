//! Status shown on the onboard WS2812 LED.
//!
//! | Status       | Colour | Pattern              |
//! |--------------|--------|----------------------|
//! | `Booting`    | white  | steady               |
//! | `Ready`      | blue   | slow blink           |
//! | `Scanning`   | blue   | fast blink           |
//! | `Connecting` | orange | fast blink           |
//! | `Online`     | green  | steady               |
//! | `Failed`     | red    | blink, until retried |

use core::sync::atomic::{AtomicU8, Ordering};

use embassy_time::{Duration, Timer};
use esp_hal::gpio::AnyPin;
use esp_hal::peripherals::RMT;
use esp_hal::rmt::Rmt;
use esp_hal::time::Rate;
use esp_hal_smartled::{RmtSmartLeds, Timing, buffer_size, color_order};
use log::warn;
use smart_leds::{RGB8, SmartLedsWrite};

/// `esp-hal-smartled2` 0.29.0 multiplies its pulse widths by an extra `* 2`,
/// assuming the RMT counter runs at twice the given source clock. That is
/// wrong here, and made the LED sit at full-brightness white whatever was
/// written (upstream issue #9, reported for the ESP32-S3). Pre-halving every
/// duration cancels that doubling.
const WS2812_TIMING_HALVED: Timing = Timing {
    time_0_high: esp_hal_smartled::WS2812_TIMING.time_0_high / 2,
    time_0_low: esp_hal_smartled::WS2812_TIMING.time_0_low / 2,
    time_1_high: esp_hal_smartled::WS2812_TIMING.time_1_high / 2,
    time_1_low: esp_hal_smartled::WS2812_TIMING.time_1_low / 2,
    reset: esp_hal_smartled::WS2812_TIMING.reset / 2,
};

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Booting = 0,
    Ready = 1,
    Scanning = 2,
    Connecting = 3,
    Online = 4,
    Failed = 5,
}

impl Status {
    fn from_byte(byte: u8) -> Self {
        match byte {
            1 => Status::Ready,
            2 => Status::Scanning,
            3 => Status::Connecting,
            4 => Status::Online,
            5 => Status::Failed,
            _ => Status::Booting,
        }
    }

    /// Colour, and how long each on/off phase lasts (`None` stays lit).
    fn pattern(self) -> (RGB8, Option<Duration>) {
        let fast = Some(Duration::from_millis(150));
        match self {
            Status::Booting => (RGB8::new(10, 10, 10), None),
            Status::Ready => (RGB8::new(0, 0, 30), Some(Duration::from_millis(500))),
            Status::Scanning => (RGB8::new(0, 0, 30), fast),
            Status::Connecting => (RGB8::new(30, 15, 0), fast),
            Status::Online => (RGB8::new(0, 20, 0), None),
            Status::Failed => (RGB8::new(30, 0, 0), Some(Duration::from_millis(300))),
        }
    }
}

static STATUS: AtomicU8 = AtomicU8::new(Status::Booting as u8);

/// riscv32imc has no atomic read-modify-write, but a plain store is enough:
/// the value is only ever overwritten, never updated in place.
pub fn set(status: Status) {
    STATUS.store(status as u8, Ordering::Relaxed);
}

fn get() -> Status {
    Status::from_byte(STATUS.load(Ordering::Relaxed))
}

async fn park() -> ! {
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

/// Drives the LED to match [`set`]. Never panics: without the LED the rest of
/// the firmware still works, so a driver failure only costs the display.
/// Only spawn this when a status LED GPIO is configured (see
/// `Storage::load_led_gpio` and `src/bin/main.rs`).
#[embassy_executor::task]
pub async fn led_task(rmt: RMT<'static>, pin: AnyPin<'static>) -> ! {
    let rmt = match Rmt::new(rmt, Rate::from_mhz(80)) {
        Ok(rmt) => rmt,
        Err(e) => {
            warn!("Status LED unavailable, RMT init failed: {e:?}");
            park().await
        }
    };
    let mut led = match RmtSmartLeds::<
        { buffer_size::<RGB8>(1) },
        _,
        RGB8,
        color_order::Rgb,
    >::new_with_memsize(WS2812_TIMING_HALVED, rmt.channel0, pin, 2)
    {
        Ok(led) => led,
        Err(e) => {
            warn!("Status LED unavailable, WS2812 init failed: {e:?}");
            park().await
        }
    };

    let mut shown: Option<RGB8> = None;
    let mut lit = true;
    loop {
        let (colour, phase) = get().pattern();
        let colour = if phase.is_some() && !lit {
            RGB8::default()
        } else {
            colour
        };
        if shown != Some(colour) {
            if let Err(e) = led.write([colour].into_iter()) {
                warn!("LED write failed: {e:?}");
            }
            shown = Some(colour);
        }

        lit = !lit;
        Timer::after(phase.unwrap_or(Duration::from_millis(200))).await;
    }
}
