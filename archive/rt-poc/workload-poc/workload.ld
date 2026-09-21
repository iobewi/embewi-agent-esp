/* POC 1+2 (see src/main.rs's doc comment): this workload is never booted by
 * the chip -- it's mapped read-execute into a fixed slot of the ESP32-C3's
 * IBUS flash-cache window by the agent (src/workload.rs, root crate) and
 * called (`workload_entry`) or interrupt-dispatched into (`workload_timer_isr`)
 * from the agent's own, already-running code. So there is no vector table,
 * no stack setup -- just `.text`/`.rodata` placed at the exact virtual
 * address the agent maps, plus (new in POC 2) a small `.data`/`.bss` RAM
 * window at a second fixed address the agent reserves, zeroes, and reads
 * back directly (see `src/workload.rs`'s doc comment for how that address
 * was chosen and verified safe).
 *
 * 0x42640000 = SOC_MMU_IBUS_VADDR_BASE (0x42000000) + WORKLOAD_MMU_ENTRY
 * (100, chosen in src/workload.rs on the agent side) * MMU_PAGE_SIZE
 * (0x10000, 64 KiB -- the only page size the C3's cache MMU supports).
 * 0x42640300 (workload_entry's ORIGIN + 0x300, generous headroom past
 * `workload_entry`'s own compiled size) is where `workload_timer_isr` is
 * force-placed -- the agent calls/binds both addresses as plain function
 * pointers, it never reads this crate's ELF symbol table (discarded below
 * along with everything else not needed in the final flashed image).
 * 0x3FCBC000 is the fixed RAM window's base -- picked from deep inside the
 * agent's own `.stack` region's *measured*-free headroom, not a genuinely
 * separate reservation. It was 0x3FCB7000 until the agent's own `.bss` grew
 * ~1.6 KiB (the serial-print timing ring, this experiment's instrumentation)
 * and `_stack_end` moved past it -- `ram_window_is_safe()` would have
 * refused to run the workload; every RAM offset below is now relative to
 * `ORIGIN(WORKLOAD_RAM)` so relocating it is a one-line change here plus
 * `WORKLOAD_RAM_BASE` in src/workload.rs.
 * All four numbers must be kept in sync by hand with `src/workload.rs`'s
 * matching constants -- a real (non-POC) version would generate both from
 * shared constants instead of duplicating literals across two independent
 * builds.
 */
MEMORY {
    WORKLOAD_TEXT (RX) : ORIGIN = 0x42640000, LENGTH = 64K
    WORKLOAD_RAM  (RW) : ORIGIN = 0x3FCBC000, LENGTH = 2048
}

ENTRY(workload_entry)

