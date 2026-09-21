//! POC 1+2+2b (agent/workload architecture spike -- see the design
//! discussion, not wired to any real deployment path). Maps a second,
//! entirely separately compiled and linked native artefact
//! (`crates/workload-poc`) executable from its own flash region while this
//! agent is already running (POC 1, hardware-confirmed with a WS2812 test
//! -- see project memory for that debugging story), then (POC 2, PASS and
//! frozen -- functional feasibility, not a load test) binds a TIMG1
//! hardware interrupt directly to a handler function living in that same
//! separately-mapped region. POC 2b (this version) raises the interrupt
//! rate for timing characterization -- see [`TIMER_PERIOD`] and
//! `crates/workload-poc`'s doc comment for why the ISR dropped its WS2812
//! animation to do that honestly.
//!
//! ## Why TIMG1, not TIMG0
//!
//! TIMG0's `timer0` is already claimed by `esp_rtos::start()` in
//! `src/bin/main.rs` (the scheduler's own tick source) -- arming a second
//! interrupt on it here would be a real, live conflict, not a POC
//! simplification. TIMG1 is completely unclaimed elsewhere in this
//! firmware.
//!
//! ## Why the agent (not `crates/workload-poc`) calls `esp_hal::interrupt`
//!
//! `esp_hal::interrupt::bind_handler`/`InterruptHandler` just write a raw
//! function-pointer `usize` into a RAM dispatch table
//! (`__EXTERNAL_INTERRUPTS`) -- they don't care whether that address is in
//! this agent's own `.text` or, as here, a separately-linked binary's
//! XIP-mapped window. Doing this from the agent side (not from
//! `crates/workload-poc` itself) means the *binding* code runs after the
//! agent's own `esp_hal::init()`, so none of POC 1's `Clocks::get()`
//! cross-binary problem applies to it -- only the ISR body itself (compiled
//! into the separate crate, see that crate's doc comment) has to stay
//! within "raw registers only".
//!
//! ## RAM window (new in POC 2)
//!
//! POC 1 never gave the workload real mutable state. POC 2's `IRQ_COUNT`
//! needs a genuine RAM window that persists and mutates across many
//! interrupt calls -- [`WORKLOAD_RAM_BASE`] is picked from deep inside this
//! agent's own `.stack` region's *measured*-free headroom (see
//! `stack_usage.rs`), not a separate reservation carved out of the linker
//! script. [`ram_window_is_safe`] re-checks that placement against real,
//! current measurements every time this runs (same discipline as
//! [`entries_are_free`] for the MMU entries) rather than trusting the
//! constant to stay valid as the agent's own memory usage grows -- and the
//! agent explicitly zeroes it before the first call, since nothing in
//! `crates/workload-poc` ever runs a startup sequence that would do that
//! automatically (see that crate's doc comment).
//!
//! ## Register-level facts this code is built on (ESP32-C3 only)
//! - MMU/XIP mapping: see POC 1's original doc comment (retained below,
//!   unchanged).
//! - TIMG1 base `0x6002_0000` (`DR_REG_TIMERGROUP1_BASE`, ESP-IDF
//!   `reg_base.h` -- distinct from TIMG0's `0x6001_F000`), used only by
//!   `crates/workload-poc`'s `workload_timer_isr` (this file arms the timer
//!   through `esp-hal`'s normal API, not raw registers -- see above).

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_time::Timer;
use esp_hal::interrupt::{InterruptHandler, Priority};
use esp_hal::peripherals::{Interrupt, TIMG1};
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::PeriodicTimer;
use esp_hal::timer::timg::TimerGroup;
use critical_section::Mutex;
use heapless::Deque;
use log::{error, info};
use static_cell::StaticCell;

/// Base address of the C3's flat, 128-entry MMU table (`DR_REG_MMU_TABLE`,
/// verified against `esp-idf/components/soc/esp32c3/register/soc/reg_base.h`).
const MMU_TABLE_BASE: u32 = 0x600C_5000;
/// `SOC_MMU_ENTRY_NUM`.
const MMU_ENTRY_COUNT: u32 = 128;
/// `MMU_PAGE_64KB` -- the only page size the C3's cache MMU supports.
const MMU_PAGE_SIZE: u32 = 0x1_0000;
/// `SOC_MMU_INVALID` (`BIT(8)`); an entry with this bit set is unused.
const MMU_INVALID: u32 = 1 << 8;
/// `SOC_MMU_IBUS_VADDR_BASE` -- start of the execute-mapped cache window.
const IBUS_VADDR_BASE: u32 = 0x4200_0000;

/// Which MMU table entry this POC claims for the workload window. Fixed
/// and generous (the agent currently uses roughly the first 19 of 128)
/// rather than dynamically scanned for the first free slot -- see this
/// module's doc comment. Must match `crates/workload-poc/workload.ld`'s
/// hardcoded `ORIGIN` by hand.
const WORKLOAD_MMU_ENTRY: u32 = 100;
/// How many 64 KiB pages to map -- `crates/workload-poc`'s whole `.text`
/// is under 2 KiB today, one page is generous headroom for this POC.
const WORKLOAD_MMU_PAGES: u32 = 1;
/// Virtual address `workload_entry` is called at -- `IBUS_VADDR_BASE +
/// WORKLOAD_MMU_ENTRY * MMU_PAGE_SIZE`, i.e. `0x4264_0000`. Must match
/// `crates/workload-poc/workload.ld`'s `ORIGIN` exactly.
const WORKLOAD_VADDR: u32 = IBUS_VADDR_BASE + WORKLOAD_MMU_ENTRY * MMU_PAGE_SIZE;
/// `workload_timer_isr`'s fixed offset past `WORKLOAD_VADDR` -- must match
/// `crates/workload-poc/workload.ld`'s `. = 0x42640300;` exactly (that
/// literal is `WORKLOAD_VADDR + 0x300`, spelled out in full there since the
/// linker script has no way to reference this constant).
const WORKLOAD_TIMER_ISR_VADDR: u32 = WORKLOAD_VADDR + 0x300;

/// Physical flash byte offset `workload.bin` (the raw `.text`/`.rodata`
/// bytes extracted from `crates/workload-poc`'s linked ELF) must be flashed
/// at -- see `scripts/save-image.sh`'s POC 1 addition, which does this
/// automatically as part of `web/firmware/esp32c3/workload.bin`. Chosen
/// right after `ota_1` ends (`0x1a0000 + 0x180000`, per `partitions.csv`)
/// in the flash tail `partitions.csv` doesn't allocate yet (chip is 4 MiB,
/// `ota_1` ends at `0x320000`, flash ends at `0x400000` -- 896 KiB free).
/// Deliberately NOT a real partition-table entry: this is a manual POC
/// placement, not a deployment mechanism -- see the design discussion for
/// why `partitions.csv` stays untouched until this POC's gate passes.
const WORKLOAD_PHYS_OFFSET: u32 = 0x32_0000;

/// Base of the workload's `.data`/`.bss` RAM window -- picked just above
/// this agent's `_stack_end` (the low boundary of `.stack`, see
/// `stack_usage.rs`), deep inside headroom [`ram_window_is_safe`] confirms
/// is genuinely unused every time this runs. Must match
/// `crates/workload-poc/workload.ld`'s `WORKLOAD_RAM` region `ORIGIN`
/// exactly.
///
/// Moved from `0x3FCB_7000` to `0x3FCB_C000` in the entry-latency pass: the
/// agent's own `.bss` grew ~1.6 KiB (log-print ring, late-event buffers) so
/// `_stack_end` moved to `0x3FCB_75C0`, *above* the old base --
/// [`ram_window_is_safe`] would have (correctly) refused to run the
/// workload at all. This sits ~19 KiB above `_stack_end` (room for the
/// agent's `.bss` to keep growing) and ~53 KiB below the deepest stack use
/// ever measured (see the memory notes on `task_hwm_min`).
pub(crate) const WORKLOAD_RAM_BASE: u32 = 0x3FCB_C000;
/// Must match `crates/workload-poc/workload.ld`'s `WORKLOAD_RAM` region
/// `LENGTH` exactly -- generous for POC 2's one `u32` counter plus
/// `esp-hal`'s own small internal GPIO-driver static. Also fed to
/// `stack_usage::free_bytes()` as a known hole to skip over -- see this
/// module's doc comment and that function's for why zeroing this window
/// would otherwise permanently corrupt that measurement.
pub(crate) const WORKLOAD_RAM_SIZE: u32 = 2048;

/// How often the agent arms `workload_timer_isr` to fire. POC 2 (10 Hz,
/// functional validation) is frozen PASS -- this is now POC 2b's first
/// characterization tier, 1 kHz, per the design discussion ("si 1 kHz est
/// propre" gates whether 10 kHz is worth trying next). This is a load/
/// timing test, not an additional condition for the IRQ architecture gate
/// itself.
/// `exp-quiet-print` switches `monitor()` into POC 2d's measurement mode: no
/// per-window print, no local print at all during a measured run, one
/// report afterwards (see [`measure_loop`]).
const QUIET_PRINT: bool = cfg!(feature = "exp-quiet-print");
/// POC 2d: 10 kHz (was 1 ms at 1 kHz through POC 2b).
const TIMER_PERIOD: Duration = Duration::from_micros(100);

/// SYSTIMER's tick rate, duplicated from `crates/workload-poc/src/main.rs`
/// (same constant, same reasoning -- XTAL/2.5, 16 MHz on the standard
/// 40 MHz XTAL this board has) so [`monitor`] can convert the raw tick
/// counts `workload_timer_isr` records back into human-readable
/// microseconds.
const SYSTIMER_HZ: u64 = 16_000_000;

/// Byte offsets within [`WORKLOAD_RAM_BASE`] -- must be kept in sync by
/// hand with `crates/workload-poc/workload.ld`'s fixed `.bss` placement
/// (see that file's doc comment for why these are pinned rather than
/// compiler-chosen: POC 2's first version trusted `IRQ_COUNT` to stay at
/// `+0`, and a later, unrelated change silently moved it to `+4`).
const IRQ_COUNT_OFFSET: u32 = 0x00;
const INTERVAL_MIN_TICKS_OFFSET: u32 = 0x18;
const INTERVAL_MAX_TICKS_OFFSET: u32 = 0x1c;
const DURATION_MIN_TICKS_OFFSET: u32 = 0x20;
const DURATION_MAX_TICKS_OFFSET: u32 = 0x24;
/// Six windowed histogram buckets (POC 2d, 10 kHz: `<50us`, `50-90`,
/// `90-110`, `110-150`, `150-500`, `>500`), in ascending offset/boundary order
/// -- see `crates/workload-poc/workload.ld`'s doc comment for why these
/// exist alongside min/max: reset every monitoring cycle (`monitor`), so a
/// one-time startup anomaly can't hide a smaller, ongoing one behind an
/// unchanging cumulative extremum.
const BUCKET_OFFSETS: [u32; 6] = [0x28, 0x2c, 0x30, 0x34, 0x38, 0x3c];
const BUCKET_LABELS: [&str; 6] = ["<50us", "50-90us", "90-110us", "110-150us", "150-500us", ">500us"];
/// Sum of ISR durations (SYSTIMER ticks, u32), POC 2d -- see the workload
/// crate's `DURATION_SUM_TICKS`.
const DURATION_SUM_TICKS_OFFSET: u32 = 0x8c;

