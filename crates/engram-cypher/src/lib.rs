//! M7 — the Cypher front end, first slice: tokenizer, expression parser,
//! the value model with three-valued logic, and the constant evaluator.
//!
//! The corpus this must eventually serve is "large but shallow" (the plan's
//! census): broad clause coverage, five APOC functions, three-valued NULL
//! logic RELIED ON (`null = 'x'` failing closed is load-bearing). Clauses and
//! patterns build on this layer; every semantic decision about null, type
//! refusal and overflow is made once, here, where a unit test can pin it.
//!
//! The kill criterion for M7 is corpus pass rate measured as IDENTICAL
//! DECODED DRIVER VALUES — which is why the evaluator exists from the first
//! revision: a parser alone can only claim "parses", the claim the plan
//! explicitly rejects as near-zero value.

#![forbid(unsafe_code)]

pub mod ast;
pub mod bindings;
pub mod clause;
pub use bindings::VarMap;
pub mod eval;
pub mod json;
pub mod parser;
pub mod stmt;
pub mod temporal;
pub mod token;
pub mod value;

pub use ast::{BinOp, Expr};
pub use clause::{parse_any, parse_statement};
pub use eval::{EvalError, GraphHooks, Scope, eval, eval_with};
pub use parser::{MIN_PARSER_STACK_BYTES, ParseError, parse_expression};
pub use stmt::{
    Clause, ConstraintKind, NodePattern, OrderItem, PathPattern, Pattern, ProjItem, Projection,
    Query, RelDir, RelPattern, RemoveItem, SchemaCmd, SetItem, SingleQuery, Stmt, SubqueryBody,
    VarLength,
};
pub use temporal::{FixedZones, ZoneProvider};
pub use token::{LexError, Token, TokenKind, tokenize};
pub use value::{Truth, Value};

use engram_observe::{Canary, Gate, Registration, Subsystem};

/// Render a temporal value as its ISO-8601 string — shared by `toString`,
/// JSON, and diagnostics. Non-temporal values render via Debug (callers
/// gate on kind first).
pub fn temporal_to_string(v: &Value) -> String {
    match v {
        Value::Date(days) => temporal::format_date(*days),
        Value::LocalTime(nanos) => temporal::format_time_of_day(*nanos),
        Value::Time {
            nanos,
            offset_seconds,
        } => {
            format!(
                "{}{}",
                temporal::format_time_of_day(*nanos),
                temporal::format_offset(*offset_seconds)
            )
        }
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => {
            let days = epoch_seconds.div_euclid(86_400);
            let tod = epoch_seconds.rem_euclid(86_400) * 1_000_000_000 + i64::from(*nanos);
            format!(
                "{}T{}",
                temporal::format_date(days),
                temporal::format_time_of_day(tod)
            )
        }
        Value::DateTime {
            epoch_seconds,
            nanos,
            offset_seconds,
            zone,
        } => {
            let local = epoch_seconds + i64::from(*offset_seconds);
            let days = local.div_euclid(86_400);
            let tod = local.rem_euclid(86_400) * 1_000_000_000 + i64::from(*nanos);
            let mut out = format!(
                "{}T{}{}",
                temporal::format_date(days),
                temporal::format_time_of_day(tod),
                temporal::format_offset(*offset_seconds)
            );
            if let Some(z) = zone {
                out.push('[');
                out.push_str(z);
                out.push(']');
            }
            out
        }
        Value::Duration {
            months,
            days,
            seconds,
            nanos,
        } => temporal::format_duration(*months, *days, *seconds, *nanos),
        other => format!("{other:?}"),
    }
}

/// The Cypher front end, as a registered subsystem.
pub struct CypherFrontend;

impl Subsystem for CypherFrontend {
    const NAME: &'static str = "cypher";

    fn register() -> Registration {
        Registration::new()
            // Reserved AT the boundary this layer will hand mutations to the
            // store (the clause executor's apply step) — the same aspirational
            // placement engram-key and engram-exec use for their pure layers.
            .crash_point("cypher.before_statement_apply")
            .sometimes("cypher.lex refused")
            .sometimes("cypher.parse refused")
            .sometimes("cypher.null propagated through a predicate")
            .sometimes("cypher.unknown function refused")
            .counter("cypher.expressions parsed")
            .counter("cypher.statements parsed")
            .counter("cypher.expressions evaluated")
            .gate(
                Gate::new(
                    "null fails closed everywhere a predicate reads it",
                    Canary::new("make Unknown collapse to false in eq3 and assert `null = 'x'` stops being null"),
                )
                .and_canary(Canary::new("make IN ignore unknown elements and assert `1 IN [null]` reads false")),
            )
            .gate(
                Gate::new(
                    "a refusal names its position and expectation",
                    Canary::new("drop the position from parse errors and assert the message test fails"),
                ),
            )
            .gate(
                Gate::new(
                    "integer arithmetic never wraps",
                    Canary::new("use wrapping_add and assert MAX + 1 is refused, not negative"),
                ),
            )
            .gate(
                Gate::new(
                    "an unknown function is a NAMED refusal, never null",
                    Canary::new("return null for unknown functions and assert the error test fails"),
                ),
            )
    }
}
