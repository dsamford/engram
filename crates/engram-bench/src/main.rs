//! The scalability + performance harness.
//!
//! Adapter-class (wall clocks allowed HERE, nowhere in the engine): sized
//! runs over every layer, per-op costs, and GROWTH SHAPES — a point read
//! whose cost grows with the corpus is a design failure the absolute number
//! alone would hide, so sizes are swept and the ratios are checked against
//! generous bounds (reported as SHAPE WARN, never a hard failure: absolute
//! wall time on a dev box is a report, not a gate).
//!
//! Output: a human table on stdout and `measurements/baseline.json` in the
//! repo — the committed record the next run compares against by eye and the
//! plan cites by number. Run with `--release`; a debug build prints a loud
//! warning into both.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::time::Instant;

use engram_bolt::{BoltServer, Pack};
use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, VectorArm, run_stmt};
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Replica, Store, StoredValue};

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Measurement {
    suite: &'static str,
    name: String,
    size: usize,
    iters: u64,
    total_ns: u128,
}

impl Measurement {
    fn ns_per_op(&self) -> f64 {
        self.total_ns as f64 / self.iters as f64
    }

    fn ops_per_sec(&self) -> f64 {
        1e9 / self.ns_per_op()
    }
}

struct Bench {
    out: Vec<Measurement>,
}

impl Bench {
    fn time<R>(
        &mut self,
        suite: &'static str,
        name: impl Into<String>,
        size: usize,
        iters: u64,
        f: impl FnOnce() -> R,
    ) -> R {
        let start = Instant::now();
        let r = f();
        let total_ns = start.elapsed().as_nanos();
        self.out.push(Measurement {
            suite,
            name: name.into(),
            size,
            iters,
            total_ns,
        });
        r
    }

    fn find(&self, name: &str, size: usize) -> Option<&Measurement> {
        self.out.iter().find(|m| m.name == name && m.size == size)
    }
}

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

// ─── Suites ─────────────────────────────────────────────────────────────────

fn bench_store(b: &mut Bench) {
    for &n in &[1_000usize, 10_000, 100_000] {
        let store = Store::new();
        let keys: Vec<[u8; 8]> = (0..n as u64).map(|i| splitmix(i).to_be_bytes()).collect();
        b.time("store", "put fresh", n, n as u64, || {
            for k in &keys {
                store
                    .put(&prefix(), k, StoredValue::Plain(k.to_vec()))
                    .expect("put");
            }
        });
        let sample: Vec<&[u8; 8]> = keys.iter().step_by((n / 1_000).max(1)).collect();
        let reads = 100_000u64;
        b.time("store", "get point", n, reads, || {
            let mut hits = 0usize;
            for i in 0..reads {
                let k = sample[(i as usize) % sample.len()];
                if store.get(&prefix(), k).is_some() {
                    hits += 1;
                }
            }
            assert_eq!(hits, reads as usize);
        });
        b.time("store", "overwrite", n, sample.len() as u64, || {
            for k in &sample {
                store
                    .put(&prefix(), *k, StoredValue::Plain(vec![0xFF]))
                    .expect("put");
            }
        });
        b.time("store", "scan full", n, n as u64, || {
            assert_eq!(store.scan(&prefix()).len(), n);
        });
        if n == 100_000 {
            let entries = store.log_tail(0);
            b.time(
                "store",
                "recover (log redo)",
                n,
                entries.len() as u64,
                || {
                    let restored = Store::recover(&entries).expect("recover");
                    assert_eq!(restored.log_len(), entries.len() as u64);
                },
            );
            b.time("replica", "apply stream", n, entries.len() as u64, || {
                let mut r = Replica::new();
                r.apply(&entries).expect("apply");
            });
        }
    }
}

