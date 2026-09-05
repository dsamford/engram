//! M5 — the crypto seam: per-tenant DEKs, AEAD sealing, redaction discipline.
//!
//! # The DEK is DERIVED from the key prefix
//!
//! R6's requirement, enabled by L2: the tenant's realm is the first component
//! of every key, so `derive_dek(master, realm)` gives one key per tenant with
//! no key table to keep consistent — the keyspace layout IS the key
//! derivation. Destroying a realm's DEK (rotating the master away from it)
//! shreds that tenant from every copy of every byte sealed under it, which is
//! the shred-latency story the commit log's header/payload split set up.
//!
//! # Deterministic nonces, and why that is safe HERE
//!
//! GCM requires nonce uniqueness per key, not unpredictability. Nonces here
//! are a per-DEK monotonic counter — deterministic, so the DST reproduces
//! ciphertexts from a seed and byte-level assertions hold across runs. The
//! counter lives in the [`Sealer`]; two sealers on one DEK would repeat
//! nonces, so `Sealer` is not `Clone` and owns its DEK.
//!
//! # `Secret`: the redaction type
//!
//! Plaintext-in-flight wears [`Secret`], which has no `Display`, a Debug that
//! prints only a length, and an `expose()` that is the SINGLE deliberate exit.
//! The plan's fuller `Sealed<T>`/`LeakageClass` discipline arrives with the
//! operator work; what is non-retrofittable is that plaintext never travels as
//! bare bytes through APIs that format their arguments — that starts here.

#![forbid(unsafe_code)]

use engram_key::Realm;
use engram_observe::{Canary, Gate, Registration, Subsystem, counted, sometimes};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;

/// Envelope version byte, frozen with the format.
const ENVELOPE_V1: u8 = 1;

/// Nonce length for AES-256-GCM.
const NONCE_LEN: usize = 12;

/// Plaintext that must not leak through formatting.
pub struct Secret(Vec<u8>);

impl Secret {
    /// Wrap plaintext.
    pub fn new(bytes: Vec<u8>) -> Self {
        Secret(bytes)
    }

    /// The single deliberate exit. Named so a grep finds every place plaintext
    /// leaves the wrapper — an anonymous accessor is unauditable.
    pub fn expose(&self) -> &[u8] {
        counted!("crypto.secret exposed");
        &self.0
    }

    /// Length without exposure.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the secret is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The length only. Printing content here is the one-line patch that
        // ships plaintext to every log aggregator at once.
        write!(f, "<secret {} bytes>", self.0.len())
    }
}

/// A per-tenant data-encryption key. Not `Clone`, no byte accessor: the key
/// exists to be USED, and an API that hands the bytes out is a key escrow
/// nobody meant to build.
pub struct Dek {
    key: LessSafeKey,
    realm: Realm,
    /// SHA-256 of the derived key bytes — safe to log and compare, impossible
    /// to reverse. Exists for two reasons: rotation diagnostics ("which key
    /// generation is this pod holding"), and TESTABILITY of the derivation
    /// itself — cross-realm isolation is doubly enforced (derivation AND the
    /// AAD), so a canary against the derivation alone was masked by the AAD
    /// refusing anyway. The fingerprint makes per-realm distinctness directly
    /// observable without weakening either guard.
    fingerprint: [u8; 32],
}

impl Dek {
    /// The key's fingerprint. Loggable; never the key.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

/// Derive the DEK for a realm from the master key.
///
/// HKDF-SHA256 with the realm's big-endian bytes as info — the same bytes the
/// key encoding writes, so the derivation and the keyspace cannot disagree
/// about tenant identity.
pub fn derive_dek(master: &[u8; 32], realm: Realm) -> Dek {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"engram-dek-v1");
    let prk = salt.extract(master);
    let info = realm.0.to_be_bytes();
    let info_refs = [&info[..]];
    let okm = prk.expand(&info_refs, hkdf::HKDF_SHA256).expect("len fits");
    let mut key_bytes = [0u8; 32];
    okm.fill(&mut key_bytes).expect("32 bytes");
    let digest = ring::digest::digest(&ring::digest::SHA256, &key_bytes);
    let mut fingerprint = [0u8; 32];
    fingerprint.copy_from_slice(digest.as_ref());
    let unbound = UnboundKey::new(&AES_256_GCM, &key_bytes).expect("valid key length");
    Dek {
        key: LessSafeKey::new(unbound),
        realm,
        fingerprint,
    }
}

/// Seals plaintext under one DEK with counter nonces.
///
/// Owns the DEK and is not `Clone`: two sealers on one key would repeat
/// nonces, and a repeated GCM nonce forfeits the key.
pub struct Sealer {
    dek: Dek,
    next_nonce: u64,
}

/// Why an open failed. One variant on purpose: distinguishing "wrong key"
/// from "tampered" from "truncated" at this layer would hand an attacker an
/// oracle, and the caller's recovery is identical in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenError;

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ciphertext did not authenticate under this key")
    }
}

impl std::error::Error for OpenError {}

