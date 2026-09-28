//! Fix 79's differential: the maintenance pass prices a repair from the change
//! logs' LENGTHS, and the price it gets must lead to the decision the walk led
//! to wherever the two could disagree.
//!
//! # What was wrong
//!
//! `adj_repair_cost_rows` called `adj_repair_change_set(.., within: None)` —
//! the whole, UNTRUNCATED delta — walked every entry, inserted every changed
//! node into a `BTreeSet`, then kept two integers and dropped the set. All of
//! it under `adj_log`'s lock, which is the lock a writer takes to record its
//! change.
//!
//! Fix 76 turned that from once per pass into once PER STALE TABLE: its shared
//! max-min budget prices every stale table before it serves any of them. On the
//! bench pod that is the half of the story the peak measurement did not show —
//! the worst refresh fell 12,344 -> 1,772 ms while the MEDIAN rose 320 -> 920,
//! and two separate 400 s A/Bs each spent 86-95 s of themselves inside refresh
//! passes on BOTH arms.
//!
//! # Why the cheap price is the same price
//!
//! A delta's distinct-node count is at most its entry count. So when the
//! ENTRIES alone cannot reach `ADJ_REPAIR_MAX` (4,096), none of the three
//! refusals can fire — not the cost gate (`nodes > ADJ_REPAIR_MAX`), not the
//! node cap, not the memory ceiling (`ADJ_REPAIR_MAX_NODES` = `ADJ_LOG_CAP`,
//! larger still). `None` is not a possible answer there, and only the NUMBER is
//! at stake. Past the threshold the exact count decides repair-versus-rebuild
//! and the walk still runs.
//!
//! The number is an UPPER bound: it charges the per-node scan for every entry,
//! as though no two entries touched the same node. Both of the PASS's uses
//! tolerate that in the safe direction — the shared budget hands out
//! `min(cost, share)`, and the lever-off arm's `cost > rows_left` test defers
//! slightly sooner, which is a delay and never a drop.
//! `an_over_estimate_never_costs_a_table_its_repair` is what says so, rather
//! than this comment.
//!
//! # The reader is NOT a consumer of the cheap price — found, not designed
//!
//! The first cut priced every caller cheaply, and `derived_refresh`'s canary
//! (`without_the_refresh_the_next_read_pays_for_the_burst`) failed at once: a
//! single-node reader prices ONCE PER SNAPSHOT and compares the number against
//! `ADJ_READER_REPAIR_MAX_ROWS` to choose between repairing and declining to
//! walk its own span. There an over-estimate is not a delay, it FLIPS the
//! decision — 3,000 entries priced as 3,000 nodes cleared the reader's ceiling
//! and a repair the reader had always taken became a decline. The reader's
//! walk is paid once per snapshot and was never the cost this fix removes, so
//! the reader keeps the exact price; `a_reader_still_prices_exactly` pins it.

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const NODES: u64 = 2_000;
/// Distinct relationship types, each with its own cached table — so the pass
/// prices SEVERAL tables, which is the shape fix 76 multiplied the walk by.
const TYPES: [&str; 4] = ["T0", "T1", "T2", "T3"];
const PER_TYPE: u64 = 800;
/// Changed nodes per type. Well under `ADJ_REPAIR_MAX` (4,096), so every table
/// stays repairable and the cheap price is the one under test.
const BURST: u64 = 500;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A graph with one cached OUT table per type, then a burst that makes every
/// one of them stale but repairable. Deterministic: both arms get the same
/// corpus, which is what makes their reports comparable at all.
fn staled_graph() -> (Graph, Vec<u64>, Vec<u32>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0x2545_F491_4F6C_DD1D);
    for t in TYPES {
        for i in 0..PER_TYPE {
            let dst = ids[(rng.next() % NODES) as usize];
            g.create_rel(ids[i as usize], t, dst, &none).expect("rel");
        }
    }
    g.shared_store().seal();
    let toks: Vec<u32> = TYPES
        .iter()
        .map(|t| g.type_tokens_peek(&[t.to_string()]).expect("minted")[0])
        .collect();
    // Build (and cache) one table per type by reading it. `refresh_stale_derived`
    // refreshes what readers have shown they use and never warms, so a table
    // nothing read is not a candidate and the pass would price nothing.
    for &tok in &toks {
        let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])).len();
    }
    for (ti, t) in TYPES.iter().enumerate() {
        for i in 0..BURST {
            let src = ids[((i + ti as u64 * 7) % NODES) as usize];
            let dst = ids[(rng.next() % NODES) as usize];
            g.create_rel(src, t, dst, &none).expect("burst rel");
        }
    }
    (g, ids, toks)
}