fn bench_columnar(b: &mut Bench) {
    use engram_key::value::Tag;
    use engram_store::record::{PropertyId, Record};
    let int64 = |v: i64| -> Vec<u8> {
        let mut out = vec![Tag::INT64.byte()];
        out.extend_from_slice(&v.to_le_bytes());
        out
    };
    let string = |s: &str| -> Vec<u8> {
        let mut out = vec![Tag::STRING.byte()];
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
        out
    };
    // The head/tail layout's measured claim: projecting ONE property across
    // a signature-homogeneous population from column blocks versus decoding
    // whole records. Rows carry a head-shape record (id, name, score) so the
    // row path pays what it really pays.
    for &n in &[10_000usize, 100_000] {
        let store = Store::new();
        for i in 0..n as u64 {
            let mut r = Record::new();
            r.set(PropertyId(1), int64(i as i64));
            r.set(PropertyId(2), string(&format!("node-{i}")));
            r.set(PropertyId(3), int64(splitmix(i) as i64));
            // The wide payload the census head actually carries (embeddings,
            // document bodies): the row path must clone it on every scan,
            // the column path never touches it.
            r.set(PropertyId(4), string(&"x".repeat(1024)));
            store
                .put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(r.encode()))
                .expect("put");
        }
        store.seal().expect("seal");
        store.compact();
        let (blocks, rows) = store.columnar_stats();
        assert!(
            blocks >= 1 && rows == n,
            "the head must block: {blocks} blocks, {rows} rows"
        );

        b.time("columnar", "row scan (whole records)", n, n as u64, || {
            assert_eq!(store.scan_at(&prefix(), u64::MAX).len(), n);
        });
        b.time(
            "columnar",
            "column scan (one property)",
            n,
            n as u64,
            || {
                assert_eq!(store.scan_column_at(&prefix(), &[], 1, u64::MAX).len(), n);
            },
        );
    }
}

