// A data generator, not the engine: its hash maps are lookups, and the one it
// iterates is sorted first (the withdraw pairs), so a seed still names one corpus.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! FinBench data, generated here instead of by Spark.
//!
//! LDBC's FinBench Datagen is a Spark job, and the bench node has no Spark —
//! so the suite has run at SF0.01 (the one corpus we were given) and nowhere
//! else, which leaves the financial workload with no scaling story at all.
//! This produces the same eighteen files, with the same headers, at any scale.
//!
//! **This is not LDBC's generator.** It follows the FinBench schema and is
//! calibrated against the SF0.01 corpus LDBC produced — entity counts, the
//! ratios between them, and the shapes the queries traverse — but the data is
//! ours. Numbers taken on it describe how Engram handles a FinBench-shaped
//! financial graph at size; they are not official FinBench results, and
//! anything published from them has to say so.
//!
//! What it must get right is not realism but REACHABILITY: a generator that
//! scatters edges uniformly produces a graph where the twelve queries match
//! nothing, and an empty answer returned quickly looks like a fast engine.
//! So the structures the queries look for are built deliberately — transfer
//! hubs and cycles, guarantee chains, loans that are deposited and then repaid
//! from the accounts that received them, and media that sign in to many
//! accounts at once.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Counts in LDBC's own SF0.01 corpus, which everything here scales from.
/// Keeping them in one place makes the calibration checkable: at scale 0.01
/// the generator should land on these, and `fbsf1.sh` compares a generated
/// corpus against LDBC's official one row-type by row-type.
///
/// Calibrating from ONE scale cannot tell a scale-invariant ratio apart from
/// one that only looks constant because the corpus is small. Every figure here
/// has since been checked against LDBC's official SF1, 100x away: the entity
/// and edge counts all land within 4%, and the multi-edge ratios within 1.5
/// points — except withdraw, which is not ratio-shaped at all. See
/// `WITHDRAW_SRC_POOL`.
mod sf001 {
    pub const PERSON: f64 = 785.0;
    pub const COMPANY: f64 = 386.0;
    pub const MEDIUM: f64 = 978.0;
    pub const ACCOUNT: f64 = 2055.0;
    pub const LOAN: f64 = 1376.0;
    pub const PERSON_OWN: f64 = 1384.0;
    // the reference count; the generator gives companies the accounts persons
    // do not own, so it is the remainder that is written, not this
    #[allow(dead_code)]
    pub const COMPANY_OWN: f64 = 671.0;
    pub const TRANSFER: f64 = 8132.0;
    pub const WITHDRAW: f64 = 9182.0;
    pub const REPAY: f64 = 2747.0;
    pub const DEPOSIT: f64 = 2758.0;
    pub const PERSON_APPLY: f64 = 927.0;
    // the reference count; companies apply for the loans persons do not
    #[allow(dead_code)]
    pub const COMPANY_APPLY: f64 = 449.0;
    pub const PERSON_GUARANTEE: f64 = 377.0;
    pub const COMPANY_GUARANTEE: f64 = 202.0;
    pub const PERSON_INVEST: f64 = 1304.0;
    pub const COMPANY_INVEST: f64 = 679.0;
    pub const SIGN_IN: f64 = 2489.0;

    /// DISTINCT PAIRS over ROWS, measured on the same corpus. Five of the edge
    /// types allow several edges between one ordered pair, and the harness
    /// notes why that matters: `AccountTransferAccount` holds 8,132 edges over
    /// 6,128 pairs, so anything keying adjacency by the pair loses a quarter
    /// of the benchmark's busiest edge type, loads clean, and answers every
    /// query. A generator that emits distinct pairs only would hide exactly
    /// the defect this family exists to catch.
    pub const TRANSFER_DISTINCT: f64 = 6128.0 / 8132.0;
    pub const DEPOSIT_DISTINCT: f64 = 1514.0 / 2758.0;
    pub const REPAY_DISTINCT: f64 = 1481.0 / 2747.0;
    pub const SIGN_IN_DISTINCT: f64 = 631.0 / 2489.0;

    /// WITHDRAW IS NOT SKEWED, AND ITS MULTI-EDGE RATIO IS NOT SCALE-INVARIANT.
    ///
    /// The four ratios above hold at every scale because they are produced by
    /// SKEW: a hub keeps its share of the edges as the graph grows, so the
    /// fraction of rows that repeat a pair stays put. Checked against LDBC's
    /// official SF1 corpus, all four land within 1.5 points.
    ///
    /// Withdraw does not work that way, and a `WITHDRAW_DISTINCT = 8782/9182`
    /// constant here reproduced SF0.01 exactly and was wrong everywhere else —
    /// it put 4.4% multi-edges in a generated SF1 where LDBC has 0.06%. The
    /// edge is drawn UNIFORMLY over two restricted pools, and its duplicates
    /// are nothing but the birthday effect over those pools:
    ///
    /// ```text
    ///          accounts  src pool  dst pool   rows   distinct  predicted
    /// SF0.01       2055       568       181   9182      8782       8783
    /// SF1        204771     56686     15702 907482    906967     907020
    /// ```
    ///
    /// `distinct = P(1 - e^(-rows/P))` over `P = src*dst` matches the measured
    /// count to within one edge at both scales, 100x apart. So the invariant
    /// to reproduce is the POOL SIZE, not the ratio: the pools are a fixed
    /// share of the accounts, and the duplicate count then falls out on its
    /// own — from 4.4% at SF0.01 to 0.06% at SF1, which is exactly the spread
    /// the old constant flattened.
    ///
    /// Semantically these pools are why the shape matters: 907k withdrawals
    /// land on 15,702 destinations, so withdraw is a funnel into a small set
    /// of cash/ATM-like accounts. A generator that spreads it over every
    /// account removes the funnel, and any query that traverses withdraw then
    /// meets a fan-out the real corpus never has.
    ///
    /// The source share is firm (27.64% at SF0.01, 27.68% at SF1). The
    /// destination share drifts — 8.81% against 7.67% — so the SF1 value is
    /// taken, being both the larger corpus and the scale that matters.
    pub const WITHDRAW_SRC_POOL: f64 = 56686.0 / 204771.0;
    pub const WITHDRAW_DST_POOL: f64 = 15702.0 / 204771.0;
}

