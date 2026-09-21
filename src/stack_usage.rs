//! Real stack high-water-mark measurement (contrat §5's `task_hwm_min`),
//! via the standard "stack painting" technique -- fills the unused portion
//! of the stack with a known byte pattern once, early at boot, then later
//! scans how much of that pattern survives to find how deep the stack has
//! ever gone. Answers concretely what `.stack`'s ~189 KiB linker
//! reservation alone can't: that figure is the *maximum* the linker set
//! aside (whatever was left over after `.bss`/`.data`/the heap), not
//! *measured* usage -- guessing from it and shrinking blindly risked a
//! silent stack overflow, which is worse than the tight heap this was
//! meant to fix.
//!
//! Only one region to paint/scan, not one per task: `esp-rtos`'s own docs
//! describe `#[esp_rtos::main]` as creating "a thread-mode executor on the
//! main thread" -- every `#[embassy_executor::task]` in this firmware
//! (cooperative, not preemptive) runs on that one shared call stack, the
//! same `.stack` region `esp-hal`'s linker scripts reserve. (`esp-radio`'s
//! own internal WiFi driver threads, if any, use `esp-rtos`'s separate
//! thread machinery with their own stacks -- outside this measurement, and
//! outside anything this firmware's own code controls.)

const PAINT: u8 = 0xAA;

unsafe extern "C" {
    // Defined by esp-hal's `ld/sections/stack.x`: `_stack_start` is the
    // high address (initial SP, stack grows down from here towards
    // `_stack_end`); `_stack_end` is the low boundary -- crossing it is an
    // overflow. Just addresses (the linker never gives them a real type),
    // so these are read via `&raw const` below, never dereferenced.
    static _stack_start: u8;
    static _stack_end: u8;
}

fn bounds() -> (usize, usize) {
    // `&raw const` only takes the symbol's *address*, never dereferences it
    // -- safe even though `_stack_start`/`_stack_end` have no real storage
    // behind them (the linker never gives them one).
    (&raw const _stack_end as usize, &raw const _stack_start as usize)
}

/// Paints the currently-unused portion of the stack (from the low boundary
/// up to, but not including, the current stack pointer) with a known
/// pattern.
///
/// Call exactly once, as early as possible in `main()` -- the closer to the
/// very first instruction, the more of the stack this captures as "unused"
/// before real usage (peripheral init, task spawning) grows past this
/// point and permanently hides that portion from the measurement.
#[inline(never)]
pub fn paint() {
    let sp: usize;
    // SAFETY: reads the `sp` register into a local, no side effects.
    unsafe {
        core::arch::asm!("mv {}, sp", out(reg) sp);
    }
    let (end, _start) = bounds();
    if sp <= end {
        return; // paranoia: a linker/layout surprise, not a real case
    }
    // SAFETY: `[end, sp)` is unused stack space at this exact instant (`sp`
    // is the live stack pointer we just read, and the stack is known to
    // extend from `end` to `_stack_start`) -- nothing else holds a
    // reference into it.
    unsafe {
        core::slice::from_raw_parts_mut(end as *mut u8, sp - end).fill(PAINT);
    }
}

/// Bytes of stack never touched since [`paint`] was called -- the
/// remaining headroom before an overflow. Contrat §5's `task_hwm_min`
/// (this firmware only ever has the one shared stack to report, see this
/// module's doc comment on why that's `min` across "all tasks" trivially).
pub fn free_bytes() -> u32 {
    let (end, start) = bounds();
    // SAFETY: `addr` is within `[end, start)`, the whole stack region;
    // reading it (not writing) is safe regardless of what's live in the
    // in-use portion -- we just check whether this byte still matches
    // `PAINT`.
    (end..start).take_while(|&addr| unsafe { core::ptr::read_volatile(addr as *const u8) } == PAINT).count() as u32
}
