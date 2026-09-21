//! POC 2 (PASS, frozen -- functional feasibility of a hardware IRQ
//! dispatching into `workload.bin`, hardware-confirmed with a visible
//! green/blue WS2812 toggle at 10 Hz) + POC 2b (this version -- timing
//! characterization, *not* a new architectural gate: see the design
//! discussion for why 1 kHz/10 kHz is a load/timing test, not an
//! additional condition for "does the IRQ architecture work"). Builds on
//! POC 1 (a second artefact, compiled and linked *entirely separately*
//! from the agent, mapped executable from its own flash region and called
//! directly -- see project memory for that debugging story).
//!
//! ## POC 2b: why the ISR no longer drives the WS2812
//!
//! POC 2's ISR sent a full WS2812 frame every interrupt -- excellent for a
//! visible "yes, this really runs" proof (it produced a very literal police
//! siren), useless for a timing measurement: one frame costs tens of
//! microseconds of strictly-sequenced signal, a large fraction of a 1 kHz
//! period (1 ms) and a dominant fraction of a 10 kHz one (100 us). POC 2b's
//! `workload_timer_isr` goes back to the minimal shape the design
//! discussion asked for -- ack, rearm, count, toggle a plain GPIO bit, done
//! -- plus two small, deliberately cheap SYSTIMER reads (see below) to
//! characterize inter-arrival jitter and the ISR's own duration.
//! `workload_entry`'s one-off WS2812 sanity sequence is unchanged (it only
//! runs once, its cost doesn't matter for characterizing the *periodic*
//! path).
//!
//! ## Fixed-offset `.bss` layout (POC 2b -- a real bug this fixes)
//!
//! POC 2's first version trusted `IRQ_COUNT` to land at
//! `WORKLOAD_RAM_BASE + 0` because that's where it happened to compile to
//! at the time. Adding `NS_PER_ITER_X1000` later silently moved it to `+4`
//! (`esp-hal`'s own internal GPIO-driver static took `+0` instead) --
//! nothing would have errored, `src/workload.rs`'s `irq_count()` would just
//! have kept reading the wrong word, a wrong-but-plausible-looking number.
//! Every field below is now forced to a fixed offset via `workload.ld`
//! (same technique already used for `workload_entry`/`workload_timer_isr`'s
//! `.text` placement), not left to whatever order the compiler picks.
//!
//! ## Measuring jitter/duration *inside* the thing being measured
//!
//! Both numbers come from SYSTIMER reads taken from within
//! `workload_timer_isr` itself -- the only timing primitive this
//! separately-linked crate has (see POC 1's git history for why the RISC-V
//! `cycle` CSR and `esp-hal`'s own time APIs aren't options). That
//! necessarily means the measurement's own overhead (a SYSTIMER
//! write-then-poll-then-read round trip, twice per interrupt) is baked
//! into the "ISR duration" number -- there is no lower-overhead clock
//! available to measure from outside. Treat these as *representative*, not
//! *zero-overhead-exact*.
//!
//! ## Design, per the POC 2/2b gate (unchanged from POC 2)
//! The agent (never this crate) owns interrupt registration -- see
//! `src/workload.rs`'s doc comment. `workload_timer_isr()` runs in
//! interrupt context, dispatched by `esp-hal`'s normal vectored dispatch;
//! everything it touches is a raw register poke, never `esp-hal`'s
//! higher-level driver API (`Clocks::get()`-cache problem, see POC 1).
//! `.data`/`.bss` are explicitly zeroed by the agent before the first call
//! -- this crate's own startup never runs, nothing else would zero them.
#![no_std]
#![no_main]

use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::Peripherals;