/// How sharply transfers concentrate on hub accounts, applied to BOTH ends
/// because LDBC's transfer graph is symmetric. Raising it makes the busiest
/// account busier; see the call site for what it was fitted against.
const TRANSFER_SKEW: f64 = 1.6;

/// SplitMix64 — a seeded generator, so a scale factor and a seed name exactly
/// one corpus. Reproducibility is the point: a benchmark whose data changes
/// between runs cannot be compared with itself.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
    /// A skewed index: small indices are drawn far more often, which is what
    /// makes some accounts hubs. Without this every account has the same
    /// degree and the queries that look for concentration find nothing.
    fn skewed(&mut self, n: usize, power: f64) -> usize {
        if n == 0 {
            return 0;
        }
        let u = self.unit().powf(power);
        ((n as f64 * u) as usize).min(n - 1)
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// A shuffled 0..n, so "rank 0" means a different node for each edge type.
fn shuffled(rng: &mut Rng, n: usize) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        v.swap(i, rng.below(i + 1));
    }
    v
}

/// HUBS MUST NOT COINCIDE ACROSS EDGE TYPES.
///
/// Skewing every edge type toward the same low indices makes one account the
/// busiest at everything, and a query looking for a coincidence of two
/// behaviours then finds one everywhere. TCR6 asks for accounts that both
/// withdrew to a target AND received more than three transfers: with shared
/// hubs it returned 84 rows on a corpus where LDBC's own data returns none.
///
/// So each edge type draws its rank from its own shuffled ordering, and being
/// a transfer hub says nothing about being a withdrawal hub.
struct Hubs {
    transfer_out: Vec<usize>,
    transfer_in: Vec<usize>,
    withdraw_out: Vec<usize>,
    withdraw_in: Vec<usize>,
    deposit_in: Vec<usize>,
    signin_account: Vec<usize>,
}

impl Hubs {
    fn build(rng: &mut Rng, accounts: usize) -> Self {
        Hubs {
            transfer_out: shuffled(rng, accounts),
            transfer_in: shuffled(rng, accounts),
            withdraw_out: shuffled(rng, accounts),
            withdraw_in: shuffled(rng, accounts),
            deposit_in: shuffled(rng, accounts),
            signin_account: shuffled(rng, accounts),
        }
    }
}

const START_MS: i64 = 1_577_836_800_000; // 2020-01-01T00:00:00Z
const WINDOW_MS: i64 = 730 * 86_400_000; // two years

/// `k` ascending timestamps for ONE repeated pair, all inside the window.
///
/// A repeated transfer must be a LATER transfer, not the same one twice, and
/// the obvious way to get that — start somewhere and add a few days per repeat
/// — walks out of the corpus. The busiest account at SF10 carried more than a
/// hundred edges stamped beyond 2027 against a window that ends in 2022,
/// because a pair with many repeats advances once per repeat and nothing
/// brings it back. Every FinBench read filters on a time window, so those
/// edges are invisible to the queries that are supposed to traverse them, and
/// a benchmark that cannot see its own busiest edges is measuring the filter.
///
/// Drawing `k` independent points and sorting keeps both properties: the
/// repeats are strictly ordered, and every one of them is a timestamp the
/// corpus actually spans.
fn ordered_times(rng: &mut Rng, k: usize) -> Vec<i64> {
    let mut ts: Vec<i64> = (0..k.max(1))
        .map(|_| START_MS + (rng.next() % WINDOW_MS as u64) as i64)
        .collect();
    ts.sort_unstable();
    // distinct, so a repeated pair reads as several events rather than one
    for i in 1..ts.len() {
        if ts[i] <= ts[i - 1] {
            ts[i] = ts[i - 1] + 1;
        }
    }
    ts
}

/// `k` ascending timestamps strictly after `after`, and still inside the
/// window. Deposit follows the loan it draws on and repayment follows the
/// deposit, so those cannot be drawn from the whole window — but they must not
/// walk out of it either, for the reason `ordered_times` gives.
///
/// If the anchor is already at or past the window's end there is no room left,
/// and the only thing that preserves the causal order is to step past it; that
/// is rare and stays a bounded number of milliseconds rather than months.
fn ordered_times_after(rng: &mut Rng, k: usize, after: i64) -> Vec<i64> {
    let end = START_MS + WINDOW_MS;
    let lo = after + 1;
    if lo >= end {
        return (0..k.max(1)).map(|i| lo + i as i64).collect();
    }
    let span = (end - lo) as u64;
    let mut ts: Vec<i64> = (0..k.max(1))
        .map(|_| lo + (rng.next() % span) as i64)
        .collect();
    ts.sort_unstable();
    for i in 1..ts.len() {
        if ts[i] <= ts[i - 1] {
            ts[i] = ts[i - 1] + 1;
        }
    }
    ts
}

