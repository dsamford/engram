#![allow(non_snake_case)]
//! The semi-mask: AND semantics, the measurement, and the refusals.

use engram_exec::{MaskReport, OffsetList, RowIdError, RowIdSet, semi_mask};

#[test]
fn the_mask_is_an_AND_with_the_measurement() {
    let input = RowIdSet::from_offsets(100, &[1, 5, 9, 50, 99]).expect("input");
    let (out, report) = semi_mask(&input, &OffsetList(&[5, 9, 60])).expect("mask");
    assert_eq!(out.iter().collect::<Vec<_>>(), vec![5, 9]);
    assert_eq!(
        report,
        MaskReport {
            input: 5,
            output: 2,
            masked_out: 3
        }
    );
}

#[test]
fn the_measurement_is_the_COUNTS_not_an_estimate() {
    // Every combination must satisfy input = output + masked_out exactly —
    // the arithmetic identity an estimate cannot fake across cases.
    for (input_offs, mask_offs) in [
        (vec![0usize, 1, 2], vec![0usize, 1, 2]),
        (vec![0, 1, 2], vec![]),
        (vec![], vec![0, 1, 2]),
        (vec![0, 63, 64, 65], vec![63, 64]),
    ] {
        let input = RowIdSet::from_offsets(70, &input_offs).expect("in");
        let (out, r) = semi_mask(&input, &OffsetList(&mask_offs)).expect("mask");
        assert_eq!(r.input, input_offs.len());
        assert_eq!(r.output, out.count());
        assert_eq!(
            r.input,
            r.output + r.masked_out,
            "the identity broke for {input_offs:?}"
        );
    }
}

#[test]
fn masks_COMPOSE_and_order_does_not_matter() {
    // Tenant ∧ ANN ∧ predicate — the claim that they are "the same bitmap-AND"
    // is only true if composition is associative and commutative, so it is
    // asserted rather than assumed.
    let tenant = RowIdSet::from_offsets(200, &(0..100).collect::<Vec<_>>()).expect("tenant");
    let ann = RowIdSet::from_offsets(200, &[3, 40, 99, 150, 199]).expect("ann");
    let pred = RowIdSet::from_offsets(200, &[40, 99, 150]).expect("pred");

    let (a, _) = semi_mask(&tenant, &ann).expect("t∧a");
    let (a_then_p, _) = semi_mask(&a, &pred).expect("(t∧a)∧p");

    let (p, _) = semi_mask(&tenant, &pred).expect("t∧p");
    let (p_then_a, _) = semi_mask(&p, &ann).expect("(t∧p)∧a");

    assert_eq!(a_then_p, p_then_a);
    assert_eq!(a_then_p.iter().collect::<Vec<_>>(), vec![40, 99]);
}

#[test]
fn the_output_is_always_a_SUBSET_of_the_input() {
    // A mask that widens invents rows the scope never admitted — the
    // cross-tenant read wearing an operator's name.
    let input = RowIdSet::from_offsets(64, &[2, 4, 8]).expect("in");
    let (out, _) = semi_mask(&input, &OffsetList(&[1, 2, 3, 4, 5, 6, 7, 8, 9])).expect("mask");
    for o in out.iter() {
        assert!(input.contains(o), "offset {o} appeared from nowhere");
    }
}

#[test]
fn a_capacity_mismatch_is_REFUSED_not_truncated() {
    // Two sets over different groups describe different row universes; an AND
    // across them yields offsets with arbitrary identity. The refusal carries
    // both sizes so the producer bug is diagnosable.
    let input = RowIdSet::from_offsets(100, &[1]).expect("in");
    let mask = RowIdSet::from_offsets(50, &[1]).expect("mask");
    assert_eq!(
        semi_mask(&input, &mask),
        Err(RowIdError::CapacityMismatch {
            left: 50,
            right: 100
        }),
    );
}

#[test]
fn an_out_of_range_candidate_is_a_producer_bug_surfaced() {
    assert_eq!(
        RowIdSet::from_offsets(10, &[3, 10]),
        Err(RowIdError::OutOfRange {
            offset: 10,
            capacity: 10
        }),
    );
}

#[test]
fn full_and_complement_carry_no_phantom_rows() {
    // Capacity 70 spans two words; the last word's tail bits must be zero or
    // `full` claims rows 70..128 exist and every complement resurrects them.
    let full = RowIdSet::full(70);
    assert_eq!(full.count(), 70);
    let empty = full.complement();
    assert_eq!(empty.count(), 0);
    let full_again = empty.complement();
    assert_eq!(full_again.count(), 70);
    assert!(!full_again.contains(70));
}

#[test]
fn iteration_is_ascending_and_word_boundary_safe() {
    let s = RowIdSet::from_offsets(200, &[199, 0, 64, 63, 128, 65]).expect("set");
    assert_eq!(s.iter().collect::<Vec<_>>(), vec![0, 63, 64, 65, 128, 199]);
}

#[test]
fn an_empty_intersection_is_a_RESULT_with_its_report() {
    // "The mask removed everything" and "there were no candidates" are
    // different facts a caller must be able to tell apart — R26 measured what
    // happens when they blur (recall 0.000 that still returned rows).
    let input = RowIdSet::from_offsets(50, &[1, 2, 3]).expect("in");
    let (out, r) = semi_mask(&input, &OffsetList(&[40, 41])).expect("mask");
    assert_eq!(out.count(), 0);
    assert_eq!(
        r,
        MaskReport {
            input: 3,
            output: 0,
            masked_out: 3
        }
    );

    let empty_in = RowIdSet::empty(50);
    let (_, r2) = semi_mask(&empty_in, &OffsetList(&[40])).expect("mask");
    assert_eq!(
        r2,
        MaskReport {
            input: 0,
            output: 0,
            masked_out: 0
        }
    );
}

#[test]
fn DIRECT_and_assign_refuses_a_capacity_mismatch() {
    // Found by a canary: semi_mask's path checks capacity in candidates(), so
    // removing check_capacity was undetectable there — but a DIRECT and_assign
    // with mismatched sets zips over the shorter word array and every offset
    // past it SURVIVES UNMASKED. full(100) ∧ empty(50) would keep rows 64..99:
    // a widening, through the API a future operator will reach for first.
    let mut a = RowIdSet::full(100);
    let b = RowIdSet::empty(50);
    assert_eq!(
        a.and_assign(&b),
        Err(RowIdError::CapacityMismatch {
            left: 100,
            right: 50
        }),
    );
    // And the refused operation must not have half-applied.
    assert_eq!(a.count(), 100, "the refused AND mutated its target");

    let mut c = RowIdSet::full(100);
    assert_eq!(
        c.or_assign(&b),
        Err(RowIdError::CapacityMismatch {
            left: 100,
            right: 50
        }),
    );
}
