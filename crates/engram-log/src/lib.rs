//! L4 — the commit log. One artifact, four requirements.
//!
//! Replication ships it, tamper-evidence hashes it, CDC tails it, PITR replays
//! it. It must precede all four, because **a hash chain added later cannot
//! attest to any history predating it** — there is no retrofit for provenance.
//!
//! # The header/payload split is the design
//!
//! > Ciphertext payloads with plaintext routing headers only — otherwise
//! > destroying a DEK does not shred that tenant's data from the log at any
//! > replication site, and log retention becomes the binding constraint on
//! > shred latency.
//!
//! Every entry is a [`RoutingHeader`] plus an opaque payload. The header
//! carries only structural, typed fields — realm, namespace, kind, partition,
//! op, commit ts — the same closed set the key encoding's sealed trait allows,
//! so user data cannot ride in it by construction. The payload is bytes the
//! log never interprets. When L8 lands, "payload" means "ciphertext under the
//! tenant DEK" and NOTHING about this format changes: the flip from plaintext
//! to ciphertext is policy, not migration.
//!
//! The consequence worth spelling out: **destroying a DEK shreds that tenant
//! from every copy of the log at once** — local, replicas, archives — because
//! what those copies hold was never readable without the key. Retention stops
//! being the binding constraint on shred latency.
//!
//! # Verification never needs a key
//!
//! The chain hashes ciphertext as it stands. A replication site can therefore
//! verify integrity — every entry, the whole chain — while being structurally
//! unable to read a byte of tenant data. Integrity and confidentiality do not
//! trade against each other here, and the test pinning it is
//! `verification_requires_no_key`.
//!
//! # What a chain can and cannot attest
//!
//! Any in-place mutation or reorder breaks every subsequent hash. What a chain
//! CANNOT detect on its own is **truncation**: a prefix of a valid chain is a
//! valid chain. Detecting it requires comparing against an externally attested
//! head — which is exactly the plan's root-beacon requirement, and why
//! [`ChainVerify::Intact`]'s `head` is a value to publish, not an internal.

#![forbid(unsafe_code)]

use engram_key::{Kind, Namespace, Partition, Realm};
use engram_observe::{Canary, Gate, Registration, Subsystem, counted, sometimes};

/// What an entry did. The CDC consumer's discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// A value was written.
    Put,
    /// A tombstone was written.
    Delete,
}

impl Op {
    fn byte(self) -> u8 {
        match self {
            Op::Put => 1,
            Op::Delete => 2,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Op::Put),
            2 => Some(Op::Delete),
            _ => None,
        }
    }
}

/// The plaintext part of an entry: everything replication and routing need,
/// and nothing a user typed.
///
/// Fields are the same closed structural set the key encoding admits. There is
/// no `bytes`/`extra` field on purpose — an open field on the plaintext side
/// of this split is where a payload byte would eventually leak in, one
/// convenient logging patch at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutingHeader {
    /// Tenant.
    pub realm: Realm,
    /// Namespace within the realm.
    pub namespace: Namespace,
    /// The key's KIND.
    pub kind: Kind,
    /// Partition.
    pub partition: Partition,
    /// What happened.
    pub op: Op,
    /// The store's commit timestamp for this write.
    pub commit_ts: u64,
}

/// Encoded header length: 4+4+1+4+1+8.
pub const HEADER_LEN: usize = 22;

impl RoutingHeader {
    /// Encode. Big-endian, matching the key encoding's discipline.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.realm.0.to_be_bytes());
        out[4..8].copy_from_slice(&self.namespace.0.to_be_bytes());
        out[8] = self.kind.byte();
        out[9..13].copy_from_slice(&self.partition.0.to_be_bytes());
        out[13] = self.op.byte();
        out[14..22].copy_from_slice(&self.commit_ts.to_be_bytes());
        out
    }

    /// Decode. Refuses an unknown op byte — an op is not skippable content,
    /// it is the instruction a replayer executes.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() != HEADER_LEN {
            return None;
        }
        Some(RoutingHeader {
            realm: Realm(u32::from_be_bytes(buf[0..4].try_into().ok()?)),
            namespace: Namespace(u32::from_be_bytes(buf[4..8].try_into().ok()?)),
            kind: Kind::from_byte(buf[8]),
            partition: Partition(u32::from_be_bytes(buf[9..13].try_into().ok()?)),
            op: Op::from_byte(buf[13])?,
            commit_ts: u64::from_be_bytes(buf[14..22].try_into().ok()?),
        })
    }
}

