//! The row budget derives itself from the container, and this is what holds
//! the derivation honest.
//!
//! It used to be the constant 20,000,000, chosen against "a 2.7M-row corpus".
//! At SF10 that constant refused a LEGITIMATE query — LSQB q7, answer
//! 331,627,527 — on a 160 GiB node where nothing was near exhaustion. The
//! guard had stopped protecting the machine and started protecting its own
//! calibration.

use engram_server::derive_row_budget;

const GIB: u64 = 1024 * 1024 * 1024;

/// THE CALIBRATION CHECK, and the reason to believe the formula at all.
///
/// A derivation that did not land near the old constant on the hardware the old
/// constant was chosen for would be a different guard wearing its name. The
/// original bench pods carried 8-16 GiB and the constant was 20M, so the
/// derivation there has to be the same SIZE of number — within a factor of two.
///
/// A factor of two rather than a bracket, deliberately. This test first asserted
/// `derive(8 GiB) <= 20M <= derive(16 GiB)`, which held while the assumed row
/// width was a reasoned 128 B. Measuring a real statement moved it to 96 B (LSQB
/// q7 at SF10: 24.3 GB for 331,627,527 rows = 73 B/row), and 8 GiB now derives
/// 22.4M — just ABOVE 20M instead of just below. The bracket was an artifact of
/// the un-measured constant; the claim it was standing in for is that the
/// formula generalises the old number, and that is what is asserted here.
#[test]
fn the_derivation_reproduces_the_constant_it_replaces() {
    const OLD_CONSTANT: u64 = 20_000_000;
    for gib in [8u64, 16] {
        let derived = derive_row_budget(gib * GIB);
        assert!(
            (OLD_CONSTANT / 2..=OLD_CONSTANT * 4).contains(&derived),
            "{gib} GiB derived {derived}, not within a factor of the {OLD_CONSTANT} \
             this formula claims to generalise — if that is intended, the claim in \
             `auto_row_budget`'s doc comment is the thing to change, not this bound"
        );
    }
    // And the direction is right: the machines the constant was chosen for get
    // roughly the constant, while the machine that exposed its failure gets
    // enough to answer the query it refused.
    assert!(derive_row_budget(8 * GIB) < derive_row_budget(160 * GIB) / 10);
}

/// The SF10 refusal that started this: q7 materialises past 20M on a 160 GiB
/// pod. The derived budget has to admit it, or nothing has changed.
#[test]
fn a_160_gib_pod_admits_what_a_20m_constant_refused() {
    let budget = derive_row_budget(160 * GIB);
    assert!(
        budget > 20_000_000,
        "the whole point is that a large container stops refusing at a small \
         machine's row count (got {budget})"
    );
    // ...and it is still a GUARD. A quarter of 160 GiB at 128 B per row.
    assert!(
        budget < 1_000_000_000,
        "a budget this large would no longer refuse a runaway product (got {budget})"
    );
}

/// Monotone in the ceiling: more memory never derives a smaller budget.
#[test]
fn more_memory_never_means_a_smaller_budget() {
    let mut prev = 0;
    for gib in [1u64, 2, 4, 8, 16, 40, 64, 160, 512] {
        let b = derive_row_budget(gib * GIB);
        assert!(b >= prev, "{gib} GiB derived {b}, less than the step below ({prev})");
        prev = b;
    }
}

/// Clamped at both ends, and NEVER zero.
///
/// Zero is the engine's "unlimited" when it reaches `set_row_budget(None)`, so
/// a derivation that bottomed out at 0 would silently turn the guard OFF on
/// exactly the smallest machines — the ones that need it most.
#[test]
fn the_clamps_hold_and_zero_is_never_derived() {
    assert_eq!(derive_row_budget(0), 1_000_000, "a zero ceiling still guards");
    assert_eq!(derive_row_budget(1), 1_000_000, "and so does an absurd one");
    assert_ne!(derive_row_budget(0), 0, "0 would read as UNLIMITED downstream");
    assert_eq!(
        derive_row_budget(u64::MAX),
        4_000_000_000,
        "an unbounded ceiling must not derive an unbounded budget"
    );
}

/// The real machine derives something sane, whatever this machine is.
#[test]
fn this_machine_derives_a_usable_budget() {
    let (budget, why) = engram_server::auto_row_budget();
    assert!(budget >= 1_000_000, "derived {budget} on this host: {why}");
    assert!(
        why.contains("per row"),
        "the explanation must state the arithmetic, because a refusal hours \
         later is only traceable if the log said how the number was reached: {why}"
    );
}

// ── The DECISION, not just the arithmetic ───────────────────────────────────
//
// Everything above tests `derive_row_budget`, a pure function. None of it
// tested the branch that decides whether that number is used at all — which
// lived in `main.rs`, where no test reaches, because nothing in this suite
// spawns the binary.
//
// The cost of that gap, measured: an SF10 server booted printing
//   row budget: 447392426 rows = 163840 MiB ceiling (cgroup-v2:memory.max)/4/96 B
// and then refused LSQB q7 with "its share of 1000000 across 1 statement(s)
// in flight". The refusal prints the graph's OWN budget, so the graph held
// 1,000,000 — the clamp floor — while the boot line advertised 447 million.
// Three LSQB passes recorded q7 as a failure on that basis.

#[test]
fn nothing_named_derives_from_the_container() {
    let (budget, why) = engram_server::resolve_row_budget(None);
    let derived = budget.expect("deriving must produce a budget, never unlimited");
    assert!(
        derived >= 1_000_000,
        "derived {derived} ({why}) is below the floor"
    );
    assert_eq!(
        derived,
        engram_server::auto_row_budget().0,
        "the resolved budget must BE the derived one; announcing one number \
         and installing another is the defect this test exists for"
    );
}

#[test]
fn an_explicit_value_wins_including_a_large_one() {
    let (budget, why) = engram_server::resolve_row_budget(Some(400_000_000));
    assert_eq!(
        budget,
        Some(400_000_000),
        "an explicit budget is pinned as given ({why}) — a reproducible run \
         needs two machines to refuse at the same row"
    );
}

#[test]
fn explicit_zero_is_unlimited_not_a_budget_of_zero() {
    let (budget, why) = engram_server::resolve_row_budget(Some(0));
    assert_eq!(
        budget, None,
        "0 spells UNLIMITED so the flag stays one type; a literal Some(0) \
         would refuse every statement ({why})"
    );
}

#[test]
fn the_announced_number_is_the_installed_number() {
    // The boot line is the only record an operator has of which budget was in
    // force. If the string and the value can disagree, a refusal three hours
    // later is untraceable — which is exactly how the SF10 q7 failure read.
    for explicit in [None, Some(7_654_321), Some(0)] {
        let (budget, why) = engram_server::resolve_row_budget(explicit);
        match budget {
            None => assert!(
                why.contains("unlimited"),
                "an unlimited budget must SAY unlimited, said {why:?}"
            ),
            Some(b) => assert!(
                why.contains(&b.to_string()),
                "the announcement {why:?} does not contain the installed \
                 budget {b}"
            ),
        }
    }
}
