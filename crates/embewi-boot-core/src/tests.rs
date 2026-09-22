use super::*;
use std::collections::HashSet;

// --- helpers ---------------------------------------------------------------

type Flash = [Raw; SECTOR_COUNT];

fn e(seq: u32, st: u32) -> Entry {
    Entry::new(seq, st)
}

fn raw(seq: u32, st: u32) -> Raw {
    e(seq, st).encode()
}

fn boots(images: [bool; 2]) -> impl FnMut(u8) -> bool {
    move |slot| images[slot as usize]
}

fn plan(f: &Flash, images: [bool; 2]) -> Plan {
    plan_boot(*f, 2, &mut boots(images))
}

/// Executes one flash command on `f`, completely. Programming only clears bits.
fn apply_op(f: &mut Flash, op: Op) {
    match op {
        Op::Erase { sector } => f[sector as usize] = BLANK,
        Op::Program { sector, offset, len, data } => {
            for i in 0..len as usize {
                f[sector as usize][offset as usize + i] &= data[i];
            }
        }
    }
}

fn full(mut f: Flash, writes: &[Write]) -> Flash {
    for w in writes {
        for op in w.ops() {
            apply_op(&mut f, op);
        }
    }
    f
}

fn decoded(f: &Flash) -> [Decoded; 2] {
    [decode(&f[0]), decode(&f[1])]
}

// --- codec -----------------------------------------------------------------

#[test]
fn crc_matches_the_rom_and_esp_bootloader_esp_idf_test_vector() {
    // esp-bootloader-esp-idf's SLOT_COUNT_1_VALID: seq=1, crc bytes 154,152,67,71.
    assert_eq!(crc32_le(u32::MAX, &1u32.to_le_bytes()), 0x4743_989A);
    // ... which is also the trailing IDF crc of an Embewi entry: tooling reading only that keeps working.
    assert_eq!(&e(1, state::VALID).encode()[28..32], &[154, 152, 67, 71]);
}

#[test]
fn entries_roundtrip_and_body_lacks_only_the_commit_word() {
    for (seq, st) in [(1, state::VALID), (2, state::NEW), (7, state::PENDING_VERIFY), (u32::MAX - 1, state::ABORTED)] {
        let entry = e(seq, st);
        assert_eq!(decode(&entry.encode()), Decoded::Ok(entry));
        let body = entry.body();
        assert_eq!(decode(&body), Decoded::Corrupt, "a body without its commit word must not be accepted");
        let diff: Vec<usize> = (0..ENTRY_SIZE).filter(|&i| body[i] != entry.encode()[i]).collect();
        assert_eq!(diff, (OFF_COMMIT..OFF_COMMIT + 4).collect::<Vec<_>>());
    }
}

#[test]
fn every_single_bit_flip_of_a_committed_entry_is_rejected() {
    let good = raw(3, state::PENDING_VERIFY);
    assert!(matches!(decode(&good), Decoded::Ok(_)));
    for byte in 0..ENTRY_SIZE {
        for bit in 0..8 {
            let mut bad = good;
            bad[byte] ^= 1 << bit;
            assert_eq!(decode(&bad), Decoded::Corrupt, "flip of byte {byte} bit {bit}");
        }
    }
}

#[test]
fn only_known_states_and_sane_sequences_are_accepted() {
    assert_eq!(decode(&e(1, 7).encode()), Decoded::Corrupt);
    assert_eq!(decode(&e(1, u32::MAX).encode()), Decoded::Corrupt);
    assert_eq!(decode(&e(0, state::VALID).encode()), Decoded::Corrupt);
    assert_eq!(decode(&e(u32::MAX, state::VALID).encode()), Decoded::Corrupt);
    assert_eq!(decode(&BLANK), Decoded::Blank);
}

