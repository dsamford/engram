//! D3 — what every subsystem must register, and the vocabulary it registers.
//!
//! # Why registration, rather than just calling the macros
//!
//! The rule that makes this crate worth existing is R14's coverage floor:
//! **a `sometimes!` that never fires is a build failure, not neutral.**
//!
//! That rule is unenforceable without a declaration step. If events only ever
//! come into being by firing, then "never fired" and "does not exist" are the
//! same observation — an empty coverage report reads as a clean run, and a
//! simulation that never injected a fault passes everything. Declaring the
//! event at construction is what makes its absence a *measurable* fact rather
//! than an absence of facts.
//!
//! It is the house defect, in the layer built to detect the house defect.
//!
//! # The four registrations
//!
//! | kind | what it buys |
//! |---|---|
//! | crash points | a place the harness can kill the process, named so a seed can name it |
//! | `sometimes!` events | the coverage floor above |
//! | operation counters | deterministic totals two runs of one seed must agree on |
//! | gates + canaries | a check, and a deliberate violation proving the check can FAIL |
//!
//! # The gate/canary pairing is a type, not a convention
//!
//! R14 asks for an xtask gate that makes "a gate without a canary structurally
//! impossible to add". A [`Gate`] cannot be constructed without one: the only
//! constructor takes a [`Canary`] by value. There is no `Gate::new(name)`, no
//! `Default`, and no public field to leave empty — so the invalid state is not
//! rejected at review or at CI, it is unrepresentable. `cargo xtask d3` then
//! checks the other direction, which types cannot: that every workspace member
//! declaring a subsystem actually registers in all four categories.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

// ─── The four registered kinds ──────────────────────────────────────────────

/// A named point at which the harness may kill the process.
///
/// Named, because a seed has to be able to say *where* it crashed. An anonymous
/// crash point cannot appear in a minimised repro.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CrashPoint(pub &'static str);

/// An event the harness expects to observe *sometimes* across a sweep.
///
/// Declared here so that never observing it is a finding rather than a silence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SometimesEvent(pub &'static str);

/// A counter whose value two runs of the same seed must agree on exactly.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CounterDecl(pub &'static str);

/// A deliberate violation of a gate, which MUST make that gate fail.
///
/// A gate nobody has watched fail is not known to be in force. This repo has
/// shipped inert guards more than once: a regex mangled through two layers of
/// quoting that could not match anything, and an audit that skipped the very
/// site it appeared to clear. Both reported success.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Canary {
    /// What is broken to provoke the failure.
    pub violation: &'static str,
}

impl Canary {
    /// Declare a deliberate violation.
    pub const fn new(violation: &'static str) -> Self {
        Self { violation }
    }
}

/// A check, together with at least one deliberate violation of it.
///
/// The single constructor takes a [`Canary`], so a gate with no canary cannot
/// be built. That is the "structurally impossible" R14 asks for, moved from a
/// CI script into the type system where it cannot be skipped, allowed, or run
/// on a stale checkout.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gate {
    name: &'static str,
    canaries: Vec<Canary>,
}

impl Gate {
    /// Declare a gate. A first canary is required by the signature.
    pub fn new(name: &'static str, first_canary: Canary) -> Self {
        Self {
            name,
            canaries: vec![first_canary],
        }
    }

    /// Add a further deliberate violation.
    #[must_use]
    pub fn and_canary(mut self, canary: Canary) -> Self {
        self.canaries.push(canary);
        self
    }

    /// The gate's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Its canaries. Never empty — the constructor requires one.
    pub fn canaries(&self) -> &[Canary] {
        &self.canaries
    }
}

// ─── Registration ───────────────────────────────────────────────────────────

/// The four categories, so a missing one can be *named* rather than inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// [`CrashPoint`]s.
    CrashPoints,
    /// [`SometimesEvent`]s.
    SometimesEvents,
    /// [`CounterDecl`]s.
    Counters,
    /// [`Gate`]s.
    Gates,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Kind::CrashPoints => "crash points",
            Kind::SometimesEvents => "sometimes! events",
            Kind::Counters => "operation counters",
            Kind::Gates => "gates (each with >=1 canary)",
        };
        f.write_str(s)
    }
}

