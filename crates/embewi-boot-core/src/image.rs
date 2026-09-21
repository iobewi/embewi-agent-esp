//! Bootability of an ESP image sitting in a flash partition -- the bootloader's
//! job, distinct from the *transport* integrity the OTA already checks with its
//! SHA-256 while writing.
//!
//! The rule that matters: nothing here touches memory the caller will write to;
//! [`validate`] only *reads*, and returns the segment table only if every
//! segment is somewhere the bootloader is willing to load it. A bad image must
//! never be able to overwrite the bootloader that is loading it, or make it
//! jump into the void.
//!
//! Image layout (ESP-IDF `esp_image_header_t`):
//!
//! ```text
//! header (24 B): magic 0xE9 | segment_count | .. | entry u32 | .. chip_id u16 @12 .. hash_appended @23
//! then per segment: load_addr u32 | data_len u32 | data
//! then padding, one checksum byte (XOR of all segment data, seed 0xEF) as the
//! last byte of a 16-byte block, then optionally a SHA-256 of everything before it.
//! ```
use core::ops::Range;
use sha2::{Digest, Sha256};

pub const MAX_SEGMENTS: usize = 16;
const HEADER_LEN: u32 = 24;
const SEGMENT_HEADER_LEN: u32 = 8;
const MAGIC: u8 = 0xE9;
const CHECKSUM_SEED: u8 = 0xEF;
const HASH_LEN: u32 = 32;

/// The chip-specific facts the bootloader validates against.
#[derive(Clone, Debug)]
pub struct MemoryMap {
    pub chip_id: u16,
    /// Flash-mapped through the MMU.
    pub drom: Range<u32>,
    pub irom: Range<u32>,
    /// Internal SRAM, instruction-bus alias, and its data-bus alias.
    pub iram: Range<u32>,
    pub dram: Range<u32>,
    pub rtc: Range<u32>,
    /// `iram - sram_alias_offset == dram`.
    pub sram_alias_offset: u32,
    /// Memory the bootloader (or the ROM) is using while it loads, in data-bus
    /// addresses: no segment may reach it.
    pub boot_window: Range<u32>,
    /// MMU page size: a flash-mapped segment's offset within a page must match its address's.
    pub mmu_page: u32,
}

impl MemoryMap {
    /// ESP32-C3, with the window `embewi-boot`'s linker script uses (`boot/boot.x`).
    pub const ESP32C3: MemoryMap = MemoryMap {
        chip_id: 0x0005,
        drom: 0x3C00_0000..0x3C80_0000,
        irom: 0x4200_0000..0x4280_0000,
        iram: 0x4037_C000..0x403E_0000,
        dram: 0x3FC8_0000..0x3FCE_0000,
        rtc: 0x5000_0000..0x5000_2000,
        sram_alias_offset: 0x0070_0000,
        boot_window: 0x3FCC_B000..0x3FCE_0000,
        mmu_page: 0x1_0000,
    };

    fn is_flash_mapped(&self, addr: u32) -> bool {
        self.drom.contains(&addr) || self.irom.contains(&addr)
    }
}