/// The ESP-IDF entry format, as `esp-bootloader-esp-idf`/ESP-IDF write it.
fn idf_entry(seq: u32, st: u32) -> Raw {
    let mut r = BLANK;
    r[0..4].copy_from_slice(&seq.to_le_bytes());
    r[24..28].copy_from_slice(&st.to_le_bytes());
    r[28..32].copy_from_slice(&crc32_le(u32::MAX, &seq.to_le_bytes()).to_le_bytes());
    r
}

/// ESP-IDF's rule: crc over `ota_seq` only; the state is trusted as read.
fn idf_reads(r: &Raw) -> Option<(u32, u32)> {
    let (seq, st, crc) = (word(r, 0), word(r, 24), word(r, 28));
    (seq != u32::MAX && crc == crc32_le(u32::MAX, &seq.to_le_bytes())).then_some((seq, st))
}

#[test]
fn the_idf_entry_format_is_forgeable_and_ours_is_not() {
    // Programming `New` (0) into an erased state word: a cut can leave any value that still has all of
    // 0's ones -- i.e. anything, including 2 = Valid -- while sequence and crc are already complete.
    let target = idf_entry(2, state::NEW);
    let mut torn = target;
    torn[24..28].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(2u32 & 0, 0, "2 is reachable from erased on the way to 0 (bits only get cleared)");
    assert_eq!(idf_reads(&torn), Some((2, state::VALID)), "ESP-IDF reads a torn `New` as `Valid`");
    // Ours: the same tear can't be accepted -- state is under ext_crc and the commit word is missing.
    let mut ours = e(2, state::NEW).body();
    ours[24..28].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(decode(&ours), Decoded::Corrupt);
    let mut committed = e(2, state::NEW).encode();
    committed[24..28].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(decode(&committed), Decoded::Corrupt, "even with the commit word, a wrong state fails ext_crc");
}

#[test]
fn entries_written_the_idf_way_are_ignored_by_design() {
    // No legacy mode: the ESP-IDF two-step activation leaves no acceptable entry, so its stale-state
    // hazard (a sequence-valid entry carrying an old `Valid`) cannot boot anything.
    assert_eq!(decode(&idf_entry(1, state::VALID)), Decoded::Corrupt);
    let stale_valid = idf_entry(4, state::VALID); // seq set, state left from an earlier cycle
    let f: Flash = [raw(3, state::VALID), stale_valid];
    let p = plan(&f, [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 3 }, "the stale entry is not a candidate");
    assert_eq!(p.writes().count(), 0);
    // A device that still has an ESP-IDF `otadata` is re-seeded on ota_0: the migration path.
    let p = plan(&[idf_entry(1, state::VALID), BLANK], [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 0, entry: e(1, state::VALID) }]);
}

#[test]
fn slot_mapping_follows_esp_idf() {
    assert_eq!([slot_of(1, 2), slot_of(2, 2), slot_of(3, 2), slot_of(4, 2)], [0, 1, 0, 1]);
    assert_eq!([slot_of(1, 3), slot_of(2, 3), slot_of(3, 3), slot_of(4, 3)], [0, 1, 2, 0]);
}

#[test]
fn a_write_is_erase_then_body_then_a_separate_commit() {
    let w = Write { sector: 1, entry: e(2, state::NEW) };
    let [erase, body, commit] = w.ops();
    assert_eq!(erase, Op::Erase { sector: 1 });
    assert_eq!(body, Op::Program { sector: 1, offset: 0, len: 32, data: e(2, state::NEW).body() });
    let Op::Program { offset, len, data, .. } = commit else { panic!() };
    assert_eq!((offset as usize, len), (OFF_COMMIT, 4));
    assert_eq!(&data[..4], &COMMIT.to_le_bytes());
    // replaying them on an erased sector yields exactly the committed entry
    let f = full([BLANK, BLANK], &[w]);
    assert_eq!(f[1], e(2, state::NEW).encode());
}

// --- plan_boot -------------------------------------------------------------

#[test]
fn blank_otadata_is_a_normal_first_boot_that_seeds_slot_0() {
    let p = plan(&[BLANK, BLANK], [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 0, entry: e(1, state::VALID) }]);
}

