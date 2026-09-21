use crate::image::*;
use sha2::{Digest, Sha256};

const MAP: MemoryMap = MemoryMap::ESP32C3;
const PART_OFFSET: u32 = 0x20000;
const PART_SIZE: u32 = 0x180000;

/// Flash as a byte vector; reads outside it fail like a dead flash would.
struct Mem {
    bytes: Vec<u8>,
    reads: u64,
    fail_at: Option<u32>,
}

impl Mem {
    fn new(image: &[u8]) -> Mem {
        let mut bytes = vec![0xFF; PART_OFFSET as usize];
        bytes.extend_from_slice(image);
        bytes.resize((PART_OFFSET + PART_SIZE) as usize, 0xFF);
        Mem { bytes, reads: 0, fail_at: None }
    }
}

impl Read for Mem {
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), ()> {
        if self.fail_at.is_some_and(|f| offset <= f && f < offset + buf.len() as u32) {
            return Err(());
        }
        let (start, end) = (offset as usize, offset as usize + buf.len());
        if end > self.bytes.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.bytes[start..end]);
        self.reads += buf.len() as u64;
        Ok(())
    }
}

/// Builds an image like espflash does: header, segments, padding, checksum, SHA-256.
struct Builder {
    entry: u32,
    chip_id: u16,
    segments: Vec<(u32, Vec<u8>)>,
    hash: bool,
}

impl Builder {
    /// A small but realistic C3 image: flash-mapped DROM+IROM and RAM segments.
    fn c3() -> Builder {
        Builder {
            entry: 0x4203_0072,
            chip_id: 5,
            hash: true,
            segments: vec![
                (0x3C00_0020, (0..300u32).map(|i| i as u8).collect()),
                (0x3FC8_9A30, vec![0x11; 100]),
                (0x4038_0000, vec![0x22; 64]),
                (0x4203_0020, (0..777u32).map(|i| (i * 7) as u8).collect()),
            ],
        }
    }