/// One committed entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Position in the log, dense from 0.
    pub seq: u64,
    /// The plaintext routing header.
    pub header: RoutingHeader,
    /// Opaque payload. Ciphertext once L8 lands; the log never looks inside.
    pub payload: Vec<u8>,
    /// `BLAKE3(prev_hash ‖ seq ‖ header ‖ payload_digest)` where
    /// `payload_digest = BLAKE3(payload_len ‖ payload)` — see [`payload_digest`].
    pub hash: [u8; 32],
}

/// Domain separation for the genesis hash: an empty log's head is a constant
/// derived from this, never all-zeroes — a zeroed head is the most likely
/// corruption and must not verify.
const GENESIS_DOMAIN: &[u8] = b"engram-log-v1-genesis";

fn genesis_hash() -> [u8; 32] {
    *blake3::hash(GENESIS_DOMAIN).as_bytes()
}

/// The digest of one payload, `BLAKE3(payload_len ‖ payload)` — the part of an
/// entry's hash that does NOT depend on its predecessor.
///
/// The chain is `prev ‖ seq ‖ header ‖ digest`, in two hashes rather than one,
/// so a writer can hash its payload — the bulk of the work — BEFORE it takes
/// the log latch, and the serialised section is a 40-byte hash. With the
/// payload hashed under the latch, eight writers on disjoint keys did less
/// work than one. The length is hashed BEFORE the payload so `(A, B)` and
/// `(AB, "")` in adjacent fields cannot collide — the classic concatenation
/// ambiguity, now inside the digest.
pub fn payload_digest(payload: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&(payload.len() as u64).to_be_bytes());
    h.update(payload);
    *h.finalize().as_bytes()
}

fn entry_hash(prev: &[u8; 32], seq: u64, header: &RoutingHeader, digest: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(prev);
    h.update(&seq.to_be_bytes());
    h.update(&header.encode());
    h.update(digest);
    *h.finalize().as_bytes()
}

/// The verdict of a verification walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainVerify {
    /// Every hash checks out.
    Intact {
        /// Entries verified.
        len: u64,
        /// The chain head — the value to publish to the root beacon. A prefix
        /// of a valid chain is itself valid, so truncation is detectable ONLY
        /// against an externally attested copy of this.
        head: [u8; 32],
    },
    /// The chain breaks at `seq`: this entry's hash does not follow from its
    /// predecessor and its contents.
    Broken {
        /// First entry whose hash fails.
        seq: u64,
    },
    /// Sequence numbers are not dense from 0 — an entry was removed or
    /// reordered even though every hash locally matches its contents.
    SequenceGap {
        /// The seq that was expected.
        expected: u64,
        /// The seq that was found.
        found: u64,
    },
}

/// The in-memory commit log.
///
/// Storage (its own column family, group commit, fsync) arrives with the LSM;
/// what is frozen HERE is the entry format, the chain rule, and the
/// header/payload split — the parts that cannot change once a byte of log
/// exists anywhere.
#[derive(Debug, Default)]
pub struct CommitLog {
    entries: Vec<Entry>,
    /// Entries dropped by [`CommitLog::truncate_below`]. Sequence allocation
    /// and head continuity survive the drop: `len()` still counts them, and
    /// the chain hash of the last dropped entry is retained so the suffix
    /// verifies against a real predecessor rather than a hole.
    truncated: u64,
    /// The hash of the last truncated entry — genesis while nothing has
    /// been truncated.
    truncated_head: Option<[u8; 32]>,
    /// Optional durable sink. `None` (the default) is the in-memory log — every
    /// existing caller is unchanged. When attached (`Store::open_wal`), each
    /// `append` writes the entry to disk and a commit calls [`CommitLog::sync`]
    /// to `fsync` it, so a committed write survives a crash.
    sink: Option<Wal>,
    /// GROUP COMMIT: when set, [`CommitLog::sync`] records that an fsync is
    /// owed instead of performing one, and [`CommitLog::sync_now`] pays every
    /// owed fsync at once. Off by default, so every existing caller — and the
    /// simulation lane — sees exactly the per-commit fsync it always did.
    deferred: bool,
    /// Whether a `sync` has been deferred since the last `sync_now`. Only ever
    /// true with a sink attached: an in-memory log has nothing to fsync.
    dirty: bool,
}

