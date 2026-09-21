/*
 * embewi-boot memory layout (ESP32-C3).
 *
 * The ROM loads the image found at flash offset 0 -- RAM segments only -- and
 * jumps to its entry point. The stock ESP-IDF/espflash bootloader lives in
 * the top of SRAM (its segments sit at 0x3fcd5830, 0x403cbf10, 0x403ce710),
 * and the ROM keeps its own data above 0x3fcdc710, so this layout stays
 * inside the same window.
 *
 * SRAM is visible through two aliases: instruction bus 0x4037c000.. and data
 * bus 0x3fc80000.., offset 0x700000. Code goes in the IRAM alias, data and
 * the stack in the DRAM alias, and the two regions below are chosen so they
 * do NOT cover the same physical bytes:
 *
 *   IRAM 0x403cb000..0x403d4000  <->  DRAM alias 0x3fccb000..0x3fcd4000
 *   DRAM 0x3fcd4000..0x3fcdc000  (data, bss, stack growing down from the end)
 *
 * The application image is loaded at the bottom of SRAM (0x40380000 /
 * 0x3fc80000 upwards); `main.rs` refuses any segment reaching this window.
 */
MEMORY
{
  IRAM     (RWX) : ORIGIN = 0x403cb000, LENGTH = 0x9000
  DRAM     (RW)  : ORIGIN = 0x3fcd4000, LENGTH = 0x8000
  RTC_FAST (RWX) : ORIGIN = 0x50000000, LENGTH = 0x2000
}

/* esp-hal's section files place things by these logical names; point all of
 * them at RAM (rodata goes to the *data* alias: byte loads from the
 * instruction alias are not safe). */
REGION_ALIAS("ROTEXT", IRAM);
REGION_ALIAS("RODATA", DRAM);
REGION_ALIAS("RWDATA", DRAM);
REGION_ALIAS("RWTEXT", IRAM);
REGION_ALIAS("RTC_FAST_RWTEXT", RTC_FAST);
REGION_ALIAS("RTC_FAST_RWDATA", RTC_FAST);

/* No flash-mapped sections, hence no `.rotext_dummy`/`.rwdata_dummy` tricks
 * (esp32c3.x): with disjoint IRAM/DRAM windows nothing overlaps. */
SECTIONS {
  INCLUDE "rwtext.x"
  INCLUDE "rwdata.x"
}
INCLUDE "rodata.x"
INCLUDE "text.x"
INCLUDE "rtc_fast.x"
INCLUDE "stack.x"
INCLUDE "metadata.x"
INCLUDE "eh_frame.x"

/* esp-riscv-rt's early trap code compares SP against this to detect a stack
 * that points below RAM. */
_dram_data_start = ORIGIN(DRAM);

INCLUDE "hal-defaults.x"