// ---- ISR entry-latency instrumentation (POC 2b, third pass) -- offsets
// must match `crates/workload-poc/workload.ld` exactly, same hand-kept
// discipline as above. ----
const ENTRY_LAT_BUCKETS_OFFSET: u32 = 0x40; // 8 x u32
const ENTRY_LAT_MAX_OFFSET: u32 = 0x60; // u32, TIMG1 ticks
const LATE_HEAD_OFFSET: u32 = 0x64; // ISR-owned
const LATE_TAIL_OFFSET: u32 = 0x68; // agent-owned
const LATE_DROPPED_OFFSET: u32 = 0x6c;
const LAT_THR_OFFSET: u32 = 0x70; // 7 x u32, TIMG1 ticks, agent-written
const LATE_RING_OFFSET: u32 = 0x90;
const LATE_RING_LEN: u32 = 64;
const ENTRY_LAT_LABELS: [&str; 8] =
    ["<10", "10-25", "25-50", "50-75", "75-100", "100-200", "200-500", ">500"];
/// Bucket boundaries in microseconds; converted to TIMG1 ticks once the
/// tick rate is known ([`init_entry_latency_tracking`]).
const ENTRY_LAT_BOUNDS_US: [u32; 7] = [10, 25, 50, 75, 100, 200, 500];

/// Base of the C3's interrupt controller (`INTERRUPT_CORE0` in the
/// `esp32c3` PAC -- not a PLIC on this chip). Layout verified against the
/// PAC's `interrupt_core0::RegisterBlock` (`core_0_intr_map: [_; 62]` at
/// +0x0, `cpu_int_enable` +0x104, `cpu_int_pri: [_; 32]` from +0x114,
/// `cpu_int_thresh` +0x194) and against `esp-hal`'s own `map_raw`, which
/// indexes the map array by peripheral-interrupt number.
const INTC_BASE: u32 = 0x600C_2000;
const INTC_CPU_INT_ENABLE: u32 = INTC_BASE + 0x104;
const INTC_CPU_INT_PRI0: u32 = INTC_BASE + 0x114;
const INTC_CPU_INT_THRESH: u32 = INTC_BASE + 0x194;
/// TIMG1's `T0ALARMLO` -- read back after `PeriodicTimer::start` to derive
/// the counter's real tick rate (alarm ticks / period) instead of assuming
/// a clock source and divider `esp-hal` chooses internally.
const TIMG1_T0ALARMLO: u32 = 0x6002_0000 + 0x10;

/// TIMG1 counter ticks per microsecond; `0` until
/// [`init_entry_latency_tracking`] has read it back from the hardware.
static TG1_TICKS_PER_US: AtomicU32 = AtomicU32::new(0);

/// One workload-recorded late event, mirrors `crates/workload-poc`'s
/// `LateEvent` (24 bytes: 20 of payload + 4 explicit padding, same field
/// order and `repr(C)`).
#[repr(C)]
#[derive(Clone, Copy)]
struct LateEvent {
    ts_ticks: u64,
    entry_ticks: u32,
    interval_ticks: u32,
    duration_ticks: u32,
    _pad: u32,
}

// ---- Agent-side record of every serial log print's critical-section
// hold time, to test the hypothesis that `esp_println`'s locked,
// busy-waiting-on-the-USB-FIFO write path is what masks the workload's
// interrupts. (`esp-println`'s default `critical-section` feature is on in
// this build -- confirmed with `cargo tree -f "{p} [{f}]" -i esp-println`
// -- so every `println!` runs under `esp_sync::RawMutex`, which on this
// single-core RISC-V target is `mstatus.MIE = 0`, and
// `write_bytes_in_cs` spins waiting for the 64-byte FIFO to drain to the
// host, up to 50 000 iterations, *inside* that lock.) Purely passive: two
// `Instant::now()` reads around the existing `println!` in
// `log_stream::Logger::log`, no change to what is printed. ----

#[derive(Clone, Copy)]
struct LogPrint {
    start_us: u64,
    end_us: u64,
    tag: [u8; 12],
    tag_len: u8,
}

struct LogPrints {
    q: Deque<LogPrint, 48>,
    dropped: u32,
}

static LOG_PRINTS: Mutex<RefCell<LogPrints>> = Mutex::new(RefCell::new(LogPrints {
    q: Deque::new(),
    dropped: 0,
}));

/// Set once [`map_and_run`] has finished (i.e. `esp_hal::init` and the
/// SYSTIMER are certainly live -- `Logger::log` can run before that, and
/// `Instant::now()` on an uninitialised SYSTIMER could hang).
static LOG_TIMING_ON: AtomicBool = AtomicBool::new(false);

