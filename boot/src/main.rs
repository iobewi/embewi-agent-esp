//! embewi-boot -- second-stage bootloader for the ESP32-C3, in Rust.
//!
//! SPIKE (boot chain step 1). The only question this answers: can a Rust
//! second stage take over from the ROM, load the *current* embewi-agent image
//! and hand control to it on real hardware?
//!
//!   ROM -> embewi-boot -> pick `ota_0` -> parse its image -> copy RAM
//!   segments -> map DROM/IROM through the flash MMU -> enable the cache ->
//!   jump to the entry point.
//!
//! Deliberately NOT here yet: rollback, the `otadata` state machine, image
//! hash/checksum verification, the watchdog handover. `esp_hal::init` also
//! disables the watchdogs; that has to be revisited before rollback can be
//! honest (a hung image must be reset by hardware).
//!
//! The boot sequence follows the documented ESP-IDF second-stage behaviour
//! (Apache-2.0, `components/bootloader_support`): RAM segments are copied,
//! flash-mapped segments get MMU entries, then the app entry point is called.
//! The flash/cache primitives are the ESP32-C3 ROM's own (`esp-rom-sys`).
#![no_std]
#![no_main]

use core::ops::Range;

use esp_println::Printer;

/// What `log!` can print. Text and hex only, on purpose: `core::fmt` (Debug,
/// padding, Unicode tables) costs ~10 KiB, and the whole bootloader has to
/// fit in the 32 KiB in front of the partition table.
trait Loggable {
    fn put(&self);
}

impl Loggable for &str {
    fn put(&self) {
        Printer::write_bytes(self.as_bytes());
    }
}

/// Printed as `0x` + 8 hex digits.
impl Loggable for u32 {
    fn put(&self) {
        let mut out = *b"0x00000000";
        for i in 0..8 {
            let nibble = ((*self >> ((7 - i) * 4)) & 0xF) as u8;
            out[2 + i] = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
        }
        Printer::write_bytes(&out);
    }
}

macro_rules! log {
    ($($part:expr),+ $(,)?) => {{
        $( Loggable::put(&$part); )+
        Printer::write_bytes(b"\r\n");
    }};
}

// --- Flash layout (embewi-ab-v1, see partitions.csv) -----------------------

/// Where the partition table lives (the ESP-IDF default, kept by espflash).
const PARTITION_TABLE_OFFSET: u32 = 0x8000;
const PARTITION_TABLE_LEN: usize = 0xC00;
/// Stored as the bytes `AA 50`, hence 0x50AA once read little-endian.
const PARTITION_ENTRY_MAGIC: u16 = 0x50AA;
const PARTITION_TYPE_APP: u8 = 0x00;
const PARTITION_SUBTYPE_OTA_0: u8 = 0x10;

// --- ESP image format -------------------------------------------------------

const IMAGE_MAGIC: u8 = 0xE9;
const IMAGE_HEADER_LEN: u32 = 24;
const SEGMENT_HEADER_LEN: u32 = 8;
const CHIP_ID_ESP32C3: u16 = 0x0005;
const MAX_SEGMENTS: usize = 16;

// --- ESP32-C3 memory map ----------------------------------------------------

/// Flash-mapped through the cache/MMU (64 KiB pages).
const DROM: Range<u32> = 0x3C00_0000..0x3C80_0000;
const IROM: Range<u32> = 0x4200_0000..0x4280_0000;
/// Internal SRAM, instruction-bus alias.
const IRAM: Range<u32> = 0x4037_C000..0x403E_0000;
/// Internal SRAM, data-bus alias (same bytes as IRAM, 0x700000 lower).
const DRAM: Range<u32> = 0x3FC8_0000..0x3FCE_0000;
const RTC_FAST: Range<u32> = 0x5000_0000..0x5000_2000;
const SRAM_ALIAS_OFFSET: u32 = 0x0070_0000;
const MMU_PAGE_SIZE: u32 = 0x1_0000;

/// Everything from here up to the end of DRAM is ours or the ROM's while we
/// run (see boot.x): an application segment reaching it would overwrite the
/// code doing the copy. Expressed in data-bus alias addresses.
const BOOT_WINDOW: Range<u32> = 0x3FCC_B000..0x3FCE_0000;

// --- ROM functions (esp32c3.rom.ld, resolved by esp-rom-sys) ---------------

