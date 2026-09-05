#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! Paged serving mode end-to-end: a server over [`Store::open_paged_dir`] with
//! a shared spill cache must (1) spill sealed segments to seg files while
//! serving, (2) drain a quiescent tail to disk once writes stop, and (3) leave
//! seg files a durable open serves in full — the bigger-than-RAM contract.
//! There is no WAL in this mode; the seg files are the only durability.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engram_bolt::client::Client;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_server::ServerConfig;
use engram_store::Store;

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "engram-paged-serving-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

fn seg_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("seg-") && n.ends_with(".seg"))
                })
                .count()
        })
        .unwrap_or(0)
}

/// Copy the seg files into `copy` and count that copy's live nodes through a
/// durable open — exactly what a RESTARTED server would serve. Seg files are
/// written tmp+rename, so a file visible under its final name is complete;
/// but compaction UNLINKS the inputs it merged once their replacement is in
/// place, so a listing can go stale between `read_dir` and `copy`. A file
/// that vanished mid-copy means the listing predates a compaction whose
/// output the listing may not hold either: start the copy over from a fresh
/// listing rather than serve a partial one. (A consistent hot copy of a
/// live store is what a checkpoint is for; this helper is not one.)
fn nodes_in_copy(dir: &Path, copy: &Path) -> usize {
    'listing: for _ in 0..50 {
        for entry in std::fs::read_dir(dir).expect("read spill dir").flatten() {
            let name = entry.file_name();
            let Some(n) = name.to_str() else { continue };
            if !(n.starts_with("seg-") && n.ends_with(".seg")) {
                continue;
            }
            match std::fs::copy(entry.path(), copy.join(&name)) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Compacted away since the listing: everything copied so
                    // far may be a superseded generation. Re-list.
                    let _ = std::fs::remove_dir_all(copy);
                    std::fs::create_dir_all(copy).expect("recreate copy dir");
                    std::thread::sleep(Duration::from_millis(50));
                    continue 'listing;
                }
                Err(e) => panic!("copy seg {n}: {e}"),
            }
        }
        let (store, _cache) = Store::open_paged_dir(copy, 4 << 20).expect("open copy");
        return Graph::new(store, Realm(1), Namespace(1)).warm().nodes;
    }
    panic!("50 listings of the spill dir in a row went stale mid-copy");
}

