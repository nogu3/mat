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
//! ## Structure: one state, two `ClusterHandler`s
//!
//! `Node::add_endpoint` takes `Vec<Box<dyn ClusterHandler>>` — each boxed
//! handler is singly owned and answers exactly one `cluster_id()`. But
//! General Commissioning and Node Operational Credentials commands share
//! state (the fail-safe timer, the CSR/AddTrustedRoot staged between
//! commands, the fabric table), so `CommissioningServer` itself is *not* a
//! `ClusterHandler` — it holds an `Arc<Mutex<Inner>>` and
//! `into_cluster_handlers` splits it into two thin adapters
//! (`GeneralCommissioningHandler`/`OperationalCredentialsHandler`) that
//! both lock the same `Inner` and delegate. `Arc<Mutex<..>>` rather than
//! `Rc<RefCell<..>>` so the handlers stay `Send` for a future async IM
//! driver (mirrors `Node`'s eventual home behind `tokio::sync::Mutex` or
//! similar) even though nothing here awaits.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mat_controller::attestation::{attestation_tbs, encode_attestation_elements};
use mat_controller::cert::{verify_noc_chain, MatterCert};
use mat_controller::commissioning::{
    decode_add_noc, decode_add_trusted_root, decode_arm_fail_safe, decode_attestation_request,
    decode_cert_chain_request, decode_csr_request, decode_set_regulatory_config,
    encode_attestation_response, encode_cert_chain_response, encode_commissioning_status_response,
    encode_csr_response, encode_noc_response, encode_nocsr_elements, CERT_TYPE_DAC, CERT_TYPE_PAI,
    CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_NOC,
    CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE, CMD_ATTESTATION_REQUEST, CMD_CERT_CHAIN_REQUEST,
    CMD_COMMISSIONING_COMPLETE, CMD_CSR_REQUEST, CMD_SET_REGULATORY_CONFIG,
};
use mat_controller::crypto::sign_ecdsa_p256;
use mat_controller::fabric::{compressed_fabric_id, derive_ipk_operational};
use mat_controller::im;
use mat_controller::tlv::{Tag, Writer};
use mat_controller::x509::{generate_csr, DevAttestation};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
use crate::core::fabric_store::{FabricEntry, FabricStore};

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
            })),
        }
    }

    /// Fabrics installed so far (cloned out of the shared state — this
    /// device expects at most a handful, so the copy is cheap).
    pub fn fabrics(&self) -> Vec<FabricEntry> {
        self.inner
            .lock()
            .expect("commissioning server mutex poisoned")
            .store
            .entries()
            .to_vec()
    }

    /// Test-only visibility into whether any CSR/AddTrustedRoot material is
    /// currently staged — lets fail-safe-transition tests assert `pending`
    /// was actually discarded without making the field `pub`.
    #[cfg(test)]
    fn pending_is_empty(&self) -> bool {
        let inner = self
            .inner
            .lock()
            .expect("commissioning server mutex poisoned");
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
        self.inner
            .lock()
            .expect("commissioning server mutex poisoned")
            .fail_safe
            .deadline()
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
        self.inner
            .lock()
            .expect("commissioning server mutex poisoned")
            .expire_fail_safe()
    }

    /// Test-only: see `FailSafeState::force_expire`.
    #[cfg(test)]
    fn force_expire_fail_safe(&self) {
        self.inner
            .lock()
            .expect("commissioning server mutex poisoned")
            .fail_safe
            .force_expire();
    }

    /// Splits into the two `ClusterHandler` adapters `Node::add_cluster`
    /// registers on endpoint 0 (General Commissioning 0x0030, Node
    /// Operational Credentials 0x003E) — see the module doc. Takes `&self`
    /// (not `self`) so a runtime can keep the original `CommissioningServer`
    /// around afterwards to poll `fabrics()` (e.g. to notice a fresh AddNOC
    /// and publish an operational mDNS advert) — both handlers just clone
    /// the shared `Arc<Mutex<Inner>>`, same as the two clones already did
    /// when this took `self` by value.
    pub fn into_cluster_handlers(&self) -> (Box<dyn ClusterHandler>, Box<dyn ClusterHandler>) {
        (
            Box::new(GeneralCommissioningHandler(Arc::clone(&self.inner))),
            Box::new(OperationalCredentialsHandler(Arc::clone(&self.inner))),
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
        let mut inner = self
            .inner
            .lock()
            .expect("commissioning server mutex poisoned");
        match cluster {
            CLUSTER_GENERAL_COMMISSIONING => {
                inner.handle_general_commissioning(command, fields_tlv)
            }
            CLUSTER_OPERATIONAL_CREDENTIALS => {
                inner.handle_operational_credentials(command, fields_tlv, ctx)
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
        self.0
            .lock()
            .expect("commissioning server mutex poisoned")
            .read_general_commissioning(attribute)
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        self.0
            .lock()
            .expect("commissioning server mutex poisoned")
            .handle_general_commissioning(command, fields_tlv)
    }
}

/// Thin `ClusterHandler` adapter for Node Operational Credentials (0x003E).
struct OperationalCredentialsHandler(Arc<Mutex<Inner>>);

impl ClusterHandler for OperationalCredentialsHandler {
    fn cluster_id(&self) -> u32 {
        CLUSTER_OPERATIONAL_CREDENTIALS
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
        self.0
            .lock()
            .expect("commissioning server mutex poisoned")
            .read_operational_credentials(attribute, ctx)
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        self.0
            .lock()
            .expect("commissioning server mutex poisoned")
            .handle_operational_credentials(command, fields_tlv, ctx)
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
    /// `self.store` alone.
    fn read_operational_credentials(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            ATTR_OC_NOCS => Some(self.encode_nocs()),
            ATTR_OC_FABRICS => Some(self.encode_fabrics()),
            ATTR_OC_SUPPORTED_FABRICS => Some(uint_value(5)),
            ATTR_OC_COMMISSIONED_FABRICS => Some(uint_value(self.store.entries().len() as u64)),
            ATTR_OC_TRUSTED_ROOT_CERTIFICATES => Some(self.encode_trusted_root_certificates()),
            ATTR_OC_CURRENT_FABRIC_INDEX => Some(uint_value(u64::from(ctx.fabric_index))),
            _ => None,
        }
    }

    /// NOCs(0): `array[ struct{1: NOCValue, 2: ICACValue?, 254:
    /// FabricIndex} ]` (spec §11.17.5.3, `NOCStruct`).
    fn encode_nocs(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.store.entries() {
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
    /// §11.17.5.3, `FabricDescriptorStruct`). `Label` is always empty —
    /// mat-device has no `UpdateFabricLabel` support to populate it.
    fn encode_fabrics(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.store.entries() {
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(1), &entry.root_public_key);
            w.put_uint(Tag::Context(2), u64::from(entry.admin_vendor_id));
            w.put_uint(Tag::Context(3), entry.fabric_id);
            w.put_uint(Tag::Context(4), entry.node_id);
            w.put_str(Tag::Context(5), "");
            w.put_uint(Tag::Context(254), u64::from(entry.fabric_index));
            w.end_container();
        }
        w.end_container();
        w.finish()
    }

    /// TrustedRootCertificates(4): `array[ bytes(RootCACertificate TLV) ]`
    /// (spec §11.17.5.3) — one entry per installed fabric's RCAC.
    fn encode_trusted_root_certificates(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.store.entries() {
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
        // A previous attempt's AddNOC that was never confirmed by
        // CommissioningComplete must not survive into this one, whether
        // this is a fresh re-arm mid-window or an early disarm (spec
        // §11.10.7.2.1) — same "don't carry a zombie fabric forward" rule
        // `expire_fail_safe` enforces on an actual timeout.
        self.rollback_uncommitted_fabric();
        self.pending = PendingCommissioning::default();
        if expiry_s == 0 {
            self.fail_safe.disarm();
        } else {
            self.fail_safe.arm(expiry_s);
        }
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

        let noc_status = |status: u8| InvokeReply::Data {
            response_command: RESP_NOC,
            fields_tlv: encode_noc_response(status, None),
        };

        let (Some(root_tlv), Some(op_private_key), Some(op_public_key)) = (
            self.pending.trusted_root_tlv.clone(),
            self.pending.op_private_key,
            self.pending.op_public_key,
        ) else {
            return noc_status(NOC_STATUS_MISSING_CSR);
        };

        let Ok(rcac) = MatterCert::parse(&root_tlv) else {
            return noc_status(NOC_STATUS_INVALID_NOC);
        };
        let Ok(noc) = MatterCert::parse(&noc_tlv) else {
            return noc_status(NOC_STATUS_INVALID_NOC);
        };
        let icac = match icac_tlv.as_deref().map(MatterCert::parse) {
            Some(Ok(cert)) => Some(cert),
            Some(Err(_)) => return noc_status(NOC_STATUS_INVALID_NOC),
            None => None,
        };
        if verify_noc_chain(&noc, icac.as_ref(), &rcac).is_err() {
            return noc_status(NOC_STATUS_INVALID_NOC);
        }
        if noc.pub_key != op_public_key {
            return noc_status(NOC_STATUS_INVALID_PUBLIC_KEY);
        }
        let (Some(node_id), Some(fabric_id)) = (noc.node_id(), noc.fabric_id()) else {
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

        InvokeReply::Data {
            response_command: RESP_NOC,
            fields_tlv: encode_noc_response(NOC_STATUS_OK, Some(fabric_index)),
        }
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
    use mat_controller::attestation::verify_device_attestation;
    use mat_controller::commissioning::{
        decode_attestation_response, decode_commissioning_status_response, decode_csr_response,
        decode_noc_response, encode_add_noc, encode_add_trusted_root, encode_arm_fail_safe,
        encode_attestation_request, encode_cert_chain_request, encode_csr_request,
        parse_nocsr_elements, CommissioningFabric,
    };
    use mat_controller::tlv::{Reader, Value};
    use mat_controller::x509::{generate_dev_attestation, parse_csr};

    /// Fixed per-test "session" attestation challenge — in the real
    /// protocol this comes from `SecureSession::attestation_challenge()`
    /// and stays constant for the lifetime of one PASE/CASE session; tests
    /// drive several commands against the same `InvokeCtx` value to match.
    const TEST_CHALLENGE: [u8; 16] = [42u8; 16];

    fn test_ctx() -> InvokeCtx {
        InvokeCtx {
            attestation_challenge: TEST_CHALLENGE,
        }
    }

    /// A `CommissioningServer` over a freshly generated dev attestation
    /// chain and an in-memory (non-persisted) `FabricStore` — file-backed
    /// persistence is `net`-only and tested in `net::store` instead (this
    /// module must stay `cargo check --no-default-features`-clean).
    fn test_server() -> CommissioningServer {
        let dev = generate_dev_attestation(0xFFF1, 0x8000, im::DEVICE_TYPE_ON_OFF_LIGHT).unwrap();
        CommissioningServer::new(dev, FabricStore::new())
    }

    /// Drives ArmFailSafe → CSRRequest → AddTrustedRootCertificate → AddNOC
    /// against `server`, installing one fabric (`fabric_id`/`node_id` from
    /// the caller; admin subject `0xAA` and admin vendor id `0xFFF1` fixed —
    /// no test here needs to vary those). Returns the `AddNOC` reply so
    /// callers that care about the command's own response (like
    /// `add_noc_installs_fabric`) can assert on it directly.
    fn install_fabric(
        server: &mut CommissioningServer,
        fabric_id: u64,
        node_id: u64,
    ) -> InvokeReply {
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

        drive_invoke(
            server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &encode_add_noc(&noc, &fabric.ipk_epoch, 0xAA, 0xFFF1),
        )
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
        }
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

    #[test]
    fn gc_serves_basic_commissioning_info() {
        let server = test_server();
        let (gc, _) = server.into_cluster_handlers();
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

    #[test]
    fn gc_serves_other_scalar_attributes() {
        let server = test_server();
        let (gc, _) = server.into_cluster_handlers();

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
        let (_, oc) = server.into_cluster_handlers();

        // NOCs(0): array[ struct{1: noc_tlv, 2: icac_tlv?, 254: fabric_index} ]
        let nocs_tlv = oc.read(ATTR_OC_NOCS, &ReadCtx::default()).expect("NOCs");
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
            .read(ATTR_OC_FABRICS, &ReadCtx::default())
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
            .read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &ReadCtx::default())
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
        let (_, oc) = server.into_cluster_handlers();

        let tlv = oc
            .read(ATTR_OC_CURRENT_FABRIC_INDEX, &ReadCtx { fabric_index: 1 })
            .expect("CurrentFabricIndex");
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
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
        let dev = generate_dev_attestation(0xFFF1, 0x8000, im::DEVICE_TYPE_ON_OFF_LIGHT).unwrap();
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
        let dev = generate_dev_attestation(0xFFF1, 0x8000, im::DEVICE_TYPE_ON_OFF_LIGHT).unwrap();
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

    #[test]
    fn rearm_resets_stale_pending_commissioning_state() {
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

        // A fresh ArmFailSafe (new commissioning attempt) must not let a
        // stale CSR keypair from a previous attempt leak into this one.
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 2),
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
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001);
        assert_eq!(server.fabrics().len(), 1);

        server.force_expire_fail_safe();

        let removed = server.expire_fail_safe();
        assert_eq!(removed.map(|e| e.fabric_index), Some(1));
        assert!(server.fabrics().is_empty());
        assert!(server.fail_safe_deadline().is_none());

        // Idempotent: the marker and the timer are both already cleared.
        assert!(server.expire_fail_safe().is_none());
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

    #[test]
    fn rearm_rolls_back_previous_attempts_uncommitted_fabric() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001);
        assert_eq!(server.fabrics().len(), 1);

        // Re-arm without a CommissioningComplete in between: the previous
        // attempt's AddNOC must be rolled back, not left as a zombie fabric.
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 2),
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

    /// End-to-end proof that `into_cluster_handlers` wires both clusters
    /// into the same shared state through real `Node`/IM wire framing (not
    /// just the direct `invoke_command` shortcut the tests above use).
    #[test]
    fn wired_into_node_dispatches_both_clusters() {
        let dev = generate_dev_attestation(0xFFF1, 0x8000, im::DEVICE_TYPE_ON_OFF_LIGHT).unwrap();
        let server = CommissioningServer::new(dev, FabricStore::new());
        let (gc, oc) = server.into_cluster_handlers();
        let mut node = crate::core::datamodel::Node::new();
        node.add_endpoint(0, vec![gc, oc]);
        let mut ctx = test_ctx();

        let req = im::encode_invoke_request(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 1)),
        );
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &crate::core::datamodel::ReadCtx::default(),
            )
            .unwrap();
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
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &crate::core::datamodel::ReadCtx::default(),
            )
            .unwrap();
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = im::decode_invoke_response_data(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_SUCCESS);
        assert!(out.fields_tlv.is_some());
    }
}
