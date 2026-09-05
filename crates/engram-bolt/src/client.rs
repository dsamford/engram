//! A minimal Bolt v5 client — the counterpart to [`server::BoltServer`](crate::server),
//! built on the same [`packstream`](crate::packstream) codec. It exists for the
//! benchmark harnesses (the concurrency-ceiling measurement drives K of these
//! at once) and for end-to-end socket tests that exercise the server exactly as
//! a real driver would, without pulling in an external driver crate — which
//! would break the pure-Rust c-deps gate.
//!
//! It speaks the subset the harnesses need: handshake → HELLO → LOGON, then
//! RUN + PULL. It is deliberately synchronous and one-statement-at-a-time; the
//! concurrency harness models load with many CLIENTS, not pipelining within one.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

use engram_cypher::Value;

use crate::packstream::{Decoder, Pack, decode_value, encode_struct};

const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];
const MSG_HELLO: u8 = 0x01;
const MSG_RUN: u8 = 0x10;
const MSG_PULL: u8 = 0x3F;
const MSG_LOGON: u8 = 0x6A;
const MSG_RESET: u8 = 0x0F;
const MSG_SUCCESS: u8 = 0x70;
const MSG_RECORD: u8 = 0x71;
const MSG_IGNORED: u8 = 0x7E;
const MSG_FAILURE: u8 = 0x7F;

/// A synchronous Bolt 5.x client connection, held at Ready.
pub struct Client {
    s: TcpStream,
    /// The `server` agent the peer announced in HELLO's SUCCESS.
    server_agent: String,
}

impl Client {
    /// Connect, negotiate a Bolt 5.x version, and reach Ready (HELLO + LOGON).
    pub fn connect<A: ToSocketAddrs>(addr: A) -> std::io::Result<Client> {
        let mut s = TcpStream::connect(addr)?;
        s.set_nodelay(true).ok();
        // MAGIC, then four version proposals as [pad, range, minor, major].
        //
        // A RANGE — 5.8 down to 5.0 — not the single 5.8 this used to send.
        // An exact proposal only connects to a server that speaks precisely
        // that minor: it worked against this project's own server, which
        // happens to answer 5.8, and failed the handshake outright against a
        // real Neo4j, whose highest 5.x minor is lower. A client that can only
        // talk to its own server cannot be used for a comparison, which is the
        // one job it has here.
        //
        // This is also what the real driver sends, so the negotiation this
        // exercises is the negotiation a driver performs.
        let mut hs = Vec::with_capacity(20);
        hs.extend_from_slice(&MAGIC);
        hs.extend_from_slice(&[0, 8, 8, 5]);
        hs.extend_from_slice(&[0u8; 12]);
        s.write_all(&hs)?;
        let mut ver = [0u8; 4];
        s.read_exact(&mut ver)?;
        if ver == [0, 0, 0, 0] {
            return Err(err("server declined every proposed Bolt version"));
        }
        let mut c = Client {
            s,
            server_agent: String::new(),
        };
        // HELLO carries `bolt_agent` as well as `user_agent`.
        //
        // Bolt 5.3 made `bolt_agent` MANDATORY, and it is a map rather than a
        // string. This client sent only `user_agent`, which its own server
        // accepted — servers are permissive about extras they do not read —
        // and a real Neo4j 5.26 refused outright. A client that only ever
        // spoke to the server it ships with had no way to discover that.
        let mut hello = std::collections::BTreeMap::new();
        hello.insert(
            "user_agent".to_string(),
            Value::Str("engram-client/1".to_string()),
        );
        hello.insert(
            "bolt_agent".to_string(),
            strmap(&[
                ("product", "engram-client/1"),
                ("platform", std::env::consts::OS),
                ("language", "Rust"),
                ("language_details", "engram-bolt"),
            ]),
        );
        c.write_msg(MSG_HELLO, &[Pack::Value(Value::Map(hello))])?;
        let meta = c.expect_success("HELLO")?;
        if let Some(Value::Str(agent)) = meta.get("server") {
            c.server_agent = agent.clone();
        }
        c.write_msg(MSG_LOGON, &[Pack::Value(strmap(&[("scheme", "none")]))])?;
        c.expect_success("LOGON")?;
        Ok(c)
    }

    /// The `server` agent string the peer announced in its HELLO SUCCESS
    /// (`Neo4j/5.26.0`, `engram/0.1.0`), or empty when it announced none.
    ///
    /// A loader that has to speak differently to different servers — a
    /// procedure one of them has and the other refuses — decides on this
    /// rather than on a flag someone has to remember to pass. The client
    /// used to throw the metadata away, which left no way to tell them apart.
    pub fn server_agent(&self) -> &str {
        &self.server_agent
    }

    /// Run a query, counting result rows and discarding their contents — the
    /// throughput-harness path. The RECORD is still fully decoded off the wire,
    /// so the protocol is exercised; only the per-row `Value` is not retained.
    pub fn run(&mut self, cypher: &str) -> std::io::Result<u64> {
        let mut rows = 0u64;
        self.exec(cypher, |_| rows += 1)?;
        Ok(rows)
    }

    /// Run a query and collect each result row as a `Value` — a list of the
    /// returned columns. The correctness/test path.
    pub fn query(&mut self, cypher: &str) -> std::io::Result<Vec<Value>> {
        let mut rows = Vec::new();
        self.exec(cypher, |mut fields| {
            if !fields.is_empty() {
                if let Ok(v) = decode_value(fields.remove(0)) {
                    rows.push(v);
                }
            }
        })?;
        Ok(rows)
    }