#[test]
fn first_boot_halts_when_slot_0_is_not_bootable() {
    let p = plan(&[BLANK, BLANK], [false, true]);
    assert_eq!(p.boot, Boot::Halt(Halt::NoImage));
    assert_eq!(p.writes().count(), 0);
}

#[test]
fn a_torn_seed_is_retried_like_a_first_boot() {
    for torn in [e(1, state::VALID).body(), {
        let mut r = e(1, state::VALID).encode();
        r[OFF_COMMIT] = 0xFF; // commit word half there
        r
    }] {
        let p = plan(&[torn, BLANK], [true, true]);
        assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    }
}

#[test]
fn a_valid_entry_boots_without_any_write() {
    let p = plan(&[raw(1, state::VALID), BLANK], [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().count(), 0);
}

#[test]
fn a_new_entry_is_marked_pending_before_it_boots() {
    let p = plan(&[raw(1, state::VALID), raw(2, state::NEW)], [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 1, sector: 1, seq: 2 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 1, entry: e(2, state::PENDING_VERIFY) }]);
}

#[test]
fn a_pending_entry_that_rebooted_is_aborted_and_the_previous_slot_boots() {
    let p = plan(&[raw(1, state::VALID), raw(2, state::PENDING_VERIFY)], [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 1, entry: e(2, state::ABORTED) }]);
}

#[test]
fn an_unbootable_candidate_is_marked_invalid_and_the_next_one_boots() {
    let p = plan(&[raw(1, state::VALID), raw(2, state::NEW)], [true, false]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 1, entry: e(2, state::INVALID) }]);
}

#[test]
fn nothing_usable_halts_explicitly_and_never_reseeds() {
    let p = plan(&[raw(1, state::INVALID), raw(2, state::ABORTED)], [true, true]);
    assert_eq!(p.boot, Boot::Halt(Halt::NoUsableEntry));
    let p = plan(&[raw(1, state::VALID), raw(2, state::NEW)], [false, false]);
    assert_eq!(p.boot, Boot::Halt(Halt::NoUsableEntry));
}

// --- activate / confirm / reject -------------------------------------------

#[test]
fn activate_refuses_without_a_known_good_image() {
    assert_eq!(activate([BLANK, BLANK], 2, 1), Err(ActivateError::NoValidBase));
    assert_eq!(activate([raw(1, state::NEW), BLANK], 2, 1), Err(ActivateError::NoValidBase));
}

#[test]
fn activate_writes_one_complete_new_entry_beside_the_valid_one() {
    assert_eq!(activate([raw(1, state::VALID), BLANK], 2, 1), Ok(Write { sector: 1, entry: e(2, state::NEW) }));
    assert_eq!(activate([BLANK, raw(1, state::VALID)], 2, 1).unwrap().sector, 0);
}

#[test]
fn activate_after_a_rollback_overwrites_the_dead_entry_not_the_valid_one() {
    // Raw sequence comparison would pick sector 0 and erase the only good entry.
    let w = activate([raw(1, state::VALID), raw(2, state::ABORTED)], 2, 1).unwrap();
    assert_eq!(w.sector, 1, "the Aborted sector is the one to reuse");
    assert_eq!(w.entry, e(4, state::NEW)); // smallest seq above 2 that selects slot 1 (3 selects slot 0)
}

#[test]
fn activate_alternates_slots_across_cycles() {
    let mut f: Flash = [raw(1, state::VALID), BLANK];
    for (target, expected_seq) in [(1u8, 2u32), (0, 3), (1, 4), (0, 5)] {
        let w = activate(f, 2, target).unwrap();
        assert_eq!(w.entry.seq, expected_seq);
        f = full(f, &[w]);
        let boot: Vec<Write> = plan(&f, [true, true]).writes().collect();
        f = full(f, &boot);
        let confirmed = confirm(f).unwrap();
        f = full(f, &[confirmed]);
    }
}