/// `exp-quiet-print` counter-experiment: whether `log_stream` should skip the
/// local serial print for this record (periodic heartbeat/logs lines only,
/// and only once boot has finished). Always false in the default build.
pub(crate) fn quiet_print_suppresses(target: &str) -> bool {
    if !(cfg!(feature = "exp-quiet-print") && log_timing_enabled()) {
        return false;
    }
    // POC 2d: during a measured run *nothing* is printed locally, whatever
    // its origin (the lines are still forwarded to the outbound log ring).
    if MEASURING.load(Ordering::Relaxed) {
        SUPPRESSED_LINES.store(SUPPRESSED_LINES.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
        return true;
    }
    target.ends_with("::heartbeat") || target.ends_with("::log_stream")
}

/// True while a POC 2d measured run is in progress ([`measure_loop`]).
static MEASURING: AtomicBool = AtomicBool::new(false);
/// Approximate count (plain load/store, no atomic RMW on this target) of
/// log lines not printed because of [`MEASURING`].
static SUPPRESSED_LINES: AtomicU32 = AtomicU32::new(0);

// ---- Control-plane liveness counters (POC 2d): single writer each (the
// heartbeat task / the log-stream task), read by the monitor. ----
static HB_ATTEMPTS: AtomicU32 = AtomicU32::new(0);
static HB_OK: AtomicU32 = AtomicU32::new(0);
static LOGS_ATTEMPTS: AtomicU32 = AtomicU32::new(0);
static LOGS_OK: AtomicU32 = AtomicU32::new(0);
static LINK_SAMPLES: AtomicU32 = AtomicU32::new(0);
static LINK_DOWN_SAMPLES: AtomicU32 = AtomicU32::new(0);
static CONFIG_DOWN_SAMPLES: AtomicU32 = AtomicU32::new(0);

fn bump(a: &AtomicU32) {
    a.store(a.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
}

fn take_suppressed() -> u32 {
    critical_section::with(|_| {
        let v = SUPPRESSED_LINES.load(Ordering::Relaxed);
        SUPPRESSED_LINES.store(0, Ordering::Relaxed);
        v
    })
}

/// Called by `heartbeat::run` once per attempt.
pub(crate) fn note_heartbeat_result(ok: bool) {
    bump(&HB_ATTEMPTS);
    if ok {
        bump(&HB_OK);
    }
}

/// Called by `log_stream::run` once per session attempt.
pub(crate) fn note_logs_result(ok: bool) {
    bump(&LOGS_ATTEMPTS);
    if ok {
        bump(&LOGS_OK);
    }
}

/// Called by `heartbeat::run` every iteration: Wi-Fi link / IP-config state
/// as seen by the network stack.
pub(crate) fn note_link(link_up: bool, config_up: bool) {
    bump(&LINK_SAMPLES);
    if !link_up {
        bump(&LINK_DOWN_SAMPLES);
    }
    if !config_up {
        bump(&CONFIG_DOWN_SAMPLES);
    }
}

pub(crate) fn log_timing_enabled() -> bool {
    LOG_TIMING_ON.load(Ordering::Relaxed)
}

/// Records one serial print's `[start, end]` and the first 12 bytes of its
/// message (enough to tell `heartbeat:`/`logs:`/`Wi-Fi:`/`SNTP:`/
/// `workload POC` lines apart). Called by `log_stream::Logger::log`; must
/// never itself log.
pub(crate) fn note_log_print(start: Instant, end: Instant, msg: &str) {
    let mut tag = [0u8; 12];
    let n = msg.len().min(12);
    tag[..n].copy_from_slice(&msg.as_bytes()[..n]);
    let entry = LogPrint {
        start_us: start.duration_since_epoch().as_micros(),
        end_us: end.duration_since_epoch().as_micros(),
        tag,
        tag_len: n as u8,
    };
    critical_section::with(|cs| {
        let mut st = LOG_PRINTS.borrow(cs).borrow_mut();
        if st.q.push_back(entry).is_err() {
            st.dropped = st.dropped.wrapping_add(1);
        }
    });
}

fn prune_log_prints(now_us: u64) -> u32 {
    critical_section::with(|cs| {
        let mut st = LOG_PRINTS.borrow(cs).borrow_mut();
        while let Some(front) = st.q.front() {
            if front.end_us + 8_000_000 < now_us {
                st.q.pop_front();
            } else {
                break;
            }
        }
        core::mem::take(&mut st.dropped)
    })
}

/// A serial print whose critical-section hold time covers the delay window
/// `[alarm_us, entry_us]` (both boot-relative microseconds), within a
/// 100 us tolerance for the skew between when `Instant::now()` was read
/// and when the lock was actually taken/released.
fn find_covering_print(alarm_us: u64, entry_us: u64) -> Option<LogPrint> {
    const TOL_US: u64 = 100;
    critical_section::with(|cs| {
        let st = LOG_PRINTS.borrow(cs).borrow();
        st.q.iter()
            .find(|p| p.start_us <= alarm_us + TOL_US && p.end_us + TOL_US >= entry_us)
            .copied()
    })
}

unsafe extern "C" {
    /// ROM function, fixed at `0x400004d4` on ESP32-C3 (verified against
    /// `esp-idf/components/esp_rom/esp32c3/ld/esp32c3.rom.ld`) -- flushes
    /// any stale icache lines for the given virtual address range so the
    /// CPU doesn't execute garbage left over from whatever was cached at
    /// that address before this mapping existed (nothing, in this POC's
    /// case, since entry 100 was unused -- but the invalidation is
    /// unconditionally correct regardless).
    fn Cache_Invalidate_Addr(addr: u32, size: u32);

    /// Duplicated from `stack_usage.rs` (same symbol, same linker-provided
    /// address, no real storage behind it -- see that module's doc comment
    /// for why it's only ever read via `&raw const`, never dereferenced).
    /// Not imported from there directly because `stack_usage` doesn't
    /// expose it publicly (by design: `free_bytes()` is the only thing
    /// other modules should need) -- this module is the one exception that
    /// needs the raw address itself, to place [`WORKLOAD_RAM_BASE`]
    /// relative to it.
    static _stack_end: u8;
}

/// Raw volatile read of one MMU table entry.
#[inline]
fn read_entry(entry: u32) -> u32 {
    // SAFETY: `entry` is checked `< MMU_ENTRY_COUNT` by every caller in
    // this module before this is reached.
    unsafe { core::ptr::read_volatile((MMU_TABLE_BASE + entry * 4) as *const u32) }
}

/// Raw volatile write of one MMU table entry.
#[inline]
fn write_entry(entry: u32, value: u32) {
    // SAFETY: same as `read_entry`.
    unsafe { core::ptr::write_volatile((MMU_TABLE_BASE + entry * 4) as *mut u32, value) };
}

/// Verifies the entries this POC is about to claim are genuinely free
/// (`MMU_INVALID` set, i.e. not already mapped by the agent's own boot-time
/// setup) before touching anything -- refuses to silently overwrite a live
/// mapping the agent itself depends on.
fn entries_are_free() -> bool {
    for i in 0..WORKLOAD_MMU_PAGES {
        let entry = WORKLOAD_MMU_ENTRY + i;
        if entry >= MMU_ENTRY_COUNT {
            error!("workload POC: entry {entry} is out of range (table has {MMU_ENTRY_COUNT})");
            return false;
        }
        let value = read_entry(entry);
        if value & MMU_INVALID == 0 {
            error!(
                "workload POC: MMU entry {entry} is already in use (raw value {value:#x}) -- \
                 refusing to overwrite it. The agent's own IROM/DROM footprint has likely grown \
                 past WORKLOAD_MMU_ENTRY; bump that constant."
            );
            return false;
        }
    }
    true
}

/// Verifies [`WORKLOAD_RAM_BASE`]/[`WORKLOAD_RAM_SIZE`] still fall entirely
/// inside stack headroom `stack_usage::free_bytes()` measures as genuinely
/// untouched *right now* -- refuses to zero/use that window blind if the
/// agent's own stack usage has grown enough to make the constant unsafe
/// (rather than silently risking corruption of live stack data).
fn ram_window_is_safe() -> bool {
    let stack_end = &raw const _stack_end as u32;
    if WORKLOAD_RAM_BASE < stack_end {
        error!(
            "workload POC 2: WORKLOAD_RAM_BASE {WORKLOAD_RAM_BASE:#x} is below _stack_end \
             {stack_end:#x} -- constant is stale, refusing to touch it."
        );
        return false;
    }
    let offset_into_free = WORKLOAD_RAM_BASE - stack_end;
    let free = crate::stack_usage::free_bytes();
    match offset_into_free.checked_add(WORKLOAD_RAM_SIZE) {
        Some(end) if end <= free => true,
        _ => {
            error!(
                "workload POC 2: RAM window [{WORKLOAD_RAM_BASE:#x}, +{WORKLOAD_RAM_SIZE}) \
                 reaches {offset_into_free}+{WORKLOAD_RAM_SIZE} bytes past _stack_end, but only \
                 {free} bytes of real stack headroom are currently measured free -- refusing to \
                 use it. The agent's own stack usage has likely grown; bump WORKLOAD_RAM_BASE."
            );
            false
        }
    }
}

/// Explicit runtime initialization of the workload's `.data`/`.bss` --
/// `crates/workload-poc` never runs a startup sequence that would zero this
/// itself (see that crate's doc comment), so the agent does it, deliberately,
/// rather than relying on power-on-zero or an accident of what was there
/// before.
fn zero_ram_window() {
    // SAFETY: `ram_window_is_safe` (checked by every caller before this) has
    // just confirmed this exact range is inside currently-unused stack
    // headroom.
    unsafe {
        core::slice::from_raw_parts_mut(WORKLOAD_RAM_BASE as *mut u8, WORKLOAD_RAM_SIZE as usize)
            .fill(0);
    }
}

/// Reads the workload's `IRQ_COUNT` directly -- real RAM state at a fixed,
/// agent-owned address, no call into workload code needed (mirrors how
/// `crates/workload-poc`'s `workload_timer_isr` writes it: a plain volatile
/// access, no exported accessor function on either side).
pub fn irq_count() -> u32 {
    // SAFETY: `WORKLOAD_RAM_BASE + IRQ_COUNT_OFFSET` holds `IRQ_COUNT` (see
    // `crates/workload-poc/workload.ld`'s fixed `.bss` layout) once
    // `map_and_run` has zeroed and the workload has been given a chance to
    // run -- reading it is safe at any time after that (single-writer, from
    // `workload_timer_isr`, never concurrently read-modified by anything
    // else).
    unsafe { core::ptr::read_volatile((WORKLOAD_RAM_BASE + IRQ_COUNT_OFFSET) as *const u32) }
}

fn read_ram_u32(offset: u32) -> u32 {
    // SAFETY: same as `irq_count` -- a fixed offset within the zeroed,
    // agent-owned RAM window, single-writer from `workload_timer_isr`.
    unsafe { core::ptr::read_volatile((WORKLOAD_RAM_BASE + offset) as *const u32) }
}

fn ticks_to_us(ticks: u32) -> u32 {
    ((ticks as u64 * 1_000_000) / SYSTIMER_HZ) as u32
}

/// `(min, max)` `workload_timer_isr` duration, in microseconds -- includes
/// the ISR's own SYSTIMER-read measurement overhead, see that crate's doc
/// comment for why a lower-overhead clock isn't available here.
pub fn duration_range_us() -> (u32, u32) {
    (
        ticks_to_us(read_ram_u32(DURATION_MIN_TICKS_OFFSET)),
        ticks_to_us(read_ram_u32(DURATION_MAX_TICKS_OFFSET)),
    )
}

fn write_ram_u32(offset: u32, value: u32) {
    // SAFETY: same as `read_ram_u32` -- a fixed offset within the zeroed,
    // agent-owned RAM window. Only ever called from [`take_interval_window`],
    // itself only called from inside a `critical_section::with` (see there
    // for why).
    unsafe { core::ptr::write_volatile((WORKLOAD_RAM_BASE + offset) as *mut u32, value) };
}

/// Reads this cycle's interval min/max/bucket-counts, then resets all
/// seven fields for the next cycle -- `workload_timer_isr` keeps updating
/// them the instant this returns. Wrapped in a `critical_section` (briefly
/// disabling interrupts -- the same primitive `crates/workload-poc`'s own
/// `disable_interrupts`/`restore_interrupts` implement by hand, available
/// here as the ordinary `critical-section` crate since this is agent code,
/// not workload code) so a `workload_timer_isr` firing *during* the
/// read-then-reset sequence can't observe or produce a torn state (e.g. a
/// bucket incremented against an already-reset min but not-yet-reset max).
/// At 1-10 kHz the whole sequence is a handful of volatile accesses, on the
/// order of a microsecond -- negligible next to the interrupt period, even
/// disabled.
struct WindowStats {
    interval_min_us: u32,
    interval_max_us: u32,
    interval_buckets: [u32; 6],
    entry_buckets: [u32; 8],
    entry_max_ticks: u32,
    /// ISR duration min / max / sum over this window, SYSTIMER ticks (POC
    /// 2d: reset every window now, they used to be cumulative since boot).
    duration_min_ticks: u32,
    duration_max_ticks: u32,
    duration_sum_ticks: u32,
}

fn take_interval_window() -> WindowStats {
    critical_section::with(|_cs| {
        let stats = WindowStats {
            interval_min_us: ticks_to_us(read_ram_u32(INTERVAL_MIN_TICKS_OFFSET)),
            interval_max_us: ticks_to_us(read_ram_u32(INTERVAL_MAX_TICKS_OFFSET)),
            interval_buckets: BUCKET_OFFSETS.map(read_ram_u32),
            entry_buckets: core::array::from_fn(|i| {
                read_ram_u32(ENTRY_LAT_BUCKETS_OFFSET + 4 * i as u32)
            }),
            entry_max_ticks: read_ram_u32(ENTRY_LAT_MAX_OFFSET),
            duration_min_ticks: read_ram_u32(DURATION_MIN_TICKS_OFFSET),
            duration_max_ticks: read_ram_u32(DURATION_MAX_TICKS_OFFSET),
            duration_sum_ticks: read_ram_u32(DURATION_SUM_TICKS_OFFSET),
        };

        write_ram_u32(INTERVAL_MIN_TICKS_OFFSET, u32::MAX);
        write_ram_u32(INTERVAL_MAX_TICKS_OFFSET, 0);
        for &offset in &BUCKET_OFFSETS {
            write_ram_u32(offset, 0);
        }
        for i in 0..8 {
            write_ram_u32(ENTRY_LAT_BUCKETS_OFFSET + 4 * i, 0);
        }
        write_ram_u32(ENTRY_LAT_MAX_OFFSET, 0);
        write_ram_u32(DURATION_MIN_TICKS_OFFSET, u32::MAX);
        write_ram_u32(DURATION_MAX_TICKS_OFFSET, 0);
        write_ram_u32(DURATION_SUM_TICKS_OFFSET, 0);

        stats
    })
}

/// Derives TIMG1's counter tick rate from the alarm register
/// `PeriodicTimer::start` just programmed (alarm ticks / period in
/// microseconds) -- the workload can't derive it itself (no
/// `Clocks::get()`, see POC 1), and `esp-hal` picks the timer's clock
/// source and divider internally -- then writes the entry-latency bucket
/// boundaries (in TIMG1 ticks) into the workload's RAM. The boundary the
/// ISR checks for "initialised" (`[0]`) is written last, so the ISR never
/// sees a half-filled table.
fn init_entry_latency_tracking() {
    // SAFETY: read-only access to a TIMG1 register `PeriodicTimer::start`
    // has just written.
    let alarm = unsafe { core::ptr::read_volatile(TIMG1_T0ALARMLO as *const u32) };
    let period_us = TIMER_PERIOD.as_micros() as u32;
    let ticks_per_us = alarm / period_us.max(1);
    if ticks_per_us == 0 {
        error!(
            "workload POC 2b: TIMG1 alarm register reads {alarm} for a {period_us} us period -- \
             can't derive a tick rate, entry-latency tracking stays off"
        );
        return;
    }
    TG1_TICKS_PER_US.store(ticks_per_us, Ordering::Relaxed);
    for i in (0..7u32).rev() {
        write_ram_u32(
            LAT_THR_OFFSET + 4 * i,
            ENTRY_LAT_BOUNDS_US[i as usize] * ticks_per_us,
        );
    }
    info!(
        "workload POC 2b: TIMG1 counter = {ticks_per_us} ticks/us (alarm register {alarm} for a \
         {period_us} us period); entry-latency bucket boundaries written"
    );
}

/// One line, logged once (first monitoring cycle, i.e. after Wi-Fi has
/// mapped its own interrupts): which CPU interrupt each peripheral
/// interrupt is actually routed to, and that CPU interrupt's hardware
/// priority, enable bit, plus the global threshold -- read straight from
/// the interrupt controller, not inferred from the `esp-hal` calls that
/// were supposed to set it.
fn log_interrupt_routing() {
    fn rd(addr: u32) -> u32 {
        // SAFETY: read-only access to the interrupt controller's registers.
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }
    let enable = rd(INTC_CPU_INT_ENABLE);
    let route = |name: &str, source: Interrupt, out: &mut heapless::String<300>| {
        let cpu = rd(INTC_BASE + 4 * source as u32) & 0x1f;
        let pri = rd(INTC_CPU_INT_PRI0 + 4 * cpu) & 0xf;
        let en = (enable >> cpu) & 1;
        let _ = core::fmt::write(
            out,
            format_args!(" {name} -> cpu_intr={cpu} priority={pri} enabled={en};"),
        );
    };
    let mut line = heapless::String::<300>::new();
    route("TG1_T0(workload)", Interrupt::TG1_T0_LEVEL, &mut line);
    route("TG0_T0(esp-rtos tick)", Interrupt::TG0_T0_LEVEL, &mut line);
    route("WIFI_MAC", Interrupt::WIFI_MAC, &mut line);
    route("WIFI_PWR", Interrupt::WIFI_PWR, &mut line);
    info!(
        "workload POC 2b: interrupt routing (read from INTERRUPT_CORE0):{line} threshold={}",
        rd(INTC_CPU_INT_THRESH) & 0xf
    );
}

/// Maps `crates/workload-poc`'s flash bytes executable, calls its
/// `workload_entry()` once, then (POC 2) arms a TIMG1 interrupt bound
/// directly to its `workload_timer_isr`. Logs every measurement point
/// called out in the POC plan. Never panics: a failure here is logged and
/// skipped, the agent's own boot continues either way -- this is meant to
/// coexist with, never threaten, the agent staying alive.
pub fn map_and_run(timg1: TIMG1<'static>) {
    info!(
        "workload POC: mapping entry {WORKLOAD_MMU_ENTRY}..{} \
         (phys offset {WORKLOAD_PHYS_OFFSET:#x}, {WORKLOAD_MMU_PAGES} page(s) of \
         {MMU_PAGE_SIZE:#x} bytes) -> vaddr {WORKLOAD_VADDR:#x}",
        WORKLOAD_MMU_ENTRY + WORKLOAD_MMU_PAGES
    );

    if !entries_are_free() {
        return;
    }

    let phys_page_base = WORKLOAD_PHYS_OFFSET / MMU_PAGE_SIZE;
    for i in 0..WORKLOAD_MMU_PAGES {
        // On the C3, `SOC_MMU_ACCESS_FLASH` and `SOC_MMU_VALID` are both
        // defined as 0 -- the bare physical page number *is* the full
        // "valid, flash-backed" entry value. See this module's doc
        // comment.
        write_entry(WORKLOAD_MMU_ENTRY + i, phys_page_base + i);
    }

    // SAFETY: `Cache_Invalidate_Addr` is a ROM function present on every
    // ESP32-C3 unconditionally; the address/size describe exactly the
    // window just mapped above.
    unsafe { Cache_Invalidate_Addr(WORKLOAD_VADDR, WORKLOAD_MMU_PAGES * MMU_PAGE_SIZE) };
    // `fence.i` so the CPU's instruction-fetch pipeline can't have any
    // stale prefetch from this address range predating the mapping/cache
    // invalidation above -- standard practice on RISC-V after modifying
    // what's executable at an address.
    unsafe { core::arch::asm!("fence.i") };

    if !ram_window_is_safe() {
        return;
    }
    zero_ram_window();

    info!("workload POC: calling workload_entry() at {WORKLOAD_VADDR:#x}");
    // SAFETY: `WORKLOAD_VADDR` was just mapped, cache-invalidated, and
    // fenced above to point at `crates/workload-poc`'s `workload_entry`,
    // whose linker script (`workload.ld`) pins it to this exact address
    // with a matching `extern "C" fn() -> i32` signature -- see that
    // crate's doc comment. `WORKLOAD_RAM_BASE` was just zeroed, so
    // `workload_entry`'s own `.bss` (`IRQ_COUNT`) starts at a known value.
    let workload_entry: extern "C" fn() -> i32 =
        unsafe { core::mem::transmute(WORKLOAD_VADDR as usize) };
    let result = workload_entry();
    info!("workload POC: workload_entry() returned {result}, agent continuing boot");

    // POC 2: bind workload_timer_isr (same mapped window, fixed offset --
    // see this module's doc comment) to TIMG1's timer0, and arm it.
    let timer_isr: extern "C" fn() =
        unsafe { core::mem::transmute(WORKLOAD_TIMER_ISR_VADDR as usize) };

    let tg1 = TimerGroup::new(timg1);
    // `PeriodicTimer` must outlive this function for the interrupt to keep
    // firing -- `StaticCell` gives it a `'static` home, same pattern this
    // codebase already uses elsewhere (e.g. `http/config.rs`'s `LPWR_CELL`).
    static PERIODIC_TIMER: StaticCell<PeriodicTimer<'static, esp_hal::Blocking>> =
        StaticCell::new();
    let periodic = PERIODIC_TIMER.init(PeriodicTimer::new(tg1.timer0));
    periodic.set_interrupt_handler(InterruptHandler::new(timer_isr, Priority::Priority2));
    periodic.listen();
    if let Err(e) = periodic.start(TIMER_PERIOD) {
        error!("workload POC 2: failed to start TIMG1 periodic timer: {e:?}");
        return;
    }
    info!(
        "workload POC 2: TIMG1 armed at {:?} period, workload_timer_isr bound at {WORKLOAD_TIMER_ISR_VADDR:#x}",
        TIMER_PERIOD
    );
    init_entry_latency_tracking();
    // From here on `Logger::log` may read SYSTIMER (`Instant::now`) around
    // each serial print -- `esp_hal::init` has certainly run by now.
    LOG_TIMING_ON.store(true, Ordering::Relaxed);
}

/// Bytes of a log print's recorded tag that are valid UTF-8 (a 12-byte cut
/// can land mid-character on the `µs` in some lines).
fn tag_str(p: &LogPrint) -> &str {
    let bytes = &p.tag[..p.tag_len as usize];
    match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""),
    }
}

