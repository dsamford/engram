//! M2 / R3 — the overlay model, as enforcement rather than convention.
//!
//! D3 (the plan's cross-cutting decision, not the registration rule): system
//! corpora and tenant overlays are different `namespace` values under the same
//! physical keyspace. A cross-namespace edge is an ordinary edge whose
//! endpoints differ in one key component. Reads of the system namespace are
//! shared; writes to it are a separate, privileged capability. A tenant
//! overlay physically cannot contain another tenant's rows.
//!
//! # What is enforced WHERE
//!
//! - **"Cannot contain another tenant's rows" is the key prefix**, already
//!   frozen: a [`TenantSession`] is constructed over one realm and derives
//!   every key itself, so a foreign realm is not expressible through it.
//! - **"Writes to the system layer are held separately" is a capability
//!   OBJECT**, not a boolean. [`SystemWriteCap`] cannot be cloned, cannot be
//!   constructed outside this module, and is minted EXACTLY ONCE per registry
//!   — a second mint returns `None`. Holding it is the privilege; there is no
//!   flag to set and no role to claim.
//! - **Per-namespace ACLs live in the [`NamespaceRegistry`]** and every
//!   session operation consults them. An UNDECLARED namespace refuses reads
//!   and writes both — absence is a refusal, never a default-open.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_observe::sometimes;
use std::collections::BTreeMap;

use crate::{Store, StoreError, StoredValue};

/// What a namespace IS. The role decides who may write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceRole {
    /// A shared corpus (markets, news, CVEs): every tenant may read, only the
    /// holder of [`SystemWriteCap`] may write.
    System,
    /// One tenant's overlay: that tenant reads and writes, nobody else does.
    TenantOverlay(Realm),
}

/// The system-write privilege, as an object.
///
/// Not `Clone`, not `Copy`, no public constructor: the ONLY way to obtain one
/// is [`NamespaceRegistry::mint_system_write`], which succeeds once. "Held
/// separately" is then literal — there is one of these, and whoever holds it
/// is the system writer. A boolean or a role string would be a claim any call
/// site can make; an object is something a call site has to be GIVEN.
#[derive(Debug)]
pub struct SystemWriteCap(());

/// The namespace registry: roles, and the one-shot system capability.
#[derive(Debug, Default)]
pub struct NamespaceRegistry {
    roles: BTreeMap<u32, NamespaceRole>,
    system_cap_minted: bool,
}

/// Overlay-layer refusals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayError {
    /// The namespace is not declared. Absence refuses; it never defaults open.
    UndeclaredNamespace(u32),
    /// A tenant write to a system namespace without the capability.
    SystemWriteRequiresCap(u32),
    /// A read or write of another tenant's overlay.
    ForeignOverlay {
        /// The namespace refused.
        namespace: u32,
        /// The overlay's owner.
        owner: u32,
    },
    /// The underlying store refused (protected KIND, invalid kind…).
    Store(StoreError),
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlayError::UndeclaredNamespace(ns) => {
                write!(
                    f,
                    "namespace {ns} is not declared — absence refuses, it does not default open"
                )
            }
            OverlayError::SystemWriteRequiresCap(ns) => {
                write!(
                    f,
                    "namespace {ns} is a system corpus; writing it requires the SystemWriteCap"
                )
            }
            OverlayError::ForeignOverlay { namespace, owner } => {
                write!(f, "namespace {namespace} is tenant {owner}'s overlay")
            }
            OverlayError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OverlayError {}

impl NamespaceRegistry {
    /// An empty registry. Nothing is declared, so everything refuses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a namespace's role. Redeclaration is refused — flipping a
    /// system corpus into a tenant overlay (or the reverse) reclassifies every
    /// row already written under it, which is a migration, not a method call.
    pub fn declare(&mut self, ns: Namespace, role: NamespaceRole) -> Result<(), OverlayError> {
        if self.roles.contains_key(&ns.0) {
            return Err(OverlayError::UndeclaredNamespace(ns.0)); // reused: "cannot (re)declare"
        }
        self.roles.insert(ns.0, role);
        Ok(())
    }

    /// The role of a namespace, if declared.
    pub fn role(&self, ns: Namespace) -> Option<NamespaceRole> {
        self.roles.get(&ns.0).copied()
    }

    /// Mint the system-write capability. Succeeds EXACTLY once.
    ///
    /// The second caller gets `None`, which is the point: two holders is two
    /// writers of the shared corpus, and "who else can write this" stops being
    /// answerable. The embedder mints at boot and hands the object to the one
    /// component allowed to write the system layer.
    pub fn mint_system_write(&mut self) -> Option<SystemWriteCap> {
        if self.system_cap_minted {
            sometimes!("overlay.second system-cap mint refused", true);
            return None;
        }
        self.system_cap_minted = true;
        Some(SystemWriteCap(()))
    }
}

/// One tenant's scoped view of the store.
///
/// Constructed over ONE realm; every key is derived inside this type, so a
/// foreign realm is not expressible through it — the "physically cannot
/// contain another tenant's rows" half of the overlay model is this
/// constructor plus the frozen key prefix.
pub struct TenantSession<'a> {
    store: &'a Store,
    registry: &'a NamespaceRegistry,
    realm: Realm,
}

impl<'a> TenantSession<'a> {
    /// A session for `realm`.
    pub fn new(store: &'a Store, registry: &'a NamespaceRegistry, realm: Realm) -> Self {
        TenantSession {
            store,
            registry,
            realm,
        }
    }

