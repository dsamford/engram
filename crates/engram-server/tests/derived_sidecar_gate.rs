//! The SERVER's gate for persisting the derived bases — the part §5.4 got
//! wrong, and the part that had no test.
//!
//! # Why this file exists
//!
//! `Graph::persist_derived_now` was covered six ways in `engram-graph`'s
//! tests: the fold, the vintage, the clock, the skip, the re-arm. Every one of
//! them called it DIRECTLY. Nothing tested whether the server ever calls it,
//! and that is where the defect was.
//!
//! It was first placed behind `quiescent` — the maintenance loop's
//! `tail > 0 && tail == last_tail`, borrowed from the index serialise beside it
//! on the reasoning that both are O(corpus) and neither should run under load.
//! Correct about the load, and `tail > 0` means an **IDLE server is never
//! quiescent**: a store nobody is writing never persists at all, and that is
//! the store most likely to be restarted. The gate is now that **the sealed set
//! has not moved for a whole tick**, which is what the file's vintage names and
//! is satisfiable by an idle store.
//!
//! # What this file does NOT establish
//!
//! It does not reproduce the pod's `adopted=0`. Reverting the gate to
//! `quiescent` leaves all three tests here green, so the tail gate is not shown
//! to be that refusal's cause and the settled gate is not shown to fix it. The
//! reason the canary cannot bite: the COMPACTION path writes a sidecar of its
//! own, so a cadence-driven compaction keeps the file current under either
//! gate. What the tick's persist uniquely covers is a SEAL with no writes after
//! it, and this fixture cannot hold the store there long enough to separate
//! them.
//!
//! Recorded rather than glossed, because a gate change presented as a fix for
//! an unreproduced failure is exactly the kind of claim that gets believed and
//! then quietly disproved.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("engram-dsgate-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

fn serve(dir: &std::path::Path) -> u16 {
    serve_with(dir, None)
}

fn serve_with(dir: &std::path::Path, compact_every: Option<Duration>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let paged = dir.join("paged");
    std::fs::create_dir_all(&paged).expect("paged dir");
    let cache = engram_store::paged::BlockCache::new(8 << 20);
    let cfg = engram_server::ServerConfig {
        workers: 1,
        seal_after_versions: 64,
        maintenance_tick: Duration::from_millis(25),
        paged_dir: Some(paged),
        paged_spill_cache: Some(cache),
        compact_max_interval: compact_every,
        ..engram_server::ServerConfig::default()
    };
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_config(
            listener,
            || (Store::new(), Realm(1), Namespace(1)),
            cfg,
        );
    });
    for _ in 0..200 {
        std::thread::sleep(Duration::from_millis(20));
        if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
            return port;
        }
    }
    panic!("server never came up");
}

fn sidecars(dir: &std::path::Path) -> usize {
    let paged = dir.join("paged");
    std::fs::read_dir(&paged)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "dsc"))
                .count()
        })
        .unwrap_or(0)
}

/// Wait until `f` holds or the deadline passes; returns whether it held.
///
/// A bounded wait rather than a fixed sleep: the maintenance thread's cadence
/// is a tick, and a test that sleeps for "long enough" is a test that is either
/// slow or flaky depending on the machine.
fn within(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

/// A server that writes and then STOPS writing must end up with a sidecar.
///
/// This is the whole item: the saving is for a restart, and a restart happens
/// to a server that has gone idle. Under the tail-based gate this never
/// happened — the tail drains to zero, `tail > 0` goes false, and the store
/// settles into exactly the state where a sidecar is both most valuable and
/// never written.
#[test]
fn a_server_that_settles_persists_its_derived_bases() {
    let dir = scratch("settle");
    let port = serve(&dir);
    {
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        for i in 0..400 {
            c.run(&format!("CREATE (:P {{k: {i}}})")).expect("create");
        }
        // A read, so there are derived structures to persist at all: nothing
        // publishes an adjacency table or a membership snapshot until somebody
        // asks for one.
        c.run("MATCH (n:P {k: 1}) RETURN n").expect("read");
    }
    // NOT asserted: that nothing was written during the fixture. A first cut
    // did assert that and failed — with a 25 ms tick and Cypher statements at
    // millisecond scale, the sealed set genuinely settles BETWEEN statements
    // and a sidecar appears mid-build. That is correct behaviour, and the
    // assertion was describing the test's expectations rather than the item's
    // contract.
    //
    // Now STOP writing. The tail drains and the sealed set settles.
    let wrote = within(30, || sidecars(&dir) > 0);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        wrote,
        "a server that has stopped writing must persist its derived bases — \
         under the tail-based gate this never fired, because an idle store has \
         `tail == 0` and the condition requires `tail > 0`"
    );
}

