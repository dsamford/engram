//! Schema: indexes, constraints, and the two procedures the corpus calls.
//!
//! The census is blunt: exactly two `db.*` procedures are ever called —
//! `db.index.vector.queryNodes` and `db.index.fulltext.queryNodes` — and
//! `CREATE CONSTRAINT` appears at 294 sites. This module gives all three a
//! real implementation over the facade.
//!
//! Uniqueness is enforced AT THE WRITE, and the single-shard synchronous
//! interpreter makes each statement atomic by construction — the property
//! the incumbent's `MERGE` sites assume and Neo4j provides with locks. The
//! online build ladder (for constraining a live, concurrent graph) arrives
//! with the multi-shard work; creating a constraint here validates the
//! EXISTING population first, which is the half that can silently lie.

use std::collections::BTreeMap;

use engram_cypher::stmt::{ConstraintKind, SchemaCmd};
use engram_cypher::{Value, json};
use engram_observe::{counted, sometimes};
use engram_store::StoredValue;

use crate::{Graph, GraphError};

/// A stored index definition.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexDef {
    /// A vector index over one label + property.
    Vector {
        /// The label.
        label: String,
        /// The property carrying the embedding (a float list).
        prop: String,
    },
    /// A fulltext index over labels × properties.
    Fulltext {
        /// The labels (OR).
        labels: Vec<String>,
        /// The properties.
        props: Vec<String>,
    },
    /// A range index (accepted and stored; the scan planner consumes the
    /// node-scoped ones).
    Range {
        /// The label — or the relationship TYPE when `on_relationships`.
        label: String,
        /// The properties.
        props: Vec<String>,
        /// Whether this indexes RELATIONSHIPS. Stored as `on_rel` (absent =
        /// nodes, matching every row written before the field existed) so
        /// `SHOW INDEXES` reports the true scope and the node scan planner
        /// can skip rel indexes instead of applying them to the wrong
        /// population — CREATE used to discard this bit.
        on_relationships: bool,
    },
}

/// A DECLARED range index, as the catalogue holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RangeIndexDef {
    /// The index's name, as `CREATE INDEX <name> ...` gave it.
    pub(crate) name: String,
    /// The label the index is scoped to.
    pub(crate) label: String,
    /// The indexed properties, in declaration order.
    pub(crate) props: Vec<String>,
}

/// Which arm answered a vector query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorArm {
    /// Exact brute-force cosine over the eligible population.
    Exact,
    /// The HNSW index.
    Ann {
        /// Whether this query (re)built the index (first use, a mutation
        /// since the last build, or a different query dimension).
        rebuilt: bool,
    },
}

/// The vector planner's account of one query — returned WITH the result, so
/// "which arm answered" is a fact the caller holds rather than a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorPlan {
    /// Dimension-matched vectors considered.
    pub eligible: usize,
    /// Rows skipped (no vector, wrong dimension, zero norm).
    pub skipped: usize,
    /// The arm that answered.
    pub arm: VectorArm,
}

/// A stored constraint.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintDef {
    /// The label — or the relationship TYPE when `on_relationships`.
    pub label: String,
    /// The properties (one, or a composite tuple).
    pub props: Vec<String>,
    /// What is required.
    pub kind: ConstraintKind,
    /// Whether this constrains RELATIONSHIPS (of type `label`) rather than
    /// nodes. Enforced against the matching population — the node path never
    /// sees a rel constraint and vice versa, so neither silently no-ops.
    pub on_relationships: bool,
}

/// A constraint as ENFORCEMENT holds it: the definition plus its name (the
/// schema-row key), the name-derived marker-family digest, and whether the
/// marker family is built. `markers_built == false` (v1, written before the
/// marker protocol) keeps the O(population) walk until upgraded — a marker
/// MISS on an unbacked constraint must never read as "no duplicate".
#[derive(Clone)]
pub(crate) struct LoadedConstraint {
    pub(crate) name: String,
    pub(crate) def: ConstraintDef,
    pub(crate) digest: [u8; 8],
    pub(crate) markers_built: bool,
}

fn def_to_value(def: &IndexDef) -> Value {
    let mut m = BTreeMap::new();
    match def {
        IndexDef::Vector { label, prop } => {
            m.insert("kind".into(), Value::Str("vector".into()));
            m.insert("label".into(), Value::Str(label.clone()));
            m.insert("prop".into(), Value::Str(prop.clone()));
        }
        IndexDef::Fulltext { labels, props } => {
            m.insert("kind".into(), Value::Str("fulltext".into()));
            m.insert(
                "labels".into(),
                Value::List(labels.iter().cloned().map(Value::Str).collect()),
            );
            m.insert(
                "props".into(),
                Value::List(props.iter().cloned().map(Value::Str).collect()),
            );
        }
        IndexDef::Range {
            label,
            props,
            on_relationships,
        } => {
            m.insert("kind".into(), Value::Str("range".into()));
            m.insert("label".into(), Value::Str(label.clone()));
            m.insert(
                "props".into(),
                Value::List(props.iter().cloned().map(Value::Str).collect()),
            );
            if *on_relationships {
                m.insert("on_rel".into(), Value::Bool(true));
            }
        }
    }
    Value::Map(m)
}

