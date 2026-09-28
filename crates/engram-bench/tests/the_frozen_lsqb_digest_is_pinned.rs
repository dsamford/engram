//! `catalogue/statements.json` is frozen, and this is the freeze.
//!
//! The digest below was computed on 2026-09-28 over 72599 bytes of
//! `crates/engram-bench/catalogue/statements.json`. It is FNV-1a (64-bit) over
//! the WHOLE file, so any edit at all moves it — a key, a reflow, a trailing
//! newline.
//!
//! It moved once, deliberately: on 2026-09-28 five prose notes that named
//! benchmark pods and their script paths were reworded for publication, and no
//! statement changed. The pair (the 2026-09-10 digest, this one) is declared in
//! `catalogue::EQUIVALENT_DIGESTS`, so runs recorded against the old bytes still
//! compare with runs recorded against these.
//!
//! This test exists because the file is the most load-bearing data in the
//! project and the consequence of changing it is silent. Nothing at runtime
//! fails when the bytes move; comparisons simply stop being built, later, in a
//! different session, for a reason the reader has to reconstruct.

use engram_bench::catalogue;

/// FNV-1a over `catalogue/statements.json` as of 2026-09-28, 72599 bytes.
const PINNED: u64 = 0x2cf6_26ba_1f17_8f53;

/// The 2026-09-10 digest, 72534 bytes: the same statements, before the notes
/// were reworded.
const BEFORE_THE_NOTES: u64 = 0x6d90_c151_3a6a_cd0e;

#[test]
fn the_frozen_lsqb_digest_is_pinned() {
    assert_eq!(
        catalogue::digest(),
        PINNED,
        "\n\
         crates/engram-bench/catalogue/statements.json HAS CHANGED.\n\
         \n\
         Its FNV-1a digest is now {0:#018x}; it was {1:#018x}, computed on\n\
         2026-09-28 over 72599 bytes.\n\
         \n\
         What that means, in full:\n\
         \n\
         * Every LSQB and stress number this project has recorded was measured\n\
           against the OLD bytes. Those numbers did not change and are not\n\
           wrong — but they now describe a catalogue that no longer exists in\n\
           the tree.\n\
         * `report::compare` refuses to build a comparison row out of two runs\n\
           whose catalogue digests differ, because such a row compares two\n\
           catalogues and not two engines. From this edit onward it will refuse\n\
           every pairing of an old run with a new one. The refusal is correct;\n\
           the edit is what has to be reconsidered.\n\
         * The refusal is also quiet in the way that matters: nothing fails at\n\
           measurement time. The run succeeds, the document is written, and the\n\
           comparison is simply never produced.\n\
         \n\
         If you are ADDING a benchmark family, it does not belong in this file.\n\
         A family lives in its own catalogue file with its own digest — see\n\
         `catalogue::Family` and `catalogue::FAMILIES` — precisely so that\n\
         adding one cannot invalidate an earlier run's comparability. Add\n\
         `catalogue/<family>.json`, a `Family` constant for it, and a\n\
         `family_for_workload` arm; leave these bytes alone.\n\
         \n\
         If you are DELIBERATELY re-cutting the LSQB/stress catalogue, then the\n\
         old numbers are being retired on purpose. Say so where the decision\n\
         lives, update PINNED here together with the byte count and the date,\n\
         and expect to re-take every baseline you still want to quote.",
        catalogue::digest(),
        PINNED
    );
}

#[test]
fn the_frozen_family_is_the_frozen_file() {
    // The `lsqb-stress` family must BE `statements.json`, not a second copy of
    // it: a per-family digest that could drift from the whole-file one would
    // let the pin above pass while the family a comparison is judged on had
    // moved.
    assert_eq!(catalogue::LSQB_STRESS.digest(), catalogue::digest());
    assert_eq!(catalogue::LSQB_STRESS.digest(), PINNED);
}

#[test]
fn the_reworded_notes_are_one_declared_pair_and_nothing_else() {
    // The pair is exact: the old bytes and the pinned bytes name the same
    // statements, in either order, and nothing else is waved through.
    let hex = |d: u64| format!("{d:016x}");
    assert!(catalogue::EQUIVALENT_DIGESTS.contains(&(BEFORE_THE_NOTES, PINNED)));
    assert_eq!(
        catalogue::EQUIVALENT_DIGESTS.len(),
        1,
        "a second pair needs its own reason here"
    );
    assert!(catalogue::same_statements(&hex(BEFORE_THE_NOTES), &hex(PINNED)));
    assert!(catalogue::same_statements(&hex(PINNED), &hex(BEFORE_THE_NOTES)));
    assert!(catalogue::same_statements(&hex(PINNED), &hex(PINNED)));
    // A third digest matches neither side, and garbage is not a digest.
    assert!(!catalogue::same_statements(&hex(BEFORE_THE_NOTES), &hex(0x1234)));
    assert!(!catalogue::same_statements(&hex(PINNED), &hex(PINNED ^ 1)));
    assert!(!catalogue::same_statements("not-a-digest", &hex(PINNED)));
}