/// The highest segment seq on disk. Unlike the file COUNT this is monotone:
/// a spill always names a seq above every existing one, while compaction
/// merges files away (and unlinks what it merged), so "more files than
/// before" stopped being evidence that anything was spilled.
fn max_seq(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name();
                    let n = name.to_str()?;
                    n.strip_prefix("seg-")?.strip_suffix(".seg")?.parse::<u64>().ok()
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

fn connect(addr: &str) -> Client {
    for _ in 0..100 {
        if let Ok(c) = Client::connect(addr) {
            return c;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server never became reachable");
}

#[test]
fn paged_mode_spills_while_serving_drains_the_quiet_tail_and_reopens_in_full() {
    let dir = tmp("store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    // The SAME cache handle the store reads through goes into the config, so
    // every spill shares the one budget — the contract `paged_spill_cache`
    // documents. Small, so paged reads genuinely fault in.
    let (store, cache) = Store::open_paged_dir(&dir, 4 << 20).expect("open empty paged dir");
    let cfg = ServerConfig {
        workers: 1,
        seal_after_versions: 64,
        paged_dir: Some(dir.clone()),
        paged_spill_cache: Some(cache),
        ..ServerConfig::default()
    };
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_config(
            listener,
            move || (store, Realm(1), Namespace(1)),
            cfg,
        );
    });
    let mut c = connect(&addr);

    // Load past the seal threshold repeatedly: each worker seal must ask the
    // maintenance thread, whose spill writes seg files while the server keeps
    // serving.
    let mut written: u64 = 0;
    for i in 0..500u32 {
        c.run(&format!("CREATE (:P {{k: {i}}})")).expect("create");
        written += 1;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while seg_count(&dir) == 0 {
        assert!(
            Instant::now() < deadline,
            "no spill within 30 s of 500 writes at seal_after=64 — the worker's \
             seal never reached the maintenance thread's spill path"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let max_seq_at_load = max_seq(&dir);

    // Reads must stay correct while the store is MIXED resident/paged.
    for i in 500..510u32 {
        c.run(&format!("CREATE (:P {{k: {i}}})")).expect("create");
        written += 1;
    }
    assert_eq!(
        c.run("MATCH (n:P) RETURN id(n)").expect("count"),
        written,
        "mixed resident/paged read lost or duplicated nodes"
    );

    // Quiescent-tail drain: writes stop, and the tick must seal + spill the
    // final partial tail. `tail_versions` is not observable over Bolt, so the
    // drain is asserted through what it exists for: the seg files ALONE
    // eventually account for every written node (a copy served in full by a
    // durable open), while the live server stays correct the whole time.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let copy = tmp(&format!("drain-{attempt}"));
        let on_disk = nodes_in_copy(&dir, &copy);
        let _ = std::fs::remove_dir_all(&copy);
        if on_disk == written as usize {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "tail never drained: {on_disk} of {written} node(s) on disk after 30 s quiet"
        );
        assert_eq!(
            c.run("MATCH (n:P) RETURN id(n)").expect("count"),
            written,
            "live reads went wrong while the tail drained"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
    // The quiet period itself must have spilled: either the tick sealed the
    // partial tail, or a phase-2 seal spilled — both name a seq above every
    // segment the load phase had. (Not the file count: compaction may have
    // merged the load's files into fewer by now.)
    assert!(
        max_seq(&dir) > max_seq_at_load,
        "no segment was spilled after the load phase (max seq {} then and now) —          the drain path never ran",
        max_seq_at_load
    );

    // Reopen: a copy taken AFTER the drain holds everything, byte-complete.
    let copy = tmp("reopen");
    assert_eq!(
        nodes_in_copy(&dir, &copy),
        written as usize,
        "a durable open of the copied seg files serves a different node count"
    );
    let _ = std::fs::remove_dir_all(&copy);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `CALL engram.checkpoint()` makes the tail durable NOW — the reply is the
/// claim, not a sleep. The canary comes first: with the seal threshold out of
/// reach and the maintenance tick an hour away, a copy of the directory
/// serves NOTHING of what was written, which is exactly what the SF3 loader
/// found twice (742,615 then 92,910 relationships, the LAST groups sent, gone
/// with a `kill -9` 25 s after the load). Then the checkpoint, and the copy
/// serves every node.
#[test]
fn a_checkpoint_puts_the_tail_on_disk_and_says_so() {
    let dir = tmp("checkpoint");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (store, cache) = Store::open_paged_dir(&dir, 4 << 20).expect("open empty paged dir");
    let cfg = ServerConfig {
        workers: 1,
        seal_after_versions: 1_000_000,
        maintenance_tick: Duration::from_secs(3600),
        paged_dir: Some(dir.clone()),
        paged_spill_cache: Some(cache),
        ..ServerConfig::default()
    };
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_config(
            listener,
            move || (store, Realm(1), Namespace(1)),
            cfg,
        );
    });
    let mut c = connect(&addr);
    for i in 0..300u32 {
        c.run(&format!("CREATE (:P {{k: {i}}})")).expect("create");
    }
    assert_eq!(c.run("MATCH (n:P) RETURN id(n)").expect("live count"), 300);

    // The canary: nothing sealed, nothing spilled — a copy serves nothing.
    let copy = tmp("checkpoint-before");
    assert_eq!(
        nodes_in_copy(&dir, &copy),
        0,
        "the tail reached disk on its own — then this test cannot show the checkpoint did it"
    );
    let _ = std::fs::remove_dir_all(&copy);

    let rows = c
        .query(
            "CALL engram.checkpoint() YIELD spilled, segments, resident, tail \
             RETURN [spilled, segments, resident, tail]",
        )
        .expect("checkpoint");
    // The client keeps the first column of each row; the RECORD's field list
    // arrives as a list, so the projected list sits one level down.
    let [engram_cypher::Value::List(outer)] = rows.as_slice() else {
        panic!("one row expected, got {rows:?}");
    };
    let fields: &Vec<engram_cypher::Value> = match outer.as_slice() {
        [engram_cypher::Value::List(inner)] => inner,
        _ => outer,
    };
    let n = |i: usize| match &fields[i] {
        engram_cypher::Value::Int(v) => *v,
        other => panic!("field {i} is {other:?}"),
    };
    assert!(n(0) >= 1, "the checkpoint spilled nothing: {fields:?}");
    assert_eq!(n(1), n(0), "sealed {} but {} spilled by this call: {fields:?}", n(1), n(0));
    assert_eq!(n(2), 0, "a sealed segment is still resident after the checkpoint: {fields:?}");
    assert_eq!(n(3), 0, "the tail is not empty after the checkpoint: {fields:?}");

    // Now the copy serves every node — the durable claim, checked by a
    // durable open rather than by the reply alone.
    let copy = tmp("checkpoint-after");
    assert_eq!(nodes_in_copy(&dir, &copy), 300, "the checkpoint's reply said durable and the disk disagrees");
    let _ = std::fs::remove_dir_all(&copy);

    // Idempotent: nothing new to seal, nothing to spill, still durable.
    let again = c
        .query("CALL engram.checkpoint() YIELD spilled, tail RETURN [spilled, tail]")
        .expect("second checkpoint");
    let flat = format!("{again:?}");
    assert!(
        flat.contains("Int(0), Int(0)"),
        "a checkpoint with nothing to do must say so: {flat}"
    );
    // And the live server is unaffected.
    assert_eq!(c.run("MATCH (n:P) RETURN id(n)").expect("live count"), 300);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A resident server has no paged directory: the procedure must REFUSE, not
/// answer "durable" about a store whose durability is its WAL.
#[test]
fn a_resident_server_refuses_the_checkpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let cfg = ServerConfig {
        workers: 1,
        ..ServerConfig::default()
    };
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_config(
            listener,
            move || (Store::new(), Realm(1), Namespace(1)),
            cfg,
        );
    });
    let mut c = connect(&addr);
    let err = c
        .query("CALL engram.checkpoint() YIELD tail RETURN tail")
        .expect_err("a resident server must refuse the checkpoint");
    assert!(
        err.to_string().contains("not served from a paged store"),
        "refusal must say why: {err}"
    );
}
