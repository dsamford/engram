//! `PgBackend` against the SNB property schema on a real corpus.
//!
//! ```sh
//! kubectl port-forward pod/<your-postgres-pod> 15432:5432
//! ENGRAM_PGWIRE_TEST_ADDR=127.0.0.1:15432 ENGRAM_PGWIRE_TEST_SNB_DB=sf1 \
//!   cargo test -p engram-bench \
//!   --test a_live_pgbackend_reads_the_snb_corpus_through_the_catalogues_own_sql \
//!   -- --nocapture
//! ```
//!
//! # What this covers that nothing else does
//!
//! `docs/bench/pg-snb-load.sh` builds the schema and checks every row count
//! against the corpus manifest, but it checks a DATABASE. The statements were
//! then smoke-tested with `psql`. Neither touches the layer the harness calls:
//! `PgBackend` had never issued a statement against an SNB corpus, so the
//! connection, the row decoding, the SQLSTATE classification and the
//! transaction seam were all unexercised on this dataset — the same gap that
//! hid three defects on the `synthetic` arm until
//! `a_live_pgbackend_runs_the_synthetic_workload_and_catches_a_dropped_write`
//! closed it there.
//!
//! # Plausibility, not equivalence — said plainly
//!
//! Nothing here compares an answer with engram's. Every assertion below is an
//! invariant the SQL must satisfy on its own terms, and two of them are
//! cross-checks between statements the catalogue holds separately:
//!
//!   * `is5-by-creator` and `is5-anchored` ask the same question from opposite
//!     ends and must return the same set of message ids;
//!   * `knows-var-length` walks `KNOWS*1..2` and `ic-foaf` walks exactly two
//!     hops, so the first count can never be below the second.
//!
//! Those catch a join written the wrong way round, which a row count cannot.
//! They do not make the statements `verified` in the catalogue's sense — that
//! word means an answer checked against another engine's, and no such check
//! has been run.
//!
//! # This suite is not a measurement, and it does not write
//!
//! It reads a shared, verified corpus. Nothing is timed. The only writes run
//! inside an explicit transaction that is rolled back, and the rollback is
//! PROVEN by re-counting afterwards rather than assumed — a write test that
//! leaked would change the corpus every later run measures.

use std::collections::BTreeSet;

use engram_bench::backend::{Backend, Cell, OpError, PgBackend};
use engram_bench::catalogue::{Catalogue, Dialect, render};
use engram_bench::workload::{Locality, Rng, bind_read};

/// The keys drawn per shape. Enough that a shape which can legitimately answer
/// nothing for one person still proves it can answer something, and small
/// enough that this stays a correctness check rather than a load.
const DRAWS: usize = 24;

/// `--thread-cap`'s value in the documented sweep invocation. Passed so this
/// connects the way a measured run would; nothing here depends on it.
const THREAD_CAP: u32 = 6;

