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

use mat_controller::attestation::{attestation_tbs, encode_attestation_elements};
use mat_controller::cert::{verify_noc_chain, MatterCert};
use mat_controller::commissioning::{
    decode_add_noc, decode_add_trusted_root, decode_arm_fail_safe, decode_attestation_request,
    decode_cert_chain_request, decode_csr_request, decode_open_commissioning_window,
    decode_remove_fabric, decode_set_regulatory_config, decode_update_fabric_label,
    encode_attestation_response, encode_cert_chain_response, encode_commissioning_status_response,
    encode_csr_response, encode_noc_response, encode_nocsr_elements, CERT_TYPE_DAC, CERT_TYPE_PAI,
    CLUSTER_ADMIN_COMMISSIONING, CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS,
    CMD_ADD_NOC, CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE, CMD_ATTESTATION_REQUEST,
    CMD_CERT_CHAIN_REQUEST, CMD_COMMISSIONING_COMPLETE, CMD_CSR_REQUEST,
    CMD_OPEN_COMMISSIONING_WINDOW, CMD_REMOVE_FABRIC, CMD_REVOKE_COMMISSIONING,
    CMD_SET_REGULATORY_CONFIG, CMD_UPDATE_FABRIC_LABEL,
};
use mat_controller::crypto::sign_ecdsa_p256;
use mat_controller::fabric::{compressed_fabric_id, derive_ipk_operational};
use mat_controller::im;
use mat_controller::sync::locked;
use mat_controller::tlv::{Tag, Writer};
use mat_controller::x509::{generate_csr, DevAttestation};

use crate::core::access_control::AclStore;
use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
use crate::core::fabric_store::{FabricEntry, FabricStore};
use crate::core::group_key_management::GroupKeyStore;
use crate::core::group_membership::GroupMembershipStore;

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
        self.inner
            .lock()
            .expect("commissioning server mutex poisoned")
            .group_key_store = Some(store);
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

/// Thin `ClusterHandler` adapter for General Commissioning (0x0030).
struct GeneralCommissioningHandler(Arc<Mutex<Inner>>);

impl ClusterHandler for GeneralCommissioningHandler {
    fn cluster_id(&self) -> u32 {
        CLUSTER_GENERAL_COMMISSIONING
    }

    /// ClusterRevision (spec §7.13): General Commissioning cluster spec
    /// revision 1 (Matter 1.4).
    fn revision(&self) -> u16 {
        1
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            ATTR_GC_BREADCRUMB,
            ATTR_GC_BASIC_COMMISSIONING_INFO,
            ATTR_GC_REGULATORY_CONFIG,
            ATTR_GC_LOCATION_CAPABILITY,
            ATTR_GC_SUPPORTS_CONCURRENT_CONNECTION,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        locked(&self.0).read_general_commissioning(attribute)
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        locked(&self.0).handle_general_commissioning(command, fields_tlv)
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![
            CMD_ARM_FAIL_SAFE,
            CMD_SET_REGULATORY_CONFIG,
            CMD_COMMISSIONING_COMPLETE,
        ]
    }

    fn generated_commands(&self) -> Vec<u32> {
        vec![
            RESP_ARM_FAIL_SAFE,
            RESP_SET_REGULATORY_CONFIG,
            RESP_COMMISSIONING_COMPLETE,
        ]
    }

    /// spec §11.10.5: General Commissioning のコマンドは全て Administer。
    /// commissioning 本番の呼び出しは PASE（fabric 0 = implicit
    /// Administer、`datamodel::acl_allows`）か、AddNOC 後の CASE で
    /// commissioner 自身の admin エントリ（`AclStore::add_case_admin`）
    /// 経由なので、この要求で正規フローが塞がることはない。
    fn invoke_privilege(&self, _command: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_ADMINISTER
    }
}

/// Thin `ClusterHandler` adapter for Node Operational Credentials (0x003E).
struct OperationalCredentialsHandler(Arc<Mutex<Inner>>);

impl ClusterHandler for OperationalCredentialsHandler {
    fn cluster_id(&self) -> u32 {
        CLUSTER_OPERATIONAL_CREDENTIALS
    }

    /// ClusterRevision (spec §7.13): Operational Credentials cluster spec
    /// revision 1 (Matter 1.4).
    fn revision(&self) -> u16 {
        1
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            ATTR_OC_NOCS,
            ATTR_OC_FABRICS,
            ATTR_OC_SUPPORTED_FABRICS,
            ATTR_OC_COMMISSIONED_FABRICS,
            ATTR_OC_TRUSTED_ROOT_CERTIFICATES,
            ATTR_OC_CURRENT_FABRIC_INDEX,
        ]
    }

    fn read(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>> {
        locked(&self.0).read_operational_credentials(attribute, ctx)
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        locked(&self.0).handle_operational_credentials(command, fields_tlv, ctx)
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![
            CMD_ATTESTATION_REQUEST,
            CMD_CERT_CHAIN_REQUEST,
            CMD_CSR_REQUEST,
            CMD_ADD_NOC,
            CMD_UPDATE_FABRIC_LABEL,
            CMD_REMOVE_FABRIC,
            CMD_ADD_TRUSTED_ROOT,
        ]
    }

    fn generated_commands(&self) -> Vec<u32> {
        vec![RESP_ATTESTATION, RESP_CERT_CHAIN, RESP_CSR, RESP_NOC]
    }

    /// spec §11.17.5: Operational Credentials のコマンドは全て Administer
    /// （AddNOC / RemoveFabric は fabric 資格そのものの操作）。理由は
    /// `GeneralCommissioningHandler::invoke_privilege` の doc と同じ。
    fn invoke_privilege(&self, _command: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_ADMINISTER
    }

    /// spec §11.17.5 のアクセス表: `NOCs` は read が Administer（NOC/ICAC
    /// はそれ自体クレデンシャルであり、`ACL` と同様にその fabric の管理者
    /// だけが読める — `access_control.rs` の `read_privilege` と同じ書き
    /// 方）。`Fabrics`/`TrustedRootCertificates`/`CurrentFabricIndex`/容量系
    /// は trait default の View のまま。
    fn read_privilege(&self, attribute: u32) -> u8 {
        match attribute {
            ATTR_OC_NOCS => crate::core::access_control::PRIVILEGE_ADMINISTER,
            _ => crate::core::access_control::PRIVILEGE_VIEW,
        }
    }
}

/// Thin `ClusterHandler` adapter for Administrator Commissioning (0x003C).
struct AdminCommissioningHandler(Arc<Mutex<Inner>>);

impl ClusterHandler for AdminCommissioningHandler {
    fn cluster_id(&self) -> u32 {
        CLUSTER_ADMIN_COMMISSIONING
    }

    /// ClusterRevision (spec §7.13): Administrator Commissioning cluster
    /// spec revision 1 (Matter 1.4).
    fn revision(&self) -> u16 {
        1
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            ATTR_AC_WINDOW_STATUS,
            ATTR_AC_ADMIN_FABRIC_INDEX,
            ATTR_AC_ADMIN_VENDOR_ID,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        locked(&self.0).read_admin_commissioning(attribute)
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        locked(&self.0).handle_admin_commissioning(command, fields_tlv, ctx)
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![CMD_OPEN_COMMISSIONING_WINDOW, CMD_REVOKE_COMMISSIONING]
    }

    /// spec §11.19.5: Administrator Commissioning のコマンドは全て
    /// Administer（別 admin を招き入れる窓の開閉なので、Manage 止まりの
    /// controller には出させない）。
    fn invoke_privilege(&self, _command: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_ADMINISTER
    }
}

impl Inner {
    fn handle_general_commissioning(&mut self, command: u32, fields_tlv: &[u8]) -> InvokeReply {
        match command {
            CMD_ARM_FAIL_SAFE => self.handle_arm_fail_safe(fields_tlv),
            CMD_SET_REGULATORY_CONFIG => self.handle_set_regulatory_config(fields_tlv),
            CMD_COMMISSIONING_COMPLETE => self.handle_commissioning_complete(),
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    fn handle_operational_credentials(
        &mut self,
        command: u32,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        match command {
            CMD_ATTESTATION_REQUEST => self.handle_attestation_request(fields_tlv, ctx),
            CMD_CERT_CHAIN_REQUEST => self.handle_cert_chain_request(fields_tlv),
            CMD_CSR_REQUEST => self.handle_csr_request(fields_tlv, ctx),
            CMD_ADD_TRUSTED_ROOT => self.handle_add_trusted_root(fields_tlv),
            CMD_ADD_NOC => self.handle_add_noc(fields_tlv),
            CMD_UPDATE_FABRIC_LABEL => self.handle_update_fabric_label(fields_tlv, ctx),
            CMD_REMOVE_FABRIC => self.handle_remove_fabric(fields_tlv),
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    fn handle_admin_commissioning(
        &mut self,
        command: u32,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        match command {
            CMD_OPEN_COMMISSIONING_WINDOW => self.handle_open_commissioning_window(fields_tlv, ctx),
            CMD_REVOKE_COMMISSIONING => self.handle_revoke_commissioning(),
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    /// General Commissioning attribute reads (spec §11.10.5). Answers the
    /// fixed set chip-tool/Echo read during and right after commissioning.
    fn read_general_commissioning(&self, attribute: u32) -> Option<Vec<u8>> {
        match attribute {
            ATTR_GC_BREADCRUMB => Some(uint_value(0)),
            ATTR_GC_BASIC_COMMISSIONING_INFO => {
                let mut w = Writer::new();
                w.start_struct(Tag::Anonymous);
                w.put_uint(Tag::Context(0), u64::from(FAIL_SAFE_EXPIRY_LENGTH_SECONDS));
                w.put_uint(Tag::Context(1), u64::from(FAIL_SAFE_MAX_CUMULATIVE_SECONDS));
                w.end_container();
                Some(w.finish())
            }
            ATTR_GC_REGULATORY_CONFIG => Some(uint_value(0)),
            ATTR_GC_LOCATION_CAPABILITY => Some(uint_value(2)),
            ATTR_GC_SUPPORTS_CONCURRENT_CONNECTION => Some(bool_value(true)),
            _ => None,
        }
    }

    /// Node Operational Credentials attribute reads (spec §11.17.5),
    /// reflecting whatever `AddNOC` has installed into `self.store` so far.
    /// `CurrentFabricIndex` (spec §11.17.5.3) is the reading session's own
    /// fabric index — carried in `ctx` (`ReadCtx`), not derivable from
    /// `self.store` alone. `NOCs`/`Fabrics`/`TrustedRootCertificates` are
    /// fabric-scoped lists, so `ctx.fabric_filtered` decides whether they
    /// answer with the accessing fabric's entry alone or the whole table
    /// (see `fabric_scoped_entries`); the two counters are plain scalars and
    /// are never filtered.
    fn read_operational_credentials(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            ATTR_OC_NOCS => Some(self.encode_nocs(ctx)),
            ATTR_OC_FABRICS => Some(self.encode_fabrics(ctx)),
            ATTR_OC_SUPPORTED_FABRICS => Some(uint_value(u64::from(SUPPORTED_FABRICS))),
            ATTR_OC_COMMISSIONED_FABRICS => Some(uint_value(self.store.entries().len() as u64)),
            ATTR_OC_TRUSTED_ROOT_CERTIFICATES => Some(self.encode_trusted_root_certificates(ctx)),
            ATTR_OC_CURRENT_FABRIC_INDEX => Some(uint_value(u64::from(ctx.fabric_index))),
            _ => None,
        }
    }

    /// Administrator Commissioning attribute reads (spec §11.19.5), backed
    /// by `admin_window`. `AdminFabricIndex`/`AdminVendorID` are `null`
    /// while the window is closed (spec §11.19.5.1/.2) rather than 0 —
    /// `Writer::put_null` distinguishes "no admin" from fabric index 0
    /// (never a real fabric index) or vendor id 0 (unassigned but legal).
    fn read_admin_commissioning(&self, attribute: u32) -> Option<Vec<u8>> {
        match attribute {
            ATTR_AC_WINDOW_STATUS => {
                Some(uint_value(if self.admin_window.is_some() { 1 } else { 0 }))
            }
            ATTR_AC_ADMIN_FABRIC_INDEX => Some(match self.admin_window {
                Some(w) => uint_value(u64::from(w.fabric_index)),
                None => null_value(),
            }),
            ATTR_AC_ADMIN_VENDOR_ID => Some(match self.admin_window {
                Some(w) => uint_value(u64::from(w.vendor_id)),
                None => null_value(),
            }),
            _ => None,
        }
    }

    /// The fabric table as one read should see it: every entry when the
    /// request asked for the unfiltered view, otherwise only the accessing
    /// fabric's (spec §8.9.2.4 — a fabric-filtered read of a fabric-scoped
    /// list returns just the accessing fabric's entries). A PASE session
    /// (`fabric_index` 0, never a valid index) therefore matches nothing and
    /// gets an empty list, which is exactly right: it has no fabric whose
    /// credentials it is entitled to see.
    fn fabric_scoped_entries(&self, ctx: &ReadCtx) -> impl Iterator<Item = &FabricEntry> {
        // Copied out of `ctx` rather than borrowed: the returned iterator
        // must only borrow `self`, not the caller's `ReadCtx`.
        let (filtered, accessing) = (ctx.fabric_filtered, ctx.fabric_index);
        self.store
            .entries()
            .iter()
            .filter(move |e| !filtered || e.fabric_index == accessing)
    }

    /// NOCs(0): `array[ struct{1: NOCValue, 2: ICACValue?, 254:
    /// FabricIndex} ]` (spec §11.17.5.3, `NOCStruct`). Fabric-scoped — see
    /// `fabric_scoped_entries`.
    fn encode_nocs(&self, ctx: &ReadCtx) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.fabric_scoped_entries(ctx) {
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(1), &entry.noc_tlv);
            if let Some(icac) = &entry.icac_tlv {
                w.put_bytes(Tag::Context(2), icac);
            }
            w.put_uint(Tag::Context(254), u64::from(entry.fabric_index));
            w.end_container();
        }
        w.end_container();
        w.finish()
    }

    /// Fabrics(1): `array[ struct{1: RootPublicKey, 2: VendorID, 3:
    /// FabricID, 4: NodeID, 5: Label, 254: FabricIndex} ]` (spec
    /// §11.17.5.3, `FabricDescriptorStruct`). `Label` reflects whatever
    /// `UpdateFabricLabel` (`handle_update_fabric_label`) has set — empty
    /// until then. Fabric-scoped — see `fabric_scoped_entries`.
    fn encode_fabrics(&self, ctx: &ReadCtx) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.fabric_scoped_entries(ctx) {
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(1), &entry.root_public_key);
            w.put_uint(Tag::Context(2), u64::from(entry.admin_vendor_id));
            w.put_uint(Tag::Context(3), entry.fabric_id);
            w.put_uint(Tag::Context(4), entry.node_id);
            w.put_str(Tag::Context(5), &entry.label);
            w.put_uint(Tag::Context(254), u64::from(entry.fabric_index));
            w.end_container();
        }
        w.end_container();
        w.finish()
    }

    /// TrustedRootCertificates(4): `array[ bytes(RootCACertificate TLV) ]`
    /// (spec §11.17.5.3) — one entry per installed fabric's RCAC.
    /// Fabric-scoped — see `fabric_scoped_entries`. Its entries carry no
    /// `FabricIndex` field of their own (they're bare certificate blobs),
    /// which is precisely why filtering matters here: an unfiltered read
    /// hands out every commissioner's root with nothing to tell them apart.
    fn encode_trusted_root_certificates(&self, ctx: &ReadCtx) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.fabric_scoped_entries(ctx) {
            w.put_bytes(Tag::Anonymous, &entry.root_tlv);
        }
        w.end_container();
        w.finish()
    }