impl CommitLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current head hash — genesis for an empty log.
    ///
    /// This is the value the root beacon publishes. It is a METHOD, not a
    /// field read, so an empty log yields the genesis constant rather than a
    /// zeroed array that a zeroed disk would also produce.
    pub fn head(&self) -> [u8; 32] {
        self.entries
            .last()
            .map(|e| e.hash)
            .or(self.truncated_head)
            .unwrap_or_else(genesis_hash)
    }

    /// Entries appended so far — INCLUDING truncated ones. This is the
    /// next sequence number; shrinking it on truncation would mint a
    /// duplicate seq and fork the chain.
    pub fn len(&self) -> u64 {
        self.truncated + self.entries.len() as u64
    }

    /// Whether the log holds no entries (truncated ones included).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop retained entries below `seq` — the SHIPPED boundary.
    ///
    /// This is a retention statement, not a deletion of history: the caller
    /// asserts every entry below `seq` has been replicated or archived
    /// elsewhere, so this process no longer has to hold its payload bytes.
    /// After truncation, `Store::recover` over this log's own `entries()`
    /// REFUSES (the suffix has no genesis) — which is correct: recovery
    /// below the boundary is the archive's job now, and a store that
    /// silently rebuilt a partial history would be worse than one that
    /// says it cannot. Returns the number of entries dropped.
    pub fn truncate_below(&mut self, seq: u64) -> u64 {
        let pos = self.entries.partition_point(|e| e.seq < seq);
        if pos == 0 {
            return 0;
        }
        self.truncated_head = Some(self.entries[pos - 1].hash);
        self.truncated += pos as u64;
        self.entries.drain(..pos);
        counted!("log.truncated entries");
        sometimes!("log.truncated below the shipped boundary", true);
        pos as u64
    }

    /// Append an entry, chaining it to the current head.
    pub fn append(&mut self, header: RoutingHeader, payload: Vec<u8>) -> &Entry {
        let digest = payload_digest(&payload);
        self.append_prehashed(header, payload, &digest)
    }

    /// [`CommitLog::append`] with the payload's digest computed by the caller
    /// — OUTSIDE whatever latch guards this log. The caller vouches that
    /// `digest == payload_digest(&payload)`; a wrong one breaks the chain at
    /// this entry for every verifier, which the chain tests would name.
    pub fn append_prehashed(
        &mut self,
        header: RoutingHeader,
        payload: Vec<u8>,
        digest: &[u8; 32],
    ) -> &Entry {
        let seq = self.len();
        let hash = entry_hash(&self.head(), seq, &header, digest);
        let entry = Entry {
            seq,
            header,
            payload,
            hash,
        };
        // Durable sink: buffer the record now; a commit `fsync`s it via `sync`.
        // A write error into the durable log is a durability failure — surfaced
        // as a panic rather than a silently-lost commit.
        if let Some(sink) = &mut self.sink {
            sink.write_entry(&entry)
                .unwrap_or_else(|e| panic!("WAL write failed (durability): {e}"));
        }
        self.entries.push(entry);
        counted!("log.appended");
        self.entries.last().expect("just pushed")
    }

    /// Flush and `fsync` the durable sink, if any — the durability point a commit
    /// calls before acknowledging. A no-op for the in-memory log.
    ///
    /// Under [`CommitLog::set_deferred_sync`] this records that an fsync is
    /// OWED and returns at once; the caller that deferred it is responsible for
    /// calling [`CommitLog::sync_now`] before acknowledging anything the append
    /// covered. Every call site keeps calling `sync()` exactly as before — the
    /// decision to batch lives in one place, not at each commit.
    pub fn sync(&mut self) -> std::io::Result<()> {
        let deferred = self.deferred;
        match &mut self.sink {
            None => Ok(()),
            Some(_) if deferred => {
                self.dirty = true;
                Ok(())
            }
            Some(sink) => {
                counted!("log.fsyncs");
                sink.sync()
            }
        }
    }

    /// Pay every deferred fsync with ONE call. `Ok` clears the owed flag; an
    /// error leaves it set, because the data is still not on disk.
    ///
    /// # Group commit
    ///
    /// This is the whole mechanism. A durable write costs an fsync (~2.6 ms
    /// measured), and with one fsync per write throughput is bounded by fsync
    /// latency no matter how many clients are writing — measured flat at
    /// 375 → 380 ops/s from 1 to 8 clients, against an incumbent that went
    /// 517 → 2,671 on the same corpus and hardware because it fsyncs once per
    /// BATCH of concurrent transactions. Deferring the sync lets a batch of
    /// appends share one fsync; acknowledging only after `sync_now` keeps the
    /// durability promise exactly where it was.
    pub fn sync_now(&mut self) -> std::io::Result<()> {
        let r = match &mut self.sink {
            Some(sink) => {
                counted!("log.fsyncs");
                sink.sync()
            }
            None => Ok(()),
        };
        if r.is_ok() {
            self.dirty = false;
        }
        r
    }

    /// Switch [`CommitLog::sync`] between fsync-per-call (off, the default) and
    /// fsync-on-[`sync_now`](CommitLog::sync_now) (on).
    pub fn set_deferred_sync(&mut self, on: bool) {
        self.deferred = on;
    }

    /// Whether an fsync is owed — a deferred `sync` has happened since the last
    /// `sync_now`.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Hand every buffered record to the OS — a `write`, not an `fsync` — and
    /// return the sequence the log had reached. Clears the owed flag: the
    /// records are out of THIS process, and the fsync that makes them durable
    /// is now the caller's to perform on the handle from
    /// [`CommitLog::sync_handle`], OUTSIDE whatever lock guards this log.
    ///
    /// That split — flush under the lock, fsync outside it — is what lets many
    /// workers share one fsync: an fsync covers every write issued to the file
    /// before it was called, from any thread, so the one worker that calls it
    /// makes durable everything the others flushed first.
    pub fn flush_to_os(&mut self) -> std::io::Result<u64> {
        if let Some(sink) = &mut self.sink {
            std::io::Write::flush(&mut sink.w)?;
        }
        self.dirty = false;
        Ok(self.len())
    }

    /// A second handle to the durable file, for an fsync performed outside the
    /// lock that guards this log. `None` for the in-memory log.
    pub fn sync_handle(&self) -> Option<std::io::Result<std::fs::File>> {
        self.sink.as_ref().map(|s| s.w.get_ref().try_clone())
    }

    /// Attach a durable sink to an otherwise in-memory log (after recovery). The
    /// sink is positioned to append after the log's current tail.
    pub fn attach_sink(&mut self, wal: Wal) {
        self.sink = Some(wal);
    }

    /// Whether a durable sink is attached.
    pub fn has_sink(&self) -> bool {
        self.sink.is_some()
    }

    /// Start an EMPTY log at a rotated WAL's anchor: the next sequence is
    /// `first_seq` and the head is `prev_hash`, exactly as if the records
    /// below the anchor had been appended and then truncated. The replayed
    /// suffix then verifies and extends the chain the file carries.
    pub fn seed_anchor(&mut self, anchor: WalAnchor) {
        assert!(self.entries.is_empty() && self.truncated == 0, "seed_anchor on a used log");
        if anchor.first_seq > 0 {
            self.truncated = anchor.first_seq;
            self.truncated_head = Some(anchor.prev_hash);
        }
    }

    /// [`CommitLog::verify_entries`] against an anchor instead of genesis —
    /// the check a rotated WAL's records get on replay.
    pub fn verify_entries_from(anchor: WalAnchor, entries: &[Entry]) -> ChainVerify {
        let mut prev = anchor.prev_hash;
        for (i, e) in entries.iter().enumerate() {
            let expected_seq = anchor.first_seq + i as u64;
            if e.seq != expected_seq {
                return ChainVerify::SequenceGap {
                    expected: expected_seq,
                    found: e.seq,
                };
            }
            let want = entry_hash(&prev, e.seq, &e.header, &payload_digest(&e.payload));
            if want != e.hash {
                return ChainVerify::Broken { seq: e.seq };
            }
            prev = e.hash;
        }
        ChainVerify::Intact {
            len: anchor.first_seq + entries.len() as u64,
            head: prev,
        }
    }

    /// CHECKPOINT the durable sink below `seq`: rotate the WAL file to one
    /// holding only the records at or after `seq`, anchored at the hash before
    /// it, and drop the retained entries below `seq` from memory. `seq` must
    /// be within the retained range (`truncated..=len`): the anchor hash of a
    /// boundary whose predecessor was already dropped is unknown. Returns the
    /// entries dropped; a log with no sink only truncates.
    ///
    /// The caller holds whatever latch guards this log across the call, so no
    /// append lands between the successor's write and the sink swap — the
    /// successor is complete for the chain it anchors.
    pub fn rotate_sink_below(&mut self, seq: u64) -> std::io::Result<u64> {
        if seq < self.truncated || seq > self.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "checkpoint boundary {seq} outside the retained range {}..={}",
                    self.truncated,
                    self.len()
                ),
            ));
        }
        if self.sink.is_some() {
            let prev_hash = if seq == self.truncated {
                self.truncated_head.unwrap_or_else(genesis_hash)
            } else {
                self.entries[(seq - self.truncated - 1) as usize].hash
            };
            let anchor = WalAnchor {
                first_seq: seq,
                prev_hash,
            };
            // Owned: the rotation borrows the sink mutably while it copies
            // the kept records; a checkpoint keeps only the records since
            // the last seal, so the copy is small.
            let keep: Vec<Entry> = self.tail(seq).to_vec();
            if let Some(sink) = self.sink.as_mut() {
                // A failure leaves the old file and the old sink in place —
                // the log stays durable, the checkpoint simply did not happen.
                sink.rotate(anchor, &keep)?;
            }
        }
        Ok(self.truncate_below(seq))
    }

    /// The entries, for a CDC tail or a replayer.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Entries at or after `from_seq` — the CDC cursor read.
    pub fn tail(&self, from_seq: u64) -> &[Entry] {
        let start = self.entries.partition_point(|e| e.seq < from_seq);
        &self.entries[start..]
    }

    /// Verify the whole chain from genesis.
    ///
    /// Recomputes every hash; needs NO key, because the chain covers payloads
    /// as they are stored. Returns the head for beacon comparison.
    pub fn verify(&self) -> ChainVerify {
        if self.truncated == 0 {
            return Self::verify_entries(&self.entries);
        }
        // A truncated log verifies its retained suffix against the RETAINED
        // predecessor hash — the same incremental rule a replica applies.
        let mut prev = self.truncated_head.unwrap_or_else(genesis_hash);
        for (i, e) in self.entries.iter().enumerate() {
            let expected_seq = self.truncated + i as u64;
            if e.seq != expected_seq {
                return ChainVerify::SequenceGap {
                    expected: expected_seq,
                    found: e.seq,
                };
            }
            let want = entry_hash(&prev, e.seq, &e.header, &payload_digest(&e.payload));
            if want != e.hash {
                return ChainVerify::Broken { seq: e.seq };
            }
            prev = e.hash;
        }
        ChainVerify::Intact {
            len: self.len(),
            head: prev,
        }
    }

    /// The genesis head — what an EMPTY chain's previous-hash is. Exposed
    /// so a replica can start its own incremental verification from the same
    /// anchor the primary did.
    pub fn genesis_hash() -> [u8; 32] {
        genesis_hash()
    }

    /// One link of the chain: the hash an entry must carry to extend `prev`.
    /// Exposed for APPLY-time verification — a follower recomputes this
    /// against its own head rather than trusting the feed's stored hashes.
    pub fn chain_hash(
        prev: &[u8; 32],
        seq: u64,
        header: &RoutingHeader,
        payload: &[u8],
    ) -> [u8; 32] {
        entry_hash(prev, seq, header, &payload_digest(payload))
    }

    /// Verify any entry slice as a from-genesis chain — usable by a
    /// replication site on entries it received, without constructing a log.
    pub fn verify_entries(entries: &[Entry]) -> ChainVerify {
        let mut prev = genesis_hash();
        for (i, e) in entries.iter().enumerate() {
            let expected_seq = i as u64;
            if e.seq != expected_seq {
                sometimes!("log.verify found a sequence gap", true);
                return ChainVerify::SequenceGap {
                    expected: expected_seq,
                    found: e.seq,
                };
            }
            let want = entry_hash(&prev, e.seq, &e.header, &payload_digest(&e.payload));
            if want != e.hash {
                sometimes!("log.verify found a broken hash", true);
                return ChainVerify::Broken { seq: e.seq };
            }
            prev = e.hash;
        }
        ChainVerify::Intact {
            len: entries.len() as u64,
            head: prev,
        }
    }
}

