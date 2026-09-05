#![allow(non_snake_case)]
//! The directory and Expand — traversal as sets over universes.

use engram_exec::{GroupAt, OffsetList, RowDirectory, RowIdError, RowIdSet, expand, semi_mask};
use engram_key::{Namespace, Partition, Realm};
use engram_runtime::Runtime as _;
use engram_store::{EdgeDir, EdgeType, NodeAt, Store, add_edge};

const ET: EdgeType = EdgeType(1);

fn group() -> GroupAt {
    GroupAt {
        realm: Realm(1),
        ns: Namespace(1),
        partition: Partition(1),
    }
}

fn node(id: u64) -> NodeAt {
    NodeAt {
        realm: Realm(1),
        ns: Namespace(1),
        partition: Partition(1),
        node: id,
    }
}

fn build_graph(edges: &[(u64, u64)]) -> Store {
    let s = Store::new();
    let rt = engram_runtime::SimRuntime::new(1);
    let store = s.clone();
    let edges = edges.to_vec();
    rt.spawn(async move {
        for (a, b) in edges {
            add_edge(&store, node(a), ET, node(b)).await.expect("edge");
        }
    });
    rt.run(10_000_000).expect("completes");
    s
}

// ─── The directory ──────────────────────────────────────────────────────────

#[test]
fn offsets_are_dense_stable_and_first_seen_ordered() {
    let mut d = RowDirectory::new();
    assert_eq!(d.intern(50), 0);
    assert_eq!(d.intern(10), 1);
    assert_eq!(d.intern(50), 0, "re-interning must return the SAME offset");
    assert_eq!(d.len(), 2);
    assert_eq!(d.id_of(0), Some(50));
    assert_eq!(d.offset_of(10), Some(1));
}

#[test]
fn to_set_REPORTS_unmapped_ids_instead_of_dropping_them() {
    // "The index is stale" and "the candidate was filtered" must not be the
    // same observation — the unmapped list is part of the result.
    let d = RowDirectory::from_ids([1, 2, 3]);
    let (set, unmapped) = d.to_set(&[1, 3, 99, 100]).expect("maps");
    assert_eq!(set.count(), 2);
    assert_eq!(unmapped, vec![99, 100]);
}

#[test]
fn to_ids_REFUSES_a_set_from_another_universe() {
    let d = RowDirectory::from_ids([1, 2, 3]);
    let foreign = RowIdSet::empty(10);
    assert!(matches!(
        d.to_ids(&foreign),
        Err(RowIdError::CapacityMismatch { .. })
    ));
}

#[test]
fn directory_round_trip_preserves_identity() {
    let d = RowDirectory::from_ids([7, 5, 9]);
    let (set, unmapped) = d.to_set(&[9, 7]).expect("maps");
    assert!(unmapped.is_empty());
    assert_eq!(
        d.to_ids(&set).expect("ids"),
        vec![7, 9],
        "ascending by OFFSET (first-seen order)"
    );
}

// ─── Expand ─────────────────────────────────────────────────────────────────

#[test]
fn expand_follows_edges_and_reports_its_work() {
    let s = build_graph(&[(1, 10), (1, 11), (2, 11), (2, 12)]);
    let src = RowDirectory::from_ids([1, 2]);
    let dst = RowDirectory::from_ids([10, 11, 12]);

    let (input, _) = src.to_set(&[1, 2]).expect("input");
    let (out, r) = expand(
        &s,
        group(),
        &src,
        &input,
        EdgeDir::Out,
        ET,
        &dst,
        Namespace(1),
    )
    .expect("expand");

    assert_eq!(dst.to_ids(&out).expect("ids"), vec![10, 11, 12]);
    assert_eq!(r.sources, 2);
    assert_eq!(
        r.edges, 4,
        "the work count is edges TRAVERSED, not distinct peers"
    );
    assert_eq!(r.in_group, 3, "11 reached twice counts once in the set");
    assert!(r.outside_group.is_empty());
}

#[test]
fn peers_OUTSIDE_the_destination_group_are_carried_not_dropped() {
    // Node 20 is reachable but not in the destination directory — a different
    // partition's row, say. Dropping it silently makes "the group covers the
    // neighbourhood" and "a third of the frontier vanished" the same reading.
    let s = build_graph(&[(1, 10), (1, 20)]);
    let src = RowDirectory::from_ids([1]);
    let dst = RowDirectory::from_ids([10]); // 20 is not here

    let (input, _) = src.to_set(&[1]).expect("input");
    let (out, r) = expand(
        &s,
        group(),
        &src,
        &input,
        EdgeDir::Out,
        ET,
        &dst,
        Namespace(1),
    )
    .expect("expand");

    assert_eq!(dst.to_ids(&out).expect("ids"), vec![10]);
    assert_eq!(r.outside_group, vec![(1, 20)]);
    assert_eq!(r.edges, 2);
}

#[test]
fn expand_composes_with_the_semi_mask() {
    // The pipeline the whole layer exists for: seeds → Expand → scope-mask,
    // every stage measured.
    let s = build_graph(&[(1, 10), (1, 11), (1, 12)]);
    let src = RowDirectory::from_ids([1]);
    let dst = RowDirectory::from_ids([10, 11, 12]);

    let (input, _) = src.to_set(&[1]).expect("input");
    let (frontier, er) = expand(
        &s,
        group(),
        &src,
        &input,
        EdgeDir::Out,
        ET,
        &dst,
        Namespace(1),
    )
    .expect("expand");
    // The tenant scope admits only offsets 0 and 2 (ids 10, 12).
    let (scoped, mr) = semi_mask(&frontier, &OffsetList(&[0, 2])).expect("mask");

    assert_eq!(dst.to_ids(&scoped).expect("ids"), vec![10, 12]);
    assert_eq!(er.in_group, 3);
    assert_eq!(
        mr.masked_out, 1,
        "the scope's cost on this query is a COUNT, end to end"
    );
}

#[test]
fn an_input_from_the_wrong_universe_is_refused() {
    let s = build_graph(&[(1, 10)]);
    let src = RowDirectory::from_ids([1]);
    let dst = RowDirectory::from_ids([10]);
    let wrong = RowIdSet::empty(99);
    assert!(matches!(
        expand(
            &s,
            group(),
            &src,
            &wrong,
            EdgeDir::Out,
            ET,
            &dst,
            Namespace(1)
        ),
        Err(engram_exec::ExpandError::Rows(
            RowIdError::CapacityMismatch { .. }
        )),
    ));
}

#[test]
fn expanding_an_empty_set_is_an_empty_RESULT_with_a_zero_report() {
    let s = build_graph(&[(1, 10)]);
    let src = RowDirectory::from_ids([1]);
    let dst = RowDirectory::from_ids([10]);
    let (out, r) = expand(
        &s,
        group(),
        &src,
        &RowIdSet::empty(1),
        EdgeDir::Out,
        ET,
        &dst,
        Namespace(1),
    )
    .expect("expand");
    assert_eq!(out.count(), 0);
    assert_eq!((r.sources, r.edges, r.in_group), (0, 0, 0));
}