/// How much of the image is read to decide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verify {
    /// Header and segment table only: cheap, catches structural damage, not a flipped data byte.
    Structure,
    /// Plus the XOR checksum over every segment's data (reads the whole image).
    Checksum,
    /// Plus the appended SHA-256 (reads the whole image; requires `hash_appended`).
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageError {
    Read,
    BadMagic(u8),
    BadChip(u16),
    BadSegmentCount(u8),
    /// Segment `i` claims more data than the partition holds.
    SegmentOutsidePartition(usize),
    /// Segment `i` loads somewhere it must not, or crosses a region boundary.
    BadLoadRange(usize),
    /// Flash-mapped segment `i`: page offset differs between address and flash.
    Misaligned(usize),
    /// Segment `i` would overwrite the bootloader (or the ROM's working memory).
    OverlapsBootloader(usize),
    /// The entry point isn't inside any executable segment.
    BadEntry,
    /// Checksum/hash trailer doesn't fit in the partition.
    Truncated,
    BadChecksum,
    /// `Verify::Full` on an image without an appended hash.
    HashMissing,
    BadHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Segment {
    pub load: u32,
    pub len: u32,
    /// Absolute flash offset of the segment's data.
    pub data_offset: u32,
}

impl Segment {
    /// Where the bytes really go, in data-bus terms, for RAM segments; `None` for flash-mapped ones.
    pub fn ram_target(&self, map: &MemoryMap) -> Option<u32> {
        if map.iram.contains(&self.load) {
            Some(self.load - map.sram_alias_offset)
        } else if map.dram.contains(&self.load) || map.rtc.contains(&self.load) {
            Some(self.load)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct Image {
    pub entry: u32,
    pub segments: [Segment; MAX_SEGMENTS],
    pub count: usize,
    pub hash_appended: bool,
    /// Absolute flash offset one past the image (checksum, and hash if any).
    pub end: u32,
}

impl Image {
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
}

/// Where the bytes come from: the flash reader (ROM) in the bootloader, a buffer in tests.
pub trait Read {
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), ()>;
}

fn read_exact(r: &mut impl Read, offset: u32, buf: &mut [u8]) -> Result<(), ImageError> {
    r.read(offset, buf).map_err(|()| ImageError::Read)
}

/// Validates the image at `part_offset` (a partition of `part_size` bytes).
pub fn validate(
    reader: &mut impl Read,
    part_offset: u32,
    part_size: u32,
    map: &MemoryMap,
    verify: Verify,
) -> Result<Image, ImageError> {
    let part_end = part_offset.checked_add(part_size).ok_or(ImageError::Truncated)?;

    let mut header = [0u8; HEADER_LEN as usize];
    read_exact(reader, part_offset, &mut header)?;
    if header[0] != MAGIC {
        return Err(ImageError::BadMagic(header[0]));
    }
    let count = header[1];
    if count == 0 || usize::from(count) > MAX_SEGMENTS {
        return Err(ImageError::BadSegmentCount(count));
    }
    let entry = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let chip_id = u16::from_le_bytes([header[12], header[13]]);
    if chip_id != map.chip_id {
        return Err(ImageError::BadChip(chip_id));
    }
    let hash_appended = header[23] == 1;

    let mut segments = [Segment::default(); MAX_SEGMENTS];
    let mut cursor = part_offset + HEADER_LEN;
    let mut entry_ok = false;
    for i in 0..usize::from(count) {
        let mut raw = [0u8; SEGMENT_HEADER_LEN as usize];
        read_exact(reader, cursor, &mut raw)?;
        let load = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let data_offset = cursor + SEGMENT_HEADER_LEN;
        let data_end = data_offset.checked_add(len).ok_or(ImageError::SegmentOutsidePartition(i))?;
        if data_end > part_end {
            return Err(ImageError::SegmentOutsidePartition(i));
        }
        let seg = Segment { load, len, data_offset };
        if len > 0 {
            check_segment(&seg, i, map)?;
            let executable = map.irom.contains(&load) || map.iram.contains(&load);
            if executable && entry >= load && entry < load.saturating_add(len) {
                entry_ok = true;
            }
        }
        segments[i] = seg;
        cursor = data_end;
    }
    if !entry_ok {
        return Err(ImageError::BadEntry);
    }

    // Padding up to the last byte of a 16-byte block (offsets relative to the image start), then the checksum.
    let rel = cursor - part_offset;
    let checksum_at = part_offset + rel + (15 - rel % 16);
    let end = checksum_at + 1 + if hash_appended { HASH_LEN } else { 0 };
    if end > part_end {
        return Err(ImageError::Truncated);
    }
    let image = Image { entry, segments, count: usize::from(count), hash_appended, end };

    if verify != Verify::Structure {
        let mut expected = [0u8; 1];
        read_exact(reader, checksum_at, &mut expected)?;
        let mut xor = CHECKSUM_SEED;
        let mut buf = [0u8; 256];
        for seg in image.segments() {
            let mut done = 0;
            while done < seg.len {
                let n = (seg.len - done).min(buf.len() as u32) as usize;
                read_exact(reader, seg.data_offset + done, &mut buf[..n])?;
                xor = buf[..n].iter().fold(xor, |acc, b| acc ^ b);
                done += n as u32;
            }
        }
        if xor != expected[0] {
            return Err(ImageError::BadChecksum);
        }
    }

    if verify == Verify::Full {
        if !hash_appended {
            return Err(ImageError::HashMissing);
        }
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 256];
        let mut at = part_offset;
        while at < checksum_at + 1 {
            let n = (checksum_at + 1 - at).min(buf.len() as u32) as usize;
            read_exact(reader, at, &mut buf[..n])?;
            hasher.update(&buf[..n]);
            at += n as u32;
        }
        let mut stored = [0u8; HASH_LEN as usize];
        read_exact(reader, checksum_at + 1, &mut stored)?;
        if hasher.finalize()[..] != stored[..] {
            return Err(ImageError::BadHash);
        }
    }
    Ok(image)
}

fn check_segment(seg: &Segment, i: usize, map: &MemoryMap) -> Result<(), ImageError> {
    let end = seg.load.checked_add(seg.len).ok_or(ImageError::BadLoadRange(i))?;
    if map.is_flash_mapped(seg.load) {
        let region = if map.drom.contains(&seg.load) { &map.drom } else { &map.irom };
        if end > region.end {
            return Err(ImageError::BadLoadRange(i));
        }
        if seg.load % map.mmu_page != seg.data_offset % map.mmu_page {
            return Err(ImageError::Misaligned(i));
        }
        return Ok(());
    }
    let region = if map.iram.contains(&seg.load) {
        &map.iram
    } else if map.dram.contains(&seg.load) {
        &map.dram
    } else if map.rtc.contains(&seg.load) {
        &map.rtc
    } else {
        return Err(ImageError::BadLoadRange(i));
    };
    if end > region.end {
        return Err(ImageError::BadLoadRange(i));
    }
    if let Some(target) = seg.ram_target(map) {
        // RTC memory is nowhere near the bootloader's window; SRAM aliases are compared as data-bus addresses.
        if !map.rtc.contains(&seg.load) && target < map.boot_window.end && target + seg.len > map.boot_window.start {
            return Err(ImageError::OverlapsBootloader(i));
        }
    }
    Ok(())
}