// ─── D3 registration ────────────────────────────────────────────────────────

impl Subsystem for CommitLog {
    const NAME: &'static str = "commit-log";

    fn register() -> Registration {
        Registration::new()
            .crash_point("log.before_append")
            .crash_point("log.between_hash_and_publish")
            .sometimes("log.verify found a broken hash")
            .sometimes("log.verify found a sequence gap")
            .counter("log.appended")
            .counter("log.truncated entries")
            .sometimes("log.truncated below the shipped boundary")
            .gate(
                Gate::new(
                    "any mutation breaks the chain at the mutated entry",
                    Canary::new("flip one payload byte and assert verify() still reports Intact"),
                )
                .and_canary(Canary::new("swap two entries and assert Intact"))
                .and_canary(Canary::new("edit a routing header field and assert Intact")),
            )
            .gate(Gate::new(
                "verification needs no key",
                Canary::new(
                    "make entry_hash cover a decrypted payload so a replica must hold the DEK",
                ),
            ))
            .gate(Gate::new(
                "no payload byte reaches the plaintext header",
                Canary::new(
                    "add an open bytes field to RoutingHeader and route a payload through it",
                ),
            ))
    }
}

// ── durable WAL sink ─────────────────────────────────────────────────────────

/// A file-backed sink for the commit log: append-only records, `fsync` on commit.
/// On-disk record is `[seq:8][header:22][payload_len:4][payload][hash:32]`,
/// big-endian lengths. A crash mid-append leaves a TORN tail record, discarded on
/// [`Wal::open`] — the commit that wrote it was never `fsync`'d, so losing it is
/// correct, and every earlier `fsync`'d record survives. The stored per-record
/// hash is checked on replay against the running chain, so a torn or bit-flipped
/// tail is caught rather than replayed as truth.
#[derive(Debug)]
pub struct Wal {
    w: std::io::BufWriter<std::fs::File>,
    /// Where the file lives — a rotation writes the successor beside it and
    /// renames over it.
    path: std::path::PathBuf,
}

