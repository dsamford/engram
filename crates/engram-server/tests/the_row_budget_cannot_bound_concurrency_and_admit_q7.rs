#![allow(non_snake_case)]
//! The auto row budget is a PER-STATEMENT share of the container ceiling, and
//! that single constant is asked to satisfy two requirements that cannot both
//! hold. This test pins the conflict so it is a fact rather than an argument.
//!
//! REQUIREMENT A — admit the widest real statement. LSQB q7 at SF10 needs
//! 447,392,426 rows (live-verified 2026-09-11, the run that answered it with no
//! flag). At the 160Gi bench pod that is EXACTLY what the derivation yields:
//! 171,798,691,840 / 4 / 96 = 447,392,426. q7 is admitted with zero margin —
//! it consumes the whole container-derived budget by itself.
//!
//! REQUIREMENT B — do not over-commit the container. The budget is handed to
//! EVERY concurrently executing statement. `--max-connections` defaults to 512
//! and the bench runs used `--workers 6`, so several statements hold a budget
//! at once. N concurrent statements are entitled to N x (ceiling / 4).
//!
//! These are mutually exclusive under one constant: A pins the divisor at 4, and
//! B is violated for any N > 4. This is not theoretical — it OOM-killed
//! the engram benchmark pod (40Gi) on 2026-09-11 during the SF10 validation-share
//! measurement, where single statements grew RSS by 5,484 MB and the SF3 arm
//! showed 641 statements growing RSS, the largest by 2,942 MB.
//!
//! The resolution is NOT a bigger divisor — that refuses q7 again, and a
//! shrinking default is a breaking change. It is a SHARED admission pool: rows
//! in flight bounded ACROSS statements, so one statement alone may use all of
//! it while N together still cannot exceed the container.

use engram_server::derive_row_budget;

const GIB: u64 = 1024 * 1024 * 1024;
/// Rows LSQB q7 at SF10 actually materialised, live-verified.
const Q7_ROWS_AT_SF10: u64 = 447_392_426;
/// The bench pod's ceiling.
const FULLNODE_CEILING: u64 = 160 * GIB;

#[test]
fn requirement_A_the_budget_admits_q7_with_no_margin_at_all() {
    let budget = derive_row_budget(FULLNODE_CEILING);
    assert_eq!(
        budget, Q7_ROWS_AT_SF10,
        "the 160Gi derivation must land exactly on q7's measured requirement; \
         if this moved, either the constants changed or q7's number did"
    );
}

#[test]
fn requirement_B_the_same_budget_over_commits_the_container_under_concurrency() {
    let budget = derive_row_budget(FULLNODE_CEILING);
    // Bytes the budget entitles ONE statement to, on the derivation's own
    // 96 B/row assumption.
    let per_statement = budget * 96;

    // The divisor is 4, so four concurrent statements exactly spend the
    // ceiling -- leaving NOTHING for the paged cache or the derived structures
    // (8.5 GB adopted at SF10), which the divisor's own docstring says that
    // share is meant to cover.
    // `>=` would fail by 256 BYTES here: the row count is an integer division,
    // so 4 x budget x 96 lands just under the ceiling it was derived from.
    // Measured as a fraction, which is what the claim actually is.
    let four_share = (per_statement * 4) as f64 / FULLNODE_CEILING as f64;
    assert!(
        four_share > 0.999,
        "four concurrent statements should account for essentially the whole          ceiling (got {four_share:.4} of it)"
    );
    println!("  4 concurrent statements: {four_share:.4}x the container ceiling — leaving nothing for the paged cache or the 8.5 GB of derived structures");

    // And the server admits far more than four. `--max-connections` defaults to
    // 512; the bench runs used 6 workers.
    for concurrent in [6u64, 8, 32] {
        let committed = per_statement * concurrent;
        assert!(
            committed > FULLNODE_CEILING,
            "{concurrent} concurrent statements must over-commit, else this \
             test has stopped measuring the conflict"
        );
        let over = committed as f64 / FULLNODE_CEILING as f64;
        println!("{concurrent:>3} concurrent statements: {over:.2}x the container ceiling");
    }
}

#[test]
fn the_conflict_has_no_solution_as_a_constant() {
    // Any divisor large enough to keep 8 concurrent statements inside the
    // container refuses q7 -- proven by construction across the whole range.
    let ceiling = FULLNODE_CEILING;
    let mut safe_for_8 = None;
    for divisor in 1..=64u64 {
        let budget = (ceiling / divisor / 96).max(1);
        let fits_8 = budget * 96 * 8 <= ceiling;
        let admits_q7 = budget >= Q7_ROWS_AT_SF10;
        assert!(
            !(fits_8 && admits_q7),
            "divisor {divisor} satisfied BOTH requirements — the conflict this \
             test documents would be solved and the shared-pool work unnecessary"
        );
        if fits_8 && safe_for_8.is_none() {
            safe_for_8 = Some(divisor);
        }
    }
    let d = safe_for_8.expect("some divisor bounds 8 statements");
    let budget_then = ceiling / d / 96;
    println!(
        "smallest divisor that bounds 8 concurrent statements: {d} -> {budget_then} rows, \
         which is {:.1}x TOO SMALL for q7's {Q7_ROWS_AT_SF10}",
        Q7_ROWS_AT_SF10 as f64 / budget_then as f64
    );
}