#[test]
fn confirm_and_reject_act_on_the_pending_entry_only() {
    let f: Flash = [raw(1, state::VALID), raw(2, state::PENDING_VERIFY)];
    assert_eq!(confirm(f), Some(Write { sector: 1, entry: e(2, state::VALID) }));
    assert_eq!(reject(f), Some(Write { sector: 1, entry: e(2, state::INVALID) }));
    // Still `New`: the bootloader did not mark it -- a chain anomaly, reported by None.
    assert_eq!(confirm([raw(1, state::VALID), raw(2, state::NEW)]), None);
    assert_eq!(confirm([raw(1, state::VALID), BLANK]), None);
}

// --- power-cut simulation --------------------------------------------------
//
// One update is three flash commands (erase, body, commit). A cut can land
// between any two, or inside one. What a cut *inside* a command leaves:
//
// * `Sequential` -- how NOR flash usually behaves: bytes in address order, a
//   prefix complete, the next byte with any subset of its bits cleared. Kept as
//   a fast secondary check.
// * `AnyOrder` -- the REFERENCE model, because no datasheet promises the above:
//   fields programmed in any order, each complete or erased, the state word
//   and the commit word with any reachable partial value.
//
// A cut inside an erase leaves the old bytes, erased bytes, or garbage.

#[derive(Clone, Copy, PartialEq, Debug)]
enum Model {
    Sequential,
    AnyOrder,
}

/// Field boundaries of an entry.
const GROUPS: [(usize, usize); 8] = [(0, 4), (4, 8), (8, 12), (12, 16), (16, 20), (20, 24), (24, 28), (28, 32)];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Values a byte can hold mid-programming towards `target`, from erased: `target` plus any of the bits it clears
/// still set.
fn partial_bytes(target: u8, thorough: bool) -> Vec<u8> {
    let clears = !target;
    if !thorough {
        return vec![0xFF, target, 0xFF & !(clears & 0x0F), 0xFF & !(clears & 0xF0)];
    }
    let mut out = Vec::new();
    let mut sub = clears;
    loop {
        out.push(0xFF & !sub);
        if sub == 0 {
            break;
        }
        sub = (sub - 1) & clears;
    }
    out
}

fn erase_variants(cur: &Flash, s: usize, model: Model, out: &mut HashSet<Flash>) {
    for prefix in [1usize, 4, 8, 16, 24, 31] {
        let mut f = *cur;
        f[s][..prefix].fill(0xFF);
        out.insert(f);
    }
    for pattern in [0x00u8, 0xA5, 0x5A] {
        let mut f = *cur;
        f[s] = [pattern; ENTRY_SIZE];
        out.insert(f);
    }
    if model == Model::AnyOrder {
        // any subset of the fields already erased, the rest still the old bytes
        for mask in 0u32..(1 << GROUPS.len()) {
            let mut f = *cur;
            for (g, &(a, b)) in GROUPS.iter().enumerate() {
                if mask & (1 << g) != 0 {
                    f[s][a..b].fill(0xFF);
                }
            }
            out.insert(f);
        }
    }
}