fn bench_adversarial(b: &mut Bench) {
    use engram_key::value::Tag;
    use engram_store::record::{PropertyId, Record};
    let int64 = |v: i64| -> Vec<u8> {
        let mut out = vec![Tag::INT64.byte()];
        out.extend_from_slice(&v.to_le_bytes());
        out
    };

    // ── Interleaved 50/50 read+write storm over a hot working set ──────
    for &n in &[10_000usize, 100_000] {
        let store = Store::new();
        for i in 0..10_000u64 {
            store
                .put(
                    &prefix(),
                    &i.to_be_bytes(),
                    StoredValue::Plain(int64(i as i64)),
                )
                .expect("seed");
        }
        b.time(
            "adversarial",
            "interleaved 50/50 read+write",
            n,
            n as u64,
            || {
                let mut hits = 0usize;
                for op in 0..n as u64 {
                    let k = (splitmix(op) % 10_000).to_be_bytes();
                    if op % 2 == 0 {
                        store
                            .put(&prefix(), &k, StoredValue::Plain(int64(op as i64)))
                            .expect("put");
                    } else if store.get(&prefix(), &k).is_some() {
                        hits += 1;
                    }
                }
                assert!(hits > 0, "reads must actually hit");
            },
        );
    }

    // ── Tiny reads against a segment PILE (read amplification) ─────────
    // The same 32k keys once in a single sealed segment, once smeared
    // across 64 generations of seals, then compacted back to one.
    let n_keys = 32_768u64;
    let reads = 100_000u64;
    let piled = Store::new();
    for generation in 0..64u64 {
        for i in (generation * n_keys / 64)..((generation + 1) * n_keys / 64) {
            piled
                .put(
                    &prefix(),
                    &i.to_be_bytes(),
                    StoredValue::Plain(int64(i as i64)),
                )
                .expect("put");
        }
        piled.seal();
    }
    let flat = Store::new();
    for i in 0..n_keys {
        flat.put(
            &prefix(),
            &i.to_be_bytes(),
            StoredValue::Plain(int64(i as i64)),
        )
        .expect("put");
    }
    flat.seal();
    let point_reads = |store: &Store| {
        let mut hits = 0usize;
        for r in 0..reads {
            let k = (splitmix(r ^ 0xAD) % n_keys).to_be_bytes();
            if store.get(&prefix(), &k).is_some() {
                hits += 1;
            }
        }
        assert_eq!(hits, reads as usize);
    };
    b.time(
        "adversarial",
        "tiny reads vs 1 segment",
        n_keys as usize,
        reads,
        || {
            point_reads(&flat);
        },
    );
    b.time(
        "adversarial",
        "tiny reads vs 64 segments",
        n_keys as usize,
        reads,
        || {
            point_reads(&piled);
        },
    );
    b.time(
        "adversarial",
        "scan vs 64 segments",
        n_keys as usize,
        n_keys,
        || {
            assert_eq!(piled.scan_at(&prefix(), u64::MAX).len(), n_keys as usize);
        },
    );
    piled.compact();
    b.time(
        "adversarial",
        "tiny reads after compaction",
        n_keys as usize,
        reads,
        || {
            point_reads(&piled);
        },
    );
    b.time(
        "adversarial",
        "scan after compaction",
        n_keys as usize,
        n_keys,
        || {
            assert_eq!(piled.scan_at(&prefix(), u64::MAX).len(), n_keys as usize);
        },
    );

    // ── Large sequential ingest with a real seal cadence ───────────────
    for &n in &[100_000usize, 500_000] {
        let store = Store::new();
        b.time(
            "adversarial",
            "sequential ingest (seal every 50k)",
            n,
            n as u64,
            || {
                for i in 0..n as u64 {
                    store
                        .put(
                            &prefix(),
                            &i.to_be_bytes(),
                            StoredValue::Plain(int64(i as i64)),
                        )
                        .expect("put");
                    if i % 50_000 == 49_999 {
                        store.seal();
                    }
                }
            },
        );
    }

    // ── A hot key under a 100k-version chain, then retirement ──────────
    // The BUILD is timed at two sizes: the full-DB port load found puts on
    // a hot key going O(chain) (insert-at-front memmoved the whole chain,
    // ~2.5 ms per put by 200k versions). Append must stay flat.
    for &n in &[10_000i64, 100_000] {
        let s = Store::new();
        b.time(
            "adversarial",
            "hot put (chain build)",
            n as usize,
            n as u64,
            || {
                for v in 0..n {
                    s.put(&prefix(), b"hot", StoredValue::Plain(int64(v)))
                        .expect("put");
                }
            },
        );
    }
    let hot = Store::new();
    for v in 0..100_000i64 {
        hot.put(&prefix(), b"hot", StoredValue::Plain(int64(v)))
            .expect("put");
    }
    b.time(
        "adversarial",
        "hot get (100k-version chain)",
        100_000,
        10_000,
        || {
            for _ in 0..10_000 {
                assert!(hot.get(&prefix(), b"hot").is_some());
            }
        },
    );
    hot.seal();
    hot.compact();
    b.time(
        "adversarial",
        "hot get (after retirement)",
        100_000,
        10_000,
        || {
            for _ in 0..10_000 {
                assert!(hot.get(&prefix(), b"hot").is_some());
            }
        },
    );

    // ── A tombstone graveyard, scanned before and after the purge ──────
    let grave = Store::new();
    let g_n = 50_000u64;
    for i in 0..g_n {
        grave
            .put(
                &prefix(),
                &i.to_be_bytes(),
                StoredValue::Plain(int64(i as i64)),
            )
            .expect("put");
    }
    for i in 0..g_n {
        if i % 10 != 0 {
            grave.delete(&prefix(), &i.to_be_bytes());
        }
    }
    let live = g_n as usize / 10;
    b.time(
        "adversarial",
        "graveyard scan (90% tombstoned)",
        g_n as usize,
        live as u64,
        || {
            assert_eq!(grave.scan_at(&prefix(), u64::MAX).len(), live);
        },
    );
    grave.seal();
    grave.compact();
    b.time(
        "adversarial",
        "graveyard scan (after purge)",
        g_n as usize,
        live as u64,
        || {
            assert_eq!(grave.scan_at(&prefix(), u64::MAX).len(), live);
        },
    );

    // ── The columnar worst case: every row a unique signature ──────────
    for &(label, unique) in &[
        ("compact same-signature", false),
        ("compact unique-signatures", true),
    ] {
        let store = Store::new();
        let n = 50_000u32;
        for i in 0..n {
            let mut r = Record::new();
            let pid = if unique { 1_000 + i } else { 1 };
            r.set(PropertyId(pid), int64(i64::from(i)));
            store
                .put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(r.encode()))
                .expect("put");
        }
        store.seal();
        b.time("adversarial", label, n as usize, n as u64, || {
            store.compact();
        });
        let (blocks, rows) = store.columnar_stats();
        if unique {
            assert_eq!((blocks, rows), (0, 0), "unique signatures must not block");
        } else {
            assert_eq!(rows, n as usize, "one signature must block fully");
        }
    }
}

