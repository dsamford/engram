#![allow(non_snake_case)]
//! The crypto seam: authentication, binding, determinism, and the marker gate.

use engram_crypto::{Sealer, Secret, derive_dek, open, open_bound};
use engram_key::Realm;

const MASTER: [u8; 32] = [7u8; 32];

fn sealer(realm: u32) -> Sealer {
    Sealer::new(derive_dek(&MASTER, Realm(realm)), 0)
}

#[test]
fn seal_open_round_trips() {
    let mut s = sealer(1);
    let env = s.seal(&Secret::new(b"the plaintext".to_vec()));
    let back = open(&derive_dek(&MASTER, Realm(1)), &env).expect("opens");
    assert_eq!(back.expose(), b"the plaintext");
}

#[test]
fn EVERY_bit_flip_is_refused() {
    // The AEAD property, verified across the whole envelope — version byte,
    // nonce, ciphertext, tag. A flip anywhere must fail closed.
    let mut s = sealer(1);
    let env = s.seal(&Secret::new(b"payload".to_vec()));
    let dek = derive_dek(&MASTER, Realm(1));
    for i in 0..env.len() {
        let mut tampered = env.clone();
        tampered[i] ^= 0x01;
        assert!(
            open(&dek, &tampered).is_err(),
            "a flip at byte {i} authenticated"
        );
    }
    // And truncations.
    for cut in 0..env.len() {
        assert!(
            open(&dek, &env[..cut]).is_err(),
            "a {cut}-byte truncation authenticated"
        );
    }
}

#[test]
fn realm_B_cannot_open_realm_A() {
    // Key separation from ONE master: the derivation, not a key table, is the
    // isolation. Same master, different realm, refusal.
    let mut a = sealer(1);
    let env = a.seal(&Secret::new(b"tenant a's row".to_vec()));
    assert!(open(&derive_dek(&MASTER, Realm(2)), &env).is_err());
}

#[test]
fn a_DIFFERENT_master_cannot_open_anything() {
    let mut s = sealer(1);
    let env = s.seal(&Secret::new(b"x".to_vec()));
    let other_master = [9u8; 32];
    assert!(open(&derive_dek(&other_master, Realm(1)), &env).is_err());
}

#[test]
fn rederiving_the_SAME_dek_still_opens() {
    // The durability half of derivation: the DEK is a pure function of
    // (master, realm), so a restart re-derives and reads its own data. Shred
    // is therefore exactly "rotate the master away", nothing subtler.
    let mut s = sealer(7);
    let env = s.seal(&Secret::new(b"durable".to_vec()));
    let _ = s;
    let again = derive_dek(&MASTER, Realm(7));
    assert_eq!(open(&again, &env).expect("opens").expose(), b"durable");
}

#[test]
fn sealing_is_DETERMINISTIC_under_counter_nonces() {
    // What lets the DST assert ciphertext bytes across runs of one seed.
    let mut a = sealer(1);
    let mut b = sealer(1);
    let e1 = a.seal(&Secret::new(b"same".to_vec()));
    let e2 = b.seal(&Secret::new(b"same".to_vec()));
    assert_eq!(e1, e2);
    // And the SECOND seal differs from the first — the nonce advanced.
    let e3 = a.seal(&Secret::new(b"same".to_vec()));
    assert_ne!(e1, e3, "a repeated nonce would forfeit the key");
    // Both still open.
    let dek = derive_dek(&MASTER, Realm(1));
    assert_eq!(open(&dek, &e3).expect("opens").expose(), b"same");
}

#[test]
fn Secret_debug_prints_no_content() {
    let s = Secret::new(b"hunter2".to_vec());
    let printed = format!("{s:?}");
    assert!(
        !printed.contains("hunter2"),
        "Debug leaked the plaintext: {printed}"
    );
    assert!(printed.contains("7 bytes"));
}