    fn prefix(&self, ns: Namespace, kind: Kind, partition: Partition) -> KeyPrefix {
        KeyPrefix {
            realm: self.realm,
            namespace: ns,
            kind,
            partition,
        }
    }

    /// May this session READ `ns`? System corpora are shared; an overlay is
    /// readable by its owner only; undeclared refuses.
    fn check_read(&self, ns: Namespace) -> Result<(), OverlayError> {
        match self.registry.role(ns) {
            None => {
                sometimes!("overlay.undeclared namespace refused", true);
                Err(OverlayError::UndeclaredNamespace(ns.0))
            }
            Some(NamespaceRole::System) => Ok(()),
            Some(NamespaceRole::TenantOverlay(owner)) if owner == self.realm => Ok(()),
            Some(NamespaceRole::TenantOverlay(owner)) => {
                sometimes!("overlay.foreign overlay refused", true);
                Err(OverlayError::ForeignOverlay {
                    namespace: ns.0,
                    owner: owner.0,
                })
            }
        }
    }

    /// May this session WRITE `ns` (without the system capability)?
    fn check_write(&self, ns: Namespace) -> Result<(), OverlayError> {
        match self.registry.role(ns) {
            None => {
                sometimes!("overlay.undeclared namespace refused", true);
                Err(OverlayError::UndeclaredNamespace(ns.0))
            }
            Some(NamespaceRole::System) => {
                sometimes!("overlay.tenant write to system refused", true);
                Err(OverlayError::SystemWriteRequiresCap(ns.0))
            }
            Some(NamespaceRole::TenantOverlay(owner)) if owner == self.realm => Ok(()),
            Some(NamespaceRole::TenantOverlay(owner)) => {
                sometimes!("overlay.foreign overlay refused", true);
                Err(OverlayError::ForeignOverlay {
                    namespace: ns.0,
                    owner: owner.0,
                })
            }
        }
    }

    /// Read from any namespace this session may read.
    ///
    /// Note the SYSTEM read path: a system corpus is written under the SYSTEM
    /// REALM (realm 0 by convention of the writer), and shared reads resolve
    /// against that realm — one copy for everyone, cacheable across tenants,
    /// exactly the economy the overlay model exists for.
    pub fn get(
        &self,
        ns: Namespace,
        kind: Kind,
        partition: Partition,
        body: &[u8],
    ) -> Result<Option<Vec<u8>>, OverlayError> {
        self.check_read(ns)?;
        let realm = match self.registry.role(ns) {
            Some(NamespaceRole::System) => SYSTEM_REALM,
            _ => self.realm,
        };
        let p = KeyPrefix {
            realm,
            namespace: ns,
            kind,
            partition,
        };
        Ok(self.store.get(&p, body))
    }

    /// Write into this tenant's overlay.
    pub fn put(
        &self,
        ns: Namespace,
        kind: Kind,
        partition: Partition,
        body: &[u8],
        value: StoredValue,
    ) -> Result<u64, OverlayError> {
        self.check_write(ns)?;
        self.store
            .put(&self.prefix(ns, kind, partition), body, value)
            .map_err(OverlayError::Store)
    }

    /// Delete from this tenant's overlay.
    pub fn delete(
        &self,
        ns: Namespace,
        kind: Kind,
        partition: Partition,
        body: &[u8],
    ) -> Result<u64, OverlayError> {
        self.check_write(ns)?;
        Ok(self.store.delete(&self.prefix(ns, kind, partition), body))
    }
}

/// The realm system corpora are written under. A convention made a constant so
/// the writer and every shared read agree by construction.
pub const SYSTEM_REALM: Realm = Realm(0);

/// Where a system write lands: the namespace and key coordinates, grouped so
/// the write function stays under the argument lint and the coordinates read
/// as one thing — which they are.
#[derive(Debug, Clone, Copy)]
pub struct SystemTarget {
    /// The system namespace.
    pub ns: Namespace,
    /// The row's KIND.
    pub kind: Kind,
    /// The partition.
    pub partition: Partition,
}

/// Write into a SYSTEM namespace. Takes the capability BY REFERENCE — the
/// caller keeps it — but requiring it at all is the enforcement: a call site
/// that does not hold the object cannot make this call, and the object cannot
/// be manufactured.
pub fn system_put(
    store: &Store,
    registry: &NamespaceRegistry,
    _cap: &SystemWriteCap,
    target: SystemTarget,
    body: &[u8],
    value: StoredValue,
) -> Result<u64, OverlayError> {
    let SystemTarget {
        ns,
        kind,
        partition,
    } = target;
    match registry.role(ns) {
        Some(NamespaceRole::System) => {}
        None => return Err(OverlayError::UndeclaredNamespace(ns.0)),
        Some(NamespaceRole::TenantOverlay(owner)) => {
            // The capability is not a skeleton key: it writes SYSTEM corpora,
            // not tenant overlays. A system writer that could reach into an
            // overlay would make the capability a cross-tenant write token.
            return Err(OverlayError::ForeignOverlay {
                namespace: ns.0,
                owner: owner.0,
            });
        }
    }
    let p = KeyPrefix {
        realm: SYSTEM_REALM,
        namespace: ns,
        kind,
        partition,
    };
    store.put(&p, body, value).map_err(OverlayError::Store)
}
