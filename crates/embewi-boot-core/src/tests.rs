use super::*;
use std::collections::HashSet;

// --- helpers ---------------------------------------------------------------

type Flash = [[u8; ENTRY_SIZE]; SECTOR_COUNT];

fn flash(e0: Entry, e1: Entry) -> Flash {
    [e0.encode(), e1.encode()]
}

fn entries(f: &Flash) -> [Entry; 2] {
    [Entry::decode(&f[0]), Entry::decode(&f[1])]
}

fn apply(f: &mut Flash, w: Write) {
    f[w.sector as usize] = w.entry.encode();
}

fn boots(images: [bool; 2]) -> impl FnMut(u8) -> bool {
    move |slot| images[slot as usize]
}

fn plan(f: &Flash, images: [bool; 2]) -> Plan {
    plan_boot(entries(f), 2, &mut boots(images))
}

fn e(seq: u32, st: u32) -> Entry {
    Entry::new(seq, st)
}

// --- codec -----------------------------------------------------------------

#[test]
fn crc_matches_the_rom_and_esp_bootloader_esp_idf_test_vector() {
    // esp-bootloader-esp-idf's SLOT_COUNT_1_VALID: seq=1, state=Valid, crc bytes 154,152,67,71.
    assert_eq!(crc32_le(u32::MAX, &1u32.to_le_bytes()), 0x4743_989A);
    let raw: [u8; 32] = [
        1, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 2, 0, 0, 0, 154, 152, 67, 71,
    ];
    let entry = Entry::decode(&raw);
    assert_eq!(entry, Entry::new(1, state::VALID));
    assert_eq!(entry.encode(), raw);
}

#[test]
fn erased_entry_is_blank_and_torn_entries_are_corrupt() {
    assert_eq!(Entry::decode(&[0xFF; 32]).classify(), Class::Blank);
    let mut torn = Entry::new(3, state::NEW);
    torn.crc = u32::MAX; // crc is programmed last
    assert_eq!(torn.classify(), Class::Corrupt);
    assert_eq!(Entry { seq: 0, ..Entry::new(1, state::VALID) }.classify(), Class::Corrupt);
}

#[test]
fn slot_mapping_follows_esp_idf() {
    assert_eq!([slot_of(1, 2), slot_of(2, 2), slot_of(3, 2), slot_of(4, 2)], [0, 1, 0, 1]);
    assert_eq!([slot_of(1, 3), slot_of(2, 3), slot_of(3, 3), slot_of(4, 3)], [0, 1, 2, 0]);
}

// --- plan_boot -------------------------------------------------------------

#[test]
fn blank_otadata_is_a_normal_first_boot_that_seeds_slot_0() {
    let p = plan(&flash(Entry::BLANK, Entry::BLANK), [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 0, entry: e(1, state::VALID) }]);
}

#[test]
fn first_boot_halts_when_slot_0_is_not_bootable() {
    let p = plan(&flash(Entry::BLANK, Entry::BLANK), [false, true]);
    assert_eq!(p.boot, Boot::Halt(Halt::NoImage));
    assert_eq!(p.writes().count(), 0);
}

#[test]
fn a_torn_seed_is_retried_like_a_first_boot() {
    let mut torn = e(1, state::VALID);
    torn.crc = u32::MAX;
    let p = plan(&flash(torn, Entry::BLANK), [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
}

#[test]
fn a_valid_entry_boots_without_any_write() {
    let p = plan(&flash(e(1, state::VALID), Entry::BLANK), [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().count(), 0);
}

#[test]
fn a_new_entry_is_marked_pending_before_it_boots() {
    let p = plan(&flash(e(1, state::VALID), e(2, state::NEW)), [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 1, sector: 1, seq: 2 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 1, entry: e(2, state::PENDING_VERIFY) }]);
}

#[test]
fn an_erased_state_counts_as_unproven_not_as_valid() {
    let mut half = e(2, state::NEW);
    half.state = state::UNDEFINED; // sequence written, state not yet
    let p = plan(&flash(e(1, state::VALID), half), [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 1, sector: 1, seq: 2 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 1, entry: e(2, state::PENDING_VERIFY) }]);
}

#[test]
fn a_pending_entry_that_rebooted_is_aborted_and_the_previous_slot_boots() {
    let p = plan(&flash(e(1, state::VALID), e(2, state::PENDING_VERIFY)), [true, true]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 1, entry: e(2, state::ABORTED) }]);
}