// ---- SYSTIMER (coarse timing, NOP-spin calibration reference, and (new in
// POC 2b) per-interrupt interval/duration measurement) ----
const SYSTIMER_BASE: u32 = 0x6002_3000;
const SYSTIMER_UNIT0_OP: u32 = SYSTIMER_BASE + 0x4;
const SYSTIMER_UNIT0_VALUE_HI: u32 = SYSTIMER_BASE + 0x40;
const SYSTIMER_UNIT0_VALUE_LO: u32 = SYSTIMER_BASE + 0x44;
const SYSTIMER_UPDATE_BIT: u32 = 1 << 30;
const SYSTIMER_VALUE_VALID_BIT: u32 = 1 << 29;
const SYSTIMER_HI_MASK: u32 = 0x000F_FFFF;
/// XTAL/2.5 on every esp32c3 -- 16 MHz on the standard 40 MHz XTAL
/// essentially every C3 board (including this one) ships with. `src/
/// workload.rs` uses this same constant to convert the tick counts read
/// back from [`INTERVAL_MIN_TICKS`] etc. into microseconds for logging.
const SYSTIMER_HZ: u64 = 16_000_000;

fn systimer_ticks() -> u64 {
    unsafe {
        core::ptr::write_volatile(SYSTIMER_UNIT0_OP as *mut u32, SYSTIMER_UPDATE_BIT);
        while core::ptr::read_volatile(SYSTIMER_UNIT0_OP as *const u32) & SYSTIMER_VALUE_VALID_BIT
            == 0
        {}
        let mut lo_prev = core::ptr::read_volatile(SYSTIMER_UNIT0_VALUE_LO as *const u32);
        loop {
            let lo = lo_prev;
            let hi = core::ptr::read_volatile(SYSTIMER_UNIT0_VALUE_HI as *const u32)
                & SYSTIMER_HI_MASK;
            lo_prev = core::ptr::read_volatile(SYSTIMER_UNIT0_VALUE_LO as *const u32);
            if lo == lo_prev {
                return ((hi as u64) << 32) | lo as u64;
            }
        }
    }
}

fn wait_systimer_us(us: u32) {
    let target_ticks = (us as u64 * SYSTIMER_HZ) / 1_000_000;
    let start = systimer_ticks();
    while systimer_ticks() - start < target_ticks {}
}

fn wait_systimer_ms(ms: u32) {
    for _ in 0..ms {
        wait_systimer_us(1000);
    }
}

/// One calibration measurement's iteration count -- see POC 1's git
/// history for the reasoning.
const CALIBRATION_ITERS: u32 = 200_000;

#[inline(never)]
fn spin_nops(iters: u32) {
    for _ in 0..iters {
        // SAFETY: no side effect beyond consuming a fixed, small number of
        // cycles -- `asm!` here only to stop the optimizer proving this
        // empty-looking loop has no observable effect and deleting it.
        unsafe { core::arch::asm!("nop") };
    }
}

/// Nanoseconds per [`spin_nops`] iteration, scaled by 1000. Only used by
/// `workload_entry`'s one-off WS2812 sanity sequence in POC 2b -- the ISR
/// no longer drives WS2812 (see this file's doc comment), so this no
/// longer needs to be shared via `.bss` the way POC 2 did.
fn calibrate_ns_per_iter_x1000() -> u64 {
    let prior = disable_interrupts();
    let start = systimer_ticks();
    spin_nops(CALIBRATION_ITERS);
    let elapsed_ticks = systimer_ticks() - start;
    restore_interrupts(prior);
    let elapsed_ns = (elapsed_ticks * 1_000_000_000) / SYSTIMER_HZ;
    (elapsed_ns * 1000) / CALIBRATION_ITERS as u64
}

fn ns_to_iters(ns_per_iter_x1000: u64, ns: u32) -> u32 {
    (((ns as u64 * 1000) + ns_per_iter_x1000 / 2) / ns_per_iter_x1000) as u32
}

// ---- WS2812 protocol (hardware-confirmed values -- see POC 1's git
// history) -- workload_entry's one-off sanity sequence only in POC 2b. ----
const T0H_NS: u32 = 300;
const T0L_NS: u32 = 950;
const T1H_NS: u32 = 900;
const T1L_NS: u32 = 350;
const RESET_US: u32 = 80;
const BRIGHTNESS: u8 = 40;

