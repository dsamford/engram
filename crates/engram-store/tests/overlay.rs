#![allow(non_snake_case)]
//! M2 — the overlay model's refusals, and the one economy it exists for.

use engram_key::{Kind, Namespace, Partition, Realm};
use engram_store::overlay::SystemTarget;
use engram_store::{
    NamespaceRegistry, NamespaceRole, OverlayError, Store, StoredValue, TenantSession, system_put,
};

const SYS_NS: Namespace = Namespace(1);
const T_A: Realm = Realm(10);
const T_B: Realm = Realm(20);
const NS_A: Namespace = Namespace(100);
const NS_B: Namespace = Namespace(200);
const PART: Partition = Partition(1);

fn registry() -> NamespaceRegistry {
    let mut r = NamespaceRegistry::new();
    r.declare(SYS_NS, NamespaceRole::System).expect("sys");
    r.declare(NS_A, NamespaceRole::TenantOverlay(T_A))
        .expect("a");
    r.declare(NS_B, NamespaceRole::TenantOverlay(T_B))
        .expect("b");
    r
}

#[test]
fn a_tenant_reads_and_writes_ITS_OWN_overlay() {
    let store = Store::new();
    let reg = registry();
    let a = TenantSession::new(&store, &reg, T_A);
    a.put(NS_A, Kind::NODE, PART, b"k", StoredValue::Plain(vec![1]))
        .expect("write");
    assert_eq!(
        a.get(NS_A, Kind::NODE, PART, b"k").expect("read"),
        Some(vec![1])
    );
}

#[test]
fn a_FOREIGN_overlay_refuses_reads_AND_writes() {
    // Both directions, because they fail differently when broken: a foreign
    // read is a disclosure, a foreign write is corruption wearing a tenant's
    // name. The refusal must name the owner so the error is diagnosable
    // without a second query.
    let store = Store::new();
    let reg = registry();
    let a = TenantSession::new(&store, &reg, T_A);
    let b = TenantSession::new(&store, &reg, T_B);
    b.put(NS_B, Kind::NODE, PART, b"k", StoredValue::Plain(vec![9]))
        .expect("b writes b");

    assert_eq!(
        a.get(NS_B, Kind::NODE, PART, b"k"),
        Err(OverlayError::ForeignOverlay {
            namespace: 200,
            owner: 20
        }),
    );
    assert_eq!(
        a.put(NS_B, Kind::NODE, PART, b"k", StoredValue::Plain(vec![0])),
        Err(OverlayError::ForeignOverlay {
            namespace: 200,
            owner: 20
        }),
    );
    // …and b's row is untouched by the refused write.
    assert_eq!(
        b.get(NS_B, Kind::NODE, PART, b"k").expect("read"),
        Some(vec![9])
    );
}

#[test]
fn overlays_cannot_collide_even_on_identical_keys() {
    // The "physically cannot contain another tenant's rows" half: same
    // namespace number is impossible (each overlay declares its own), but even
    // with every OTHER component identical, the realm prefix separates them.
    let store = Store::new();
    let reg = registry();
    let a = TenantSession::new(&store, &reg, T_A);
    let b = TenantSession::new(&store, &reg, T_B);
    a.put(NS_A, Kind::NODE, PART, b"same", StoredValue::Plain(vec![1]))
        .expect("a");
    b.put(NS_B, Kind::NODE, PART, b"same", StoredValue::Plain(vec![2]))
        .expect("b");
    assert_eq!(
        a.get(NS_A, Kind::NODE, PART, b"same").expect("a"),
        Some(vec![1])
    );
    assert_eq!(
        b.get(NS_B, Kind::NODE, PART, b"same").expect("b"),
        Some(vec![2])
    );
}

#[test]
fn an_UNDECLARED_namespace_refuses_everything() {
    // Absence is a refusal. Defaulting open would make a typo'd namespace id a
    // fresh, ungoverned corpus that nothing ever declared — invisible to every
    // audit that walks the registry.
    let store = Store::new();
    let reg = registry();
    let a = TenantSession::new(&store, &reg, T_A);
    assert_eq!(
        a.get(Namespace(999), Kind::NODE, PART, b"k"),
        Err(OverlayError::UndeclaredNamespace(999))
    );
    assert_eq!(
        a.put(
            Namespace(999),
            Kind::NODE,
            PART,
            b"k",
            StoredValue::Plain(vec![1])
        ),
        Err(OverlayError::UndeclaredNamespace(999)),
    );
}

// ─── The system layer ───────────────────────────────────────────────────────