    /// ArmFailSafe（spec §11.10.6.2）: records the timer. An
    /// `ExpiryLengthSeconds` of 0 disarms early (spec-legal way to release
    /// the fail-safe without waiting for it to expire). Either way — a
    /// fresh (re-)arm or an early disarm — the CSR/AddTrustedRoot material
    /// staged by a previous attempt is discarded (spec §11.10.7.2.1: that
    /// state must not survive a fail-safe transition without a completed
    /// `AddNOC`).
    fn handle_arm_fail_safe(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        let Ok((expiry_s, _breadcrumb)) = decode_arm_fail_safe(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        // spec §11.10.6.2: armed 中の非ゼロ ArmFailSafe は**タイマー延長**。
        // 進行中の試行の staged 素材（CSR/RCAC）と未確定 fabric は生かす —
        // matter.js は AddNOC 後に再アームしてから CASE で
        // CommissioningComplete を送るため、ここで巻き戻すと commissioning が
        // 完了できない（2026-08-18 実測）。zombie fabric（未確定のまま残る
        // AddNOC 結果, spec §11.10.7.2.1）の回収は fail-safe 満了
        // （`expire_fail_safe`）と早期 disarm（expiry=0）が担保する。
        // 未対応エッジ: 別セッションからの armed 中 ArmFailSafe は spec 上
        // BUSY を返すべきだが、セッション同一性の追跡は未実装（M3 で拾う）。
        let rolled_back = if expiry_s == 0 || !self.fail_safe.is_armed() {
            let rolled_back = self.rollback_uncommitted_fabric();
            self.pending = PendingCommissioning::default();
            rolled_back
        } else {
            None
        };
        if expiry_s == 0 {
            self.fail_safe.disarm();
        } else {
            self.fail_safe.arm(expiry_s);
        }
        // Debug-only, no behavior change (Echo interop observability): says
        // which of arm/disarm/re-arm this call took and whether it rolled
        // back a zombie fabric from a previous attempt.
        tracing::debug!(
            expiry_s,
            disarm = expiry_s == 0,
            rolled_back_fabric_index = ?rolled_back.as_ref().map(|e| e.fabric_index),
            "ArmFailSafe"
        );
        InvokeReply::Data {
            response_command: RESP_ARM_FAIL_SAFE,
            fields_tlv: encode_commissioning_status_response(0, ""),
        }
    }

    /// SetRegulatoryConfig（spec §11.10.6.4）: record-only, always succeeds
    /// once the fields decode (mat-device has no real regulatory table).
    fn handle_set_regulatory_config(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        if decode_set_regulatory_config(fields_tlv).is_err() {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        }
        InvokeReply::Data {
            response_command: RESP_SET_REGULATORY_CONFIG,
            fields_tlv: encode_commissioning_status_response(0, ""),
        }
    }

    /// CommissioningComplete（spec §11.10.6.6）: disarms the fail-safe and
    /// discards any staged CSR/AddTrustedRoot material — a completed
    /// commissioning has already consumed it via `AddNOC` (which clears
    /// `pending` itself on success), so nothing legitimate is lost.
    fn handle_commissioning_complete(&mut self) -> InvokeReply {
        self.fail_safe.disarm();
        self.pending = PendingCommissioning::default();
        // The AddNOC (if any) that installed a fabric within this window is
        // now confirmed — clear the marker without removing anything.
        // Contrast `rollback_uncommitted_fabric`, which `expire_fail_safe`
        // and `handle_arm_fail_safe` use to discard an *unconfirmed* one.
        self.uncommitted_fabric_index = None;
        InvokeReply::Data {
            response_command: RESP_COMMISSIONING_COMPLETE,
            fields_tlv: encode_commissioning_status_response(0, ""),
        }
    }

    /// Removes whatever fabric `uncommitted_fabric_index` marks (if any) —
    /// a fabric `AddNOC` installed within the current/most-recent fail-safe
    /// window that hasn't yet been confirmed by `CommissioningComplete`.
    /// Shared by `expire_fail_safe` (the window's deadline passed) and
    /// `handle_arm_fail_safe` (a fresh attempt must not inherit the
    /// previous attempt's zombie fabric). Returns the removed entry, if
    /// any — only `expire_fail_safe`'s caller needs it (Task 8's runtime
    /// needs `fabric_id`/`node_id` off it for the mDNS goodbye before it's
    /// gone from the store).
    fn rollback_uncommitted_fabric(&mut self) -> Option<FabricEntry> {
        let fabric_index = self.uncommitted_fabric_index.take()?;
        let removed = self
            .store
            .entries()
            .iter()
            .find(|e| e.fabric_index == fabric_index)
            .cloned();
        // See `FabricStore::remove`'s doc comment for why a save failure
        // here is swallowed rather than propagated: the removal from
        // memory already happened either way, and there's no `InvokeReply`
        // to report it through at this call site (both callers are
        // fire-and-forget housekeeping, not itself the response to a
        // command).
        let _ = self.store.remove(fabric_index);
        if let Some(store) = &self.acl_store {
            store.purge_fabric(fabric_index);
        }
        if let Some(store) = &self.group_key_store {
            store.purge_fabric(fabric_index);
        }
        if let Some(store) = &self.group_membership_store {
            store.purge_fabric(fabric_index);
        }
        removed
    }

    /// spec §11.10.7.2: if the fail-safe's deadline has passed, rolls back
    /// whatever `AddNOC` installed during that window without a following
    /// `CommissioningComplete`. See `CommissioningServer::expire_fail_safe`
    /// (the public entry point this backs) for the full contract.
    fn expire_fail_safe(&mut self) -> Option<FabricEntry> {
        if !self.fail_safe.is_expired() {
            return None;
        }
        self.fail_safe.disarm();
        self.pending = PendingCommissioning::default();
        self.rollback_uncommitted_fabric()
    }

    /// AttestationRequest（spec §11.17.6.7）: signs `AttestationElements`
    /// with the DAC key over `elements ‖ attestation_challenge` — the same
    /// construction `mat_controller::attestation::verify_device_attestation`
    /// checks on the commissioner side. Requires the fail-safe to be armed
    /// (spec §11.17: this and the other commissioning-flow commands below
    /// are only meaningful inside an armed fail-safe window).
    fn handle_attestation_request(&mut self, fields_tlv: &[u8], ctx: &InvokeCtx) -> InvokeReply {
        if !self.fail_safe.is_armed() {
            return InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED);
        }
        let Ok(nonce) = decode_attestation_request(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        // Real CMS-signed Certification Declaration (`mat_controller::cd`),
        // not a placeholder: chip-derived commissioners (chip-tool, Alexa,
        // Google) extract the CMS signer key id, look the verifying key up
        // in their own CD trust store, check the signature, and match the
        // CD's vendor_id/product_id against Basic Information — a device
        // whose CD they can't parse fails commissioning outright
        // (`kCertificationDeclarationNoKeyId`). `mat`'s own commissioner is
        // the lenient one (`attestation::verify_cd_warn` only warns), which
        // is why M1 got away with a placeholder here.
        let elements = encode_attestation_elements(&self.dev.certification_declaration, &nonce, 0);
        let tbs = attestation_tbs(&elements, &ctx.attestation_challenge);
        let signature = sign_ecdsa_p256(&self.dev.dac_private_key, &tbs)
            .expect("dac private key from generate_dev_attestation is always a valid p256 key");
        InvokeReply::Data {
            response_command: RESP_ATTESTATION,
            fields_tlv: encode_attestation_response(&elements, &signature),
        }
    }

    /// CertificateChainRequest（spec §11.17.6.4）: returns the DAC or PAI
    /// DER (never PAA — the commissioner never asks for it directly, it
    /// carries its own trust store per spec §6.2.3).
    fn handle_cert_chain_request(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        let Ok(cert_type) = decode_cert_chain_request(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        let der: &[u8] = match cert_type {
            CERT_TYPE_DAC => &self.dev.dac_der,
            CERT_TYPE_PAI => &self.dev.pai_der,
            _ => return InvokeReply::Status(im::STATUS_INVALID_COMMAND),
        };
        InvokeReply::Data {
            response_command: RESP_CERT_CHAIN,
            fields_tlv: encode_cert_chain_response(der),
        }
    }

    /// CSRRequest（spec §11.17.6.9）: generates a fresh operational
    /// keypair, stages it in `pending` for `AddNOC` to cross-check, and
    /// signs the `NOCSRElements` the same way `AttestationResponse` is
    /// signed (`elements ‖ attestation_challenge` with the DAC key — spec
    /// §11.17.5.6, mirrored exactly from the verification code in
    /// `mat_controller::commissioning::run_credential_steps`). Requires the
    /// fail-safe to be armed.
    fn handle_csr_request(&mut self, fields_tlv: &[u8], ctx: &InvokeCtx) -> InvokeReply {
        if !self.fail_safe.is_armed() {
            return InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED);
        }
        let Ok(nonce) = decode_csr_request(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };

        let secret = random_p256_secret();
        let op_public_key = public_key_bytes(&secret);
        let op_private_key: [u8; 32] = secret.to_bytes().into();
        let csr_der = generate_csr(&secret)
            .expect("csr generation over a freshly generated p256 key never fails");

        self.pending.op_private_key = Some(op_private_key);
        self.pending.op_public_key = Some(op_public_key);

        let elements = encode_nocsr_elements(&csr_der, &nonce);
        let tbs = attestation_tbs(&elements, &ctx.attestation_challenge);
        let signature = sign_ecdsa_p256(&self.dev.dac_private_key, &tbs)
            .expect("dac private key from generate_dev_attestation is always a valid p256 key");

        InvokeReply::Data {
            response_command: RESP_CSR,
            fields_tlv: encode_csr_response(&elements, &signature),
        }
    }

    /// AddTrustedRootCertificate（spec §11.17.6.11）: stages the RCAC for
    /// `AddNOC` to verify the NOC's chain against. Response is a plain IM
    /// success status, not a `NOCResponse` (spec defines no dedicated
    /// response command for this one). Requires the fail-safe to be armed.
    fn handle_add_trusted_root(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        if !self.fail_safe.is_armed() {
            return InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED);
        }
        let Ok(rcac_tlv) = decode_add_trusted_root(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        self.pending.trusted_root_tlv = Some(rcac_tlv);
        InvokeReply::Status(im::STATUS_SUCCESS)
    }

    /// AddNOC（spec §11.17.6.13）: verifies the NOC's chain against the
    /// staged RCAC, cross-checks its public key against the staged CSR
    /// keypair, derives the operational IPK, and installs a `FabricEntry`.
    /// Requires the fail-safe to be armed (spec: most General/Operational
    /// Credentials commissioning commands do; `mat-device` enforces it here
    /// since `AddNOC` is the one with externally observable side effects —
    /// a fabric actually gets installed).
    fn handle_add_noc(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        if !self.fail_safe.is_armed() {
            return InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED);
        }
        let Ok((noc_tlv, icac_tlv, ipk_epoch, case_admin_subject, admin_vendor_id)) =
            decode_add_noc(fields_tlv)
        else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        // matter.js 系コントローラ（HA matter-server 等）は「ICAC なし」を
        // フィールド省略ではなく空バイト列の ICACValue で表現する。chip 本家の
        // デバイス実装も空 ICAC は「無し」扱いなので、ここで正規化する。
        let icac_tlv = icac_tlv.filter(|v| !v.is_empty());

        let noc_status = |status: u8| InvokeReply::Data {
            response_command: RESP_NOC,
            fields_tlv: encode_noc_response(status, None),
        };

        // spec §11.17.6.13.1: past `SupportedFabrics` the answer is
        // `TableFull`, and it comes before any of the certificate checks —
        // there is no point verifying a chain for a fabric that has nowhere
        // to go, and the capacity is the same number the `SupportedFabrics`
        // attribute reports.
        if self.store.entries().len() >= usize::from(SUPPORTED_FABRICS) {
            tracing::debug!(
                supported_fabrics = SUPPORTED_FABRICS,
                "AddNOC rejected: TableFull"
            );
            return noc_status(NOC_STATUS_TABLE_FULL);
        }

        // spec §11.17.6.8.1: the admin subject must be a real operational
        // node id or a CAT with a non-zero version — anything else would
        // install an Administer ACL entry nobody can ever match (a fabric
        // whose only admin is locked out). Checked before the certificate
        // work, like `TableFull`: no point verifying a chain for a fabric
        // that can't be administered. `pending` is left intact so the same
        // session can retry with a valid subject.
        if crate::core::access_control::subject_kind(case_admin_subject).is_none() {
            tracing::debug!(
                reason = "invalid admin subject",
                case_admin_subject = format_args!("{case_admin_subject:#x}"),
                "AddNOC rejected: InvalidAdminSubject"
            );
            return noc_status(NOC_STATUS_INVALID_ADMIN_SUBJECT);
        }

        let (Some(root_tlv), Some(op_private_key), Some(op_public_key)) = (
            self.pending.trusted_root_tlv.clone(),
            self.pending.op_private_key,
            self.pending.op_public_key,
        ) else {
            return noc_status(NOC_STATUS_MISSING_CSR);
        };

        // InvalidNOC は複数分岐から返る。どの検証で落ちたかは応答からは
        // 区別できないため、拒否時は理由と素材（TLV hex）を debug ログに残す。
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let rcac = match MatterCert::parse(&root_tlv) {
            Ok(cert) => cert,
            Err(e) => {
                tracing::debug!(reason = "rcac parse", error = %e, rcac_tlv = %hex(&root_tlv), "AddNOC rejected: InvalidNOC");
                return noc_status(NOC_STATUS_INVALID_NOC);
            }
        };
        let noc = match MatterCert::parse(&noc_tlv) {
            Ok(cert) => cert,
            Err(e) => {
                tracing::debug!(reason = "noc parse", error = %e, noc_tlv = %hex(&noc_tlv), "AddNOC rejected: InvalidNOC");
                return noc_status(NOC_STATUS_INVALID_NOC);
            }
        };
        let icac = match icac_tlv.as_deref().map(MatterCert::parse) {
            Some(Ok(cert)) => Some(cert),
            Some(Err(e)) => {
                tracing::debug!(reason = "icac parse", error = %e, icac_tlv = %hex(icac_tlv.as_deref().unwrap_or_default()), "AddNOC rejected: InvalidNOC");
                return noc_status(NOC_STATUS_INVALID_NOC);
            }
            None => None,
        };
        if let Err(e) = verify_noc_chain(&noc, icac.as_ref(), &rcac) {
            tracing::debug!(reason = "chain verify", error = %e, has_icac = icac.is_some(), noc_tlv = %hex(&noc_tlv), rcac_tlv = %hex(&root_tlv), "AddNOC rejected: InvalidNOC");
            return noc_status(NOC_STATUS_INVALID_NOC);
        }
        if noc.pub_key != op_public_key {
            tracing::debug!(
                reason = "public key mismatch",
                "AddNOC rejected: InvalidPublicKey"
            );
            return noc_status(NOC_STATUS_INVALID_PUBLIC_KEY);
        }
        let (Some(node_id), Some(fabric_id)) = (noc.node_id(), noc.fabric_id()) else {
            tracing::debug!(reason = "node/fabric id missing", node_id = ?noc.node_id(), fabric_id = ?noc.fabric_id(), noc_tlv = %hex(&noc_tlv), "AddNOC rejected: InvalidNOC");
            return noc_status(NOC_STATUS_INVALID_NOC);
        };

        let cfid = compressed_fabric_id(&rcac.pub_key, fabric_id);
        let ipk_operational = derive_ipk_operational(&ipk_epoch, &cfid);
        let fabric_index = self.store.next_fabric_index();
        let entry = FabricEntry {
            fabric_index,
            root_tlv,
            noc_tlv,
            icac_tlv,
            op_private_key,
            ipk_operational,
            node_id,
            fabric_id,
            root_public_key: rcac.pub_key,
            admin_subject: case_admin_subject,
            admin_vendor_id,
            label: String::new(),
        };
        if self.store.insert(entry).is_err() {
            return noc_status(NOC_STATUS_TABLE_FULL);
        }

        // Prerequisite steps are one-shot: a second AddNOC on this session
        // must re-CSR / re-AddTrustedRoot, not silently reuse stale material.
        self.pending = PendingCommissioning::default();
        // Installed, but not yet confirmed — `handle_commissioning_complete`
        // clears this marker on success; a fail-safe expiry or a fresh/early
        // `ArmFailSafe` before that rolls the fabric back (spec §11.10.7.2).
        self.uncommitted_fabric_index = Some(fabric_index);

        // spec §11.17.6.8: AddNOC installs an automatic ACL entry granting
        // Administer privilege to the CASE admin subject — without it, the
        // commissioner that just wrote this fabric could never read/write
        // its own ACL again (nothing on the device would authorize it to).
        if let Some(store) = &self.acl_store {
            store.add_case_admin(fabric_index, case_admin_subject);
        }

        InvokeReply::Data {
            response_command: RESP_NOC,
            fields_tlv: encode_noc_response(NOC_STATUS_OK, Some(fabric_index)),
        }
    }