/// `YYYY-MM-DD HH:MM:SS.mmm`, the format every `createTime` column uses.
fn stamp(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil(days);
    let (h, mi, s, milli) = (
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1000) % 60,
        rem % 1000,
    );
    let mut out = String::with_capacity(23);
    let _ = write!(out, "{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{milli:03}");
    out
}

fn date_only(ms: i64) -> String {
    let (y, m, d) = civil(ms.div_euclid(86_400_000));
    let mut out = String::with_capacity(19);
    let _ = write!(out, "{y:04}-{m:02}-{d:02} 00:00:00");
    out
}

/// Days since the epoch to a civil date (Howard Hinnant's algorithm).
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `rows` edges over `rows * ratio` distinct ordered pairs, drawn by `draw`.
///
/// The extra occurrences are concentrated rather than spread: a pair that
/// repeats once usually repeats again, which is what makes a handful of
/// (medium, account) pairs carry most of the sign-ins in the real corpus.
fn pairs_with_multiplicity(
    rng: &mut Rng,
    rows: usize,
    ratio: f64,
    mut draw: impl FnMut(&mut Rng) -> (usize, usize),
) -> Vec<((usize, usize), usize)> {
    let distinct = ((rows as f64 * ratio).round() as usize).clamp(1, rows);
    let mut seen = std::collections::HashSet::with_capacity(distinct * 2);
    let mut pairs: Vec<((usize, usize), usize)> = Vec::with_capacity(distinct);
    let mut guard = 0usize;
    while pairs.len() < distinct && guard < distinct * 64 {
        guard += 1;
        let (a, b) = draw(rng);
        if a == b || !seen.insert((a, b)) {
            continue;
        }
        pairs.push(((a, b), 1));
    }
    if pairs.is_empty() {
        return pairs;
    }
    let mut extra = rows.saturating_sub(pairs.len());
    while extra > 0 {
        let i = rng.skewed(pairs.len(), 2.0);
        // a repeat begets repeats: take several at once from the same pair
        let take = (1 + rng.below(3)).min(extra);
        pairs[i].1 += take;
        extra -= take;
    }
    pairs
}

struct Csv(BufWriter<fs::File>);

impl Csv {
    fn create(dir: &Path, name: &str, header: &str) -> std::io::Result<Self> {
        let mut w = BufWriter::with_capacity(1 << 20, fs::File::create(dir.join(name))?);
        writeln!(w, "{header}")?;
        Ok(Csv(w))
    }
    fn row(&mut self, line: &str) -> std::io::Result<()> {
        writeln!(self.0, "{line}")
    }
}

const COUNTRIES: [&str; 8] = [
    "Australia",
    "Tunisia",
    "Vietnam",
    "Japan",
    "Brazil",
    "Canada",
    "Italy",
    "Kenya",
];
const CITIES: [&str; 8] = [
    "Adelaide",
    "La_Marsa",
    "Bắc_Giang",
    "Osaka",
    "Recife",
    "Halifax",
    "Bologna",
    "Nakuru",
];
const FIRST: [&str; 10] = [
    "Bertrand", "Arlena", "Mikael", "Sofia", "Jun", "Ana", "Tomas", "Leila", "Ivan", "Noor",
];
const LAST: [&str; 8] = [
    "Penha",
    "Rosenbaum",
    "Okafor",
    "Lindqvist",
    "Moretti",
    "Haddad",
    "Novak",
    "Tanaka",
];
const ACCOUNT_TYPE: [&str; 4] = [
    "brokerage account",
    "debit card",
    "savings account",
    "credit card",
];
const LOGIN_TYPE: [&str; 4] = ["QRCode", "APP", "WEB", "SMS"];
const ACCOUNT_LEVEL: [&str; 3] = ["Basic level", "Gold level", "Platinum level"];
const MEDIUM_TYPE: [&str; 4] = ["PHONE", "PC", "TABLET", "IOT"];
const RISK: [&str; 3] = ["Low risk", "Moderate risk", "High risk"];
const LOAN_USAGE: [&str; 5] = ["vacations", "business", "education", "housing", "medical"];
const ORG: [&str; 5] = [
    "Avant",
    "State Farm Bank",
    "Lending Club",
    "Prosper",
    "SoFi",
];
const RELATION: [&str; 4] = ["parents", "business associate", "classmate", "neighbour"];
const PAY_TYPE: [&str; 3] = ["card", "transfer", "wallet"];
const GOODS_TYPE: [&str; 4] = ["electronics", "grocery", "travel", "services"];

