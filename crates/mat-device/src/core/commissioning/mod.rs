//! Device-side commissioning server: `ClusterHandler` implementations for
//! General Commissioning (spec §11.10, cluster 0x0030) and Node Operational
//! Credentials (spec §11.17, cluster 0x003E) — the responder half of
//! `mat_controller::commissioning`'s commissioner-side step machine
//! (`run_credential_steps`/`commission_on_network`).
//!
//! Pure — no tokio, no sockets, no files (checked by `cargo check -p
//! mat-device --no-default-features`). Fabric persistence goes through
//! `core::fabric_store::FabricStore`'s `FabricPersist` trait boundary; the
//! only concrete (file-backed) implementation lives under `net::store`.
//!
//! ## Structure: one state, three `ClusterHandler`s
//!
//! `Node::add_endpoint` takes `Vec<Box<dyn ClusterHandler>>` — each boxed
//! handler is singly owned and answers exactly one `cluster_id()`. But
//! General Commissioning, Node Operational Credentials, and Administrator
//! Commissioning commands share state (the fail-safe timer, the
//! CSR/AddTrustedRoot staged between commands, the fabric table, the open
//! commissioning window), so `CommissioningServer` itself is *not* a
//! `ClusterHandler` — it holds an `Arc<Mutex<Inner>>` and
//! `into_cluster_handlers` splits it into three thin adapters
//! (`GeneralCommissioningHandler`/`OperationalCredentialsHandler`/
//! `AdminCommissioningHandler`) that all lock the same `Inner` and
//! delegate. `Arc<Mutex<..>>` rather than `Rc<RefCell<..>>` so the handlers
//! stay `Send` for a future async IM driver (mirrors `Node`'s eventual home
//! behind `tokio::sync::Mutex` or similar) even though nothing here awaits.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
use mat_controller::commissioning::{
    CLUSTER_ADMIN_COMMISSIONING, CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS,
};
#[cfg(test)]
use mat_controller::im;
use mat_controller::sync::locked;
use mat_controller::x509::DevAttestation;

use crate::core::access_control::AclStore;
use crate::core::datamodel::ClusterHandler;
#[cfg(test)]
use crate::core::datamodel::{InvokeCtx, InvokeReply};
use crate::core::fabric_store::{FabricEntry, FabricStore};
use crate::core::group_key_management::GroupKeyStore;
use crate::core::group_membership::GroupMembershipStore;

mod admin_commissioning;
mod attestation;
mod general_commissioning;
mod handlers;
mod noc;
#[cfg(test)]
mod test_util;

use handlers::{
    AdminCommissioningHandler, GeneralCommissioningHandler, OperationalCredentialsHandler,
};

/// Response command ids (spec §11.10.6 / §11.17.6 — the comments next to
/// each `CMD_*` request const in `mat_controller::commissioning` record
/// these; there's no `pub` home for them there since only the commissioner
/// side has needed them as decode-only targets until now).
const RESP_ARM_FAIL_SAFE: u32 = 0x01;

const RESP_SET_REGULATORY_CONFIG: u32 = 0x03;

const RESP_COMMISSIONING_COMPLETE: u32 = 0x05;

const RESP_ATTESTATION: u32 = 0x01;

const RESP_CERT_CHAIN: u32 = 0x03;

const RESP_CSR: u32 = 0x05;

const RESP_NOC: u32 = 0x08;

/// NodeOperationalCertStatusEnum (spec §11.17.6.13.2) values this server
/// actually returns. Not exhaustive — only the outcomes `handle_add_noc`
/// distinguishes.
const NOC_STATUS_OK: u8 = 0;

const NOC_STATUS_INVALID_PUBLIC_KEY: u8 = 1;

const NOC_STATUS_INVALID_NOC: u8 = 3;

/// Used both for its literal spec meaning (no `CSRRequest` was ever served,
/// so there is no pending operational key to install) and, loosely, for
/// "no `AddTrustedRootCertificate` yet" — the closest enum member available
/// for "the prerequisite staging step never happened".
const NOC_STATUS_MISSING_CSR: u8 = 4;

