//! embewi-boot -- second-stage bootloader for the ESP32-C3, in Rust.
//!
//!   ROM -> embewi-boot -> the slot `otadata` selects -> embewi-agent
//!
//! Every *decision* -- which slot, what to write to `otadata`, whether an image
//! is bootable -- is `fibewi-esp::boot`, tested on the host against power cuts.
//! This file only performs them on the real flash and hands over control.
//!
//! What it does today (boot chain step 5):
//! * clears the ROM's "flash boot" watchdog protection (see below);
//! * reads the partition table, then the two `otadata` entries;
//! * asks `plan_boot` what to do. A blank `otadata` is a first boot: slot 0 is
//!   validated, then `Valid(seq=1)` is written by the protocol below, and only
//!   then does it boot. Anything unexpected halts -- it never guesses;
//! * loads the chosen image (RAM segments copied, DROM/IROM mapped through the
//!   flash MMU) and jumps.
//!
//! Not yet: checksum/SHA verification (image checks stop at `Verify::Structure`),
//! the watchdog handover a hung image needs, and an agent that writes the entry
//! format this reads -- until then only the bootloader creates entries.
//!
//! Writing one `otadata` entry (`Write::ops`), each step verified before the next:
//!
//! ```text
//! erase sector      -> read back: erased
//! program the body  -> read back: exactly the body, commit word still erased
//! program the commit word (a separate flash command)
//!                   -> read back: exactly the committed entry, and it decodes
//! ```
//!
//! The commit word therefore means "I checked this exact body", not merely "a
//! second command ran". Any failed step halts.
#![no_std]
#![no_main]

use fibewi_esp::boot as boot_core;

use boot_core::image::{self, MemoryMap, Verify};
use boot_core::{BLANK, Boot, Decoded, ENTRY_SIZE, Halt, Op, Raw, Write, decode, plan_boot};
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
const TYPE_APP: u8 = 0x00;
const TYPE_DATA: u8 = 0x01;
const SUBTYPE_OTA_0: u8 = 0x10;
const SUBTYPE_OTA_1: u8 = 0x11;
const SUBTYPE_OTADATA: u8 = 0x00;
const SECTOR: u32 = 0x1000;
const SLOT_COUNT: u8 = 2;

const MAP: MemoryMap = MemoryMap::ESP32C3;

// --- ROM functions (esp32c3.rom.ld, resolved by esp-rom-sys) ---------------

unsafe extern "C" {
    fn esp_rom_spiflash_read(src_addr: u32, data: *mut u32, len: u32) -> i32;
    fn esp_rom_spiflash_write(dest_addr: u32, data: *const u32, len: u32) -> i32;
    fn esp_rom_spiflash_erase_sector(sector_number: u32) -> i32;
    fn esp_rom_spiflash_unlock() -> i32;
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

    /// The ROM flash driver's state: a *pointer* to `esp_rom_spiflash_legacy_data_t`, whose first member is
    /// `chip { device_id, chip_size, block_size, sector_size, page_size, status_mask }` (all `u32`).
    static rom_spiflash_legacy_data: *mut [u32; 6];

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
    /// A partition the layout requires is missing: 1 = otadata, 2 = ota_0, 3 = ota_1.
    MissingPartition(u32),
    /// `plan_boot` says there is nothing safe to boot: 1 = first boot, slot 0 unbootable; 2 = no usable entry.
    Halt(u32),
    /// An `otadata` write step failed verification: 0 erase, 1 body, 2 commit.
    OtadataWrite { step: u32, at: u32 },
    /// The chosen image failed validation when re-read to be loaded.
    Image,
    Mmu(i32),
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

/// The image validator reads through this.
struct RomFlash;

impl image::Read for RomFlash {
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), ()> {
        flash_read(offset, buf).map_err(|_| ())
    }
}

/// Flash size in bytes, from the high nibble of byte 3 of our own image header (ESP image format:
/// 0 = 1 MiB, 1 = 2 MiB, 2 = 4 MiB, 3 = 8 MiB, 4 = 16 MiB, ...).
fn flash_size_from_header() -> Result<u32, BootError> {
    let mut header = [0u8; 4];
    flash_read(0, &mut header)?;
    Ok(match header[3] >> 4 {
        0 => 1 << 20,
        1 => 2 << 20,
        2 => 4 << 20,
        3 => 8 << 20,
        4 => 16 << 20,
        _ => 4 << 20,
    })
}