/// Type-tagged id ranges, so an id says what it belongs to — the same idea
/// LDBC's corpus uses, and it makes a dangling reference obvious rather than
/// plausible.
const COMPANY_BASE: u64 = 1 << 40;
const MEDIUM_BASE: u64 = 1 << 42;
const LOAN_BASE: u64 = 1 << 61;
const ACCOUNT_BASE: u64 = 1 << 62;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut scale = 1.0f64;
    let mut out = PathBuf::from("/datasets/finbench/generated");
    let mut seed = 42u64;
    let mut i = 1;
    while i < args.len() {
        // each arm consumes its own value; nothing advances afterwards, or a
        // flag is skipped and its value read as a flag
        let need = |i: usize| -> &str {
            args.get(i + 1).map(String::as_str).unwrap_or_else(|| {
                eprintln!("fbgen: `{}` needs a value", args[i]);
                std::process::exit(2);
            })
        };
        match args[i].as_str() {
            "--scale" => {
                scale = need(i).parse().expect("--scale takes a number");
                i += 2
            }
            "--out" => {
                out = PathBuf::from(need(i));
                i += 2
            }
            "--seed" => {
                seed = need(i).parse().expect("--seed takes a number");
                i += 2
            }
            other => {
                eprintln!("fbgen: unknown argument `{other}`");
                eprintln!("usage: fbgen --scale <sf> --out <dir> [--seed <n>]");
                std::process::exit(2);
            }
        }
    }

    let snapshot = out.join("snapshot");
    fs::create_dir_all(&snapshot)?;
    let k = scale / 0.01; // everything is calibrated against the SF0.01 corpus
    let n = |per_001: f64| -> usize { (per_001 * k).round().max(1.0) as usize };

    let persons = n(sf001::PERSON);
    let companies = n(sf001::COMPANY);
    let media = n(sf001::MEDIUM);
    let accounts = n(sf001::ACCOUNT);
    let loans = n(sf001::LOAN);
    let mut rng = Rng(seed ^ ((scale * 1e6) as u64));
    let hubs = Hubs::build(&mut rng, accounts);

    eprintln!(
        "[fbgen] scale {scale}: {persons} persons, {companies} companies, {media} media, \
         {accounts} accounts, {loans} loans -> {}",
        snapshot.display()
    );

    // ── entities ───────────────────────────────────────────────────────────
    let mut f = Csv::create(
        &snapshot,
        "Person.csv",
        "personId|personName|isBlocked|createTime|gender|birthday|country|city",
    )?;
    let mut person_created = Vec::with_capacity(persons);
    for id in 0..persons {
        let t = START_MS + (rng.next() % (WINDOW_MS as u64 / 4)) as i64;
        person_created.push(t);
        let name = format!("{} {}", rng.pick(&FIRST), rng.pick(&LAST));
        let birth = START_MS - (rng.below(30) as i64 + 18) * 365 * 86_400_000;
        f.row(&format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            id + 1,
            name,
            rng.chance(0.03),
            stamp(t),
            if rng.chance(0.5) { "male" } else { "female" },
            date_only(birth),
            rng.pick(&COUNTRIES),
            rng.pick(&CITIES)
        ))?;
    }

    let mut f = Csv::create(
        &snapshot,
        "Company.csv",
        "companyId|companyName|isBlocked|createTime|country|city|business|description|url",
    )?;
    for id in 0..companies {
        let t = START_MS + (rng.next() % (WINDOW_MS as u64 / 4)) as i64;
        f.row(&format!(
            "{}|{}-{}|{}|{}|{}|{}|{}|generated for scale testing|http://example.invalid/{}",
            COMPANY_BASE + id as u64,
            rng.pick(&LAST),
            rng.pick(&LAST),
            rng.chance(0.04),
            stamp(t),
            rng.pick(&COUNTRIES),
            rng.pick(&CITIES),
            "Voting Facility",
            id
        ))?;
    }

    let mut f = Csv::create(
        &snapshot,
        "Medium.csv",
        "mediumId|mediumType|isBlocked|createTime|lastLoginTime|riskLevel",
    )?;
    for id in 0..media {
        let t = START_MS + (rng.next() % WINDOW_MS as u64) as i64;
        f.row(&format!(
            "{}|{}|{}|{}|{}|{}",
            MEDIUM_BASE + id as u64,
            rng.pick(&MEDIUM_TYPE),
            rng.chance(0.08),
            stamp(t),
            t + 86_400_000,
            rng.pick(&RISK)
        ))?;
    }

    let mut f = Csv::create(
        &snapshot,
        "Account.csv",
        "accountId|createTime|isBlocked|accoutType|nickname|phonenum|email|freqLoginType|lastLoginTime|accountLevel",
    )?;
    let mut account_created = Vec::with_capacity(accounts);
    for id in 0..accounts {
        let t = START_MS + (rng.next() % (WINDOW_MS as u64 / 2)) as i64;
        account_created.push(t);
        f.row(&format!(
            "{}|{}|{}|{}|{} {}|{:03}-{:04}|{}|{}|{}|{}",
            ACCOUNT_BASE + id as u64,
            stamp(t),
            rng.chance(0.05),
            rng.pick(&ACCOUNT_TYPE),
            rng.pick(&FIRST),
            rng.pick(&LAST),
            rng.below(1000),
            rng.below(10000),
            "example.invalid",
            rng.pick(&LOGIN_TYPE),
            t + 172_800_000,
            rng.pick(&ACCOUNT_LEVEL)
        ))?;
    }

    let mut f = Csv::create(
        &snapshot,
        "Loan.csv",
        "loanId|loanAmount|balance|createTime|loanUsage|interestRate",
    )?;
    let mut loan_created = Vec::with_capacity(loans);
    for id in 0..loans {
        let t = START_MS + (rng.next() % WINDOW_MS as u64) as i64;
        loan_created.push(t);
        let amount = (1000.0 + rng.unit() * 9_000_000.0).round();
        f.row(&format!(
            "{}|{}|{}|{}|{}|{:.3}",
            LOAN_BASE + id as u64,
            amount,
            (amount * (0.2 + rng.unit() * 0.8)).round(),
            stamp(t),
            rng.pick(&LOAN_USAGE),
            0.01 + rng.unit() * 0.12
        ))?;
    }

    // ── ownership: every account belongs to someone ────────────────────────
    let person_owned = n(sf001::PERSON_OWN).min(accounts);
    let mut owner_of: Vec<(bool, usize)> = Vec::with_capacity(accounts);
    let mut f = Csv::create(
        &snapshot,
        "PersonOwnAccount.csv",
        "personId|accountId|createTime",
    )?;
    for (a, created) in account_created.iter().enumerate().take(person_owned) {
        let p = rng.below(persons);
        owner_of.push((true, p));
        f.row(&format!(
            "{}|{}|{}",
            p + 1,
            ACCOUNT_BASE + a as u64,
            stamp(*created)
        ))?;
    }
    let mut f = Csv::create(
        &snapshot,
        "CompanyOwnAccount.csv",
        "companyId|accountId|createTime",
    )?;
    for (a, created) in account_created.iter().enumerate().take(accounts).skip(person_owned) {
        let c = rng.below(companies);
        owner_of.push((false, c));
        f.row(&format!(
            "{}|{}|{}",
            COMPANY_BASE + c as u64,
            ACCOUNT_BASE + a as u64,
            stamp(*created)
        ))?;
    }

    // ── transfers: hubs, and deliberate cycles ─────────────────────────────
    //
    // The cycles are the point of several queries: money that leaves an
    // account and comes back through two or three others. Uniform random
    // edges almost never close one, so a share of the budget builds them.
    let transfers = n(sf001::TRANSFER);
    let mut f = Csv::create(
        &snapshot,
        "AccountTransferAccount.csv",
        "fromId|toId|amount|createTime|orderNum|comment|payType|goodsType",
    )?;
    let transfer_row = |rng: &mut Rng, from: usize, to: usize, t: i64| -> String {
        format!(
            "{}|{}|{:.2}|{}|{}|generated|{}|{}",
            ACCOUNT_BASE + from as u64,
            ACCOUNT_BASE + to as u64,
            10.0 + rng.unit() * 4_000_000.0,
            stamp(t),
            rng.next() % 1_000_000_000_000_000,
            rng.pick(&PAY_TYPE),
            rng.pick(&GOODS_TYPE)
        )
    };
    // The cycles come first and are kept as distinct pairs; the rest of the
    // budget is drawn with hub skew, then multiplicity is applied over the
    // whole set so the corpus carries repeat transfers at the measured rate.
    let cycle_budget = transfers / 8;
    let mut cycle_pairs: Vec<(usize, usize)> = Vec::with_capacity(cycle_budget);
    while cycle_pairs.len() < cycle_budget {
        let len = 3 + rng.below(2);
        let ring: Vec<usize> = (0..len)
            .map(|_| hubs.transfer_in[rng.skewed(accounts, 1.6)])
            .collect();
        for h in 0..len {
            let (a, b) = (ring[h], ring[(h + 1) % len]);
            if a != b && cycle_pairs.len() < cycle_budget {
                cycle_pairs.push((a, b));
            }
        }
    }
    let mut cycle_iter = cycle_pairs.into_iter();
    let pairs = pairs_with_multiplicity(&mut rng, transfers, sf001::TRANSFER_DISTINCT, |r| {
        match cycle_iter.next() {
            Some(p) => p,
            // LDBC'S TRANSFER GRAPH IS SYMMETRIC; THESE POWERS WERE NOT.
            //
            // 1.3 out against 2.2 in produced a corpus whose busiest RECEIVER
            // took 2,576 transfers while its busiest SENDER took 243. In
            // LDBC's official SF1 the two ends are within 2% of each other —
            // 597 out and 586 in — so the asymmetry was ours, and it put a
            // receiving hub four times heavier than any that exists in the
            // real corpus right where the hub-seeking queries look.
            //
            // Matched powers reproduce the symmetry and hold the body: p50 = 3,
            // p95 = 9 and mean = 4.2 agree with both official corpora.
            //
            // KNOWN RESIDUAL, left deliberately unfitted. The busiest account
            // is still light — 292/431 here against LDBC's 597/586 at SF1, and
            // 62/78 against 363/312 at SF0.01 — because LDBC's top hub is very
            // nearly scale-INVARIANT in absolute terms (363 -> 597 across a
            // 100x step), holding 4.5% of all transfers at SF0.01 and 0.073%
            // at SF1. No power law does that while keeping p50 = 3 and
            // mean = 4.2, so LDBC is evidently seeding a small set of
            // designated hubs rather than drawing a pure rank law.
            //
            // A two-point curve through those anchors would close the gap on
            // paper, and is exactly the mistake `WITHDRAW_SRC_POOL` documents:
            // there the model was accepted because a MECHANISM (uniform draws
            // over fixed-share pools) predicted both anchors to within one
            // edge, not because a fit passed through them. There is no such
            // mechanism here yet, so the gap is recorded rather than papered
            // over — hub-seeking queries meet a lighter top hub than the real
            // corpus has, and that is a stated limitation of this generator.
            None => (
                hubs.transfer_out[r.skewed(accounts, TRANSFER_SKEW)],
                hubs.transfer_in[r.skewed(accounts, TRANSFER_SKEW)],
            ),
        }
    });
    for ((from, to), k) in pairs {
        for t in ordered_times(&mut rng, k) {
            let line = transfer_row(&mut rng, from, to, t);
            f.row(&line)?;
        }
    }

    let withdraws = n(sf001::WITHDRAW);
    let mut f = Csv::create(
        &snapshot,
        "AccountWithdrawAccount.csv",
        "fromId|toId|amount|createTime",
    )?;
    // Uniform over two restricted pools — see `sf001::WITHDRAW_SRC_POOL` for
    // why this edge type alone is drawn this way and not skewed toward hubs.
    // The pools still come off the per-edge-type shuffles, so a withdrawal
    // endpoint is unrelated to a transfer hub; what changes is that the draw
    // inside the pool is flat and the duplicates are left to arise on their own.
    let src_pool = ((accounts as f64 * sf001::WITHDRAW_SRC_POOL).round() as usize).max(1);
    let dst_pool = ((accounts as f64 * sf001::WITHDRAW_DST_POOL).round() as usize).max(1);
    let mut counts: std::collections::HashMap<(usize, usize), u32> =
        std::collections::HashMap::new();
    for _ in 0..withdraws {
        // redraw rather than skip: the pools overlap, so dropping a self-edge
        // would quietly lose the row and undershoot the calibrated count
        let (from, to) = loop {
            let from = hubs.withdraw_out[(rng.next() % src_pool as u64) as usize];
            let to = hubs.withdraw_in[(rng.next() % dst_pool as u64) as usize];
            if from != to {
                break (from, to);
            }
        };
        *counts.entry((from, to)).or_insert(0) += 1;
    }
    // sorted, so a seed and a scale name exactly one corpus regardless of the
    // hash map's iteration order
    let mut pairs: Vec<((usize, usize), u32)> = counts.into_iter().collect();
    pairs.sort_unstable();
    for ((from, to), k) in pairs {
        for t in ordered_times(&mut rng, k as usize) {
            f.row(&format!(
                "{}|{}|{:.2}|{}",
                ACCOUNT_BASE + from as u64,
                ACCOUNT_BASE + to as u64,
                10.0 + rng.unit() * 2_000_000.0,
                stamp(t)
            ))?;
        }
    }

    // ── loans: applied for, deposited, then repaid from the account that
    //    received them, so the three edges describe one story ──────────────
    let mut f = Csv::create(
        &snapshot,
        "PersonApplyLoan.csv",
        "personId|loanId|createTime|org",
    )?;
    let person_apply = n(sf001::PERSON_APPLY).min(loans);
    for (l, created) in loan_created.iter().enumerate().take(person_apply) {
        f.row(&format!(
            "{}|{}|{}|{}",
            rng.below(persons) + 1,
            LOAN_BASE + l as u64,
            stamp(*created),
            rng.pick(&ORG)
        ))?;
    }
    let mut f = Csv::create(
        &snapshot,
        "CompanyApplyLoan.csv",
        "companyId|loanId|createTime|org",
    )?;
    for (l, created) in loan_created.iter().enumerate().take(loans).skip(person_apply) {
        f.row(&format!(
            "{}|{}|{}|{}",
            COMPANY_BASE + rng.below(companies) as u64,
            LOAN_BASE + l as u64,
            stamp(*created),
            rng.pick(&ORG)
        ))?;
    }

    // A loan is deposited into an account several times and repaid from it
    // several times: nearly half of both edge types are repeats in the real
    // corpus, and the queries that follow a loan through an account rely on it.
    let deposits = n(sf001::DEPOSIT);
    let mut f = Csv::create(
        &snapshot,
        "LoanDepositAccount.csv",
        "loanId|accountId|amount|createTime",
    )?;
    let mut seq = 0usize;
    let dep_pairs = pairs_with_multiplicity(&mut rng, deposits, sf001::DEPOSIT_DISTINCT, |r| {
        let l = seq % loans;
        seq += 1;
        (l, hubs.deposit_in[r.skewed(accounts, 1.1)])
    });
    let mut deposited: Vec<(usize, usize, i64)> = Vec::with_capacity(deposits);
    for ((l, a), k) in &dep_pairs {
        let anchor = loan_created[*l] + (rng.next() % 86_400_000) as i64;
        for t in ordered_times_after(&mut rng, *k, anchor) {
            deposited.push((*l, *a, t));
            f.row(&format!(
                "{}|{}|{:.2}|{}",
                LOAN_BASE + *l as u64,
                ACCOUNT_BASE + *a as u64,
                100.0 + rng.unit() * 2_000_000.0,
                stamp(t)
            ))?;
        }
    }

    let repays = n(sf001::REPAY);
    let mut f = Csv::create(
        &snapshot,
        "AccountRepayLoan.csv",
        "accountId|loanId|amount|createTime",
    )?;
    let mut seq = 0usize;
    let rep_pairs = pairs_with_multiplicity(&mut rng, repays, sf001::REPAY_DISTINCT, |_r| {
        // repay from an account that actually received the loan
        let (l, a, _) = deposited[seq % deposited.len()];
        seq += 1;
        (a, l)
    });
    // A MAP, NOT A SCAN. Finding each repayment's deposit by walking the
    // deposit list is O(deposits) per repayment: at SF10 that is 27.5 M
    // repayments over 27.6 M deposits, which is not slow, it is quadratic —
    // SF1 generated in eleven seconds and SF10 had not finished in ten
    // minutes.
    let mut deposit_at: std::collections::HashMap<(usize, usize), i64> =
        std::collections::HashMap::with_capacity(deposited.len());
    for (l, a, t) in &deposited {
        deposit_at.entry((*l, *a)).or_insert(*t);
    }
    for ((a, l), k) in rep_pairs {
        let after = deposit_at.get(&(l, a)).copied().unwrap_or(loan_created[l]);
        for t in ordered_times_after(&mut rng, k, after) {
            f.row(&format!(
                "{}|{}|{:.2}|{}",
                ACCOUNT_BASE + a as u64,
                LOAN_BASE + l as u64,
                50.0 + rng.unit() * 500_000.0,
                stamp(t)
            ))?;
        }
    }

    // ── guarantee chains, which several queries walk several hops of ───────
    let mut f = Csv::create(
        &snapshot,
        "PersonGuaranteePerson.csv",
        "fromId|toId|createTime|relation",
    )?;
    let pg = n(sf001::PERSON_GUARANTEE);
    let mut made = 0;
    while made < pg {
        let len = 2 + rng.below(4);
        let mut cur = rng.below(persons);
        for _ in 0..len {
            if made >= pg {
                break;
            }
            let next = rng.below(persons);
            if next == cur {
                continue;
            }
            f.row(&format!(
                "{}|{}|{}|{}",
                cur + 1,
                next + 1,
                stamp(START_MS + (rng.next() % WINDOW_MS as u64) as i64),
                rng.pick(&RELATION)
            ))?;
            cur = next;
            made += 1;
        }
    }
    let mut f = Csv::create(
        &snapshot,
        "CompanyGuaranteeCompany.csv",
        "fromId|toId|createTime|relation",
    )?;
    let cg = n(sf001::COMPANY_GUARANTEE);
    let mut made = 0;
    while made < cg {
        let len = 2 + rng.below(3);
        let mut cur = rng.below(companies);
        for _ in 0..len {
            if made >= cg {
                break;
            }
            let next = rng.below(companies);
            if next == cur {
                continue;
            }
            f.row(&format!(
                "{}|{}|{}|{}",
                COMPANY_BASE + cur as u64,
                COMPANY_BASE + next as u64,
                stamp(START_MS + (rng.next() % WINDOW_MS as u64) as i64),
                rng.pick(&RELATION)
            ))?;
            cur = next;
            made += 1;
        }
    }

    // ── investment ─────────────────────────────────────────────────────────
    let mut f = Csv::create(
        &snapshot,
        "PersonInvestCompany.csv",
        "investorId|companyId|ratio|createTime",
    )?;
    for _ in 0..n(sf001::PERSON_INVEST) {
        f.row(&format!(
            "{}|{}|{}|{}",
            rng.below(persons) + 1,
            COMPANY_BASE + rng.skewed(companies, 1.4) as u64,
            rng.unit(),
            stamp(START_MS + (rng.next() % WINDOW_MS as u64) as i64)
        ))?;
    }
    let mut f = Csv::create(
        &snapshot,
        "CompanyInvestCompany.csv",
        "investorId|companyId|ratio|createTime",
    )?;
    let company_invest = n(sf001::COMPANY_INVEST);
    let mut done = 0usize;
    while done < company_invest {
        let (a, b) = (rng.below(companies), rng.skewed(companies, 1.4));
        if a == b {
            continue;
        }
        done += 1;
        f.row(&format!(
            "{}|{}|{}|{}",
            COMPANY_BASE + a as u64,
            COMPANY_BASE + b as u64,
            rng.unit(),
            stamp(START_MS + (rng.next() % WINDOW_MS as u64) as i64)
        ))?;
    }

    // ── sign-ins: a few media reach many accounts, which is the shape the
    //    shared-device queries look for ──────────────────────────────────────
    let mut f = Csv::create(
        &snapshot,
        "MediumSignInAccount.csv",
        "mediumId|accountId|createTime|location",
    )?;
    let signins = n(sf001::SIGN_IN);
    let pairs = pairs_with_multiplicity(&mut rng, signins, sf001::SIGN_IN_DISTINCT, |r| {
        (r.skewed(media, 2.4), hubs.signin_account[r.below(accounts)])
    });
    for ((m, a), k) in pairs {
        for t in ordered_times(&mut rng, k) {
            f.row(&format!(
                "{}|{}|{}|{} -> {}",
                MEDIUM_BASE + m as u64,
                ACCOUNT_BASE + a as u64,
                stamp(t),
                rng.pick(&COUNTRIES),
                rng.pick(&CITIES)
            ))?;
        }
    }

    eprintln!("[fbgen] done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Rng, START_MS, WINDOW_MS, ordered_times, ordered_times_after, sf001};

    /// Expected DISTINCT ordered pairs when `rows` edges are thrown uniformly
    /// at a pool of `src * dst` slots — the standard occupancy result.
    fn birthday_distinct(src: f64, dst: f64, rows: f64) -> f64 {
        let slots = src * dst;
        slots * (1.0 - (-rows / slots).exp())
    }

    /// WITHDRAW'S DUPLICATES ARE THE BIRTHDAY EFFECT, AT BOTH OFFICIAL SCALES.
    ///
    /// This is the property a single-scale calibration could not see. LDBC's
    /// SF0.01 corpus has 4.4% of its withdrawals repeating an ordered pair and
    /// its SF1 corpus has 0.06% — not because the generator changes, but
    /// because the same uniform draw over a pool that grew 100x collides far
    /// less often. Holding the RATIO fixed (`8782/9182`, which is what this
    /// file used to do) reproduces SF0.01 perfectly and is wrong at every
    /// other scale.
    ///
    /// Asserting the formula against both official corpora pins the shape: if
    /// anyone reintroduces a fixed distinct-ratio for withdraw, the model this
    /// test describes and the code will have parted company.
    #[test]
    fn withdraw_duplicates_follow_the_birthday_curve_at_both_official_scales() {
        // (accounts, src pool, dst pool, rows, measured distinct pairs)
        let official = [
            (2055.0, 568.0, 181.0, 9182.0, 8782.0),
            (204_771.0, 56686.0, 15702.0, 907_482.0, 906_967.0),
        ];
        for (accounts, src, dst, rows, measured) in official {
            let predicted = birthday_distinct(src, dst, rows);
            let err = (predicted - measured).abs() / measured;
            assert!(
                err < 0.001,
                "uniform draws over {src}x{dst} should give {measured} distinct pairs \
                 from {rows} rows, formula says {predicted:.0} ({:.3}% off)",
                err * 100.0
            );

            // and the pools this generator would build for that many accounts
            // must be the pools the official corpus actually has
            let our_src = accounts * sf001::WITHDRAW_SRC_POOL;
            let our_dst = accounts * sf001::WITHDRAW_DST_POOL;
            assert!(
                (our_src - src).abs() / src < 0.02,
                "source pool at {accounts} accounts: ours {our_src:.0}, LDBC's {src}"
            );
            // the destination share drifts between the two official corpora
            // (8.81% at SF0.01 against 7.67% at SF1), so this is the looser of
            // the two bounds deliberately, and the SF1 value is the one taken
            assert!(
                (our_dst - dst).abs() / dst < 0.15,
                "destination pool at {accounts} accounts: ours {our_dst:.0}, LDBC's {dst}"
            );
        }
    }

    /// The four SKEW-driven edge types keep their multi-edge ratio across
    /// scales, which is why a ratio constant is the right model for them and
    /// not for withdraw. Measured on LDBC's official SF1.
    #[test]
    fn the_skewed_edge_types_keep_their_multi_edge_ratio_at_sf1() {
        // (name, our SF0.01 ratio, LDBC's SF1 ratio)
        let checks = [
            ("transfer", sf001::TRANSFER_DISTINCT, 614_043.0 / 794_180.0),
            ("deposit", sf001::DEPOSIT_DISTINCT, 149_855.0 / 272_417.0),
            ("repay", sf001::REPAY_DISTINCT, 146_312.0 / 263_972.0),
            ("sign-in", sf001::SIGN_IN_DISTINCT, 63_889.0 / 258_056.0),
        ];
        for (name, ours, sf1) in checks {
            assert!(
                (ours - sf1).abs() < 0.03,
                "{name}: SF0.01 distinct ratio {ours:.4} vs official SF1 {sf1:.4} — \
                 a skew-driven ratio should hold across scales"
            );
        }
    }

    /// EVERY EDGE MUST BE INSIDE THE WINDOW ITS QUERIES FILTER ON.
    ///
    /// Repeats used to advance a running timestamp by up to a month each, with
    /// nothing to bring them back, so a pair with many repeats walked out of
    /// the corpus: at SF10 the busiest account carried over a hundred transfers
    /// stamped past 2027 against a window ending in 2022. Every FinBench read
    /// filters on a time window, so those edges were invisible to the queries
    /// meant to traverse them — the benchmark was measuring its own filter.
    #[test]
    fn a_repeated_pair_stays_inside_the_window() {
        let end = START_MS + WINDOW_MS;
        let mut rng = Rng(12345);
        for k in [1usize, 2, 5, 40, 500] {
            let ts = ordered_times(&mut rng, k);
            assert_eq!(ts.len(), k.max(1));
            for (i, t) in ts.iter().enumerate() {
                assert!(
                    *t >= START_MS && *t <= end,
                    "k={k}, element {i} is {t}, outside [{START_MS}, {end}]"
                );
            }
            for w in ts.windows(2) {
                assert!(w[1] > w[0], "k={k}: repeats must be strictly later");
            }
        }
    }

    /// The causally anchored edges — a deposit follows its loan, a repayment
    /// follows its deposit — keep both properties: after the anchor, and still
    /// inside the window.
    #[test]
    fn an_anchored_repeat_is_after_its_cause_and_still_in_the_window() {
        let end = START_MS + WINDOW_MS;
        let mut rng = Rng(999);
        for anchor in [START_MS, START_MS + WINDOW_MS / 2, end - 1_000] {
            for k in [1usize, 3, 50] {
                let ts = ordered_times_after(&mut rng, k, anchor);
                for t in &ts {
                    assert!(*t > anchor, "anchor {anchor}, got {t}");
                    assert!(*t <= end, "anchor {anchor}, k={k}: {t} is past {end}");
                }
                for w in ts.windows(2) {
                    assert!(w[1] > w[0], "anchored repeats must be strictly later");
                }
            }
        }
    }

    /// An anchor with no room left cannot satisfy both rules; causal order is
    /// the one that must hold, and the overshoot stays milliseconds.
    #[test]
    fn an_anchor_past_the_window_steps_by_milliseconds_not_months() {
        let end = START_MS + WINDOW_MS;
        let mut rng = Rng(7);
        let ts = ordered_times_after(&mut rng, 10, end + 5);
        for w in ts.windows(2) {
            assert!(w[1] > w[0]);
        }
        let overshoot = ts[ts.len() - 1] - end;
        assert!(
            overshoot < 86_400_000,
            "overshoot {overshoot} ms is more than a day past the window"
        );
    }
}