/// Approximation for "fabric persistence failed" — the enum has no
/// "storage write error" member; `TableFull` is the closest existing
/// meaning ("could not add this fabric").
const NOC_STATUS_TABLE_FULL: u8 = 5;

/// `UpdateFabricLabel`/`RemoveFabric`'s "no such fabric on this device"
/// outcome (spec §11.17.6.14.1, `InvalidFabricIndex`). Returned by both
/// `handle_update_fabric_label` and `handle_remove_fabric`.
const NOC_STATUS_INVALID_FABRIC_INDEX: u8 = 0x0A;

/// `NodeOperationalCertStatusEnum::InvalidAdminSubject` (spec §11.17.5.9):
/// `AddNOC.CaseAdminSubject` が operational node id でも CAT でもない
/// （`access_control::subject_kind` が `None`）。
const NOC_STATUS_INVALID_ADMIN_SUBJECT: u8 = 0x0B;

/// General Commissioning (0x0030) attribute ids this server serves (spec
/// §11.10.5). This cluster has no `CurrentFabricIndex` — that attribute is
/// Operational Credentials' (see the `ATTR_OC_*` consts below).
const ATTR_GC_BREADCRUMB: u32 = 0;

const ATTR_GC_BASIC_COMMISSIONING_INFO: u32 = 1;

const ATTR_GC_REGULATORY_CONFIG: u32 = 2;

const ATTR_GC_LOCATION_CAPABILITY: u32 = 3;

const ATTR_GC_SUPPORTS_CONCURRENT_CONNECTION: u32 = 4;

/// Node Operational Credentials (0x003E) attribute ids this server serves
/// (spec §11.17.5). `CurrentFabricIndex(5)` was deliberately absent through
/// Task 4 — it needs the session's fabric index, which the old `read`
/// signature didn't carry; Task 5's `ReadCtx` adds it (see
/// `read_operational_credentials`).
const ATTR_OC_NOCS: u32 = 0;

const ATTR_OC_FABRICS: u32 = 1;

const ATTR_OC_SUPPORTED_FABRICS: u32 = 2;

const ATTR_OC_COMMISSIONED_FABRICS: u32 = 3;

const ATTR_OC_TRUSTED_ROOT_CERTIFICATES: u32 = 4;

const ATTR_OC_CURRENT_FABRIC_INDEX: u32 = 5;

/// How many fabrics this device can hold (spec §11.17.5.2,
/// `SupportedFabrics`; the spec's own floor is 5). Single source of truth
/// for both the attribute's value and `handle_add_noc`'s capacity check —
/// an `AddNOC` that installed a sixth fabric would contradict the number
/// the device just reported.
const SUPPORTED_FABRICS: u8 = 5;

/// Administrator Commissioning (0x003C) attribute ids this server serves
/// (spec §11.19.5). `WindowStatus` is 0 (closed) or 1 (`EnhancedWindowOpen`
/// — this server only ever opens an ECM window, never the legacy basic-
/// commissioning-method window 2); `AdminFabricIndex`/`AdminVendorID` are
/// `null` while closed.
const ATTR_AC_WINDOW_STATUS: u32 = 0;

const ATTR_AC_ADMIN_FABRIC_INDEX: u32 = 1;

const ATTR_AC_ADMIN_VENDOR_ID: u32 = 2;

/// AdministratorCommissioning `StatusCode` (spec §11.19.6) values this
/// server returns via `InvokeReply::ClusterStatus`. `Success`(0) isn't
/// listed — that's the plain `InvokeReply::Status(im::STATUS_SUCCESS)` path.
const AC_STATUS_BUSY: u8 = 2;

const AC_STATUS_PAKE_PARAMETER_ERROR: u8 = 3;

const AC_STATUS_WINDOW_NOT_OPEN: u8 = 4;

/// `BasicCommissioningInfo` (spec §11.10.5.2) fields: the single-attempt
/// fail-safe expiry and the cumulative budget across an entire commissioning
/// session (mat-device doesn't vary either — one fixed pair for all
/// attempts). Named consts rather than inlined at the one `read_general_
/// commissioning` call site because Task 7's `ArmFailSafe` rollback timing
/// references the same values and must not drift from what
/// `BasicCommissioningInfo` advertises.
const FAIL_SAFE_EXPIRY_LENGTH_SECONDS: u16 = 60;