// ---- GPIO10 (raw register access) ----
const GPIO_BASE: u32 = 0x6000_4000;
const GPIO_OUT_W1TS: u32 = GPIO_BASE + 0x8;
const GPIO_OUT_W1TC: u32 = GPIO_BASE + 0xc;
const GPIO10_BIT: u32 = 1 << 10;

fn gpio10_high() {
    unsafe { core::ptr::write_volatile(GPIO_OUT_W1TS as *mut u32, GPIO10_BIT) };
}

fn gpio10_low() {
    unsafe { core::ptr::write_volatile(GPIO_OUT_W1TC as *mut u32, GPIO10_BIT) };
}

fn send_bit(ns_per_iter_x1000: u64, one: bool) {
    let high_iters = ns_to_iters(ns_per_iter_x1000, if one { T1H_NS } else { T0H_NS });
    let low_iters = ns_to_iters(ns_per_iter_x1000, if one { T1L_NS } else { T0L_NS });
    gpio10_high();
    spin_nops(high_iters);
    gpio10_low();
    spin_nops(low_iters);
}

fn send_byte(ns_per_iter_x1000: u64, byte: u8) {
    for i in (0..8).rev() {
        send_bit(ns_per_iter_x1000, (byte >> i) & 1 != 0);
    }
}

fn send_colour(ns_per_iter_x1000: u64, r: u8, g: u8, b: u8) {
    let prior = disable_interrupts();
    send_byte(ns_per_iter_x1000, g);
    send_byte(ns_per_iter_x1000, r);
    send_byte(ns_per_iter_x1000, b);
    restore_interrupts(prior);
    wait_systimer_us(RESET_US);
}

#[inline(always)]
fn disable_interrupts() -> usize {
    let prior: usize;
    unsafe { core::arch::asm!("csrrci {0}, mstatus, 0b1000", out(reg) prior) };
    prior
}

#[inline(always)]
fn restore_interrupts(prior: usize) {
    unsafe { core::arch::asm!("csrw mstatus, {0}", in(reg) prior) };
}

// ---- TIMG1 (raw register access -- see src/workload.rs's doc comment for
// why TIMG1, and why T0_ALARM_EN must be rewritten every interrupt even in
// autoreload mode) ----
const TIMG1_BASE: u32 = 0x6002_0000;
const TIMG1_T0CONFIG: u32 = TIMG1_BASE;
/// `T0LO` (+0x4) is only meaningful after a `T0UPDATE` (+0xc, bit 31) latch
/// -- same procedure `esp-hal`'s own `Timer::now` uses (offsets verified
/// against the `esp32c3` PAC, `timg0/t.rs`). With `autoreload` on and
/// `T0LOAD`=0 (what `PeriodicTimer::start` programs), the counter restarts
/// from 0 at every alarm, so reading it at ISR entry gives the time since
/// the alarm that raised this interrupt.
const TIMG1_T0LO: u32 = TIMG1_BASE + 0x4;
const TIMG1_T0UPDATE: u32 = TIMG1_BASE + 0xc;
const TIMG1_T0_UPDATE_BIT: u32 = 1 << 31;
const TIMG1_INT_CLR_TIMERS: u32 = TIMG1_BASE + 0x7c;
const TIMG1_T0_ALARM_EN_BIT: u32 = 1 << 10;
const TIMG1_T0_INT_CLR_BIT: u32 = 1 << 0;

/// Latches and reads TIMG1's timer-0 counter (low word only -- at any
/// plausible tick rate a latency that overflowed 32 bits would be minutes,
/// not the ms this measures). The completion poll is bounded, unlike
/// `esp-hal`'s own: a stuck poll inside an ISR would hang the whole chip,
/// so after 1000 iterations we read anyway rather than spin forever.
#[inline(always)]
fn timg1_counter() -> u32 {
    unsafe {
        core::ptr::write_volatile(TIMG1_T0UPDATE as *mut u32, TIMG1_T0_UPDATE_BIT);
        let mut spins = 0u32;
        while core::ptr::read_volatile(TIMG1_T0UPDATE as *const u32) & TIMG1_T0_UPDATE_BIT != 0
            && spins < 1000
        {
            spins += 1;
        }
        core::ptr::read_volatile(TIMG1_T0LO as *const u32)
    }
}

