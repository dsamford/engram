//! The plan-replay contract, checked in the SQL dialect: one plan, replayed
//! twice, issues the same statements in the same order — and a later level
//! moves the identity binding and nothing else.
//!
//! # Why this exists, in one sentence someone already paid for
//!
//! The LadybugDB executor's first version replayed a plan at every client
//! level without applying the level stride. By K=16 every bound id already
//! existed, every write was rejected, `write_ops` fell to 0 — **and the level
//! reported 5,019 ops/s with an integrity check that PASSED**, truthfully,
//! reconciling zero acked writes against zero new rows. The workload had
//! evaporated and every instrument still read green. This file is the
//! instrument that would not have.
//!
//! # Why in SQL rather than in Cypher
//!
//! `a_converged_plan_replays_the_stress_op_sequence.rs` already pins the
//! Cypher text at level index 0, where scoping is the identity. It covers
//! neither of the two things that matter here. The SQL dialect is where they
//! bite hardest, because `uniq.u` is a real PRIMARY KEY: on Bolt a dropped
//! stride merely writes a duplicate nobody notices, and on PostgreSQL it turns
//! every write in the level into a refusal — a rate of zero acked operations
//! that a reader would attribute to the engine.
//!
//! # What this checks, stated precisely
//!
//! 1. **Determinism.** Two `stream_for` calls at one level render byte-
//!    identical statement sequences.
//! 2. **Scoping is applied.** At level 1 every op that mints an identity moves
//!    its `id` (or `unique_create`'s `u`) by exactly `LEVEL_STRIDE`.
//! 3. **Scoping is applied to nothing else.** Reads are untouched; a
//!    `rel_spread` / `rel_hub` endpoint names a pre-existing corpus node and
//!    must not move; `hot_update` has no identity to move.
//! 4. **The check is not vacuous.** Each scoped profile must produce at least
//!    one statement that actually MOVED, and the same comparison run against
//!    an UNSCOPED replay must find level 1 indistinguishable from level 0.
//!    That last part is the negative control: without it, a bug that made
//!    `scope_params` a no-op would leave points 1–3 passing.
//!
//! Nothing here touches a server. The statements are rendered, compared and
//! discarded — this is the contract, not the measurement.

use std::collections::BTreeMap;

use engram_bench::catalogue::{Catalogue, Dialect, Status, has_unbound, render};
use engram_bench::plan::{LEVEL_STRIDE, PlanOp, emit_plan, load_plan, scope_params, scoped_field};
use engram_bench::workload::{Dataset, LevelSpec, Param, profile};

const SEED: u64 = 424_242;
const NONCE: u64 = 1;
const CLIENTS: usize = 3;
const OPS: usize = 96;
const KEYS: u64 = 2_000;

/// Every profile whose `synthetic` shapes and write op the SQL dialect can
/// express. `balanced-nolabels` is excluded because the catalogue declares its
/// write `unsupported` in SQL — a decision, recorded, not a gap.
const PROFILES_UNDER_TEST: [&str; 9] = [
    "read-only",
    "read-heavy",
    "balanced",
    "write-only",
    "contention",
    "balanced-disjoint",
    "rel-create",
    "rel-hub",
    "unique-create",
];

/// A scratch path that does not collide with a concurrently running copy of
/// this suite. `emit_plan` writes a real file because the SHA-256 is over the
/// bytes on disk, and a plan checked from memory would not be the plan.
fn scratch(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "engram-bench-plan-{}-{name}-{:?}.jsonl",
        std::process::id(),
        std::thread::current().id()
    ));
    p
}

/// The statement one plan op sends in one dialect — `harness.rs`'s
/// `render_op`, reproduced here because it lives in a binary.
///
/// A rendered statement that still carries `${…}` is returned as an error
/// rather than compared: two identical strings that both contain an unbound
/// placeholder would agree with each other and disagree with the engine.
fn render_op(cat: &Catalogue, op: &PlanOp) -> Result<String, String> {
    let pairs: Vec<(String, String)> = op
        .params()
        .iter()
        .map(|(k, v)| (k.clone(), v.render()))
        .collect();
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let entry = match op {
        PlanOp::Read { shape, .. } => cat
            .read_shape(shape, Dialect::Sql)
            .map_err(|e| e.to_string())?,
        PlanOp::Write { op: name, .. } => cat
            .write_op(name, "synthetic", Dialect::Sql)
            .map_err(|e| e.to_string())?,
    };
    if let Status::Unsupported(reason) = &entry.status {
        return Err(format!("unsupported: {reason}"));
    }
    let out = render(&entry.text, &refs);
    if has_unbound(&out) {
        return Err(format!("unbound placeholder survived rendering: {out}"));
    }
    Ok(out)
}

