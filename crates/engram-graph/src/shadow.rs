//! The shadow comparator — one statement, two engines, a named verdict.
//!
//! M8's shadow-read discipline in engine form, and the instrument the
//! corpus-compatibility measurement runs on: the same statement executes
//! against two graphs and the DECODED results are compared value by value.
//! An agreed refusal is agreement (two engines refusing the same statement
//! the same way are compatible); a divergence names the first differing row
//! rather than reporting a bare boolean.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_cypher::stmt::Stmt;
use engram_observe::counted;

use crate::Graph;
use crate::interp::{QueryResult, run_stmt};

/// The comparison's outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum ShadowVerdict {
    /// Same columns, same rows (or the same refusal).
    Agree,
    /// The engines diverged.
    Diverge {
        /// What differed, named.
        detail: String,
    },
}

/// Run `stmt` against both graphs and compare.
pub fn shadow_compare(
    a: &Graph,
    b: &Graph,
    stmt: &Stmt,
    params: BTreeMap<String, Value>,
) -> ShadowVerdict {
    counted!("shadow.statements compared");
    let ra = run_stmt(a, stmt, params.clone());
    let rb = run_stmt(b, stmt, params);
    match (ra, rb) {
        (Ok(ra), Ok(rb)) => diff_results(&ra, &rb),
        (Err(ea), Err(eb)) => {
            // An agreed refusal is agreement — but only the SAME refusal.
            let (ea, eb) = (ea.to_string(), eb.to_string());
            if ea == eb {
                ShadowVerdict::Agree
            } else {
                ShadowVerdict::Diverge {
                    detail: format!("refusals differ: `{ea}` vs `{eb}`"),
                }
            }
        }
        (Ok(ra), Err(e)) => ShadowVerdict::Diverge {
            detail: format!("A answered {} row(s); B refused: {e}", ra.rows.len()),
        },
        (Err(e), Ok(rb)) => ShadowVerdict::Diverge {
            detail: format!("A refused: {e}; B answered {} row(s)", rb.rows.len()),
        },
    }
}

fn diff_results(a: &QueryResult, b: &QueryResult) -> ShadowVerdict {
    if a.columns != b.columns {
        return ShadowVerdict::Diverge {
            detail: format!("columns differ: {:?} vs {:?}", a.columns, b.columns),
        };
    }
    if a.rows.len() != b.rows.len() {
        return ShadowVerdict::Diverge {
            detail: format!("row counts differ: {} vs {}", a.rows.len(), b.rows.len()),
        };
    }
    for (i, (ra, rb)) in a.rows.iter().zip(&b.rows).enumerate() {
        if ra != rb {
            return ShadowVerdict::Diverge {
                detail: format!("row {i} differs: {ra:?} vs {rb:?}"),
            };
        }
    }
    ShadowVerdict::Agree
}
