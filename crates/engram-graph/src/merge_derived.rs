//! Building the derived structures as a BY-PRODUCT of compaction.
//!
//! # The cost this removes
//!
//! `build_adj_table` maintains the adjacency CSR by SCANNING the whole
//! adjacency span — 17.26M rows at official LDBC SF1, across ~32 tables — and
//! `members_at_token` rebuilds label membership the same way. That rebuild is
//! what the derived-refresh pass costs, and it is why every headline write
//! number in this programme so far was measured with `--no-derived-refresh`.
//!
//! # Why compaction can produce it for free
//!
//! The adjacency span's key order IS a CSR. A key is
//! `tag | from | type | to | rel`, so walking the span in order and packing
//! `offsets`/`entries` is exactly what `build_adj_table` does — and compaction
//! already walks every key in the merged run, in that order, because it has to
//! write them out sorted. Membership is the same shape: `'L' | label | node`
//! is already grouped by label and ascending by node, which is precisely what
//! `MembersView`'s base wants.
//!
//! So the structure costs O(merged run) rather than O(corpus), and it is
//! produced by work that must happen anyway.
//!
//! # The three properties that make it safe
//!
//! **1. Only a FULL compaction emits.** `compact_paged_to_dir` merges every
//! segment, so what this observer sees is the complete sealed set. A partial
//! merge would produce a partial CSR — a wrong answer rather than a stale one.
//!
//! **2. The stamp never exceeds what the CSR covers.** The stamp is the max
//! `max_commit_ts` over the merged segments, and the seal fence
//! (`store/lib.rs`) makes the tail strictly newer than every segment and
//! segment N+1 strictly newer than N. So every row at or below the stamp IS in
//! the merged set. `Graph::fenced` is then applied at publish time and can only
//! LOWER it, never raise it — which is what keeps the published claim true even
//! though the merge ran for minutes.
//!
//! **3. A base the change log cannot reach declines to a rebuild.**
//! `adj_repair_change_set` refuses on `!log.covers(built_at)` and
//! `members_caught_up` makes the same check, so a base published at a stamp the
//! log has already pruned past causes a REBUILD — today's behaviour — not a
//! silently short answer.
//!
//! Together those three are why this is a base rather than a cache: the reader
//! catches up from the change log for everything above the stamp, exactly as it
//! does for a table it built itself.

use std::collections::{BTreeMap, BTreeSet};

use engram_observe::{counted, sometimes};
use engram_store::MergeObserver;

use crate::derived::{MembersView, Slot, Snapshot, slot_in};
use crate::{
    ADJ_TABLE_CACHE_MAX, AdjTable, DEGREE_TABLE_MAX_ID, Graph, MEMBERS_CACHE_MAX, SlimAdj,
};

/// The key an adjacency table is cached under: a side tag and a SORTED type
/// token set, empty meaning "any type". Mirrors `AdjTables`' key exactly, so a
/// bucket built here lands in the slot a reader looks in.
type AdjKey = (u8, Vec<u32>);

/// What one merge collected for one graph: its adjacency CSRs, keyed as
/// `adj_tables` is, and its membership id-sets by label token.
type Collected = (Vec<(AdjKey, AdjTable)>, BTreeMap<u32, Vec<u64>>);

/// Collects adjacency and membership from one graph's share of a compaction.
pub(crate) struct MergeDerived {
    /// The encoded `index` key prefix — every row this cares about lives under
    /// it, and the observer is handed FULL logical keys.
    index_prefix: Vec<u8>,
    /// One CSR per WANTED table. Not "one per type": building all 32 buckets
    /// unconditionally would add ~1.6 GB to compaction's peak at SF1, so the
    /// wanted set is the tables a reader has actually asked for.
    adj: Vec<(AdjKey, AdjBuild)>,
    /// Wanted label tokens, and their ascending member ids.
    members: BTreeMap<u32, Vec<u64>>,
    /// The stamp the merged segments cover, once `finish` has been called.
    stamp: Option<u64>,
    /// Entries any ONE table may hold before this observer gives up on it.
    /// Mirrors `adj_table_max_entries` — a budget so a pathological corpus
    /// cannot turn a maintenance pass into an allocation.
    max_entries: usize,
    /// Whether any table went over budget. The whole graph's adjacency is then
    /// dropped, for `build_adj_tables_all_types`' reason: publishing the
    /// buckets that fit would publish a table that answers, and answers SHORT.
    over_budget: bool,
}