const FAIL_SAFE_MAX_CUMULATIVE_SECONDS: u16 = 900;

/// Fail-safe timer (spec §11.10.1). `Instant`-based — armed until a wall
/// point in the future; `is_armed` is `false` once that point passes or the
/// timer was never armed / was explicitly disarmed.
#[derive(Debug, Default)]
struct FailSafeState {
    armed_until: Option<Instant>,
}

impl FailSafeState {
    fn arm(&mut self, expiry_s: u16) {
        self.armed_until = Some(Instant::now() + Duration::from_secs(u64::from(expiry_s)));
    }

    fn disarm(&mut self) {
        self.armed_until = None;
    }

    /// The instant this window closes, if it's still open right now —
    /// `None` both when never armed and once the window has already
    /// passed. `CommissioningServer::fail_safe_deadline` hands this
    /// straight to the runtime's `select`, which only ever wants a live
    /// deadline to wait on.
    fn deadline(&self) -> Option<Instant> {
        self.armed_until.filter(|&t| Instant::now() < t)
    }

    fn is_armed(&self) -> bool {
        self.deadline().is_some()
    }

    /// `true` if this was armed and the window has now passed. Distinct
    /// from `!is_armed()`, which is also true when never armed at all —
    /// `expire_fail_safe` needs to tell "nothing to expire" from "there was
    /// something, and it's due" apart. Fires exactly once per window: the
    /// `disarm()` that follows a `true` result clears `armed_until`, so a
    /// repeat call reads back as "never armed" and returns `false`.
    fn is_expired(&self) -> bool {
        self.armed_until.is_some_and(|t| Instant::now() >= t)
    }

    /// Test-only: pushes the window into the past without waiting on a
    /// real clock, so fail-safe-expiry tests don't need a wall-clock sleep.
    /// A no-op if never armed.
    #[cfg(test)]
    fn force_expire(&mut self) {
        if let Some(t) = self.armed_until.as_mut() {
            *t = Instant::now() - Duration::from_millis(1);
        }
    }
}

/// State staged between commands within one commissioning attempt:
/// `CSRRequest`'s freshly generated operational keypair (needed again at
/// `AddNOC` to cross-check the NOC's public key and to fill
/// `FabricEntry::op_private_key`) and `AddTrustedRootCertificate`'s RCAC
/// (needed again at `AddNOC` to verify the NOC's chain). Cleared once
/// `AddNOC` successfully installs a fabric.
#[derive(Debug, Default)]
struct PendingCommissioning {
    op_private_key: Option<[u8; 32]>,
    op_public_key: Option<[u8; 65]>,
    trusted_root_tlv: Option<Vec<u8>>,
}

/// Administrator Commissioning window state (spec §11.19.5) backing the
/// `WindowStatus`/`AdminFabricIndex`/`AdminVendorID` attributes — `Some`
/// while `OpenCommissioningWindow` has been accepted and the window hasn't
/// since been revoked or closed (by `close_admin_window`, Task 4's
/// timeout/`CommissioningComplete` handling).
#[derive(Debug, Clone, Copy)]
struct AdminWindow {
    fabric_index: u8,
    vendor_id: u16,
}

/// Staged by a successful `OpenCommissioningWindow` (spec §11.19.8.1) for
/// the net runtime (Task 4) to pick up via
/// `CommissioningServer::take_pending_window_request` and turn into an
/// actual PASE listener bound to `verifier`/`discriminator`. Core stays
/// timer/socket free (module doc), so `timeout_s` is handed over as a plain
/// duration — the runtime is the one that turns it into a deadline.
#[derive(Debug, Clone)]
pub struct WindowRequest {
    pub verifier: [u8; 97],
    pub discriminator: u16,
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub timeout_s: u16,
}