/// Open a `PgBackend` on a database that must already hold the SNB property
/// schema, or explain — loudly — why the test is not running.
///
/// The database is NOT defaulted. A default of `postgres` would connect to an
/// empty database, find no `person` table, and the failure would look like a
/// broken schema rather than a misaimed test; a default of `sf1` would run
/// against a shared corpus somebody did not offer.
fn live_or_skip(what: &str) -> Option<PgBackend> {
    let addr = std::env::var("ENGRAM_PGWIRE_TEST_ADDR")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let db = std::env::var("ENGRAM_PGWIRE_TEST_SNB_DB")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let strict = matches!(
        std::env::var("ENGRAM_PGWIRE_TEST_REQUIRE_LIVE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    let (Some(addr), Some(db)) = (addr, db) else {
        assert!(
            !strict,
            "[pgbackend-snb] {what} could NOT run: ENGRAM_PGWIRE_TEST_REQUIRE_LIVE says a live \
             run was intended and ENGRAM_PGWIRE_TEST_ADDR / ENGRAM_PGWIRE_TEST_SNB_DB are not \
             both set. Failing rather than skipping."
        );
        eprintln!(
            "[pgbackend-snb] {what} SKIPPED — set ENGRAM_PGWIRE_TEST_ADDR and \
             ENGRAM_PGWIRE_TEST_SNB_DB (the database docs/bench/pg-snb-load.sh loaded, e.g. \
             sf1). Nothing else exercises PgBackend against the SNB schema: the loader checks a \
             database and the smoke test drives psql. \
             ENGRAM_PGWIRE_TEST_REQUIRE_LIVE=1 makes this a failure."
        );
        return None;
    };
    let user = std::env::var("ENGRAM_PGWIRE_TEST_USER").unwrap_or_else(|_| "postgres".to_string());
    let mut b = PgBackend::connect(&addr, &user, &db, Some(THREAD_CAP)).unwrap_or_else(|e| {
        panic!("[pgbackend-snb] {what}: could not connect to {addr} as {user}/{db}: {e}")
    });
    // Refuse a database that is not an SNB one, rather than reporting its
    // emptiness as a schema failure further down.
    let cols = b
        .scalar(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'person' \
             AND column_name IN ('first_name','last_name','birthday','location_ip','browser_used','hits')",
        )
        .unwrap_or_else(|e| panic!("[pgbackend-snb] {what}: person column probe: {e}"));
    assert_eq!(
        cols, 6,
        "[pgbackend-snb] {what}: `{db}` does not carry the SNB property schema \
         (person has {cols} of the 6 property columns). Run \
         `pg-snb-load.sh load <corpus-dir> {db}` first; refusing to report that as a query \
         failure."
    );
    Some(b)
}

/// The key space, probed exactly as `seed_or_attach` probes it.
fn probe_keys(b: &mut PgBackend, cat: &Catalogue) -> u64 {
    let attach = cat
        .dataset_str("snb", "sql_attach")
        .expect("snb.sql_attach")
        .expect("snb declares a sql_attach");
    let rows = b
        .run(&attach)
        .unwrap_or_else(|e| panic!("[pgbackend-snb] attach probe ({attach}): {e}"));
    assert!(
        rows > 0,
        "[pgbackend-snb] the attach probe found no persons; this is not a loaded SNB corpus"
    );
    rows
}

/// Render one read shape with the workload's own binding.
fn read_stmt(cat: &Catalogue, shape: &str, key: u64, space: u64) -> String {
    let entry = cat
        .read_shape(shape, Dialect::Sql)
        .unwrap_or_else(|e| panic!("catalogue read_shape {shape}: {e}"));
    assert!(
        entry.status.runnable(),
        "{shape} is declared unsupported for sql; this file should not be asking for it"
    );
    let owned: Vec<(String, String)> = bind_read(shape, key, space)
        .iter()
        .map(|(k, v)| (k.clone(), v.render()))
        .collect();
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    render(&entry.text, &refs)
}

/// The keys a client would actually draw for this shape, from a fixed seed.
///
/// Drawn rather than chosen: a hand-picked person that happens to have friends,
/// messages and replies would prove the statement runs on the one row somebody
/// looked at.
fn drawn_keys(space: u64) -> Vec<u64> {
    let mut rng = Rng::new(20_260_909);
    (0..DRAWS)
        .map(|_| Locality::Zipfian.pick(&mut rng, space))
        .collect()
}

/// Run one shape over the drawn keys. Returns every result set, and fails on
/// the first error rather than counting how many worked.
fn run_shape(
    b: &mut PgBackend,
    cat: &Catalogue,
    shape: &str,
    keys: &[u64],
    space: u64,
) -> Vec<(u64, Vec<Vec<Cell>>)> {
    keys.iter()
        .map(|&k| {
            let sql = read_stmt(cat, shape, k, space);
            let rows = b.query(&sql).unwrap_or_else(|e| {
                panic!("[pgbackend-snb] {shape} at key {k} failed: {e}\n  {sql}")
            });
            (k, rows)
        })
        .collect()
}

fn scalar_of(rows: &[Vec<Cell>], shape: &str) -> i64 {
    assert_eq!(rows.len(), 1, "{shape} must answer exactly one row");
    assert_eq!(rows[0].len(), 1, "{shape} must answer exactly one column");
    rows[0][0]
        .as_int()
        .unwrap_or_else(|| panic!("{shape} answered a non-integer: {:?}", rows[0][0]))
}

fn ids(rows: &[Vec<Cell>]) -> BTreeSet<i64> {
    rows.iter()
        .map(|r| r[0].as_int().expect("an id column is an integer"))
        .collect()
}

#[test]
fn every_snb_read_shape_runs_through_pgbackend_and_answers_plausibly() {
    let Some(mut b) = live_or_skip("read shapes") else {
        return;
    };
    let cat = Catalogue::load().expect("catalogue");
    let space = probe_keys(&mut b, &cat);
    let keys = drawn_keys(space);
    eprintln!(
        "[pgbackend-snb] {} v{} — {space} person(s), {DRAWS} drawn keys",
        b.engine(),
        b.version()
    );

    // The fixture probes the harness forces before a level, first: they are
    // the cheapest possible statement and a failure here means the schema is
    // not there at all.
    for p in cat
        .fixture("snb", Dialect::Sql, "probes")
        .expect("snb fixtures")
    {
        let rows = b
            .query(&p)
            .unwrap_or_else(|e| panic!("fixture probe ({p}): {e}"));
        assert_eq!(rows.len(), 1, "fixture probe must find its row: {p}");
    }

    // is1-profile: a point lookup on a dense id space must always hit, and the
    // five projected properties must all be present. A schema that loaded ids
    // and no properties would return a row of NULLs and pass a row count.
    let mut nonempty = 0usize;
    for (k, rows) in run_shape(&mut b, &cat, "is1-profile", &keys, space) {
        assert_eq!(
            rows.len(),
            1,
            "is1-profile missed person {k} in a dense id space"
        );
        assert_eq!(rows[0].len(), 5, "is1-profile projects five columns");
        for (i, c) in rows[0].iter().enumerate() {
            assert!(
                !matches!(c, Cell::Null),
                "is1-profile column {i} is NULL for person {k}: the corpus has a value there"
            );
        }
        nonempty += 1;
    }
    assert_eq!(nonempty, DRAWS);

    let friends = run_shape(&mut b, &cat, "is3-friends", &keys, space);
    assert!(
        friends.iter().any(|(_, r)| !r.is_empty()),
        "is3-friends answered nothing for all {DRAWS} drawn keys"
    );
    for (k, rows) in &friends {
        assert!(
            rows.len() <= 25,
            "is3-friends is LIMIT 25, got {} for {k}",
            rows.len()
        );
        for r in rows {
            assert_eq!(r.len(), 2, "is3-friends projects (id, first_name)");
            assert!(
                !matches!(r[1], Cell::Null),
                "is3-friends returned a friend with no first_name; the join reached a row the \
                 property load did not fill"
            );
        }
    }

    // The two cross-statement invariants. These are what a row count cannot
    // see: a join written from the wrong side keeps the shape and changes the
    // answer.
    let foaf = run_shape(&mut b, &cat, "ic-foaf", &keys, space);
    let varlen = run_shape(&mut b, &cat, "knows-var-length", &keys, space);
    let mut some_positive = false;
    for ((k, f), (k2, v)) in foaf.iter().zip(varlen.iter()) {
        assert_eq!(k, k2);
        let f = scalar_of(f, "ic-foaf");
        let v = scalar_of(v, "knows-var-length");
        assert!(
            v >= f,
            "KNOWS*1..2 reaches {v} distinct persons from {k} but exactly-two-hops reaches {f}; \
             the 1..2 walk cannot be the smaller set"
        );
        some_positive |= f > 0;
    }
    assert!(
        some_positive,
        "ic-foaf answered 0 for all {DRAWS} drawn keys"
    );

    let by_creator = run_shape(&mut b, &cat, "is5-by-creator", &keys, space);
    let anchored = run_shape(&mut b, &cat, "is5-anchored", &keys, space);
    let mut saw_messages = false;
    let mut compared = 0usize;
    for ((k, a), (k2, c)) in by_creator.iter().zip(anchored.iter()) {
        assert_eq!(k, k2);
        // Both carry `LIMIT 25` and neither carries an ORDER BY, so when the
        // limit is REACHED the two are each entitled to a different 25 and a
        // set comparison would be a flake waiting to happen. Compare the sets
        // only where the limit did not bind -- there the result is the whole
        // answer and the comparison is exact.
        if a.len() < 25 && c.len() < 25 {
            assert_eq!(
                ids(a),
                ids(c),
                "is5-by-creator and is5-anchored ask the same question from opposite ends and \
                 disagree for person {k}"
            );
            compared += 1;
        } else {
            assert_eq!(
                a.len(),
                c.len(),
                "both is5 shapes must hit the same LIMIT for {k}"
            );
        }
        saw_messages |= !a.is_empty();
    }
    assert!(
        compared > 0,
        "every drawn key hit the LIMIT, so the is5 cross-check compared nothing -- it must not \
         report a pass it did not make"
    );
    assert!(
        saw_messages,
        "is5-* answered nothing for all {DRAWS} drawn keys"
    );

    let tags = run_shape(&mut b, &cat, "ic6-friend-tags", &keys, space);
    assert!(
        tags.iter().any(|(_, r)| !r.is_empty()),
        "ic6-friend-tags answered nothing"
    );
    for (k, rows) in &tags {
        assert!(
            rows.len() <= 10,
            "ic6-friend-tags is LIMIT 10, got {}",
            rows.len()
        );
        let counts: Vec<i64> = rows
            .iter()
            .map(|r| r[1].as_int().expect("a count"))
            .collect();
        assert!(
            counts.windows(2).all(|w| w[0] >= w[1]),
            "ic6-friend-tags is ORDER BY c DESC and came back unsorted for {k}: {counts:?}"
        );
        for r in rows {
            assert!(
                !matches!(r[0], Cell::Null),
                "ic6-friend-tags returned a tag with no name"
            );
        }
    }

    let replies = run_shape(&mut b, &cat, "is7-replies", &keys, space);
    for (_, rows) in &replies {
        assert!(rows.len() <= 25, "is7-replies is LIMIT 25");
    }

    // agg-by-city takes no parameter, so once is the whole shape.
    let city = b
        .query(&read_stmt(&cat, "agg-by-city", 0, space))
        .expect("agg-by-city");
    assert_eq!(
        city.len(),
        10,
        "agg-by-city is LIMIT 10 over a corpus with 1,343 cities"
    );
    let counts: Vec<i64> = city
        .iter()
        .map(|r| r[1].as_int().expect("a count"))
        .collect();
    assert!(
        counts.windows(2).all(|w| w[0] >= w[1]),
        "agg-by-city is ORDER BY n DESC and came back unsorted: {counts:?}"
    );
    assert!(counts[0] > 0 && !matches!(city[0][0], Cell::Null));
    eprintln!("[pgbackend-snb] all nine read shapes answered plausibly");
}

#[test]
fn a_duplicate_message_id_classifies_as_a_refusal_and_leaves_the_connection_usable() {
    // `unique-create`'s entire measurement is that a constraint violation is a
    // REFUSAL and not a transport failure: K clients race one value, one wins,
    // and the other K-1 must be counted as correct refusals rather than as a
    // broken run. `message.id` is a primary key on this schema, so a corpus id
    // reinserted is the same SQLSTATE class (23) reached through a different
    // table -- which makes it a free check that the classification holds here.
    let Some(mut b) = live_or_skip("refusal classification") else {
        return;
    };
    b.begin().expect("begin");
    let err = b
        .run(
            "INSERT INTO message (id, creation_date, content, length, is_comment) \
             VALUES (0, 1, 'dup', 3, true)",
        )
        .expect_err("inserting an id the corpus already holds must fail");
    assert!(
        matches!(err, OpError::Refusal(_)),
        "a primary-key violation must classify as a REFUSAL, not as {err}"
    );
    b.rollback().expect("rollback");

    // The connection survives a refusal -- if it did not, every unique-create
    // level would reconnect K-1 times per round and measure the handshake.
    let n = b
        .scalar("SELECT count(*) FROM message WHERE id = 0")
        .expect("post-refusal query");
    assert_eq!(n, 1, "the corpus row is still there and exactly one of it");
    eprintln!("[pgbackend-snb] 23505 classified as a refusal; connection still usable");
}

#[test]
fn the_snb_write_ops_run_and_the_rollback_is_proven_rather_than_assumed() {
    let Some(mut b) = live_or_skip("write ops") else {
        return;
    };
    let cat = Catalogue::load().expect("catalogue");
    let space = probe_keys(&mut b, &cat);

    // Ids from the level-scoping arithmetic, far above any corpus id.
    let seq = 11u64;
    let ident = (7u64 << 40) | seq;
    let nonce = 20_260_909u64;
    // Baselines rather than an assumption of emptiness: a database a sweep has
    // already run against holds rows in these tables, and a test that demanded
    // zero afterwards would fail for the wrong reason and look like a leak.
    const TOUCHED: [&str; 7] = [
        "message",
        "stressed",
        "msgnode",
        "uniq",
        "churn",
        "churn_rel",
        "churn_anchor",
    ];
    let before: Vec<i64> = TOUCHED
        .iter()
        .map(|t| {
            b.scalar(&format!("SELECT count(*) FROM {t}"))
                .expect("census")
        })
        .collect();
    let hits_before = b
        .scalar("SELECT count(*) FROM person WHERE hits IS NOT NULL")
        .expect("hits census");

    let params: Vec<(String, String)> = vec![
        ("id", ident.to_string()),
        ("cid", "7".to_string()),
        ("seq", seq.to_string()),
        ("author", (seq % space).to_string()),
        ("mdate", (1_400_000_000_000u64 + seq).to_string()),
        ("nonce", nonce.to_string()),
        ("anchor", ident.to_string()),
        ("u", ident.to_string()),
        ("a", "1".to_string()),
        ("b", "0".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let refs: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    b.begin().expect("begin");
    for op in [
        "hot_update",
        "node_create",
        "node_only",
        "node_only_fresh_props",
        "unique_create",
        "rel_spread",
        "rel_hub",
        "churn_anchor",
        "churn_create",
        "churn_delete",
    ] {
        let entry = cat
            .write_op(op, "snb", Dialect::Sql)
            .unwrap_or_else(|e| panic!("catalogue write_op {op}: {e}"));
        assert!(entry.status.runnable(), "{op} is unsupported for sql");
        let sql = render(&entry.text, &refs);
        let n = b
            .run(&sql)
            .unwrap_or_else(|e| panic!("[pgbackend-snb] write op {op} failed: {e}\n  {sql}"));
        assert_eq!(n, 1, "{op} must affect exactly one row: {sql}");
    }
    // The effects, while they still exist.
    assert_eq!(
        b.scalar(&format!("SELECT count(*) FROM message WHERE id = {ident}"))
            .expect("q"),
        1,
        "node_create did not land a message"
    );
    assert_eq!(
        b.scalar(&format!(
            "SELECT count(*) FROM has_creator WHERE src = {ident}"
        ))
        .expect("q"),
        1,
        "node_create did not land the creator edge"
    );
    assert_eq!(
        b.scalar("SELECT count(*) FROM stressed").expect("q") - before[1],
        2,
        "rel_spread and rel_hub must each land one edge"
    );
    assert_eq!(
        b.scalar(&format!("SELECT count(*) FROM churn WHERE nonce = {nonce}"))
            .expect("q"),
        0,
        "churn_delete must have removed what churn_create made"
    );
    b.rollback().expect("rollback");

    // Proven, not assumed. A leaked row would change the corpus every later
    // run reads, and the row counts the loader verified would stop matching
    // the manifest -- silently, because nothing re-checks them per run.
    for (t, n) in TOUCHED.iter().zip(before.iter()) {
        assert_eq!(
            b.scalar(&format!("SELECT count(*) FROM {t}")).expect("q"),
            *n,
            "{t} did not return to its pre-transaction size"
        );
    }
    assert_eq!(
        b.scalar("SELECT count(*) FROM person WHERE hits IS NOT NULL")
            .expect("q"),
        hits_before,
        "hot_update's counter survived the rollback"
    );
    eprintln!("[pgbackend-snb] ten write ops ran; the transaction rolled back clean");
}
