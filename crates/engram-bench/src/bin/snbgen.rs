//! LDBC Social Network Benchmark data generator — deterministic, dependency
//! free, and emitting the same `nodes.jsonl` / `rels.jsonl` / `meta.json`
//! corpus format `engram_bench::load_export` reads and the port harness
//! compares. `snbgen <out_dir> <persons> [seed]` writes a faithful SNB-schema
//! social graph sized by the person count (the scale knob), with SNB-like
//! power-law fan-outs on KNOWS / HAS_MEMBER / LIKES / reply threads.
//!
//! Determinism is the whole point (it is a benchmark input loaded into Engram
//! AND Neo4j for a head-to-head): a SplitMix64 stream seeded by `seed` drives
//! every choice, so `(persons, seed)` reproduces the graph byte for byte. No
//! wall clock, no system RNG — dates are computed from a base epoch by
//! civil-from-days, ids are dense per label.
//!
//! Schema (labels / rel types) matches the LDBC SNB Interactive+BI reference so
//! the standard query set runs unmodified. Cardinality character per rel type
//! is documented at its generator.

use std::fmt::Write as _;
use std::io::Write as _;

// ── deterministic PRNG ──────────────────────────────────────────────────────

/// SplitMix64 — the same generator the HNSW build uses, so the whole engine
/// shares one deterministic stream family.
struct Rng {
    s: u64,
}
impl Rng {
    fn new(seed: u64) -> Self {
        Rng {
            s: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }
    fn next(&mut self) -> u64 {
        self.s = self.s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `[0, n)`.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next() % n
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// A power-law-ish out-degree in `[min, max]`: most nodes near `min`, a
    /// heavy tail toward `max` — the shape KNOWS / LIKES / membership have.
    fn power_degree(&mut self, min: u64, max: u64, alpha: f64) -> u64 {
        if max <= min {
            return min;
        }
        let u = self.unit().max(1e-12);
        let span = (max - min) as f64;
        let d = span * u.powf(alpha); // alpha>1 skews toward min
        min + d as u64
    }
}

// ── civil date from a day offset (Howard Hinnant's algorithm) ───────────────

/// `days` since 1970-01-01 → (year, month, day). No external calendar dep.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// An SNB `creationDate` as an ISO-8601 datetime string, from an epoch-seconds
/// value. SNB spans ~2010-2013; callers pass a base plus a deterministic delta.
fn iso_dt(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000+0000")
}

/// A deterministic body of `n` bytes - realistic message text so decoding it
/// is a real column cost (LDBC content runs to ~2000 chars). Late projection
/// is only worth measuring when the projected column is actually expensive.
fn filler(seed: u64, n: usize) -> String {
    const W: &[&str] = &[
        "the", "graph", "query", "friend", "message", "forum", "reply", "tag", "person", "post",
        "comment", "network", "social", "path", "join", "scan",
    ];
    let mut out = String::with_capacity(n + 8);
    let mut z = seed | 1;
    while out.len() < n {
        z = z
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push_str(W[(z >> 33) as usize % W.len()]);
        out.push(' ');
    }
    out.truncate(n);
    out
}

// SNB creation window: 2010-01-01 .. 2013-01-01 (seconds since epoch).
const T0: i64 = 1_262_304_000; // 2010-01-01
const T1: i64 = 1_356_998_400; // 2013-01-01

// ── JSONL emission ──────────────────────────────────────────────────────────

fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A property value, emitted in the tagged corpus format: ints as `~bigint`
/// (SNB ids exceed 2^53), datetimes/dates as `~dt`/`~d`, strings/plain as-is.
enum P {
    Int(i64),
    Str(String),
}

struct Node {
    id: String,
    labels: &'static [&'static str],
    props: Vec<(&'static str, P)>,
}
struct Rel {
    s: String,
    d: String,
    t: &'static str,
    props: Vec<(&'static str, P)>,
}

fn write_props(props: &[(&'static str, P)], out: &mut String) {
    out.push_str(",\"p\":{");
    for (i, (k, v)) in props.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        esc(k, out);
        out.push(':');
        match v {
            P::Int(n) => {
                out.push_str("{\"~bigint\":");
                let mut s = String::new();
                let _ = write!(s, "{n}");
                esc(&s, out);
                out.push('}');
            }
            P::Str(s) => esc(s, out),
        }
    }
    out.push('}');
}

fn emit_node(n: &Node, out: &mut String) {
    out.push_str("{\"i\":");
    esc(&n.id, out);
    out.push_str(",\"l\":[");
    for (i, l) in n.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        esc(l, out);
    }
    out.push(']');
    write_props(&n.props, out);
    out.push_str("}\n");
}

fn emit_rel(r: &Rel, out: &mut String) {
    out.push_str("{\"s\":");
    esc(&r.s, out);
    out.push_str(",\"d\":");
    esc(&r.d, out);
    out.push_str(",\"t\":");
    esc(r.t, out);
    write_props(&r.props, out);
    out.push_str("}\n");
}

/// A buffered JSONL sink that flushes every ~4 MB so multi-million-line files
/// never hold the whole corpus in memory.
struct Sink {
    f: std::io::BufWriter<std::fs::File>,
    buf: String,
    lines: u64,
}
impl Sink {
    fn create(path: &std::path::Path) -> Self {
        Sink {
            f: std::io::BufWriter::new(std::fs::File::create(path).expect("create jsonl")),
            buf: String::with_capacity(1 << 22),
            lines: 0,
        }
    }
    fn node(&mut self, n: &Node) {
        emit_node(n, &mut self.buf);
        self.lines += 1;
        self.maybe_flush();
    }
    fn rel(&mut self, r: &Rel) {
        emit_rel(r, &mut self.buf);
        self.lines += 1;
        self.maybe_flush();
    }
    fn maybe_flush(&mut self) {
        if self.buf.len() >= (1 << 22) {
            self.f.write_all(self.buf.as_bytes()).expect("write");
            self.buf.clear();
        }
    }
    fn finish(mut self) -> u64 {
        self.f.write_all(self.buf.as_bytes()).expect("write");
        self.f.flush().expect("flush");
        self.lines
    }
}

// ── reference data (small fixed vocabularies) ───────────────────────────────

const FIRST: &[&str] = &[
    "Ahmed", "Ana", "Bo", "Carlos", "Chen", "Deepa", "Elena", "Fatima", "Giulia", "Hans", "Ivan",
    "Jing", "Kwame", "Lena", "Mateo", "Nadia", "Omar", "Priya", "Qi", "Rosa", "Sven", "Tara",
    "Umar", "Vera", "Wei", "Xu", "Yara", "Zoe",
];
const LAST: &[&str] = &[
    "Andersson",
    "Bauer",
    "Costa",
    "Dubois",
    "Evans",
    "Ferrari",
    "Garcia",
    "Hansen",
    "Ivanov",
    "Jensen",
    "Kim",
    "Lopez",
    "Muller",
    "Nguyen",
    "Okafor",
    "Petrov",
    "Rossi",
    "Silva",
    "Tanaka",
    "Ueno",
    "Vidal",
    "Wang",
    "Yilmaz",
    "Zhang",
];
const BROWSERS: &[&str] = &["Chrome", "Firefox", "Safari", "InternetExplorer", "Opera"];
const CONTINENTS: &[&str] = &[
    "Africa",
    "Asia",
    "Europe",
    "North_America",
    "South_America",
    "Oceania",
];
const TAGCLASSES: &[&str] = &[
    "Thing",
    "Person",
    "Organisation",
    "Place",
    "Event",
    "Work",
    "Agent",
    "Award",
    "Country",
    "Company",
    "Album",
    "Film",
    "Sport",
    "Science",
    "Music",
    "Politics",
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: snbgen <out_dir> <persons> [seed]");
        std::process::exit(2);
    }
    let out = std::path::PathBuf::from(&args[1]);
    let persons: u64 = args[2].parse().expect("persons");
    let seed: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(42);
    std::fs::create_dir_all(&out).expect("mkdir out");
    let mut rng = Rng::new(seed);

    // Derived sizes — SNB-like proportions, scaled by the person count.
    let n_countries: u64 = 40.min(persons.max(4));
    let n_cities: u64 = (persons / 20).clamp(8, 4_000);
    let n_tags: u64 = (persons / 4).clamp(16, 16_000);
    let n_univ: u64 = (persons / 25).clamp(2, 6_000);
    let n_company: u64 = (persons / 40).clamp(2, 1_500);
    let n_forums: u64 = persons + persons / 8; // a wall per person + group forums

    let mut nodes = Sink::create(&out.join("nodes.jsonl"));
    let mut rels = Sink::create(&out.join("rels.jsonl"));

    eprintln!(
        "[snbgen] persons={persons} seed={seed} cities={n_cities} tags={n_tags} forums={n_forums}"
    );

    // ── Places: continents → countries → cities ────────────────────────────
    for (i, c) in CONTINENTS.iter().enumerate() {
        nodes.node(&Node {
            id: format!("cont:{i}"),
            labels: &["Place", "Continent"],
            props: vec![
                ("id", P::Int(i as i64)),
                ("name", P::Str((*c).to_string())),
                ("url", P::Str(format!("http://dbpedia.org/resource/{c}"))),
                ("type", P::Str("continent".into())),
            ],
        });
    }
    for i in 0..n_countries {
        let cont = rng.below(CONTINENTS.len() as u64);
        nodes.node(&Node {
            id: format!("country:{i}"),
            labels: &["Place", "Country"],
            props: vec![
                ("id", P::Int(i as i64)),
                ("name", P::Str(format!("Country{i}"))),
                (
                    "url",
                    P::Str(format!("http://dbpedia.org/resource/Country{i}")),
                ),
                ("type", P::Str("country".into())),
            ],
        });
        rels.rel(&Rel {
            s: format!("country:{i}"),
            d: format!("cont:{cont}"),
            t: "IS_PART_OF",
            props: vec![],
        });
    }
    for i in 0..n_cities {
        let country = rng.below(n_countries);
        nodes.node(&Node {
            id: format!("city:{i}"),
            labels: &["Place", "City"],
            props: vec![
                ("id", P::Int(i as i64)),
                ("name", P::Str(format!("City{i}"))),
                (
                    "url",
                    P::Str(format!("http://dbpedia.org/resource/City{i}")),
                ),
                ("type", P::Str("city".into())),
            ],
        });
        rels.rel(&Rel {
            s: format!("city:{i}"),
            d: format!("country:{country}"),
            t: "IS_PART_OF",
            props: vec![],
        });
    }

    // ── TagClasses (subclass tree) and Tags ────────────────────────────────
    for (i, tc) in TAGCLASSES.iter().enumerate() {
        nodes.node(&Node {
            id: format!("tc:{i}"),
            labels: &["TagClass"],
            props: vec![
                ("id", P::Int(i as i64)),
                ("name", P::Str((*tc).to_string())),
                ("url", P::Str(format!("http://dbpedia.org/ontology/{tc}"))),
            ],
        });
        if i > 0 {
            rels.rel(&Rel {
                s: format!("tc:{i}"),
                d: format!("tc:{}", rng.below(i as u64)),
                t: "IS_SUBCLASS_OF",
                props: vec![],
            });
        }
    }
    for i in 0..n_tags {
        let tc = rng.below(TAGCLASSES.len() as u64);
        nodes.node(&Node {
            id: format!("tag:{i}"),
            labels: &["Tag"],
            props: vec![
                ("id", P::Int(i as i64)),
                ("name", P::Str(format!("Tag{i}"))),
                ("url", P::Str(format!("http://dbpedia.org/resource/Tag{i}"))),
            ],
        });
        rels.rel(&Rel {
            s: format!("tag:{i}"),
            d: format!("tc:{tc}"),
            t: "HAS_TYPE",
            props: vec![],
        });
    }

    // ── Organisations ──────────────────────────────────────────────────────
    for i in 0..n_univ {
        nodes.node(&Node {
            id: format!("univ:{i}"),
            labels: &["Organisation", "University"],
            props: vec![
                ("id", P::Int(i as i64)),
                ("name", P::Str(format!("University{i}"))),
                (
                    "url",
                    P::Str(format!("http://dbpedia.org/resource/University{i}")),
                ),
                ("type", P::Str("university".into())),
            ],
        });
        rels.rel(&Rel {
            s: format!("univ:{i}"),
            d: format!("city:{}", rng.below(n_cities)),
            t: "IS_LOCATED_IN",
            props: vec![],
        });
    }
    for i in 0..n_company {
        nodes.node(&Node {
            id: format!("company:{i}"),
            labels: &["Organisation", "Company"],
            props: vec![
                ("id", P::Int(i as i64)),
                ("name", P::Str(format!("Company{i}"))),
                (
                    "url",
                    P::Str(format!("http://dbpedia.org/resource/Company{i}")),
                ),
                ("type", P::Str("company".into())),
            ],
        });
        rels.rel(&Rel {
            s: format!("company:{i}"),
            d: format!("country:{}", rng.below(n_countries)),
            t: "IS_LOCATED_IN",
            props: vec![],
        });
    }

    // ── Persons + located-in / study / work / interests ────────────────────
    for i in 0..persons {
        let created = T0 + (rng.below((T1 - T0) as u64) as i64);
        let bday = 315_532_800 + rng.below(631_152_000) as i64; // 1980..2000
        let fname = FIRST[(rng.below(FIRST.len() as u64)) as usize];
        let lname = LAST[(rng.below(LAST.len() as u64)) as usize];
        nodes.node(&Node {
            id: format!("p:{i}"),
            labels: &["Person"],
            props: vec![
                ("id", P::Int(i as i64)),
                ("firstName", P::Str(fname.to_string())),
                ("lastName", P::Str(lname.to_string())),
                (
                    "gender",
                    P::Str(if rng.below(2) == 0 { "male" } else { "female" }.into()),
                ),
                ("birthday", P::Int(bday * 1000)),
                ("creationDate", P::Int(created * 1000)),
                (
                    "locationIP",
                    P::Str(format!(
                        "10.{}.{}.{}",
                        rng.below(256),
                        rng.below(256),
                        rng.below(256)
                    )),
                ),
                (
                    "browserUsed",
                    P::Str(BROWSERS[(rng.below(BROWSERS.len() as u64)) as usize].to_string()),
                ),
                ("email", P::Str(format!("{fname}.{lname}.{i}@example.com"))),
            ],
        });
        rels.rel(&Rel {
            s: format!("p:{i}"),
            d: format!("city:{}", rng.below(n_cities)),
            t: "IS_LOCATED_IN",
            props: vec![],
        });
        // STUDY_AT one university (most people), WORK_AT 0-2 companies.
        if rng.below(10) < 7 {
            rels.rel(&Rel {
                s: format!("p:{i}"),
                d: format!("univ:{}", rng.below(n_univ)),
                t: "STUDY_AT",
                props: vec![("classYear", P::Int(2000 + rng.below(15) as i64))],
            });
        }
        for _ in 0..rng.below(3) {
            rels.rel(&Rel {
                s: format!("p:{i}"),
                d: format!("company:{}", rng.below(n_company)),
                t: "WORK_AT",
                props: vec![("workFrom", P::Int(2005 + rng.below(15) as i64))],
            });
        }
        // HAS_INTEREST: 2-10 tags (power-law).
        let n_int = rng.power_degree(2, 10, 1.4);
        for _ in 0..n_int {
            rels.rel(&Rel {
                s: format!("p:{i}"),
                d: format!("tag:{}", rng.below(n_tags)),
                t: "HAS_INTEREST",
                props: vec![],
            });
        }
        if (i + 1) % 200_000 == 0 {
            eprintln!("[snbgen] persons: {}", i + 1);
        }
    }

    // ── KNOWS: undirected friendship, power-law degree, emitted both ways ──
    // Many-to-many, the join that dominates SNB. Neighbours are drawn near the
    // person's id (a locality band) plus a few long-range links — the small-
    // world shape SNB uses.
    for i in 0..persons {
        let deg = rng.power_degree(3, 60.min(persons.max(3)), 1.6);
        for _ in 0..deg {
            let j = if rng.below(4) == 0 {
                rng.below(persons) // long-range
            } else {
                // local band of +-500
                let lo = i.saturating_sub(500);
                let hi = (i + 500).min(persons - 1);
                lo + rng.below((hi - lo).max(1) + 1)
            };
            if j == i {
                continue;
            }
            let created = T0 + rng.below((T1 - T0) as u64) as i64;
            rels.rel(&Rel {
                s: format!("p:{i}"),
                d: format!("p:{j}"),
                t: "KNOWS",
                props: vec![("creationDate", P::Int(created * 1000))],
            });
        }
    }

    // ── Forums (wall per person + group forums), members, moderators ───────
    let mut msg_id: u64 = 0;
    for f in 0..n_forums {
        let created = T0 + rng.below((T1 - T0) as u64) as i64;
        let moderator = rng.below(persons);
        let is_wall = f < persons;
        nodes.node(&Node {
            id: format!("f:{f}"),
            labels: &["Forum"],
            props: vec![
                ("id", P::Int(f as i64)),
                (
                    "title",
                    P::Str(if is_wall {
                        format!("Wall of Person {f}")
                    } else {
                        format!("Group Forum {f}")
                    }),
                ),
                ("creationDate", P::Int(created * 1000)),
            ],
        });
        rels.rel(&Rel {
            s: format!("f:{f}"),
            d: format!("p:{moderator}"),
            t: "HAS_MODERATOR",
            props: vec![],
        });
        // A few tags per forum.
        for _ in 0..rng.power_degree(1, 5, 1.3) {
            rels.rel(&Rel {
                s: format!("f:{f}"),
                d: format!("tag:{}", rng.below(n_tags)),
                t: "HAS_TAG",
                props: vec![],
            });
        }
        // Members (power-law).
        let n_mem = rng.power_degree(2, 40.min(persons.max(2)), 1.6);
        let mut members: Vec<u64> = Vec::with_capacity(n_mem as usize);
        for _ in 0..n_mem {
            let m = rng.below(persons);
            let joined = created + rng.below((T1 - created).max(1) as u64) as i64;
            rels.rel(&Rel {
                s: format!("f:{f}"),
                d: format!("p:{m}"),
                t: "HAS_MEMBER",
                props: vec![("joinDate", P::Int(joined * 1000))],
            });
            members.push(m);
        }
        if members.is_empty() {
            members.push(moderator);
        }
        // Posts in this forum, by members.
        let n_posts = rng.power_degree(0, 20, 1.5);
        for _ in 0..n_posts {
            let author = members[(rng.below(members.len() as u64)) as usize];
            let pcreated = created + rng.below((T1 - created).max(1) as u64) as i64;
            let plen = 20 + rng.below(2000);
            let pid = format!("m:{msg_id}");
            nodes.node(&Node {
                id: pid.clone(),
                labels: &["Message", "Post"],
                props: vec![
                    ("id", P::Int(msg_id as i64)),
                    ("creationDate", P::Int(pcreated * 1000)),
                    (
                        "locationIP",
                        P::Str(format!(
                            "10.{}.{}.{}",
                            rng.below(256),
                            rng.below(256),
                            rng.below(256)
                        )),
                    ),
                    (
                        "browserUsed",
                        P::Str(BROWSERS[(rng.below(BROWSERS.len() as u64)) as usize].to_string()),
                    ),
                    (
                        "language",
                        P::Str(if rng.below(2) == 0 { "en" } else { "de" }.into()),
                    ),
                    ("content", P::Str(filler(msg_id ^ 0xF00D, plen as usize))),
                    ("length", P::Int(plen as i64)),
                ],
            });
            rels.rel(&Rel {
                s: format!("f:{f}"),
                d: pid.clone(),
                t: "CONTAINER_OF",
                props: vec![],
            });
            rels.rel(&Rel {
                s: pid.clone(),
                d: format!("p:{author}"),
                t: "HAS_CREATOR",
                props: vec![],
            });
            rels.rel(&Rel {
                s: pid.clone(),
                d: format!("country:{}", rng.below(n_countries)),
                t: "IS_LOCATED_IN",
                props: vec![],
            });
            for _ in 0..rng.power_degree(1, 6, 1.3) {
                rels.rel(&Rel {
                    s: pid.clone(),
                    d: format!("tag:{}", rng.below(n_tags)),
                    t: "HAS_TAG",
                    props: vec![],
                });
            }
            let post_id = msg_id;
            msg_id += 1;
            // Reply thread: comments reply to the post or to earlier comments.
            let mut thread: Vec<u64> = vec![post_id];
            let n_comments = rng.power_degree(0, 12, 1.4);
            for _ in 0..n_comments {
                let commenter = members[(rng.below(members.len() as u64)) as usize];
                let parent = thread[(rng.below(thread.len() as u64)) as usize];
                let ccreated = pcreated + rng.below((T1 - pcreated).max(1) as u64) as i64;
                let cid = format!("m:{msg_id}");
                let clen = 10 + rng.below(500);
                nodes.node(&Node {
                    id: cid.clone(),
                    labels: &["Message", "Comment"],
                    props: vec![
                        ("id", P::Int(msg_id as i64)),
                        ("creationDate", P::Int(ccreated * 1000)),
                        (
                            "locationIP",
                            P::Str(format!(
                                "10.{}.{}.{}",
                                rng.below(256),
                                rng.below(256),
                                rng.below(256)
                            )),
                        ),
                        (
                            "browserUsed",
                            P::Str(
                                BROWSERS[(rng.below(BROWSERS.len() as u64)) as usize].to_string(),
                            ),
                        ),
                        ("content", P::Str(filler(msg_id ^ 0xC0DE, clen as usize))),
                        ("length", P::Int(clen as i64)),
                    ],
                });
                rels.rel(&Rel {
                    s: cid.clone(),
                    d: format!("m:{parent}"),
                    t: "REPLY_OF",
                    props: vec![],
                });
                rels.rel(&Rel {
                    s: cid.clone(),
                    d: format!("p:{commenter}"),
                    t: "HAS_CREATOR",
                    props: vec![],
                });
                for _ in 0..rng.power_degree(0, 3, 1.3) {
                    rels.rel(&Rel {
                        s: cid.clone(),
                        d: format!("tag:{}", rng.below(n_tags)),
                        t: "HAS_TAG",
                        props: vec![],
                    });
                }
                thread.push(msg_id);
                msg_id += 1;
            }
        }
        if (f + 1) % 100_000 == 0 {
            eprintln!("[snbgen] forums: {} (messages so far {msg_id})", f + 1);
        }
    }

    // ── LIKES: persons like messages, power-law ────────────────────────────
    if msg_id > 0 {
        for i in 0..persons {
            let n_likes = rng.power_degree(0, 30, 1.7);
            for _ in 0..n_likes {
                let m = rng.below(msg_id);
                let created = T0 + rng.below((T1 - T0) as u64) as i64;
                rels.rel(&Rel {
                    s: format!("p:{i}"),
                    d: format!("m:{m}"),
                    t: "LIKES",
                    props: vec![("creationDate", P::Int(created * 1000))],
                });
            }
        }
    }

    let n_nodes = nodes.finish();
    let n_rels = rels.finish();

    // meta.json — the load sets wall-ms from captured_at; use T1 (end of window).
    let meta = format!(
        "{{\"captured_at\":{{\"~dt\":\"{}\"}},\"generator\":\"snbgen\",\"persons\":{persons},\"seed\":{seed},\"messages\":{msg_id}}}",
        iso_dt(T1)
    );
    std::fs::write(out.join("meta.json"), meta).expect("write meta");
    eprintln!(
        "[snbgen] DONE nodes={n_nodes} rels={n_rels} messages={msg_id} -> {}",
        out.display()
    );
}