/// What one subsystem declared at construction.
#[derive(Debug, Default, Clone)]
pub struct Registration {
    crash_points: Vec<CrashPoint>,
    sometimes: Vec<SometimesEvent>,
    counters: Vec<CounterDecl>,
    gates: Vec<Gate>,
}

impl Registration {
    /// An empty registration. Not valid on its own — see [`Registration::missing`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a crash point.
    #[must_use]
    pub fn crash_point(mut self, name: &'static str) -> Self {
        self.crash_points.push(CrashPoint(name));
        self
    }

    /// Declare a `sometimes!` event that the sweep must observe at least once.
    #[must_use]
    pub fn sometimes(mut self, name: &'static str) -> Self {
        self.sometimes.push(SometimesEvent(name));
        self
    }

    /// Declare a deterministic operation counter.
    #[must_use]
    pub fn counter(mut self, name: &'static str) -> Self {
        self.counters.push(CounterDecl(name));
        self
    }

    /// Declare a gate. Its canary is required by [`Gate::new`].
    #[must_use]
    pub fn gate(mut self, gate: Gate) -> Self {
        self.gates.push(gate);
        self
    }

    /// Declared crash points.
    pub fn crash_points(&self) -> &[CrashPoint] {
        &self.crash_points
    }

    /// Declared `sometimes!` events.
    pub fn sometimes_events(&self) -> &[SometimesEvent] {
        &self.sometimes
    }

    /// Declared counters.
    pub fn counters(&self) -> &[CounterDecl] {
        &self.counters
    }

    /// Declared gates.
    pub fn gates(&self) -> &[Gate] {
        &self.gates
    }

    /// Which of the four categories are empty.
    ///
    /// Returns every missing kind rather than the first: reporting one at a
    /// time turns a single fix-and-rerun cycle into four, and the second
    /// omission reads as "introduced by the fix".
    pub fn missing(&self) -> Vec<Kind> {
        let mut out = Vec::new();
        if self.crash_points.is_empty() {
            out.push(Kind::CrashPoints);
        }
        if self.sometimes.is_empty() {
            out.push(Kind::SometimesEvents);
        }
        if self.counters.is_empty() {
            out.push(Kind::Counters);
        }
        if self.gates.is_empty() {
            out.push(Kind::Gates);
        }
        out
    }
}

/// D3: every subsystem declares its testability surface at construction.
pub trait Subsystem {
    /// Stable identifier, used in traces and in gate output.
    const NAME: &'static str;

    /// Declare crash points, `sometimes!` events, counters and gates.
    fn register() -> Registration;
}

/// Register a subsystem, refusing an incomplete declaration.
///
/// # Panics
///
/// If any of the four categories is empty, naming every missing one. This is a
/// panic rather than a `Result` on purpose: it runs at construction, and a
/// subsystem that has not declared its testability surface must not be able to
/// start and then be discovered later to be unobservable.
pub fn register<S: Subsystem>() -> Registration {
    let reg = S::register();
    let missing = reg.missing();
    assert!(
        missing.is_empty(),
        "subsystem `{}` registered no {} — D3 requires all four, because an \
         undeclared category cannot be reported as uncovered later; it is \
         simply invisible, which reads exactly like covered",
        S::NAME,
        missing
            .iter()
            .map(Kind::to_string)
            .collect::<Vec<_>>()
            .join(", no "),
    );
    reg
}

// ─── The trace: what the determinism gate hashes ────────────────────────────

/// One observation, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Monotonic position in the run.
    pub seq: u64,
    /// What kind of observation this is.
    pub tag: EventTag,
    /// The name given at the call site.
    pub name: String,
}

