//! A path whose START names nothing to seek and whose END names an inline
//! equality is walked from the end -- `reverse_to_selective_end` -- but only
//! when the end's label was no larger than the start's, so that a seek that
//! could not answer would fall back to the smaller scan. A larger label is not
//! a larger answer: LDBC FinBench tcr9 opens with
//! `OPTIONAL MATCH (loan1:Loan)-[:deposit]->(mid:Account {id: $id})`, 1.4M
//! loans against 2.1M accounts at SF10, and scanned every loan to reach one
//! account (1.5 s against Neo4j's 5 ms). The end's seek is now ASKED, capped
//! at 1/64th of the start's label, and the walk turns round when it answers.
//!
//! The oracle is the same statement with the selective-anchor lever off,
//! which keeps the written direction.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const TURNED: &str =
    "interp.path driven from its index-servable end: the seek answered under a larger label";

/// 400 accounts (every tenth `kind: 'rare'`, the rest `kind: 'common'`), 200
/// loans, each loan depositing into two accounts; account 7 takes five
/// deposits from loans 0-4 and one more from a loan outside the time window.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut accounts = Vec::new();
    for i in 0..400i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert(
            "kind".to_string(),
            Value::Str(if i % 10 == 0 { "rare" } else { "common" }.into()),
        );
        accounts.push(g.create_node(&["Account".into()], &m).expect("account"));
    }
    for i in 0..200i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(10_000 + i));
        let loan = g.create_node(&["Loan".into()], &m).expect("loan");
        let deposit = |to: u64, amount: i64, ts: i64| {
            let mut r = BTreeMap::new();
            r.insert("amount".to_string(), Value::Int(amount));
            r.insert("timestamp".to_string(), Value::Int(ts));
            g.create_rel(loan, "deposit", to, &r).expect("deposit");
        };
        if i < 5 {
            deposit(accounts[7], 100 + i, 50);
        } else if i == 5 {
            deposit(accounts[7], 1_000, 500);
        } else {
            deposit(accounts[(i as usize * 3) % 400], i, 50);
        }
        deposit(accounts[(i as usize * 7 + 11) % 400], 2 * i, 50);
    }
    g
}

fn traced(g: &Graph, src: &str, params: &BTreeMap<String, Value>) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let (r, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, params.clone())
            .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
            .rows
    });
    (r, trace.counters().clone())
}

fn oracle(g: &Graph, src: &str, params: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    g.set_selective_anchor(false);
    let (rows, _) = traced(g, src, params);
    g.set_selective_anchor(true);
    rows
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn params(id: i64) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("id".to_string(), Value::Int(id));
    p.insert("start".to_string(), Value::Int(0));
    p.insert("end".to_string(), Value::Int(100));
    p
}

/// tcr9's first clause: the account's seek answers one candidate, so the walk
/// starts there and reads its five deposits in the window.
#[test]
fn a_one_row_seek_turns_the_walk_round_under_a_larger_label() {
    let g = corpus();
    // The interpreter's walk (an OPTIONAL MATCH never reaches the columnar
    // planner, whose own re-rooting keeps its label guard).
    let src = "OPTIONAL MATCH (loan:Loan)-[e:deposit]->(mid:Account {id: $id}) \
               WHERE e.timestamp > $start AND e.timestamp < $end \
               WITH sum(e.amount) AS s RETURN s";
    let want = oracle(&g, src, &params(7));
    let (got, c) = traced(&g, src, &params(7));
    assert_eq!(got, want, "{src}");
    assert!(count_of(&c, TURNED) >= 1, "the walk was not turned round: {c:?}");
    // The same pattern as a MATCH answers alike, whichever planner takes it.
    let src = "MATCH (loan:Loan)-[e:deposit]->(mid:Account {id: $id}) \
               WHERE e.timestamp > $start AND e.timestamp < $end \
               RETURN loan.id AS loan, e.amount AS amount ORDER BY loan";
    let want = oracle(&g, src, &params(7));
    let (got, _) = traced(&g, src, &params(7));
    assert_eq!(got, want, "{src}");
    assert_eq!(got.len(), 5, "{got:?}");
    let (got, _) = traced(
        &g,
        "OPTIONAL MATCH (loan:Loan)-[e:deposit]->(mid:Account {id: $id}) \
         WHERE e.timestamp > $start AND e.timestamp < $end \
         WITH sum(e.amount) AS s RETURN s",
        &params(7),
    );
    assert_eq!(got, vec![vec![Value::Int(100 + 101 + 102 + 103 + 104)]]);
    // an id nothing carries: the OPTIONAL row stays, and sums to zero
    let src = "OPTIONAL MATCH (loan:Loan)-[e:deposit]->(mid:Account {id: $id}) \
               WITH sum(e.amount) AS s RETURN s";
    let (got, _) = traced(&g, src, &params(9_999));
    assert_eq!(got, oracle(&g, src, &params(9_999)));
    assert_eq!(got, vec![vec![Value::Int(0)]]);
}

/// A seek that answers more than 1/64th of the start's label -- 360 common
/// accounts against 200 loans -- keeps the written direction.
#[test]
fn b_a_wide_seek_under_a_larger_label_keeps_the_written_direction() {
    let g = corpus();
    let src = "MATCH (loan:Loan)-[e:deposit]->(a:Account {kind: 'common'}) RETURN count(*) AS n";
    let want = oracle(&g, src, &params(0));
    let (got, c) = traced(&g, src, &params(0));
    assert_eq!(got, want);
    assert_eq!(count_of(&c, TURNED), 0, "{c:?}");
}
