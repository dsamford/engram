#![allow(non_snake_case)]
//! Fix 91: A LEVER ONLY `cargo test` CAN REACH HAS NEVER BEEN MEASURED — and
//! this test is what stops the set of them GROWING again.
//!
//! Strategy S2 (fix 77) found two: `set_whole_label_read_max` and
//! `pipeline::set_subquery_end_gather` were declared, documented, and reachable
//! only from unit tests, so fixes 118 and 121 could not be A/B'd on the pod at
//! all — their pod numbers had been attributed by comparing two BINARIES, which
//! credits the whole delta between them to one fix.
//!
//! It recurred, silently, and was found by hand rather than by a gate:
//! `Graph::set_prop_column_epoch_currency` — fix 124's arm, which decides
//! whether a cached property column survives a commit that touched neither its
//! label's nor its property's epoch — was reachable from ONE integration test
//! and nothing else. Under a write stream that lever decides whether the
//! columnar fast paths exist at all, and it had never been run both ways on
//! the pod. It is wired now (`--no-prop-column-epoch-currency`).
//!
//! So the rule is mechanical from here. `KNOWN_UNWIRED` is the DEBT as it
//! stood when this test was written: 41 levers the server does not set. It is
//! not an approval — it is the list of things nobody can A/B on the pod today.
//! The gate is a RATCHET: a lever not on the list must be wired, and a lever
//! on the list that becomes wired must be REMOVED from it, so the debt can
//! only shrink. Auditing the 41 is separate work; making the 42nd impossible
//! is this test.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The debt, frozen at fix 91. See the module doc: shrink only.
const KNOWN_UNWIRED: &[&str] = &[
    "set_adj_cost_repair",
    "set_agg_native_key",
    "set_algo_min_vertices",
    "set_bulk_ingest",
    "set_chain_count_fold",
    "set_columnar_agg_batch",
    "set_columnar_agg_batch_size",
    "set_columnar_column_budget_factor",
    "set_columnar_scans",
    "set_degree_aggregate",
    "set_demote_adjacency_rebuild",
    "set_detach_via_rel_ids",
    "set_edge_probe",
    "set_entity_latching",
    "set_estimate_sample_budget",
    "set_frontier_expand",
    "set_hop_reversal",
    "set_incremental_caches",
    "set_label_epoch_atomics",
    "set_late_projection",
    "set_lean_subquery_seed",
    "set_members_batch_fold",
    "set_merge_race_hook_for_test",
    "set_multistage_topk_batch",
    "set_parallel_fold_min_rows",
    "set_parallel_min_rows",
    "set_pattern_map_seek",
    "set_persist_derived",
    "set_persist_growth_interval",
    "set_prop",
    "set_read_set_bindings_only",
    "set_reader_rebuild_admission",
    "set_scan_resistant_rebuild",
    "set_scope_pruning",
    "set_selective_anchor",
    "set_serialisable_autocommit",
    "set_stats_delta",
    "set_vector_exact_max",
    "set_volatile_guards",
    "set_zone_provider",
];

fn crate_src(krate: &str, file: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push(krate);
    p.push("src");
    p.push(file);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Every `pub fn set_NAME(` in `text`.
fn levers(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("pub fn set_") else {
            continue;
        };
        let Some(name) = rest.split(['(', '<']).next() else {
            continue;
        };
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            out.insert(format!("set_{name}"));
        }
    }
    out
}

#[test]
fn no_new_lever_ships_unreachable_from_the_server() {
    let mut found = levers(&crate_src("engram-graph", "lib.rs"));
    found.extend(levers(&crate_src("engram-graph", "pipeline.rs")));
    let server = crate_src("engram-server", "lib.rs");

    // A scan that finds nothing would pass every assertion below while
    // checking nothing — the vacuous-differential failure, one level up.
    assert!(
        found.len() > 60,
        "the lever scan found only {} — the `pub fn set_` shape must have changed",
        found.len()
    );
    let wired = |name: &str| server.contains(&format!("{name}("));
    assert!(
        wired("set_deferred_reader_fold") && wired("set_count_fold"),
        "the wiring probe itself is broken: two levers known to be server-set read as unwired"
    );

    let known: BTreeSet<&str> = KNOWN_UNWIRED.iter().copied().collect();
    let new_debt: Vec<&str> = found
        .iter()
        .map(String::as_str)
        .filter(|n| !known.contains(n) && !wired(n))
        .collect();
    assert!(
        new_debt.is_empty(),
        "these levers are declared but the SERVER never sets them, so no pod run can put them \
         on the other arm and whatever they gate cannot be measured against its own control \
         (strategy S2, fix 77 — it has already happened twice):\n  {}\n\
         Wire each into `ServerConfig` with a CLI flag, or add it to KNOWN_UNWIRED and say in \
         the commit why it can never be an arm.",
        new_debt.join("\n  ")
    );

    // The debt list must not rot: a name that is now WIRED reads as
    // outstanding debt that has in fact been paid, and a debt list that
    // overstates the debt stops being read.
    //
    // A name that is simply ABSENT is deliberately NOT an error. The list is
    // a superset across trees: the ship tree is a subset of the working tree
    // for as long as a concurrent workstream's levers have not shipped, so a
    // name can be missing here because this tree PREDATES the lever, not
    // because anyone deleted it. Failing on absence would make this test
    // untransferable between the two trees — it would have to be edited on
    // every transfer, which is how a gate becomes something people route
    // around. Absence is not debt; only a paid debt still claimed is.
    let stale: Vec<&str> = KNOWN_UNWIRED.iter().copied().filter(|n| wired(n)).collect();
    assert!(
        stale.is_empty(),
        "KNOWN_UNWIRED still lists levers the server now SETS. Remove them — a debt list that \
         overstates the debt stops being read:\n  {}",
        stale.join("\n  ")
    );
}