unsafe extern "C" {
    fn esp_rom_spiflash_read(src_addr: u32, data: *mut u32, len: u32) -> i32;
    fn esp_rom_spiflash_attach(config: u32, legacy: bool);
    fn esp_rom_spiflash_config_param(
        device_id: u32,
        chip_size: u32,
        block_size: u32,
        sector_size: u32,
        page_size: u32,
        status_mask: u32,
    ) -> u32;
    fn ets_efuse_get_spiconfig() -> u32;
    fn esp_rom_delay_us(us: u32);

    fn Cache_MMU_Init();
    fn Cache_Enable_ICache(autoload: u32);
    fn Cache_Suspend_ICache() -> u32;
    fn Cache_Resume_ICache(autoload: u32);
    fn Cache_Invalidate_ICache_All();
    fn Cache_Ibus_MMU_Set(ext_ram: u32, vaddr: u32, paddr: u32, psize: u32, num: u32, fixed: u32) -> i32;
    fn Cache_Dbus_MMU_Set(ext_ram: u32, vaddr: u32, paddr: u32, psize: u32, num: u32, fixed: u32) -> i32;
}

enum BootError {
    FlashRead(u32),
    NoPartitionTable,
    NoOta0,
    BadMagic(u8),
    BadChip(u16),
    BadSegmentCount(u8),
    BadSegment(usize),
    Mmu(i32),
}