fn program_variants(cur: &Flash, s: usize, offset: usize, len: usize, data: &[u8], model: Model, thorough: bool, out: &mut HashSet<Flash>) {
    let target: Vec<u8> = (0..len).map(|i| cur[s][offset + i] & data[i]).collect();
    let mut with = |f: &mut Flash, at: usize, bytes: &[u8]| f[s][offset + at..offset + at + bytes.len()].copy_from_slice(bytes);
    match model {
        Model::Sequential => {
            for k in 0..len {
                for partial in partial_bytes(target[k], thorough) {
                    let mut f = *cur;
                    with(&mut f, 0, &target[..k]);
                    f[s][offset + k] = cur[s][offset + k] & partial;
                    out.insert(f);
                }
            }
        }
        Model::AnyOrder => {
            // Groups this command programs (relative to the command).
            let groups: Vec<(usize, usize)> = GROUPS
                .iter()
                .filter(|&&(a, b)| a >= offset && b <= offset + len && target[a - offset..b - offset] != cur[s][a..b])
                .copied()
                .collect();
            let combos = |skip: Option<usize>| -> Vec<Flash> {
                let others: Vec<(usize, usize)> = groups.iter().copied().filter(|g| Some(g.0) != skip).collect();
                (0u32..(1 << others.len()))
                    .map(|mask| {
                        let mut f = *cur;
                        for (i, &(a, b)) in others.iter().enumerate() {
                            if mask & (1 << i) != 0 {
                                with(&mut f, a - offset, &target[a - offset..b - offset]);
                            }
                        }
                        f
                    })
                    .collect()
            };
            // Every group complete-or-erased.
            out.extend(combos(None));
            // One group at a time with a reachable *partial* value, the others complete-or-erased.
            for &(a, b) in &groups {
                if b - a != 4 || !(a == OFF_COMMIT || a == OFF_STATE) {
                    continue; // partial values only matter where a word is compared for equality or read as a state
                }
                let t = &target[a - offset..b - offset];
                let words: Vec<[u8; 4]> = if a == OFF_STATE {
                    let cand = |x: u8| -> Vec<u8> {
                        let mut v = vec![0xFF, x, 0x00, 0x01, 0x02, 0x03, 0x04];
                        if !thorough {
                            v = vec![0xFF, x, 0x02];
                        }
                        v.into_iter().filter(|c| c & x == x).collect()
                    };
                    let mut w = Vec::new();
                    for b0 in cand(t[0]) {
                        for b1 in cand(t[1]) {
                            for b2 in cand(t[2]) {
                                for b3 in cand(t[3]) {
                                    w.push([b0, b1, b2, b3]);
                                }
                            }
                        }
                    }
                    w
                } else {
                    // commit word: T plus any subset of the bits it clears
                    let tw = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
                    let mut vals: HashSet<u32> = HashSet::new();
                    for bit in 0..32 {
                        if tw & (1 << bit) == 0 {
                            vals.insert(tw | (1 << bit)); // one bit short of complete
                            vals.insert(u32::MAX & !(1 << bit)); // one bit cleared
                        }
                    }
                    vals.insert(u32::MAX);
                    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ u64::from(tw));
                    for _ in 0..(if thorough { 200 } else { 30 }) {
                        vals.insert(tw | (rng.next() as u32 & !tw));
                    }
                    vals.into_iter().map(u32::to_le_bytes).collect()
                };
                for base in combos(Some(a)) {
                    for word in &words {
                        let mut f = base;
                        with(&mut f, a - offset, word);
                        out.insert(f);
                    }
                }
            }
        }
    }
}

/// Every flash state reachable by cutting power somewhere inside `writes` (applied in order), plus the
/// fully-applied one.
fn states_after_cuts(before: &Flash, writes: &[Write], model: Model, thorough: bool) -> Vec<Flash> {
    let mut out: HashSet<Flash> = HashSet::new();
    let mut cur = *before;
    for w in writes {
        for op in w.ops() {
            out.insert(cur); // before this command
            match op {
                Op::Erase { sector } => erase_variants(&cur, sector as usize, model, &mut out),
                Op::Program { sector, offset, len, data } => {
                    program_variants(&cur, sector as usize, offset as usize, len as usize, &data, model, thorough, &mut out)
                }
            }
            apply_op(&mut cur, op);
            out.insert(cur); // after it
        }
    }
    out.into_iter().collect()
}

struct Ctx {
    /// Entries that may legitimately read `Valid`: those that exist, and those being written as `Valid`.
    allowed_valid: Vec<Entry>,
    /// A previously validated image must stay selectable: one of `allowed_valid` must remain in the flash.
    keep_valid: bool,
    images: [bool; 2],
}