/// The assertion vocabulary, borrowed from Antithesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTag {
    /// `always!` held.
    AlwaysHeld,
    /// `always!` was violated.
    AlwaysViolated,
    /// `sometimes!` fired with a true condition.
    SometimesHit,
    /// `sometimes!` was evaluated and its condition was false.
    ///
    /// Recorded distinctly from not being evaluated at all. "Reached but false"
    /// and "never reached" are different findings with different fixes, and
    /// collapsing them is how a coverage report comes to say nothing.
    SometimesMissed,
    /// `reachable!` was reached.
    Reachable,
    /// `unreachable!` was reached, which is a violation.
    UnreachableHit,
    /// A counter was incremented.
    Count,
    /// A registered crash point was PASSED (not fired).
    CrashPointPassed,
}

impl EventTag {
    fn as_str(self) -> &'static str {
        match self {
            EventTag::AlwaysHeld => "always.held",
            EventTag::AlwaysViolated => "always.VIOLATED",
            EventTag::SometimesHit => "sometimes.hit",
            EventTag::SometimesMissed => "sometimes.missed",
            EventTag::Reachable => "reachable",
            EventTag::UnreachableHit => "unreachable.HIT",
            EventTag::Count => "count",
            EventTag::CrashPointPassed => "crash_point.passed",
        }
    }
}

/// The ordered record of a run.
#[derive(Debug, Default, Clone)]
pub struct Trace {
    events: Vec<Event>,
    counters: BTreeMap<String, u64>,
    next_seq: u64,
}

impl Trace {
    /// An empty trace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an observation.
    pub fn record(&mut self, tag: EventTag, name: impl Into<String>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push(Event {
            seq,
            tag,
            name: name.into(),
        });
    }

    /// Increment a named counter and record the increment.
    pub fn count(&mut self, name: &str, by: u64) {
        *self.counters.entry(name.to_string()).or_insert(0) += by;
        self.record(EventTag::Count, name);
    }

    /// The events, in order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Final counter values, in a deterministic (sorted) order.
    pub fn counters(&self) -> &BTreeMap<String, u64> {
        &self.counters
    }

    /// Every violation the run recorded.
    pub fn violations(&self) -> Vec<&Event> {
        self.events
            .iter()
            .filter(|e| matches!(e.tag, EventTag::AlwaysViolated | EventTag::UnreachableHit))
            .collect()
    }

    /// Names of `sometimes!` events that fired at least once.
    pub fn sometimes_hit(&self) -> BTreeSet<&str> {
        self.events
            .iter()
            .filter(|e| e.tag == EventTag::SometimesHit)
            .map(|e| e.name.as_str())
            .collect()
    }

    /// A hash of the whole trace, for run-to-run comparison.
    ///
    /// Hand-rolled FNV-1a rather than `DefaultHasher`: `RandomState` is seeded
    /// per process, so two runs of the same seed would hash differently and the
    /// determinism gate would fail on its own instrument. Hashing the SEQUENCE
    /// (not a set) is the point — a run that produces the same events in a
    /// different order is not the same run.
    pub fn digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
        };
        for e in &self.events {
            mix(&e.seq.to_le_bytes());
            mix(e.tag.as_str().as_bytes());
            mix(e.name.as_bytes());
            mix(b"\x1e");
        }
        // Counter TOTALS as well as their increments: an off-by-one that adds
        // 2 where it should add 1 produces the same event sequence.
        for (k, v) in &self.counters {
            mix(k.as_bytes());
            mix(&v.to_le_bytes());
            mix(b"\x1f");
        }
        h
    }
}

// ─── The per-run recorder the macros write to ───────────────────────────────

thread_local! {
    static CURRENT: RefCell<Option<Trace>> = const { RefCell::new(None) };
}

/// Run `f` with a fresh trace installed, and return it alongside the result.
///
/// Thread-local rather than global because D2 is single-threaded-per-shard: one
/// shard, one trace, no cross-thread interleaving to make the order depend on
/// the scheduler.
pub fn with_trace<T>(f: impl FnOnce() -> T) -> (T, Trace) {
    CURRENT.with(|c| *c.borrow_mut() = Some(Trace::new()));
    let out = f();
    let trace = CURRENT.with(|c| c.borrow_mut().take()).unwrap_or_default();
    (out, trace)
}