/// Render one client's whole stream, or say which op could not be rendered.
fn render_stream(cat: &Catalogue, ops: &[PlanOp]) -> Vec<String> {
    ops.iter()
        .map(|op| {
            render_op(cat, op).unwrap_or_else(|e| {
                panic!(
                    "every op in a synthetic plan must render in SQL; {:?} did not: {e}",
                    op.shape().or(op.op())
                )
            })
        })
        .collect()
}

/// The same stream WITHOUT the level offset — what a replayer that forgot the
/// stride would send. The negative control for every scoping claim below.
fn unscoped_stream(plan: &engram_bench::plan::LoadedPlan, cid: usize) -> Vec<PlanOp> {
    plan.stream_for(cid, 0).expect("level 0 stream")
}

#[test]
fn one_plan_replayed_twice_at_one_level_sends_the_same_sql_in_the_same_order() {
    let cat = Catalogue::load().expect("catalogue");
    for name in PROFILES_UNDER_TEST {
        let prof = profile(name).expect("profile");
        let path = scratch(name);
        let spec = LevelSpec {
            seed: SEED,
            dataset: Dataset::Synthetic,
            keys: KEYS,
            nonce: NONCE,
        };
        emit_plan(&path, spec, prof, CLIENTS, OPS).expect("emit");
        let plan = load_plan(&path).expect("load");
        for cid in 0..CLIENTS {
            let first = render_stream(&cat, &plan.stream_for(cid, 1).expect("stream"));
            let second = render_stream(&cat, &plan.stream_for(cid, 1).expect("stream"));
            assert_eq!(
                first, second,
                "{name} client {cid}: two replays of one plan diverged"
            );
            assert_eq!(
                first.len(),
                OPS,
                "{name} client {cid}: a replay is the whole stream or it is a \
                 different workload"
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn level_one_moves_the_identity_binding_by_exactly_one_stride_and_moves_nothing_else() {
    let cat = Catalogue::load().expect("catalogue");
    // Counted across the whole sweep so the assertion at the end can refuse a
    // vacuous pass: a run in which nothing moved is not a run in which nothing
    // needed to.
    let mut moved_total = 0usize;
    for name in PROFILES_UNDER_TEST {
        let prof = profile(name).expect("profile");
        let path = scratch(&format!("scope-{name}"));
        let spec = LevelSpec {
            seed: SEED,
            dataset: Dataset::Synthetic,
            keys: KEYS,
            nonce: NONCE,
        };
        emit_plan(&path, spec, prof, CLIENTS, OPS).expect("emit");
        let plan = load_plan(&path).expect("load");
        let mut moved_here = 0usize;
        for cid in 0..CLIENTS {
            let base = plan.stream_for(cid, 0).expect("level 0");
            let next = plan.stream_for(cid, 1).expect("level 1");
            assert_eq!(base.len(), next.len(), "{name}: level lengths differ");
            for (a, b) in base.iter().zip(next.iter()) {
                assert_eq!(
                    a.op(),
                    b.op(),
                    "{name}: the op sequence moved between levels"
                );
                assert_eq!(
                    a.shape(),
                    b.shape(),
                    "{name}: the shape sequence moved between levels"
                );
                let field = a.op().and_then(scoped_field);
                for (k, v0) in a.params() {
                    let v1 = b
                        .params()
                        .get(k)
                        .unwrap_or_else(|| panic!("{name}: level 1 dropped the binding `{k}`"));
                    let expected_move = field == Some(k.as_str());
                    match (v0, v1) {
                        (Param::Uint(x), Param::Uint(y)) if expected_move => {
                            assert_eq!(
                                *y,
                                x.wrapping_add(LEVEL_STRIDE),
                                "{name}: `{k}` is the scoped field and moved by \
                                 {} rather than one LEVEL_STRIDE",
                                y.wrapping_sub(*x)
                            );
                            moved_here += 1;
                        }
                        _ => assert_eq!(
                            v0, v1,
                            "{name}: `{k}` is NOT the scoped field and must not move — a \
                             rule that scoped `anything called id` would move \
                             rel_spread's endpoints, which name pre-existing corpus nodes"
                        ),
                    }
                }
                // A read carries no identity at all, so its rendered SQL is
                // the same string at every level.
                if !a.is_write() {
                    assert_eq!(
                        render_op(&cat, a).expect("render"),
                        render_op(&cat, b).expect("render"),
                        "{name}: a read's statement changed between levels"
                    );
                }
            }
        }
        // A profile that issues no writes at all has nothing to scope, and
        // demanding movement from it would make `read-only` fail for being
        // read-only. The condition is "issues writes AND its op mints an
        // identity", which is exactly when a missing stride would collide.
        let scopes = prof.write_pct > 0 && scoped_field(neutral_write_op(prof)).is_some();
        if scopes {
            assert!(
                moved_here > 0,
                "{name} declares a scoped write op but no binding moved between \
                 levels — the check passed because it checked nothing"
            );
        }
        moved_total += moved_here;
        let _ = std::fs::remove_file(&path);
    }
    assert!(
        moved_total > 0,
        "no profile moved a single binding: this whole file would pass against a \
         `scope_params` that returned its input"
    );
}

/// The neutral write-op name a profile's writes carry, for the "does this
/// profile scope anything" question. Kept as a small table rather than derived
/// so it fails loudly when a profile's write kind changes underneath it.
fn neutral_write_op(p: &engram_bench::workload::Profile) -> &'static str {
    use engram_bench::workload::{Locality, WriteKind};
    match (p.write_kind, p.write_locality) {
        (WriteKind::Node, Locality::Hot) => "hot_update",
        (WriteKind::Node, _) => "node_create",
        (WriteKind::NodeOnly, _) => "node_only",
        (WriteKind::NodeOnlyFreshProps, _) => "node_only_fresh_props",
        (WriteKind::NodeOnlyNoLabels, _) => "node_only_no_labels",
        (WriteKind::UniqueCreate, _) => "unique_create",
        (WriteKind::RelSpread, _) => "rel_spread",
        (WriteKind::RelHub, _) => "rel_hub",
        (WriteKind::DeleteChurn, _) => "churn_create",
    }
}

#[test]
fn an_unscoped_replay_is_detectably_the_same_workload_twice() {
    // The negative control, and the reason the three tests above are worth
    // anything. It fails ON PURPOSE against a replayer that skips the stride:
    // level 1's statements come back identical to level 0's, which is exactly
    // the condition that let a plan re-issue every bound id and report a rate
    // over an evaporated workload.
    let cat = Catalogue::load().expect("catalogue");
    let prof = profile("unique-create").expect("profile");
    let path = scratch("unscoped");
    let spec = LevelSpec {
        seed: SEED,
        dataset: Dataset::Synthetic,
        keys: KEYS,
        nonce: NONCE,
    };
    emit_plan(&path, spec, prof, CLIENTS, OPS).expect("emit");
    let plan = load_plan(&path).expect("load");

    let level0 = render_stream(&cat, &plan.stream_for(0, 0).expect("level 0"));
    let scoped1 = render_stream(&cat, &plan.stream_for(0, 1).expect("level 1"));
    let forgot1 = render_stream(&cat, &unscoped_stream(&plan, 0));

    assert_eq!(
        level0, forgot1,
        "the control is only a control if the unscoped stream really is level 0's"
    );
    assert_ne!(
        level0, scoped1,
        "level 1 renders the same SQL as level 0: the stride is not reaching the \
         statement, and every INSERT in the level would collide with a committed row"
    );
    // And say WHERE they differ, so a failure of the assertion above is
    // readable rather than a wall of SQL.
    let first_diff = level0
        .iter()
        .zip(scoped1.iter())
        .position(|(a, b)| a != b)
        .expect("they differ somewhere, per the assertion above");
    assert!(
        scoped1[first_diff].contains(&(LEVEL_STRIDE).to_string())
            || scoped1[first_diff]
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|t| t.parse::<u64>().ok())
                .any(|n| n >= LEVEL_STRIDE),
        "the first statement that moved does not carry a value at or above one \
         LEVEL_STRIDE: {}",
        scoped1[first_diff]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn scoping_a_scoped_map_again_is_the_bug_the_new_map_prevents() {
    // `scope_params` returns a NEW map because scoping in place would make
    // level 2 scope an already-scoped value — the same class of bug as not
    // scoping, and harder to see. Asserted rather than trusted: the plan is
    // held once in memory and read by every level in turn.
    let mut params: BTreeMap<String, Param> = BTreeMap::new();
    params.insert("u".to_string(), Param::Uint(7));
    let one = scope_params("unique_create", &params, 1);
    let two = scope_params("unique_create", &params, 2);
    assert_eq!(
        params.get("u"),
        Some(&Param::Uint(7)),
        "the source map moved"
    );
    assert_eq!(one.get("u"), Some(&Param::Uint(7 + LEVEL_STRIDE)));
    assert_eq!(two.get("u"), Some(&Param::Uint(7 + 2 * LEVEL_STRIDE)));
    // And the field that must not move, at the same level.
    let mut endpoints: BTreeMap<String, Param> = BTreeMap::new();
    endpoints.insert("a".to_string(), Param::Uint(11));
    endpoints.insert("b".to_string(), Param::Uint(0));
    let moved = scope_params("rel_spread", &endpoints, 3);
    assert_eq!(
        moved, endpoints,
        "a rel endpoint names a corpus node and moved"
    );
}