/// The safety properties on one flash state. Returns the plan, or what broke.
fn check(f: &Flash, ctx: &Ctx) -> Result<Plan, String> {
    let dec = decoded(f);
    let mut valid_present = false;
    for d in &dec {
        if let Decoded::Ok(entry) = d {
            if entry.state == state::VALID {
                if !ctx.allowed_valid.contains(entry) {
                    return Err(format!("forged Valid entry {entry:?}"));
                }
                valid_present = true;
            }
        }
    }
    if ctx.keep_valid && !valid_present {
        return Err("no Valid entry left although one existed and its replacement was not committed".into());
    }
    let p = plan_boot(*f, 2, &mut boots(ctx.images));
    match p.boot {
        Boot::Slot { slot, sector, seq } => {
            if !ctx.images[slot as usize] {
                return Err(format!("boots slot {slot} whose image is not bootable"));
            }
            match dec[sector as usize] {
                Decoded::Ok(entry) => match Trust::of(entry.state) {
                    Trust::Valid => {}
                    Trust::Unproven => {
                        let marked = p
                            .writes()
                            .any(|w| w.sector == sector && w.entry == e(entry.seq, state::PENDING_VERIFY));
                        if !marked {
                            return Err(format!("boots unproven entry {entry:?} without marking it Pending"));
                        }
                    }
                    other => return Err(format!("boots an entry whose trust is {other:?}")),
                },
                _ => {
                    let seeded = sector == 0 && seq == 1 && p.writes().any(|w| w.sector == 0 && w.entry == e(1, state::VALID));
                    if !seeded {
                        return Err("boots from a sector with no accepted entry and no seed write".into());
                    }
                }
            }
        }
        Boot::Halt(h) => {
            if ctx.keep_valid || ctx.images[0] && dec.iter().all(|d| matches!(d, Decoded::Blank | Decoded::Corrupt)) {
                return Err(format!("halts ({h:?}) although a boot was possible"));
            }
        }
    }
    Ok(p)
}

/// `check` every state reachable by cutting inside `writes`; then, `depth` more times, do the same for the
/// bootloader's own writes on the next boot (a crash loop).
fn verify(before: &Flash, writes: &[Write], ctx: &Ctx, model: Model, depth: u8, what: &str) -> Result<usize, String> {
    let mut seen = HashSet::new();
    verify_level(before, writes, ctx, model, depth, true, &mut seen, what)?;
    Ok(seen.len())
}

#[allow(clippy::too_many_arguments)]
fn verify_level(
    before: &Flash,
    writes: &[Write],
    ctx: &Ctx,
    model: Model,
    depth: u8,
    top: bool,
    seen: &mut HashSet<(Flash, u8, usize)>,
    what: &str,
) -> Result<(), String> {
    for state in states_after_cuts(before, writes, model, top) {
        if !seen.insert((state, depth, ctx.allowed_valid.len())) {
            continue; // the same flash state is reached through many cut points
        }
        let p = check(&state, ctx).map_err(|err| format!("{what}: {err}\n  flash: {:?}", decoded(&state)))?;
        if depth > 0 {
            let next: Vec<Write> = p.writes().collect();
            let mut ctx2 = Ctx { allowed_valid: ctx.allowed_valid.clone(), keep_valid: ctx.keep_valid, images: ctx.images };
            for w in &next {
                if w.entry.state == state::VALID && !ctx2.allowed_valid.contains(&w.entry) {
                    ctx2.allowed_valid.push(w.entry);
                }
            }
            verify_level(&state, &next, &ctx2, model, depth - 1, false, seen, &format!("{what} -> next boot"))?;
        }
    }
    Ok(())
}

const REFERENCE: Model = Model::AnyOrder;

fn ctx(valid: &[Entry], keep_valid: bool) -> Ctx {
    Ctx { allowed_valid: valid.to_vec(), keep_valid, images: [true, true] }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "exhaustive: run with --release")]