/// Where a WAL file's chain starts: the sequence of its first record and the
/// chain hash immediately before it — genesis and zero for a file that was
/// never rotated, the checkpoint boundary for one that was.
///
/// A rotated file verifies its records against THIS anchor instead of
/// genesis, which is what lets a checkpoint drop a durable prefix (the rows
/// are in a sealed segment on disk) without breaking chain verification —
/// the two were mutually exclusive while `Wal::open` assumed genesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalAnchor {
    /// The sequence of the first record in the file.
    pub first_seq: u64,
    /// The chain hash before `first_seq`.
    pub prev_hash: [u8; 32],
}

impl WalAnchor {
    /// The anchor of a never-rotated file.
    pub fn genesis() -> Self {
        WalAnchor {
            first_seq: 0,
            prev_hash: genesis_hash(),
        }
    }
}

/// Process-wide count of real `fsync` calls made by every [`Wal`].
///
/// A DIAGNOSTIC, not part of the determinism trace: `counted!` records only
/// inside `with_trace`, which a running server never installs, so nothing could
/// answer "how many fsyncs did that workload cost" from outside. This can. It
/// is monotonic, never read by engine logic, and exists so a test can assert
/// the group-commit property directly — N concurrent writes cost fewer than N
/// fsyncs, and one write costs exactly one — rather than inferring it from a
/// latency that could equally be explained by a missing fsync.
pub static FSYNCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The WAL file header: `"ENGRWAL1"` then a big-endian format version.
///
/// # Why a header, and why now
///
/// Without one, `Wal::open` could not tell a WAL from any other file — it simply
/// parsed from byte 0 and treated whatever it could not parse as a torn tail.
/// With `good == 0` (a foreign file, a future format, a damaged first record)
/// that meant `set_len(0)`: **opening the wrong path silently destroyed it**.
///
/// Adding this later would be a format break, so the fields are settled here
/// even though not all of them are read yet:
///
/// - `magic` — distinguishes "not a WAL" from "a damaged WAL". Those need
///   different answers: refuse the first, recover the second.
/// - `format_version` — lets a future reader refuse a newer file by name
///   instead of misparsing it.
/// - `first_seq` — the sequence of the first record IN THIS FILE. Zero today
///   because nothing rotates yet, but a checkpointed (prefix-truncated) log
///   needs it, and so does `prev_hash` below: `Store::recover` refuses a suffix
///   with no genesis, so without these two, checkpointing and chain
///   verification are mutually exclusive.
/// - `prev_hash` — the chain hash immediately BEFORE `first_seq`, i.e. the
///   anchor a truncated log verifies against. Genesis today.
///
/// Reserving `first_seq`/`prev_hash` now costs 40 bytes and buys the ability to
/// add rotation without a migration.
pub const WAL_MAGIC: [u8; 8] = *b"ENGRWAL1";
/// Current on-disk WAL format version.
pub const WAL_FORMAT_VERSION: u32 = 1;
/// magic(8) + version(4) + first_seq(8) + prev_hash(32) + reserved(12)
pub const WAL_HEADER_LEN: usize = 64;

