//! `YIELD` binding and row emission — once, for every procedure.
//!
//! # The defect this module exists to end
//!
//! Every procedure has to do the same two things, and every procedure used to
//! do them itself. Three copies of "resolve the `YIELD` list against the
//! fields this procedure has, defaulting to all of them when the list is
//! empty", and three copies of "for each input row, for each output row, bind
//! the aliases and apply the trailing `WHERE`". They were written at different
//! times and had already diverged: one validated its fields against a
//! hard-coded array, one against the first catalog row with a hand-maintained
//! fallback list for when the catalog came back empty, and one against a pair
//! of literals.
//!
//! Three copies of a rule is three chances to get it wrong and one chance in
//! three of fixing it. The rule is here now, stated once, and the fields come
//! from [`engram_proc`] rather than from a literal beside the body — so a
//! procedure's declared signature and the fields it will actually bind cannot
//! disagree.

use std::collections::BTreeMap;

use engram_cypher::ast::Expr;
use engram_cypher::value::{Truth, Value};
use engram_proc::ProcedureSignature;

use crate::Graph;
use crate::interp::{Row, RunError, eval_expr};

/// One resolved `YIELD` entry: the field the procedure produces, and the name
/// it is bound to in the row.
pub(crate) struct Binding {
    /// The procedure's own column name.
    pub(crate) field: String,
    /// What the row calls it — the alias if `YIELD x AS y` gave one, otherwise
    /// the field name itself.
    pub(crate) alias: String,
}

/// Resolve a `YIELD` list against what the procedure declares.
///
/// An EMPTY `yields` binds every declared output column under its own name.
/// That is the default output signature, and it is the whole reason a
/// standalone `CALL` can name its result: the columns come from the
/// catalogue, not from whatever literal happened to sit beside the body.
///
/// A `YIELD` naming a field the procedure does not declare is REFUSED, and the
/// refusal lists what it does declare — a user who mistypes `relationshipTypes`
/// for `relationshipType` should be told the spelling, not handed an empty
/// result.
pub(crate) fn resolve_bindings(
    sig: &ProcedureSignature,
    yields: &[(String, Option<String>)],
) -> Result<Vec<Binding>, RunError> {
    if yields.is_empty() {
        return Ok(sig
            .outputs
            .iter()
            .map(|c| Binding {
                field: c.name.to_string(),
                alias: c.name.to_string(),
            })
            .collect());
    }
    let mut out = Vec::with_capacity(yields.len());
    for (field, alias) in yields {
        if !sig.yields(field) {
            return Err(RunError::Semantic(format!(
                "`{}` does not yield `{field}` ({})",
                sig.display,
                sig.output_list()
            )));
        }
        out.push(Binding {
            field: field.clone(),
            alias: alias.clone().unwrap_or_else(|| field.clone()),
        });
    }
    Ok(out)
}

/// Bind one procedure's output rows onto the input rows, applying the trailing
/// `WHERE`.
///
/// A procedure produces its rows PER INPUT ROW — `MATCH (n) CALL p() YIELD x`
/// runs `p` once for every `n` — so the shape is a nested loop and the input
/// row is cloned rather than moved. `lookup` is asked for each bound field's
/// value by name; it returns `None` for a field the producer did not supply,
/// which binds null rather than panicking, because a producer that omits a
/// declared column is a bug in the producer and a null in the result is a far
/// better diagnostic than a crash in a database.
///
/// The `WHERE` is evaluated AFTER binding, against the combined row, and a
/// non-`True` result drops the row — the three-valued rule the rest of the
/// engine uses, where an `Unknown` in a filter fails closed.
pub(crate) fn emit_procedure_rows<F>(
    graph: &Graph,
    rows: &[Row],
    bindings: &[Binding],
    where_: Option<&Expr>,
    params: &BTreeMap<String, Value>,
    mut produce: F,
) -> Result<Vec<Row>, RunError>
where
    F: FnMut(&Row) -> Result<Vec<ProcRow>, RunError>,
{
    let mut out = Vec::new();
    for row in rows {
        for produced in produce(row)? {
            let mut r = row.clone();
            for b in bindings {
                r.insert(b.alias.clone(), produced.get(&b.field));
            }
            if let Some(w) = where_ {
                let v = eval_expr(graph, w, &r, params)?;
                if v.truth() != Some(Truth::True) {
                    continue;
                }
            }
            out.push(r);
        }
    }
    Ok(out)
}

/// One row a procedure produced, as `(field name, value)` pairs in the
/// procedure's declared column order.
///
/// A `Vec` and not a map: a procedure has a handful of columns, the lookup is
/// by `&str`, and a linear scan over four entries beats allocating a map for
/// every row of every call.
pub(crate) struct ProcRow(pub(crate) Vec<(&'static str, Value)>);

impl ProcRow {
    /// The value of `field`, or null if the producer did not supply it.
    fn get(&self, field: &str) -> Value {
        self.0
            .iter()
            .find(|(f, _)| *f == field)
            .map_or(Value::Null, |(_, v)| v.clone())
    }
}
