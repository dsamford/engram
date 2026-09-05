//! L1 — the cardinality model, the cost planner's foundation.
//!
//! Measured at SF1, Engram loses every complex analytical query to Neo4j
//! (IC5 26×, IC9 13×, IC3 14×) because a stage's paths and hops execute
//! left-to-right in SOURCE order with no cost input — the friends-of-friends
//! join explodes whichever way the query happened to be written. A cost
//! planner needs cardinality estimates to order that work; this module is
//! where they come from.
//!
//! It is grounded in EXACT counts, not sampled sketches, because the engine
//! already keeps them: [`Graph::count_hop`] answers `|(:A)-[:T]->(:B)|` by
//! iterating the smaller labelled side, and [`Graph::count_label_nodes`] gives
//! `|:A|`. So a hop's average fan-out is `count_hop / |start|`, and a path's
//! cardinality is the seed count times the product of its hops' fan-outs. A
//! variable-length `*min..max` sums the per-length products — the FoF blow-up
//! made visible to the planner.
//!
//! What it deliberately does NOT model: WHERE selectivity, downstream clauses,
//! and degree SKEW (a supernode is averaged in). It answers the STRUCTURAL
//! cardinality the join orderer (L2) ranks candidates by; when two orderings
//! tie, the planner must break the tie on source position, never on a
//! float-equal cost, or the determinism digest destabilises.

use engram_cypher::stmt::{Clause, PathPattern, Query, RelDir};

use crate::{Dir, Graph};

/// How far an unbounded or very long variable-length hop is expanded for the
/// estimate. `KNOWS*` is not summed to infinity — `min..=min+SPAN` yields a
/// finite estimate whose growth still flags the hop as expensive, which is all
/// the planner needs to avoid driving a join from that end.
const VARLEN_EST_SPAN: u64 = 3;

/// Nodes a labelled start scans: the SMALLEST label's count bounds a
/// multi-label MATCH (the labels are an AND), or every node if unlabelled.
fn start_count(g: &Graph, labels: &[String]) -> u64 {
    if labels.is_empty() {
        return g.count_all_nodes();
    }
    labels
        .iter()
        .map(|l| g.count_label_nodes(l))
        .min()
        .unwrap_or(0)
}

impl Graph {
    /// Average matching relationships per start node for one hop —
    /// `count_hop(start, dir, types, end) / |start|`. Undirected sums both
    /// directions. Zero when there are no start nodes.
    pub fn hop_fanout(
        &self,
        start_labels: &[String],
        dir: Dir,
        types: &[String],
        end_labels: &[String],
    ) -> f64 {
        let start = start_count(self, start_labels);
        if start == 0 {
            return 0.0;
        }
        // count_hop debug-asserts a directed hop, so Both is summed as two
        // directed counts rather than passed through.
        let count = match dir {
            Dir::Out | Dir::In => self
                .count_hop_estimate(start_labels, dir, types, end_labels)
                .unwrap_or(0),
            Dir::Both => {
                self.count_hop_estimate(start_labels, Dir::Out, types, end_labels)
                    .unwrap_or(0)
                    + self
                        .count_hop_estimate(start_labels, Dir::In, types, end_labels)
                        .unwrap_or(0)
            }
        };
        count as f64 / start as f64
    }

    /// Estimated rows one path produces, given which of its variables are
    /// already bound (each bound start contributes one row). The seed count
    /// times the product of hop fan-outs; a variable-length hop sums `f^k`
    /// over its (capped) length range.
    pub fn estimate_path_rows(
        &self,
        path: &PathPattern,
        bound: &std::collections::BTreeSet<String>,
    ) -> f64 {
        let start = &path.start;
        let mut card = if start.var.as_ref().is_some_and(|v| bound.contains(v)) {
            // The incoming row already fixes this node: one seed.
            1.0
        } else if start.props.is_some() {
            // A pattern-map equality is a range-index seek — a point lookup.
            1.0
        } else {
            start_count(self, &start.labels) as f64
        };
        let mut cur: &[String] = &start.labels;
        for (rel, node) in &path.hops {
            let dir = match rel.dir {
                RelDir::Out => Dir::Out,
                RelDir::In => Dir::In,
                RelDir::Undirected => Dir::Both,
            };
            let f = self.hop_fanout(cur, dir, &rel.types, &node.labels);
            card *= match &rel.length {
                None => f,
                Some(vl) => {
                    let min = vl.min.unwrap_or(1).max(1);
                    let max = vl
                        .max
                        .unwrap_or(min + VARLEN_EST_SPAN)
                        .min(min + VARLEN_EST_SPAN);
                    (min..=max).map(|k| f.powi(k as i32)).sum::<f64>()
                }
            };
            cur = &node.labels;
        }
        card
    }

    /// Estimated rows the query's FIRST MATCH produces, from label counts and
    /// per-hop fan-out. Independent comma-separated paths multiply (a cartesian
    /// join, which this gets exactly); paths that SHARE a variable also
    /// multiply here, which over-counts a correlated join but preserves the
    /// relative ordering the planner needs. `None` when there is no leading
    /// MATCH (or the query is a UNION).
    pub fn estimate_match_rows(&self, q: &Query) -> Option<f64> {
        let Query::Single(s) = q else {
            return None;
        };
        let pattern = s.clauses.iter().find_map(|c| match c {
            Clause::Match { pattern, .. } => Some(pattern),
            _ => None,
        })?;
        let bound = std::collections::BTreeSet::new();
        Some(
            pattern
                .paths
                .iter()
                .map(|p| self.estimate_path_rows(p, &bound))
                .product(),
        )
    }
}