// ---- Agent-activity markers (POC 2b, fourth pass): when each periodic
// agent activity was running, on the same monotone boot-relative
// microsecond clock as the workload's late events (`Instant::now()` and
// the workload's SYSTIMER stamps share SYSTIMER unit 0). Recorded in RAM by
// a scope guard around the activity -- *no* serial print, deliberately: the
// point is to stop measuring `println!` by adding `println!`.
//
// `Monitor`, `Heartbeat` and `Logs` are the three agent tasks that run on a
// ~5 s cycle. `Storage` is an addition beyond that list: every
// `Storage::get`/`set`/`delete`/`cfg_entries` first rebuilds an `Nvs`
// (`Storage::nvs`), which rescans the whole NVS partition in flash, and
// `heartbeat`/`logs` do that every iteration (`agent::ctrl_url`, then the
// CA/cert lookups in `tls::connect_client`) -- a periodic flash access, the
// kind of thing that can stall an interrupt handler executing from XIP
// flash. It is only a marker, not a claim about cause. ----

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activity {
    Monitor = 0,
    Heartbeat = 1,
    Logs = 2,
    Storage = 3,
}

#[derive(Clone, Copy)]
struct Span {
    start_us: u64,
    end_us: u64,
}

struct Marks {
    /// Closed spans, most recent last; the oldest is dropped when full.
    spans: [Deque<Span, 32>; 4],
    /// Start of the span currently in progress, if any.
    open: [Option<u64>; 4],
}

static MARKS: Mutex<RefCell<Marks>> = Mutex::new(RefCell::new(Marks {
    spans: [const { Deque::new() }; 4],
    open: [None; 4],
}));

fn now_us() -> u64 {
    Instant::now().duration_since_epoch().as_micros()
}

/// Records `[begin, drop]` of one activity iteration. A no-op until
/// [`map_and_run`] has finished (before that `esp_hal::init`/SYSTIMER
/// aren't guaranteed live, and `Storage` is used earlier in boot).
pub(crate) struct ActivityGuard {
    kind: Activity,
    live: bool,
}

impl ActivityGuard {
    pub(crate) fn begin(kind: Activity) -> Self {
        if !log_timing_enabled() {
            return Self { kind, live: false };
        }
        let start = now_us();
        critical_section::with(|cs| {
            MARKS.borrow(cs).borrow_mut().open[kind as usize] = Some(start);
        });
        Self { kind, live: true }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let end = now_us();
        let k = self.kind as usize;
        critical_section::with(|cs| {
            let mut m = MARKS.borrow(cs).borrow_mut();
            if let Some(start_us) = m.open[k].take() {
                if m.spans[k].is_full() {
                    m.spans[k].pop_front();
                }
                let _ = m.spans[k].push_back(Span { start_us, end_us: end });
            }
        });
    }
}

/// A zero-length span at "now" -- gives the very first monitoring window a
/// last-start to measure against.
fn mark_point(kind: Activity) {
    let t = now_us();
    critical_section::with(|cs| {
        let mut m = MARKS.borrow(cs).borrow_mut();
        let k = kind as usize;
        let _ = m.spans[k].push_back(Span { start_us: t, end_us: t });
    });
}

fn prune_marks(now: u64) {
    critical_section::with(|cs| {
        let mut m = MARKS.borrow(cs).borrow_mut();
        for q in m.spans.iter_mut() {
            while let Some(front) = q.front() {
                if front.end_us + 12_000_000 < now {
                    q.pop_front();
                } else {
                    break;
                }
            }
        }
    });
}


// ---- Flash-access spans (POC 2b, step A). `storage::TimedFlash` records
// `[start, end]` of every NVS flash `read`/`write`/`erase` call here, in a
// lock-free single-producer / single-consumer ring: no allocation, no print
// and *no critical section* (the producer is the one task holding `&mut
// Storage`; the consumer is `monitor`). Purpose: for each late workload
// interrupt, test whether its *due time* (`entry - entry_latency`) fell
// inside a flash access -- if `esp-storage` masks interrupts around the ROM
// read, an alarm due inside the access can only be serviced after its end.
// A no-op until [`map_and_run`] has run (same gate as the other markers). ----

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlashOp {
    Read = 0,
    Write = 1,
    Erase = 2,
}

#[derive(Clone, Copy)]
struct FlashSpan {
    start_us: u64,
    end_us: u64,
    /// Recorded for post-hoc inspection (debugger); not used by the summary.
    #[allow(dead_code)]
    offset: u32,
    len: u32,
    op: FlashOp,
}

const FLASH_RING_LEN: usize = 96;

struct FlashRing {
    slots: core::cell::UnsafeCell<[FlashSpan; FLASH_RING_LEN]>,
    /// Written only by the producer (`flash_span_end`).
    head: AtomicU32,
    /// Written only by the consumer (`take_flash_spans`).
    tail: AtomicU32,
    dropped: AtomicU32,
}

// SAFETY: single producer / single consumer; a slot is only read after
// `head` (Release) has advanced past it and only rewritten after `tail`
// (Release) has advanced beyond it.
unsafe impl Sync for FlashRing {}

static FLASH_RING: FlashRing = FlashRing {
    slots: core::cell::UnsafeCell::new(
        [FlashSpan { start_us: 0, end_us: 0, offset: 0, len: 0, op: FlashOp::Read }; FLASH_RING_LEN],
    ),
    head: AtomicU32::new(0),
    tail: AtomicU32::new(0),
    dropped: AtomicU32::new(0),
};


// ---- Aggregate flash-I/O counters (POC 2d follow-up): cumulative,
// producer-written (the task holding `&mut Storage`), so they cannot
// overflow like the 96-slot span ring can; the monitor takes deltas between
// two snapshots. Plain load/store (no atomic RMW on this target). The three
// `*_max_us` are per-run: the monitor bumps `epoch`, the producer resets them
// the first time it sees the new value. ----

const FLASH_LEN_LABELS: [&str; 6] = ["<=64B", "65-256B", "257-512B", "513-1024B", "1025-4095B", "4096B"];
const FLASH_DUR_LABELS: [&str; 6] = ["<50us", "50-100us", "100-200us", "200-500us", "500-1000us", ">1000us"];

struct FlashStats {
    reads: AtomicU32,
    len: [AtomicU32; 6],
    /// Summed read duration per length bucket, us (mean = this / `len`).
    len_us: [AtomicU32; 6],
    dur: [AtomicU32; 6],
    read_us: AtomicU32,
    writes: AtomicU32,
    erases: AtomicU32,
    read_max_us: AtomicU32,
    write_max_us: AtomicU32,
    erase_max_us: AtomicU32,
    /// Consumer-written; producer compares with `seen_epoch`.
    epoch: AtomicU32,
    seen_epoch: AtomicU32,
}

static FLASH_STATS: FlashStats = FlashStats {
    reads: AtomicU32::new(0),
    len: [const { AtomicU32::new(0) }; 6],
    len_us: [const { AtomicU32::new(0) }; 6],
    dur: [const { AtomicU32::new(0) }; 6],
    read_us: AtomicU32::new(0),
    writes: AtomicU32::new(0),
    erases: AtomicU32::new(0),
    read_max_us: AtomicU32::new(0),
    write_max_us: AtomicU32::new(0),
    erase_max_us: AtomicU32::new(0),
    epoch: AtomicU32::new(0),
    seen_epoch: AtomicU32::new(0),
};

fn add(a: &AtomicU32, n: u32) {
    a.store(a.load(Ordering::Relaxed).wrapping_add(n), Ordering::Relaxed);
}