#[test]
fn the_system_corpus_is_READ_SHARED_and_write_refused() {
    let store = Store::new();
    let mut reg = registry();
    let cap = reg.mint_system_write().expect("first mint");

    system_put(
        &store,
        &reg,
        &cap,
        SystemTarget {
            ns: SYS_NS,
            kind: Kind::NODE,
            partition: PART,
        },
        b"cve-1",
        StoredValue::Plain(vec![7]),
    )
    .expect("system write");

    // EVERY tenant reads the SAME row — one copy, the economy the model
    // exists for.
    let a = TenantSession::new(&store, &reg, T_A);
    let b = TenantSession::new(&store, &reg, T_B);
    assert_eq!(
        a.get(SYS_NS, Kind::NODE, PART, b"cve-1").expect("a reads"),
        Some(vec![7])
    );
    assert_eq!(
        b.get(SYS_NS, Kind::NODE, PART, b"cve-1").expect("b reads"),
        Some(vec![7])
    );

    // …and neither can write it.
    assert_eq!(
        a.put(
            SYS_NS,
            Kind::NODE,
            PART,
            b"cve-1",
            StoredValue::Plain(vec![0])
        ),
        Err(OverlayError::SystemWriteRequiresCap(1)),
    );
    // The refused write really did not land.
    assert_eq!(
        b.get(SYS_NS, Kind::NODE, PART, b"cve-1")
            .expect("b re-reads"),
        Some(vec![7])
    );
}

#[test]
fn the_system_capability_mints_EXACTLY_once() {
    // Two holders is two writers of the shared corpus, and "who else can write
    // this" stops being answerable. The second mint is refused, not logged.
    let mut reg = registry();
    assert!(reg.mint_system_write().is_some());
    assert!(
        reg.mint_system_write().is_none(),
        "a second system capability was minted"
    );
}

#[test]
fn the_capability_is_not_a_skeleton_key() {
    // It writes SYSTEM corpora. Pointed at a tenant overlay it must refuse —
    // otherwise the capability is a cross-tenant write token with a nicer name.
    let store = Store::new();
    let mut reg = registry();
    let cap = reg.mint_system_write().expect("mint");
    assert_eq!(
        system_put(
            &store,
            &reg,
            &cap,
            SystemTarget {
                ns: NS_A,
                kind: Kind::NODE,
                partition: PART
            },
            b"k",
            StoredValue::Plain(vec![1])
        ),
        Err(OverlayError::ForeignOverlay {
            namespace: 100,
            owner: 10
        }),
    );
}

#[test]
fn redeclaring_a_namespace_is_refused() {
    // Flipping a system corpus into an overlay reclassifies every row already
    // written under it — a migration, not a method call.
    let mut reg = registry();
    assert!(
        reg.declare(SYS_NS, NamespaceRole::TenantOverlay(T_A))
            .is_err()
    );
    assert_eq!(
        reg.role(SYS_NS),
        Some(NamespaceRole::System),
        "the original role must survive"
    );
}

#[test]
fn a_cross_namespace_EDGE_is_an_ordinary_row() {
    // D3's payoff: an edge from a tenant node to a system node is a row in the
    // TENANT's overlay whose body names the system endpoint. Nothing special
    // anywhere — which is the whole point, and this pins that the key layer
    // expresses it today so L5's adjacency inherits it.
    let store = Store::new();
    let mut reg = registry();
    let cap = reg.mint_system_write().expect("mint");
    system_put(
        &store,
        &reg,
        &cap,
        SystemTarget {
            ns: SYS_NS,
            kind: Kind::NODE,
            partition: PART,
        },
        b"cve-42",
        StoredValue::Plain(vec![1]),
    )
    .expect("system node");

    let a = TenantSession::new(&store, &reg, T_A);
    // The edge body: (peer namespace, peer id) — structural components only.
    let mut edge_body = b"my-doc->".to_vec();
    edge_body.extend_from_slice(&SYS_NS.0.to_be_bytes());
    edge_body.extend_from_slice(b"cve-42");
    a.put(
        NS_A,
        Kind::EDGE,
        PART,
        &edge_body,
        StoredValue::Plain(vec![]),
    )
    .expect("edge");

    // The edge lives in A's overlay; B cannot see it; the endpoint resolves
    // through the shared read path.
    assert!(
        a.get(NS_A, Kind::EDGE, PART, &edge_body)
            .expect("a sees its edge")
            .is_some()
    );
    let b = TenantSession::new(&store, &reg, T_B);
    assert!(matches!(
        b.get(NS_A, Kind::EDGE, PART, &edge_body),
        Err(OverlayError::ForeignOverlay { .. })
    ));
    assert_eq!(
        a.get(SYS_NS, Kind::NODE, PART, b"cve-42")
            .expect("endpoint"),
        Some(vec![1])
    );
}