// ---- Fixed-offset `.bss` fields (see this file's and workload.ld's doc
// comments for why each one is pinned via `link_section` instead of left
// to compiler-chosen order) ----

/// Total interrupts handled. `src/workload.rs`'s `irq_count()` reads this
/// directly.
#[unsafe(link_section = ".bss.irq_count")]
static mut IRQ_COUNT: u32 = 0;
/// Set once by `workload_entry`, read (not written) by `workload_timer_isr`
/// for its one-off WS2812 sanity sequence -- unused by the ISR itself in
/// POC 2b, kept only so `send_colour` has a calibrated value to call with.
#[unsafe(link_section = ".bss.ns_per_iter_x1000")]
static mut NS_PER_ITER_X1000: u64 = 0;
/// SYSTIMER ticks at the previous interrupt's arrival -- `0` means "no
/// previous interrupt yet" (zeroed by the agent, and `0` is not a real
/// SYSTIMER value this soon after boot), used to skip the interval
/// calculation on the very first interrupt.
#[unsafe(link_section = ".bss.last_arrival_ticks")]
static mut LAST_ARRIVAL_TICKS: u64 = 0;
/// Smallest/largest observed gap between consecutive interrupt arrivals,
/// in SYSTIMER ticks (16 MHz, 62.5 ns/tick) -- jitter. `workload_entry`
/// resets [`INTERVAL_MIN_TICKS`] to `u32::MAX` explicitly (the agent's
/// zeroing alone would leave it at `0`, which is already smaller than any
/// real interval and would never update).
#[unsafe(link_section = ".bss.interval_min_ticks")]
static mut INTERVAL_MIN_TICKS: u32 = 0;
#[unsafe(link_section = ".bss.interval_max_ticks")]
static mut INTERVAL_MAX_TICKS: u32 = 0;
/// Smallest/largest observed `workload_timer_isr` duration, in SYSTIMER
/// ticks -- includes the measurement's own SYSTIMER-read overhead, see
/// this file's doc comment. Same `u32::MAX` reset note as above applies to
/// [`DURATION_MIN_TICKS`].
#[unsafe(link_section = ".bss.duration_min_ticks")]
static mut DURATION_MIN_TICKS: u32 = 0;
#[unsafe(link_section = ".bss.duration_max_ticks")]
static mut DURATION_MAX_TICKS: u32 = 0;

// ---- Windowed interval histogram (POC 2b, second pass) -- the agent
// resets INTERVAL_MIN_TICKS/INTERVAL_MAX_TICKS *and* these six bucket
// counts every monitoring cycle (see src/workload.rs's `monitor` doc
// comment), so a one-time startup anomaly can't sit in MAX forever and
// hide a smaller, ongoing problem behind it -- each report reflects only
// the interrupts since the last reset. Bucket boundaries are compared in
// raw SYSTIMER ticks (`us * 16`, `>> 4`'s inverse) rather than converting
// the interval to microseconds first: a multiply-by-16 the agent already
// has to do anyway to define these constants, avoiding a division inside
// the ISR.
//
// POC 2d (10 kHz, 100 us period): boundaries are 50/90/110/150/500 us --
// centred on the period instead of the 1 kHz set's 500/900/1100/1500/5000.
const BUCKET_50US_TICKS: u32 = 50 * 16;
const BUCKET_90US_TICKS: u32 = 90 * 16;
const BUCKET_110US_TICKS: u32 = 110 * 16;
const BUCKET_150US_TICKS: u32 = 150 * 16;
const BUCKET_500US_TICKS: u32 = 500 * 16;

/// Interval histogram, 6 buckets: `<50us`, `50-90`, `90-110`, `110-150`,
/// `150-500`, `>500`.
#[unsafe(link_section = ".bss.bucket_0")]
static mut BUCKET_0: u32 = 0;
#[unsafe(link_section = ".bss.bucket_1")]
static mut BUCKET_1: u32 = 0;
#[unsafe(link_section = ".bss.bucket_2")]
static mut BUCKET_2: u32 = 0;
#[unsafe(link_section = ".bss.bucket_3")]
static mut BUCKET_3: u32 = 0;
#[unsafe(link_section = ".bss.bucket_4")]
static mut BUCKET_4: u32 = 0;
#[unsafe(link_section = ".bss.bucket_5")]
static mut BUCKET_5: u32 = 0;