fn max_to(a: &AtomicU32, v: u32) {
    if v > a.load(Ordering::Relaxed) {
        a.store(v, Ordering::Relaxed);
    }
}

impl FlashStats {
    fn record(&self, op: FlashOp, len: u32, dur_us: u32) {
        let e = self.epoch.load(Ordering::Relaxed);
        if e != self.seen_epoch.load(Ordering::Relaxed) {
            self.seen_epoch.store(e, Ordering::Relaxed);
            self.read_max_us.store(0, Ordering::Relaxed);
            self.write_max_us.store(0, Ordering::Relaxed);
            self.erase_max_us.store(0, Ordering::Relaxed);
        }
        match op {
            FlashOp::Read => {
                let li = match len {
                    0..=64 => 0,
                    65..=256 => 1,
                    257..=512 => 2,
                    513..=1024 => 3,
                    1025..=4095 => 4,
                    _ => 5,
                };
                let di = match dur_us {
                    0..=49 => 0,
                    50..=99 => 1,
                    100..=199 => 2,
                    200..=499 => 3,
                    500..=999 => 4,
                    _ => 5,
                };
                add(&self.reads, 1);
                add(&self.len[li], 1);
                add(&self.len_us[li], dur_us);
                add(&self.dur[di], 1);
                add(&self.read_us, dur_us);
                max_to(&self.read_max_us, dur_us);
            }
            FlashOp::Write => {
                add(&self.writes, 1);
                max_to(&self.write_max_us, dur_us);
            }
            FlashOp::Erase => {
                add(&self.erases, 1);
                max_to(&self.erase_max_us, dur_us);
            }
        }
    }