#[test]
fn an_unbootable_candidate_is_marked_invalid_and_the_next_one_boots() {
    let p = plan(&flash(e(1, state::VALID), e(2, state::NEW)), [true, false]);
    assert_eq!(p.boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });
    assert_eq!(p.writes().collect::<Vec<_>>(), [Write { sector: 1, entry: e(2, state::INVALID) }]);
}

#[test]
fn nothing_usable_halts_explicitly_and_never_reseeds() {
    let p = plan(&flash(e(1, state::INVALID), e(2, state::ABORTED)), [true, true]);
    assert_eq!(p.boot, Boot::Halt(Halt::NoUsableEntry));
    let p = plan(&flash(e(1, state::VALID), e(2, state::NEW)), [false, false]);
    assert_eq!(p.boot, Boot::Halt(Halt::NoUsableEntry));
}

// --- activate / confirm / reject -------------------------------------------

#[test]
fn activate_refuses_without_a_known_good_image() {
    assert_eq!(activate([Entry::BLANK; 2], 2, 1), Err(ActivateError::NoValidBase));
    assert_eq!(activate([e(1, state::NEW), Entry::BLANK], 2, 1), Err(ActivateError::NoValidBase));
}

#[test]
fn activate_writes_one_complete_new_entry_beside_the_valid_one() {
    let w = activate([e(1, state::VALID), Entry::BLANK], 2, 1).unwrap();
    assert_eq!(w, Write { sector: 1, entry: e(2, state::NEW) });
    let w = activate([Entry::BLANK, e(1, state::VALID)], 2, 1);
    assert_eq!(w, Err(ActivateError::NoValidBase).or(w)); // seq 1 in sector 1 is still a Valid base
    assert_eq!(w.unwrap().sector, 0);
}

#[test]
fn activate_after_a_rollback_overwrites_the_dead_entry_not_the_valid_one() {
    // Raw sequence comparison would pick sector 0 (the other one) and erase the only good entry.
    let after_rollback = [e(1, state::VALID), e(2, state::ABORTED)];
    let w = activate(after_rollback, 2, 1).unwrap();
    assert_eq!(w.sector, 1, "the Aborted sector is the one to reuse");
    assert_eq!(w.entry, e(4, state::NEW)); // smallest seq above 2 that selects slot 1 (3 selects slot 0)
}

#[test]
fn activate_alternates_slots_across_cycles() {
    let mut f = flash(e(1, state::VALID), Entry::BLANK);
    for (target, expected_seq) in [(1u8, 2u32), (0, 3), (1, 4), (0, 5)] {
        let w = activate(entries(&f), 2, target).unwrap();
        assert_eq!(w.entry.seq, expected_seq);
        apply(&mut f, w);
        let boot = plan(&f, [true, true]);
        for w in boot.writes() {
            apply(&mut f, w);
        }
        let confirmed = confirm(entries(&f)).unwrap();
        apply(&mut f, confirmed);
    }
}

#[test]
fn confirm_and_reject_act_on_the_pending_entry_only() {
    let f = [e(1, state::VALID), e(2, state::PENDING_VERIFY)];
    assert_eq!(confirm(f), Some(Write { sector: 1, entry: e(2, state::VALID) }));
    assert_eq!(reject(f), Some(Write { sector: 1, entry: e(2, state::INVALID) }));
    // Still `New`: the bootloader did not mark it -- a chain anomaly, reported by None.
    assert_eq!(confirm([e(1, state::VALID), e(2, state::NEW)]), None);
    assert_eq!(confirm([e(1, state::VALID), Entry::BLANK]), None);
}

// --- power-cut simulation --------------------------------------------------
//
// A `Write` is "erase the sector, then program 32 bytes". A cut can land before
// the erase, during it, between erase and program, or anywhere in the program.
// Two models of what a cut leaves behind:
//
// * `Sequential` -- how NOR flash behaves: bytes are programmed in address
//   order, so a cut leaves a prefix complete, the next byte with any subset of
//   its bits cleared, the rest still erased. The crc is the last field, so a
//   torn entry never has a valid crc.
// * `AnyOrder` -- adversarial: any subset of bytes complete, every byte with any
//   subset of bits cleared. Documents what the format does NOT guard against.

#[derive(Clone, Copy, PartialEq, Debug)]
enum Model {
    Sequential,
    AnyOrder,
}