/// A parsed segment: where it lands, how long, and where its bytes are in flash.
#[derive(Clone, Copy)]
struct Segment {
    load: u32,
    len: u32,
    data_offset: u32,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Reads `out.len()` bytes at flash offset `offset`. The ROM reader wants
/// word-aligned offsets, buffers and lengths, so this reads aligned windows
/// and copies the requested bytes out of them.
fn flash_read(offset: u32, out: &mut [u8]) -> Result<(), BootError> {
    let mut done = 0usize;
    while done < out.len() {
        let pos = offset + done as u32;
        let start = pos & !3;
        let skip = (pos - start) as usize;
        // 252 + up to 3 bytes of skew fits the 64-word window.
        let want = (out.len() - done).min(252);
        let words = (skip + want).div_ceil(4);
        let mut window = [0u32; 64];
        let rc = unsafe { esp_rom_spiflash_read(start, window.as_mut_ptr(), (words * 4) as u32) };
        if rc != 0 {
            return Err(BootError::FlashRead(start));
        }
        let bytes = unsafe { core::slice::from_raw_parts(window.as_ptr().cast::<u8>(), words * 4) };
        out[done..done + want].copy_from_slice(&bytes[skip..skip + want]);
        done += want;
    }
    Ok(())
}

/// The ROM already attached the flash to read *us*; if its reader still
/// refuses (state differs from what the ROM boot path left), attach and
/// configure it the way the ESP-IDF second stage does, sized from our own
/// image header (flash size lives in the high nibble of byte 3).
fn flash_reinit() {
    let mut header = [0u8; 4];
    let chip_size = match flash_read(0, &mut header) {
        Ok(()) => match header[3] >> 4 {
            0 => 1 << 20,
            1 => 2 << 20,
            3 => 8 << 20,
            4 => 16 << 20,
            _ => 4 << 20,
        },
        Err(_) => 4 << 20,
    };
    log!("boot: re-attaching flash, chip_size=", chip_size);
    unsafe {
        esp_rom_spiflash_attach(ets_efuse_get_spiconfig(), false);
        esp_rom_spiflash_config_param(0, chip_size, 0x1_0000, 0x1000, 0x100, 0xFFFF);
    }
}

/// Finds `ota_0` in the partition table (offset, size).
fn find_ota_0() -> Result<(u32, u32), BootError> {
    let mut table = [0u8; PARTITION_TABLE_LEN];
    if flash_read(PARTITION_TABLE_OFFSET, &mut table).is_err() {
        flash_reinit();
        flash_read(PARTITION_TABLE_OFFSET, &mut table)?;
    }
    log!("boot: partition table head ", le32(&table, 0), " ", le32(&table, 4));
    for entry in table.chunks_exact(32) {
        if u16::from_le_bytes([entry[0], entry[1]]) != PARTITION_ENTRY_MAGIC {
            // 0xEBEB (MD5 marker) or erased flash: end of the entries.
            break;
        }
        if entry[2] == PARTITION_TYPE_APP && entry[3] == PARTITION_SUBTYPE_OTA_0 {
            return Ok((le32(entry, 4), le32(entry, 8)));
        }
    }
    if u16::from_le_bytes([table[0], table[1]]) != PARTITION_ENTRY_MAGIC {
        return Err(BootError::NoPartitionTable);
    }
    Err(BootError::NoOta0)
}

/// Where a RAM segment really is in data-bus terms, or `None` if `load..+len`
/// isn't a RAM range we're willing to write to.
fn ram_target(load: u32, len: u32) -> Option<u32> {
    let end = load.checked_add(len)?;
    if IRAM.contains(&load) && end <= IRAM.end {
        Some(load - SRAM_ALIAS_OFFSET)
    } else if DRAM.contains(&load) && end <= DRAM.end {
        Some(load)
    } else if RTC_FAST.contains(&load) && end <= RTC_FAST.end {
        Some(load)
    } else {
        None
    }
}

fn overlaps_boot(target: u32, len: u32) -> bool {
    // RTC memory isn't part of the window.
    !RTC_FAST.contains(&target) && target < BOOT_WINDOW.end && target + len > BOOT_WINDOW.start
}

fn boot() -> Result<core::convert::Infallible, BootError> {
    let (part_offset, part_size) = find_ota_0()?;
    log!("boot: ota_0 at ", part_offset, " size ", part_size);

    let mut header = [0u8; IMAGE_HEADER_LEN as usize];
    flash_read(part_offset, &mut header)?;
    if header[0] != IMAGE_MAGIC {
        return Err(BootError::BadMagic(header[0]));
    }
    let chip_id = u16::from_le_bytes([header[12], header[13]]);
    if chip_id != CHIP_ID_ESP32C3 {
        return Err(BootError::BadChip(chip_id));
    }
    let count = header[1];
    if count == 0 || count as usize > MAX_SEGMENTS {
        return Err(BootError::BadSegmentCount(count));
    }
    let entry = le32(&header, 4);
    log!("boot: image ok, segments=", u32::from(count), " entry=", entry);

    // Pass 1: read and validate every segment header before touching memory.
    let part_end = part_offset + part_size;
    let mut segments = [Segment { load: 0, len: 0, data_offset: 0 }; MAX_SEGMENTS];
    let mut cursor = part_offset + IMAGE_HEADER_LEN;
    for (i, seg) in segments.iter_mut().take(count as usize).enumerate() {
        let mut raw = [0u8; SEGMENT_HEADER_LEN as usize];
        flash_read(cursor, &mut raw)?;
        let (load, len) = (le32(&raw, 0), le32(&raw, 4));
        let data_offset = cursor + SEGMENT_HEADER_LEN;
        if len > part_size || data_offset.checked_add(len).is_none_or(|end| end > part_end) {
            return Err(BootError::BadSegment(i));
        }
        let flash_mapped = DROM.contains(&load) || IROM.contains(&load);
        let ok = if len == 0 {
            true
        } else if flash_mapped {
            // The MMU maps whole pages: the offset within a page must agree.
            let range = if DROM.contains(&load) { &DROM } else { &IROM };
            load.checked_add(len).is_some_and(|end| end <= range.end)
                && load % MMU_PAGE_SIZE == data_offset % MMU_PAGE_SIZE
        } else {
            ram_target(load, len).is_some_and(|target| !overlaps_boot(target, len))
        };
        if !ok {
            return Err(BootError::BadSegment(i));
        }
        log!("boot:  seg ", i as u32, " load=", load, " len=", len, " flash=", data_offset);
        *seg = Segment { load, len, data_offset };
        cursor = data_offset + len;
    }

    // Pass 2: copy the RAM segments.
    for seg in segments.iter().take(count as usize) {
        if seg.len == 0 || DROM.contains(&seg.load) || IROM.contains(&seg.load) {
            continue;
        }
        let dst = ram_target(seg.load, seg.len).unwrap_or(seg.load) as *mut u8;
        let mut chunk = [0u8; 256];
        let mut copied = 0u32;
        while copied < seg.len {
            let n = (seg.len - copied).min(chunk.len() as u32) as usize;
            flash_read(seg.data_offset + copied, &mut chunk[..n])?;
            unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), dst.add(copied as usize), n) };
            copied += n as u32;
        }
    }

    // Pass 3: flash MMU + cache for the DROM/IROM segments.
    unsafe {
        Cache_MMU_Init();
        Cache_Enable_ICache(0);
        let autoload = Cache_Suspend_ICache();
        for seg in segments.iter().take(count as usize) {
            let is_drom = DROM.contains(&seg.load);
            if seg.len == 0 || !(is_drom || IROM.contains(&seg.load)) {
                continue;
            }
            let vaddr = seg.load & !(MMU_PAGE_SIZE - 1);
            let paddr = seg.data_offset & !(MMU_PAGE_SIZE - 1);
            let pages = (seg.len + (seg.load - vaddr)).div_ceil(MMU_PAGE_SIZE);
            let rc = if is_drom {
                Cache_Dbus_MMU_Set(0, vaddr, paddr, 64, pages, 0)
            } else {
                Cache_Ibus_MMU_Set(0, vaddr, paddr, 64, pages, 0)
            };
            log!("boot: map ", vaddr, " <- flash ", paddr, " pages=", pages, " rc=", rc as u32);
            if rc != 0 {
                return Err(BootError::Mmu(rc));
            }
        }
        Cache_Invalidate_ICache_All();
        Cache_Resume_ICache(autoload);
    }

    log!("boot: jump ", entry);
    // Let the USB-Serial-JTAG FIFO drain before the application reconfigures it.
    unsafe { esp_rom_delay_us(50_000) };
    let entry: extern "C" fn() -> ! = unsafe { core::mem::transmute(entry as usize) };
    entry()
}