/// Run `f` with trace recording SUPPRESSED: the installed trace (if any) is set
/// aside for the duration and restored afterward, so events during `f` are
/// recorded NOWHERE. For internal sub-evaluations — a query-rewrite's prelude
/// probe — whose events must not pollute the OUTER query's trace / counters.
/// Nesting-safe (it saves and restores, unlike `with_trace`, which installs a
/// fresh trace and takes it).
pub fn with_suppressed_trace<T>(f: impl FnOnce() -> T) -> T {
    let saved = CURRENT.with(|c| c.borrow_mut().take());
    let out = f();
    CURRENT.with(|c| *c.borrow_mut() = saved);
    out
}

/// Record into the installed trace, if there is one.
///
/// Outside [`with_trace`] this is a no-op: production code carrying assertion
/// macros must not pay for a recorder nobody installed.
pub fn record(tag: EventTag, name: &str) {
    CURRENT.with(|c| {
        if let Some(t) = c.borrow_mut().as_mut() {
            t.record(tag, name);
        }
    });
}

/// Increment a counter in the installed trace, if there is one.
pub fn count(name: &str, by: u64) {
    CURRENT.with(|c| {
        if let Some(t) = c.borrow_mut().as_mut() {
            t.count(name, by);
        }
    });
}

// ─── Crash injection ────────────────────────────────────────────────────────

thread_local! {
    static ARMED_CRASH: RefCell<Option<&'static str>> = const { RefCell::new(None) };
}

/// The panic payload a fired crash point unwinds with.
///
/// A distinct type, so a harness catching the unwind can tell an INJECTED
/// crash from a genuine bug's panic. Conflating them is how a fault harness
/// reports "recovered cleanly" over a run that actually hit an assertion.
#[derive(Debug)]
pub struct InjectedCrash {
    /// The crash point that fired.
    pub at: &'static str,
}

/// Run `f` with the named crash point ARMED: the first time execution reaches
/// it, the run panics with [`InjectedCrash`].
///
/// Returns `Ok(value)` when `f` completes without reaching the point (which
/// the caller should treat as a FINDING — an armed point that was never
/// reached means the schedule did not exercise the boundary), and `Err` with
/// the crash when it fired.
///
/// Panics that are NOT the injected crash are resumed — a real bug must fail
/// the test as itself, not dissolve into the harness's expected unwind.
pub fn with_crash_at<T>(point: &'static str, f: impl FnOnce() -> T) -> Result<T, InjectedCrash> {
    ARMED_CRASH.with(|c| *c.borrow_mut() = Some(point));
    // AssertUnwindSafe, deliberately and soundly. The unwind SIMULATES a
    // process kill, and observing the state it leaves behind is the entire
    // point of the harness — the "logical invariant may be broken" that
    // UnwindSafe warns about is the object under test, not a hazard. RefCell
    // borrow flags are released correctly during unwind (the guards' Drop
    // runs), so the post-crash structures are mechanically usable; whether
    // their CONTENTS are consistent is what the recovery assertions decide.
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    ARMED_CRASH.with(|c| *c.borrow_mut() = None);
    match out {
        Ok(v) => Ok(v),
        Err(payload) => match payload.downcast::<InjectedCrash>() {
            Ok(crash) => Err(*crash),
            Err(other) => std::panic::resume_unwind(other),
        },
    }
}

/// Marks a place the harness may kill the process — D3's crash points, live.
///
/// Disarmed (the default), it records a [`EventTag::CrashPointPassed`] event
/// and costs a thread-local read. Armed with this point's name, it panics with
/// [`InjectedCrash`] — the in-process stand-in for `kill -9` at this boundary,
/// which the recovery test then survives or does not.
pub fn crash_point(name: &'static str) {
    let armed = ARMED_CRASH.with(|c| {
        let mut b = c.borrow_mut();
        if *b == Some(name) {
            // One shot: the same point reached again after recovery must not
            // crash the recovery. Re-arm explicitly to crash twice.
            *b = None;
            true
        } else {
            false
        }
    });
    if armed {
        std::panic::panic_any(InjectedCrash { at: name });
    }
    record(EventTag::CrashPointPassed, name);
}