/// Tells the ROM flash driver how big the chip is. Its default bound is smaller than this layout
/// (`ota_1` ends past 2 MiB): without this, reading the second slot fails. ESP-IDF's second stage does
/// the same (`bootloader_flash_update_size`: `rom_spiflash_legacy_data->chip.chip_size = size`).
/// Found under QEMU: the first A/B boot would otherwise have failed on the device too.
fn set_flash_size() -> Result<(), BootError> {
    let size = flash_size_from_header()?;
    unsafe { (*rom_spiflash_legacy_data)[1] = size };
    log!("boot: flash size ", size);
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

/// The three partitions the A/B layout needs: (offset, size) of each.
struct Layout {
    otadata: (u32, u32),
    apps: [(u32, u32); SLOT_COUNT as usize],
}

fn read_layout() -> Result<Layout, BootError> {
    let mut table = [0u8; PARTITION_TABLE_LEN];
    if flash_read(PARTITION_TABLE_OFFSET, &mut table).is_err() {
        flash_reinit();
        flash_read(PARTITION_TABLE_OFFSET, &mut table)?;
    }
    if u16::from_le_bytes([table[0], table[1]]) != PARTITION_ENTRY_MAGIC {
        return Err(BootError::NoPartitionTable);
    }
    let (mut otadata, mut ota_0, mut ota_1) = (None, None, None);
    for entry in table.chunks_exact(32) {
        if u16::from_le_bytes([entry[0], entry[1]]) != PARTITION_ENTRY_MAGIC {
            break; // 0xEBEB (MD5 marker) or erased flash: end of the entries
        }
        let found = Some((le32(entry, 4), le32(entry, 8)));
        match (entry[2], entry[3]) {
            (TYPE_DATA, SUBTYPE_OTADATA) => otadata = found,
            (TYPE_APP, SUBTYPE_OTA_0) => ota_0 = found,
            (TYPE_APP, SUBTYPE_OTA_1) => ota_1 = found,
            _ => {}
        }
    }
    let otadata = otadata.ok_or(BootError::MissingPartition(1))?;
    if otadata.1 < 2 * SECTOR {
        return Err(BootError::MissingPartition(1));
    }
    Ok(Layout {
        otadata,
        apps: [ota_0.ok_or(BootError::MissingPartition(2))?, ota_1.ok_or(BootError::MissingPartition(3))?],
    })
}

fn read_otadata(layout: &Layout) -> Result<[Raw; 2], BootError> {
    let mut entries = [BLANK; 2];
    for (i, raw) in entries.iter_mut().enumerate() {
        flash_read(layout.otadata.0 + i as u32 * SECTOR, raw)?;
    }
    Ok(entries)
}

/// Programs `data` (a multiple of 4 bytes, at a 4-byte-aligned address) through the ROM.
fn rom_program(at: u32, data: &[u8]) -> bool {
    let mut words = [0u32; ENTRY_SIZE / 4];
    for (word, chunk) in words.iter_mut().zip(data.chunks_exact(4)) {
        *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    unsafe { esp_rom_spiflash_write(at, words.as_ptr(), data.len() as u32) == 0 }
}

/// Performs one `otadata` entry update with a read-back after every command.
fn execute(write: Write, layout: &Layout) -> Result<(), BootError> {
    let base = layout.otadata.0 + u32::from(write.sector) * SECTOR;
    let fail = |step: u32| BootError::OtadataWrite { step, at: base };
    let [erase, body, commit] = write.ops();
    let mut back = [0u8; ENTRY_SIZE];

    if unsafe { esp_rom_spiflash_unlock() } != 0 {
        return Err(fail(0));
    }

    // 0. erase, and see it erased
    if !matches!(erase, Op::Erase { .. }) || unsafe { esp_rom_spiflash_erase_sector(base / SECTOR) } != 0 {
        return Err(fail(0));
    }
    flash_read(base, &mut back)?;
    if back != BLANK {
        return Err(fail(0));
    }

    // 1. the body, and see exactly the body (commit word still erased)
    let Op::Program { offset, len, data, .. } = body else { return Err(fail(1)) };
    if !rom_program(base + u32::from(offset), &data[..usize::from(len)]) {
        return Err(fail(1));
    }
    flash_read(base, &mut back)?;
    if back != write.entry.body() {
        return Err(fail(1));
    }

    // 2. the commit word, on its own; then the whole entry must read back committed and exact
    let Op::Program { offset, len, data, .. } = commit else { return Err(fail(2)) };
    if !rom_program(base + u32::from(offset), &data[..usize::from(len)]) {
        return Err(fail(2));
    }
    flash_read(base, &mut back)?;
    if back != write.entry.encode() || decode(&back) != Decoded::Ok(write.entry) {
        return Err(fail(2));
    }
    Ok(())
}

fn boot() -> Result<core::convert::Infallible, BootError> {
    set_flash_size()?;
    let layout = read_layout()?;
    log!("boot: otadata at ", layout.otadata.0, " ota_0 ", layout.apps[0].0, " ota_1 ", layout.apps[1].0);
    let entries = read_otadata(&layout)?;

    let mut image_ok = |slot: u8| {
        let (offset, size) = layout.apps[usize::from(slot)];
        match image::validate(&mut RomFlash, offset, size, &MAP, Verify::Structure) {
            Ok(_) => true,
            Err(e) => {
                log!("boot: slot ", u32::from(slot), " image refused, reason ", image_error_code(&e));
                false
            }
        }
    };
    let plan = plan_boot(entries, SLOT_COUNT, &mut image_ok);

    let (slot, seq) = match plan.boot {
        Boot::Slot { slot, seq, .. } => (slot, seq),
        Boot::Halt(Halt::NoImage) => return Err(BootError::Halt(1)),
        Boot::Halt(Halt::NoUsableEntry) => return Err(BootError::Halt(2)),
    };
    log!("boot: plan slot=", u32::from(slot), " seq=", seq);

    for write in plan.writes() {
        log!(
            "boot: otadata write sector=",
            u32::from(write.sector),
            " seq=",
            write.entry.seq,
            " state=",
            write.entry.state
        );
        execute(write, &layout)?;
    }

    load(&layout, slot)
}

/// Loads slot `slot`'s image and jumps to it.
fn load(layout: &Layout, slot: u8) -> Result<core::convert::Infallible, BootError> {
    let (offset, size) = layout.apps[usize::from(slot)];
    let image = image::validate(&mut RomFlash, offset, size, &MAP, Verify::Structure).map_err(|_| BootError::Image)?;
    log!("boot: image ok, segments=", image.count as u32, " entry=", image.entry);

    // Copy the RAM segments (validated: inside the map, clear of this bootloader).
    for seg in image.segments().iter().filter(|s| s.len > 0) {
        let Some(target) = seg.ram_target(&MAP) else { continue };
        let dst = target as *mut u8;
        let mut chunk = [0u8; 256];
        let mut copied = 0u32;
        while copied < seg.len {
            let n = (seg.len - copied).min(chunk.len() as u32) as usize;
            flash_read(seg.data_offset + copied, &mut chunk[..n])?;
            unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), dst.add(copied as usize), n) };
            copied += n as u32;
        }
    }

    // Flash MMU + cache for the DROM/IROM segments.
    unsafe {
        Cache_MMU_Init();
        Cache_Enable_ICache(0);
        let autoload = Cache_Suspend_ICache();
        for seg in image.segments().iter().filter(|s| s.len > 0) {
            let is_drom = MAP.drom.contains(&seg.load);
            if !(is_drom || MAP.irom.contains(&seg.load)) {
                continue;
            }
            let vaddr = seg.load & !(MAP.mmu_page - 1);
            let paddr = seg.data_offset & !(MAP.mmu_page - 1);
            let pages = (seg.len + (seg.load - vaddr)).div_ceil(MAP.mmu_page);
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

    log!("boot: jump ", image.entry);
    // Let the USB-Serial-JTAG FIFO drain before the application reconfigures it.
    unsafe { esp_rom_delay_us(50_000) };
    let entry: extern "C" fn() -> ! = unsafe { core::mem::transmute(image.entry as usize) };
    entry()
}

/// A stable number per refusal reason (no `Debug`: it costs code size).
fn image_error_code(e: &image::ImageError) -> u32 {
    use image::ImageError::*;
    match e {
        Read => 1,
        BadMagic(_) => 2,
        BadChip(_) => 3,
        BadSegmentCount(_) => 4,
        SegmentOutsidePartition(_) => 5,
        BadLoadRange(_) => 6,
        Misaligned(_) => 7,
        OverlapsBootloader(_) => 8,
        BadEntry => 9,
        Truncated => 10,
        BadChecksum => 11,
        HashMissing => 12,
        BadHash => 13,
    }
}

/// One line per failure, with the offending value where there is one.
fn report(e: &BootError) {
    match e {
        BootError::FlashRead(at) => log!("boot: FAILED flash read at ", *at),
        BootError::NoPartitionTable => log!("boot: FAILED no partition table"),
        BootError::MissingPartition(which) => log!("boot: FAILED missing partition ", *which),
        BootError::Halt(why) => log!("boot: HALT, nothing safe to boot, reason ", *why),
        BootError::OtadataWrite { step, at } => log!("boot: FAILED otadata write step ", *step, " at ", *at),
        BootError::Image => log!("boot: FAILED image no longer validates"),
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
    unsafe { (clear_bit(0x6001_F000, 0x48, 0x64, 14), clear_bit(0x6000_8000, 0x90, 0xA8, 12)) }
}

#[esp_hal::main]
fn main() -> ! {
    // First thing: the ROM's watchdogs are already ticking.
    let (tg0_wdt, rtc_wdt) = clear_flashboot_watchdogs();
    esp_hal::init(esp_hal::Config::default());
    log!("\r\nembewi-boot ", env!("CARGO_PKG_VERSION"), " (otadata bootstrap; no rollback yet)");
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