/// Why a WAL could not be opened.
///
/// A dedicated error type rather than `io::Error`, because the three cases
/// demand different operator responses and a single "invalid data" would hide
/// which one happened — and the previous behaviour (silently truncating) hid
/// all three.
#[derive(Debug)]
pub enum WalError {
    /// The path exists and is not a WAL. Refused; the file is untouched.
    NotAWal {
        /// The first bytes found, for the operator to identify it.
        found: [u8; 8],
    },
    /// The file is a WAL written by a NEWER format than this build understands.
    /// Refused rather than partially parsed: a reader that ignores fields it
    /// does not know silently drops data.
    FutureFormat {
        /// The version on disk.
        found: u32,
        /// The newest version this build can read.
        supported: u32,
    },
    /// The file is a WAL, but the header itself is incomplete — shorter than a
    /// header, so it was never a usable file.
    ShortHeader {
        /// Bytes present.
        len: usize,
    },
    /// The file was ROTATED (its chain starts at `first_seq`, not genesis) and
    /// the caller asked for a from-genesis log. A rotated WAL belongs to a
    /// paged store whose earlier records live in sealed segments; opening it
    /// as a whole-history log would replay a suffix as the database.
    Rotated {
        /// The sequence the file's chain starts at.
        first_seq: u64,
    },
    /// Underlying I/O.
    Io(std::io::Error),
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::NotAWal { found } => write!(
                f,
                "not an engram WAL (expected magic {:?}, found {:?}) — refusing to touch it",
                String::from_utf8_lossy(&WAL_MAGIC),
                String::from_utf8_lossy(found)
            ),
            WalError::FutureFormat { found, supported } => write!(
                f,
                "WAL format version {found} is newer than this build supports ({supported}) — \
                 refusing rather than misreading it"
            ),
            WalError::ShortHeader { len } => write!(
                f,
                "WAL header truncated at {len} bytes (needs {WAL_HEADER_LEN})"
            ),
            WalError::Rotated { first_seq } => write!(
                f,
                "WAL was rotated (its chain starts at seq {first_seq}, not genesis): it fronts a \
                 paged store's sealed segments and cannot be opened as a whole-history log"
            ),
            WalError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WalError {}

impl From<std::io::Error> for WalError {
    fn from(e: std::io::Error) -> Self {
        WalError::Io(e)
    }
}

impl Wal {
    /// Open (creating if absent) the WAL at `path`, replay every COMPLETE, chain-
    /// valid record into a `Vec<Entry>`, truncate a torn tail, and return the
    /// entries plus a sink positioned to append after them.
    ///
    /// A file that is not a WAL, or is a newer format, is REFUSED and left
    /// byte-for-byte untouched. Only a torn TAIL — records after a valid prefix
    /// — is discarded, and only because the commit that wrote it was never
    /// `fsync`'d, so losing it is correct.
    pub fn open(path: &std::path::Path) -> Result<(Vec<Entry>, Wal), WalError> {
        let (anchor, entries, wal) = Self::open_anchored(path)?;
        if anchor.first_seq != 0 {
            return Err(WalError::Rotated {
                first_seq: anchor.first_seq,
            });
        }
        Ok((entries, wal))
    }