    /// UpdateFabricLabel（spec §11.17.6.11）: fabric-scoped — always targets
    /// the *invoking session's own* fabric (`ctx.fabric_index`), never a
    /// fabric index named in the command fields (the command has none; only
    /// `RemoveFabric` takes an explicit `FabricIndex`). No fail-safe
    /// requirement, unlike `AddNOC` — this runs against a fabric that's
    /// already fully commissioned.
    fn handle_update_fabric_label(&mut self, fields_tlv: &[u8], ctx: &InvokeCtx) -> InvokeReply {
        let Ok(label) = decode_update_fabric_label(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };

        let fabric_index = ctx.fabric_index;
        // `update_label` itself distinguishes "no such fabric" (`Ok(false)`)
        // from "found it, but the write-through to disk failed" (`Err`) —
        // branch on that directly instead of a separate existence pre-scan
        // (the pre-scan and `update_label`'s own lookup would otherwise walk
        // `self.store.entries()` twice to answer the same question).
        match self.store.update_label(fabric_index, label) {
            Ok(true) => InvokeReply::Data {
                response_command: RESP_NOC,
                fields_tlv: encode_noc_response(NOC_STATUS_OK, Some(fabric_index)),
            },
            Ok(false) => InvokeReply::Data {
                response_command: RESP_NOC,
                fields_tlv: encode_noc_response(NOC_STATUS_INVALID_FABRIC_INDEX, None),
            },
            // No `NodeOperationalCertStatusEnum` member means "storage
            // write error" (`AddNOC` fakes one with `NOC_STATUS_TABLE_FULL`
            // — that borrowed meaning doesn't fit here without misleading
            // the caller about *which* fabric failed). The global `FAILURE`
            // status is the honest signal instead: the label already applies
            // to the in-memory table (so the next `Fabrics` read reflects it
            // regardless of this branch), but the caller must not be told
            // `OK` when the change may not survive a restart.
            Err(e) => {
                tracing::debug!(error = %e, fabric_index, "UpdateFabricLabel: persist failed");
                InvokeReply::Status(im::STATUS_FAILURE)
            }
        }
    }

    /// RemoveFabric（spec §11.17.6.15）: unlike `UpdateFabricLabel`, targets
    /// the `FabricIndex` named in the command's own fields, not necessarily
    /// the invoking session's fabric — an administrator (or, per this
    /// branch's motivating case, a commissioner phone that just handed a
    /// device off via `OpenCommissioningWindow`) can remove any fabric,
    /// including its own. Looks the entry up (and clones it) *before*
    /// calling `store.remove` — `FabricStore::remove` only reports whether
    /// something was removed, not what it was, and the runtime needs the
    /// full entry (root public key, fabric id, node id) to retract the
    /// mDNS operational advert and decide whether to drop the session (see
    /// `removed_fabric`'s doc comment). Mirrors
    /// `rollback_uncommitted_fabric`'s find-then-remove shape.
    fn handle_remove_fabric(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        let Ok(fabric_index) = decode_remove_fabric(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };

        let Some(entry) = self
            .store
            .entries()
            .iter()
            .find(|e| e.fabric_index == fabric_index)
            .cloned()
        else {
            return InvokeReply::Data {
                response_command: RESP_NOC,
                fields_tlv: encode_noc_response(NOC_STATUS_INVALID_FABRIC_INDEX, None),
            };
        };

        match self.store.remove(fabric_index) {
            Ok(true) => {
                self.removed_fabric = Some(entry);
                if let Some(store) = &self.acl_store {
                    store.purge_fabric(fabric_index);
                }
                if let Some(store) = &self.group_key_store {
                    store.purge_fabric(fabric_index);
                }
                if let Some(store) = &self.group_membership_store {
                    store.purge_fabric(fabric_index);
                }
                InvokeReply::Data {
                    response_command: RESP_NOC,
                    fields_tlv: encode_noc_response(NOC_STATUS_OK, Some(fabric_index)),
                }
            }
            // `entry` was just found above under the same `&mut self`
            // borrow — nothing can have removed it out from under us
            // between the two calls, so `remove` reporting "wasn't there"
            // here would mean the two lookups disagree, which can't happen.
            Ok(false) => unreachable!(
                "fabric_index {fabric_index} found in store.entries() moments ago, \
                 store.remove() must still find it"
            ),
            // Same asymmetry as `UpdateFabricLabel`: the in-memory removal
            // already happened (`FabricStore::remove`'s doc comment — it
            // does *not* roll the removal back on a save failure), so the
            // fabric is really gone from this device's perspective even
            // though the reply below is `STATUS_FAILURE` rather than a
            // success `NOCResponse` — the caller must not be told `OK` when
            // the removal may not survive a restart. `removed_fabric` is
            // still staged, though (unlike the reply, which must stay
            // honest about durability): the runtime's mDNS retract and
            // same-session drop must follow the in-memory truth, not the
            // reply. Leaving it unstaged would strand a stale operational
            // advert and, worse, leave a session alive against a fabric
            // that no longer resolves — spec §2.5.11 requires removing a
            // fabric to terminate its sessions, and that's true regardless
            // of whether the removal also made it to disk. Same reasoning
            // forces the ACL purge here too (precedent: 31f4b44 did this
            // for `removed_fabric` itself) — `next_fabric_index` reissues a
            // removed index as `max(existing)+1`, so an unpurged entry here
            // would apply the previous occupant's ACL to whatever
            // unrelated fabric a later `AddNOC` installs at that index
            // (cross-fabric ACL leak), regardless of whether this removal
            // made it to disk.
            Err(e) => {
                tracing::debug!(error = %e, fabric_index, "RemoveFabric: persist failed");
                self.removed_fabric = Some(entry);
                if let Some(store) = &self.acl_store {
                    store.purge_fabric(fabric_index);
                }
                if let Some(store) = &self.group_key_store {
                    store.purge_fabric(fabric_index);
                }
                if let Some(store) = &self.group_membership_store {
                    store.purge_fabric(fabric_index);
                }
                InvokeReply::Status(im::STATUS_FAILURE)
            }
        }
    }