fn value_to_def(v: &Value) -> Option<IndexDef> {
    let Value::Map(m) = v else { return None };
    let s = |k: &str| match m.get(k) {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let list = |k: &str| match m.get(k) {
        Some(Value::List(items)) => {
            let mut out = Vec::new();
            for i in items {
                match i {
                    Value::Str(s) => out.push(s.clone()),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    };
    match s("kind")?.as_str() {
        "vector" => Some(IndexDef::Vector {
            label: s("label")?,
            prop: s("prop")?,
        }),
        "fulltext" => Some(IndexDef::Fulltext {
            labels: list("labels")?,
            props: list("props")?,
        }),
        "range" => Some(IndexDef::Range {
            label: s("label")?,
            props: list("props")?,
            on_relationships: matches!(m.get("on_rel"), Some(Value::Bool(true))),
        }),
        _ => None,
    }
}

impl Graph {
    fn schema_row(&self, family: &str, name: &str) -> Vec<u8> {
        format!("{family}:{name}").into_bytes()
    }

    /// Execute a schema command.
    pub fn apply_schema(&self, cmd: &SchemaCmd) -> Result<(), GraphError> {
        match cmd {
            SchemaCmd::CreateVectorIndex {
                name,
                if_not_exists,
                label,
                prop,
                options: _,
            } => self.create_index(
                name,
                *if_not_exists,
                IndexDef::Vector {
                    label: label.clone(),
                    prop: prop.clone(),
                },
            ),
            SchemaCmd::CreateFulltextIndex {
                name,
                if_not_exists,
                labels,
                props,
            } => self.create_index(
                name,
                *if_not_exists,
                IndexDef::Fulltext {
                    labels: labels.clone(),
                    props: props.clone(),
                },
            ),
            SchemaCmd::CreateRangeIndex {
                name,
                if_not_exists,
                label,
                props,
                on_relationships,
            } => {
                let name = name
                    .clone()
                    .unwrap_or_else(|| format!("range_{label}_{}", props.join("_")));
                self.create_index(
                    &name,
                    *if_not_exists,
                    IndexDef::Range {
                        label: label.clone(),
                        props: props.clone(),
                        on_relationships: *on_relationships,
                    },
                )
            }
            SchemaCmd::CreateConstraint {
                name,
                if_not_exists,
                label,
                props,
                kind,
                on_relationships,
            } => {
                let name = name
                    .clone()
                    .unwrap_or_else(|| format!("constraint_{label}_{}", props.join("_")));
                self.create_constraint(
                    &name,
                    *if_not_exists,
                    ConstraintDef {
                        label: label.clone(),
                        props: props.clone(),
                        kind: *kind,
                        on_relationships: *on_relationships,
                    },
                )
            }
            // SHOW is the one schema command that ANSWERS rather than
            // mutates; `run_stmt` routes it to `show_schema` before this
            // point. Refusing here (rather than answering and dropping the
            // rows) keeps a direct `apply_schema` caller honest.
            SchemaCmd::Show { subject, .. } => Err(GraphError::SchemaConflict(format!(
                "SHOW {subject} produces rows — run it as a statement"
            ))),
            SchemaCmd::DropIndex { name, if_exists } => self.drop_schema("idx", name, *if_exists),
            SchemaCmd::DropConstraint { name, if_exists } => {
                self.drop_schema("con", name, *if_exists)
            }
        }
    }

    fn create_index(
        &self,
        name: &str,
        if_not_exists: bool,
        def: IndexDef,
    ) -> Result<(), GraphError> {
        let row = self.schema_row("idx", name);
        if self.store.get(&self.kv, &row).is_some() {
            if if_not_exists {
                return Ok(());
            }
            return Err(GraphError::SchemaConflict(format!(
                "index `{name}` already exists"
            )));
        }
        let encoded = json::to_json(&def_to_value(&def)).into_bytes();
        self.store
            .put(&self.kv, &row, StoredValue::Plain(encoded))
            .map_err(GraphError::Store)?;
        self.invalidate_vector_indexes();
        // The DECLARED-index catalogue is cached against the schema epoch, and
        // index DDL does NOT bump that epoch (only constraint DDL does). So the
        // cache has to be cleared HERE, or a planner keeps consulting a
        // catalogue taken before this index existed — which is exactly what a
        // test caught: `CREATE INDEX` after a first read left the planner
        // seeing an empty catalogue for the rest of the process.
        *self.range_index_cache.borrow_mut() = None;
        counted!("graph.indexes created");
        Ok(())
    }

    fn create_constraint(
        &self,
        name: &str,
        if_not_exists: bool,
        def: ConstraintDef,
    ) -> Result<(), GraphError> {
        // Schema and data operations cannot share a transaction (the
        // incumbent's rule): the population check + marker backfill below
        // run in their OWN transaction, and opening it over a session's
        // would destroy the session's buffered writes.
        if self.in_txn() {
            return Err(GraphError::SchemaConflict(
                "constraint DDL cannot run inside an open transaction".into(),
            ));
        }
        // Bounded retry: the backfill's read-set is the population, so any
        // concurrent write to a walked entity aborts it — re-run on fresh
        // state rather than surface a spurious failure. A retry that finds
        // violating data returns the violation, exactly as a first run.
        let mut attempt = 0u32;
        loop {
            let txn = self.open_txn();
            let (txn, r) = self.with_txn(txn, || {
                self.create_constraint_in_txn(name, if_not_exists, &def)
            });
            match r {
                Ok(()) => match self.commit_owned(txn) {
                    Ok(()) => return Ok(()),
                    Err(GraphError::TxnConflict) if attempt < 64 => {
                        attempt += 1;
                        counted!("graph.constraint ddl re-run on conflict");
                        std::thread::yield_now();
                        continue;
                    }
                    Err(e) => return Err(e),
                },
                Err(e) => {
                    self.rollback_owned(txn);
                    return Err(e);
                }
            }
        }
    }

    fn create_constraint_in_txn(
        &self,
        name: &str,
        if_not_exists: bool,
        def: &ConstraintDef,
    ) -> Result<(), GraphError> {
        let def = def.clone();
        let row = self.schema_row("con", name);
        // A RECORDED existence read: a racing create of the same name
        // conflicts at validation instead of both writing the row.
        if self.store_get_w(&self.kv, &row).is_some() {
            if if_not_exists {
                return Ok(());
            }
            return Err(GraphError::SchemaConflict(format!(
                "constraint `{name}` already exists"
            )));
        }
        // VALIDATE THE EXISTING POPULATION FIRST — a constraint created over
        // violating data and enforced only forward would certify an integrity
        // rule that does not hold. The walk's reads are RECORDED (this runs
        // inside the DDL's transaction), so a concurrent same-value create
        // aborts one side instead of slipping past the check.
        let population: Vec<(u64, BTreeMap<String, Value>)> = if def.on_relationships {
            self.rels_of_type(&def.label)?
                .into_iter()
                .map(|r| (r.id, r.props))
                .collect()
        } else {
            let mut v = Vec::new();
            for id in self.nodes_by_label(Some(&def.label))? {
                if let Some(Value::Node { props, .. }) = self.node(id)? {
                    v.push((id, props));
                }
            }
            v
        };
        let what = if def.on_relationships {
            "relationship"
        } else {
            "node"
        };
        let digest = crate::constraint_key::constraint_digest(name);
        // Backfill the marker family while validating. `all_encodable` gates
        // the flag: a population member whose tuple the canonical encoding
        // does not cover keeps the constraint on walk enforcement (v1) — a
        // marker MISS must never mean "no duplicate" against an unmarked row.
        let mut all_encodable = true;
        let mut seen: Vec<Vec<Value>> = Vec::new();
        for (who, props) in &population {
            let tuple: Vec<Option<&Value>> = def.props.iter().map(|p| props.get(p)).collect();
            let missing = tuple.iter().any(|v| v.is_none());
            if missing && matches!(def.kind, ConstraintKind::NotNull | ConstraintKind::NodeKey) {
                sometimes!("graph.constraint refused", true);
                return Err(GraphError::ConstraintViolation(format!(
                    "existing {what} {who} lacks `{}` required by `{name}`",
                    def.props.join("`, `")
                )));
            }
            if !missing && matches!(def.kind, ConstraintKind::Unique | ConstraintKind::NodeKey) {
                let t: Vec<Value> = tuple
                    .into_iter()
                    .map(|v| v.expect("checked").clone())
                    .collect();
                let dup = seen.iter().any(|s| {
                    s.iter()
                        .zip(&t)
                        .all(|(a, b)| a.eq3(b) == engram_cypher::Truth::True)
                });
                if dup {
                    sometimes!("graph.constraint refused", true);
                    return Err(GraphError::ConstraintViolation(format!(
                        "existing duplicate `{}` blocks `{name}`",
                        def.props.join("`, `")
                    )));
                }
                let refs: Vec<&Value> = t.iter().collect();
                match crate::constraint_key::canonical_tuple(&refs) {
                    Some(canon) => {
                        self.store_put(
                            &self.index,
                            &crate::constraint_key::marker_body(&digest, &canon),
                            StoredValue::Plain(who.to_be_bytes().to_vec()),
                        )
                        .map_err(GraphError::Store)?;
                    }
                    None => all_encodable = false,
                }
                seen.push(t);
            }
        }
        let markers_built =
            all_encodable || matches!(def.kind, ConstraintKind::NotNull);
        let mut m = BTreeMap::new();
        m.insert("label".into(), Value::Str(def.label.clone()));
        m.insert(
            "props".into(),
            Value::List(def.props.iter().cloned().map(Value::Str).collect()),
        );
        m.insert(
            "kind".into(),
            Value::Str(match def.kind {
                ConstraintKind::Unique => "unique".into(),
                ConstraintKind::NotNull => "notnull".into(),
                ConstraintKind::NodeKey => "nodekey".into(),
            }),
        );
        // Scope is part of the constraint's identity — a rel constraint that
        // decoded as a node one would enforce over the wrong population.
        m.insert("on_rel".into(), Value::Bool(def.on_relationships));
        m.insert("markers".into(), Value::Bool(markers_built));
        let encoded = json::to_json(&Value::Map(m)).into_bytes();
        self.store_put(&self.kv, &row, StoredValue::Plain(encoded))
            .map_err(GraphError::Store)?;
        // The epoch bump commits atomically with the constraint: in-flight
        // writers that enforced against the old list read this key and abort.
        self.bump_constraint_epoch()?;
        counted!("graph.constraints created");
        Ok(())
    }

    fn drop_schema(&self, family: &str, name: &str, if_exists: bool) -> Result<(), GraphError> {
        if family == "con" {
            return self.drop_constraint(name, if_exists);
        }
        let row = self.schema_row(family, name);
        if self.store.get(&self.kv, &row).is_none() {
            if if_exists {
                return Ok(());
            }
            return Err(GraphError::SchemaConflict(format!(
                "`{name}` does not exist"
            )));
        }
        self.store.delete(&self.kv, &row);
        // Same obligation as `create_index`: a dropped index must stop being
        // consulted, and index DDL does not move the schema epoch.
        *self.range_index_cache.borrow_mut() = None;
        Ok(())
    }

    /// Drop a constraint: the schema row, its marker family, and the epoch
    /// bump, in one transaction — a recreate under the same name must not
    /// inherit stale markers. (A writer that loaded the old list and commits
    /// BEFORE this drop can strand a marker our snapshot cannot see; that
    /// straggler is harmless — verify-on-hit re-checks every hit against the
    /// live owner under the CURRENT constraint — and `verify_constraint_markers`
    /// names it.)
    fn drop_constraint(&self, name: &str, if_exists: bool) -> Result<(), GraphError> {
        if self.in_txn() {
            return Err(GraphError::SchemaConflict(
                "constraint DDL cannot run inside an open transaction".into(),
            ));
        }
        let mut attempt = 0u32;
        loop {
            let txn = self.open_txn();
            let (txn, r) = self.with_txn(txn, || {
                let row = self.schema_row("con", name);
                if self.store_get_w(&self.kv, &row).is_none() {
                    if if_exists {
                        return Ok(());
                    }
                    return Err(GraphError::SchemaConflict(format!(
                        "`{name}` does not exist"
                    )));
                }
                self.store_delete_w(&self.kv, &row);
                let digest = crate::constraint_key::constraint_digest(name);
                let mut prefix = vec![crate::constraint_key::MARKER_TAG];
                prefix.extend_from_slice(&digest);
                for body in self.store.scan_bodies_prefix(&self.index, &prefix) {
                    self.store_delete_w(&self.index, &body);
                }
                self.bump_constraint_epoch()?;
                Ok(())
            });
            match r {
                Ok(()) => match self.commit_owned(txn) {
                    Ok(()) => return Ok(()),
                    Err(GraphError::TxnConflict) if attempt < 64 => {
                        attempt += 1;
                        counted!("graph.constraint ddl re-run on conflict");
                        std::thread::yield_now();
                        continue;
                    }
                    Err(e) => return Err(e),
                },
                Err(e) => {
                    self.rollback_owned(txn);
                    return Err(e);
                }
            }
        }
    }

    /// Upgrade v1 constraints (population-walk enforcement) to marker
    /// families — one bounded-retry transaction per constraint, idempotent,
    /// called at server boot. A constraint whose population holds a tuple
    /// the canonical encoding does not cover stays v1 (the walk keeps
    /// enforcing it, correctly); one whose population already holds
    /// duplicates (drift through the phantom this work closes) also stays
    /// v1 and is REPORTED — an upgrade must not certify what does not hold.
    /// Returns `(upgraded, skipped-with-reasons)`.
    pub fn upgrade_constraint_markers(
        &self,
    ) -> Result<(usize, Vec<String>), GraphError> {
        if self.in_txn() {
            return Err(GraphError::SchemaConflict(
                "constraint upgrade cannot run inside an open transaction".into(),
            ));
        }
        let list = self.load_constraints_named()?;
        let mut upgraded = 0usize;
        let mut skipped = Vec::new();
        for c in list {
            if c.markers_built {
                continue;
            }
            let mut attempt = 0u32;
            loop {
                // Delete-and-recreate in ONE transaction: the recreate runs
                // the population check + marker backfill and stamps
                // `markers: true`; the delete first is what lets
                // `create_constraint_in_txn`'s existence check pass.
                let txn = self.open_txn();
                let (txn, r) = self.with_txn(txn, || {
                    let row = self.schema_row("con", &c.name);
                    self.store_delete_w(&self.kv, &row);
                    self.create_constraint_in_txn(&c.name, false, &c.def)
                });
                match r {
                    Ok(()) => match self.commit_owned(txn) {
                        Ok(()) => {
                            upgraded += 1;
                            break;
                        }
                        Err(GraphError::TxnConflict) if attempt < 64 => {
                            attempt += 1;
                            std::thread::yield_now();
                            continue;
                        }
                        Err(e) => return Err(e),
                    },
                    Err(GraphError::ConstraintViolation(why)) => {
                        self.rollback_owned(txn);
                        skipped.push(format!(
                            "`{}` stays walk-enforced: {why}",
                            c.name
                        ));
                        break;
                    }
                    Err(e) => {
                        self.rollback_owned(txn);
                        return Err(e);
                    }
                }
            }
        }
        Ok((upgraded, skipped))
    }

    /// Rebuild every v2 constraint's marker family from the committed
    /// population — the bulk-exit half of the marker contract. Unlogged,
    /// like the bulk rows it covers; single-writer by the bulk contract.
    pub(crate) fn rebuild_constraint_markers_after_bulk(&self) -> Result<(), GraphError> {
        let list = self.load_constraints_named()?;
        for c in list.iter().filter(|c| {
            c.markers_built
                && matches!(c.def.kind, ConstraintKind::Unique | ConstraintKind::NodeKey)
        }) {
            let population: Vec<(u64, BTreeMap<String, Value>)> = if c.def.on_relationships {
                self.rels_of_type(&c.def.label)?
                    .into_iter()
                    .map(|r| (r.id, r.props))
                    .collect()
            } else {
                let mut v = Vec::new();
                for id in self.nodes_by_label(Some(&c.def.label))? {
                    if let Some(Value::Node { props, .. }) = self.node(id)? {
                        v.push((id, props));
                    }
                }
                v
            };
            for (id, props) in &population {
                if let Some(key) = self.marker_key_for(c, props) {
                    self.store
                        .put_unlogged(
                            &self.index,
                            &key,
                            StoredValue::Plain(id.to_be_bytes().to_vec()),
                        )
                        .map_err(GraphError::Store)?;
                }
            }
        }
        Ok(())
    }

    /// FSCK for the marker protocol: every complete, encodable tuple in a
    /// v2 constraint's population must have a marker resolving to a live
    /// owner with an eq3-equal tuple. Returns problems; empty is healthy.
    /// (Orphan markers — owner gone or moved — are self-healing via
    /// verify-on-hit and are reported as notes, not failures.)
    pub fn verify_constraint_markers(&self) -> Result<Vec<String>, GraphError> {
        let mut problems = Vec::new();
        for c in self.load_constraints_named()? {
            if !c.markers_built
                || !matches!(c.def.kind, ConstraintKind::Unique | ConstraintKind::NodeKey)
            {
                continue;
            }
            let population: Vec<(u64, BTreeMap<String, Value>)> = if c.def.on_relationships {
                self.rels_of_type(&c.def.label)?
                    .into_iter()
                    .map(|r| (r.id, r.props))
                    .collect()
            } else {
                let mut v = Vec::new();
                for id in self.nodes_by_label(Some(&c.def.label))? {
                    if let Some(Value::Node { props, .. }) = self.node(id)? {
                        v.push((id, props));
                    }
                }
                v
            };
            for (id, props) in &population {
                let Some(key) = self.marker_key_for(&c, props) else {
                    continue; // un-encodable tuple: walk-enforced, no marker owed
                };
                match self.store.get(&self.index, &key) {
                    None => problems.push(format!(
                        "constraint `{}`: {} {id} has no marker for its tuple",
                        c.name,
                        if c.def.on_relationships {
                            "relationship"
                        } else {
                            "node"
                        },
                    )),
                    Some(v) => {
                        let owner = v
                            .get(..8)
                            .and_then(|b| <[u8; 8]>::try_from(b).ok())
                            .map(u64::from_be_bytes);
                        let ok = match owner {
                            Some(o) if o == *id => true,
                            Some(o) => {
                                // Another owner claims the tuple — legal only
                                // if that owner is live and eq3-equal (i.e.
                                // this is a pre-existing duplicate the
                                // constraint predates, reported separately).
                                let t: Vec<&Value> = c
                                    .def
                                    .props
                                    .iter()
                                    .filter_map(|p| props.get(p))
                                    .collect();
                                self.marker_owner_live(&c, c.def.on_relationships, o, &t)?
                            }
                            None => false,
                        };
                        if !ok {
                            problems.push(format!(
                                "constraint `{}`: marker for {} {id}'s tuple resolves to a \
                                 dead or mismatched owner",
                                c.name,
                                if c.def.on_relationships {
                                    "relationship"
                                } else {
                                    "node"
                                },
                            ));
                        }
                    }
                }
            }
        }
        Ok(problems)
    }

    /// Look an index up by name.
    /// Every vector index, as `(name, label, prop)` — the write path's
    /// membership test. Corrupt rows are skipped, not fatal (the reader
    /// never trusts a single side).
    /// The DECLARED range indexes, read back from the schema catalogue.
    ///
    /// Cached against the schema epoch exactly as `constraints_snapshot` is,
    /// and for the same two reasons: the planner consults it per statement, and
    /// a DDL that commits after a statement's snapshot must abort that
    /// statement rather than let it keep planning against a stale catalogue.
    pub(crate) fn declared_range_indexes(
        &self,
    ) -> Result<std::sync::Arc<Vec<RangeIndexDef>>, GraphError> {
        if let Some((_, list)) = self.range_index_cache.borrow().as_ref() {
            counted!("graph.range index catalogue served from cache");
            self.store_note_read(&self.kv, Self::CON_EPOCH_BODY);
            return Ok(std::sync::Arc::clone(list));
        }
        let epoch = self.constraint_epoch_recorded();
        let list = std::sync::Arc::new(self.scan_range_indexes());
        *self.range_index_cache.borrow_mut() = Some((epoch, std::sync::Arc::clone(&list)));
        Ok(list)
    }

    /// Every `IndexDef::Range` in the catalogue.
    pub(crate) fn scan_range_indexes(&self) -> Vec<RangeIndexDef> {
        let mut out = Vec::new();
        for (body, bytes) in self.store.scan_body_prefix(&self.kv, b"idx:") {
            let Some(pos) = body.windows(4).position(|w| w == b"idx:") else {
                continue;
            };
            let name = String::from_utf8_lossy(&body[pos + 4..]).into_owned();
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let Ok(v) = json::from_json(&text) else {
                continue;
            };
            if let Some(IndexDef::Range {
                label,
                props,
                on_relationships,
            }) = value_to_def(&v)
            {
                // The planner consumes NODE indexes only — a rel-scoped
                // range index consulted for a node scan would "accelerate"
                // the wrong population. Rel range indexes are stored and
                // SHOWable, but never planned against.
                if on_relationships {
                    continue;
                }
                out.push(RangeIndexDef { name, label, props });
            }
        }
        // Deterministic order: the planner picks among these, and a rule whose
        // answer depended on catalogue scan order would diverge between two
        // runs of the same seed — which the determinism gate compares.
        out.sort_by(|a, b| (&a.label, &a.props, &a.name).cmp(&(&b.label, &b.props, &b.name)));
        out
    }

    pub(crate) fn scan_vector_indexes(&self) -> Vec<crate::VecIndex> {
        let mut out = Vec::new();
        for (body, bytes) in self.store.scan_body_prefix(&self.kv, b"idx:") {
            let Some(pos) = body.windows(4).position(|w| w == b"idx:") else {
                continue;
            };
            let name = String::from_utf8_lossy(&body[pos + 4..]).into_owned();
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let Ok(v) = json::from_json(&text) else {
                continue;
            };
            if let Some(IndexDef::Vector { label, prop }) = value_to_def(&v) {
                out.push(crate::VecIndex { name, label, prop });
            }
        }
        out
    }

    /// The stored definition of index `name`, or `None` if absent.
    pub fn index_def(&self, name: &str) -> Result<Option<IndexDef>, GraphError> {
        let Some(bytes) = self.store.get(&self.kv, &self.schema_row("idx", name)) else {
            return Ok(None);
        };
        let text =
            String::from_utf8(bytes).map_err(|_| GraphError::Corrupt("index def utf8".into()))?;
        let v = json::from_json(&text).map_err(GraphError::Corrupt)?;
        value_to_def(&v)
            .ok_or_else(|| GraphError::Corrupt("index def shape".into()))
            .map(Some)
    }

    /// Answer `SHOW INDEXES` / `SHOW CONSTRAINTS` from the schema catalogue:
    /// `(columns, rows)` in Neo4j's column vocabulary — tools key on those
    /// exact names. Unimplemented subjects refuse BY NAME (the same contract
    /// procedures follow), and a YIELD/WHERE tail refuses rather than
    /// answering with the projection silently ignored.
    pub fn show_schema(
        &self,
        subject: &str,
        tail: bool,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>), GraphError> {
        let up = subject.to_ascii_uppercase();
        let known = matches!(up.as_str(), "INDEX" | "INDEXES" | "CONSTRAINT" | "CONSTRAINTS");
        if known && tail {
            return Err(GraphError::SchemaConflict(format!(
                "SHOW {subject} with a trailing clause (YIELD/WHERE/RETURN) is not supported yet"
            )));
        }
        match up.as_str() {
            "INDEX" | "INDEXES" => {
                counted!("graph.show indexes");
                let mut rows = Vec::new();
                for (name, def) in self.scan_index_defs() {
                    let (ty, entity, labels, props) = match def {
                        IndexDef::Vector { label, prop } => {
                            ("VECTOR", "NODE", vec![label], vec![prop])
                        }
                        IndexDef::Fulltext { labels, props } => ("FULLTEXT", "NODE", labels, props),
                        IndexDef::Range {
                            label,
                            props,
                            on_relationships,
                        } => (
                            "RANGE",
                            if on_relationships { "RELATIONSHIP" } else { "NODE" },
                            vec![label],
                            props,
                        ),
                    };
                    rows.push(vec![
                        Value::Str(name),
                        Value::Str(ty.into()),
                        Value::Str(entity.into()),
                        Value::List(labels.into_iter().map(Value::Str).collect()),
                        Value::List(props.into_iter().map(Value::Str).collect()),
                        // Index population is synchronous here — a stored
                        // index is a usable index, so state is a constant.
                        Value::Str("ONLINE".into()),
                    ]);
                }
                Ok((
                    [
                        "name",
                        "type",
                        "entityType",
                        "labelsOrTypes",
                        "properties",
                        "state",
                    ]
                    .map(String::from)
                    .to_vec(),
                    rows,
                ))
            }
            "CONSTRAINT" | "CONSTRAINTS" => {
                counted!("graph.show constraints");
                let mut cs = self.load_constraints_named()?;
                cs.sort_by(|a, b| a.name.cmp(&b.name));
                let rows = cs
                    .into_iter()
                    .map(|c| {
                        let ty = match (c.def.kind, c.def.on_relationships) {
                            (ConstraintKind::Unique, false) => "UNIQUENESS",
                            (ConstraintKind::Unique, true) => "RELATIONSHIP_UNIQUENESS",
                            (ConstraintKind::NotNull, false) => "NODE_PROPERTY_EXISTENCE",
                            (ConstraintKind::NotNull, true) => "RELATIONSHIP_PROPERTY_EXISTENCE",
                            (ConstraintKind::NodeKey, false) => "NODE_KEY",
                            (ConstraintKind::NodeKey, true) => "RELATIONSHIP_KEY",
                        };
                        let entity = if c.def.on_relationships {
                            "RELATIONSHIP"
                        } else {
                            "NODE"
                        };
                        vec![
                            Value::Str(c.name),
                            Value::Str(ty.into()),
                            Value::Str(entity.into()),
                            Value::List(vec![Value::Str(c.def.label)]),
                            Value::List(c.def.props.into_iter().map(Value::Str).collect()),
                        ]
                    })
                    .collect();
                Ok((
                    ["name", "type", "entityType", "labelsOrTypes", "properties"]
                        .map(String::from)
                        .to_vec(),
                    rows,
                ))
            }
            _ => Err(GraphError::SchemaConflict(format!(
                "SHOW {subject} is not supported yet"
            ))),
        }
    }

    /// Every stored index as `(name, def)`, name-ordered — two runs of one
    /// seed must render one catalogue (the determinism gate compares output).
    fn scan_index_defs(&self) -> Vec<(String, IndexDef)> {
        let mut out = Vec::new();
        for (body, bytes) in self.store.scan_body_prefix(&self.kv, b"idx:") {
            let Some(pos) = body.windows(4).position(|w| w == b"idx:") else {
                continue;
            };
            let name = String::from_utf8_lossy(&body[pos + 4..]).into_owned();
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let Ok(v) = json::from_json(&text) else {
                continue;
            };
            if let Some(def) = value_to_def(&v) {
                out.push((name, def));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Decode every stored constraint, both scopes. Callers filter by scope:
    /// [`Self::constraints_for`] (nodes) and [`Self::rel_constraints_for`]
    /// (relationships) must each see ONLY their own, or a constraint validated
    /// against the wrong population would silently never fire.
    fn load_constraints(&self) -> Result<Vec<ConstraintDef>, GraphError> {
        Ok(self
            .load_constraints_named()?
            .into_iter()
            .map(|c| c.def)
            .collect())
    }

    /// Constraints as ENFORCEMENT sees them: the def plus the name-derived
    /// marker identity and whether the marker family is built (see
    /// `constraint_key`). Reads the committed schema rows.
    pub(crate) fn load_constraints_named(&self) -> Result<Vec<LoadedConstraint>, GraphError> {
        let mut out = Vec::new();
        for (body, bytes) in self.store.scan_body_prefix(&self.kv, b"con:") {
            let name = String::from_utf8(body.get(4..).unwrap_or_default().to_vec())
                .map_err(|_| GraphError::Corrupt("constraint name utf8".into()))?;
            let (def, markers_built) = Self::decode_constraint(&bytes)?;
            out.push(LoadedConstraint {
                digest: crate::constraint_key::constraint_digest(&name),
                name,
                def,
                markers_built,
            });
        }
        Ok(out)
    }

    fn decode_constraint(bytes: &[u8]) -> Result<(ConstraintDef, bool), GraphError> {
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| GraphError::Corrupt("constraint utf8".into()))?;
        let v = json::from_json(&text).map_err(GraphError::Corrupt)?;
        let Value::Map(m) = v else {
            return Err(GraphError::Corrupt("constraint shape".into()));
        };
        let (Some(Value::Str(label)), Some(Value::List(props)), Some(Value::Str(kind))) =
            (m.get("label"), m.get("props"), m.get("kind"))
        else {
            return Err(GraphError::Corrupt("constraint fields".into()));
        };
        let mut ps = Vec::with_capacity(props.len());
        for p in props {
            match p {
                Value::Str(s) => ps.push(s.clone()),
                _ => return Err(GraphError::Corrupt("constraint prop".into())),
            }
        }
        // `on_rel` is absent on constraints written before relationship
        // constraints existed — those are all node constraints. `markers` is
        // absent on constraints written before the marker protocol (v1) —
        // those are enforced by the population walk until upgraded.
        let on_relationships = matches!(m.get("on_rel"), Some(Value::Bool(true)));
        let markers_built = matches!(m.get("markers"), Some(Value::Bool(true)));
        Ok((
            ConstraintDef {
                label: label.clone(),
                props: ps,
                kind: match kind.as_str() {
                    "unique" => ConstraintKind::Unique,
                    "notnull" => ConstraintKind::NotNull,
                    "nodekey" => ConstraintKind::NodeKey,
                    _ => return Err(GraphError::Corrupt("constraint kind".into())),
                },
                on_relationships,
            },
            markers_built,
        ))
    }

    /// The schema EPOCH row — bumped by every constraint DDL, READ (recorded)
    /// by every constrained write's enforcement. This is what makes
    /// constraint DDL OCC-visible: an in-flight transaction that enforced
    /// against the old constraint list read this key; the DDL's commit moves
    /// it; validation aborts the writer, whose re-run reloads the list. The
    /// body deliberately does NOT start with `con:` so the schema-row scan
    /// never sees it.
    pub(crate) const CON_EPOCH_BODY: &'static [u8] = b"con\x00epoch";

    /// The committed schema epoch — a RECORDED read inside a transaction.
    fn constraint_epoch_recorded(&self) -> u64 {
        self.store_get_w(&self.kv, Self::CON_EPOCH_BODY)
            .and_then(|b| b.try_into().ok().map(u64::from_le_bytes))
            .unwrap_or(0)
    }

    /// The constraint list, cached against the schema epoch: one recorded
    /// point read per enforcement (needed for OCC-visibility anyway) instead
    /// of a scan + JSON decode per constrained write.
    pub(crate) fn constraints_snapshot(
        &self,
    ) -> Result<std::sync::Arc<Vec<LoadedConstraint>>, GraphError> {
        // The HIT path, and the reason this function is shaped this way: the
        // epoch probe is an ALWAYS-ABSENT KV read that the sparse index cannot
        // reject, so it descends every sealed segment on every constrained
        // write. We only ever used its answer to decide whether the cache was
        // current — and the cache is invalidated directly by
        // `bump_constraint_epoch`, which is the only writer of that key and
        // runs on the one `Arc<Graph>` every session shares.
        //
        // So register the read instead of performing it. Validation asks only
        // whether the key MOVED since the snapshot, never what it said, so the
        // verdict and the abort set are unchanged: a constraint DDL committing
        // after our snapshot still moves `kv/con\0epoch` and still aborts every
        // in-flight enforcing writer.
        if self.constraint_epoch_cache.get() {
            if let Some((_, list)) = self.constraint_cache.borrow().as_ref() {
                counted!("graph.constraint epoch served from cache");
                self.store_note_read(&self.kv, Self::CON_EPOCH_BODY);
                return Ok(std::sync::Arc::clone(list));
            }
        } else {
            // The differential arm: probe, compare, and only then serve.
            let epoch = self.constraint_epoch_recorded();
            if let Some((e, list)) = self.constraint_cache.borrow().as_ref() {
                if *e == epoch {
                    return Ok(std::sync::Arc::clone(list));
                }
            }
        }
        // COLD: the real recorded read, exactly as before. A miss must still
        // learn the epoch, because that is what a later probe-arm hit compares
        // against.
        let epoch = self.constraint_epoch_recorded();
        let list = std::sync::Arc::new(self.load_constraints_named()?);
        *self.constraint_cache.borrow_mut() = Some((epoch, std::sync::Arc::clone(&list)));
        Ok(list)
    }

    /// Bump the schema epoch — WITHIN the caller's transaction, so the bump
    /// commits atomically with the DDL it describes.
    fn bump_constraint_epoch(&self) -> Result<(), GraphError> {
        let next = self.constraint_epoch_recorded() + 1;
        self.store_put(
            &self.kv,
            Self::CON_EPOCH_BODY,
            StoredValue::Plain(next.to_le_bytes().to_vec()),
        )
        .map_err(GraphError::Store)?;
        *self.constraint_cache.borrow_mut() = None;
        Ok(())
    }

    /// Every NODE constraint whose label is in `labels`.
    pub fn constraints_for(&self, labels: &[String]) -> Result<Vec<ConstraintDef>, GraphError> {
        Ok(self
            .load_constraints()?
            .into_iter()
            .filter(|c| !c.on_relationships && labels.contains(&c.label))
            .collect())
    }

    /// Every RELATIONSHIP constraint on `rel_type`.
    pub fn rel_constraints_for(&self, rel_type: &str) -> Result<Vec<ConstraintDef>, GraphError> {
        Ok(self
            .load_constraints()?
            .into_iter()
            .filter(|c| c.on_relationships && c.label == rel_type)
            .collect())
    }

    /// Enforce constraints for a node about to hold `labels` × `props`.
    /// `owner` is the node being written (fresh-minted for a create); its
    /// own current value never counts as a duplicate of itself.
    ///
    /// For a v2 constraint (marker family built) this is 1–2 recorded point
    /// reads plus a buffered marker put — and, because both racing writers
    /// of one value WRITE the same marker key, OCC validation turns the
    /// concurrent-duplicate phantom into an ordinary conflict. For a v1
    /// constraint, or a tuple the canonical encoding does not cover, the
    /// O(population) walk still answers.
    pub fn enforce_constraints(
        &self,
        owner: u64,
        labels: &[String],
        props: &BTreeMap<String, Value>,
    ) -> Result<(), GraphError> {
        let cons = self.constraints_snapshot()?;
        for c in cons.iter() {
            if c.def.on_relationships || !labels.contains(&c.def.label) {
                continue;
            }
            self.enforce_one(c, owner, false, props)?;
        }
        Ok(())
    }

    fn enforce_one(
        &self,
        c: &LoadedConstraint,
        owner: u64,
        is_rel: bool,
        props: &BTreeMap<String, Value>,
    ) -> Result<(), GraphError> {
        let tuple: Vec<Option<&Value>> = c
            .def
            .props
            .iter()
            .map(|p| props.get(p).filter(|v| !matches!(v, Value::Null)))
            .collect();
        let missing = tuple.iter().any(|v| v.is_none());
        if missing && matches!(c.def.kind, ConstraintKind::NotNull | ConstraintKind::NodeKey) {
            sometimes!("graph.constraint refused", true);
            return Err(GraphError::ConstraintViolation(format!(
                "`{}`.`{}` is required",
                c.def.label,
                c.def.props.join("`, `")
            )));
        }
        // Uniqueness applies to rows with a COMPLETE tuple — null
        // components exempt a row, Neo4j's rule.
        if missing || !matches!(c.def.kind, ConstraintKind::Unique | ConstraintKind::NodeKey) {
            return Ok(());
        }
        let t: Vec<&Value> = tuple.into_iter().map(|v| v.expect("checked")).collect();
        if c.markers_built {
            if let Some(canon) = crate::constraint_key::canonical_tuple(&t) {
                return self.enforce_marker(c, owner, is_rel, &t, &canon);
            }
        }
        self.enforce_walk(c, owner, is_rel, &t)
    }

    /// The marker path — see `constraint_key` for the key and the protocol.
    fn enforce_marker(
        &self,
        c: &LoadedConstraint,
        owner: u64,
        is_rel: bool,
        t: &[&Value],
        canon: &[u8],
    ) -> Result<(), GraphError> {
        let key = crate::constraint_key::marker_body(&c.digest, canon);
        let hit = self.store_get_w(&self.index, &key);
        let other = hit
            .as_deref()
            .and_then(|v| v.get(..8))
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_be_bytes);
        match other {
            // Idempotent re-mark of our own value (a SET to the same tuple).
            Some(o) if o == owner => Ok(()),
            Some(o) => {
                // VERIFY-ON-HIT: refuse only against a LIVE owner whose tuple
                // is still eq3-equal. Everything stale — a crash-stranded
                // marker, a digest collision against a different tuple, an
                // owner that lost the label — heals by overwrite, so no
                // marker defect is ever silent corruption.
                if self.marker_owner_live(c, is_rel, o, t)? {
                    sometimes!("graph.constraint refused", true);
                    counted!("graph.constraint marker refused a duplicate");
                    Graph::note_unique_refusal(is_rel, o);
                    return Err(GraphError::ConstraintViolation(format!(
                        "`{}`.(`{}`) already exists on {} {o}",
                        c.def.label,
                        c.def.props.join("`, `"),
                        if is_rel { "relationship" } else { "node" },
                    )));
                }
                counted!("graph.constraint marker healed");
                self.store_put(
                    &self.index,
                    &key,
                    StoredValue::Plain(owner.to_be_bytes().to_vec()),
                )
                .map_err(GraphError::Store)?;
                Ok(())
            }
            None => {
                // A recorded MISS (the read-set covers it) plus a buffered
                // put: two racing writers of one fresh value now collide
                // write-write at validation — the phantom, closed.
                self.store_put(
                    &self.index,
                    &key,
                    StoredValue::Plain(owner.to_be_bytes().to_vec()),
                )
                .map_err(GraphError::Store)?;
                Ok(())
            }
        }
    }

    /// Is `other` a live entity that still carries `c`'s label/type and an
    /// eq3-equal tuple? Recorded reads — a refusal based on `other` must
    /// abort if `other` moves before we commit.
    fn marker_owner_live(
        &self,
        c: &LoadedConstraint,
        is_rel: bool,
        other: u64,
        t: &[&Value],
    ) -> Result<bool, GraphError> {
        if is_rel {
            let Some(r) = self.rel(other)? else {
                return Ok(false);
            };
            if r.rel_type != c.def.label {
                return Ok(false);
            }
            Ok(c.def.props.iter().zip(t).all(|(p, v)| {
                r.props
                    .get(p)
                    .is_some_and(|o| o.eq3(v) == engram_cypher::Truth::True)
            }))
        } else {
            let Some(Value::Node { labels, props, .. }) = self.node(other)? else {
                return Ok(false);
            };
            if !labels.contains(&c.def.label) {
                return Ok(false);
            }
            Ok(c.def.props.iter().zip(t).all(|(p, v)| {
                props
                    .get(p)
                    .is_some_and(|o| o.eq3(v) == engram_cypher::Truth::True)
            }))
        }
    }

    /// The v1 / uncovered-tuple fallback: the population walk, exactly as it
    /// always worked.
    fn enforce_walk(
        &self,
        c: &LoadedConstraint,
        owner: u64,
        is_rel: bool,
        t: &[&Value],
    ) -> Result<(), GraphError> {
        if is_rel {
            for other in self.rels_of_type(&c.def.label)? {
                if other.id == owner {
                    continue;
                }
                let same = c.def.props.iter().zip(t).all(|(p, v)| {
                    other
                        .props
                        .get(p)
                        .is_some_and(|o| o.eq3(v) == engram_cypher::Truth::True)
                });
                if same {
                    sometimes!("graph.constraint refused", true);
                    Graph::note_unique_refusal(true, other.id);
                    return Err(GraphError::ConstraintViolation(format!(
                        "`{}`.(`{}`) already exists on relationship {}",
                        c.def.label,
                        c.def.props.join("`, `"),
                        other.id
                    )));
                }
            }
        } else {
            for id in self.nodes_by_label(Some(&c.def.label))? {
                if id == owner {
                    continue;
                }
                let Some(Value::Node { props: other, .. }) = self.node(id)? else {
                    continue;
                };
                let same = c.def.props.iter().zip(t).all(|(p, v)| {
                    other
                        .get(p)
                        .is_some_and(|o| o.eq3(v) == engram_cypher::Truth::True)
                });
                if same {
                    sometimes!("graph.constraint refused", true);
                    Graph::note_unique_refusal(false, id);
                    return Err(GraphError::ConstraintViolation(format!(
                        "`{}`.(`{}`) already exists on node {id}",
                        c.def.label,
                        c.def.props.join("`, `")
                    )));
                }
            }
        }
        Ok(())
    }

    /// Ownership-checked marker removal for `owner`'s current tuples under
    /// every applicable v2 constraint — the delete half of the protocol.
    /// `applies_to` is the node's labels (or the one relationship type).
    pub(crate) fn remove_constraint_markers(
        &self,
        owner: u64,
        is_rel: bool,
        applies_to: &[String],
        props: &BTreeMap<String, Value>,
    ) -> Result<(), GraphError> {
        let cons = self.constraints_snapshot()?;
        for c in cons.iter() {
            if c.def.on_relationships != is_rel
                || !c.markers_built
                || !matches!(c.def.kind, ConstraintKind::Unique | ConstraintKind::NodeKey)
                || !applies_to.contains(&c.def.label)
            {
                continue;
            }
            let Some(key) = self.marker_key_for(c, props) else {
                continue;
            };
            let owned = self
                .store_get_w(&self.index, &key)
                .as_deref()
                .and_then(|v| v.get(..8))
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_be_bytes)
                == Some(owner);
            if owned {
                self.store_delete_w(&self.index, &key);
            }
        }
        Ok(())
    }

    /// The marker key for `props`' tuple under `c`, if complete and
    /// canonically encodable.
    fn marker_key_for(
        &self,
        c: &LoadedConstraint,
        props: &BTreeMap<String, Value>,
    ) -> Option<Vec<u8>> {
        let tuple: Vec<&Value> = c
            .def
            .props
            .iter()
            .map(|p| props.get(p).filter(|v| !matches!(v, Value::Null)))
            .collect::<Option<Vec<_>>>()?;
        let canon = crate::constraint_key::canonical_tuple(&tuple)?;
        Some(crate::constraint_key::marker_body(&c.digest, &canon))
    }

    /// Marker moves for an UPDATE: delete the pre-image tuple's marker
    /// (ownership-checked) wherever the tuple CHANGED. The post-image marker
    /// is placed by the enforcement call the writer already makes — run this
    /// AFTER enforcement, so a refusal leaves every marker untouched.
    pub(crate) fn move_constraint_markers(
        &self,
        owner: u64,
        is_rel: bool,
        applies_to: &[String],
        pre: &BTreeMap<String, Value>,
        post: &BTreeMap<String, Value>,
    ) -> Result<(), GraphError> {
        let cons = self.constraints_snapshot()?;
        for c in cons.iter() {
            if c.def.on_relationships != is_rel
                || !c.markers_built
                || !matches!(c.def.kind, ConstraintKind::Unique | ConstraintKind::NodeKey)
                || !applies_to.contains(&c.def.label)
            {
                continue;
            }
            let pre_key = self.marker_key_for(c, pre);
            let post_key = self.marker_key_for(c, post);
            if pre_key == post_key {
                continue; // the tuple did not move
            }
            if let Some(key) = pre_key {
                let owned = self
                    .store_get_w(&self.index, &key)
                    .as_deref()
                    .and_then(|v| v.get(..8))
                    .and_then(|b| <[u8; 8]>::try_from(b).ok())
                    .map(u64::from_be_bytes)
                    == Some(owner);
                if owned {
                    self.store_delete_w(&self.index, &key);
                }
            }
        }
        Ok(())
    }

    /// Enforce RELATIONSHIP constraints for an edge of `rel_type` about to
    /// hold `props`. `owner` is the relationship being written. The mirror of
    /// [`Self::enforce_constraints`] over the relationship population — same
    /// null-exempts-uniqueness rule, same refusals, same marker protocol.
    pub fn enforce_rel_constraints(
        &self,
        owner: u64,
        rel_type: &str,
        props: &BTreeMap<String, Value>,
    ) -> Result<(), GraphError> {
        let cons = self.constraints_snapshot()?;
        for c in cons.iter() {
            if !c.def.on_relationships || c.def.label != rel_type {
                continue;
            }
            self.enforce_one(c, owner, true, props)?;
        }
        Ok(())
    }

    // ── The two live procedures ─────────────────────────────────────────

    /// `db.index.vector.queryNodes(name, k, query)` — the two-arm planner.
    ///
    /// R26's rule as code: at or below the crossover the EXACT arm runs
    /// (brute force is exact AND faster there); above it the HNSW arm runs,
    /// rebuilt whenever the graph's mutation epoch moved (a stale
    /// approximate index is a correctness bug, not a performance trade).
    /// Which arm answered is REPORTED, never inferred — and the returned
    /// scores are exact f64 cosines on BOTH arms (ANN candidates are
    /// rescored), so a ranking cannot reveal the arm by its precision.
    pub fn vector_query(
        &self,
        index: &str,
        k: usize,
        query: &[f64],
    ) -> Result<(Vec<(Value, f64)>, VectorPlan), GraphError> {
        let Some(def) = self.index_def(index)? else {
            return Err(GraphError::SchemaConflict(format!(
                "no such index `{index}`"
            )));
        };
        let IndexDef::Vector { label, prop } = def else {
            return Err(GraphError::SchemaConflict(format!(
                "`{index}` is not a vector index"
            )));
        };
        let qnorm = query.iter().map(|x| x * x).sum::<f64>().sqrt();
        if qnorm == 0.0 {
            return Err(GraphError::BadPropertyValue("a zero query vector".into()));
        }
        let epoch = self.mutation_epoch();
        let dim = query.len();

        let exact_cosine = |v: &[f64]| -> f64 {
            let dot: f64 = v.iter().zip(query).map(|(a, b)| a * b).sum();
            dot / (v.iter().map(|x| x * x).sum::<f64>().sqrt() * qnorm)
        };

        // INCREMENTAL MAINTENANCE: fold the writes this index has seen since
        // it was last current into its cached HNSW and vectors, id by id — a
        // stream of writes no longer rebuilds. The `vectors` map is the
        // liveness truth: a deleted id leaves it (a stale HNSW node then
        // rescores to nothing and is filtered), an updated id's vector is
        // replaced there. A delta past its cap, a cross into the ANN arm, or
        // HNSW bloat past the ratio falls back to the cold rebuild. Writes to
        // OTHER indexes leave this delta empty, so they cost this index
        // nothing — the epoch-equality invalidation this replaces rebuilt on
        // every write anywhere.
        // Inside a transaction with buffered writes this query answers
        // COMMITTED state (as the incumbent's vector indexes do — they are
        // not transaction-consistent, so a transaction does not see its own
        // uncommitted embeddings). The shared delta stream and the shared
        // ANN cache are therefore left untouched here: consuming a delta on
        // this thread would resolve upserts through the transaction's
        // buffered records into a cache every session shares, and this
        // transaction's own changes reach the deltas only at commit
        // (`TxnTouched::vectors`).
        let in_txn_writes = self.in_txn_with_writes();
        let mut force_rebuild = false;
        if let Some(delta) = (!in_txn_writes)
            .then(|| self.vector_deltas.borrow_mut().remove(index))
            .flatten()
        {
            if delta.overflow {
                force_rebuild = true;
            } else {
                // Gather the upserts' current vectors BEFORE borrowing the
                // cache — node_vector reads the store, which the cache borrow
                // must not span.
                let mut inserts: Vec<(u64, Vec<f64>)> = Vec::with_capacity(delta.upserts.len());
                for id in &delta.upserts {
                    if let Some(v) = self.node_vector(*id, &prop)? {
                        if v.len() == dim {
                            inserts.push((*id, v));
                        }
                    }
                }
                let present: std::collections::BTreeSet<u64> =
                    inserts.iter().map(|(id, _)| *id).collect();
                let mut cache = self.ann_cache.borrow_mut();
                if let Some(e) = cache.get_mut(index) {
                    if e.dim == dim {
                        let vectors = std::sync::Arc::make_mut(&mut e.vectors);
                        for id in &delta.deletes {
                            vectors.remove(id);
                        }
                        for id in &delta.upserts {
                            if !present.contains(id) {
                                vectors.remove(id); // the vector is gone
                            }
                        }
                        for (id, v) in &inserts {
                            vectors.insert(*id, v.clone());
                        }
                        e.epoch = epoch;
                        if vectors.len() > self.vector_exact_max.get() {
                            let bloat = e.index.len() as f64
                                > vectors.len() as f64 * (1.0 + crate::VECTOR_BLOAT_RATIO);
                            if e.index.is_empty() || bloat {
                                force_rebuild = true; // exact->ann cross, or too many dead
                            } else {
                                let hnsw = std::sync::Arc::make_mut(&mut e.index);
                                for (id, v) in &inserts {
                                    let vf: Vec<f32> = v.iter().map(|x| *x as f32).collect();
                                    hnsw.insert(*id, &vf);
                                }
                                counted!("graph.vector index incrementally maintained");
                            }
                        } else {
                            counted!("graph.vector index incrementally maintained");
                        }
                    } else {
                        force_rebuild = true;
                    }
                }
            }
        }

        // WARM PATH: a fresh cache means the world has not changed since the
        // build — no node record is touched until the k winners materialise.
        // (The bench harness found the cold gather running per query and
        // costing as much as the exact scan.)
        {
            let cache = self.ann_cache.borrow();
            if let Some(e) = cache.get(index) {
                // An exact-cached entry holds its vectors but an EMPTY HNSW;
                // it serves the exact arm but not the ANN arm. Usable when
                // this query would take the exact arm, or the cached HNSW
                // actually covers the vectors. (Only a runtime change to
                // `vector_exact_max` — the tests — moves an index across the
                // crossover; production's is fixed.)
                // Usable when the query takes the exact arm, or the HNSW
                // covers the live set (incremental inserts leave it with dead
                // nodes, so `>=`, not `==`).
                let usable = e.vectors.len() <= self.vector_exact_max.get()
                    || e.index.len() >= e.vectors.len();
                if !force_rebuild && e.dim == dim && usable {
                    let (vectors, skipped, h) = (
                        std::sync::Arc::clone(&e.vectors),
                        e.skipped,
                        std::sync::Arc::clone(&e.index),
                    );
                    drop(cache);
                    let mut ranked: Vec<(u64, f64)>;
                    let arm;
                    if vectors.len() <= self.vector_exact_max.get() {
                        ranked = vectors
                            .iter()
                            .map(|(id, v)| (*id, exact_cosine(v)))
                            .collect();
                        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                        arm = VectorArm::Exact;
                    } else {
                        sometimes!("graph.vector planner chose ann", true);
                        let qf: Vec<f32> = query.iter().map(|x| *x as f32).collect();
                        // Over-fetch by the dead-node count so k LIVE results
                        // survive the rescore filter (dead nodes are absent
                        // from `vectors`).
                        let dead = h.len().saturating_sub(vectors.len());
                        ranked = h
                            .search(&qf, k + dead)
                            .into_iter()
                            .filter_map(|(id, _)| vectors.get(&id).map(|v| (id, exact_cosine(v))))
                            .collect();
                        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                        arm = VectorArm::Ann { rebuilt: false };
                    }
                    ranked.truncate(k);
                    let mut out = Vec::with_capacity(ranked.len());
                    for (id, score) in ranked {
                        if let Some(node) = self.node(id)? {
                            out.push((node, score));
                        }
                    }
                    counted!("graph.vector queries");
                    return Ok((
                        out,
                        VectorPlan {
                            eligible: vectors.len(),
                            skipped,
                            arm,
                        },
                    ));
                }
            }
        }

        // COLD PATH: one gathering pass — ids + vectors only. The store's
        // column scan serves the embedding property straight from a column
        // block where compaction has built one (the census's head/tail
        // layout) and falls back to row decodes where it has not; either
        // way, whole-node materialisation stays off this path.
        let members: std::collections::BTreeSet<u64> =
            self.nodes_by_label(Some(&label))?.into_iter().collect();
        let prop_token = self.token("prop:", &self.props, &prop)?;
        let mut eligible: Vec<(u64, Vec<f64>)> = Vec::new();
        for (body, tagged) in self
            .store
            .scan_column_at(&self.nodes, &[], prop_token, u64::MAX)
        {
            let Ok(id_bytes) = <[u8; 8]>::try_from(body.as_slice()) else {
                continue;
            };
            let id = u64::from_be_bytes(id_bytes);
            if !members.contains(&id) {
                continue; // another label's node carrying the same property
            }
            let Some(Value::List(items)) = crate::decode_prop_opt(&tagged) else {
                continue;
            };
            let mut v = Vec::with_capacity(items.len());
            let mut ok = true;
            for i in &items {
                match i {
                    Value::Float(f) => v.push(*f),
                    Value::Int(n) => v.push(*n as f64),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || v.len() != query.len() {
                // No vector, or a DIFFERENT EMBEDDING SPACE — never scored.
                continue;
            }
            if v.iter().map(|x| x * x).sum::<f64>().sqrt() == 0.0 {
                continue;
            }
            eligible.push((id, v));
        }
        // Every member either scored or was skipped — absent property,
        // foreign shape and zero norm alike land here, exactly as before.
        let skipped = members.len() - eligible.len();
        if skipped > 0 {
            sometimes!("graph.vector query skipped unindexable rows", true);
        }

        let (mut ranked, arm): (Vec<(u64, f64)>, VectorArm) =
            if eligible.len() <= self.vector_exact_max.get() {
                // Cache the gathered vectors under this epoch, exactly as the
                // ANN arm caches its HNSW: a warm exact index then scores
                // from memory instead of re-scanning the nodes partition for
                // its embeddings every query. Measured on portserve: an
                // 89-vector index cost 300 ms a query COLD (the column scan
                // + decode dominates a sub-millisecond cosine), and 15 such
                // indexes were the harness's 4.5 s/query seed cost. The HNSW
                // slot holds an empty index — the warm path never searches it
                // for an exact-sized population.
                let vectors: std::collections::BTreeMap<u64, Vec<f64>> =
                    eligible.iter().cloned().collect();
                let vectors = std::sync::Arc::new(vectors);
                if !in_txn_writes {
                    // Never cached from inside a transaction: the entry would
                    // be stamped current while missing what the transaction
                    // is about to commit.
                    self.ann_cache.borrow_mut().insert(
                        index.to_string(),
                        crate::AnnEntry {
                            epoch,
                            dim,
                            index: std::sync::Arc::new(crate::hnsw::Hnsw::new(dim, 0)),
                            vectors: std::sync::Arc::clone(&vectors),
                            skipped,
                        },
                    );
                    counted!("graph.vector exact index cached");
                }
                let mut scored: Vec<(u64, f64)> = vectors
                    .iter()
                    .map(|(id, v)| (*id, exact_cosine(v)))
                    .collect();
                scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                scored.truncate(k);
                (scored, VectorArm::Exact)
            } else {
                sometimes!("graph.vector planner chose ann", true);
                let mut seed = 0xcbf2_9ce4_8422_2325u64;
                for b in index.as_bytes() {
                    seed ^= u64::from(*b);
                    seed = seed.wrapping_mul(0x0000_0100_0000_01B3);
                }
                let mut h = crate::hnsw::Hnsw::new(dim, seed);
                for (id, v) in &eligible {
                    let vf: Vec<f32> = v.iter().map(|x| *x as f32).collect();
                    h.insert(*id, &vf);
                }
                let vectors: std::collections::BTreeMap<u64, Vec<f64>> =
                    eligible.iter().cloned().collect();
                let vectors = std::sync::Arc::new(vectors);
                let h = std::sync::Arc::new(h);
                if !in_txn_writes {
                    // Never cached from inside a transaction — as above.
                    self.ann_cache.borrow_mut().insert(
                        index.to_string(),
                        crate::AnnEntry {
                            epoch,
                            dim,
                            index: std::sync::Arc::clone(&h),
                            vectors: std::sync::Arc::clone(&vectors),
                            skipped,
                        },
                    );
                }
                sometimes!("graph.vector ann index built", true);
                counted!("graph.vector ann index builds");
                let qf: Vec<f32> = query.iter().map(|x| *x as f32).collect();
                let mut scored: Vec<(u64, f64)> = h
                    .search(&qf, k)
                    .into_iter()
                    .filter_map(|(id, _)| vectors.get(&id).map(|v| (id, exact_cosine(v))))
                    .collect();
                scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                (scored, VectorArm::Ann { rebuilt: true })
            };

        ranked.truncate(k);
        let mut out = Vec::with_capacity(ranked.len());
        for (id, score) in ranked {
            if let Some(node) = self.node(id)? {
                out.push((node, score));
            }
        }
        counted!("graph.vector queries");
        Ok((
            out,
            VectorPlan {
                eligible: eligible.len(),
                skipped,
                arm,
            },
        ))
    }

    /// `db.index.fulltext.queryNodes(name, query)` — term matching with a
    /// term-frequency score. Lucene's full syntax this is not (yet): terms
    /// are OR-combined, case-insensitive, punctuation-split. Documented
    /// divergence, refused nowhere — a simple query behaves as Neo4j's.
    pub fn fulltext_query(
        &self,
        index: &str,
        query: &str,
    ) -> Result<Vec<(Value, f64)>, GraphError> {
        let Some(def) = self.index_def(index)? else {
            return Err(GraphError::SchemaConflict(format!(
                "no such index `{index}`"
            )));
        };
        let IndexDef::Fulltext { labels, props } = def else {
            return Err(GraphError::SchemaConflict(format!(
                "`{index}` is not a fulltext index"
            )));
        };
        let terms: Vec<String> = tokenize(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut scored: Vec<(Value, f64)> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for label in &labels {
            for id in self.nodes_by_label(Some(label))? {
                if !seen.insert(id) {
                    continue;
                }
                let Some(node) = self.node(id)? else { continue };
                let Value::Node { props: nprops, .. } = &node else {
                    unreachable!()
                };
                let mut score = 0.0;
                for p in &props {
                    if let Some(Value::Str(text)) = nprops.get(p) {
                        let toks = tokenize(text);
                        for t in &terms {
                            score += toks.iter().filter(|x| *x == t).count() as f64;
                        }
                    }
                }
                if score > 0.0 {
                    scored.push((node, score));
                }
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        counted!("graph.fulltext queries");
        Ok(scored)
    }
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_key::{Namespace, Realm};
    use engram_store::Store;

    /// The node scan planner must never consult a REL-scoped range index —
    /// the scope bit CREATE used to discard. Both scopes are stored and
    /// SHOWable; only the node one reaches the planner's catalogue. Proven
    /// here because `scan_range_indexes` is the planner's feed and is not
    /// visible to integration tests.
    #[test]
    fn rel_scoped_range_indexes_never_reach_the_node_planner() {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        g.apply_schema(&SchemaCmd::CreateRangeIndex {
            name: Some("ix_node".into()),
            if_not_exists: false,
            label: "Person".into(),
            props: vec!["name".into()],
            on_relationships: false,
        })
        .unwrap();
        g.apply_schema(&SchemaCmd::CreateRangeIndex {
            name: Some("ix_rel".into()),
            if_not_exists: false,
            label: "KNOWS".into(),
            props: vec!["since".into()],
            on_relationships: true,
        })
        .unwrap();
        let planned = g.scan_range_indexes();
        assert_eq!(planned.len(), 1, "only the node index is planned against");
        assert_eq!(planned[0].name, "ix_node");
        let (_, rows) = g.show_schema("INDEXES", false).unwrap();
        assert_eq!(rows.len(), 2, "both scopes stay SHOWable");
    }
}