fn seeded_graph(nodes: usize, fan_out: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..nodes {
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), Value::Int(i as i64));
        props.insert("bucket".to_string(), Value::Int((i % 100) as i64));
        props.insert("name".to_string(), Value::Str(format!("node-{i}")));
        g.create_node(&["N".to_string()], &props).expect("node");
    }
    for i in 0..nodes {
        for f in 1..=fan_out {
            let dst = (splitmix((i * fan_out + f) as u64) as usize) % nodes;
            let _ = g.create_rel((i + 1) as u64, "R", (dst + 1) as u64, &BTreeMap::new());
        }
    }
    g
}

fn bench_graph(b: &mut Bench) {
    for &n in &[1_000usize, 10_000, 50_000] {
        let g = b.time(
            "graph",
            "create nodes+rels (fan 4)",
            n,
            (n * 5) as u64,
            || seeded_graph(n, 4),
        );
        let hops = 20_000u64;
        b.time("graph", "rels_of (1-hop step)", n, hops, || {
            let mut edges = 0usize;
            for i in 0..hops {
                let node = (splitmix(i) as usize % n) as u64 + 1;
                edges += g
                    .rels_of(node, engram_graph::Dir::Out, None)
                    .expect("rels")
                    .len();
            }
            assert!(edges > 0);
        });
        b.time("graph", "label scan", n, n as u64, || {
            assert_eq!(g.nodes_by_label(Some("N")).expect("scan").len(), n);
        });
    }
}

fn bench_cypher(b: &mut Bench) {
    // Parse throughput over a corpus-shaped mix.
    let statements = [
        "MATCH (n:User {id: $id})-[:OWNS]->(m:Doc) WHERE m.title CONTAINS 'x' RETURN n, m ORDER BY m.title DESC LIMIT 10",
        "MERGE (n:User {id: $id}) ON CREATE SET n.created = timestamp() ON MATCH SET n.seen = timestamp()",
        "MATCH (a)-[:R*1..3]->(b) WHERE b.v > 10 RETURN count(*) AS c",
        "UNWIND $items AS item CREATE (:Row {v: item})",
        "MATCH (n:E) WITH n.k AS k, count(*) AS c WHERE c > 1 RETURN k, c ORDER BY c DESC",
        "MATCH (n) WHERE EXISTS { (n)-[:X]->(:Y) } AND n.at > datetime() - duration('P30D') RETURN n",
    ];
    let iters = 20_000u64;
    b.time(
        "cypher",
        "parse (mixed corpus shapes)",
        statements.len(),
        iters,
        || {
            for i in 0..iters {
                let s = statements[(i as usize) % statements.len()];
                parse_any(s).expect("parses");
            }
        },
    );

    for &n in &[1_000usize, 10_000] {
        let g = seeded_graph(n, 4);
        let filter = parse_statement("MATCH (n:N {bucket: 7}) RETURN n.i ORDER BY n.i LIMIT 20")
            .expect("parses");
        let q_iters = 200u64;
        b.time("cypher", "match+filter+sort e2e", n, q_iters, || {
            for _ in 0..q_iters {
                let r = engram_graph::run_query(&g, &filter, BTreeMap::new()).expect("run");
                assert!(!r.rows.is_empty());
            }
        });
        let agg = parse_statement(
            "MATCH (n:N) RETURN n.bucket AS b, count(*) AS c ORDER BY c DESC LIMIT 5",
        )
        .expect("parses");
        let a_iters = 20u64;
        b.time("cypher", "group-aggregate e2e", n, a_iters, || {
            for _ in 0..a_iters {
                let r = engram_graph::run_query(&g, &agg, BTreeMap::new()).expect("run");
                assert_eq!(r.rows.len(), 5);
            }
        });
        let hop = parse_statement("MATCH (a:N {i: 1})-[:R]->(b)-[:R]->(c) RETURN count(c) AS n")
            .expect("parses");
        let h_iters = 500u64;
        b.time("cypher", "2-hop traversal e2e", n, h_iters, || {
            for _ in 0..h_iters {
                engram_graph::run_query(&g, &hop, BTreeMap::new()).expect("run");
            }
        });
    }
}