    /// OpenCommissioningWindow（spec §11.19.8.1, ECM — this server never
    /// serves the legacy basic-commissioning-method window）: validates the
    /// PAKE parameters, rejects if a window is already open, then records
    /// `admin_window` (for the AC attributes) and stages a `WindowRequest`
    /// for the runtime to turn into an actual PASE listener. Requires a
    /// timed invoke in the real protocol (spec §11.19.8.1) — enforced by
    /// the IM layer upstream of this handler, not re-checked here.
    fn handle_open_commissioning_window(
        &mut self,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        let Ok((timeout_s, verifier, discriminator, iterations, salt)) =
            decode_open_commissioning_window(fields_tlv)
        else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        if !(180..=900).contains(&timeout_s) {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        }
        if verifier.len() != 97
            || !(1000..=100_000).contains(&iterations)
            || !(16..=32).contains(&salt.len())
        {
            return InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: AC_STATUS_PAKE_PARAMETER_ERROR,
            };
        }
        if self.admin_window.is_some() {
            return InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: AC_STATUS_BUSY,
            };
        }

        // AdminVendorID (spec §11.19.5.3) reflects the vendor id of the
        // fabric that opened this window — the vendor id the *invoking*
        // admin's own AddNOC recorded (`FabricEntry::admin_vendor_id`), not
        // anything from this command's own fields (OpenCommissioningWindow
        // carries no vendor id). Unassigned (0) if the invoking session's
        // fabric index isn't in the table — shouldn't happen for a CASE
        // session past AddNOC, but this handler doesn't assume it.
        let vendor_id = self
            .store
            .entries()
            .iter()
            .find(|e| e.fabric_index == ctx.fabric_index)
            .map_or(0, |e| e.admin_vendor_id);
        self.admin_window = Some(AdminWindow {
            fabric_index: ctx.fabric_index,
            vendor_id,
        });
        let verifier: [u8; 97] = verifier
            .try_into()
            .expect("verifier length already checked == 97 above");
        self.pending_window_request = Some(WindowRequest {
            verifier,
            discriminator,
            iterations,
            salt,
            timeout_s,
        });
        InvokeReply::Status(im::STATUS_SUCCESS)
    }

    /// RevokeCommissioning（spec §11.19.8.2）: closes the window if one is
    /// open, or `WindowNotOpen` if not. The actual PASE listener teardown
    /// happens in the net runtime (Task 4), which reads
    /// `admin_window_is_open()` after this dispatches to notice the
    /// closure — this handler only owns the AC attribute state.
    fn handle_revoke_commissioning(&mut self) -> InvokeReply {
        if self.admin_window.is_none() {
            return InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: AC_STATUS_WINDOW_NOT_OPEN,
            };
        }
        self.admin_window = None;
        InvokeReply::Status(im::STATUS_SUCCESS)
    }
}

/// Encodes a scalar as one standalone, `Tag::Anonymous`-tagged TLV element
/// (the `ClusterHandler::read` contract) — same convention as
/// `datamodel::uint_value`, duplicated here since that one is private to its
/// own module.
fn uint_value(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_uint(Tag::Anonymous, v);
    w.finish()
}

fn bool_value(v: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_bool(Tag::Anonymous, v);
    w.finish()
}

/// A standalone, `Tag::Anonymous`-tagged TLV `null` element — for nullable
/// attributes (e.g. `AdminFabricIndex`/`AdminVendorID` while the
/// Administrator Commissioning window is closed) that must read back
/// distinct from a valid `0`.
fn null_value() -> Vec<u8> {
    let mut w = Writer::new();
    w.put_null(Tag::Anonymous);
    w.finish()
}

/// Generates a fresh non-zero P-256 secret key (rejects the ~0-probability
/// out-of-range case and retries with fresh randomness) — device-side
/// equivalent of `mat_controller::case::random_p256_secret`, which is
/// `pub(crate)` there and so not reachable from this crate.
fn random_p256_secret() -> p256::SecretKey {
    loop {
        let mut b = [0u8; 32];
        getrandom::getrandom(&mut b).expect("os rng");
        if let Ok(sk) = p256::SecretKey::from_slice(&b) {
            return sk;
        }
    }
}

