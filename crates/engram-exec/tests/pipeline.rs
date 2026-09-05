#![allow(non_snake_case)]
//! The pipeline shell — operators chained, the report as one artifact.

use engram_exec::{
    GroupAt, OffsetList, Pipeline, PipelineCause, RowDirectory, RowIdError, RowIdSet, StageDetail,
};
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

#[test]
fn a_chained_run_reports_every_stage_with_its_counts() {
    // seed {1,2,3} → mask to {1,2} → expand → finish. The report must carry
    // all three stages, and the counts must be the counts.
    let s = build_graph(&[(1, 10), (1, 11), (2, 11), (3, 12)]);
    let src = RowDirectory::from_ids([1, 2, 3]);
    let dst = RowDirectory::from_ids([10, 11, 12]);
    let (seed, _) = src.to_set(&[1, 2, 3]).expect("seed");

    let (ids, report) = Pipeline::seed(&s, &src, seed, "seed")
        .expect("seed")
        .mask(&OffsetList(&[0, 1]), "tenant")
        .expect("mask")
        .expand_hop(group(), EdgeDir::Out, ET, &dst, Namespace(1), "hop")
        .expect("expand")
        .finish()
        .expect("finish");

    assert_eq!(
        ids,
        vec![10, 11],
        "nodes 1 and 2 reach 10 and 11; node 3 was masked"
    );
    assert_eq!(report.stages.len(), 3);
    assert_eq!(report.stages[0].detail, StageDetail::Seed);
    assert_eq!((report.stages[0].input, report.stages[0].output), (3, 3));
    assert_eq!(report.stages[1].detail, StageDetail::Mask { masked_out: 1 });
    assert_eq!((report.stages[1].input, report.stages[1].output), (3, 2));
    match report.stages[2].detail {
        StageDetail::Expand {
            edges,
            outside_group,
        } => {
            assert_eq!(
                edges, 3,
                "1→10, 1→11, 2→11 — node 3's edge was never walked"
            );
            assert_eq!(outside_group, 0);
        }
        ref other => panic!("expected an expand stage, got {other:?}"),
    }
    assert_eq!(report.final_output(), 2);
    assert_eq!(report.total_edges(), 3);
    assert_eq!(
        report.stages[1].label, "tenant",
        "the planner's label survives into the report"
    );
}

#[test]
fn an_emptied_pipeline_SKIPS_downstream_stages_and_says_so() {
    // "Ran and found nothing" and "never ran" are different facts. A mask that
    // empties the set must leave downstream stages reported as Skipped — not
    // absent from the report, which would read as a two-stage plan.
    let s = build_graph(&[(1, 10)]);
    let src = RowDirectory::from_ids([1]);
    let dst = RowDirectory::from_ids([10]);
    let (seed, _) = src.to_set(&[1]).expect("seed");

    let (ids, report) = Pipeline::seed(&s, &src, seed, "seed")
        .expect("seed")
        .mask(&RowIdSet::empty(1), "empty-tenant")
        .expect("mask")
        .expand_hop(group(), EdgeDir::Out, ET, &dst, Namespace(1), "hop")
        .expect("expand")
        .mask(&OffsetList(&[0]), "post-filter")
        .expect("mask2")
        .finish()
        .expect("finish");

    assert!(ids.is_empty());
    assert_eq!(
        report.stages.len(),
        4,
        "every planned stage appears, skipped or not"
    );
    assert_eq!(report.stages[2].detail, StageDetail::Skipped);
    assert_eq!(report.stages[3].detail, StageDetail::Skipped);
    assert_eq!(
        report.final_output(),
        0,
        "final output is the mask's 0, not a skipped stage's"
    );
    assert_eq!(
        report.total_edges(),
        0,
        "no edge was walked for a skipped expand"
    );
}

#[test]
fn a_skipped_expand_still_moves_the_universe() {
    // The empty fast-path must not leave the set in the SOURCE offset space
    // while the directory moves on: `finish` maps the set through the CURRENT
    // directory, so a half-moved universe (dir at dst, set still src-sized)
    // refuses with a capacity mismatch. Different capacities on purpose, so
    // the mismatch cannot hide.
    let s = build_graph(&[(1, 10)]);
    let src = RowDirectory::from_ids([1, 2, 3, 4, 5]); // 5 rows
    let dst = RowDirectory::from_ids([10, 11]); // 2 rows — different capacity
    let (seed, _) = src.to_set(&[1]).expect("seed");

    let (ids, report) = Pipeline::seed(&s, &src, seed, "seed")
        .expect("seed")
        .mask(&RowIdSet::empty(5), "empty")
        .expect("mask")
        .expand_hop(group(), EdgeDir::Out, ET, &dst, Namespace(1), "hop")
        .expect("expand")
        .finish()
        .expect("finish maps through the DESTINATION directory — set and dir agree");

    assert!(ids.is_empty());
    assert_eq!(report.stages[2].detail, StageDetail::Skipped);
}