    fn build(&self) -> Vec<u8> {
        let mut out = vec![0u8; 24];
        out[0] = 0xE9;
        out[1] = self.segments.len() as u8;
        out[4..8].copy_from_slice(&self.entry.to_le_bytes());
        out[12..14].copy_from_slice(&self.chip_id.to_le_bytes());
        out[23] = self.hash as u8;
        let mut xor = 0xEFu8;
        for (load, data) in &self.segments {
            out.extend_from_slice(&load.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            // Flash-mapped segments need paddr % 64K == vaddr % 64K: pad the file so they line up.
            if MAP.drom.contains(load) || MAP.irom.contains(load) {
                while (out.len() % 0x10000) != (*load as usize % 0x10000) {
                    // Only possible by growing the previous data; keep the builder simple: fixed layout below.
                    out.push(0);
                    break;
                }
            }
            out.extend_from_slice(data);
            xor = data.iter().fold(xor, |a, b| a ^ b);
        }
        while out.len() % 16 != 15 {
            out.push(0);
        }
        out.push(xor);
        if self.hash {
            let digest = Sha256::digest(&out);
            out.extend_from_slice(&digest);
        }
        out
    }
}

fn ok_image() -> Vec<u8> {
    Builder::c3().build()
}

/// Segment `load` and its flash offset must agree modulo the MMU page: relocate the test image's
/// flash-mapped segments by choosing loads that match where the builder put the data.
fn aligned_builder() -> Builder {
    let mut b = Builder::c3();
    // header 24 + seg hdr 8 -> data at image offset 32; PART_OFFSET is page-aligned, so DROM load = 0x3C000000 + 32 - wait: +0x20.
    b.segments[0].0 = 0x3C00_0000 + 32;
    // Data of segment 3 starts after: 32 + 300 + (8+100) + (8+64) + 8 = 520 -> load offset 520 within the page.
    b.segments[3].0 = 0x4200_0000 + 520;
    b.entry = 0x4200_0000 + 520 + 4;
    b
}

fn validate_bytes(image: &[u8], verify: Verify) -> Result<Image, ImageError> {
    validate(&mut Mem::new(image), PART_OFFSET, PART_SIZE, &MAP, verify)
}

#[test]
fn a_well_formed_image_passes_every_level() {
    let image = aligned_builder().build();
    for verify in [Verify::Structure, Verify::Checksum, Verify::Full] {
        let info = validate_bytes(&image, verify).unwrap_or_else(|e| panic!("{verify:?}: {e:?}"));
        assert_eq!(info.count, 4);
        assert_eq!(info.entry, 0x4200_0000 + 520 + 4);
        assert_eq!(info.end, PART_OFFSET + image.len() as u32);
    }
}

#[test]
fn structure_reads_only_headers_but_checksum_and_full_read_everything() {
    let image = aligned_builder().build();
    let mut mem = Mem::new(&image);
    validate(&mut mem, PART_OFFSET, PART_SIZE, &MAP, Verify::Structure).unwrap();
    assert!(mem.reads < 200, "Structure read {} bytes", mem.reads);
    let mut mem = Mem::new(&image);
    validate(&mut mem, PART_OFFSET, PART_SIZE, &MAP, Verify::Full).unwrap();
    assert!(mem.reads > 2 * 1000, "Full read only {} bytes", mem.reads);
}

#[test]
fn structural_damage_is_refused_with_a_reason() {
    let base = aligned_builder();
    let mutate = |f: &dyn Fn(&mut Builder)| {
        let mut b = aligned_builder();
        f(&mut b);
        validate_bytes(&b.build(), Verify::Structure).unwrap_err()
    };
    assert_eq!(mutate(&|b| b.chip_id = 9), ImageError::BadChip(9));
    assert_eq!(mutate(&|b| b.segments.clear()), ImageError::BadSegmentCount(0));
    assert_eq!(mutate(&|b| b.entry = 0x1000_0000), ImageError::BadEntry);
    // entry inside a *data* segment is not executable
    assert_eq!(mutate(&|b| b.entry = 0x3FC8_9A30 + 4), ImageError::BadEntry);
    assert_eq!(mutate(&|b| b.segments[1].0 = 0x6000_0000), ImageError::BadLoadRange(1));
    // a RAM segment running off the end of its region
    assert_eq!(mutate(&|b| b.segments[1].0 = 0x3FCD_FFF0), ImageError::BadLoadRange(1));
    // ... or landing on the bootloader, through either alias
    assert_eq!(mutate(&|b| b.segments[1].0 = 0x3FCC_B000), ImageError::OverlapsBootloader(1));
    assert_eq!(mutate(&|b| b.segments[2].0 = 0x403C_B000), ImageError::OverlapsBootloader(2));
    // straddling the start of the window (64 bytes from 0x403CAFF0 reach 0x3FCCB030 in data-bus terms) ...
    assert_eq!(mutate(&|b| b.segments[2].0 = 0x403C_AFF0), ImageError::OverlapsBootloader(2));
    // ... while ending exactly at it is fine, and is not refused
    let mut b = aligned_builder();
    b.segments[2].0 = 0x403C_AFC0; // 64 bytes -> ends at 0x3FCCB000
    assert!(validate_bytes(&b.build(), Verify::Structure).is_ok());
    // flash-mapped with a page offset the MMU can't honour
    assert_eq!(mutate(&|b| b.segments[0].0 += 4), ImageError::Misaligned(0));
    let _ = base;
}

#[test]
fn magic_and_truncation_are_refused() {
    let mut image = aligned_builder().build();
    image[0] = 0x65;
    assert_eq!(validate_bytes(&image, Verify::Structure).unwrap_err(), ImageError::BadMagic(0x65));

    let image = aligned_builder().build();
    let mut mem = Mem::new(&image);
    // partition too small for the trailer
    let err = validate(&mut mem, PART_OFFSET, image.len() as u32 - 1, &MAP, Verify::Structure).unwrap_err();
    assert!(matches!(err, ImageError::Truncated | ImageError::SegmentOutsidePartition(_)), "{err:?}");
    // a segment that claims more than the partition holds
    let mut image = aligned_builder().build();
    image[28..32].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
    assert_eq!(validate_bytes(&image, Verify::Structure).unwrap_err(), ImageError::SegmentOutsidePartition(0));
}

#[test]
fn data_corruption_needs_checksum_or_full() {
    let mut image = aligned_builder().build();
    image[200] ^= 0x55; // inside segment 0's data
    assert!(validate_bytes(&image, Verify::Structure).is_ok(), "Structure doesn't read data");
    assert_eq!(validate_bytes(&image, Verify::Checksum).unwrap_err(), ImageError::BadChecksum);
    assert_eq!(validate_bytes(&image, Verify::Full).unwrap_err(), ImageError::BadChecksum);
}

#[test]
fn the_appended_hash_catches_what_the_checksum_misses() {
    // Two flipped bytes cancel out in an XOR checksum.
    let mut image = aligned_builder().build();
    image[200] ^= 0x33;
    image[201] ^= 0x33;
    assert!(validate_bytes(&image, Verify::Checksum).is_ok());
    assert_eq!(validate_bytes(&image, Verify::Full).unwrap_err(), ImageError::BadHash);
}

#[test]
fn full_needs_a_hash_and_a_dead_flash_is_an_error_not_a_pass() {
    let mut b = aligned_builder();
    b.hash = false;
    let image = b.build();
    assert!(validate_bytes(&image, Verify::Checksum).is_ok());
    assert_eq!(validate_bytes(&image, Verify::Full).unwrap_err(), ImageError::HashMissing);

    let image = aligned_builder().build();
    for at in [PART_OFFSET, PART_OFFSET + 26, PART_OFFSET + 200, PART_OFFSET + image.len() as u32 - 5] {
        let mut mem = Mem::new(&image);
        mem.fail_at = Some(at);
        assert_eq!(validate(&mut mem, PART_OFFSET, PART_SIZE, &MAP, Verify::Full).unwrap_err(), ImageError::Read, "at {at:#x}");
    }
}

#[test]
fn any_single_byte_flip_in_the_hashed_region_is_caught_by_full() {
    let image = aligned_builder().build();
    let hashed = image.len() - 32;
    for i in 0..hashed {
        let mut bad = image.clone();
        bad[i] ^= 0x01;
        assert!(validate_bytes(&bad, Verify::Full).is_err(), "flip at byte {i} was accepted");
    }
    // and in the stored hash itself
    for i in hashed..image.len() {
        let mut bad = image.clone();
        bad[i] ^= 0x80;
        assert_eq!(validate_bytes(&bad, Verify::Full).unwrap_err(), ImageError::BadHash);
    }
}

/// Whatever garbage the headers hold, an accepted image never has a segment where the
/// bootloader must not be written.
#[test]
fn accepted_images_never_overlap_the_bootloader_or_leave_the_map() {
    let image = aligned_builder().build();
    let mut rng = 0x1234_5678_9ABC_DEF0u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let (mut accepted, mut rejected) = (0, 0);
    for _ in 0..60_000 {
        let mut bad = image.clone();
        // scramble 1-4 bytes of the header / first segment headers (the first ~140 bytes)
        for _ in 0..=(next() % 4) {
            let at = (next() % 140) as usize;
            bad[at] = next() as u8;
        }
        match validate_bytes(&bad, Verify::Structure) {
            Err(_) => rejected += 1,
            Ok(info) => {
                accepted += 1;
                for seg in info.segments().iter().filter(|s| s.len > 0) {
                    let end = seg.load + seg.len;
                    let inside = |r: &std::ops::Range<u32>| r.contains(&seg.load) && end <= r.end;
                    assert!(inside(&MAP.drom) || inside(&MAP.irom) || inside(&MAP.iram) || inside(&MAP.dram) || inside(&MAP.rtc), "{seg:?} outside the memory map");
                    if let Some(t) = seg.ram_target(&MAP) {
                        if !MAP.rtc.contains(&seg.load) {
                            assert!(t + seg.len <= MAP.boot_window.start || t >= MAP.boot_window.end, "{seg:?} overlaps the bootloader");
                        }
                    }
                    assert!(seg.data_offset + seg.len <= PART_OFFSET + PART_SIZE);
                }
                assert!(info.end <= PART_OFFSET + PART_SIZE);
            }
        }
    }
    assert!(rejected > 1000 && accepted > 100, "mutations too weak: {accepted} accepted / {rejected} rejected");
}

/// The image the device actually runs, if it has been built (`web/firmware/esp32c3/app.bin`, git-ignored).
#[test]
fn the_real_agent_image_validates_and_a_flipped_byte_does_not() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/firmware/esp32c3/app.bin");
    let Ok(image) = std::fs::read(path) else {
        eprintln!("skipped: {path} not built");
        return;
    };
    let info = validate_bytes(&image, Verify::Full).expect("the agent image must pass the bootloader's own check");
    assert_eq!(info.count, 5);
    assert_eq!(info.entry, 0x4203_0072);
    assert_eq!(info.end, PART_OFFSET + image.len() as u32);
    // spot flips across header, segment headers and data
    let hashed = image.len() - 32;
    let mut at = 0;
    while at < hashed {
        let mut bad = image.clone();
        bad[at] ^= 0x04;
        assert!(validate_bytes(&bad, Verify::Full).is_err(), "flip at {at} accepted");
        at += if at < 200 { 1 } else { 60_013 };
    }
}