    fn snapshot(&self) -> FlashSnap {
        let ld = |a: &AtomicU32| a.load(Ordering::Relaxed);
        FlashSnap {
            reads: ld(&self.reads),
            len: core::array::from_fn(|i| ld(&self.len[i])),
            len_us: core::array::from_fn(|i| ld(&self.len_us[i])),
            dur: core::array::from_fn(|i| ld(&self.dur[i])),
            read_us: ld(&self.read_us),
            writes: ld(&self.writes),
            erases: ld(&self.erases),
            ring_dropped: FLASH_RING.dropped.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
struct FlashSnap {
    reads: u32,
    len: [u32; 6],
    len_us: [u32; 6],
    dur: [u32; 6],
    read_us: u32,
    writes: u32,
    erases: u32,
    ring_dropped: u32,
}

/// Start stamp of one flash access, `None` while recording is off.
pub(crate) fn flash_span_begin() -> Option<u64> {
    log_timing_enabled().then(now_us)
}

pub(crate) fn flash_span_end(start: Option<u64>, op: FlashOp, offset: u32, len: u32) {
    let Some(start_us) = start else { return };
    let end_us = now_us();
    FLASH_STATS.record(op, len, end_us.saturating_sub(start_us) as u32);
    let head = FLASH_RING.head.load(Ordering::Relaxed);
    let tail = FLASH_RING.tail.load(Ordering::Acquire);
    if head.wrapping_sub(tail) as usize >= FLASH_RING_LEN {
        // Producer-only cumulative counter (no atomic RMW on this target).
        FLASH_RING.dropped.store(FLASH_RING.dropped.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
        return;
    }
    // SAFETY: slot `head` is not visible to the consumer until `head` is
    // published below, and the ring is not full.
    unsafe {
        (FLASH_RING.slots.get() as *mut FlashSpan)
            .add(head as usize % FLASH_RING_LEN)
            .write(FlashSpan { start_us, end_us, offset, len, op });
    }
    FLASH_RING.head.store(head.wrapping_add(1), Ordering::Release);
}

/// Moves every recorded span into `out`; returns how many were new.
fn take_flash_spans(out: &mut alloc::vec::Vec<FlashSpan>) -> usize {
    let head = FLASH_RING.head.load(Ordering::Acquire);
    let mut tail = FLASH_RING.tail.load(Ordering::Relaxed);
    let mut n = 0;
    while tail != head {
        // SAFETY: published by the producer before `head` advanced.
        let sp = unsafe { (FLASH_RING.slots.get() as *const FlashSpan).add(tail as usize % FLASH_RING_LEN).read() };
        out.push(sp);
        tail = tail.wrapping_add(1);
        n += 1;
    }
    FLASH_RING.tail.store(tail, Ordering::Release);
    n
}

/// Late-event (`alarm_us`, `entry_us`, `lat_us`) against the flash spans.
type LateRec = (u64, u64, u32);

/// A: due inside a flash access and entry within this of its end.
const FLASH_A_MAX_RESIDUAL_US: i64 = 100;
const FLASH_CLASS_LABELS: [&str; 4] = ["100-500us", "500-1000us", "1-2ms", ">2ms"];

fn flash_class(lat_us: u32) -> usize {
    match lat_us {
        0..=499 => 0,
        500..=999 => 1,
        1000..=1999 => 2,
        _ => 3,
    }
}

#[derive(Default)]
struct FlashReport {
    // Accesses recorded since the previous window.
    reads: u32,
    writes: u32,
    erases: u32,
    dropped: u32,
    read_dur: [u32; 3], // min / median / max us
    read_total_us: u64,
    /// len <=64, <=512, <4096, ==4096, >4096
    read_len: [u32; 5],
    // Late events.
    events: u32,
    due_in: [u32; 3], // read / write / erase
    due_out: u32,
    /// Per latency class: [events, A, B, C].
    class: [[u32; 4]; 4],
    residual: [i64; 3], // min / median / max over events with due inside
    residual_n: u32,
    entry_in_span: u32,
    spans_hit: u32,
}

fn median<T: Copy + Ord>(v: &mut [T]) -> T {
    v.sort_unstable();
    v[v.len() / 2]
}

fn flash_analyze(
    events: &[LateRec],
    spans: &[FlashSpan],
    new_spans: usize,
    dropped_total: u32,
) -> FlashReport {
    let mut r = FlashReport::default();
    r.dropped = dropped_total;
    let mut durs = alloc::vec::Vec::new();
    for sp in &spans[spans.len() - new_spans..] {
        match sp.op {
            FlashOp::Read => {
                r.reads += 1;
                let d = (sp.end_us - sp.start_us) as u32;
                durs.push(d);
                r.read_total_us += d as u64;
                let b = match sp.len {
                    0..=64 => 0,
                    65..=512 => 1,
                    513..=4095 => 2,
                    4096 => 3,
                    _ => 4,
                };
                r.read_len[b] += 1;
            }
            FlashOp::Write => r.writes += 1,
            FlashOp::Erase => r.erases += 1,
        }
    }
    if !durs.is_empty() {
        let (mn, mx) = (*durs.iter().min().unwrap(), *durs.iter().max().unwrap());
        r.read_dur = [mn, median(&mut durs), mx];
    }
    let mut hit = alloc::vec![false; spans.len()];
    let mut residuals = alloc::vec::Vec::new();
    for &(alarm, entry, lat) in events {
        r.events += 1;
        let c = flash_class(lat);
        r.class[c][0] += 1;
        if spans.iter().any(|s| s.start_us <= entry && entry <= s.end_us) {
            r.entry_in_span += 1;
        }
        match spans.iter().position(|s| s.start_us <= alarm && alarm <= s.end_us) {
            Some(i) => {
                hit[i] = true;
                r.due_in[spans[i].op as usize] += 1;
                let res = entry as i64 - spans[i].end_us as i64;
                residuals.push(res);
                r.class[c][if res <= FLASH_A_MAX_RESIDUAL_US { 1 } else { 3 }] += 1;
            }
            None => {
                r.due_out += 1;
                r.class[c][2] += 1;
            }
        }
    }
    r.spans_hit = hit.iter().filter(|h| **h).count() as u32;
    if !residuals.is_empty() {
        let (mn, mx) = (*residuals.iter().min().unwrap(), *residuals.iter().max().unwrap());
        r.residual_n = residuals.len() as u32;
        r.residual = [mn, median(&mut residuals), mx];
    }
    r
}

// ---- B events vs serial-print spans (POC 2b, step A2). B = late event
// (>= 100 us) whose due time is inside *no* NVS flash span. For each, is the
// due time inside a `serial_print` span (`log_stream` -> `note_log_print`,
// the existing ring -- no new print in the measured path), and how long
// after that print's end stamp did the ISR enter? Print spans are wider than
// the real `esp_println` critical section (they include the lock wait and
// formatting), so "inside" is necessary, not sufficient. ----

const PRINT_KINDS: [&str; 4] = ["monitor", "heartbeat", "logs", "other"];
/// |residual_print| within this = P1 ("entry right after the print's end").
const PRINT_P1_MAX_RESIDUAL_US: i64 = 100;

fn print_kind(p: &LogPrint) -> usize {
    let t = tag_str(p);
    if t.starts_with("workload POC") {
        0
    } else if t.starts_with("heartbeat:") {
        1
    } else if t.starts_with("logs:") {
        2
    } else {
        3
    }
}

fn snapshot_log_prints() -> alloc::vec::Vec<LogPrint> {
    critical_section::with(|cs| LOG_PRINTS.borrow(cs).borrow().q.iter().copied().collect())
}

#[derive(Clone, Copy)]
struct BWorst {
    lat_us: u32,
    due_us: u64,
    entry_us: u64,
    /// (start, end, kind) of the print containing `due_us`, if any.
    print: Option<(u64, u64, usize)>,
    residual_us: Option<i64>,
    since_monitor_start_us: Option<u64>,
}

#[derive(Default)]
struct BReport {
    total: u32,
    inside_print: u32,
    outside_print: u32,
    /// Due inside a print of kind [monitor, heartbeat, logs, other].
    kind: [u32; 4],
    /// Per latency class: [n, P1, P2, P3, P4].
    class: [[u32; 5]; 4],
    residual: [i64; 3],
    residual_n: u32,
    worst: [Option<BWorst>; 3],
}

fn b_analyze(events: &[LateRec], flash: &[FlashSpan], prints: &[LogPrint]) -> BReport {
    let mut r = BReport::default();
    let mut residuals = alloc::vec::Vec::new();
    for &(alarm, entry, lat) in events {
        if flash.iter().any(|s| s.start_us <= alarm && alarm <= s.end_us) {
            continue; // an A/C event, not a B.
        }
        r.total += 1;
        let c = flash_class(lat);
        r.class[c][0] += 1;
        let hit = prints.iter().find(|p| p.start_us <= alarm && alarm <= p.end_us);
        let (print, residual) = match hit {
            Some(p) => {
                let res = entry as i64 - p.end_us as i64;
                r.inside_print += 1;
                r.kind[print_kind(p)] += 1;
                residuals.push(res);
                // P1: entry right after the end; P3: entry well before it;
                // P4: entry long after it.
                let cls = if res.abs() <= PRINT_P1_MAX_RESIDUAL_US {
                    1
                } else if res < 0 {
                    3
                } else {
                    4
                };
                r.class[c][cls] += 1;
                (Some((p.start_us, p.end_us, print_kind(p))), Some(res))
            }
            None => {
                r.outside_print += 1;
                r.class[c][2] += 1;
                (None, None)
            }
        };
        let cand = BWorst {
            lat_us: lat,
            due_us: alarm,
            entry_us: entry,
            print,
            residual_us: residual,
            since_monitor_start_us: lookup(Activity::Monitor, alarm, entry).since_start_us,
        };
        if let Some(i) = r.worst.iter().position(|w| w.map_or(true, |w| cand.lat_us > w.lat_us)) {
            for j in (i + 1..3).rev() {
                r.worst[j] = r.worst[j - 1];
            }
            r.worst[i] = Some(cand);
        }
    }
    if !residuals.is_empty() {
        let (mn, mx) = (*residuals.iter().min().unwrap(), *residuals.iter().max().unwrap());
        r.residual_n = residuals.len() as u32;
        r.residual = [mn, median(&mut residuals), mx];
    }
    r
}

struct Lookup {
    /// Microseconds from the most recent start (at or before the event's
    /// entry) of this activity to the event's entry.
    since_start_us: Option<u64>,
    /// Whether the event's delay window `[alarm, entry]` intersects any
    /// span of this activity.
    overlaps: bool,
    /// Among spans the delay window intersects and that have ended, the
    /// signed `entry - end` closest to zero (i.e. how close the ISR's
    /// actual entry was to the moment that activity finished).
    entry_minus_end_us: Option<i64>,
}

fn lookup(kind: Activity, alarm_us: u64, entry_us: u64) -> Lookup {
    critical_section::with(|cs| {
        let m = MARKS.borrow(cs).borrow();
        let k = kind as usize;
        let open = m.open[k].map(|start_us| Span { start_us, end_us: u64::MAX });
        let mut out = Lookup { since_start_us: None, overlaps: false, entry_minus_end_us: None };
        let mut latest_start: Option<u64> = None;
        for sp in m.spans[k].iter().copied().chain(open) {
            if sp.start_us > entry_us {
                continue;
            }
            latest_start = Some(latest_start.map_or(sp.start_us, |t| t.max(sp.start_us)));
            if sp.end_us >= alarm_us {
                out.overlaps = true;
                if sp.end_us != u64::MAX {
                    let d = entry_us as i64 - sp.end_us as i64;
                    if out.entry_minus_end_us.map_or(true, |best| d.abs() < best.abs()) {
                        out.entry_minus_end_us = Some(d);
                    }
                }
            }
        }
        out.since_start_us = latest_start.map(|t| entry_us - t);
        out
    })
}

// ---- Per-window correlation of late (>= 100 us) entries with the agent's
// activities. Computed here, outside the ISR, from the workload's ring and
// the markers above. ----

const PHASE_BIN_US: u64 = 250_000;
const PHASE_BINS: usize = 24;

#[derive(Clone, Copy)]
struct Worst {
    lat_us: u32,
    ts_us: u64,
    interval_us: u32,
    since: [Option<u64>; 3],
    storage_entry_minus_end_us: Option<i64>,
}

struct Correlation {
    total: u32,
    low: u32,
    high: u32,
    /// Delay window intersects a span of [monitor, heartbeat, logs, storage].
    overlap: [u32; 4],
    print_overlap: u32,
    print_self: u32,
    storage_near_end: u32,
    no_last_start: [u32; 3],
    /// [monitor, heartbeat, logs] x [100us-1ms, >=1ms] x 250 ms bins of
    /// "microseconds since that activity's last start".
    phase: [[[u16; PHASE_BINS]; 2]; 3],
    worst: [Option<Worst>; 3],
    /// Every drained event, for the flash-span comparison.
    late: alloc::vec::Vec<LateRec>,
}

/// Drains the workload's late-event ring (lock-free: only advances
/// `LATE_TAIL`, which only this function writes) and correlates every event.
/// `ts_ticks` is stamped a few microseconds after ISR entry (after the
/// ack/rearm); that difference is treated as zero -- negligible against the
/// millisecond-scale deltas this looks at, but not against the +-10 us edge
/// of the 100 us class, so read that class's boundaries with that in mind.
fn drain_and_correlate(ticks_per_us: u32) -> Correlation {
    let head = read_ram_u32(LATE_HEAD_OFFSET);
    core::sync::atomic::compiler_fence(Ordering::Acquire);
    let mut tail = read_ram_u32(LATE_TAIL_OFFSET);
    let mut c = Correlation {
        total: 0,
        low: 0,
        high: 0,
        overlap: [0; 4],
        print_overlap: 0,
        print_self: 0,
        storage_near_end: 0,
        no_last_start: [0; 3],
        phase: [[[0; PHASE_BINS]; 2]; 3],
        worst: [None; 3],
        late: alloc::vec::Vec::new(),
    };
    let sys_per_us = SYSTIMER_HZ / 1_000_000;
    const TASKS: [Activity; 3] = [Activity::Monitor, Activity::Heartbeat, Activity::Logs];
    while tail != head {
        let addr = WORKLOAD_RAM_BASE + LATE_RING_OFFSET + (tail % LATE_RING_LEN) * 24;
        // SAFETY: a fixed slot of the workload's ring, written entirely by
        // the ISR before `LATE_HEAD` advanced past it (checked above).
        let ev = unsafe { core::ptr::read_volatile(addr as *const LateEvent) };
        tail = tail.wrapping_add(1);

        let lat_us = ev.entry_ticks / ticks_per_us;
        let entry_us = ev.ts_ticks / sys_per_us;
        let alarm_us = entry_us.saturating_sub(lat_us as u64);
        let class = usize::from(lat_us >= 1000);

        c.late.push((alarm_us, entry_us, lat_us));
        c.total += 1;
        if class == 0 { c.low += 1 } else { c.high += 1 }

        let mut since = [None; 3];
        for (i, kind) in TASKS.iter().enumerate() {
            let l = lookup(*kind, alarm_us, entry_us);
            if l.overlaps {
                c.overlap[i] += 1;
            }
            match l.since_start_us {
                Some(d) => {
                    let bin = ((d / PHASE_BIN_US) as usize).min(PHASE_BINS - 1);
                    c.phase[i][class][bin] = c.phase[i][class][bin].saturating_add(1);
                    since[i] = Some(d);
                }
                None => c.no_last_start[i] += 1,
            }
        }
        let sto = lookup(Activity::Storage, alarm_us, entry_us);
        if sto.overlaps {
            c.overlap[3] += 1;
        }
        if sto.entry_minus_end_us.is_some_and(|d| d.abs() <= 300) {
            c.storage_near_end += 1;
        }
        if let Some(p) = find_covering_print(alarm_us, entry_us) {
            c.print_overlap += 1;
            if tag_str(&p).starts_with("workload POC") {
                c.print_self += 1;
            }
        }

        let cand = Worst {
            lat_us,
            ts_us: entry_us,
            interval_us: ev.interval_ticks / sys_per_us as u32,
            since,
            storage_entry_minus_end_us: sto.entry_minus_end_us,
        };
        // Keep the three largest entry latencies, descending.
        let mut slot = None;
        for (i, w) in c.worst.iter().enumerate() {
            if w.map_or(true, |w| cand.lat_us > w.lat_us) {
                slot = Some(i);
                break;
            }
        }
        if let Some(i) = slot {
            for j in (i + 1..3).rev() {
                c.worst[j] = c.worst[j - 1];
            }
            c.worst[i] = Some(cand);
        }
    }
    write_ram_u32(LATE_TAIL_OFFSET, tail);
    c
}

fn fmt_phase(out: &mut heapless::String<200>, bins: &[u16; PHASE_BINS]) {
    for (i, &n) in bins.iter().enumerate() {
        if n > 0 {
            let ms = i as u32 * 250;
            let _ = core::fmt::write(out, format_args!(" {}.{:02}:{n}", ms / 1000, (ms % 1000) / 10));
        }
    }
}

/// Endurance/characterization visibility, per the POC 2b design discussion:
/// requested vs. observed frequency, missed interrupts, interval (jitter)
/// and ISR-duration min/max, `task_hwm_min`, the ISR entry-latency
/// histogram, and (this pass) how every entry latency >= 100 us lines up in
/// time with the agent's periodic activities -- all read directly from
/// agent-owned RAM/registers. Also doubles as the "agent still alive"
/// signal: this task runs on the same executor as Wi-Fi/HTTP/heartbeat.
///
/// Every line starts with `workload POC` on purpose: `log_stream`'s print
/// ring tags each serial print by its first 12 bytes, and the correlation
/// counts prints with that prefix as this monitor's own output.
#[embassy_executor::task]
pub async fn monitor() -> ! {
    if QUIET_PRINT {
        measure_loop().await;
    }
    let requested_hz = 1_000_000u64 / TIMER_PERIOD.as_micros();
    let mut last_count = irq_count();
    let mut last_instant = esp_hal::time::Instant::now();
    let mut routing_logged = false;
    let mut last_late_dropped = 0u32;
    let mut last_flash_dropped = 0u32;
    let mut flash_spans: alloc::vec::Vec<FlashSpan> = alloc::vec::Vec::new();
    mark_point(Activity::Monitor);
    loop {
        Timer::after(embassy_time::Duration::from_secs(5)).await;
        let activity = ActivityGuard::begin(Activity::Monitor);

        let count = irq_count();
        let now = esp_hal::time::Instant::now();
        let elapsed_ms = (now - last_instant).as_millis().max(1);
        let delta = count.wrapping_sub(last_count);
        let observed_hz = (delta as u64 * 1000) / elapsed_ms;
        let expected_delta = (requested_hz * elapsed_ms) / 1000;
        let missed = expected_delta.saturating_sub(delta as u64);

        let stats = take_interval_window();
        let (duration_min_us, duration_max_us) = duration_range_us();
        let tpu = TG1_TICKS_PER_US.load(Ordering::Relaxed);
        // Spans first: an event's flash access is recorded (post-return)
        // no later than the event itself is drained below.
        let new_spans = take_flash_spans(&mut flash_spans);
        let corr = if tpu != 0 { Some(drain_and_correlate(tpu)) } else { None };
        let late_dropped_total = read_ram_u32(LATE_DROPPED_OFFSET);
        let now_us_v = now.duration_since_epoch().as_micros();
        let print_snapshot = snapshot_log_prints();
        let log_prints_dropped = prune_log_prints(now_us_v);
        prune_marks(now_us_v);
        let flash_report = corr.as_ref().map(|c| flash_analyze(&c.late, &flash_spans, new_spans, {
            let d = FLASH_RING.dropped.load(Ordering::Relaxed);
            let w = d.wrapping_sub(last_flash_dropped);
            last_flash_dropped = d;
            w
        }));
        let b_report = corr.as_ref().map(|c| b_analyze(&c.late, &flash_spans, &print_snapshot));
        flash_spans.retain(|sp| sp.end_us + 12_000_000 >= now_us_v);

        if !QUIET_PRINT {
        // Existing line, unchanged in content/format so this run stays
        // directly comparable with the earlier logs.
        let mut histogram = heapless::String::<160>::new();
        for (label, n) in BUCKET_LABELS.iter().zip(stats.interval_buckets) {
            let _ = core::fmt::write(&mut histogram, format_args!(" {label}={n}"));
        }
        info!(
            "workload POC 2b: irq_count = {count} (+{delta} in {elapsed_ms} ms, \
             ~{observed_hz} Hz observed vs {requested_hz} Hz requested, \
             ~{missed} possibly missed), this-window interval[min={}us \
             max={}us]{histogram}, isr_duration[min={duration_min_us}us \
             max={duration_max_us}us] (cumulative since boot), task_hwm_min = {}",
            stats.interval_min_us,
            stats.interval_max_us,
            crate::stack_usage::free_bytes()
        );

        if tpu != 0 {
            let mut entry_hist = heapless::String::<160>::new();
            for (label, n) in ENTRY_LAT_LABELS.iter().zip(stats.entry_buckets) {
                let _ = core::fmt::write(&mut entry_hist, format_args!(" {label}={n}"));
            }
            info!(
                "workload POC 2b: entry_latency max={}us{entry_hist}",
                stats.entry_max_ticks / tpu
            );
        }


        if let Some(c) = corr.as_ref() {
            info!(
                "workload POC 2b: late>=100us total={} (100us-1ms={}, >=1ms={}) ring_dropped={} \
                 | delay-window overlaps span of: monitor={} heartbeat={} logs={} storage={} \
                 serial_print={} (of which own={}) | storage_entry_within_300us_of_span_end={} \
                 | no_earlier_start: monitor={} heartbeat={} logs={} | print_ring_dropped={log_prints_dropped}",
                c.total,
                c.low,
                c.high,
                late_dropped_total.wrapping_sub(last_late_dropped),
                c.overlap[0],
                c.overlap[1],
                c.overlap[2],
                c.overlap[3],
                c.print_overlap,
                c.print_self,
                c.storage_near_end,
                c.no_last_start[0],
                c.no_last_start[1],
                c.no_last_start[2],
            );
            for (i, name) in ["monitor", "heartbeat", "logs"].iter().enumerate() {
                let mut lo = heapless::String::<200>::new();
                let mut hi = heapless::String::<200>::new();
                fmt_phase(&mut lo, &c.phase[i][0]);
                fmt_phase(&mut hi, &c.phase[i][1]);
                info!(
                    "workload POC 2b: phase since last {name} start (s:count, 250 ms bins) \
                     100us-1ms[{lo} ] >=1ms[{hi} ]"
                );
            }
            for w in c.worst.iter().flatten() {
                let mut since = heapless::String::<72>::new();
                for (name, d) in [("monitor", w.since[0]), ("heartbeat", w.since[1]), ("logs", w.since[2])] {
                    let _ = match d {
                        Some(v) => core::fmt::write(&mut since, format_args!(" {name}={v}")),
                        None => core::fmt::write(&mut since, format_args!(" {name}=n/a")),
                    };
                }
                let mut sto = heapless::String::<32>::new();
                let _ = match w.storage_entry_minus_end_us {
                    Some(v) => core::fmt::write(&mut sto, format_args!("{v:+}us")),
                    None => core::fmt::write(&mut sto, format_args!("n/a")),
                };
                info!(
                    "workload POC 2b: worst lat={}us t=+{}.{:06} interval={}us \
                     us_since_last_start[{since} ] storage_entry_minus_end={sto}",
                    w.lat_us,
                    w.ts_us / 1_000_000,
                    w.ts_us % 1_000_000,
                    w.interval_us,
                );
            }
        }

        if let Some(f) = flash_report.as_ref() {
            info!(
                "workload POC 2b: flash accesses this window: reads={} writes={} erases={} \
                 ring_dropped={} | read_us[min={} med={} max={}] read_total={}us | read_len \
                 <=64B={} <=512B={} <4096B={} =4096B={} >4096B={}",
                f.reads,
                f.writes,
                f.erases,
                f.dropped,
                f.read_dur[0],
                f.read_dur[1],
                f.read_dur[2],
                f.read_total_us,
                f.read_len[0],
                f.read_len[1],
                f.read_len[2],
                f.read_len[3],
                f.read_len[4],
            );
            info!(
                "workload POC 2b: due-vs-flash: events>=100us={} due_inside_read={} \
                 due_inside_write={} due_inside_erase={} due_outside={} | flash_spans_containing_a_due={} \
                 | entry_inside_a_flash_span={} | residual(entry-flash_end)_us[min={} med={} max={}] over {} events",
                f.events,
                f.due_in[0],
                f.due_in[1],
                f.due_in[2],
                f.due_out,
                f.spans_hit,
                f.entry_in_span,
                f.residual[0],
                f.residual[1],
                f.residual[2],
                f.residual_n,
            );
            info!(
                "workload POC 2b: due-vs-flash by class (n A/B/C; A=due inside+residual<={}us, \
                 B=due outside, C=due inside+residual larger): {}[{} {}/{}/{}] {}[{} {}/{}/{}] \
                 {}[{} {}/{}/{}] {}[{} {}/{}/{}]",
                FLASH_A_MAX_RESIDUAL_US,
                FLASH_CLASS_LABELS[0], f.class[0][0], f.class[0][1], f.class[0][2], f.class[0][3],
                FLASH_CLASS_LABELS[1], f.class[1][0], f.class[1][1], f.class[1][2], f.class[1][3],
                FLASH_CLASS_LABELS[2], f.class[2][0], f.class[2][1], f.class[2][2], f.class[2][3],
                FLASH_CLASS_LABELS[3], f.class[3][0], f.class[3][1], f.class[3][2], f.class[3][3],
            );
        }

        if let Some(b) = b_report.as_ref() {
            info!(
                "workload POC 2b: B-vs-serial_print: B_total={} due_inside_print={} due_outside_print={} \
                 | inside print of: monitor={} heartbeat={} logs={} other={} | \
                 residual_print(entry-print_end)_us[min={} med={} max={}] over {} events",
                b.total,
                b.inside_print,
                b.outside_print,
                b.kind[0],
                b.kind[1],
                b.kind[2],
                b.kind[3],
                b.residual[0],
                b.residual[1],
                b.residual[2],
                b.residual_n,
            );
            info!(
                "workload POC 2b: B-vs-serial_print by class (n P1/P2/P3/P4; P1=inside+|res|<={}us, \
                 P2=outside print, P3=inside+entry well before end, P4=inside+entry long after end): \
                 {}[{} {}/{}/{}/{}] {}[{} {}/{}/{}/{}] {}[{} {}/{}/{}/{}] {}[{} {}/{}/{}/{}]",
                PRINT_P1_MAX_RESIDUAL_US,
                FLASH_CLASS_LABELS[0], b.class[0][0], b.class[0][1], b.class[0][2], b.class[0][3], b.class[0][4],
                FLASH_CLASS_LABELS[1], b.class[1][0], b.class[1][1], b.class[1][2], b.class[1][3], b.class[1][4],
                FLASH_CLASS_LABELS[2], b.class[2][0], b.class[2][1], b.class[2][2], b.class[2][3], b.class[2][4],
                FLASH_CLASS_LABELS[3], b.class[3][0], b.class[3][1], b.class[3][2], b.class[3][3], b.class[3][4],
            );
            for w in b.worst.iter().flatten() {
                let mut pr = heapless::String::<96>::new();
                let _ = match w.print {
                    Some((st, en, k)) => core::fmt::write(
                        &mut pr,
                        format_args!(
                            "{}[+{}.{:06}..+{}.{:06}, {}us] residual_print={:+}us",
                            PRINT_KINDS[k],
                            st / 1_000_000,
                            st % 1_000_000,
                            en / 1_000_000,
                            en % 1_000_000,
                            en - st,
                            w.residual_us.unwrap_or(0)
                        ),
                    ),
                    None => core::fmt::write(&mut pr, format_args!("n/a")),
                };
                let mut ph = heapless::String::<24>::new();
                let _ = match w.since_monitor_start_us {
                    Some(v) => core::fmt::write(&mut ph, format_args!("{}us", v)),
                    None => core::fmt::write(&mut ph, format_args!("n/a")),
                };
                info!(
                    "workload POC 2b: worst B lat={}us t_due=+{}.{:06} t_entry=+{}.{:06} print={pr} \
                     since_last_monitor_start={ph}",
                    w.lat_us,
                    w.due_us / 1_000_000,
                    w.due_us % 1_000_000,
                    w.entry_us / 1_000_000,
                    w.entry_us % 1_000_000,
                );
            }
        }

        }
        if !routing_logged {
            log_interrupt_routing();
            routing_logged = true;
        }

        last_late_dropped = late_dropped_total;
        last_count = count;
        last_instant = now;
        drop(activity);
    }
}

// ---- POC 2d measured runs (10 kHz) -------------------------------------
//
// `monitor()` becomes this loop with `exp-quiet-print`. Sequence, forever:
// warm-up until `MEASURE_WARMUP_S` of uptime (Wi-Fi/SNTP/boot prints are
// long over) -> a measured run of `MEASURE_WINDOWS` x 5 s during which
// *nothing* is printed locally ([`MEASURING`], enforced in `log_stream`'s
// logger for every target) and the per-window statistics are only
// accumulated in RAM -> one report right after -> `MEASURE_GAP_TICKS` x 5 s
// of ordinary operation (prints allowed) -> the next run. The workload, the
// agent tasks (Wi-Fi, heartbeat, log streaming, HTTP) and the NVS cache
// behave exactly as during the 1 kHz combined run; only the timer period
// and the measurement buckets changed.

const MEASURE_WARMUP_S: u64 = 30;
const MEASURE_WINDOWS: usize = 12;
const MEASURE_GAP_TICKS: u32 = 4;

#[derive(Clone, Copy)]
struct LivenessSnap {
    hb_attempts: u32,
    hb_ok: u32,
    logs_attempts: u32,
    logs_ok: u32,
    link_samples: u32,
    link_down: u32,
    config_down: u32,
}

fn liveness_snap() -> LivenessSnap {
    LivenessSnap {
        hb_attempts: HB_ATTEMPTS.load(Ordering::Relaxed),
        hb_ok: HB_OK.load(Ordering::Relaxed),
        logs_attempts: LOGS_ATTEMPTS.load(Ordering::Relaxed),
        logs_ok: LOGS_OK.load(Ordering::Relaxed),
        link_samples: LINK_SAMPLES.load(Ordering::Relaxed),
        link_down: LINK_DOWN_SAMPLES.load(Ordering::Relaxed),
        config_down: CONFIG_DOWN_SAMPLES.load(Ordering::Relaxed),
    }
}

struct MeasureAcc {
    t_start_us: u64,
    last_tick_us: u64,
    irq_start: u32,
    windows: usize,
    interval_min_us: u32,
    interval_max_us: u32,
    interval_buckets: [u64; 6],
    entry_buckets: [u64; 8],
    entry_max_ticks: u32,
    dur_min_ticks: u32,
    dur_max_ticks: u32,
    dur_sum_ticks: u64,
    /// Late events drained from the ring (>= 100 us = >= one period).
    ring_events: u32,
    ring_dropped: u32,
    periods_late: u64,
    a_flash: u32,
    b_total: u32,
    b_in_print: u32,
    b_none: u32,
    flash_reads: u32,
    flash_read_us: u64,
    prints_in_run: u32,
    suppressed: u32,
    hwm_min: u32,
    win_entry_max_us: [u32; MEASURE_WINDOWS],
    win_overruns: [u32; MEASURE_WINDOWS],
    live_start: LivenessSnap,
    flash_start: FlashSnap,
}

impl MeasureAcc {
    fn new(t_start_us: u64, irq_start: u32) -> Self {
        Self {
            t_start_us,
            last_tick_us: t_start_us,
            irq_start,
            windows: 0,
            interval_min_us: u32::MAX,
            interval_max_us: 0,
            interval_buckets: [0; 6],
            entry_buckets: [0; 8],
            entry_max_ticks: 0,
            dur_min_ticks: u32::MAX,
            dur_max_ticks: 0,
            dur_sum_ticks: 0,
            ring_events: 0,
            ring_dropped: 0,
            periods_late: 0,
            a_flash: 0,
            b_total: 0,
            b_in_print: 0,
            b_none: 0,
            flash_reads: 0,
            flash_read_us: 0,
            prints_in_run: 0,
            suppressed: 0,
            hwm_min: u32::MAX,
            win_entry_max_us: [0; MEASURE_WINDOWS],
            win_overruns: [0; MEASURE_WINDOWS],
            live_start: liveness_snap(),
            flash_start: {
                // New epoch: per-run maxima restart on the producer's next op.
                FLASH_STATS.epoch.store(FLASH_STATS.epoch.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
                FLASH_STATS.snapshot()
            },
        }
    }
}

fn fmt_list<T: core::fmt::Display>(out: &mut heapless::String<200>, items: &[T]) {
    for (i, v) in items.iter().enumerate() {
        let _ = core::fmt::write(out, format_args!("{}{v}", if i == 0 { "" } else { "," }));
    }
}

fn print_measure_report(run_no: u32, acc: &MeasureAcc, count_end: u32, now_us_v: u64, tpu: u32, requested_hz: u64) {
    let elapsed_us = now_us_v.saturating_sub(acc.t_start_us).max(1);
    let irq = count_end.wrapping_sub(acc.irq_start) as u64;
    let f_eff_x10 = irq * 10_000_000 / elapsed_us;
    let expected = requested_hz * elapsed_us / 1_000_000;
    let n_entries: u64 = acc.entry_buckets.iter().sum();
    let overruns: u64 = acc.entry_buckets[5..].iter().sum();
    let dur_us = |t: u64| t * 1_000_000 / SYSTIMER_HZ;
    let mean_dur_x100 = if irq > 0 { acc.dur_sum_ticks * 100_000_000 / (SYSTIMER_HZ * irq) } else { 0 };
    let u_isr_x10000 = acc.dur_sum_ticks * 1_000_000 / SYSTIMER_HZ * 10_000 / elapsed_us; // % x 100
    let live = liveness_snap();
    let d = |now: u32, before: u32| now.wrapping_sub(before);

    info!(
        "workload POC 2d [run {run_no}] 10 kHz: t=[+{}s..+{}s] windows={} elapsed={}ms irq={irq} \
         f_eff={}.{}Hz (requested {requested_hz}, expected {expected}, shortfall {}) resets=none(same boot)",
        acc.t_start_us / 1_000_000,
        now_us_v / 1_000_000,
        acc.windows,
        elapsed_us / 1000,
        f_eff_x10 / 10,
        f_eff_x10 % 10,
        expected.saturating_sub(irq),
    );

    let mut ivl = heapless::String::<160>::new();
    for (label, n) in BUCKET_LABELS.iter().zip(acc.interval_buckets) {
        let _ = core::fmt::write(&mut ivl, format_args!(" {label}={n}"));
    }
    info!(
        "workload POC 2d [run {run_no}] interval: min={}us max={}us |{ivl}",
        if acc.interval_min_us == u32::MAX { 0 } else { acc.interval_min_us },
        acc.interval_max_us
    );

    let mut ent = heapless::String::<200>::new();
    for (label, n) in ENTRY_LAT_LABELS.iter().zip(acc.entry_buckets) {
        let _ = core::fmt::write(&mut ent, format_args!(" {label}={n}"));
    }
    let mut wmax = heapless::String::<200>::new();
    let wmax_us: [u32; MEASURE_WINDOWS] = acc.win_entry_max_us;
    fmt_list(&mut wmax, &wmax_us);
    info!(
        "workload POC 2d [run {run_no}] entry_lateness (us, n={n_entries}):{ent} | max={}us | max/window=[{wmax}]",
        acc.entry_max_ticks / tpu.max(1)
    );

    let mut wov = heapless::String::<200>::new();
    fmt_list(&mut wov, &acc.win_overruns);
    info!(
        "workload POC 2d [run {run_no}] deadline: deadline_overruns(>=100us)={overruns} \
         (per window [{wov}]) periods_late={} (from {} ring events, ring_dropped={}; lower bound if dropped>0; \
         not a count of lost IRQs) | overruns attributed: due_in_nvs_flash_read={} in_print_span={} none={}",
        acc.periods_late, acc.ring_events, acc.ring_dropped, acc.a_flash, acc.b_in_print, acc.b_none
    );

    info!(
        "workload POC 2d [run {run_no}] cpu: isr_duration min={}us mean={}.{:02}us max={}us \
         (t1-t0 only: excludes dispatch, entry read, ack, histogram -> lower bound) | \
         U_isr>={}.{:02}% | task_hwm_min={} | nvs_flash_reads={} ({}us total)",
        dur_us(if acc.dur_min_ticks == u32::MAX { 0 } else { acc.dur_min_ticks } as u64),
        mean_dur_x100 / 100,
        mean_dur_x100 % 100,
        dur_us(acc.dur_max_ticks as u64),
        u_isr_x10000 / 100,
        u_isr_x10000 % 100,
        acc.hwm_min,
        acc.flash_reads,
        acc.flash_read_us,
    );

    let fs = FLASH_STATS.snapshot();
    let mut lens = heapless::String::<200>::new();
    let mut means = heapless::String::<200>::new();
    for i in 0..6 {
        let n = fs.len[i].wrapping_sub(acc.flash_start.len[i]);
        let us = fs.len_us[i].wrapping_sub(acc.flash_start.len_us[i]);
        let _ = core::fmt::write(&mut lens, format_args!(" {}={n}", FLASH_LEN_LABELS[i]));
        let _ = core::fmt::write(&mut means, format_args!(" {}={}", FLASH_LEN_LABELS[i], if n > 0 { us / n } else { 0 }));
    }
    let mut durs = heapless::String::<200>::new();
    for i in 0..6 {
        let _ = core::fmt::write(
            &mut durs,
            format_args!(" {}={}", FLASH_DUR_LABELS[i], fs.dur[i].wrapping_sub(acc.flash_start.dur[i])),
        );
    }
    info!(
        "workload POC 2d [run {run_no}] nvs flash io (aggregate counters, cannot overflow): reads={} total={}us \
         max_read={}us | by length:{lens} | mean us by length:{means} | by duration:{durs} | writes={} (max {}us) \
         erases={} (max {}us) | span_ring_dropped={}",
        fs.reads.wrapping_sub(acc.flash_start.reads),
        fs.read_us.wrapping_sub(acc.flash_start.read_us),
        FLASH_STATS.read_max_us.load(Ordering::Relaxed),
        fs.writes.wrapping_sub(acc.flash_start.writes),
        FLASH_STATS.write_max_us.load(Ordering::Relaxed),
        fs.erases.wrapping_sub(acc.flash_start.erases),
        FLASH_STATS.erase_max_us.load(Ordering::Relaxed),
        fs.ring_dropped.wrapping_sub(acc.flash_start.ring_dropped),
    );

    info!(
        "workload POC 2d [run {run_no}] control-plane: heartbeat attempts={} ok={} | log-stream attempts={} ok={} | \
         net link_down_samples={}/{} config_down_samples={} | local prints during run={} (log lines suppressed={}) | \
         watchdog/reset=none",
        d(live.hb_attempts, acc.live_start.hb_attempts),
        d(live.hb_ok, acc.live_start.hb_ok),
        d(live.logs_attempts, acc.live_start.logs_attempts),
        d(live.logs_ok, acc.live_start.logs_ok),
        d(live.link_down, acc.live_start.link_down),
        d(live.link_samples, acc.live_start.link_samples),
        d(live.config_down, acc.live_start.config_down),
        acc.prints_in_run,
        acc.suppressed,
    );
}

async fn measure_loop() -> ! {
    let requested_hz = 1_000_000u64 / TIMER_PERIOD.as_micros();
    let mut flash_spans: alloc::vec::Vec<FlashSpan> = alloc::vec::Vec::new();
    let mut last_late_dropped = 0u32;
    let mut routing_logged = false;
    let mut run_no = 0u32;
    let mut acc: Option<MeasureAcc> = None;
    let mut gap_left = 0u32;
    mark_point(Activity::Monitor);
    info!(
        "workload POC 2d: 10 kHz measurement mode -- first measured run after {MEASURE_WARMUP_S} s of uptime, \
         {MEASURE_WINDOWS} x 5 s each with zero local prints, report printed right after"
    );
    loop {
        Timer::after(embassy_time::Duration::from_secs(5)).await;
        let activity = ActivityGuard::begin(Activity::Monitor);
        let now = esp_hal::time::Instant::now();
        let now_us_v = now.duration_since_epoch().as_micros();
        let tpu = TG1_TICKS_PER_US.load(Ordering::Relaxed);

        // Every tick drains everything (so no ring overflows), measured or not.
        let stats = take_interval_window();
        let count = irq_count();
        let new_spans = take_flash_spans(&mut flash_spans);
        let corr = if tpu != 0 { Some(drain_and_correlate(tpu)) } else { None };
        let late_dropped_total = read_ram_u32(LATE_DROPPED_OFFSET);
        let print_snapshot = snapshot_log_prints();
        let _ = prune_log_prints(now_us_v);
        prune_marks(now_us_v);

        if let Some(a) = acc.as_mut() {
            // ---- accumulate one measured window ----
            let w = a.windows;
            for i in 0..6 {
                a.interval_buckets[i] += stats.interval_buckets[i] as u64;
            }
            for i in 0..8 {
                a.entry_buckets[i] += stats.entry_buckets[i] as u64;
            }
            a.interval_min_us = a.interval_min_us.min(stats.interval_min_us);
            a.interval_max_us = a.interval_max_us.max(stats.interval_max_us);
            a.entry_max_ticks = a.entry_max_ticks.max(stats.entry_max_ticks);
            a.dur_min_ticks = a.dur_min_ticks.min(stats.duration_min_ticks);
            a.dur_max_ticks = a.dur_max_ticks.max(stats.duration_max_ticks);
            a.dur_sum_ticks += stats.duration_sum_ticks as u64;
            if w < MEASURE_WINDOWS {
                a.win_entry_max_us[w] = stats.entry_max_ticks / tpu.max(1);
                a.win_overruns[w] = stats.entry_buckets[5..].iter().sum();
            }
            a.ring_dropped += late_dropped_total.wrapping_sub(last_late_dropped);
            if let Some(c) = corr.as_ref() {
                a.ring_events += c.total;
                a.periods_late += c.late.iter().map(|&(_, _, lat_us)| (lat_us / 100) as u64).sum::<u64>();
                let f = flash_analyze(&c.late, &flash_spans, new_spans, 0);
                let b = b_analyze(&c.late, &flash_spans, &print_snapshot);
                a.a_flash += f.due_in[0] + f.due_in[1] + f.due_in[2];
                a.b_total += b.total;
                a.b_in_print += b.inside_print;
                a.b_none += b.outside_print;
                a.flash_reads += f.reads;
                a.flash_read_us += f.read_total_us;
            }
            a.prints_in_run += print_snapshot.iter().filter(|p| p.start_us >= a.last_tick_us).count() as u32;
            a.suppressed += take_suppressed();
            a.hwm_min = a.hwm_min.min(crate::stack_usage::free_bytes());
            a.last_tick_us = now_us_v;
            a.windows += 1;

            if a.windows == MEASURE_WINDOWS {
                // Report *after* re-enabling local prints.
                MEASURING.store(false, Ordering::Relaxed);
                run_no += 1;
                if let Some(done) = acc.take() {
                    print_measure_report(run_no, &done, count, now_us_v, tpu, requested_hz);
                }
                gap_left = MEASURE_GAP_TICKS;
            }
        } else if gap_left > 0 {
            gap_left -= 1;
        } else if now_us_v >= MEASURE_WARMUP_S * 1_000_000 && tpu != 0 {
            // Start a run: this tick's data is discarded, the next 12 are measured.
            let _ = take_suppressed();
            acc = Some(MeasureAcc::new(now_us_v, count));
            MEASURING.store(true, Ordering::Relaxed);
        }

        if !routing_logged && !MEASURING.load(Ordering::Relaxed) {
            log_interrupt_routing();
            routing_logged = true;
        }
        flash_spans.retain(|sp| sp.end_us + 12_000_000 >= now_us_v);
        last_late_dropped = late_dropped_total;
        drop(activity);
    }
}