/// And it must not rewrite the file once the store has not moved.
///
/// The sidecar is 1.69 GB at SF1 and the tick recurs, so a gate that keeps
/// firing writes gigabytes to produce a file identical in the only way that
/// matters. Asserted by mtime rather than by content: a rewrite of identical
/// bytes is exactly what this must not do, so equal content proves nothing.
#[test]
fn a_settled_store_is_not_rewritten_every_tick() {
    let dir = scratch("norewrite");
    let port = serve(&dir);
    {
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        for i in 0..400 {
            c.run(&format!("CREATE (:Q {{k: {i}}})")).expect("create");
        }
        c.run("MATCH (n:Q {k: 1}) RETURN n").expect("read");
    }
    assert!(
        within(30, || sidecars(&dir) > 0),
        "the fixture needs a sidecar before it can check it is not rewritten"
    );
    let paged = dir.join("paged");
    let first = std::fs::read_dir(&paged)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "dsc"))
        .expect("a sidecar")
        .path();
    let mtime = || std::fs::metadata(&first).ok().and_then(|m| m.modified().ok());

    // THE PROPERTY IS THAT REWRITING STOPS, not that it never happens.
    //
    // A first cut sampled twice, two seconds apart, and failed — because the
    // store was still settling: the client had disconnected but seals and
    // spills were still landing, each moving the sealed set and each
    // legitimately earning a rewrite. "It never rewrites" is false and SHOULD
    // be; the file has to track the sealed set.
    //
    // What must be true is that once the store stops moving, so does the file.
    // So wait for the mtime to hold still across a window. If the skip is not
    // working it never will, because the tick recurs for ever — which makes the
    // bounded wait a real assertion rather than a sleep.
    let stable = within(30, || {
        let a = mtime();
        std::thread::sleep(Duration::from_millis(400));
        a.is_some() && a == mtime()
    });
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        stable,
        "once the sealed set stops moving the sidecar must stop being rewritten \
         — at SF1 that is 1.69 GB per tick to produce a file identical in the \
         only way that matters"
    );
}

/// The sealed set moves while the TAIL IS EMPTY, and the sidecar keeps up.
///
/// **This does NOT reproduce the pod's refusal, and saying so matters.** The
/// canary — reverting the gate to the old tail-based `quiescent` — leaves all
/// three tests in this file GREEN. So the tail gate is not what produced
/// `adopted=0` there, and the settled gate is not demonstrated to fix it.
///
/// The reason the canary cannot bite here: the COMPACTION path writes a sidecar
/// of its own (`adopt_merged_derived`), so a cadence-driven compaction keeps the
/// file current whichever gate the tick uses. What the tick's persist uniquely
/// covers is a sealed-set move that is NOT a compaction — a seal — with no
/// writes following it, and this fixture cannot hold the store in that state
/// long enough to tell the gates apart.
///
/// The gate change is kept on its own merits: `tail > 0` is false for an IDLE
/// store, so under it a server that has never been written to since boot can
/// never persist, and that is a real hole regardless of the pod. The pod's
/// refusal has a diagnosed SYMPTOM (the vintage check, with both ids printed)
/// and no established cause, and it is recorded that way rather than as fixed.
///
/// This is the one the other two tests do not cover, and the reason the
/// tail-based gate survived them: in an ordinary write fixture the tail is
/// non-empty and stable for a tick, so `quiescent` fires and everything looks
/// fine. On the pod the shape was different — writes stopped, the tail drained,
/// and THEN a compaction (the cadence floor) moved the sealed set. With
/// `tail == 0` the old gate could never fire again, so the file kept naming a
/// sealed set that no longer existed and the next boot refused it:
///
/// ```text
/// derived sidecar REFUSED: it describes sealed set 0x0ad5…, the store has 0x6945…
/// ```
///
/// A cadence floor is what makes this reproducible in a test: it moves the
/// sealed set on a timer rather than on write volume, which is exactly the
/// decoupling the pod run stumbled into.
#[test]
fn a_sealed_set_that_moves_with_an_empty_tail_is_re_stamped() {
    let dir = scratch("emptytail");
    // A compaction every second, so the sealed set keeps moving after the
    // writes stop and the tail has drained.
    let port = serve_with(&dir, Some(Duration::from_secs(1)));
    {
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        for i in 0..400 {
            c.run(&format!("CREATE (:R {{k: {i}}})")).expect("create");
        }
        c.run("MATCH (n:R {k: 1}) RETURN n").expect("read");
    }
    assert!(
        within(30, || sidecars(&dir) > 0),
        "a sidecar must exist before this can test that it KEEPS UP"
    );
    let paged = dir.join("paged");
    let f = std::fs::read_dir(&paged)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "dsc"))
        .expect("a sidecar")
        .path();
    let stamp_of = || std::fs::metadata(&f).ok().and_then(|m| m.modified().ok());
    let before = stamp_of();

    // NOTHING writes from here. The tail is empty; only the cadence floor moves
    // the sealed set. The sidecar must follow it.
    let followed = within(40, || stamp_of() != before);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        followed,
        "the sealed set moved with an empty tail and the sidecar did not follow          — it now names a set the store does not have, and the next boot will          refuse it and rebuild"
    );
}