fn power_cut_first_boot_seed() {
    let blank: Flash = [BLANK, BLANK];
    let seed: Vec<Write> = plan(&blank, [true, true]).writes().collect();
    for model in [REFERENCE, Model::Sequential] {
        verify(&blank, &seed, &ctx(&[e(1, state::VALID)], false), model, 2, "seed").unwrap();
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "exhaustive: run with --release")]
fn power_cut_full_update_cycle_keeps_a_validated_image_selectable() {
    for model in [REFERENCE, Model::Sequential] {
        let a = e(1, state::VALID);
        let mut f: Flash = [a.encode(), BLANK];

        // agent: activate slot 1
        let act = activate(f, 2, 1).unwrap();
        verify(&f, &[act], &ctx(&[a], true), model, 2, "activate").unwrap();
        f = full(f, &[act]);

        // bootloader: New -> Pending, boot slot 1
        let boot: Vec<Write> = plan(&f, [true, true]).writes().collect();
        verify(&f, &boot, &ctx(&[a], true), model, 2, "boot New").unwrap();
        f = full(f, &boot);

        // agent: self-check passed
        let b = e(2, state::VALID);
        let conf = confirm(f).unwrap();
        verify(&f, &[conf], &ctx(&[a, b], true), model, 2, "confirm").unwrap();
        f = full(f, &[conf]);

        // and back to slot 0: the write must spare the newest Valid entry (sector 1)
        let act2 = activate(f, 2, 0).unwrap();
        assert_eq!(act2.sector, 0);
        verify(&f, &[act2], &ctx(&[a, b], true), model, 2, "activate back").unwrap();
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "exhaustive: run with --release")]
fn power_cut_rollback_then_new_attempt() {
    for model in [REFERENCE, Model::Sequential] {
        let a = e(1, state::VALID);
        // new image booted (Pending) and never confirmed: reset
        let f: Flash = [a.encode(), raw(2, state::PENDING_VERIFY)];
        let rollback: Vec<Write> = plan(&f, [true, true]).writes().collect();
        verify(&f, &rollback, &ctx(&[a], true), model, 2, "rollback").unwrap();
        let f = full(f, &rollback);
        assert_eq!(plan(&f, [true, true]).boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });

        // the agent tries again: it must reuse the Aborted sector, never the Valid one
        let act = activate(f, 2, 1).unwrap();
        assert_eq!(act.sector, 1);
        verify(&f, &[act], &ctx(&[a], true), model, 2, "activate after rollback").unwrap();
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "exhaustive: run with --release")]
fn power_cut_reject_after_failed_self_check() {
    let a = e(1, state::VALID);
    let f: Flash = [a.encode(), raw(2, state::PENDING_VERIFY)];
    let rej = reject(f).unwrap();
    for model in [REFERENCE, Model::Sequential] {
        verify(&f, &[rej], &ctx(&[a], true), model, 2, "reject").unwrap();
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "exhaustive: run with --release")]
fn power_cut_invalid_candidate_is_dropped_safely() {
    let a = e(1, state::VALID);
    let f: Flash = [a.encode(), raw(2, state::NEW)];
    let mut c = ctx(&[a], true);
    c.images = [true, false];
    let boot: Vec<Write> = plan(&f, c.images).writes().collect();
    for model in [REFERENCE, Model::Sequential] {
        verify(&f, &boot, &c, model, 2, "bad candidate").unwrap();
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "exhaustive: run with --release")]
fn power_cut_second_cycle_after_a_confirmed_update() {
    // Both entries Valid (seq 1 in sector 0, seq 2 in sector 1): the next activation must spare sector 1.
    let (a, b) = (e(1, state::VALID), e(2, state::VALID));
    let f: Flash = [a.encode(), b.encode()];
    let act = activate(f, 2, 0).unwrap();
    assert_eq!((act.sector, act.entry), (0, e(3, state::NEW)));
    verify(&f, &[act], &ctx(&[a, b], true), REFERENCE, 2, "activate over an older Valid").unwrap();
}

// --- the simulator itself --------------------------------------------------