/// Shared state behind `CommissioningServer`'s `Arc<Mutex<..>>` — see the
/// module doc for why this isn't `CommissioningServer` itself.
struct Inner {
    dev: DevAttestation,
    fail_safe: FailSafeState,
    pending: PendingCommissioning,
    store: FabricStore,
    /// The fabric index `handle_add_noc` most recently installed within the
    /// current fail-safe attempt, if `CommissioningComplete` hasn't
    /// confirmed it yet (spec §11.10.7.2: a fail-safe transition without a
    /// completed commissioning must roll back the fabric change). Cleared
    /// (without removing anything) by `handle_commissioning_complete` on
    /// success; consumed by `rollback_uncommitted_fabric` (removing the
    /// fabric) on expiry or on a fresh/early `ArmFailSafe`.
    uncommitted_fabric_index: Option<u8>,
    /// The currently open Administrator Commissioning window, if any —
    /// backs the AC cluster's attribute reads. `None` both before the first
    /// `OpenCommissioningWindow` and after a `RevokeCommissioning` /
    /// `close_admin_window`.
    admin_window: Option<AdminWindow>,
    /// The most recent `OpenCommissioningWindow` request, staged for the
    /// runtime to collect (and clear) via `take_pending_window_request`.
    pending_window_request: Option<WindowRequest>,
    /// The `FabricEntry` a successful `RemoveFabric` (spec §11.17.6.15)
    /// most recently removed from `store`, staged for the runtime to
    /// collect (and clear) via `take_removed_fabric`. The runtime needs the
    /// full entry (not just the index) for two things `store` no longer has
    /// it for once `remove` returns: the `root_public_key`/`fabric_id` to
    /// derive the `compressed_fabric_id` for the mDNS operational-advert
    /// goodbye, and the `fabric_index` to compare against the invoking
    /// session's own — dropping that session if they match (an ephemeral
    /// commissioner fabric removing itself after handing off, e.g. an
    /// Android phone handing a device to Home Assistant).
    removed_fabric: Option<FabricEntry>,
    /// The shared ACL store `AccessControlHandler` (EP0) also holds, if a
    /// runtime has wired one in via `CommissioningServer::set_acl_store`.
    /// `None` in the commissioning-module's own unit tests that don't call
    /// it — `handle_add_noc`'s auto-admin-entry and `handle_remove_fabric`/
    /// `rollback_uncommitted_fabric`'s purge become no-ops rather than
    /// panicking, so every pre-existing test keeps passing unmodified.
    acl_store: Option<AclStore>,
    /// The shared GroupKeyStore `GroupKeyManagementHandler` (EP0) also
    /// holds, if a runtime has wired one in via
    /// `CommissioningServer::set_group_key_store` — same `Option`/purge
    /// shape as `acl_store` (doc above), including the `None`-is-a-no-op
    /// discipline for pre-existing tests.
    group_key_store: Option<GroupKeyStore>,
    /// The shared Groups membership store every bridged endpoint's
    /// `GroupsHandler` also holds, if a runtime has wired one in via
    /// `CommissioningServer::set_group_membership_store` — same
    /// `Option`/purge shape as `acl_store`/`group_key_store` (doc above),
    /// including the `None`-is-a-no-op discipline for pre-existing tests.
    group_membership_store: Option<GroupMembershipStore>,
}

impl Inner {
    /// Drops every per-fabric row `fabric_index` owns in the three shared
    /// stores (ACL, group keys, group membership). Called wherever a fabric
    /// leaves the store — `RemoveFabric` (both the persisted and the
    /// persist-failed branch) and the fail-safe rollback — so a later
    /// `AddNOC` reusing the index never inherits the previous occupant's
    /// rows (cross-fabric leak; see `handle_remove_fabric`'s comment).
    pub(super) fn purge_fabric_stores(&self, fabric_index: u8) {
        if let Some(store) = &self.acl_store {
            store.purge_fabric(fabric_index);
        }
        if let Some(store) = &self.group_key_store {
            store.purge_fabric(fabric_index);
        }
        if let Some(store) = &self.group_membership_store {
            store.purge_fabric(fabric_index);
        }
    }
}

/// Device-side commissioning server. Construct with `new`, then either call
/// `into_cluster_handlers` to register it on a `Node`'s endpoint 0, or (in
/// tests) dispatch commands directly.
pub struct CommissioningServer {
    inner: Arc<Mutex<Inner>>,
}