    /// [`Wal::open`] for a file that may have been ROTATED: the records are
    /// verified against the header's anchor (`first_seq`, `prev_hash`) rather
    /// than genesis, and the anchor comes back so the caller can seed its
    /// commit log's sequence and head from it. A never-rotated file anchors
    /// at genesis and behaves exactly as `open`.
    pub fn open_anchored(
        path: &std::path::Path,
    ) -> Result<(WalAnchor, Vec<Entry>, Wal), WalError> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false) // open-or-create; NEVER discard an existing log
            .read(true)
            .write(true)
            .open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        // ── Header: identify the file BEFORE parsing or truncating anything ──
        if buf.is_empty() {
            // A fresh file: write the header and start empty.
            let anchor = WalAnchor::genesis();
            Self::write_header(&mut file, &anchor)?;
            file.sync_all()?;
            return Ok((
                anchor,
                Vec::new(),
                Wal {
                    w: std::io::BufWriter::new(file),
                    path: path.to_path_buf(),
                },
            ));
        }
        if buf.len() < WAL_HEADER_LEN {
            return Err(WalError::ShortHeader { len: buf.len() });
        }
        if buf[..8] != WAL_MAGIC {
            let mut found = [0u8; 8];
            found.copy_from_slice(&buf[..8]);
            return Err(WalError::NotAWal { found });
        }
        let version = u32::from_be_bytes(buf[8..12].try_into().expect("4"));
        if version > WAL_FORMAT_VERSION {
            return Err(WalError::FutureFormat {
                found: version,
                supported: WAL_FORMAT_VERSION,
            });
        }
        let first_seq = u64::from_be_bytes(buf[12..20].try_into().expect("8"));
        let mut prev_hash = [0u8; 32];
        prev_hash.copy_from_slice(&buf[20..52]);
        let anchor = WalAnchor {
            first_seq,
            prev_hash,
        };