#[test]
fn THE_MARKER_GATE_no_sealed_surface_carries_plaintext() {
    // The plan's gate, with its mandatory violating control. A canary-tenant
    // marker is sealed and written through the protected path; EVERY byte
    // surface the store exposes — log payloads, log heads, scans — is then
    // searched for the marker. Zero hits required. Then the CONTROL: the same
    // marker written PLAIN through an unprotected path MUST be found, or the
    // scanner is dead and the zero above meant nothing.
    use engram_key::{KeyPrefix, Kind, Namespace, Partition};
    use engram_store::{Store, StoredValue};

    let marker: [u8; 16] = *b"MARKER-123456789";
    let store = Store::new();
    let mut s = sealer(42);
    let protected = KeyPrefix {
        realm: Realm(42),
        namespace: Namespace(1),
        kind: Kind::PROTECTED_PROPERTY,
        partition: Partition(1),
    };
    let env = s.seal(&Secret::new(marker.to_vec()));
    store
        .put(&protected, b"row", StoredValue::Sealed(env))
        .expect("sealed put");

    let contains = |hay: &[u8]| hay.windows(marker.len()).any(|w| w == marker);
    let mut surfaces: Vec<Vec<u8>> = Vec::new();
    for e in store.log_tail(0) {
        surfaces.push(e.payload.clone());
        surfaces.push(e.hash.to_vec());
    }
    for (body, value) in store.scan(&protected) {
        surfaces.push(body);
        surfaces.push(value);
    }
    for surf in &surfaces {
        assert!(
            !contains(surf),
            "the marker appeared in a stored byte surface"
        );
    }

    // ── The mandatory violating control ─────────────────────────────────
    let plain_prefix = KeyPrefix {
        realm: Realm(42),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    };
    store
        .put(&plain_prefix, b"row", StoredValue::Plain(marker.to_vec()))
        .expect("plain put");
    let found = store.log_tail(0).iter().any(|e| contains(&e.payload))
        || store.scan(&plain_prefix).iter().any(|(_, v)| contains(v));
    assert!(
        found,
        "the CONTROL failed: the scanner cannot find a marker it should — the zero above is unmeasured"
    );
}

#[test]
fn each_realm_derives_a_DISTINCT_key() {
    // Isolation is doubly enforced — derivation AND the AAD — so a canary
    // breaking the derivation alone was masked by the AAD refusing anyway.
    // Two guards covering one input is not two tested guards (the
    // KeyPrefix::decode lesson, in the crypto layer): the fingerprint makes
    // the derivation's own contribution directly observable.
    let a = derive_dek(&MASTER, Realm(1));
    let b = derive_dek(&MASTER, Realm(2));
    assert_ne!(
        a.fingerprint(),
        b.fingerprint(),
        "two realms derived the SAME key"
    );
    // Determinism: the same inputs re-derive the same key.
    assert_eq!(a.fingerprint(), derive_dek(&MASTER, Realm(1)).fingerprint());
    // And a different master moves every fingerprint.
    assert_ne!(
        a.fingerprint(),
        derive_dek(&[9u8; 32], Realm(1)).fingerprint()
    );
}

#[test]
fn a_binding_is_part_of_the_seal_and_the_empty_binding_is_plain_seal() {
    // seal() must be exactly seal_bound(&[]) — two envelope formats for one
    // version byte would be undecodable — and a binding mismatch must be the
    // same single refusal as every other tamper.
    let dek = derive_dek(&MASTER, Realm(1));
    let mut sealer = Sealer::new(derive_dek(&MASTER, Realm(1)), 0);
    let env = sealer.seal_bound(&Secret::new(b"chunk".to_vec()), b"pos-3-of-9");

    assert_eq!(
        open_bound(&dek, &env, b"pos-3-of-9")
            .expect("opens with its binding")
            .expose(),
        b"chunk"
    );
    assert!(
        open_bound(&dek, &env, b"pos-4-of-9").is_err(),
        "a moved binding refuses"
    );
    assert!(
        open(&dek, &env).is_err(),
        "and the UNBOUND open refuses a bound envelope"
    );

    let mut sealer2 = Sealer::new(derive_dek(&MASTER, Realm(1)), 100);
    let plain_env = sealer2.seal(&Secret::new(b"row".to_vec()));
    assert_eq!(
        open_bound(&dek, &plain_env, b"")
            .expect("empty binding IS the plain seal")
            .expose(),
        b"row"
    );
}
