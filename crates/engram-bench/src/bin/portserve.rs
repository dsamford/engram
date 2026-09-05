//! `portserve <export dir> [bind addr]` — load the production export the
//! port benchmark uses and serve it over Bolt, so the assistant's own
//! retrieval statements can be measured end to end against the same
//! world (the cutover measurement). The load happens INSIDE the engine's
//! factory, on the engine thread: the store handle is single-threaded by
//! design and never crosses one.
use std::net::TcpListener;

// Thread-caching allocator on musl (same as the production `engram-server`): the
// multi-worker harness serves the concurrency benchmark, and the single-arena
// system allocator's lock caps concurrent multi-hop queries. mimalloc lifts it
// (foaf 32→18 collapse becomes 73→324, a 4.4× scale). musl-only.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn main() -> std::io::Result<()> {
    let dir = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/work".to_string()),
    );
    let addr = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "0.0.0.0:7687".to_string());
    let listener = TcpListener::bind(&addr)?;
    eprintln!(
        "[portserve] listening on bolt://{} (loading {} on the engine thread)",
        listener.local_addr()?,
        dir.display()
    );
    // A/B switches for the stress lane, read HERE rather than in the engine —
    // the engine takes its configuration from its caller, and a benchmark that
    // wants a pre-fix arm asks for one explicitly.
    //
    // `ENGRAM_BENCH_BASELINE=1` turns off both families of fix at once
    // (selectivity-based anchor choice, incremental cache maintenance), so one
    // run on one host produces the before and the after. Without it a
    // before/after comparison depends on remembering which machine each half
    // was measured on, which is how a performance claim quietly stops meaning
    // anything.
    let baseline = std::env::var_os("ENGRAM_BENCH_BASELINE").is_some();
    // Through `configure_graph`, NOT on the graph built below.
    //
    // The graph built in this closure loads the export and is then dropped —
    // `run_server` is handed a `Store`, and every graph that serves a query is
    // constructed later by the resolver. Setting the toggles on the loading
    // graph left both A/B arms running the identical engine, and the two
    // sweeps came out within 2% of each other. That looks precisely like a
    // fix that does nothing, which is the most expensive kind of wrong number.
    let mut cfg = engram_server::ServerConfig::from_env();
    if baseline {
        cfg.configure_graph = Some(std::sync::Arc::new(|g: &Graph| {
            g.set_selective_anchor(false);
            g.set_incremental_caches(false);
            // Printed from INSIDE the hook, and reporting what the graph says
            // rather than what was asked for. A banner printed next to the env
            // lookup proves only that the variable was set — which is exactly
            // what the discarded-configuration version printed while serving a
            // fully-fixed engine.
            eprintln!(
                "[portserve] BASELINE ARM applied to a serving graph: \
                 selective_anchor={}, incremental_caches={} \
                 — this is the PRE-FIX engine, not the shipping one",
                g.selective_anchor_enabled(),
                g.incremental_caches_enabled()
            );
        }));
    }
    engram_server::run_server_with_config(
        listener,
        move || {
        let graph = Graph::new(Store::new(), Realm(1), Namespace(1));
        let stats = engram_bench::load_export(&graph, &dir);
        eprintln!(
            "[portserve] world loaded: {} nodes, {} rels in {} ms; sealing and compacting",
            stats.nodes, stats.rels, stats.load_ms
        );
        let store = graph.shared_store();
        store.seal();
        let (blocks, rows) = store.compact();
        eprintln!("[portserve] compacted: {blocks} blocks / {rows} rows; serving");
        (store, Realm(1), Namespace(1))
        },
        cfg,
    )
}