impl Sealer {
    /// A sealer over a derived DEK, nonces starting at `nonce_base`.
    ///
    /// The base is the CALLER's durability problem, stated here so it cannot
    /// be discovered later: on restart the sealer must resume PAST every nonce
    /// ever used under this DEK (the store's commit ts is the natural source —
    /// monotonic and already durable). Restarting at zero repeats nonces.
    pub fn new(dek: Dek, nonce_base: u64) -> Sealer {
        Sealer {
            dek,
            next_nonce: nonce_base,
        }
    }

    /// The realm this sealer's key belongs to.
    pub fn realm(&self) -> Realm {
        self.dek.realm
    }

    /// The next nonce — persist alongside whatever the envelope protects.
    pub fn next_nonce(&self) -> u64 {
        self.next_nonce
    }

    /// Seal plaintext into an envelope: `version:u8 | nonce:12 | ct+tag`.
    ///
    /// The realm rides in the ASSOCIATED DATA, not the envelope: a ciphertext
    /// moved to another tenant's rows fails authentication even under the
    /// correct global master, because the binding is part of the seal.
    pub fn seal(&mut self, plaintext: &Secret) -> Vec<u8> {
        self.seal_bound(plaintext, &[])
    }

    /// Seal with an extra CALLER binding in the associated data.
    ///
    /// AAD = `realm_be ‖ binding`. The binding is whatever position the
    /// ciphertext must be immovable from — a chunk's `header ‖ k ‖ n` in the
    /// blob layer, so a chunk spliced into another blob, another position, or
    /// a truncated count fails authentication (SSC1 v2's property, at zero
    /// ciphertext expansion). The binding is NOT stored: both sides must
    /// already know it, which is the point.
    pub fn seal_bound(&mut self, plaintext: &Secret, binding: &[u8]) -> Vec<u8> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes[4..].copy_from_slice(&self.next_nonce.to_be_bytes());
        self.next_nonce += 1;

        let mut buf = plaintext.expose().to_vec();
        let mut aad = Vec::with_capacity(4 + binding.len());
        aad.extend_from_slice(&self.dek.realm.0.to_be_bytes());
        aad.extend_from_slice(binding);
        self.dek
            .key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad),
                &mut buf,
            )
            .expect("sealing cannot fail for in-memory buffers");

        let mut out = Vec::with_capacity(1 + NONCE_LEN + buf.len());
        out.push(ENVELOPE_V1);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&buf);
        counted!("crypto.sealed");
        out
    }
}

/// Open an envelope under a DEK. Fails CLOSED: any tamper, truncation, wrong
/// key or wrong realm is the same refusal.
pub fn open(dek: &Dek, envelope: &[u8]) -> Result<Secret, OpenError> {
    open_bound(dek, envelope, &[])
}

/// Open an envelope sealed with [`Sealer::seal_bound`], supplying the same
/// binding. A wrong binding is the same single refusal as every other tamper.
pub fn open_bound(dek: &Dek, envelope: &[u8], binding: &[u8]) -> Result<Secret, OpenError> {
    if envelope.first() != Some(&ENVELOPE_V1) {
        sometimes!("crypto.open refused an envelope", true);
        return Err(OpenError);
    }
    let nonce_bytes: [u8; NONCE_LEN] = envelope
        .get(1..1 + NONCE_LEN)
        .ok_or(OpenError)?
        .try_into()
        .map_err(|_| OpenError)?;
    let mut buf = envelope.get(1 + NONCE_LEN..).ok_or(OpenError)?.to_vec();
    let mut aad = Vec::with_capacity(4 + binding.len());
    aad.extend_from_slice(&dek.realm.0.to_be_bytes());
    aad.extend_from_slice(binding);
    let plain_len = dek
        .key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(aad),
            &mut buf,
        )
        .map_err(|_| {
            sometimes!("crypto.open refused an envelope", true);
            OpenError
        })?
        .len();
    buf.truncate(plain_len);
    counted!("crypto.opened");
    Ok(Secret::new(buf))
}

// ─── D3 registration ────────────────────────────────────────────────────────

/// The crypto seam, as a registered subsystem.
pub struct CryptoSeam;

impl Subsystem for CryptoSeam {
    const NAME: &'static str = "crypto";

    fn register() -> Registration {
        Registration::new()
            .crash_point("crypto.between_seal_and_write")
            .sometimes("crypto.open refused an envelope")
            .counter("crypto.sealed")
            .counter("crypto.opened")
            .counter("crypto.secret exposed")
            .gate(
                Gate::new(
                    "no sealed byte surface carries plaintext",
                    Canary::new("write a marker value through a plain put and assert the marker scan still reports zero"),
                )
                .and_canary(Canary::new("make seal a copy and assert the marker is absent from the log")),
            )
            .gate(
                Gate::new(
                    "a tenant's DEK opens only that tenant's envelopes",
                    Canary::new("derive realm B's DEK and open realm A's envelope"),
                )
                .and_canary(Canary::new("move a sealed envelope onto another realm's rows and open it there")),
            )
    }
}