/// Every flash state reachable by cutting power somewhere inside `writes`
/// (applied in order), plus the fully-applied one.
fn states_after_cuts(before: &Flash, writes: &[Write], model: Model, thorough: bool) -> Vec<Flash> {
    let mut out: HashSet<Flash> = HashSet::new();
    let mut cur = *before;
    for w in writes {
        let s = w.sector as usize;
        let target = w.entry.encode();
        let mut erased = cur;
        erased[s] = [0xFF; ENTRY_SIZE];

        out.insert(cur); // before the erase
        for prefix in [1usize, 4, 8, 16, 24, 31] {
            let mut f = cur;
            f[s][..prefix].fill(0xFF); // erase in progress
            out.insert(f);
        }
        for pattern in [0x00u8, 0xA5, 0x5A] {
            let mut f = cur;
            f[s] = [pattern; ENTRY_SIZE]; // erase left garbage
            out.insert(f);
        }
        out.insert(erased); // erased, nothing programmed

        match model {
            Model::Sequential => {
                for k in 0..ENTRY_SIZE {
                    for partial in partial_bytes(target[k], thorough) {
                        let mut f = erased;
                        f[s][..k].copy_from_slice(&target[..k]);
                        f[s][k] = partial;
                        out.insert(f);
                    }
                }
            }
            Model::AnyOrder => {
                // Fields programmed in any order, the state word byte by byte with any reachable value.
                // Structured, not random: forging needs sequence AND crc complete plus one precise
                // partial state word, far too rare for random sampling.
                let candidates = |t: u8| -> Vec<u8> {
                    [0xFF, t, 0x00, 0x01, 0x02, 0x03, 0x04]
                        .into_iter()
                        .filter(|v| v & t == t) // reachable: only bits `t` clears may be cleared
                        .collect()
                };
                let (c0, c1, c2, c3) =
                    (candidates(target[24]), candidates(target[25]), candidates(target[26]), candidates(target[27]));
                for seq_done in [false, true] {
                    for crc_done in [false, true] {
                        for &b0 in &c0 {
                            for &b1 in &c1 {
                                for &b2 in &c2 {
                                    for &b3 in &c3 {
                                        let mut f = erased;
                                        if seq_done {
                                            f[s][0..4].copy_from_slice(&target[0..4]);
                                        }
                                        f[s][24..28].copy_from_slice(&[b0, b1, b2, b3]);
                                        if crc_done {
                                            f[s][28..32].copy_from_slice(&target[28..32]);
                                        }
                                        out.insert(f);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        cur[s] = target;
        out.insert(cur); // complete
    }
    out.into_iter().collect()
}

/// A byte mid-programming: erased (0xFF) with a subset of the bits `target` clears already cleared.
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

struct Ctx {
    /// Entries that legitimately were, or are being made, `Valid`.
    allowed_valid: Vec<Entry>,
    /// Whether a boot must be possible (a previously validated image is intact, or a first boot with a good slot 0).
    expect_bootable: bool,
    images: [bool; 2],
}

/// The safety properties, on one flash state. Returns the plan or what broke.
fn check(f: &Flash, ctx: &Ctx) -> Result<Plan, String> {
    let ents = entries(f);
    for (i, ent) in ents.iter().enumerate() {
        if let Class::Ok { trust: Trust::Valid, .. } = ent.classify() {
            if !ctx.allowed_valid.contains(ent) {
                return Err(format!("forged Valid entry in sector {i}: {ent:?}"));
            }
        }
    }
    let p = plan_boot(ents, 2, &mut boots(ctx.images));
    match p.boot {
        Boot::Slot { slot, sector, seq } => {
            if !ctx.images[slot as usize] {
                return Err(format!("boots slot {slot} whose image is not bootable"));
            }
            let Class::Ok { trust, seq: entry_seq } = ents[sector as usize].classify() else {
                // The first-boot seed: no entry yet, the plan writes it.
                return if p.writes().any(|w| w.sector == sector && w.entry == Entry::new(seq, state::VALID)) {
                    Ok(p)
                } else {
                    Err("boots from a sector with no usable entry and no seed write".into())
                };
            };
            match trust {
                Trust::Valid => {}
                Trust::Unproven => {
                    let marked = p.writes().any(|w| {
                        w.sector == sector && w.entry == Entry::new(entry_seq, state::PENDING_VERIFY)
                    });
                    if !marked {
                        return Err(format!("boots unproven entry {entry_seq} without marking it Pending"));
                    }
                }
                other => return Err(format!("boots an entry whose trust is {other:?}")),
            }
        }
        Boot::Halt(h) => {
            if ctx.expect_bootable {
                return Err(format!("halts ({h:?}) although a validated image is intact"));
            }
        }
    }
    Ok(p)
}

/// `check` every state reachable by cutting inside `writes`; then, `depth` more
/// times, do the same for the bootloader's own writes on the next boot (a crash loop).
fn verify(before: &Flash, writes: &[Write], ctx: &Ctx, model: Model, depth: u8, what: &str) -> Result<(), String> {
    verify_level(before, writes, ctx, model, depth, true, &mut HashSet::new(), what)
}

/// `top`: the first cut is enumerated exhaustively (every partial byte); the
/// crash-loop levels below it use a reduced set, or the search explodes.
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
        // The same flash state is reached through many cut points: check it once per depth.
        if !seen.insert((state, depth, ctx.allowed_valid.len())) {
            continue;
        }
        let p = check(&state, ctx).map_err(|e| format!("{what}: {e}\n  flash: {:?}", entries(&state)))?;
        if depth > 0 {
            let next: Vec<Write> = p.writes().collect();
            let mut ctx2 =
                Ctx { allowed_valid: ctx.allowed_valid.clone(), expect_bootable: ctx.expect_bootable, images: ctx.images };
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

fn full(mut f: Flash, writes: &[Write]) -> Flash {
    for w in writes {
        apply(&mut f, *w);
    }
    f
}

#[test]
fn power_cut_first_boot_seed() {
    let blank = flash(Entry::BLANK, Entry::BLANK);
    let ctx = Ctx { allowed_valid: vec![e(1, state::VALID)], expect_bootable: true, images: [true, true] };
    let seed: Vec<Write> = plan(&blank, ctx.images).writes().collect();
    verify(&blank, &seed, &ctx, Model::Sequential, 2, "seed").unwrap();
}

#[test]
fn power_cut_full_update_cycle_keeps_a_validated_image_selectable() {
    let ctx = |valid: &[Entry]| Ctx { allowed_valid: valid.to_vec(), expect_bootable: true, images: [true, true] };
    let a = e(1, state::VALID);
    let mut f = flash(a, Entry::BLANK);

    // agent: activate slot 1
    let act = activate(entries(&f), 2, 1).unwrap();
    verify(&f, &[act], &ctx(&[a]), Model::Sequential, 2, "activate").unwrap();
    f = full(f, &[act]);

    // bootloader: New -> Pending, boot slot 1
    let boot: Vec<Write> = plan(&f, [true, true]).writes().collect();
    verify(&f, &boot, &ctx(&[a]), Model::Sequential, 2, "boot New").unwrap();
    f = full(f, &boot);

    // agent: self-check passed
    let b = e(2, state::VALID);
    let conf = confirm(entries(&f)).unwrap();
    verify(&f, &[conf], &ctx(&[a, b]), Model::Sequential, 2, "confirm").unwrap();
    f = full(f, &[conf]);

    // and back to slot 0: the write must spare the newest Valid entry (sector 1)
    let act2 = activate(entries(&f), 2, 0).unwrap();
    assert_eq!(act2.sector, 0);
    verify(&f, &[act2], &ctx(&[a, b]), Model::Sequential, 2, "activate back").unwrap();
}

#[test]
fn power_cut_rollback_then_new_attempt() {
    let ctx = |valid: &[Entry]| Ctx { allowed_valid: valid.to_vec(), expect_bootable: true, images: [true, true] };
    let a = e(1, state::VALID);
    // new image booted (Pending) and never confirmed: reset
    let f = flash(a, e(2, state::PENDING_VERIFY));
    let rollback: Vec<Write> = plan(&f, [true, true]).writes().collect();
    verify(&f, &rollback, &ctx(&[a]), Model::Sequential, 2, "rollback").unwrap();
    let f = full(f, &rollback);
    assert_eq!(plan(&f, [true, true]).boot, Boot::Slot { slot: 0, sector: 0, seq: 1 });

    // the agent tries again: it must reuse the Aborted sector, never the Valid one
    let act = activate(entries(&f), 2, 1).unwrap();
    assert_eq!(act.sector, 1);
    verify(&f, &[act], &ctx(&[a]), Model::Sequential, 2, "activate after rollback").unwrap();
}

#[test]
fn power_cut_reject_after_failed_self_check() {
    let a = e(1, state::VALID);
    let f = flash(a, e(2, state::PENDING_VERIFY));
    let ctx = Ctx { allowed_valid: vec![a], expect_bootable: true, images: [true, true] };
    let rej = reject(entries(&f)).unwrap();
    verify(&f, &[rej], &ctx, Model::Sequential, 2, "reject").unwrap();
}

#[test]
fn power_cut_invalid_candidate_is_dropped_safely() {
    let a = e(1, state::VALID);
    let f = flash(a, e(2, state::NEW));
    let ctx = Ctx { allowed_valid: vec![a], expect_bootable: true, images: [true, false] };
    let boot: Vec<Write> = plan(&f, ctx.images).writes().collect();
    verify(&f, &boot, &ctx, Model::Sequential, 2, "bad candidate").unwrap();
}

#[test]
fn naive_two_step_activation_can_boot_an_unverified_image() {
    // esp-bootloader-esp-idf: set_current_app_partition() reads the target sector, sets sequence
    // and crc, and writes it back -- keeping the sector's previous state. Sector 1 last held a
    // Valid entry (seq 2, from an earlier cycle); the agent activates seq 4 the naive way and
    // power is cut before the second write (state = New) happens.
    let stale_valid_sector = Entry { seq: 4, crc: crc32_le(u32::MAX, &4u32.to_le_bytes()), ..e(2, state::VALID) };
    let f = flash(e(3, state::VALID), stale_valid_sector);
    let p = plan(&f, [true, true]);
    // Slot 0 (seq 3) is Valid and slot_of(4) = 1, seq 4 wins: booted as trusted, no Pending mark.
    assert_eq!(p.boot, Boot::Slot { slot: 1, sector: 1, seq: 4 });
    assert_eq!(p.writes().count(), 0, "no Pending marker: the image runs unverified and can't be rolled back");
    // Hence: the agent must use `activate` (one write, state New). Nothing in the bootloader can tell.
}

#[test]
fn adversarial_programming_order_can_forge_a_valid_entry() {
    // Not a claim about real flash (see the Sequential model): the crc covers only `ota_seq`, so if a
    // cut could leave sequence and crc programmed but the state word half-programmed, `New` (0) could
    // read as `Valid` (2). Recorded so the limit of the otadata format is on the record.
    let a = e(1, state::VALID);
    let f = flash(a, Entry::BLANK);
    let ctx = Ctx { allowed_valid: vec![a], expect_bootable: true, images: [true, true] };
    let act = activate(entries(&f), 2, 1).unwrap();
    let result = verify(&f, &[act], &ctx, Model::AnyOrder, 0, "activate (any programming order)");
    let err = result.expect_err("expected the any-order model to find a forged Valid entry");
    assert!(err.contains("forged Valid"), "unexpected failure: {err}");
}

// --- sanity of the simulator itself -----------------------------------------

#[test]
fn the_simulation_really_enumerates_torn_states() {
    let before = flash(e(1, state::VALID), Entry::BLANK);
    let w = activate(entries(&before), 2, 1).unwrap();

    let sequential = states_after_cuts(&before, &[w], Model::Sequential, true);
    assert!(sequential.len() > 1000, "only {} states", sequential.len());
    let classes: Vec<Class> = sequential.iter().map(|f| Entry::decode(&f[1]).classify()).collect();
    assert!(classes.contains(&Class::Corrupt), "no torn (crc-invalid) entry was produced");
    assert!(classes.contains(&Class::Blank), "the erased state is missing");
    assert!(classes.iter().any(|c| matches!(c, Class::Ok { trust: Trust::Unproven, .. })), "the completed write is missing");
    // Sequential programming never leaves a valid crc on an incomplete entry.
    let complete = Entry::new(2, state::NEW);
    for f in &sequential {
        let ent = Entry::decode(&f[1]);
        if matches!(ent.classify(), Class::Ok { .. }) {
            assert_eq!(ent, complete, "a torn write produced a valid-looking entry: {ent:?}");
        }
    }

    let any_order = states_after_cuts(&before, &[w], Model::AnyOrder, true);
    assert!(any_order.len() > sequential.len() / 4);
}

#[test]
fn the_checker_catches_what_it_claims_to() {
    // Unproven entry booted without a Pending marker -> reported.
    let ctx = Ctx { allowed_valid: vec![e(1, state::VALID)], expect_bootable: true, images: [true, true] };
    let forged = flash(e(1, state::VALID), e(2, state::VALID)); // Valid entry nobody is allowed to have written
    assert!(check(&forged, &ctx).unwrap_err().contains("forged Valid"));
    // Halting although a validated image is intact -> reported.
    let all_dead = flash(e(1, state::INVALID), e(2, state::ABORTED));
    assert!(check(&all_dead, &ctx).unwrap_err().contains("halts"));
}