// ─── Coverage: the rule this crate exists for ───────────────────────────────

/// The result of comparing declared `sometimes!` events against observed ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// Declared and observed at least once.
    pub hit: Vec<String>,
    /// Declared and NEVER observed. Any entry here is a build failure.
    pub never_fired: Vec<String>,
    /// Observed but never declared — the gate cannot vouch for these.
    ///
    /// Not an error, but reported: an undeclared event is outside the coverage
    /// floor, so it can silently stop firing without anything noticing.
    pub undeclared: Vec<String>,
}

impl Coverage {
    /// True when every declared event fired at least once.
    pub fn is_covered(&self) -> bool {
        self.never_fired.is_empty()
    }
}

/// Compare what was declared against what a sweep observed.
///
/// `declared` is the union of every subsystem's [`Registration::sometimes_events`];
/// `traces` is every run in the sweep. A single run is not expected to hit
/// every event — the floor is over the sweep, which is what makes swarm
/// configuration meaningful rather than decorative.
pub fn coverage<'a>(
    declared: impl IntoIterator<Item = &'a SometimesEvent>,
    traces: impl IntoIterator<Item = &'a Trace>,
) -> Coverage {
    let declared: BTreeSet<&str> = declared.into_iter().map(|e| e.0).collect();
    let mut observed: BTreeSet<String> = BTreeSet::new();
    for t in traces {
        for name in t.sometimes_hit() {
            observed.insert(name.to_string());
        }
    }
    let hit = declared
        .iter()
        .filter(|d| observed.contains(**d))
        .map(|d| (*d).to_string())
        .collect();
    let never_fired = declared
        .iter()
        .filter(|d| !observed.contains(**d))
        .map(|d| (*d).to_string())
        .collect();
    let undeclared = observed
        .iter()
        .filter(|o| !declared.contains(o.as_str()))
        .cloned()
        .collect();
    Coverage {
        hit,
        never_fired,
        undeclared,
    }
}

// ─── The vocabulary ─────────────────────────────────────────────────────────

/// Assert an invariant that must hold on every run.
#[macro_export]
macro_rules! always {
    ($name:expr, $cond:expr $(,)?) => {{
        let held: bool = $cond;
        $crate::record(
            if held {
                $crate::EventTag::AlwaysHeld
            } else {
                $crate::EventTag::AlwaysViolated
            },
            $name,
        );
        held
    }};
}

/// Record that an interesting state was reached, with a condition.
///
/// The anti-house-defect primitive: the sweep requires this to fire at least
/// once, so a fault injector that never injects becomes a failure instead of a
/// clean run.
#[macro_export]
macro_rules! sometimes {
    ($name:expr, $cond:expr $(,)?) => {{
        let hit: bool = $cond;
        $crate::record(
            if hit {
                $crate::EventTag::SometimesHit
            } else {
                $crate::EventTag::SometimesMissed
            },
            $name,
        );
        hit
    }};
    ($name:expr $(,)?) => {{
        $crate::record($crate::EventTag::SometimesHit, $name);
        true
    }};
}

/// Record that a branch was reached.
#[macro_export]
macro_rules! reachable {
    ($name:expr $(,)?) => {{
        $crate::record($crate::EventTag::Reachable, $name);
    }};
}

/// Record that a branch believed impossible was reached. Always a violation.
#[macro_export]
macro_rules! unreachable_hit {
    ($name:expr $(,)?) => {{
        $crate::record($crate::EventTag::UnreachableHit, $name);
    }};
}

/// Increment a declared operation counter.
#[macro_export]
macro_rules! counted {
    ($name:expr $(,)?) => {{
        $crate::count($name, 1);
    }};
    ($name:expr, $by:expr $(,)?) => {{
        $crate::count($name, $by);
    }};
}