#[test]
fn a_refusal_names_its_stage_and_carries_the_partial_report() {
    let s = build_graph(&[(1, 10)]);
    let src = RowDirectory::from_ids([1]);
    let (seed, _) = src.to_set(&[1]).expect("seed");

    // A mask from another universe: the refusal must say which stage, and the
    // report must still hold the stages that DID run.
    let err = Pipeline::seed(&s, &src, seed, "seed")
        .expect("seed")
        .mask(&RowIdSet::empty(99), "foreign-mask")
        .unwrap_err();
    assert_eq!(err.stage, "foreign-mask");
    assert!(matches!(
        err.cause,
        PipelineCause::Rows(RowIdError::CapacityMismatch { .. })
    ));
    assert_eq!(
        err.report.stages.len(),
        1,
        "the seed stage survives into the error"
    );
    assert_eq!(err.report.stages[0].detail, StageDetail::Seed);
}

#[test]
fn seeding_with_a_foreign_set_is_refused_at_the_door() {
    let s = Store::new();
    let src = RowDirectory::from_ids([1, 2]);
    let err = Pipeline::seed(&s, &src, RowIdSet::empty(7), "seed").unwrap_err();
    assert_eq!(err.stage, "seed");
    assert!(matches!(
        err.cause,
        PipelineCause::Rows(RowIdError::CapacityMismatch { .. })
    ));
}

#[test]
fn outside_group_peers_are_COUNTED_in_the_report() {
    // Node 1 reaches 10 (in dst) and 20 (not in dst). The report must carry
    // the outside count — a frontier that silently vanished is the
    // assert-arrival defect in traversal form.
    let s = build_graph(&[(1, 10), (1, 20)]);
    let src = RowDirectory::from_ids([1]);
    let dst = RowDirectory::from_ids([10]); // 20 deliberately absent
    let (seed, _) = src.to_set(&[1]).expect("seed");

    let (ids, report) = Pipeline::seed(&s, &src, seed, "seed")
        .expect("seed")
        .expand_hop(group(), EdgeDir::Out, ET, &dst, Namespace(1), "hop")
        .expect("expand")
        .finish()
        .expect("finish");

    assert_eq!(ids, vec![10]);
    match report.stages[1].detail {
        StageDetail::Expand {
            edges,
            outside_group,
        } => {
            assert_eq!(edges, 2);
            assert_eq!(
                outside_group, 1,
                "the peer outside the group is a count, not a disappearance"
            );
        }
        ref other => panic!("expected an expand stage, got {other:?}"),
    }
}

#[test]
fn final_output_on_a_hand_built_report_reads_past_skips() {
    // Pipeline-generated reports can only skip after an emptied set, so their
    // trailing skips always follow a 0 — the skip-awareness is only
    // falsifiable on a constructed report, which planners may legitimately
    // build (a plan echoed before execution, with unreached stages Skipped).
    use engram_exec::{PipelineReport, StageReport};
    let report = PipelineReport {
        stages: vec![
            StageReport {
                label: "seed".into(),
                input: 9,
                output: 9,
                detail: StageDetail::Seed,
            },
            StageReport {
                label: "budget-dropped".into(),
                input: 0,
                output: 0,
                detail: StageDetail::Skipped,
            },
        ],
    };
    assert_eq!(
        report.final_output(),
        9,
        "the skip is not the answer; the last REAL stage is"
    );
    let empty = PipelineReport { stages: vec![] };
    assert_eq!(empty.final_output(), 0);
}

#[test]
fn final_output_reads_past_trailing_skipped_stages() {
    // A report ending in skips must report the last REAL output, not 0-from-skip
    // and not the skip itself.
    let s = build_graph(&[(1, 10)]);
    let src = RowDirectory::from_ids([1]);
    let dst = RowDirectory::from_ids([10]);
    let (seed, _) = src.to_set(&[1]).expect("seed");

    let (_, report) = Pipeline::seed(&s, &src, seed, "seed")
        .expect("seed")
        .expand_hop(group(), EdgeDir::Out, ET, &dst, Namespace(1), "hop")
        .expect("expand")
        .mask(&RowIdSet::empty(1), "kill")
        .expect("mask")
        .mask(&OffsetList(&[0]), "after-kill")
        .expect("skip")
        .finish()
        .expect("finish");

    assert_eq!(report.stages[3].detail, StageDetail::Skipped);
    assert_eq!(
        report.final_output(),
        0,
        "the kill mask's 0 is the answer; the skip is not consulted"
    );
    // And the identity on the mask stage holds.
    assert_eq!(report.stages[2].detail, StageDetail::Mask { masked_out: 1 });
}