/// `secret`'s SEC1 uncompressed public key (65 bytes) — device-side
/// equivalent of `mat_controller::case::eph_pub_bytes`.
fn public_key_bytes(secret: &p256::SecretKey) -> [u8; 65] {
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    secret
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .expect("uncompressed p256 point is 65 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fabric_store::FabricPersist;
    use mat_controller::attestation::verify_device_attestation;
    use mat_controller::commissioning::{
        decode_attestation_response, decode_commissioning_status_response, decode_csr_response,
        decode_noc_response, encode_add_noc, encode_add_trusted_root, encode_arm_fail_safe,
        encode_attestation_request, encode_cert_chain_request, encode_csr_request,
        encode_open_commissioning_window, encode_remove_fabric, encode_update_fabric_label,
        parse_nocsr_elements, CommissioningFabric,
    };
    use mat_controller::im;
    use mat_controller::tlv::{Reader, Tag, Value, Writer};
    use mat_controller::x509::{generate_dev_attestation, parse_csr};

    /// Fixed per-test "session" attestation challenge — in the real
    /// protocol this comes from `SecureSession::attestation_challenge()`
    /// and stays constant for the lifetime of one PASE/CASE session; tests
    /// drive several commands against the same `InvokeCtx` value to match.
    const TEST_CHALLENGE: [u8; 16] = [42u8; 16];

    fn test_ctx() -> InvokeCtx {
        InvokeCtx {
            attestation_challenge: TEST_CHALLENGE,
            ..InvokeCtx::default()
        }
    }

    /// A `ReadCtx` for a session on `fabric_index`, at the (now filtered)
    /// default — for the tests that only care *which* fabric is reading and
    /// always pass the fabric that's actually installed, so filtered vs.
    /// unfiltered makes no difference to what comes back. The
    /// fabric-filtering behavior itself is covered by
    /// `oc_reads_are_fabric_scoped_when_fabric_filtered`, which spells both
    /// fields out at every call site.
    fn read_ctx(fabric_index: u8) -> ReadCtx {
        ReadCtx {
            fabric_index,
            ..ReadCtx::default()
        }
    }

    /// A `CommissioningServer` over a freshly generated dev attestation
    /// chain and an in-memory (non-persisted) `FabricStore` — file-backed
    /// persistence is `net`-only and tested in `net::store` instead (this
    /// module must stay `cargo check --no-default-features`-clean).
    fn test_server() -> CommissioningServer {
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        CommissioningServer::new(dev, FabricStore::new())
    }

    /// Drives ArmFailSafe → CSRRequest → AddTrustedRootCertificate → AddNOC
    /// against `server`, installing one fabric (`fabric_id`/`node_id` from
    /// the caller; admin subject `0xAA` and admin vendor id `0xFFF1` fixed —
    /// no test here needs to vary those). Returns the `AddNOC` reply so
    /// callers that care about the command's own response (like
    /// `add_noc_installs_fabric`) can assert on it directly. Delegates to
    /// `install_fabric_with_admin` with admin subject `0xAA`.
    fn install_fabric(
        server: &mut CommissioningServer,
        fabric_id: u64,
        node_id: u64,
    ) -> InvokeReply {
        install_fabric_with_admin(server, fabric_id, node_id, 0xAA).0
    }

    /// `install_fabric` の admin subject 可変版: ArmFailSafe → CSR →
    /// AddTrustedRoot まで進めてから `case_admin_subject` で AddNOC を
    /// 打ち、その reply と「同じ pending で再 AddNOC するための NOC/
    /// fabric」を返す。
    fn install_fabric_with_admin(
        server: &mut CommissioningServer,
        fabric_id: u64,
        node_id: u64,
        case_admin_subject: u64,
    ) -> (InvokeReply, Vec<u8>, CommissioningFabric) {
        let fabric = CommissioningFabric::generate(fabric_id, 0xAA).unwrap();

        drive_invoke(
            server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );

        let (_, csr_resp) = expect_data(drive_invoke(
            server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[3u8; 32]),
        ));
        let (elements, _sig) = decode_csr_response(&csr_resp).unwrap();
        let (csr_der, _nonce) = parse_nocsr_elements(&elements).unwrap();
        let device_pub = parse_csr(&csr_der).unwrap();
        let noc = fabric.issue_device_noc(&device_pub, node_id).unwrap();

        assert_eq!(
            drive_invoke(
                server,
                CLUSTER_OPERATIONAL_CREDENTIALS,
                CMD_ADD_TRUSTED_ROOT,
                &encode_add_trusted_root(&fabric.rcac_tlv),
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );

        let reply = drive_invoke(
            server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &encode_add_noc(&noc, &fabric.ipk_epoch, case_admin_subject, 0xFFF1),
        );
        (reply, noc, fabric)
    }

    /// A `CommissioningServer` with one fabric already installed
    /// (fabric_id=0x1122, node_id=0x5001, admin_vendor_id=0xFFF1) — shared
    /// setup for the GC/OC attribute-read tests below, which only care
    /// about the resulting fabric state, not the `AddNOC` command's own
    /// reply (`add_noc_installs_fabric` covers that separately via
    /// `install_fabric` directly).
    fn commissioned_server() -> CommissioningServer {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001);
        server
    }

    /// Drives one command directly against `server`'s shared state
    /// (bypassing `Node`/IM wire framing — `wired_into_node_dispatches_
    /// both_clusters` below covers that separately). Unlike the brief's
    /// illustrative 3-arg sketch, this takes `cluster` explicitly: several
    /// `CMD_*` request ids collide numerically across the two clusters
    /// (e.g. `CMD_ARM_FAIL_SAFE == CMD_ATTESTATION_REQUEST == 0x00`), so
    /// inferring the cluster from the command id alone would be ambiguous.
    fn drive_invoke(
        server: &mut CommissioningServer,
        cluster: u32,
        command: u32,
        fields: &[u8],
    ) -> InvokeReply {
        server.invoke_command(cluster, command, fields, &test_ctx())
    }

    fn expect_data(reply: InvokeReply) -> (u32, Vec<u8>) {
        match reply {
            InvokeReply::Data {
                response_command,
                fields_tlv,
            } => (response_command, fields_tlv),
            InvokeReply::Status(status) => {
                panic!("expected data reply, got status 0x{status:02X}")
            }
            InvokeReply::ClusterStatus {
                status,
                cluster_status,
            } => {
                panic!(
                    "expected data reply, got cluster status 0x{status:02X} (cluster-specific: 0x{cluster_status:02X})"
                )
            }
        }
    }

    /// rollback / RemoveFabric の purge 対象 3 store を配線した server。
    fn server_with_stores() -> (
        CommissioningServer,
        crate::core::access_control::AclStore,
        GroupKeyStore,
        GroupMembershipStore,
    ) {
        let mut server = test_server();
        let acl = crate::core::access_control::AclStore::new();
        let gk = GroupKeyStore::new();
        let membership = GroupMembershipStore::new();
        server.set_acl_store(acl.clone());
        server.set_group_key_store(gk.clone());
        server.set_group_membership_store(membership.clone());
        (server, acl, gk, membership)
    }

    /// fabric 1 の admin (subject 0xAA) が ACL 上 Administer を持つか —
    /// AddNOC の自動 admin エントリの有無を表す。
    fn admin_allowed(acl: &crate::core::access_control::AclStore) -> bool {
        acl.check(
            1,
            crate::core::access_control::Subject::node(0xAA),
            crate::core::access_control::PRIVILEGE_ADMINISTER,
            0,
            im::CLUSTER_ACCESS_CONTROL,
        )
    }

    /// install 直後に 3 store へ fabric 1 の状態を仕込む（admin ACL は AddNOC が
    /// 自動で入れる）。
    fn seed_fabric_state(gk: &GroupKeyStore, membership: &GroupMembershipStore) {
        gk.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();
        gk.replace_fabric_map(1, vec![(0x000A, 7)]);
        membership.add(1, 0x000A, 2).unwrap();
    }

    fn assert_fabric_state_purged(
        acl: &crate::core::access_control::AclStore,
        gk: &GroupKeyStore,
        membership: &GroupMembershipStore,
    ) {
        assert!(!admin_allowed(acl), "ACL admin entry must be purged");
        assert!(!gk.keyset_exists(1, 7));
        assert!(gk.map_entries_for(1).is_empty());
        assert!(
            membership.groups_by_fabric().is_empty(),
            "membership must be purged"
        );
    }

    #[test]
    fn add_noc_installs_fabric() {
        let mut server = test_server();
        let (_, resp) = expect_data(install_fabric(&mut server, 0x1122, 0x5001));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);
        assert_eq!(fabric_index, Some(1));
        assert_eq!(server.fabrics().len(), 1);
        assert_eq!(server.fabrics()[0].node_id, 0x5001);
        assert_eq!(server.fabrics()[0].fabric_id, 0x1122);
        assert_eq!(server.fabrics()[0].admin_vendor_id, 0xFFF1);
    }

    /// matter.js（Home Assistant の matter-server 等）は「ICAC なし」を
    /// フィールド省略ではなく**空バイト列の ICACValue** で送る（chip-tool は
    /// 省略）。chip 本家のデバイス実装も空 ICAC は「無し」として扱うため、
    /// 空 ICACValue 付きの AddNOC は省略時と同様に成功しなければならない。
    /// 実測: HA matter-server 1.1.7 からの commissioning が本ケースで
    /// InvalidNOC(3) になり中断していた（2026-08-18）。
    #[test]
    fn add_noc_accepts_empty_icac_value_as_absent() {
        let mut server = test_server();
        let fabric = CommissioningFabric::generate(0x1122, 0xAA).unwrap();

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        let (_, csr_resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[3u8; 32]),
        ));
        let (elements, _sig) = decode_csr_response(&csr_resp).unwrap();
        let (csr_der, _nonce) = parse_nocsr_elements(&elements).unwrap();
        let device_pub = parse_csr(&csr_der).unwrap();
        let noc = fabric.issue_device_noc(&device_pub, 0x5001).unwrap();
        assert_eq!(
            drive_invoke(
                &mut server,
                CLUSTER_OPERATIONAL_CREDENTIALS,
                CMD_ADD_TRUSTED_ROOT,
                &encode_add_trusted_root(&fabric.rcac_tlv),
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );

        // AddNOC {0: NOCValue, 1: ICACValue(空), 2: IPKValue, 3:
        // CaseAdminSubject, 4: AdminVendorId} — encode_add_noc は ICAC を
        // 書かないので、matter.js が送る形をここで直接組み立てる。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), &noc);
        w.put_bytes(Tag::Context(1), &[]);
        w.put_bytes(Tag::Context(2), &fabric.ipk_epoch);
        w.put_uint(Tag::Context(3), 0xAA);
        w.put_uint(Tag::Context(4), 0xFFF1);
        w.end_container();

        let (_, resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &w.finish(),
        ));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0, "empty ICACValue must be treated as absent");
        assert_eq!(fabric_index, Some(1));
        assert_eq!(server.fabrics().len(), 1);
    }

    /// UpdateFabricLabel: NOCResponse(Ok) を返し、store に永続化され、
    /// Fabrics 属性の読みに Label が反映される。
    #[test]
    fn update_fabric_label_persists_and_reflects_in_fabrics_attr() {
        let server = commissioned_server(); // fabric_index=1
        let fields = encode_update_fabric_label("Alexa-1");
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_UPDATE_FABRIC_LABEL,
            &fields,
            &ctx,
        ));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);
        assert_eq!(fabric_index, Some(1));
        assert_eq!(server.fabrics()[0].label, "Alexa-1");

        // Fabrics 属性（ATTR_OC_FABRICS）の TLV に "Alexa-1" が Label
        // フィールド（context tag 5, spec §11.17.5.20 FabricDescriptorStruct）
        // として現れることを Reader で確認する。
        let (_, oc, _) = server.into_cluster_handlers();
        let fabrics_tlv = oc
            .read(ATTR_OC_FABRICS, &ReadCtx::unfiltered(0))
            .expect("Fabrics");
        let mut r = Reader::new(&fabrics_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut label = None;
        loop {
            let el = r
                .next()
                .unwrap()
                .expect("truncated FabricDescriptor struct");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(5), Value::Utf8(s)) => label = Some(s.to_string()),
                _ => {}
            }
        }
        assert_eq!(label.as_deref(), Some("Alexa-1"));
    }

    /// 対象は「呼び出しセッションの fabric」（spec: fabric-scoped コマンド）。
    /// ctx.fabric_index の fabric が存在しなければ InvalidFabricIndex(0x0A)。
    #[test]
    fn update_fabric_label_unknown_fabric_returns_invalid_fabric_index() {
        let server = test_server(); // fabric なし
        let fields = encode_update_fabric_label("x");
        let ctx = InvokeCtx {
            fabric_index: 7,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_UPDATE_FABRIC_LABEL,
            &fields,
            &ctx,
        ));
        let (status, _) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0x0A);
    }

    /// Label 長 >32 は INVALID_COMMAND（グローバルステータス）。
    #[test]
    fn update_fabric_label_too_long_returns_invalid_command() {
        let server = commissioned_server();
        let fields = encode_update_fabric_label(&"x".repeat(33));
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let reply = server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_UPDATE_FABRIC_LABEL,
            &fields,
            &ctx,
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_INVALID_COMMAND));
    }

    /// A `FabricPersist` whose `save` can be toggled to fail after
    /// construction — same shape as `fabric_store`'s own `FlakySavePersist`
    /// (that one is private to `fabric_store`'s test module, so this is a
    /// separate copy). `load` always returns empty; tests using this get
    /// their one fabric in via `install_fabric` (which needs `save` to
    /// succeed) before flipping `fail_save` on.
    struct FlakySavePersist {
        fail_save: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl FabricPersist for FlakySavePersist {
        fn save(&self, _entries: &[FabricEntry]) -> Result<(), String> {
            if self.fail_save.load(std::sync::atomic::Ordering::SeqCst) {
                Err("disk full".to_string())
            } else {
                Ok(())
            }
        }
        fn load(&self) -> Result<Vec<FabricEntry>, String> {
            Ok(Vec::new())
        }
    }

    /// UpdateFabricLabel: a persist failure must not be reported as
    /// `NOCResponse(OK)` — the caller has no other signal that the label
    /// change won't survive a restart. `update_label`'s in-memory write
    /// (`FabricStore::update_label` sets the field before attempting the
    /// save) still applies regardless — same asymmetry `FabricStore::remove`
    /// already documents for the fail-safe-rollback path — but the *reply*
    /// must be honest, so this asserts the global `STATUS_FAILURE`, not a
    /// success `NOCResponse`.
    #[test]
    fn update_fabric_label_persist_failure_returns_status_failure() {
        let fail_save = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = FabricStore::with_persist(Box::new(FlakySavePersist {
            fail_save: std::sync::Arc::clone(&fail_save),
        }));
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let mut server = CommissioningServer::new(dev, store);
        install_fabric(&mut server, 0x1122, 0x5001);

        fail_save.store(true, std::sync::atomic::Ordering::SeqCst);
        let fields = encode_update_fabric_label("Alexa-1");
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let reply = server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_UPDATE_FABRIC_LABEL,
            &fields,
            &ctx,
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILURE));
        // In-memory table still got the label — the reply is what must not
        // lie, not the (already-established) in-memory-vs-disk asymmetry.
        assert_eq!(server.fabrics()[0].label, "Alexa-1");
    }

    /// RemoveFabric: NOCResponse(Ok) + store から消え、removed が stage される。
    #[test]
    fn remove_fabric_removes_and_stages_entry() {
        let server = commissioned_server();
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(1),
            &ctx,
        ));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);
        assert_eq!(fabric_index, Some(1));
        assert!(server.fabrics().is_empty());
        assert_eq!(
            server.take_removed_fabric().map(|e| e.fabric_index),
            Some(1)
        );
    }

    /// AddNOC が case admin subject に Administer の自動 ACL エントリを
    /// 発行し、その後の RemoveFabric がそのエントリを purge することを
    /// `set_acl_store` で配線した `AclStore` 越しに検証する
    /// （Task 3 rulings）。あわせて `set_group_key_store` で配線した
    /// `GroupKeyStore` の KeySet/GroupKeyMap も同じ RemoveFabric で
    /// purge されることを確認する（Task 2、`handle_remove_fabric`の
    /// 成功径路 = purge 3箇所のうちの1つ）。`set_group_membership_store`
    /// で配線した `GroupMembershipStore` の membership も同じ経路で purge
    /// される（groupcast レーン A フェーズ 2 Task 2）。
    #[test]
    fn add_noc_installs_case_admin_acl_and_remove_fabric_purges_it() {
        use crate::core::access_control::{decode_entries_for_test, AccessControlHandler};

        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let mut server = CommissioningServer::new(dev, FabricStore::new());
        let acl_store = AclStore::new();
        server.set_acl_store(acl_store.clone());
        let gk_store = GroupKeyStore::new();
        server.set_group_key_store(gk_store.clone());
        let membership = GroupMembershipStore::new();
        server.set_group_membership_store(membership.clone());

        // install_fabric drives ArmFailSafe/CSR/AddTrustedRoot/AddNOC with
        // admin subject fixed at 0xAA (see its doc comment).
        install_fabric(&mut server, 0x1122, 0x5001);

        let handler = AccessControlHandler::new(acl_store.clone());
        let entries = decode_entries_for_test(&handler.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert_eq!(entries, vec![(5u8, 2u8, vec![0xAAu64], 1u8)]);

        // GroupKeyStore side of the same fabric: a KeySet and a GroupKeyMap
        // entry, both scoped to fabric_index 1.
        gk_store.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();
        gk_store.replace_fabric_map(1, vec![(0x000A, 7)]);
        assert!(gk_store.keyset_exists(1, 7));
        assert_eq!(gk_store.map_entries_for(1), vec![(0x000A, 7)]);

        // GroupMembershipStore side of the same fabric.
        membership.add(1, 10, 2).unwrap();

        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(1),
            &ctx,
        ));
        let (status, _) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);

        let entries = decode_entries_for_test(&handler.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert!(entries.is_empty());
        assert!(!gk_store.keyset_exists(1, 7));
        assert!(gk_store.map_entries_for(1).is_empty());
        assert!(
            membership.groups_by_fabric().is_empty(),
            "purge must drop the fabric's memberships"
        );
    }

    /// 存在しない index は InvalidFabricIndex(0x0A)。
    #[test]
    fn remove_fabric_unknown_index_returns_invalid_fabric_index() {
        let server = commissioned_server();
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(9),
            &ctx,
        ));
        let (status, _) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0x0A);
        assert_eq!(server.fabrics().len(), 1);
    }

    /// RemoveFabric: a persist failure must still stage `removed_fabric`
    /// for the runtime — `FabricStore::remove`'s in-memory removal already
    /// happened (same asymmetry `update_fabric_label_persist_failure_
    /// returns_status_failure` documents for `update_label`), so the mDNS
    /// retract and (if it were this session's own fabric) session drop
    /// must follow that in-memory truth, not the reply. The reply itself
    /// still stays honest (`STATUS_FAILURE`, not a success `NOCResponse`).
    #[test]
    fn remove_fabric_persist_failure_still_stages_removal() {
        let fail_save = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = FabricStore::with_persist(Box::new(FlakySavePersist {
            fail_save: std::sync::Arc::clone(&fail_save),
        }));
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let mut server = CommissioningServer::new(dev, store);
        install_fabric(&mut server, 0x1122, 0x5001);

        fail_save.store(true, std::sync::atomic::Ordering::SeqCst);
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let reply = server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(1),
            &ctx,
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILURE));
        assert!(server.fabrics().is_empty());
        assert_eq!(
            server.take_removed_fabric().map(|e| e.fabric_index),
            Some(1)
        );
    }

    /// RemoveFabric: a persist failure must still purge the removed
    /// fabric's ACL entries — `FabricStore::remove`'s in-memory removal
    /// already happened (same asymmetry the sibling
    /// `remove_fabric_persist_failure_still_stages_removal` test documents
    /// for `removed_fabric`), and `next_fabric_index` reissues a removed
    /// index as `max(existing)+1` — an unpurged entry here would let a
    /// later `AddNOC` at that same index inherit the previous occupant's
    /// ACL (cross-fabric leak). Same reasoning applies to `GroupKeyStore`
    /// (Task 2's purge site, `handle_remove_fabric`'s error branch) and to
    /// `GroupMembershipStore` (groupcast レーン A フェーズ 2 Task 2).
    #[test]
    fn remove_fabric_persist_failure_still_purges_acl() {
        use crate::core::access_control::{decode_entries_for_test, AccessControlHandler};

        let fail_save = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = FabricStore::with_persist(Box::new(FlakySavePersist {
            fail_save: std::sync::Arc::clone(&fail_save),
        }));
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let mut server = CommissioningServer::new(dev, store);
        let acl_store = AclStore::new();
        server.set_acl_store(acl_store.clone());
        let gk_store = GroupKeyStore::new();
        server.set_group_key_store(gk_store.clone());
        let membership = GroupMembershipStore::new();
        server.set_group_membership_store(membership.clone());
        install_fabric(&mut server, 0x1122, 0x5001);

        let handler = AccessControlHandler::new(acl_store.clone());
        assert_eq!(
            decode_entries_for_test(&handler.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).len(),
            1
        );
        gk_store.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();
        gk_store.replace_fabric_map(1, vec![(0x000A, 7)]);
        membership.add(1, 10, 2).unwrap();

        fail_save.store(true, std::sync::atomic::Ordering::SeqCst);
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let reply = server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(1),
            &ctx,
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILURE));
        assert!(
            decode_entries_for_test(&handler.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).is_empty()
        );
        assert!(!gk_store.keyset_exists(1, 7));
        assert!(gk_store.map_entries_for(1).is_empty());
        assert!(
            membership.groups_by_fabric().is_empty(),
            "purge must drop the fabric's memberships"
        );
    }

    #[test]
    fn gc_serves_basic_commissioning_info() {
        let server = test_server();
        let (gc, ..) = server.into_cluster_handlers();
        let tlv = gc
            .read(ATTR_GC_BASIC_COMMISSIONING_INFO, &ReadCtx::default())
            .expect("BasicCommissioningInfo");

        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut expiry = None;
        let mut max_cumulative = None;
        loop {
            let el = r.next().unwrap().expect("truncated BasicCommissioningInfo");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => expiry = Some(v),
                (Tag::Context(1), Value::Uint(v)) => max_cumulative = Some(v),
                _ => {}
            }
        }
        assert_eq!(expiry, Some(60));
        assert_eq!(max_cumulative, Some(900));
    }

    /// AcceptedCommandList/GeneratedCommandList (spec §7.13) for the three
    /// commissioning clusters must name their real command sets —
    /// conformance-checking controllers (Apple Home) read them during the
    /// post-commissioning interview.
    #[test]
    fn commissioning_handlers_declare_their_command_lists() {
        let server = test_server();
        let (gc, oc, ac) = server.into_cluster_handlers();

        assert_eq!(
            gc.accepted_commands(),
            vec![
                CMD_ARM_FAIL_SAFE,
                CMD_SET_REGULATORY_CONFIG,
                CMD_COMMISSIONING_COMPLETE
            ]
        );
        assert_eq!(
            gc.generated_commands(),
            vec![
                RESP_ARM_FAIL_SAFE,
                RESP_SET_REGULATORY_CONFIG,
                RESP_COMMISSIONING_COMPLETE
            ]
        );

        assert_eq!(
            oc.accepted_commands(),
            vec![
                CMD_ATTESTATION_REQUEST,
                CMD_CERT_CHAIN_REQUEST,
                CMD_CSR_REQUEST,
                CMD_ADD_NOC,
                CMD_UPDATE_FABRIC_LABEL,
                CMD_REMOVE_FABRIC,
                CMD_ADD_TRUSTED_ROOT
            ]
        );
        assert_eq!(
            oc.generated_commands(),
            vec![RESP_ATTESTATION, RESP_CERT_CHAIN, RESP_CSR, RESP_NOC]
        );

        assert_eq!(
            ac.accepted_commands(),
            vec![CMD_OPEN_COMMISSIONING_WINDOW, CMD_REVOKE_COMMISSIONING]
        );
        assert_eq!(ac.generated_commands(), Vec::<u32>::new());
    }

    #[test]
    fn gc_serves_other_scalar_attributes() {
        let server = test_server();
        let (gc, ..) = server.into_cluster_handlers();

        let breadcrumb = gc
            .read(ATTR_GC_BREADCRUMB, &ReadCtx::default())
            .expect("Breadcrumb");
        let mut r = Reader::new(&breadcrumb);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0));

        let regulatory = gc
            .read(ATTR_GC_REGULATORY_CONFIG, &ReadCtx::default())
            .expect("RegulatoryConfig");
        let mut r = Reader::new(&regulatory);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0));

        let location = gc
            .read(ATTR_GC_LOCATION_CAPABILITY, &ReadCtx::default())
            .expect("LocationCapability");
        let mut r = Reader::new(&location);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(2));

        let concurrent = gc
            .read(ATTR_GC_SUPPORTS_CONCURRENT_CONNECTION, &ReadCtx::default())
            .expect("SupportsConcurrentConnection");
        let mut r = Reader::new(&concurrent);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Bool(true));
    }

    #[test]
    fn oc_fabrics_and_nocs_reflect_installed_fabric() {
        let server = commissioned_server(); // fabric_id=0x1122, node=0x5001, admin_vendor_id=0xFFF1
        let (_, oc, _) = server.into_cluster_handlers();

        // NOCs(0): array[ struct{1: noc_tlv, 2: icac_tlv?, 254: fabric_index} ]
        let nocs_tlv = oc
            .read(ATTR_OC_NOCS, &ReadCtx::unfiltered(0))
            .expect("NOCs");
        let mut r = Reader::new(&nocs_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut noc_tlv = None;
        let mut fabric_index = None;
        loop {
            let el = r.next().unwrap().expect("truncated NOC struct");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(1), Value::Bytes(b)) => noc_tlv = Some(b.to_vec()),
                (Tag::Context(254), Value::Uint(v)) => fabric_index = Some(v),
                _ => {}
            }
        }
        assert!(noc_tlv.is_some());
        assert_eq!(fabric_index, Some(1));

        // Fabrics(1): array[ struct{1: root_public_key, 2: admin_vendor_id,
        // 3: fabric_id, 4: node_id, 5: label, 254: fabric_index} ]
        let fabrics_tlv = oc
            .read(ATTR_OC_FABRICS, &ReadCtx::unfiltered(0))
            .expect("Fabrics");
        let mut r = Reader::new(&fabrics_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut admin_vendor_id = None;
        let mut fabric_id = None;
        let mut node_id = None;
        let mut fidx = None;
        loop {
            let el = r
                .next()
                .unwrap()
                .expect("truncated FabricDescriptor struct");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(2), Value::Uint(v)) => admin_vendor_id = Some(v),
                (Tag::Context(3), Value::Uint(v)) => fabric_id = Some(v),
                (Tag::Context(4), Value::Uint(v)) => node_id = Some(v),
                (Tag::Context(254), Value::Uint(v)) => fidx = Some(v),
                _ => {}
            }
        }
        assert_eq!(admin_vendor_id, Some(0xFFF1));
        assert_eq!(fabric_id, Some(0x1122));
        assert_eq!(node_id, Some(0x5001));
        assert_eq!(fidx, Some(1));

        // SupportedFabrics / CommissionedFabrics are plain scalars.
        let supported = oc
            .read(ATTR_OC_SUPPORTED_FABRICS, &ReadCtx::default())
            .expect("SupportedFabrics");
        let mut r = Reader::new(&supported);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(5));

        let commissioned = oc
            .read(ATTR_OC_COMMISSIONED_FABRICS, &ReadCtx::default())
            .expect("CommissionedFabrics");
        let mut r = Reader::new(&commissioned);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));

        // TrustedRootCertificates(4): array[ bytes(root_tlv) ]
        let roots_tlv = oc
            .read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &ReadCtx::unfiltered(0))
            .expect("TrustedRootCertificates");
        let mut r = Reader::new(&roots_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let el = r.next().unwrap().expect("one root cert");
        assert!(matches!(el.value, Value::Bytes(_)));
    }

    /// `CurrentFabricIndex` (spec §11.17.5.3) echoes back the *reading
    /// session's* fabric index from `ReadCtx`, not anything derived from
    /// the fabric table — it's session-scoped, so two different sessions
    /// against the same installed fabric would report their own selected
    /// index (M2 has one fabric, but the attribute's whole point is not
    /// assuming that).
    #[test]
    fn oc_current_fabric_index_reflects_read_ctx() {
        let server = commissioned_server();
        let (_, oc, _) = server.into_cluster_handlers();

        let tlv = oc
            .read(ATTR_OC_CURRENT_FABRIC_INDEX, &read_ctx(1))
            .expect("CurrentFabricIndex");
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
    }

    /// Reads every `FabricIndex` (context tag 254) of an
    /// `array[ struct{ …, 254: FabricIndex } ]` attribute straight off the
    /// encoded bytes with the TLV `Reader` — no decode helper in between,
    /// so the assertion is about what actually goes on the wire. Returns one
    /// entry per array element (and panics if an element carries none, which
    /// would itself be a spec violation for a fabric-scoped list).
    fn fabric_indices_of_list(tlv: &[u8]) -> Vec<u64> {
        let mut r = Reader::new(tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let mut out = Vec::new();
        loop {
            match r.next().unwrap().expect("truncated list").value {
                Value::ContainerEnd => break, // end of the array itself
                Value::StructStart => {}
                other => panic!("unexpected array element: {other:?}"),
            }
            let mut fabric_index = None;
            loop {
                let el = r.next().unwrap().expect("truncated list entry struct");
                match (el.tag, el.value) {
                    (_, Value::ContainerEnd) => break,
                    (Tag::Context(254), Value::Uint(v)) => fabric_index = Some(v),
                    _ => {}
                }
            }
            out.push(fabric_index.expect("list entry without FabricIndex(254)"));
        }
        out
    }

    /// Number of elements in an `array[ bytes ]` attribute
    /// (`TrustedRootCertificates`), read off the encoded bytes directly —
    /// its entries carry no `FabricIndex`, so the count is all there is to
    /// assert.
    fn byte_list_len(tlv: &[u8]) -> usize {
        let mut r = Reader::new(tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let mut n = 0;
        loop {
            match r.next().unwrap().expect("truncated byte list").value {
                Value::ContainerEnd => break,
                Value::Bytes(_) => n += 1,
                other => panic!("unexpected array element: {other:?}"),
            }
        }
        n
    }

    /// spec §11.17.5 / §8.9.2.4: `NOCs`/`Fabrics`/`TrustedRootCertificates`
    /// are fabric-scoped lists, so a read with `IsFabricFiltered=true` must
    /// only see the accessing fabric's own entry — anything else leaks one
    /// commissioner's credentials to another (Apple alone establishes two
    /// fabrics in practice). A PASE session (fabric index 0, no fabric yet)
    /// sees an empty list. `IsFabricFiltered=false` keeps the unfiltered
    /// view.
    #[test]
    fn oc_reads_are_fabric_scoped_when_fabric_filtered() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001); // fabric_index 1
        install_fabric(&mut server, 0x3344, 0x5002); // fabric_index 2
        assert_eq!(server.fabrics().len(), 2);
        let (_, oc, _) = server.into_cluster_handlers();

        let read = |attribute: u32, ctx: &ReadCtx| oc.read(attribute, ctx).expect("OC attribute");

        for accessing in [1u8, 2] {
            let ctx = ReadCtx {
                fabric_index: accessing,
                fabric_filtered: true,
                ..ReadCtx::default()
            };
            assert_eq!(
                fabric_indices_of_list(&read(ATTR_OC_NOCS, &ctx)),
                vec![u64::from(accessing)],
                "NOCs must only carry fabric {accessing}'s entry"
            );
            assert_eq!(
                fabric_indices_of_list(&read(ATTR_OC_FABRICS, &ctx)),
                vec![u64::from(accessing)],
                "Fabrics must only carry fabric {accessing}'s entry"
            );
            assert_eq!(
                byte_list_len(&read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &ctx)),
                1,
                "TrustedRootCertificates must only carry fabric {accessing}'s root"
            );
        }

        // PASE (fabric index 0 — never a valid fabric index): nothing matches.
        let pase = ReadCtx {
            fabric_index: 0,
            fabric_filtered: true,
            ..ReadCtx::default()
        };
        assert!(fabric_indices_of_list(&read(ATTR_OC_NOCS, &pase)).is_empty());
        assert!(fabric_indices_of_list(&read(ATTR_OC_FABRICS, &pase)).is_empty());
        assert_eq!(
            byte_list_len(&read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &pase)),
            0
        );

        // IsFabricFiltered=false: unfiltered, as before (regression guard).
        let unfiltered = ReadCtx {
            fabric_index: 1,
            fabric_filtered: false,
            ..ReadCtx::default()
        };
        assert_eq!(
            fabric_indices_of_list(&read(ATTR_OC_NOCS, &unfiltered)),
            vec![1, 2]
        );
        assert_eq!(
            fabric_indices_of_list(&read(ATTR_OC_FABRICS, &unfiltered)),
            vec![1, 2]
        );
        assert_eq!(
            byte_list_len(&read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &unfiltered)),
            2
        );

        // Scalars stay unfiltered either way (spec: not fabric-scoped).
        for ctx in [&pase, &unfiltered] {
            let commissioned = read(ATTR_OC_COMMISSIONED_FABRICS, ctx);
            let mut r = Reader::new(&commissioned);
            assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(2));
            let supported = read(ATTR_OC_SUPPORTED_FABRICS, ctx);
            let mut r = Reader::new(&supported);
            assert_eq!(
                r.next().unwrap().unwrap().value,
                Value::Uint(u64::from(SUPPORTED_FABRICS))
            );
        }
    }

    /// spec §11.17.5 のアクセス表: `NOCs` の read は Administer — Operate
    /// までしか持たない subject には `STATUS_UNSUPPORTED_ACCESS` が
    /// per-entry status で返り、Administer を持つ subject には通常どおり
    /// data が返る（`access_control.rs` の
    /// `acl_read_privilege_is_per_attribute_and_global_attributes_stay_view`
    /// と同じ検証手法を `Node`/ACL 経由で行う）。
    #[test]
    fn oc_nocs_read_requires_administer() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001); // fabric_index 1
        let (_, oc, _) = server.into_cluster_handlers();

        let acl = AclStore::new();
        acl.set_entries_for_test(
            1,
            vec![
                crate::core::access_control::AclDeviceEntry {
                    privilege: crate::core::access_control::PRIVILEGE_OPERATE,
                    auth_mode: crate::core::access_control::AUTH_MODE_CASE,
                    subjects: vec![7],
                    targets_raw: None,
                    fabric_index: 1,
                },
                crate::core::access_control::AclDeviceEntry {
                    privilege: crate::core::access_control::PRIVILEGE_ADMINISTER,
                    auth_mode: crate::core::access_control::AUTH_MODE_CASE,
                    subjects: vec![9],
                    targets_raw: None,
                    fabric_index: 1,
                },
            ],
        );

        let mut node = crate::core::datamodel::Node::new();
        node.add_endpoint(0, vec![oc]);
        node.set_acl_store(acl);

        let nocs_path = [im::AttrPathIn {
            endpoint: Some(0),
            cluster: Some(CLUSTER_OPERATIONAL_CREDENTIALS),
            attribute: Some(ATTR_OC_NOCS),
        }];

        // Operate だけの subject: per-entry UNSUPPORTED_ACCESS status。
        let operate_ctx = ReadCtx {
            fabric_index: 1,
            subject: crate::core::access_control::Subject::node(7),
            ..ReadCtx::default()
        };
        let entries = node.read_entries(&nocs_path, &operate_ctx);
        assert!(matches!(
            &entries[..],
            [im::ReportEntryOut::Status { status, .. }]
                if *status == im::STATUS_UNSUPPORTED_ACCESS
        ));

        // Administer を持つ subject: data が返る。
        let admin_ctx = ReadCtx {
            fabric_index: 1,
            subject: crate::core::access_control::Subject::node(9),
            ..ReadCtx::default()
        };
        let entries = node.read_entries(&nocs_path, &admin_ctx);
        assert!(matches!(&entries[..], [im::ReportEntryOut::Data(_)]));
    }

    /// spec §11.17.5.2: `SupportedFabrics` is the device's capacity, and
    /// `AddNOC` past it must answer `NOCResponse(TableFull=5)` rather than
    /// installing a fabric the attribute says can't exist.
    #[test]
    fn add_noc_rejects_sixth_fabric_with_table_full() {
        let mut server = test_server();
        for i in 0..u64::from(SUPPORTED_FABRICS) {
            let (_, resp) = expect_data(install_fabric(&mut server, 0x1122 + i, 0x5001 + i));
            let (status, _) = decode_noc_response(&resp).unwrap();
            assert_eq!(status, NOC_STATUS_OK, "fabric {} must install", i + 1);
        }
        assert_eq!(server.fabrics().len(), usize::from(SUPPORTED_FABRICS));

        let (response_command, resp) = expect_data(install_fabric(&mut server, 0x9999, 0x5999));
        assert_eq!(response_command, RESP_NOC);
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, NOC_STATUS_TABLE_FULL);
        assert_eq!(fabric_index, None);
        assert_eq!(
            server.fabrics().len(),
            usize::from(SUPPORTED_FABRICS),
            "a rejected AddNOC must not install anything"
        );
    }

    /// spec §11.17.6.8.1: `CaseAdminSubject` は operational node id か
    /// CAT（version ≠ 0）でなければ `NOCResponse(InvalidAdminSubject=0x0B)`。
    /// fabric も ACL エントリも作られず、pending（CSR/root）は残るので
    /// 同じセッションで正しい subject の AddNOC をやり直せる。
    #[test]
    fn add_noc_rejects_invalid_case_admin_subject_and_allows_retry() {
        use crate::core::access_control::{
            cat_subject, AclStore, Subject, OPERATIONAL_NODE_ID_MAX, PRIVILEGE_ADMINISTER,
        };
        for bad in [
            0u64,
            cat_subject(0xABCD_0000),    // CAT version 0
            OPERATIONAL_NODE_ID_MAX + 1, // 予約域の先頭
            0xFFFF_FFFF_FFFF_0001,       // group 域
        ] {
            let mut server = test_server();
            let acl_store = AclStore::new();
            server.set_acl_store(acl_store.clone());

            let (reply, noc, fabric) = install_fabric_with_admin(&mut server, 0x1122, 0x5001, bad);
            let (response_command, resp) = expect_data(reply);
            assert_eq!(response_command, RESP_NOC);
            let (status, fabric_index) = decode_noc_response(&resp).unwrap();
            assert_eq!(status, NOC_STATUS_INVALID_ADMIN_SUBJECT, "subject {bad:#x}");
            assert_eq!(fabric_index, None);
            assert!(
                server.fabrics().is_empty(),
                "rejected AddNOC must not install a fabric"
            );
            assert!(
                !acl_store.check(1, Subject::node(0x5001), PRIVILEGE_ADMINISTER, 0, 0x001F),
                "rejected AddNOC must not add an admin ACL entry"
            );

            // 同じ pending（CSR keypair / trusted root）で正しい subject なら通る。
            let (_, resp) = expect_data(drive_invoke(
                &mut server,
                CLUSTER_OPERATIONAL_CREDENTIALS,
                CMD_ADD_NOC,
                &encode_add_noc(&noc, &fabric.ipk_epoch, 0xAA, 0xFFF1),
            ));
            let (status, fabric_index) = decode_noc_response(&resp).unwrap();
            assert_eq!(status, NOC_STATUS_OK, "retry after {bad:#x}");
            assert_eq!(fabric_index, Some(1));
            assert_eq!(server.fabrics().len(), 1);
        }
    }

    /// CAT 形の admin subject（Apple Home が送る形）は version ≠ 0 なら受理。
    #[test]
    fn add_noc_accepts_cat_case_admin_subject() {
        use crate::core::access_control::cat_subject;
        let mut server = test_server();
        let (reply, _, _) =
            install_fabric_with_admin(&mut server, 0x1122, 0x5001, cat_subject(0xABCD_0002));
        let (_, resp) = expect_data(reply);
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, NOC_STATUS_OK);
        assert_eq!(fabric_index, Some(1));
    }

    #[test]
    fn arm_fail_safe_response_roundtrips() {
        let mut server = test_server();
        let (response_command, fields_tlv) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 7),
        ));
        assert_eq!(response_command, RESP_ARM_FAIL_SAFE);
        let (code, _text) = decode_commissioning_status_response(&fields_tlv).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn attestation_response_passes_verify_device_attestation() {
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let (dac_der, pai_der, paa_der) = (
            dev.dac_der.clone(),
            dev.pai_der.clone(),
            dev.paa_der.clone(),
        );
        let mut server = CommissioningServer::new(dev, FabricStore::new());
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );

        let nonce = [9u8; 32];
        let (response_command, fields_tlv) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ATTESTATION_REQUEST,
            &encode_attestation_request(&nonce),
        ));
        assert_eq!(response_command, RESP_ATTESTATION);
        let (elements, signature) = decode_attestation_response(&fields_tlv).unwrap();

        verify_device_attestation(
            &dac_der,
            &pai_der,
            std::slice::from_ref(&paa_der),
            &[],
            &elements,
            &signature,
            &nonce,
            &TEST_CHALLENGE,
        )
        .unwrap();
    }

    #[test]
    fn cert_chain_request_returns_dac_and_pai_der() {
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let (dac_der, pai_der) = (dev.dac_der.clone(), dev.pai_der.clone());
        let mut server = CommissioningServer::new(dev, FabricStore::new());

        let (_, resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            &encode_cert_chain_request(CERT_TYPE_DAC),
        ));
        assert_eq!(
            mat_controller::commissioning::decode_cert_chain_response(&resp).unwrap(),
            dac_der
        );

        let (_, resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            &encode_cert_chain_request(CERT_TYPE_PAI),
        ));
        assert_eq!(
            mat_controller::commissioning::decode_cert_chain_response(&resp).unwrap(),
            pai_der
        );
    }

    #[test]
    fn add_noc_rejected_without_fail_safe() {
        // AddNOC checks `is_armed()` before decoding its fields at all, so
        // a never-armed server rejects it outright — no valid CSR/NOC is
        // even reachable without a fail-safe window (CSRRequest/
        // AddTrustedRootCertificate are gated too, see
        // `csr_request_rejected_without_fail_safe` below).
        let mut server = test_server();
        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &[],
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
        assert!(server.fabrics().is_empty());
    }

    #[test]
    fn csr_request_rejected_without_fail_safe() {
        let mut server = test_server();
        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[1u8; 32]),
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
        assert!(server.pending_is_empty());
    }

    #[test]
    fn attestation_request_rejected_without_fail_safe() {
        let mut server = test_server();
        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ATTESTATION_REQUEST,
            &encode_attestation_request(&[1u8; 32]),
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
    }

    #[test]
    fn add_trusted_root_rejected_without_fail_safe() {
        let mut server = test_server();
        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_TRUSTED_ROOT,
            &encode_add_trusted_root(b"rcac"),
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
        assert!(server.pending_is_empty());
    }

    #[test]
    fn disarm_clears_pending_commissioning_state() {
        let mut server = test_server();
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[1u8; 32]),
        );
        assert!(!server.pending_is_empty());

        // ArmFailSafe(expiry=0) disarms early (spec-legal, spec §11.10.7.2.1)
        // — the CSR keypair staged above must not survive it.
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 2),
        );
        assert!(server.pending_is_empty());
    }

    /// 同一窓内の再アームは同じ試行の延長なので staged CSR を維持する
    /// （matter.js の BLE フローは CSR と AddNOC の間でも定期再アームする、
    /// spec §11.10.6.2）。窓をまたいだ fresh arm（disarm 後の新規試行）では
    /// 前の試行の CSR keypair を持ち越してはいけない。
    #[test]
    fn rearm_keeps_pending_within_window_but_fresh_arm_resets_it() {
        let mut server = test_server();
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[1u8; 32]),
        );
        assert!(!server.pending_is_empty());

        // 同一窓内の再アーム: staged CSR は生きたまま
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 2),
        );
        assert!(!server.pending_is_empty());

        // disarm → fresh arm: 新規試行に前の CSR を持ち越さない
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 3),
        );
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 4),
        );
        assert!(server.pending_is_empty());
    }

    #[test]
    fn commissioning_complete_clears_pending_commissioning_state() {
        let mut server = test_server();
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[1u8; 32]),
        );
        assert!(!server.pending_is_empty());
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            &[],
        );
        assert!(server.pending_is_empty());
    }

    #[test]
    fn commissioning_complete_disarms_fail_safe_so_add_noc_is_then_rejected() {
        let mut server = test_server();
        let fabric = CommissioningFabric::generate(0x1122, 0xAA).unwrap();

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        let (_, csr_resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[2u8; 32]),
        ));
        let (elements, _) = decode_csr_response(&csr_resp).unwrap();
        let (csr_der, _) = parse_nocsr_elements(&elements).unwrap();
        let device_pub = parse_csr(&csr_der).unwrap();
        let noc = fabric.issue_device_noc(&device_pub, 0x5001).unwrap();
        drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_TRUSTED_ROOT,
            &encode_add_trusted_root(&fabric.rcac_tlv),
        );

        // CommissioningComplete disarms early — before AddNOC.
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            &[],
        );

        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &encode_add_noc(&noc, &fabric.ipk_epoch, 0xAA, 0xFFF1),
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
    }

    #[test]
    fn fail_safe_deadline_reflects_armed_state() {
        let mut server = test_server();
        assert!(server.fail_safe_deadline().is_none());

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        assert!(server.fail_safe_deadline().is_some());

        // ExpiryLengthSeconds=0 disarms early (spec §11.10.7.2.1).
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 2),
        );
        assert!(server.fail_safe_deadline().is_none());
    }

    #[test]
    fn fail_safe_expiry_rolls_back_uncommitted_fabric() {
        let (mut server, acl, gk, membership) = server_with_stores();
        install_fabric(&mut server, 0x1122, 0x5001);
        assert_eq!(server.fabrics().len(), 1);
        assert!(admin_allowed(&acl), "AddNOC installs the admin ACL entry");
        seed_fabric_state(&gk, &membership);

        server.force_expire_fail_safe();
        let removed = server.expire_fail_safe();
        assert_eq!(removed.map(|e| e.fabric_index), Some(1));
        assert!(server.fabrics().is_empty());
        assert!(server.fail_safe_deadline().is_none());
        assert_fabric_state_purged(&acl, &gk, &membership);

        // Idempotent: the marker and the timer are both already cleared.
        assert!(server.expire_fail_safe().is_none());
    }

    /// 早期 disarm（`ArmFailSafe(0)`）も満了と同じ rollback 経路: 未確定
    /// fabric と 3 store の状態が消える。
    #[test]
    fn arm_fail_safe_zero_rolls_back_uncommitted_fabric_and_purges_stores() {
        let (mut server, acl, gk, membership) = server_with_stores();
        install_fabric(&mut server, 0x1122, 0x5001);
        seed_fabric_state(&gk, &membership);

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 3),
        );
        assert!(server.fabrics().is_empty());
        assert!(server.fail_safe_deadline().is_none());
        assert_fabric_state_purged(&acl, &gk, &membership);
    }

    #[test]
    fn commissioning_complete_commits_the_fabric() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001);
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            &[],
        );

        // CommissioningComplete already disarmed, so there's no deadline to
        // expire — but even if there were, the fabric is confirmed, not
        // rolled back.
        assert!(server.expire_fail_safe().is_none());
        assert_eq!(server.fabrics().len(), 1);
    }

    /// spec §11.10.6.2: armed 中の非ゼロ ArmFailSafe は**タイマー延長**で
    /// あって新規試行の開始ではない。未確定 fabric を巻き戻してはいけない。
    /// 実測: matter.js（HA matter-server 1.1.7）は AddNOC 成功後に
    /// fail-safe を再アームしてから CASE を張り直し、その上で
    /// CommissioningComplete を送る。旧実装は再アームで fabric を
    /// 巻き戻してしまい、直後の Sigma1 が「destination id matched no
    /// fabric」で永遠に失敗していた（2026-08-18）。zombie fabric 対策は
    /// 満了時（`fail_safe_expiry_rolls_back_uncommitted_fabric`）と
    /// disarm 時の rollback が引き続き担保する。
    #[test]
    fn rearm_keeps_uncommitted_fabric_and_complete_commits_it() {
        let (mut server, acl, gk, membership) = server_with_stores();
        install_fabric(&mut server, 0x1122, 0x5001);
        seed_fabric_state(&gk, &membership);

        // AddNOC 後の再アーム（matter.js の Reconnect ステップ相当）
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(313, 2),
        );
        assert_eq!(
            server.fabrics().len(),
            1,
            "re-arm while armed must not roll back the pending fabric"
        );
        assert!(admin_allowed(&acl));
        assert!(gk.keyset_exists(1, 7));
        assert_eq!(membership.endpoints_for(1, 0x000A), vec![2]);

        // CASE 再接続後の CommissioningComplete で確定する
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            &[],
        );
        assert!(server.expire_fail_safe().is_none());
        assert_eq!(server.fabrics().len(), 1);
    }

    /// 早期 disarm（expiry=0）は従来どおり未確定 fabric を巻き戻し、
    /// 解放された index は次の試行で再利用される。
    #[test]
    fn early_disarm_rolls_back_uncommitted_fabric() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001);
        assert_eq!(server.fabrics().len(), 1);

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 2),
        );
        assert!(server.fabrics().is_empty());

        // The freed index (1) must be reusable, not skipped.
        let (_, resp) = expect_data(install_fabric(&mut server, 0x3344, 0x6002));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);
        assert_eq!(fabric_index, Some(1));
    }

    #[test]
    fn unknown_command_and_cluster_are_rejected() {
        let mut server = test_server();
        assert_eq!(
            drive_invoke(&mut server, CLUSTER_GENERAL_COMMISSIONING, 0x7F, &[]),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
        assert_eq!(
            drive_invoke(&mut server, 0x9999, CMD_ARM_FAIL_SAFE, &[]),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_CLUSTER)
        );
    }

    /// OCW 成功: WindowStatus=1、Admin 属性が呼び出し元 fabric を反映、
    /// WindowRequest が stage される。
    #[test]
    fn open_commissioning_window_stages_request_and_updates_attrs() {
        let server = commissioned_server(); // fabric_index=1 が入っている既存ヘルパ
        let material = [0x42u8; 97];
        let fields = encode_open_commissioning_window(300, &material, 0x0ABC, 1000, &[0x5A; 16]);
        let reply = server.invoke_command(
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            &fields,
            &InvokeCtx {
                fabric_index: 1,
                ..test_ctx()
            },
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_SUCCESS));
        let req = server.take_pending_window_request().expect("staged");
        assert_eq!(req.discriminator, 0x0ABC);
        assert_eq!(req.timeout_s, 300);
        assert_eq!(req.verifier, material);
        // 属性: WindowStatus=1(EnhancedWindowOpen), AdminFabricIndex=1,
        // AdminVendorId=登録済み fabric の admin_vendor_id(0xFFF1)
        let (_, _, ac) = server.into_cluster_handlers();
        let tlv = ac.read(ATTR_AC_WINDOW_STATUS, &ReadCtx::default()).unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
        let tlv = ac
            .read(ATTR_AC_ADMIN_FABRIC_INDEX, &ReadCtx::default())
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
        let tlv = ac
            .read(ATTR_AC_ADMIN_VENDOR_ID, &ReadCtx::default())
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0xFFF1));
    }

    /// 窓が既に開いていれば Busy(2)。
    #[test]
    fn open_commissioning_window_while_open_returns_busy() {
        let server = commissioned_server();
        let fields = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        server.invoke_command(
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            &fields,
            &ctx,
        );
        let reply = server.invoke_command(
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            &fields,
            &ctx,
        );
        assert_eq!(
            reply,
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 2
            }
        );
    }

    /// パラメータ検証: verifier 長 ≠97 / iterations 範囲外(1000..=100000) /
    /// salt 長範囲外(16..=32) は PAKEParameterError(3)。timeout 範囲外
    /// (180..=900) は INVALID_COMMAND。
    #[test]
    fn open_commissioning_window_rejects_bad_parameters() {
        let server = commissioned_server();
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let bad_iter = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 999, &[0x5A; 16]);
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_OPEN_COMMISSIONING_WINDOW,
                &bad_iter,
                &ctx
            ),
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 3
            }
        );
        let bad_salt = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 8]);
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_OPEN_COMMISSIONING_WINDOW,
                &bad_salt,
                &ctx
            ),
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 3
            }
        );
        let bad_timeout =
            encode_open_commissioning_window(60, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_OPEN_COMMISSIONING_WINDOW,
                &bad_timeout,
                &ctx
            ),
            InvokeReply::Status(im::STATUS_INVALID_COMMAND)
        );

        // `encode_open_commissioning_window` takes `verifier: &[u8; 97]`, so
        // a wrong-length verifier can't be produced through it — build the
        // fields TLV directly (same technique as
        // `add_noc_accepts_empty_icac_value_as_absent`) with a 96-byte
        // verifier to exercise the third PAKEParameterError disjunct
        // (verifier.len() != 97) independently of iterations/salt.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 300);
        w.put_bytes(Tag::Context(1), &[0x42; 96]);
        w.put_uint(Tag::Context(2), 0x0ABC);
        w.put_uint(Tag::Context(3), 1000);
        w.put_bytes(Tag::Context(4), &[0x5A; 16]);
        w.end_container();
        let bad_verifier = w.finish();
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_OPEN_COMMISSIONING_WINDOW,
                &bad_verifier,
                &ctx
            ),
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 3
            }
        );
    }

    /// Revoke: 開いていれば閉じ、閉じていれば WindowNotOpen(4)。
    #[test]
    fn revoke_commissioning_closes_or_rejects() {
        let server = commissioned_server();
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_REVOKE_COMMISSIONING,
                &[],
                &ctx
            ),
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 4
            }
        );
        let fields = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
        server.invoke_command(
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            &fields,
            &ctx,
        );
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_REVOKE_COMMISSIONING,
                &[],
                &ctx
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        // 閉じた後の属性は WindowStatus=0 / Admin* は null
        let (_, _, ac) = server.into_cluster_handlers();
        let tlv = ac.read(ATTR_AC_WINDOW_STATUS, &ReadCtx::default()).unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0));
        let tlv = ac
            .read(ATTR_AC_ADMIN_FABRIC_INDEX, &ReadCtx::default())
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Null);
        let tlv = ac
            .read(ATTR_AC_ADMIN_VENDOR_ID, &ReadCtx::default())
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Null);
    }

    /// End-to-end proof that `into_cluster_handlers` wires all three
    /// clusters into the same shared state through real `Node`/IM wire
    /// framing (not just the direct `invoke_command` shortcut the tests
    /// above use).
    #[test]
    fn wired_into_node_dispatches_both_clusters() {
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let server = CommissioningServer::new(dev, FabricStore::new());
        let (gc, oc, ac) = server.into_cluster_handlers();
        let mut node = crate::core::datamodel::Node::new();
        node.add_endpoint(0, vec![gc, oc, ac]);
        let mut ctx = test_ctx();

        let req = im::encode_invoke_request(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 1)),
        );
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &crate::core::datamodel::ReadCtx::default(),
            )
            .unwrap();
        let (opcode, payload) = (outcome.opcode, outcome.payload);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = im::decode_invoke_response_data(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_SUCCESS);
        let (code, _) =
            decode_commissioning_status_response(out.fields_tlv.as_deref().unwrap()).unwrap();
        assert_eq!(code, 0);

        let req = im::encode_invoke_request(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            Some(&encode_cert_chain_request(CERT_TYPE_DAC)),
        );
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &crate::core::datamodel::ReadCtx::default(),
            )
            .unwrap();
        let (opcode, payload) = (outcome.opcode, outcome.payload);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = im::decode_invoke_response_data(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_SUCCESS);
        assert!(out.fields_tlv.is_some());

        let req = im::encode_invoke_request(
            0,
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            Some(&encode_open_commissioning_window(
                300,
                &[0x42; 97],
                0x0ABC,
                1000,
                &[0x5A; 16],
            )),
        );
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &crate::core::datamodel::ReadCtx::default(),
            )
            .unwrap();
        let (opcode, payload) = (outcome.opcode, outcome.payload);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = im::decode_invoke_response_data(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_SUCCESS);
    }
}