fn bench_vector(b: &mut Bench) {
    const DIM: usize = 128;
    let make = |n: usize| -> Graph {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        run_stmt(
            &g,
            &parse_any("CREATE VECTOR INDEX vi FOR (v:V) ON (v.e)").expect("p"),
            BTreeMap::new(),
        )
        .expect("index");
        for i in 0..n {
            let v: Vec<Value> = (0..DIM)
                .map(|d| {
                    let r = splitmix((i * DIM + d) as u64);
                    Value::Float(((r >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0)
                })
                .collect();
            let mut props = BTreeMap::new();
            props.insert("e".to_string(), Value::List(v));
            g.create_node(&["V".to_string()], &props).expect("node");
        }
        g
    };
    let query = |qi: u64| -> Vec<f64> {
        (0..DIM)
            .map(|d| {
                let r = splitmix(1_000_000 + qi * DIM as u64 + d as u64);
                ((r >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
            })
            .collect()
    };

    for &n in &[1_000usize, 10_000, 50_000] {
        let g = make(n);
        g.set_vector_exact_max(1_000_000);
        let iters = if n >= 50_000 { 20u64 } else { 50 };
        b.time("vector", "exact scan k=10", n, iters, || {
            for qi in 0..iters {
                let (hits, plan) = g.vector_query("vi", 10, &query(qi)).expect("q");
                assert_eq!(plan.arm, VectorArm::Exact);
                assert_eq!(hits.len(), 10);
            }
        });
        if n >= 10_000 {
            g.set_vector_exact_max(100);
            b.time("vector", "ann build (hnsw)", n, n as u64, || {
                let (_, plan) = g.vector_query("vi", 10, &query(0)).expect("q");
                assert_eq!(plan.arm, VectorArm::Ann { rebuilt: true });
            });
            let q_iters = 200u64;
            b.time("vector", "ann query k=10", n, q_iters, || {
                for qi in 0..q_iters {
                    let (hits, plan) = g.vector_query("vi", 10, &query(qi)).expect("q");
                    assert_eq!(plan.arm, VectorArm::Ann { rebuilt: false });
                    assert_eq!(hits.len(), 10);
                }
            });
            // Recall at SCALE — the measurement the unit-test canary limit
            // defers to (inverted pruning is only visible up here).
            let mut hit = 0usize;
            let mut total = 0usize;
            for qi in 0..30u64 {
                let q = query(500 + qi);
                g.set_vector_exact_max(1_000_000);
                let (exact, _) = g.vector_query("vi", 10, &q).expect("exact");
                g.set_vector_exact_max(100);
                let (ann, _) = g.vector_query("vi", 10, &q).expect("ann");
                let want: Vec<String> = exact.iter().map(|(v, _)| format!("{v:?}")).collect();
                total += want.len();
                hit += ann
                    .iter()
                    .filter(|(v, _)| want.contains(&format!("{v:?}")))
                    .count();
            }
            let recall = hit as f64 / total as f64;
            b.out.push(Measurement {
                suite: "vector",
                name: format!("ann recall@10 x1000 (={recall:.3})"),
                size: n,
                iters: 1000,
                total_ns: (recall * 1000.0) as u128, // recall×1000 carried in total_ns
            });
            assert!(recall >= 0.90, "ANN recall at n={n} fell to {recall:.3}");
        }
    }
}

fn bench_wire(b: &mut Bench) {
    // PackStream codec throughput on a record-shaped value.
    let mut props = BTreeMap::new();
    props.insert("name".to_string(), Value::Str("Ada Lovelace".into()));
    props.insert("score".to_string(), Value::Float(0.987));
    props.insert(
        "tags".to_string(),
        Value::List(vec![Value::Str("x".into()), Value::Str("y".into())]),
    );
    let node = Value::Node {
        id: 42,
        labels: vec!["Person".into()],
        props,
    };
    let iters = 200_000u64;
    let mut bytes = Vec::new();
    b.time("wire", "packstream encode (node)", 1, iters, || {
        for _ in 0..iters {
            bytes.clear();
            engram_bolt::encode_value(&node, &mut bytes).expect("encode");
        }
    });
    b.time("wire", "packstream decode (node)", 1, iters, || {
        for _ in 0..iters {
            let p = engram_bolt::Decoder::new(&bytes).decode().expect("decode");
            let _ = engram_bolt::decode_value(p).expect("value");
        }
    });

    // Whole in-process Bolt sessions (sans-io feed — protocol cost only).
    const HANDSHAKE: [u8; 20] = [
        0x60, 0x60, 0xB0, 0x17, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04, 0x00, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x00, 0x00,
    ];
    let msg = |tag: u8, fields: Vec<Pack>| -> Vec<u8> {
        let mut payload = Vec::new();
        engram_bolt::packstream::encode_struct(tag, &fields, &mut payload).expect("encodes");
        let mut out = (payload.len() as u16).to_be_bytes().to_vec();
        out.extend_from_slice(&payload);
        out.extend_from_slice(&[0, 0]);
        out
    };
    let empty = || Pack::Value(Value::Map(BTreeMap::new()));
    let g = seeded_graph(10_000, 2);
    let mut srv = BoltServer::new(g);
    srv.feed(&HANDSHAKE).expect("handshake");
    srv.feed(&msg(0x01, vec![empty()])).expect("hello");
    srv.feed(&msg(0x6A, vec![empty()])).expect("logon");
    let mut run_pull = msg(
        0x10,
        vec![
            Pack::Value(Value::Str(
                "MATCH (n:N {bucket: 3}) RETURN n.i LIMIT 5".into(),
            )),
            empty(),
            empty(),
        ],
    );
    let mut pull = BTreeMap::new();
    pull.insert("n".to_string(), Value::Int(-1));
    run_pull.extend(msg(0x3F, vec![Pack::Value(Value::Map(pull))]));
    let s_iters = 200u64;
    b.time(
        "wire",
        "bolt RUN+PULL (sans-io, 10k graph)",
        10_000,
        s_iters,
        || {
            for _ in 0..s_iters {
                let out = srv.feed(&run_pull).expect("session");
                assert!(!out.is_empty());
            }
        },
    );
}

// ─── Shapes ─────────────────────────────────────────────────────────────────

struct Shape {
    what: String,
    ratio: f64,
    bound: f64,
    ok: bool,
}

fn shapes(b: &Bench) -> Vec<Shape> {
    let mut out = Vec::new();
    let mut check =
        |what: &str, small: Option<&Measurement>, big: Option<&Measurement>, bound: f64| {
            if let (Some(s), Some(l)) = (small, big) {
                let ratio = l.ns_per_op() / s.ns_per_op();
                out.push(Shape {
                    what: what.to_string(),
                    ratio,
                    bound,
                    ok: ratio <= bound,
                });
            }
        };
    // Point reads must stay near-flat as the corpus grows 100x.
    check(
        "store get ns/op, 100k vs 1k (flat point reads)",
        b.find("get point", 1_000),
        b.find("get point", 100_000),
        8.0,
    );
    // Scans are linear: ns/ROW roughly flat across 100x.
    check(
        "store scan ns/row, 100k vs 1k (linear scans)",
        b.find("scan full", 1_000),
        b.find("scan full", 100_000),
        8.0,
    );
    check(
        "label scan ns/row, 50k vs 1k",
        b.find("label scan", 1_000),
        b.find("label scan", 50_000),
        8.0,
    );
    // A 1-hop step must not grow with graph size (fan-out fixed).
    check(
        "rels_of ns/op, 50k vs 1k (flat traversal step)",
        b.find("rels_of (1-hop step)", 1_000),
        b.find("rels_of (1-hop step)", 50_000),
        30.0,
    );
    // The adversarial shapes: each anti-pattern must degrade BOUNDEDLY and
    // recover after the maintenance event that exists to fix it.
    check(
        "interleaved read+write ns/op, 100k vs 10k (flat under churn)",
        b.find("interleaved 50/50 read+write", 10_000),
        b.find("interleaved 50/50 read+write", 100_000),
        8.0,
    );
    check(
        "tiny reads vs 64 segments over 1 segment (read amplification bound)",
        b.find("tiny reads vs 1 segment", 32_768),
        b.find("tiny reads vs 64 segments", 32_768),
        64.0,
    );
    check(
        "tiny reads after compaction vs 1 segment (compaction RESTORES)",
        b.find("tiny reads vs 1 segment", 32_768),
        b.find("tiny reads after compaction", 32_768),
        3.0,
    );
    check(
        "sequential ingest ns/op, 500k vs 100k (flat bulk writes)",
        b.find("sequential ingest (seal every 50k)", 100_000),
        b.find("sequential ingest (seal every 50k)", 500_000),
        8.0,
    );
    check(
        "hot put ns/op, 100k-chain vs 10k-chain (append stays FLAT)",
        b.find("hot put (chain build)", 10_000),
        b.find("hot put (chain build)", 100_000),
        4.0,
    );
    check(
        "unique-signature compact vs same-signature (grouping overhead bound)",
        b.find("compact same-signature", 50_000),
        b.find("compact unique-signatures", 50_000),
        10.0,
    );
    // ANN queries must beat exact by a widening margin at 50k.
    if let (Some(exact), Some(ann)) = (
        b.find("exact scan k=10", 50_000),
        b.find("ann query k=10", 50_000),
    ) {
        let speedup = exact.ns_per_op() * exact.iters as f64 / (ann.ns_per_op() * ann.iters as f64)
            * (ann.iters as f64 / exact.iters as f64);
        let per_q_exact = exact.total_ns as f64 / exact.iters as f64;
        let per_q_ann = ann.total_ns as f64 / ann.iters as f64;
        let ratio = per_q_exact / per_q_ann;
        let _ = speedup;
        out.push(Shape {
            what: format!("ann speedup over exact at 50k (={ratio:.1}x)"),
            ratio,
            bound: 2.0,
            ok: ratio >= 2.0,
        });
    }
    out
}

// ─── Report ─────────────────────────────────────────────────────────────────

fn main() {
    let debug_build = cfg!(debug_assertions);
    if debug_build {
        eprintln!("!! DEBUG BUILD — numbers are not the baseline; rerun with --release");
    }
    let mut b = Bench { out: Vec::new() };
    bench_store(&mut b);
    bench_columnar(&mut b);
    bench_adversarial(&mut b);
    bench_graph(&mut b);
    bench_cypher(&mut b);
    bench_vector(&mut b);
    bench_wire(&mut b);

    println!(
        "\n{:<40} {:>9} {:>12} {:>14}",
        "measurement", "size", "ns/op", "ops/sec"
    );
    println!("{}", "-".repeat(80));
    let mut suite = "";
    for m in &b.out {
        if m.suite != suite {
            suite = m.suite;
            println!("[{suite}]");
        }
        println!(
            "  {:<38} {:>9} {:>12.0} {:>14.0}",
            m.name,
            m.size,
            m.ns_per_op(),
            m.ops_per_sec()
        );
    }

    println!("\nscaling shapes (generous bounds; WARN is a flag, not a gate):");
    let shapes = shapes(&b);
    for s in &shapes {
        println!(
            "  {:<58} ratio {:>7.2} bound {:>5.1}  {}",
            s.what,
            s.ratio,
            s.bound,
            if s.ok { "OK" } else { "WARN" }
        );
    }

    // The committed baseline.
    let mut doc = BTreeMap::new();
    doc.insert(
        "build".to_string(),
        Value::Str(if debug_build {
            "debug".into()
        } else {
            "release".to_string()
        }),
    );
    doc.insert(
        "os".to_string(),
        Value::Str(std::env::consts::OS.to_string()),
    );
    doc.insert(
        "arch".to_string(),
        Value::Str(std::env::consts::ARCH.to_string()),
    );
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        doc.insert(
            "captured_epoch_seconds".to_string(),
            Value::Int(now.as_secs() as i64),
        );
    }
    doc.insert(
        "measurements".to_string(),
        Value::List(
            b.out
                .iter()
                .map(|m| {
                    let mut e = BTreeMap::new();
                    e.insert("suite".to_string(), Value::Str(m.suite.to_string()));
                    e.insert("name".to_string(), Value::Str(m.name.clone()));
                    e.insert("size".to_string(), Value::Int(m.size as i64));
                    e.insert("ns_per_op".to_string(), Value::Float(m.ns_per_op()));
                    e.insert("ops_per_sec".to_string(), Value::Float(m.ops_per_sec()));
                    Value::Map(e)
                })
                .collect(),
        ),
    );
    doc.insert(
        "shapes".to_string(),
        Value::List(
            shapes
                .iter()
                .map(|s| {
                    let mut e = BTreeMap::new();
                    e.insert("what".to_string(), Value::Str(s.what.clone()));
                    e.insert("ratio".to_string(), Value::Float(s.ratio));
                    e.insert("bound".to_string(), Value::Float(s.bound));
                    e.insert("ok".to_string(), Value::Bool(s.ok));
                    Value::Map(e)
                })
                .collect(),
        ),
    );
    let json = engram_cypher::json::to_json(&Value::Map(doc));
    std::fs::create_dir_all("measurements").expect("mkdir");
    std::fs::write("measurements/baseline.json", &json).expect("write baseline");
    println!(
        "\nbaseline written to measurements/baseline.json ({} bytes)",
        json.len()
    );
}
