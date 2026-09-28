#![allow(non_snake_case)]
//! The adjacency entry budget must be sized to the machine, not to a constant.
//!
//! `ADJ_TABLE_MAX_ENTRIES` is `64 << 20` = 67,108,864 entries — about 1.6 GB —
//! identical on an 8 GB laptop and a 160 GiB server. SF10 carries 176,623,448
//! edges, so the UNTYPED adjacency bucket is 2.63x over it while every TYPED
//! table fits (largest: HAS_TAG at 38,405,215, 57% of the old budget).
//!
//! That one overflow declined the entire boot warm pass, so the server warmed
//! NOTHING and every query paid its own lazy first build.

use engram_server::derive_adj_table_max_entries;

const GIB: u64 = 1024 * 1024 * 1024;
/// The old machine-independent constant.
const OLD: u64 = 64 << 20;
/// SF10's total edge count — what the untyped bucket must hold.
const SF10_EDGES: u64 = 176_623_448;
/// SF10's largest single type.
const SF10_LARGEST_TYPE: u64 = 38_405_215;

#[test]
fn the_bench_node_can_hold_sf10s_untyped_bucket() {
    let got = derive_adj_table_max_entries(160 * GIB);
    assert!(
        got >= SF10_EDGES,
        "160 GiB derived {got} entries, which cannot hold SF10's {SF10_EDGES}-edge \
         untyped bucket — the overflow this exists to prevent"
    );
    println!("160 GiB -> {got} entries ({:.1}x SF10's untyped bucket)", got as f64 / SF10_EDGES as f64);
}

#[test]
fn the_old_constant_is_a_floor_so_no_machine_loses_budget() {
    // A small container must not derive LESS than it had, or this "fix" is a
    // regression everywhere it does not help.
    for gib in [1u64, 2, 4, 8, 16] {
        let got = derive_adj_table_max_entries(gib * GIB);
        assert!(
            got >= OLD,
            "{gib} GiB derived {got}, below the old constant {OLD} — a shrinking \
             default is a breaking change"
        );
    }
}

#[test]
fn every_sf10_typed_table_fits_even_the_old_budget() {
    // Why the bucket fix matters independently of the budget: the typed tables
    // were never the problem, and dropping only the overflowing bucket keeps
    // all fifteen.
    const {
        assert!(
            SF10_LARGEST_TYPE < OLD,
            "SF10's largest type already fits the OLD budget; only the untyped aggregate does not"
        )
    };
}

#[test]
fn the_budget_scales_with_the_container() {
    let small = derive_adj_table_max_entries(8 * GIB);
    let large = derive_adj_table_max_entries(256 * GIB);
    assert!(
        large > small,
        "a larger container must derive a larger budget ({large} vs {small})"
    );
    // And the share is bounded: adjacency is one structure among several.
    let bytes = large * 24;
    assert!(
        bytes <= 256 * GIB / 8,
        "the adjacency budget took {bytes} bytes of a 256 GiB container — more \
         than an eighth, leaving too little for cache, memberships and columns"
    );
}