/// One line per failure, with the offending value where there is one.
fn report(e: &BootError) {
    match e {
        BootError::FlashRead(at) => log!("boot: FAILED flash read at ", *at),
        BootError::NoPartitionTable => log!("boot: FAILED no partition table"),
        BootError::NoOta0 => log!("boot: FAILED no ota_0 partition"),
        BootError::BadMagic(m) => log!("boot: FAILED bad image magic ", u32::from(*m)),
        BootError::BadChip(c) => log!("boot: FAILED wrong chip id ", u32::from(*c)),
        BootError::BadSegmentCount(n) => log!("boot: FAILED bad segment count ", u32::from(*n)),
        BootError::BadSegment(i) => log!("boot: FAILED bad segment ", *i as u32),
        BootError::Mmu(rc) => log!("boot: FAILED MMU rc ", *rc as u32),
    }
}

/// Register write-protect unlock key, common to TIMG and RTC_CNTL watchdogs.
const WDT_WKEY: u32 = 0x50D8_3AA1;

/// The ROM boots from flash with both the main watchdog (TIMG0, MWDT0) and the
/// RTC watchdog in "flash boot" mode: hardware keeps a watchdog running until
/// software clears `WDT_FLASHBOOT_MOD_EN`. esp-hal's `Wdt::disable()` only
/// clears `WDT_EN` (it touches the flashboot bit solely when *enabling*), so
/// without this the TG0 watchdog resets the chip shortly after the agent
/// starts -- observed on hardware as `rst:0x7 (TG0WDT_SYS_RST)` in a boot loop.
/// This is what ESP-IDF's `bootloader_config_wdt` does for the same reason.
///
/// Returns the two config registers as they were, for the boot log.
fn clear_flashboot_watchdogs() -> (u32, u32) {
    use core::ptr::{read_volatile, write_volatile};
    // ESP32-C3: TIMG0 0x6001F000 (WDTCONFIG0 +0x48 bit 14, WDTWPROTECT +0x64),
    // RTC_CNTL 0x60008000 (WDTCONFIG0 +0x90 bit 12, WDTWPROTECT +0xA8).
    unsafe fn clear_bit(base: usize, config: usize, protect: usize, bit: u32) -> u32 {
        let cfg = (base + config) as *mut u32;
        let wprotect = (base + protect) as *mut u32;
        let before = unsafe { read_volatile(cfg) };
        unsafe {
            write_volatile(wprotect, WDT_WKEY);
            write_volatile(cfg, before & !(1 << bit));
            write_volatile(wprotect, 0);
        }
        before
    }
    unsafe {
        (
            clear_bit(0x6001_F000, 0x48, 0x64, 14),
            clear_bit(0x6000_8000, 0x90, 0xA8, 12),
        )
    }
}

#[esp_hal::main]
fn main() -> ! {
    // First thing: the ROM's watchdogs are already ticking.
    let (tg0_wdt, rtc_wdt) = clear_flashboot_watchdogs();
    esp_hal::init(esp_hal::Config::default());
    log!("\r\nembewi-boot ", env!("CARGO_PKG_VERSION"), " (spike: no rollback yet)");
    log!("boot: wdt tg0=", tg0_wdt, " rtc=", rtc_wdt, " (flashboot bits cleared)");
    // `boot` only ever returns on failure (success ends in a jump).
    let Err(e) = boot();
    report(&e);
    // Nothing safe left to do; stay put so the message can be read.
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log!("boot: PANIC");
    loop {
        core::hint::spin_loop();
    }
}