/// The vintage a written sidecar is remembered by: the sealed set it describes
/// and how many adjacency / membership bases it holds. See
/// `Graph::persisted_vintage` for why the count is part of it.
fn derived_vintage(sealed_id: u64, n_adj: usize, n_members: usize) -> u64 {
    // A mix rather than a tuple so it fits the one atomic; collisions cost a
    // skipped write at worst only when BOTH the sealed set and the counts
    // move to a colliding pair, and a redundant write otherwise.
    let mut h = sealed_id ^ 0x9E37_79B9_7F4A_7C15;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ (n_adj as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ (n_members as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
    h ^ (h >> 31)
}

/// One CSR under construction, in the layout `AdjTable` takes. The row
/// directory is built SPARSE as rows arrive (`RowIndexBuilder`): a dense
/// offsets vector per table was O(ids) per table × 318 tables of transient
/// on the production mirror, at exactly the moment a compaction had the
/// heap largest.
struct AdjBuild {
    index: crate::RowIndexBuilder,
    entries: Vec<SlimAdj>,
    /// Whether every node's row is non-decreasing in `peer`. ESTABLISHED here
    /// by the same per-entry comparison `build_adj_table` makes, never inferred
    /// from the key layout — see `AdjTable::sorted_by_peer` for why a property
    /// relied on silently is one a later change can break unnoticed.
    sorted: bool,
}

impl MergeDerived {
    /// An observer for the tables and labels `graph` has CURRENTLY PUBLISHED.
    ///
    /// Scoping to published slots is what bounds the memory, and it is also the
    /// right set on its own terms: an unpublished table has no reader waiting
    /// on it, and one that appears later builds itself as it always did.
    pub(crate) fn for_graph(graph: &Graph) -> MergeDerived {
        let mut adj: Vec<(AdjKey, AdjBuild)> = Vec::new();
        for key in graph.adj_tables.load().keys() {
            adj.push((key.clone(), AdjBuild::new()));
        }
        let members: BTreeMap<u32, Vec<u64>> = graph
            .members_cache
            .load()
            .keys()
            .map(|t| (*t, Vec::new()))
            .collect();
        MergeDerived {
            index_prefix: graph.index_prefix_bytes(),
            adj,
            members,
            stamp: None,
            max_entries: graph.adj_table_max_entries.get(),
            over_budget: false,
        }
    }

    /// Whether this observer would collect anything at all — the cheap check
    /// that keeps a compaction on a graph with no published derived structures
    /// from paying any per-key cost.
    pub(crate) fn is_empty(&self) -> bool {
        self.adj.is_empty() && self.members.is_empty()
    }

    /// The stamp, once the merge finished.
    pub(crate) fn stamp(&self) -> Option<u64> {
        self.stamp
    }

    /// Whether a table exceeded the entry budget, so adjacency was abandoned.
    pub(crate) fn over_budget(&self) -> bool {
        self.over_budget
    }

    /// The completed adjacency CSRs as `AdjTable`s, keyed as `adj_tables` is.
    /// Empty if any went over budget.
    pub(crate) fn into_tables(self) -> Collected {
        let members = self.members;
        if self.over_budget {
            return (Vec::new(), members);
        }
        let tables = self
            .adj
            .into_iter()
            .map(|(key, b)| {
                (
                    key,
                    AdjTable {
                        index: std::sync::Arc::new(b.index.finish(b.entries.len())),
                        entries: std::sync::Arc::new(b.entries),
                        overlay: BTreeMap::new(),
                        sorted_by_peer: b.sorted,
                    },
                )
            })
            .collect();
        (tables, members)
    }
}

impl MergeObserver for MergeDerived {
    fn key_prefixes(&self) -> Vec<Vec<u8>> {
        // Adjacency (the tags actually wanted) and membership. Everything else
        // in the merge — records, guards, counters, properties — is offered to
        // nobody, so the per-key cost on a compaction is a prefix test.
        let mut tags: BTreeSet<u8> = self.adj.iter().map(|((t, _), _)| *t).collect();
        if !self.members.is_empty() {
            tags.insert(b'L');
        }
        tags.into_iter()
            .map(|tag| {
                let mut p = self.index_prefix.clone();
                p.push(tag);
                p
            })
            .collect()
    }

    fn visit(&mut self, key: &Vec<u8>, live: bool) {
        // A TOMBSTONE that survived compaction still shadows a base row, so it
        // is not part of the derived structure. (After a FULL compaction a
        // surviving tombstone is rare — `retain_chain` purges those at or below
        // the watermark — but a newer one is kept and must be honoured.)
        if !live {
            return;
        }
        let Some(body) = key.get(self.index_prefix.len()..) else {
            return;
        };
        match body.first() {
            // `'L' | label(4) | node(8)`
            Some(b'L') if body.len() == 1 + 4 + 8 => {
                let label = u32::from_be_bytes(body[1..5].try_into().expect("4"));
                let Some(ids) = self.members.get_mut(&label) else {
                    return; // a label with no published snapshot
                };
                // Ascending BY CONSTRUCTION: the merge visits keys in order and
                // `label | node` is big-endian, so pushing preserves the sort
                // `MembersView`'s base requires.
                ids.push(u64::from_be_bytes(body[5..13].try_into().expect("8")));
            }
            // `tag | from(8) | type(4) | to(8) | rel(8)`
            Some(&tag) if body.len() == 1 + 8 + 4 + 8 + 8 => {
                if self.over_budget {
                    return;
                }
                let node = u64::from_be_bytes(body[1..9].try_into().expect("8"));
                if node > DEGREE_TABLE_MAX_ID {
                    return;
                }
                let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                let e = SlimAdj {
                    rel: u64::from_be_bytes(body[21..29].try_into().expect("8")),
                    type_token: t,
                    peer: u64::from_be_bytes(body[13..21].try_into().expect("8")),
                };
                let max = self.max_entries;
                for ((want_tag, types), b) in self.adj.iter_mut() {
                    if *want_tag != tag {
                        continue;
                    }
                    // Empty type set = the untyped table, which accepts every
                    // row; otherwise the same `binary_search` filter
                    // `build_adj_table` applies, over the same sorted tokens.
                    if !types.is_empty() && types.binary_search(&t).is_err() {
                        continue;
                    }
                    if b.entries.len() >= max {
                        // ABANDON, do not truncate. A CSR missing rows answers
                        // a traversal with a subset and no error — the silent
                        // wrong answer this whole path exists to avoid.
                        self.over_budget = true;
                        return;
                    }
                    b.push(node, e);
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self, stamp: u64) {
        // The directories close themselves in `into_tables` (the terminator
        // `build_adj_table` writes is `RowIndexBuilder::finish`'s).
        self.stamp = Some(stamp);
    }
}

impl AdjBuild {
    fn new() -> AdjBuild {
        AdjBuild {
            index: crate::RowIndexBuilder::new(),
            entries: Vec::new(),
            sorted: true,
        }
    }

    /// Append one entry to `node`'s row, in the same order and with the same
    /// `sorted` rule `build_adj_table` uses.
    fn push(&mut self, node: u64, e: SlimAdj) {
        // The row's start is where this node's row began; an entry past it is
        // this node's previous entry.
        let row_start = self.index.note(node, self.entries.len());
        if self.entries.len() > row_start && self.entries[self.entries.len() - 1].peer > e.peer {
            self.sorted = false;
        }
        self.entries.push(e);
    }
}

/// A compaction that also refreshes every graph's derived structures.
///
/// One observer per graph, composed so the STORE keeps its single-observer API
/// and stays free of any notion of realms, namespaces, labels or edges. The
/// dispatch is by key prefix, which is the only thing that distinguishes one
/// graph's rows from another's in the merged run.
struct Composite {
    parts: Vec<MergeDerived>,
}

impl MergeObserver for Composite {
    fn key_prefixes(&self) -> Vec<Vec<u8>> {
        self.parts.iter().flat_map(|p| p.key_prefixes()).collect()
    }

    fn visit(&mut self, key: &Vec<u8>, live: bool) {
        for p in self.parts.iter_mut() {
            if key.starts_with(&p.index_prefix) {
                p.visit(key, live);
                // Prefixes are disjoint across graphs (realm+namespace are part
                // of the encoded prefix), so the first match is the only one.
                return;
            }
        }
    }

    fn finish(&mut self, stamp: u64) {
        for p in self.parts.iter_mut() {
            p.finish(stamp);
        }
    }
}

/// Compact `store`'s paged set, emitting the derived bases for `graphs` from
/// the same merge — §5.2.
///
/// Returns the compaction's `(retired, dropped)` unchanged, so the caller's
/// reporting is identical whether or not anything was emitted.
///
/// `Graph::set_compaction_csr(false)` on every graph makes this exactly
/// `Store::compact_paged_to_dir`, which is what the differential compares
/// against.
pub fn compact_paged_emitting(
    graphs: &[std::sync::Arc<Graph>],
    store: &engram_store::Store,
    dir: &std::path::Path,
    cache: &std::sync::Arc<engram_store::paged::BlockCache>,
) -> std::io::Result<(u64, u64)> {
    let mut parts: Vec<MergeDerived> = Vec::new();
    let mut owners: Vec<std::sync::Arc<Graph>> = Vec::new();
    for g in graphs {
        if !g.compaction_csr.get() {
            continue;
        }
        let part = MergeDerived::for_graph(g);
        if part.is_empty() {
            continue; // nothing published: nothing worth collecting
        }
        parts.push(part);
        owners.push(std::sync::Arc::clone(g));
    }
    if parts.is_empty() {
        return store.compact_paged_to_dir(dir, cache);
    }
    let mut composite = Composite { parts };
    let out = store.compact_paged_observed(dir, cache, Some(&mut composite))?;
    // The sealed-set identity is read AFTER the merge published its new
    // segment, so it names the set the emitted bases actually describe. Read
    // before, it would name the set they were merged FROM — a vintage that
    // matches nothing and refuses every load, which is a silent no-op rather
    // than a wrong answer, and therefore exactly the kind of bug that ships.
    let sealed_id = store.sealed_set_id();
    for (g, part) in owners.iter().zip(composite.parts) {
        g.adopt_merged_derived(part, Some((dir, sealed_id)));
    }
    Ok(out)
}

/// Publish what one merge collected for this graph.
impl Graph {
    pub(crate) fn adopt_merged_derived(
        &self,
        part: MergeDerived,
        persist: Option<(&std::path::Path, u64)>,
    ) {
        let Some(stamp) = part.stamp() else {
            // `finish` was never called: the compaction failed or emitted
            // nothing. Publishing at a stamp we cannot justify is exactly the
            // silent wrong answer, so publish nothing.
            return;
        };
        if part.over_budget() {
            sometimes!("graph.adjacency table declined by the entry budget", true);
        }
        // FENCED AT PUBLISH TIME, not at merge start — the merge runs for
        // minutes, and `fenced` can only lower the stamp, so this cannot
        // publish a claim wider than what the CSR actually covers.
        let at = self.fenced(stamp);
        let (tables, members) = part.into_tables();
        let mut published = 0usize;
        // ONLY WHAT THE PUBLISH WON is persisted. A publish that lost the CAS
        // left a NEWER table in the slot, and writing the older emitted one to
        // disk would persist a base this process never served — which the next
        // start would then adopt as though it had.
        // The published tables THEMSELVES are what gets persisted — by `Arc`,
        // not by a dense copy of each: cloning 318 tables' offsets into a
        // vector before writing was a 4.4 GB transient on the production
        // mirror, at exactly the moment a compaction had the heap largest.
        let mut published_adj: Vec<(AdjKey, std::sync::Arc<AdjTable>)> = Vec::new();
        let mut published_members = BTreeMap::new();
        for (key, table) in tables {
            let slot: std::sync::Arc<Slot<AdjTable>> =
                slot_in(&self.adj_tables, &key, ADJ_TABLE_CACHE_MAX);
            let table = std::sync::Arc::new(table);
            if slot.publish_snapshot(std::sync::Arc::new(Snapshot {
                at,
                value: std::sync::Arc::clone(&table),
            })) {
                published += 1;
                published_adj.push((key, table));
                counted!("graph.adjacency tables published by compaction");
            }
        }
        for (token, ids) in members {
            let slot: std::sync::Arc<Slot<MembersView>> =
                slot_in(&self.members_cache, &token, MEMBERS_CACHE_MAX);
            let copy = ids.clone();
            if slot.publish(
                at,
                std::sync::Arc::new(MembersView::from_base(std::sync::Arc::new(ids))),
            ) {
                published += 1;
                published_members.insert(token, copy);
                counted!("graph.membership snapshots published by compaction");
            }
        }
        if published > 0 {
            // Only behind a publish that WON — a lost one moved no snapshot
            // for a prune to sit behind. Same rule as
            // `publish_repaired_adj_table`.
            for tag in [b'O', b'I'] {
                self.prune_adj_logs(tag);
            }
        }
        // §5.4 — persist what was just published, so the next process start
        // adopts it instead of walking the span again.
        //
        // Written from the SAME structures that were published, and at the
        // SAME `at`, so the file cannot describe a base the process never
        // served. Building the sidecar from a second, separately-derived
        // source is how a persisted cache silently diverges from the live one.
        if let Some((dir, sealed_id)) = persist {
            if !self.persist_derived.get() {
                return;
            }
            match write_sidecar(
                dir,
                &self.index_prefix_bytes(),
                at,
                sealed_id,
                published_adj.iter().map(|(k, t)| (k, t.as_ref())),
                &published_members,
            ) {
                // Record the vintage HERE too, or the next quiescent tick
                // rewrites the same 1.69 GB: `persist_derived_now` skips on an
                // unchanged vintage, and a file written down this path would
                // otherwise look like one that had never been written. The
                // vintage counts what THIS file holds; a tick that finds more
                // bases published than that writes the fuller file once.
                // Stamped with the last tick's clock: the engine reads no clock
                // of its own (the simulation owns time), and a compaction lands
                // between ticks — at worst the growth interval after it is
                // shorter by one tick period, the harmless direction.
                Ok(()) => self.note_persisted(
                    sealed_id,
                    derived_vintage(sealed_id, published_adj.len(), published_members.len()),
                    self.last_tick_secs.load(std::sync::atomic::Ordering::Relaxed),
                ),
                Err(e) => {
                    // A sidecar that cannot be written costs a rebuild at the
                    // next start. Nothing about the store is wrong, so this is
                    // a note, not a failure — the compaction already succeeded.
                    counted!("graph.derived sidecar write failed");
                    let _ = e;
                }
            }
        }
    }

    /// §5.4, kept CURRENT: re-stamp the sidecar from the bases this process is
    /// serving right now.
    ///
    /// # Why this exists, from a measurement
    ///
    /// A sidecar's vintage is the sealed set it was produced from, so the NEXT
    /// SEAL invalidates it. On the pod that is immediate: a full compaction
    /// wrote a 1.69 GB sidecar and the sweep's own writes sealed a new segment
    /// seconds later, leaving a file that would be refused at every subsequent
    /// boot. §5.4 as specified is then correct and useless — it delivers its
    /// 55 s -> <2 s only for a server that compacts and is then stopped without
    /// writing again, which is not how a server is used.
    ///
    /// So on a QUIESCENT tick — the same gate `persist_declared_indexes` uses,
    /// and for the same reason: the serialise is O(corpus) and doing it under
    /// load would trade a restart cost for a steady-state one — the currently
    /// published bases are written out against the CURRENT sealed set.
    ///
    /// # The two conditions, both load-bearing
    ///
    /// **Every published base must be current for the clock.** A base at a
    /// stamp below `now_ts` is short of rows, and at the next boot there is no
    /// change log to carry the difference. Writing it would persist a subset
    /// and call it complete.
    ///
    /// **The overlay must be FOLDED IN, not dropped.** A repaired table's base
    /// holds rows that have since moved; the overlay is where the correction
    /// lives. `AdjTable::flattened` is what makes persisting a repaired table
    /// honest — see its doc.
    ///
    /// Returns `true` if a sidecar was written. `false` is the ordinary case
    /// (something is stale, or a writer is in flight) and costs nothing.
    ///
    /// `now_secs` is the CALLER'S monotonic clock in seconds — the engine
    /// reads none of its own (a direct clock read is invisible to the
    /// simulation); it is what the growth-rewrite interval is measured on.
    pub fn persist_derived_now(&self, dir: &std::path::Path, now_secs: u64) -> bool {
        if !self.persist_derived.get() {
            return false;
        }
        self.last_tick_secs
            .store(now_secs, std::sync::atomic::Ordering::Relaxed);
        // SKIP IF THE SEALED SET HAS NOT MOVED since the last file we wrote.
        //
        // The sidecar is 1.69 GB at SF1, and a quiescent tick recurs. Without
        // this, an idle server rewrites the whole CSR to disk every time the
        // tick finds the tail unchanged — gigabytes of writes to produce a file
        // identical in the only way that matters.
        //
        // The sealed set is the right key because it is exactly what the
        // sidecar describes: rows in the TAIL are not in it, and a paged store
        // has no replay contract, so at boot the tail is gone and the segments
        // are the whole store. An unchanged sealed set means an unchanged
        // answer to the only question the file is asked.
        //
        // ...FOR THE BASES THE FILE HOLDS. The vintage also counts how many
        // bases were published when the file was written: on the production
        // mirror the first quiescent tick fired while the 74 s warm was still
        // walking the span, wrote a sidecar holding ONE membership, and — the
        // sealed set of a read-only mirror never moving — skipped every tick
        // after it, so the 318 warmed tables were rebuilt at every start and
        // the file existed only to say so. A tick that finds more published
        // than the last file held writes again; once more is never published,
        // it skips as before.
        let sealed_now = self.store.sealed_set_id();
        let n_adj = self
            .adj_tables
            .load()
            .iter()
            .filter(|(_, slot)| slot.load().is_some())
            .count();
        let n_members = self
            .members_cache
            .load()
            .iter()
            .filter(|(_, slot)| slot.load().is_some())
            .count();
        // 0 is the never-written sentinel: a real vintage hashing to 0 costs
        // one redundant write, which is the harmless direction.
        let vintage_now = derived_vintage(sealed_now, n_adj, n_members);
        if self.persisted_vintage.load(std::sync::atomic::Ordering::Relaxed) == vintage_now {
            counted!("graph.derived sidecar skipped: the sealed set has not moved");
            return false;
        }
        // GROWTH ON AN UNCHANGED SEALED SET is rate-limited; a moved sealed
        // set is not (the file on disk then names rows this store does not
        // have, and the next start would refuse it). v84 in production wrote
        // the 1.3 GB file nine times in four minutes — once per membership the
        // shadow traffic built — with no limit here.
        if self.persisted_sealed_id.load(std::sync::atomic::Ordering::Relaxed) == sealed_now {
            let since = now_secs
                .saturating_sub(self.persisted_at_secs.load(std::sync::atomic::Ordering::Relaxed));
            let interval = self
                .persist_growth_interval_secs
                .load(std::sync::atomic::Ordering::Relaxed);
            if since < interval {
                counted!("graph.derived sidecar deferred: growth inside the rewrite interval");
                return false;
            }
        }
        let now = self.store.now_ts();
        // CURRENT FOR ITS OWN EPOCH is the test, not "stamped at the clock".
        //
        // A table is published at the stamp of the last write it saw, and the
        // clock moves for reasons a table has nothing to do with — another
        // label's write, a seal. So `snap.at < now_ts()` is true of a table
        // that is complete, and using it as the gate declines almost every
        // time. (It did: the first cut of this never persisted anything.)
        //
        // The engine's own definition of current is `snap.at >= epoch`, where
        // the epoch is the last write to what the table covers. A table current
        // by that test has every row, and can therefore be STAMPED at the clock
        // — which is what the adoption check at boot needs, since a restored
        // store's clock sits at its newest segment.
        // Written ONE TABLE AT A TIME to the temporary file — a folded copy
        // exists only for a table that has an overlay, and only while it is
        // being written. Gathering every table's flattened copy first was a
        // 4.4 GB transient on the production mirror.
        let prefix = self.index_prefix_bytes();
        let Ok(mut w) = crate::derived_sidecar::SidecarWriter::create(dir, &prefix) else {
            counted!("graph.derived sidecar write failed");
            return false;
        };
        for (key, slot) in self.adj_tables.load().iter() {
            let Some(snap) = slot.load() else {
                continue; // nothing published: not this pass's business
            };
            let (tag, types) = key;
            let tokens = if types.is_empty() {
                None
            } else {
                Some(types.clone())
            };
            let _ = tag;
            if snap.at < self.adjacency_epoch(&tokens) {
                counted!("graph.derived sidecar deferred: a base is stale");
                w.abandon();
                return false;
            }
            // The overlay must be FOLDED IN, not dropped — see the doc above.
            let folded;
            let table: &AdjTable = if snap.value.overlay.is_empty() {
                &snap.value
            } else {
                folded = snap.value.folded();
                &folded
            };
            if w.add_adj(key, &table.index, &table.entries, table.sorted_by_peer).is_err() {
                counted!("graph.derived sidecar write failed");
                w.abandon();
                return false;
            }
        }
        for (token, slot) in self.members_cache.load().iter() {
            let Some(snap) = slot.load() else {
                continue;
            };
            if snap.at < self.label_epoch(*token) {
                counted!("graph.derived sidecar deferred: a base is stale");
                w.abandon();
                return false;
            }
            let ids = snap.value.to_arc_vec();
            if w.add_members(*token, &ids).is_err() {
                counted!("graph.derived sidecar write failed");
                w.abandon();
                return false;
            }
        }
        if w.len() == 0 {
            w.abandon();
            return false;
        }
        // The sealed-set id LAST, after the bases were read: if a spill landed
        // in between, the id names a set the bases do not describe, and the
        // vintage check at boot would accept a file it should refuse. Read in
        // this order, a spill in between produces an id that matches nothing
        // and the sidecar is refused — the safe direction.
        let sealed_id = self.store.sealed_set_id();
        if self.store.now_ts() != now {
            // A write landed while we serialised. The bases no longer cover
            // the clock, so the stamp we would write is a claim we cannot make.
            counted!("graph.derived sidecar deferred: the clock moved mid-write");
            w.abandon();
            return false;
        }
        match w.finish(now, sealed_id) {
            Ok(()) => {
                // Keyed on what was COUNTED, not on `w.len()`: a base published
                // between the count and the write is one the next tick should
                // find "more than the file holds" and write.
                self.note_persisted(sealed_id, derived_vintage(sealed_id, n_adj, n_members), now_secs);
                true
            }
            Err(_) => {
                counted!("graph.derived sidecar write failed");
                false
            }
        }
    }

    /// Record that a sidecar of `vintage` describing `sealed_id` was written
    /// at `now_secs` — what the skip and the growth interval are measured
    /// against.
    fn note_persisted(&self, sealed_id: u64, vintage: u64, now_secs: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.persisted_vintage.store(vintage, Relaxed);
        self.persisted_sealed_id.store(sealed_id, Relaxed);
        self.persisted_at_secs.store(now_secs, Relaxed);
    }

    /// §5.4's load path: adopt every base a sidecar in `dir` still vouches
    /// for, and say how many were adopted.
    ///
    /// Called at open. Each adopted base is published at the sidecar's stamp,
    /// so a reader catches up from the change log for anything above it —
    /// exactly as it does for a table this process built itself. A sidecar that
    /// is absent, corrupt, or of a vintage the sealed set has moved past
    /// adopts NOTHING and every structure is built on first use, which is the
    /// behaviour before this item existed.
    pub fn adopt_derived_sidecar(&self, dir: &std::path::Path) -> usize {
        if !self.persist_derived.get() {
            return 0;
        }
        // The refusal REASONS are reported, not merely counted.
        //
        // A sidecar that is silently declined looks exactly like one that was
        // never written: the server warms slowly and nothing says why. That
        // cost a pod run to diagnose — the file was present, correct and
        // refused, and the only evidence was a warm time that had not improved.
        // An operator cannot act on a counter they cannot see.
        let want = self.store.sealed_set_id();
        let mut reader = match crate::derived_sidecar::SidecarReader::open_reporting(
            dir,
            &self.index_prefix_bytes(),
            want,
        ) {
            Ok(r) => r,
            // Absent is the ordinary first start; the vintage refusal reports
            // itself inside `open_reporting`. Everything else is a file that
            // is PRESENT and unusable — said out loud, because the v1→v2
            // transition on the bench deployment refused the old file without
            // a word and the only evidence was a 74 s warm.
            Err(crate::derived_sidecar::SidecarRefusal::Absent)
            | Err(crate::derived_sidecar::SidecarRefusal::Vintage { .. }) => return 0,
            Err(crate::derived_sidecar::SidecarRefusal::Unreadable(why)) => {
                counted!("graph.derived sidecar refused: unreadable");
                eprintln!(
                    "[engram-graph] derived sidecar REFUSED: present but unreadable ({why}) — an                      older version, or a file this writer did not finish. The structures are                      rebuilt, and the next quiescent tick writes a current one."
                );
                return 0;
            }
        };
        // NOTHING IN THE STORE MAY BE NEWER THAN THE BASE.
        //
        // In a running process a base older than the clock is fine: the change
        // log carries the difference and the reader catches up. At OPEN there
        // is no change log — it is an in-memory structure and this process has
        // just started — so a base below the clock has no way to be brought
        // current, and a reader would treat a table that is short of the newest
        // rows as current for its epoch. That is the silent subset answer.
        //
        // The sealed-set vintage does not cover this on its own: a WAL replay
        // can reconstruct the same sealed set AND a non-empty tail above it.
        // So the clock is checked as well, and a store with anything above the
        // sidecar's stamp builds instead.
        let now = self.store.now_ts();
        if now > reader.stamp() {
            counted!("graph.derived sidecar refused: store is newer");
            sometimes!("graph.derived sidecar refused for a store above its stamp", true);
            eprintln!(
                "[engram-graph] derived sidecar REFUSED: the store's clock is {now}, above the                  sidecar's stamp {}. It covers the segments it was written from and nothing                  above them, and at open there is no change log to carry the difference — so                  the structures are rebuilt.",
                reader.stamp()
            );
            return 0;
        }
        // FENCED, for the same reason a build's stamp is: a writer registered
        // below this stamp is still in flight, and its rows are not in the
        // sidecar. `fenced` can only lower, so the published claim stays true.
        let at = self.fenced(reader.stamp());
        // ONE RECORD AT A TIME: read, verify, publish, drop. The file's other
        // records are on disk, not in memory — the whole point of v2, after
        // the dense v1 file OOM-killed a 12Gi pod at this exact step.
        let mut adopted = 0usize;
        let mut held = 0usize; // bytes the adopted tables hold, for the log line
        let total = reader.len();
        for i in 0..total {
            match reader.read_record(i) {
                Some(crate::derived_sidecar::Record::Adj {
                    key,
                    index,
                    entries,
                    sorted,
                }) => {
                    let slot: std::sync::Arc<Slot<AdjTable>> =
                        slot_in(&self.adj_tables, &key, ADJ_TABLE_CACHE_MAX);
                    let table = AdjTable {
                        index: std::sync::Arc::new(index),
                        entries: std::sync::Arc::new(entries),
                        overlay: BTreeMap::new(),
                        sorted_by_peer: sorted,
                    };
                    let bytes = table.index.bytes()
                        + table.entries.len() * std::mem::size_of::<crate::SlimAdj>();
                    if slot.publish_snapshot(std::sync::Arc::new(Snapshot {
                        at,
                        value: std::sync::Arc::new(table),
                    })) {
                        adopted += 1;
                        held += bytes;
                        counted!("graph.adjacency tables adopted from disk");
                    }
                }
                Some(crate::derived_sidecar::Record::Members { token, ids }) => {
                    let slot: std::sync::Arc<Slot<MembersView>> =
                        slot_in(&self.members_cache, &token, MEMBERS_CACHE_MAX);
                    held += ids.len() * 8;
                    if slot.publish(
                        at,
                        std::sync::Arc::new(MembersView::from_base(std::sync::Arc::new(ids))),
                    ) {
                        adopted += 1;
                        counted!("graph.membership snapshots adopted from disk");
                    }
                }
                None => {
                    // A corrupt or malformed record: what came before it was
                    // verified on its own and stays; this and the rest are
                    // built on first use. Said out loud, for the same reason
                    // the vintage refusal is.
                    counted!("graph.derived sidecar refused: record");
                    eprintln!(
                        "[engram-graph] derived sidecar record {i} of {total} REFUSED (corrupt or                          malformed); {adopted} adopted before it, the rest are rebuilt on first use."
                    );
                    break;
                }
            }
        }
        if adopted > 0 {
            eprintln!(
                "[engram-graph] adopted {adopted} derived structure(s) holding {} MB (sparse row                  directories + entries)",
                held / (1024 * 1024)
            );
        }
        adopted
    }
}

/// Write one sidecar from published tables and member sets, one record at a
/// time — the compaction path's writer. Tables here are freshly built (no
/// overlay), so their directory and rows are written as they are.
fn write_sidecar<'a>(
    dir: &std::path::Path,
    prefix: &[u8],
    stamp: u64,
    sealed_id: u64,
    adj: impl Iterator<Item = (&'a AdjKey, &'a AdjTable)>,
    members: &BTreeMap<u32, Vec<u64>>,
) -> std::io::Result<()> {
    let mut w = crate::derived_sidecar::SidecarWriter::create(dir, prefix)?;
    for (key, table) in adj {
        debug_assert!(table.overlay.is_empty(), "a compaction-built table has no overlay");
        w.add_adj(key, &table.index, &table.entries, table.sorted_by_peer)?;
    }
    for (token, ids) in members {
        w.add_members(*token, ids)?;
    }
    w.finish(stamp, sealed_id)
}