        let mut entries: Vec<Entry> = Vec::new();
        // The valid prefix INCLUDES the header, so a torn-tail truncation can
        // never cut into it — which is what made `good == 0` destructive.
        let mut good = WAL_HEADER_LEN;
        let mut off = WAL_HEADER_LEN;
        while off + 8 + HEADER_LEN + 4 <= buf.len() {
            let seq = u64::from_be_bytes(buf[off..off + 8].try_into().unwrap());
            let hdr = off + 8;
            let Some(header) = RoutingHeader::decode(&buf[hdr..hdr + HEADER_LEN]) else {
                break;
            };
            let plen_at = hdr + HEADER_LEN;
            let plen = u32::from_be_bytes(buf[plen_at..plen_at + 4].try_into().unwrap()) as usize;
            let pay_at = plen_at + 4;
            let end = pay_at + plen + 32;
            if end > buf.len() {
                break; // payload/hash truncated → torn tail
            }
            let payload = buf[pay_at..pay_at + plen].to_vec();
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&buf[pay_at + plen..end]);
            // The chain anchors at the header, not at genesis: a rotated
            // file's first record extends the hash the checkpoint recorded.
            let (prev, expected_seq) = match entries.last() {
                Some(e) => (e.hash, e.seq + 1),
                None => (anchor.prev_hash, anchor.first_seq),
            };
            if seq != expected_seq
                || hash != entry_hash(&prev, seq, &header, &payload_digest(&payload))
            {
                break; // torn / bit-flipped tail
            }
            entries.push(Entry {
                seq,
                header,
                payload,
                hash,
            });
            off = end;
            good = end;
        }
        if good != buf.len() {
            file.set_len(good as u64)?; // drop the torn tail
        }
        file.seek(SeekFrom::Start(good as u64))?;
        Ok((
            anchor,
            entries,
            Wal {
                w: std::io::BufWriter::new(file),
                path: path.to_path_buf(),
            },
        ))
    }

    /// The anchor recorded in `path`'s header — `None` for a file that is
    /// absent, empty, or not a WAL. A read of the header alone, for a test or
    /// an operator asking "where does this file's chain start".
    pub fn read_anchor(path: &std::path::Path) -> Option<WalAnchor> {
        use std::io::Read;
        let mut file = std::fs::File::open(path).ok()?;
        let mut hdr = [0u8; WAL_HEADER_LEN];
        file.read_exact(&mut hdr).ok()?;
        if hdr[..8] != WAL_MAGIC {
            return None;
        }
        let first_seq = u64::from_be_bytes(hdr[12..20].try_into().ok()?);
        let mut prev_hash = [0u8; 32];
        prev_hash.copy_from_slice(&hdr[20..52]);
        Some(WalAnchor {
            first_seq,
            prev_hash,
        })
    }

    fn write_header(file: &mut std::fs::File, anchor: &WalAnchor) -> std::io::Result<()> {
        use std::io::Write;
        let mut hdr = Vec::with_capacity(WAL_HEADER_LEN);
        hdr.extend_from_slice(&WAL_MAGIC);
        hdr.extend_from_slice(&WAL_FORMAT_VERSION.to_be_bytes());
        hdr.extend_from_slice(&anchor.first_seq.to_be_bytes());
        hdr.extend_from_slice(&anchor.prev_hash);
        hdr.extend_from_slice(&[0u8; 12]); // reserved
        debug_assert_eq!(hdr.len(), WAL_HEADER_LEN);
        file.write_all(&hdr)
    }

    /// ROTATE the file at `self.path`: write a successor holding only the
    /// records at or after `anchor.first_seq` (`keep`, which the caller took
    /// from its commit log), anchored at the checkpoint boundary; `fsync` it;
    /// rename it over the old file; `fsync` the directory. The successor is
    /// complete before the rename, so a crash at any point leaves either the
    /// whole old file or the whole new one — never a torn checkpoint. The
    /// old handle is dropped with `self`; the returned sink appends to the
    /// new file after its last record.
    ///
    /// The prefix dropped is the checkpoint's retention statement: every
    /// record below `first_seq` is in a sealed segment on disk. Recovery of
    /// those rows is the segment's job now, and the new header's anchor is
    /// what lets the suffix still verify.
    pub fn rotate(&mut self, anchor: WalAnchor, keep: &[Entry]) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let path = self.path.clone();
        let successor = path.with_extension("wal.rotating");
        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&successor)?;
            Self::write_header(&mut file, &anchor)?;
            let mut w = std::io::BufWriter::new(file);
            for e in keep {
                debug_assert!(e.seq >= anchor.first_seq, "a kept record precedes the anchor");
                w.write_all(&e.seq.to_be_bytes())?;
                w.write_all(&e.header.encode())?;
                w.write_all(&(e.payload.len() as u32).to_be_bytes())?;
                w.write_all(&e.payload)?;
                w.write_all(&e.hash)?;
            }
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        // Everything above left `self` untouched: a failure there keeps the
        // old file and the old sink, and the caller's log is still durable.
        // The old handle is flushed before the swap so a failed rename leaves
        // nothing of it unwritten either.
        self.w.flush()?;
        std::fs::rename(&successor, &path)?;
        sync_parent_dir(&path)?;
        let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        file.seek(SeekFrom::End(0))?;
        self.w = std::io::BufWriter::new(file);
        counted!("wal.rotated");
        Ok(())
    }

    fn write_entry(&mut self, e: &Entry) -> std::io::Result<()> {
        use std::io::Write;
        self.w.write_all(&e.seq.to_be_bytes())?;
        self.w.write_all(&e.header.encode())?;
        self.w.write_all(&(e.payload.len() as u32).to_be_bytes())?;
        self.w.write_all(&e.payload)?;
        self.w.write_all(&e.hash)?;
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.w.flush()?;
        FSYNCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.w.get_ref().sync_all()
    }
}

/// `fsync` the directory holding `path`, so a rename in it is durable. A
/// no-op where the platform cannot open a directory for syncing (Windows
/// commits renames through the filesystem's own journal).
pub fn sync_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        if let Some(dir) = path.parent() {
            let d = std::fs::File::open(dir)?;
            d.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