/// Sum of `workload_timer_isr` durations (the same `duration` the min/max
/// above track: `t1 - t0`) since the agent last reset it -- POC 2d's one
/// addition to the ISR beyond constants: the agent needs a mean / cumulative
/// value for the CPU-budget estimate, min/max alone can't give one.
#[unsafe(link_section = ".bss.duration_sum_ticks")]
static mut DURATION_SUM_TICKS: u32 = 0;

// ---- ISR entry latency (POC 2b, third pass) -- see workload.ld for why
// every field is pinned and why no critical section guards the ring. ----

/// Windowed histogram of entry latency in TIMG1 counter ticks, 8 buckets
/// (POC 2d, 10 kHz): `<10us`, `10-25`, `25-50`, `50-75`, `75-100`,
/// `100-200`, `200-500`, `>500`. Reset by the agent every monitoring cycle.
#[unsafe(link_section = ".bss.entry_lat_buckets")]
static mut ENTRY_LAT_BUCKETS: [u32; 8] = [0; 8];
/// Largest entry latency this window, TIMG1 ticks. Reset by the agent.
#[unsafe(link_section = ".bss.entry_lat_max_ticks")]
static mut ENTRY_LAT_MAX_TICKS: u32 = 0;
/// Ring bookkeeping: `LATE_HEAD` counts events ever pushed (ISR-owned),
/// `LATE_TAIL` events ever consumed (agent-owned); `LATE_DROPPED` counts
/// events lost because the ring was full.
#[unsafe(link_section = ".bss.late_head")]
static mut LATE_HEAD: u32 = 0;
#[unsafe(link_section = ".bss.late_tail")]
static mut LATE_TAIL: u32 = 0;
#[unsafe(link_section = ".bss.late_dropped")]
static mut LATE_DROPPED: u32 = 0;
/// Bucket boundaries in TIMG1 ticks (10, 25, 50, 75, 100, 200, 500
/// microseconds), written by the agent once it has derived TIMG1's tick
/// rate from the alarm register it just programmed -- the workload can't
/// derive it itself (no `Clocks::get()`, see POC 1). `[0] == 0` means "not
/// initialised yet": the ISR then skips latency recording entirely.
#[unsafe(link_section = ".bss.lat_thr_ticks")]
static mut LAT_THR_TICKS: [u32; 7] = [0; 7];

/// Bucket index from which an entry latency is a "late event" worth
/// ring-buffering: 5 = the `100-200 us` bucket with POC 2d's boundaries, i.e.
/// every entry latency >= 100 us = one full 10 kHz period (a deadline
/// overrun). It was 3 (also >= 100 us) with the 1 kHz boundaries
/// (10/50/100/500/...) -- the *meaning* is unchanged, the index moved.
const LATE_EVENT_BUCKET: usize = 5;
/// 64 events: ~21/window steady state is expected, more during boot; the
/// agent drains every ~5 s and `LATE_DROPPED` counts any overflow.
const LATE_RING_LEN: u32 = 64;

/// One ring-buffered late event, 24 bytes (20 of payload + 4 explicit
/// padding, so both sides agree on the `repr(C)` layout). Times are raw
/// ticks of their own clock (`ts_ticks`/`interval_ticks`/`duration_ticks`:
/// SYSTIMER, 16 MHz; `entry_ticks`: TIMG1 counter) -- the agent converts.
/// `ts_ticks` is stamped a few microseconds *after* ISR entry (after the
/// ack/rearm), which the agent treats as the entry time. An earlier
/// version carried a `stamp_delay_ticks` field (TIMG1 counter elapsed
/// between the entry read and the stamp) -- removed: when the entry
/// latency exceeds the timer period the rearm hits an already-elapsed alarm,
/// the counter is reloaded between the two reads, and the field comes out
/// as ~2^32/20 us of garbage, an invalid value that invites a wrong
/// reading. Dropping it also removes one register round trip per ISR.
#[repr(C)]
#[derive(Clone, Copy)]
struct LateEvent {
    ts_ticks: u64,
    entry_ticks: u32,
    interval_ticks: u32,
    duration_ticks: u32,
    _pad: u32,
}