#[test]
#[cfg_attr(debug_assertions, ignore = "exhaustive: run with --release")]
fn the_reference_model_really_enumerates_torn_states() {
    let before: Flash = [raw(1, state::VALID), BLANK];
    let w = activate(before, 2, 1).unwrap();
    let states = states_after_cuts(&before, &[w], Model::AnyOrder, true);
    assert!(states.len() > 20_000, "only {} states", states.len());

    let target = e(2, state::NEW);
    let (mut commit_partial, mut commit_done, mut body_no_commit) = (0, 0, 0);
    for f in &states {
        match decode(&f[1]) {
            Decoded::Ok(entry) => {
                // The property the whole design rests on: whatever the cut left, an accepted entry is exact.
                assert!(entry == target || entry == e(1, state::VALID), "accepted a wrong entry: {entry:?}");
                commit_done += 1;
            }
            Decoded::Corrupt => {
                let r = f[1];
                if r[..OFF_COMMIT] == target.body()[..OFF_COMMIT] && r[24..] == target.body()[24..] && r[OFF_COMMIT..24] != [0xFF; 4] {
                    commit_partial += 1;
                }
                if r == target.body() {
                    body_no_commit += 1;
                }
            }
            Decoded::Blank => {}
        }
    }
    assert!(commit_done > 0 && commit_partial > 10 && body_no_commit > 0, "{commit_partial} {commit_done} {body_no_commit}");
}

#[test]
fn the_checker_catches_what_it_claims_to() {
    let c = ctx(&[e(1, state::VALID)], true);
    // A Valid entry nobody is allowed to have written.
    let forged: Flash = [raw(1, state::VALID), raw(2, state::VALID)];
    assert!(check(&forged, &c).unwrap_err().contains("forged Valid"));
    // The last Valid entry gone.
    let gone: Flash = [BLANK, raw(2, state::NEW)];
    assert!(check(&gone, &c).unwrap_err().contains("no Valid entry left"));
    // Halting although a validated image is intact.
    let mut only_images_bad = ctx(&[e(1, state::VALID)], true);
    only_images_bad.images = [false, false];
    assert!(check(&[raw(1, state::VALID), BLANK], &only_images_bad).is_err());
}

#[test]
#[cfg_attr(debug_assertions, ignore = "exhaustive: run with --release")]
fn a_torn_body_is_rejected_even_if_a_faulty_executor_commits_it_anyway() {
    // Defence in depth. The protocol commits only after the body is programmed (and an executor should read
    // it back first); if one commits blindly after a torn body, the entry must still not be accepted as
    // anything but the exact target -- that is what `ext_crc` is for.
    let before: Flash = [raw(1, state::VALID), BLANK];
    let w = activate(before, 2, 1).unwrap();
    let target = w.entry;
    let [_, _, commit] = w.ops();
    let mut checked = 0;
    for mut f in states_after_cuts(&before, &[w], Model::AnyOrder, true) {
        if f[1][OFF_COMMIT..OFF_COMMIT + 4] != [0xFF; 4] {
            continue; // only states from before the commit
        }
        apply_op(&mut f, commit);
        match decode(&f[1]) {
            Decoded::Corrupt => {}
            Decoded::Ok(entry) => assert_eq!(entry, target, "a torn body became a different valid entry"),
            Decoded::Blank => panic!("commit cannot leave a blank entry"),
        }
        checked += 1;
    }
    assert!(checked > 10_000, "only {checked} torn bodies were exercised");
}

#[test]
fn the_entry_format_is_frozen_by_golden_vectors() {
    // Cross-checked against scripts/ewbt-otadata.py (an independent implementation): if these change,
    // every device already flashed loses its otadata -- bump FORMAT_VERSION instead.
    let hex = |raw: Raw| raw.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(hex(e(1, state::VALID).encode()), "010000004557425401000000484e1c0dffffffff3ca5c35a020000009a984347");
    assert_eq!(hex(e(2, state::NEW).encode()), "0200000045574254010000002564efc8ffffffff3ca5c35a000000007437f655");
}