/// Every OUT edge the graph holds, as an answer rather than as a structure.
/// Equality here outranks any tally: it is what a reader would get.
fn edges(g: &Graph, ids: &[u64], toks: &[u32]) -> Vec<(u64, u32, u64)> {
    let mut out = Vec::new();
    for &tok in toks {
        for &id in ids {
            for e in g.adjacent_slim(id, Dir::Out, &Some(vec![tok])) {
                out.push((id, tok, e.peer));
            }
        }
    }
    out.sort_unstable();
    out
}

/// THE MECHANISM AND ITS CONTROL, and the answer-equality that outranks both.
#[test]
fn the_cheap_price_and_the_walked_price_lead_to_the_same_pass() {
    // ── The arm that WALKS to price: the behaviour before fix 79.
    let (walked, w_ids, w_toks) = staled_graph();
    walked.set_cheap_repair_pricing(false);
    let (rep_walk, c_walk) = engram_observe::with_trace(|| walked.refresh_stale_derived());
    let c_walk = c_walk.counters().clone();
    let adj_walk = edges(&walked, &w_ids, &w_toks);

    // ── The arm that prices from the logs' lengths.
    let (cheap, c_ids, c_toks) = staled_graph();
    let (rep_cheap, c_cheap) = engram_observe::with_trace(|| cheap.refresh_stale_derived());
    let c_cheap = c_cheap.counters().clone();
    let adj_cheap = edges(&cheap, &c_ids, &c_toks);

    eprintln!("[fix79] walked: {rep_walk:?}");
    eprintln!("[fix79] cheap:  {rep_cheap:?}");

    // 1. THE SAME PASS. Under the threshold the two prices cannot lead to
    //    different decisions, so every tally in the report must match. A
    //    difference here means the cheap price changed what the pass DID, which
    //    is exactly what `adj_repair_cost_rows`'s argument denies.
    assert_eq!(
        format!("{rep_walk:?}"),
        format!("{rep_cheap:?}"),
        "the cheap price changed what the pass did"
    );

    // 2. THE NON-VACUITY FLOOR. Without it an optimisation that quietly
    //    declined — a lever read wrong, a threshold off by one — passes
    //    assertion 1 by doing exactly what the control does.
    let cheap_hits = *c_cheap
        .get("graph.adjacency repair priced from the log's length")
        .unwrap_or(&0);
    assert!(
        cheap_hits > 0,
        "the cheap arm never took the cheap path, so assertion 1 compares the \
         control against itself: {c_cheap:?}"
    );
    assert_eq!(
        *c_cheap
            .get("graph.adjacency repair priced by walking the change set")
            .unwrap_or(&0),
        0,
        "the cheap arm still walked a change set to price it — every delta here \
         is {BURST} entries, far under ADJ_REPAIR_MAX: {c_cheap:?}"
    );

    // 3. AND THE CONTROL MUST ACTUALLY WALK, for the same reason.
    assert!(
        *c_walk
            .get("graph.adjacency repair priced by walking the change set")
            .unwrap_or(&0)
            > 0,
        "the control arm never priced by walking, so it is not the control: \
         {c_walk:?}"
    );
    assert_eq!(
        *c_walk
            .get("graph.adjacency repair priced from the log's length")
            .unwrap_or(&0),
        0,
        "the control arm took the cheap path with the lever OFF: {c_walk:?}"
    );

    // 4. THE ANSWER. Both arms serve the same graph.
    assert_eq!(
        adj_walk,
        adj_cheap,
        "the two pricing arms converged to DIFFERENT adjacencies — {} edges \
         against {}",
        adj_walk.len(),
        adj_cheap.len()
    );
    assert!(
        !adj_walk.is_empty(),
        "the corpus produced no edges, so this compares two empty answers"
    );
}

/// THE PRICE IS PAID ONCE PER STALE TABLE, WHICH IS WHY IT HAD TO BE CHEAP.
///
/// Fix 76's shared budget prices every stale table before serving any, so with
/// four stale tables the control walks four whole deltas. This pins that the
/// cheap arm walks NONE of them, which is the mechanism's whole claim, and it
/// is the assertion that would fail if a later change reintroduced a walk on
/// the common path.
#[test]
fn the_pass_prices_every_stale_table_and_walks_none_of_them() {
    let (g, _ids, _toks) = staled_graph();
    let (rep, t) = engram_observe::with_trace(|| g.refresh_stale_derived());
    let c = t.counters().clone();
    eprintln!("[fix79] {rep:?}");
    let priced = *c
        .get("graph.adjacency repair priced from the log's length")
        .unwrap_or(&0);
    assert!(
        priced >= TYPES.len() as u64,
        "the pass priced {priced} table(s) cheaply and there are {} stale — a \
         shared budget prices them all: {c:?}",
        TYPES.len()
    );
    assert_eq!(
        *c.get("graph.adjacency repair priced by walking the change set")
            .unwrap_or(&0),
        0,
        "a delta under ADJ_REPAIR_MAX was still walked to price it: {c:?}"
    );
}

