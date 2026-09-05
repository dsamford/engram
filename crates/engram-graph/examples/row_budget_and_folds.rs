//! The bound that actually protects the process, demonstrated.
//!
//! `docs/book/src/using/result-paging.md` makes two claims that are easy to
//! state and easy to get wrong, and the page's own first draft got the second
//! one's error code wrong:
//!
//!  1. a query materialising more than the row budget is REFUSED;
//!  2. `count(*)` is answered by a fold that never builds those rows, so it
//!     answers correctly under a budget far smaller than its own result set.
//!
//! The second is the interesting one. It is what makes "aggregate rather than
//! return rows" real advice instead of a platitude, and nothing in a prose
//! page can keep it true.
//!
//! ```text
//! cargo run -p engram-store --example row_budget_and_folds
//! ```

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> Result<Vec<Vec<Value>>, String> {
    let stmt = parse_statement(src).map_err(|e| format!("parse: {e}"))?;
    run_query(g, &stmt, BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| format!("{e}"))
}

fn demo() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));

    // A budget far below what the self-join below produces.
    g.set_row_budget(Some(100));

    for i in 0..50i64 {
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), Value::Int(i));
        g.create_node(&["N".to_string()], &props).expect("create");
    }

    // ── Returning the rows is refused ────────────────────────────────────
    //
    // 50 x 50 = 2,500 rows against a budget of 100.
    let refused = run(&g, "MATCH (a:N), (b:N) RETURN a.i, b.i");
    let msg = refused.expect_err("2,500 rows under a 100-row budget must refuse");
    assert!(
        msg.contains("row budget"),
        "the refusal must say WHICH bound was hit, or an operator cannot act \
         on it: {msg}"
    );

    // ── Counting them is not ─────────────────────────────────────────────
    //
    // The same 2,500 rows, answered correctly under a budget twenty-five times
    // smaller — because the fold multiplies weights instead of materialising
    // rows, and the product is the same count.
    let counted = run(&g, "MATCH (a:N), (b:N) RETURN count(*) AS c")
        .expect("count(*) folds rather than materialising, so the budget is not reached");
    assert_eq!(counted.len(), 1);
    assert!(
        matches!(counted[0][0], Value::Int(2500)),
        "the fold must produce the SAME count the enumeration would: {:?}",
        counted[0]
    );

    // ── An unbounded budget is a choice, not a default ───────────────────
    g.set_row_budget(None);
    let now_allowed = run(&g, "MATCH (a:N), (b:N) RETURN a.i, b.i")
        .expect("with no budget the same statement is allowed through");
    assert_eq!(
        now_allowed.len(),
        2500,
        "and it really does produce the rows the fold counted"
    );

    println!(
        "row_budget_and_folds: refused 2500 rows under a 100-row budget, \
         counted them anyway, and produced all 2500 with the budget removed"
    );
}

fn main() {
    demo();
}

/// `cargo test --examples` COMPILES an example but does not run its `main`, so
/// an example alone proves the snippet type-checks and nothing about whether it
/// still answers. This is what makes the assertions above run in CI.
#[cfg(test)]
mod tests {
    #[test]
    fn the_documented_behaviour_still_holds() {
        super::demo();
    }
}