#[unsafe(link_section = ".bss.late_ring")]
static mut LATE_RING: [LateEvent; 64] = [LateEvent {
    ts_ticks: 0,
    entry_ticks: 0,
    interval_ticks: 0,
    duration_ticks: 0,
    _pad: 0,
}; 64];

/// Forced into its own linker section (`workload.ld` places
/// `.text.workload_entry` first) so this function's first instruction
/// lands exactly at the fixed virtual address the agent maps and calls.
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.workload_entry")]
pub extern "C" fn workload_entry() -> i32 {
    // SAFETY: the agent has already run `esp_hal::init()` once, and has
    // already zeroed this workload's `.data`/`.bss` window, before mapping
    // and calling this function. Only used once, here, for the one-time
    // IO_MUX/output-enable configuration -- everything after this
    // (including every future call from `workload_timer_isr`) talks to the
    // GPIO peripheral directly via `gpio10_high`/`gpio10_low`.
    let peripherals = unsafe { Peripherals::steal() };
    let _pin_config = Output::new(peripherals.GPIO10, Level::Low, OutputConfig::default());

    // The agent zeroes `.bss` to `0`, which is a real (smaller-than-any-
    // real-value) number for the `_MIN_TICKS` trackers -- without this,
    // `workload_timer_isr`'s `if interval < INTERVAL_MIN_TICKS` would never
    // be true, since `0` already looks like the smallest possible interval.
    unsafe {
        (&raw mut INTERVAL_MIN_TICKS).write_volatile(u32::MAX);
        (&raw mut DURATION_MIN_TICKS).write_volatile(u32::MAX);
    }

    let ns_per_iter_x1000 = calibrate_ns_per_iter_x1000();
    unsafe { (&raw mut NS_PER_ITER_X1000).write_volatile(ns_per_iter_x1000) };

    // One-off WS2812 sanity sequence -- proves this function actually ran.
    // The steady interrupt-driven toggling that follows is plain GPIO now
    // (POC 2b, see this file's doc comment), not another WS2812 animation.
    send_colour(ns_per_iter_x1000, BRIGHTNESS, 0, 0); // red
    wait_systimer_ms(500);
    send_colour(ns_per_iter_x1000, 0, 0, 0); // off
    wait_systimer_ms(500);

    0
}