/// The cheap price charges the per-node scan for EVERY entry, as though no two
/// entries touched the same node. That is an over-estimate by construction, and
/// this pins that it cannot cost a table its repair.
///
/// The worst case for the estimate is a delta that is one node hit many times:
/// the walk prices it at one node's scan, the cheap path at `BURST` nodes'. The
/// pass must still repair it, and the repaired table must still describe the
/// edges the corpus holds.
#[test]
fn an_over_estimate_never_costs_a_table_its_repair() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    g.create_rel(ids[0], "T0", ids[1], &none).expect("rel");
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["T0".to_string()]).expect("minted")[0];
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])).len();

    // ONE node, many entries: the estimate's worst case.
    for i in 2..(BURST + 2) {
        g.create_rel(ids[0], "T0", ids[i as usize], &none)
            .expect("burst rel");
    }

    let (rep, t) = engram_observe::with_trace(|| g.refresh_stale_derived());
    let c = t.counters().clone();
    eprintln!("[fix79] one-node delta: {rep:?}");

    assert!(
        *c.get("graph.adjacency repair priced from the log's length")
            .unwrap_or(&0)
            > 0,
        "the cheap path did not price this, so the over-estimate is not under \
         test: {c:?}"
    );
    assert!(
        rep.adjacency_repaired > 0,
        "the over-estimate cost the table its repair: {rep:?}"
    );
    let peers = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])).len() as u64;
    assert_eq!(
        peers,
        BURST + 1,
        "the repaired table does not describe the edges the corpus holds"
    );
}

/// THE READER KEEPS THE EXACT PRICE. With the lever ON (the default), a
/// single-node reader that finds its table stale must price the repair by
/// WALKING the change set — the cheap counter must not fire on a read — and it
/// must still repair rather than decline.
///
/// THE CORPUS IS THE CLAIM. `ADJ_READER_REPAIR_MAX_ROWS` is 8,192 rows and the
/// per-node scan is 32, so the two prices straddle the ceiling only when many
/// entries land on FEW nodes: `BURST` (500) entries on ONE node price exactly
/// at 500 + 32 = 532 (repair) and cheaply at 500 + 500 × 32 = 16,500
/// (decline). The first cut of this test used `staled_graph`, whose 500
/// entries come from 500 distinct sources — 16,500 rows on the EXACT price
/// too — and its decline was the reader's correct, pre-existing verdict, not
/// the regression. This is the regression `derived_refresh`'s canary caught
/// (3,000 entries, priced as 3,000 nodes, declined), stated where the
/// mechanism lives.
#[test]
fn a_reader_still_prices_exactly() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    g.create_rel(ids[0], "T0", ids[1], &none).expect("rel");
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["T0".to_string()]).expect("minted")[0];
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])).len();
    for i in 2..(BURST + 2) {
        g.create_rel(ids[0], "T0", ids[i as usize], &none)
            .expect("burst rel");
    }
    // No maintenance pass: the READ is what finds the table stale.
    let (peers, t) =
        engram_observe::with_trace(|| g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])).len());
    let c = t.counters().clone();
    eprintln!("[fix79] reader: {c:?}");
    assert_eq!(
        *c.get("graph.adjacency repair priced from the log's length")
            .unwrap_or(&0),
        0,
        "a READER took the cheap price — an over-estimate there flips \
         repair-or-decline: {c:?}"
    );
    assert!(
        *c.get("graph.adjacency repair priced by walking the change set")
            .unwrap_or(&0)
            > 0,
        "the reader did not price its repair at all, so nothing here is under \
         test: {c:?}"
    );
    assert_eq!(
        *c.get("graph.adjacency stale table declined to a single-node reader")
            .unwrap_or(&0),
        0,
        "the reader DECLINED a {BURST}-entry repair it has always taken: {c:?}"
    );
    assert!(
        *c.get("graph.adjacency tables repaired").unwrap_or(&0) > 0,
        "the reader neither repaired nor declined — which path did it take? {c:?}"
    );
    assert!(peers > 0, "the read returned no peers");
}