    /// RUN + PULL, invoking `on_record` with each RECORD's fields, until both
    /// the RUN and the PULL summaries have arrived. A FAILURE clears the stream
    /// (RESET) so the connection stays reusable, then surfaces as an error.
    fn exec(&mut self, cypher: &str, mut on_record: impl FnMut(Vec<Pack>)) -> std::io::Result<()> {
        self.write_msg(
            MSG_RUN,
            &[
                Pack::Value(Value::Str(cypher.to_string())),
                Pack::Value(Value::Map(Default::default())),
                Pack::Value(Value::Map(Default::default())),
            ],
        )?;
        self.write_msg(MSG_PULL, &[Pack::Value(intmap("n", -1))])?;
        let mut summaries = 0; // the RUN's, then the PULL's
        while summaries < 2 {
            let (tag, fields) = self.read_msg()?;
            match tag {
                MSG_RECORD => on_record(fields),
                MSG_SUCCESS => summaries += 1,
                MSG_FAILURE => {
                    let detail = fields
                        .into_iter()
                        .next()
                        .and_then(|p| decode_value(p).ok())
                        .map(|v| format!("{v:?}"))
                        .unwrap_or_default();
                    self.write_msg(MSG_RESET, &[])?;
                    self.expect_success("RESET")?;
                    return Err(err(&format!("query failed: {detail}")));
                }
                other => return Err(err(&format!("unexpected message 0x{other:02X} mid-stream"))),
            }
        }
        Ok(())
    }

    fn write_msg(&mut self, tag: u8, fields: &[Pack]) -> std::io::Result<()> {
        let mut payload = Vec::new();
        encode_struct(tag, fields, &mut payload).map_err(|e| err(&format!("encode: {e:?}")))?;
        let mut framed = Vec::with_capacity(payload.len() + 8);
        for chunk in payload.chunks(0xFFFF) {
            framed.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
            framed.extend_from_slice(chunk);
        }
        framed.extend_from_slice(&[0, 0]); // the message terminator
        self.s.write_all(&framed)
    }

    /// Read one chunked message and decode it to `(tag, fields)`.
    fn read_msg(&mut self) -> std::io::Result<(u8, Vec<Pack>)> {
        let mut payload = Vec::new();
        loop {
            let mut len = [0u8; 2];
            self.s.read_exact(&mut len)?;
            let n = u16::from_be_bytes(len) as usize;
            if n == 0 {
                break;
            }
            let start = payload.len();
            payload.resize(start + n, 0);
            self.s.read_exact(&mut payload[start..])?;
        }
        match Decoder::new(&payload)
            .decode()
            .map_err(|e| err(&format!("decode: {e:?}")))?
        {
            Pack::Struct { tag, fields } => Ok((tag, fields)),
            Pack::Value(_) | Pack::Bytes(_) => {
                Err(err("expected a message struct, got a bare value"))
            }
        }
    }

    /// Wait for a SUCCESS and return its metadata map (empty when the summary
    /// carried none or something other than a map).
    fn expect_success(
        &mut self,
        what: &str,
    ) -> std::io::Result<std::collections::BTreeMap<String, Value>> {
        let (tag, fields) = loop {
            let (tag, fields) = self.read_msg()?;
            // IGNORED answers a message that was queued behind a FAILURE —
            // for this client, the PULL pipelined after a RUN that failed.
            // Drain them; the summary we are waiting for follows.
            if tag != MSG_IGNORED {
                break (tag, fields);
            }
        };
        match tag {
            MSG_SUCCESS => Ok(fields
                .into_iter()
                .next()
                .and_then(|p| decode_value(p).ok())
                .and_then(|v| match v {
                    Value::Map(m) => Some(m),
                    _ => None,
                })
                .unwrap_or_default()),
            // Carry the server's OWN words. This used to report only
            // "<what> was refused", which turns every protocol disagreement
            // into a guessing game — the failure that motivated this said
            // exactly which HELLO field was missing, and the client threw it
            // away before anyone could read it.
            MSG_FAILURE => {
                let detail = fields
                    .first()
                    .and_then(|p| p.clone().into_value().ok())
                    .and_then(|v| match v {
                        Value::Map(m) => {
                            let get = |k: &str| match m.get(k) {
                                Some(Value::Str(s)) => Some(s.clone()),
                                _ => None,
                            };
                            match (get("code"), get("message")) {
                                (Some(c), Some(msg)) => Some(format!("{c}: {msg}")),
                                (None, Some(msg)) => Some(msg),
                                (Some(c), None) => Some(c),
                                _ => None,
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| "no detail supplied".to_string());
                Err(err(&format!("{what} was refused — {detail}")))
            }
            other => Err(err(&format!("{what}: unexpected message 0x{other:02X}"))),
        }
    }
}

fn strmap(kv: &[(&str, &str)]) -> Value {
    Value::Map(
        kv.iter()
            .map(|(k, v)| (k.to_string(), Value::Str(v.to_string())))
            .collect(),
    )
}
fn intmap(k: &str, v: i64) -> Value {
    let mut m = std::collections::BTreeMap::new();
    m.insert(k.to_string(), Value::Int(v));
    Value::Map(m)
}
fn err(m: &str) -> std::io::Error {
    std::io::Error::other(m.to_string())
}