/// Forced to a fixed offset past `workload_entry` (`workload.ld`). Bound by
/// the agent as a vectored `esp_hal::interrupt::InterruptHandler` for
/// TIMG1's timer0. POC 2b: deliberately minimal (see this file's doc
/// comment) -- ack, rearm, count, two cheap SYSTIMER reads for
/// jitter/duration, toggle a plain GPIO bit. No WS2812, no branching beyond
/// min/max comparisons, no calls into anything that isn't this file.
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.workload_timer_isr")]
pub extern "C" fn workload_timer_isr() {
    unsafe {
        // FIRST thing, before even the ack: how long ago did the alarm
        // that raised this interrupt fire? (See `TIMG1_T0LO`'s doc comment.)
        // This is the experiment's whole point -- "does the ISR itself
        // start late?" -- so nothing may precede it but the dispatcher
        // that already ran to get here.
        let entry_ticks = timg1_counter();

        // Ack the interrupt, then rearm the alarm -- both required every
        // time, see src/workload.rs's doc comment on TIMG1.
        core::ptr::write_volatile(TIMG1_INT_CLR_TIMERS as *mut u32, TIMG1_T0_INT_CLR_BIT);
        let config = core::ptr::read_volatile(TIMG1_T0CONFIG as *const u32);
        core::ptr::write_volatile(TIMG1_T0CONFIG as *mut u32, config | TIMG1_T0_ALARM_EN_BIT);

        let t0 = systimer_ticks();

        let mut interval = 0u32;
        let last_ptr = &raw mut LAST_ARRIVAL_TICKS;
        let last = last_ptr.read_volatile();
        if last != 0 {
            interval = (t0 - last) as u32;
            let min_ptr = &raw mut INTERVAL_MIN_TICKS;
            if interval < min_ptr.read_volatile() {
                min_ptr.write_volatile(interval);
            }
            let max_ptr = &raw mut INTERVAL_MAX_TICKS;
            if interval > max_ptr.read_volatile() {
                max_ptr.write_volatile(interval);
            }

            let bucket_ptr = if interval < BUCKET_50US_TICKS {
                &raw mut BUCKET_0
            } else if interval < BUCKET_90US_TICKS {
                &raw mut BUCKET_1
            } else if interval < BUCKET_110US_TICKS {
                &raw mut BUCKET_2
            } else if interval < BUCKET_150US_TICKS {
                &raw mut BUCKET_3
            } else if interval < BUCKET_500US_TICKS {
                &raw mut BUCKET_4
            } else {
                &raw mut BUCKET_5
            };
            bucket_ptr.write_volatile(bucket_ptr.read_volatile().wrapping_add(1));
        }
        last_ptr.write_volatile(t0);

        let count_ptr = &raw mut IRQ_COUNT;
        let count = count_ptr.read_volatile().wrapping_add(1);
        count_ptr.write_volatile(count);

        if count & 1 == 0 {
            gpio10_high();
        } else {
            gpio10_low();
        }

        let t1 = systimer_ticks();
        let duration = (t1 - t0) as u32;
        let dmin_ptr = &raw mut DURATION_MIN_TICKS;
        if duration < dmin_ptr.read_volatile() {
            dmin_ptr.write_volatile(duration);
        }
        let dmax_ptr = &raw mut DURATION_MAX_TICKS;
        if duration > dmax_ptr.read_volatile() {
            dmax_ptr.write_volatile(duration);
        }
        let dsum_ptr = &raw mut DURATION_SUM_TICKS;
        dsum_ptr.write_volatile(dsum_ptr.read_volatile().wrapping_add(duration));

        // Entry-latency histogram + late-event ring. Skipped until the
        // agent has written the bucket boundaries (see `LAT_THR_TICKS`).
        // Done last so none of it lands between `t0` and `t1` above:
        // `duration` keeps meaning what it meant in the earlier passes.
        let thr = (&raw const LAT_THR_TICKS).cast::<u32>();
        if thr.read_volatile() != 0 {
            let mut idx = 0usize;
            while idx < 7 && entry_ticks >= thr.add(idx).read_volatile() {
                idx += 1;
            }
            let bucket = (&raw mut ENTRY_LAT_BUCKETS).cast::<u32>().add(idx);
            bucket.write_volatile(bucket.read_volatile().wrapping_add(1));

            let max_ptr = &raw mut ENTRY_LAT_MAX_TICKS;
            if entry_ticks > max_ptr.read_volatile() {
                max_ptr.write_volatile(entry_ticks);
            }

            if idx >= LATE_EVENT_BUCKET {
                let head_ptr = &raw mut LATE_HEAD;
                let head = head_ptr.read_volatile();
                let tail = (&raw const LATE_TAIL).read_volatile();
                if head.wrapping_sub(tail) >= LATE_RING_LEN {
                    let dropped = &raw mut LATE_DROPPED;
                    dropped.write_volatile(dropped.read_volatile().wrapping_add(1));
                } else {
                    let slot = (&raw mut LATE_RING)
                        .cast::<LateEvent>()
                        .add((head % LATE_RING_LEN) as usize);
                    slot.write_volatile(LateEvent {
                        ts_ticks: t0,
                        entry_ticks,
                        interval_ticks: interval,
                        duration_ticks: duration,
                        _pad: 0,
                    });
                    // Entry fully written before the consumer can see it.
                    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
                    head_ptr.write_volatile(head.wrapping_add(1));
                }
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