impl CommissioningServer {
    pub fn new(dev: DevAttestation, store: FabricStore) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                dev,
                fail_safe: FailSafeState::default(),
                pending: PendingCommissioning::default(),
                store,
                uncommitted_fabric_index: None,
                admin_window: None,
                pending_window_request: None,
                removed_fabric: None,
                acl_store: None,
                group_key_store: None,
                group_membership_store: None,
            })),
        }
    }

    /// Wires a shared `AclStore` in — the same store a runtime registers
    /// `AccessControlHandler` on EP0 with (`device.rs`), so `handle_add_noc`
    /// installs its automatic admin entry and `handle_remove_fabric`/the
    /// fail-safe rollback purge it into the store the cluster actually
    /// reads back. Must be called before any commissioning command that
    /// touches fabrics — in practice, before `into_cluster_handlers`.
    pub fn set_acl_store(&mut self, store: AclStore) {
        locked(&self.inner).acl_store = Some(store);
    }

    /// Wires a shared `GroupKeyStore` in — the same store a runtime
    /// registers `GroupKeyManagementHandler` on EP0 with (`device.rs`), so
    /// `handle_remove_fabric`/the fail-safe rollback purge the removed
    /// fabric's KeySets and GroupKeyMap entries out of the store the
    /// cluster actually reads back (`AclStore`'s `set_acl_store` doc above
    /// — same purpose, same "call before any fabric-touching command"
    /// requirement). Unlike `AclStore`, no commissioning command installs
    /// anything into this store automatically — `KeySetWrite`/the
    /// `ATTR_GROUP_KEY_MAP` write are commissionee-invoked commands the
    /// cluster handler itself serves, not something `AddNOC` stages.
    pub fn set_group_key_store(&mut self, store: GroupKeyStore) {
        locked(&self.inner).group_key_store = Some(store);
    }

    /// Wires a shared `GroupMembershipStore` in — the same store every
    /// bridged endpoint's `GroupsHandler` delegates to (`device.rs`), so
    /// `handle_remove_fabric`/the fail-safe rollback purge the removed
    /// fabric's memberships out of the store the handlers actually read
    /// back (`AclStore`'s `set_acl_store` doc above — same purpose, same
    /// "call before any fabric-touching command" requirement).
    pub fn set_group_membership_store(&mut self, store: GroupMembershipStore) {
        locked(&self.inner).group_membership_store = Some(store);
    }

    /// Fabrics installed so far (cloned out of the shared state — this
    /// device expects at most a handful, so the copy is cheap).
    pub fn fabrics(&self) -> Vec<FabricEntry> {
        locked(&self.inner).store.entries().to_vec()
    }

    /// Test-only visibility into whether any CSR/AddTrustedRoot material is
    /// currently staged — lets fail-safe-transition tests assert `pending`
    /// was actually discarded without making the field `pub`.
    #[cfg(test)]
    fn pending_is_empty(&self) -> bool {
        let inner = locked(&self.inner);
        inner.pending.op_private_key.is_none()
            && inner.pending.op_public_key.is_none()
            && inner.pending.trusted_root_tlv.is_none()
    }

    /// The current fail-safe window's deadline, if one is open right now —
    /// `None` both when never armed and once the window has already
    /// passed. Meant for a runtime `select` (Task 8) to wait on, so it can
    /// call `expire_fail_safe` right when the window closes instead of
    /// only noticing on the next incoming command.
    pub fn fail_safe_deadline(&self) -> Option<Instant> {
        locked(&self.inner).fail_safe.deadline()
    }

    /// spec §11.10.7.2: if the fail-safe's deadline has passed, rolls back
    /// whatever `AddNOC` installed within that window without a following
    /// `CommissioningComplete`, and returns the removed `FabricEntry` —
    /// the runtime (Task 8) needs its `fabric_id`/`node_id` to compute the
    /// `compressed_fabric_id` for the mDNS goodbye it sends once the
    /// operational advert for that fabric is no longer valid.
    ///
    /// `None` if the deadline hasn't passed yet, or if it has but there was
    /// nothing uncommitted to roll back — including a second call right
    /// after the first: `disarm()` already ran, so the fail-safe reads back
    /// as "never armed" and this short-circuits before touching the store.
    /// Callable either lazily (the next command handler could call this
    /// before doing anything else — not yet wired up; core only exposes
    /// the primitive) or from the runtime's own deadline timer.
    pub fn expire_fail_safe(&self) -> Option<FabricEntry> {
        locked(&self.inner).expire_fail_safe()
    }

    /// Takes (clearing) the `WindowRequest` staged by the most recent
    /// successful `OpenCommissioningWindow`, if any — the net runtime
    /// (Task 4) collects this right after dispatch and turns it into an
    /// actual PASE listener. `None` if no `OpenCommissioningWindow` has
    /// succeeded since the last time this was called.
    pub fn take_pending_window_request(&self) -> Option<WindowRequest> {
        locked(&self.inner).pending_window_request.take()
    }

    /// Takes (clearing) the `FabricEntry` a successful `RemoveFabric`
    /// (spec §11.17.6.15) most recently removed from the store, if any —
    /// the net runtime (Task 6) collects this right after dispatch, same
    /// spot as `take_pending_window_request`, and uses it to retract the
    /// fabric's mDNS operational advert and (if it was the invoking
    /// session's own fabric) end that session. `None` if no `RemoveFabric`
    /// has succeeded since the last time this was called.
    pub fn take_removed_fabric(&self) -> Option<FabricEntry> {
        locked(&self.inner).removed_fabric.take()
    }

    /// Whether an Administrator Commissioning window is currently open —
    /// the net runtime (Task 4) polls this to decide whether its own PASE
    /// listener should still be accepting connections.
    pub fn admin_window_is_open(&self) -> bool {
        locked(&self.inner).admin_window.is_some()
    }

    /// Closes the Administrator Commissioning window without going through
    /// `RevokeCommissioning` — for the net runtime (Task 4) to call on
    /// timeout expiry or `CommissioningComplete`, mirroring
    /// `handle_revoke_commissioning`'s effect on the AC attributes. A no-op
    /// if already closed.
    pub fn close_admin_window(&self) {
        locked(&self.inner).admin_window = None;
    }

    /// Test-only: see `FailSafeState::force_expire`.
    #[cfg(test)]
    fn force_expire_fail_safe(&self) {
        locked(&self.inner).fail_safe.force_expire();
    }

    /// Splits into the three `ClusterHandler` adapters `Node::add_cluster`
    /// registers on endpoint 0 (General Commissioning 0x0030, Node
    /// Operational Credentials 0x003E, Administrator Commissioning 0x003C)
    /// — see the module doc. Takes `&self` (not `self`) so a runtime can
    /// keep the original `CommissioningServer` around afterwards to poll
    /// `fabrics()` (e.g. to notice a fresh AddNOC and publish an
    /// operational mDNS advert) or `take_pending_window_request()` — all
    /// three handlers just clone the shared `Arc<Mutex<Inner>>`, same as the
    /// two clones already did when this took `self` by value.
    pub fn into_cluster_handlers(
        &self,
    ) -> (
        Box<dyn ClusterHandler>,
        Box<dyn ClusterHandler>,
        Box<dyn ClusterHandler>,
    ) {
        (
            Box::new(GeneralCommissioningHandler(Arc::clone(&self.inner))),
            Box::new(OperationalCredentialsHandler(Arc::clone(&self.inner))),
            Box::new(AdminCommissioningHandler(Arc::clone(&self.inner))),
        )
    }

    /// Dispatches one command directly, bypassing `Node`/IM wire framing
    /// and the two-adapter split — this module's own tests use it to
    /// exercise command logic without paying for TLV invoke-request framing
    /// on every step (`wired_into_node_dispatches_both_clusters` below
    /// separately proves the real `ClusterHandler`/`Node` path works).
    #[cfg(test)]
    fn invoke_command(
        &self,
        cluster: u32,
        command: u32,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        let mut inner = locked(&self.inner);
        match cluster {
            CLUSTER_GENERAL_COMMISSIONING => {
                inner.handle_general_commissioning(command, fields_tlv)
            }
            CLUSTER_OPERATIONAL_CREDENTIALS => {
                inner.handle_operational_credentials(command, fields_tlv, ctx)
            }
            CLUSTER_ADMIN_COMMISSIONING => {
                inner.handle_admin_commissioning(command, fields_tlv, ctx)
            }
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_CLUSTER),
        }
    }
}