SECTIONS {
    .text : {
        /* Forced first so `workload_entry`'s first instruction lands
         * exactly at ORIGIN -- the agent calls that address directly, it
         * never reads the ELF symbol table. */
        KEEP(*(.text.workload_entry))
        /* Padded up to a fixed offset (not immediately after
         * `workload_entry`, whatever size that happens to compile to) so
         * the agent's own `WORKLOAD_TIMER_ISR_VADDR` constant stays valid
         * across rebuilds of this crate that don't touch `workload_entry`'s
         * size. */
        . = 0x42640300;
        KEEP(*(.text.workload_timer_isr))
        *(.text .text.*)
        . = ALIGN(4);
        *(.rodata .rodata.*)
    } > WORKLOAD_TEXT

    /* New in POC 2: real, actively-used mutable state, unlike POC 1 where
     * this was reserved but empty. `.data` stays empty for now (nothing
     * here has a non-zero initializer) -- a real workload with
     * non-zero-initialized statics would need a flash load address (`AT>`)
     * for this section too, which the agent would then copy from at
     * startup; not implemented since nothing here needs it yet. */
    .data : {
        *(.data .data.*)
    } > WORKLOAD_RAM

    /* POC 2b: each field forced to a fixed offset, the same technique
     * `.text.workload_entry`/`.text.workload_timer_isr` already use above --
     * *not* left to whatever order the compiler happens to pick. POC 2's
     * first version trusted `IRQ_COUNT` to land at `WORKLOAD_RAM_BASE + 0`
     * because that's where it happened to compile to at the time; adding
     * `NS_PER_ITER_X1000` later silently moved it to `+4` (`esp-hal`'s own
     * internal GPIO-driver static took `+0` instead) -- `src/workload.rs`'s
     * `irq_count()` would have kept reading the wrong word with no error,
     * just a wrong (but plausible-looking) number, had POC 2b not caught it
     * before shipping. Every offset here must be kept in sync by hand with
     * `src/workload.rs`'s matching `WORKLOAD_RAM_*_OFFSET` constants. */
    .bss (NOLOAD) : {
        /* `u64` fields need 8-byte alignment on this target -- offsets
         * below leave an explicit 4-byte gap after `irq_count` (a `u32`)
         * rather than packing tightly, matching what the linker would
         * insist on anyway (the first build of this layout, without the
         * gap, failed with "unable to move location counter ... backward"
         * because `ns_per_iter_x1000` silently consumed the padding
         * itself). */
        . = ORIGIN(WORKLOAD_RAM);
        KEEP(*(.bss.irq_count))
        . = ORIGIN(WORKLOAD_RAM) + 0x08;
        KEEP(*(.bss.ns_per_iter_x1000))
        . = ORIGIN(WORKLOAD_RAM) + 0x10;
        KEEP(*(.bss.last_arrival_ticks))
        . = ORIGIN(WORKLOAD_RAM) + 0x18;
        KEEP(*(.bss.interval_min_ticks))
        . = ORIGIN(WORKLOAD_RAM) + 0x1c;
        KEEP(*(.bss.interval_max_ticks))
        . = ORIGIN(WORKLOAD_RAM) + 0x20;
        KEEP(*(.bss.duration_min_ticks))
        . = ORIGIN(WORKLOAD_RAM) + 0x24;
        KEEP(*(.bss.duration_max_ticks))
        /* POC 2b, windowed jitter histogram: `interval_min_ticks`/
         * `interval_max_ticks` above are reset by the agent every
         * monitoring cycle (not left cumulative-since-boot) specifically so
         * a startup-only anomaly can't hide an ongoing, smaller one behind
         * an unchanging extremum -- these six bucket counts are the
         * per-cycle distribution that makes that check possible without
         * trusting min/max alone. */
        . = ORIGIN(WORKLOAD_RAM) + 0x28;
        KEEP(*(.bss.bucket_0))
        . = ORIGIN(WORKLOAD_RAM) + 0x2c;
        KEEP(*(.bss.bucket_1))
        . = ORIGIN(WORKLOAD_RAM) + 0x30;
        KEEP(*(.bss.bucket_2))
        . = ORIGIN(WORKLOAD_RAM) + 0x34;
        KEEP(*(.bss.bucket_3))
        . = ORIGIN(WORKLOAD_RAM) + 0x38;
        KEEP(*(.bss.bucket_4))
        . = ORIGIN(WORKLOAD_RAM) + 0x3c;
        KEEP(*(.bss.bucket_5))
        /* POC 2b, third pass -- ISR *entry* latency (TIMG1 counter read as
         * the very first thing the ISR does = time since the alarm that
         * raised this interrupt), its windowed histogram (8 buckets,
         * boundaries in `LAT_THR_TICKS`, written by the agent once it knows
         * TIMG1's tick rate), per-window max, and a lock-free
         * single-producer (ISR) / single-consumer (agent monitor) ring of
         * events whose entry latency exceeded 1000 us. `LATE_HEAD` is only
         * written by the ISR, `LATE_TAIL` only by the agent -- no
         * critical section anywhere on this path. */
        . = ORIGIN(WORKLOAD_RAM) + 0x40;
        KEEP(*(.bss.entry_lat_buckets))
        . = ORIGIN(WORKLOAD_RAM) + 0x60;
        KEEP(*(.bss.entry_lat_max_ticks))
        . = ORIGIN(WORKLOAD_RAM) + 0x64;
        KEEP(*(.bss.late_head))
        . = ORIGIN(WORKLOAD_RAM) + 0x68;
        KEEP(*(.bss.late_tail))
        . = ORIGIN(WORKLOAD_RAM) + 0x6c;
        KEEP(*(.bss.late_dropped))
        . = ORIGIN(WORKLOAD_RAM) + 0x70;
        KEEP(*(.bss.lat_thr_ticks))
        /* POC 2d: sum of ISR durations, fits exactly between `lat_thr_ticks`
         * (7 x u32 = 0x70..0x8c) and the ring. */
        . = ORIGIN(WORKLOAD_RAM) + 0x8c;
        KEEP(*(.bss.duration_sum_ticks))
        . = ORIGIN(WORKLOAD_RAM) + 0x90;
        KEEP(*(.bss.late_ring))
        *(.bss .bss.*)
    } > WORKLOAD_RAM

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.riscv.attributes)
        *(.debug*)
        *(.symtab)
        *(.strtab)
    }
}
