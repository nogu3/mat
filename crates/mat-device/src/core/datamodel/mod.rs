//! Data model dispatch skeleton: endpoint/cluster registry (`Node`) and the
//! per-cluster handler trait (`ClusterHandler`) that serves incoming
//! Interaction Model requests. Pure — one opcode+payload in, one (opcode,
//! payload) out, no tokio, no sockets, no files (checked by `cargo check -p
//! mat-device --no-default-features` in CI). Wire codecs live in
//! `mat_controller::im` (this module only knows attribute/command
//! semantics, never TLV byte layout directly).
//!
//! `ReadRequest` (with wildcard endpoint/cluster/attribute expansion — see
//! `Node::read_entries`), `InvokeRequest` (single command), and
//! `WriteRequest` (`Node::handle_write`) are the opcodes `Node::handle_im`
//! dispatches here. `SubscribeRequest` is handled, but not by this
//! dispatch: `mat-device`'s `net::runtime` intercepts it before ever
//! calling `handle_im` (see `serve_subscribe_request`), since the
//! interaction spans priming chunks plus the subscription's lifetime
//! rather than one request/response pair. Every opcode `handle_im` itself
//! has no handler for still gets `StatusResponse(STATUS_INVALID_ACTION)`
//! (spec §8.10.1) rather than being silently dropped or failing the whole
//! exchange.

use std::collections::{BTreeSet, HashMap};

use mat_controller::im::{
    self, AttrPathIn, AttrReportOut, EventEntryOut, EventPathIn, EventReportOut, ImError,
    ReportEntryOut,
};

use crate::core::access_control::Subject;
use crate::core::events::{EmittedEvent, EventLog, StoredEvent};
use crate::core::stimulus::{Stimulus, StimulusError, StimulusOutcome, StimulusReply};
use crate::core::tlv_value;
use mat_controller::tlv::{Reader, Tag, Value, Writer};

/// The DataVersion (spec §7.10.3) every `(endpoint, cluster)` starts at,
/// and what `Node::data_version` reports for a cluster whose values have
/// never changed. `Node::handle_invoke` bumps it for every `(endpoint,
/// cluster)` a command actually changed a value on (see `InvokeCtx::
/// changed`) — a subscribing controller (chip included) keys its own dirty
/// tracking off this field, so a permanently static version would tell it
/// nothing on this node ever changes.
const INITIAL_DATA_VERSION: u32 = 1;

/// Data model schema revision `mat-device` claims to implement (spec
/// §7.13, DataModelRevision). Not spec-load-bearing for M1 — just needs to
/// be a plausible, stable value.
const DATA_MODEL_REVISION: u64 = 17;

/// Per-invoke scratch context threaded through `ClusterHandler::invoke`.
/// Kept as a struct (not `()`) so `invoke`'s signature stays stable as more
/// fields get added.
///
/// `attestation_challenge` (Task 9): the current secure session's
/// attestation challenge (spec §4.13.2.3, `SessionKeys::
/// attestation_challenge`) — `CommissioningServer` binds it into
/// `AttestationResponse`/`CSRResponse` signatures
/// (`mat_controller::attestation::attestation_tbs`). Defaults to all-zero,
/// which is never a real session's challenge (derived by HKDF) but is fine
/// for the existing `datamodel` tests, which never invoke commissioning
/// commands.
/// `changed` (Task 12): every attribute id *within the invoked cluster*
/// whose value actually changed as a result of this command — pushed by the
/// `ClusterHandler::invoke` implementation itself (e.g. `OnOffHandler`
/// pushes `im::ATTR_ON_OFF` when On/Off/Toggle flips the state, and pushes
/// nothing for an On command on an already-on light). `Node::handle_invoke`
/// is the one place that knows the `(endpoint, cluster)` these ids belong
/// to, so it pairs them up into full paths on the way out (`ImOutcome::
/// changed`) and bumps the cluster's DataVersion. The device runtime
/// (`net::runtime`) matches those paths against the active subscription to
/// decide what to report.
#[derive(Debug, Clone, Default)]
pub struct InvokeCtx {
    pub attestation_challenge: [u8; 16],
    pub changed: Vec<u32>,
    /// The invoking session's fabric index (0 for a PASE/non-CASE session,
    /// where no fabric applies yet). `AdministratorCommissioning`'s
    /// `OpenCommissioningWindow` records this as `AdminFabricIndex` (spec
    /// §11.19.7.2.1) — the only current consumer.
    pub fabric_index: u8,
    /// The invoking session's authenticated subject — the peer's operational
    /// Node ID plus its NOC's CASE Authenticated Tags for a CASE session
    /// (spec §6.6.2.1), meaningless (and left at the node-0 default) for
    /// PASE. Paired with `fabric_index` it is the identity every ACL
    /// decision is made against (`Node::handle_invoke`/`handle_write` →
    /// `AclStore::check`). `Default` is node 0, which no real CASE peer uses.
    pub subject: Subject,
    /// Every event the handler emitted while serving this command / write /
    /// stimulus — the event-side counterpart of `changed`, pushed by the
    /// `ClusterHandler` implementation itself (which knows only its own
    /// event ids and priorities). `Node` is the one place that knows the
    /// `(endpoint, cluster)` and the EventNumber they belong to, so it
    /// drains them into its `EventLog` (`Node::drain_events`) right where
    /// it bumps the DataVersion for `changed`.
    pub events: Vec<EmittedEvent>,
}

/// What `Node::handle_im` produced for one incoming IM message: the reply
/// to send back (`opcode`/`payload`, already IM-wire-encoded via
/// `mat_controller::im`) plus the full `(endpoint, cluster, attribute)`
/// paths whose values changed while serving it (`changed` — always empty
/// except for an `InvokeRequest` that actually mutated state). Replaces the
/// bare `(u8, Vec<u8>)` tuple this used to return: subscriptions need the
/// change set, and threading it back through a side channel (a `take_
/// changed` accessor on `Node`) would make "did *this* request change
/// anything" order-dependent.
#[derive(Debug, Clone, PartialEq)]
pub struct ImOutcome {
    pub opcode: u8,
    pub payload: Vec<u8>,
    pub changed: Vec<(u16, u32, u32)>,
}

impl ImOutcome {
    /// The common case: a reply that changed nothing (every read, every
    /// rejected/unsupported request).
    fn unchanged(opcode: u8, payload: Vec<u8>) -> Self {
        Self {
            opcode,
            payload,
            changed: Vec::new(),
        }
    }
}

/// Per-read scratch context threaded through `ClusterHandler::read` and
/// `Node::read_entries`. Carries the current secure session's fabric index
/// (spec §7.9, `FabricIndex`) — needed for fabric-scoped attributes like
/// Operational Credentials' `CurrentFabricIndex`. `0` (the default) is not a
/// valid fabric index (fabric indices start at 1) but matches what a PASE
/// session (no fabric yet) should report.
///
/// `fabric_filtered` is the request's `IsFabricFiltered` (spec §8.4.1 /
/// §8.9.2.4): when set, a fabric-scoped list attribute must only return
/// `fabric_index`'s own entries. `Default` matches the wire default (an
/// absent `IsFabricFiltered` flag means `true`, spec §8.4.1) — filtered, the
/// non-disclosing side — so a `ReadCtx::default()` in a test or a non-read
/// path never accidentally discloses every fabric's entries. Tests that
/// deliberately want the whole table should use `ReadCtx::unfiltered`
/// instead of relying on `default()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadCtx {
    pub fabric_index: u8,
    pub fabric_filtered: bool,
    /// The reading session's authenticated subject — same meaning as
    /// `InvokeCtx::subject` (the peer's node id + CATs on CASE, the node-0
    /// placeholder on PASE), and the identity `Node::read_entries` checks
    /// every expanded path against.
    pub subject: Subject,
}

impl Default for ReadCtx {
    fn default() -> Self {
        Self {
            fabric_index: 0,
            fabric_filtered: true,
            subject: Subject::default(),
        }
    }
}

impl ReadCtx {
    /// 全 fabric を返す読み（IsFabricFiltered=false 相当）。テスト用 —
    /// production の ReadCtx は必ず decode 済みフラグから組む。
    pub fn unfiltered(fabric_index: u8) -> Self {
        Self {
            fabric_index,
            fabric_filtered: false,
            subject: Subject::default(),
        }
    }
}

/// A cluster's outcome for one invoked command: either a bare status (the
/// common case — most commands have no response payload) or response data
/// (a CommandDataIB, e.g. a cluster's declared response command).
#[derive(Debug, Clone, PartialEq)]
pub enum InvokeReply {
    Status(u8),
    /// spec §8.10.1: IM status + クラスタ固有ステータス（例:
    /// AdministratorCommissioning の Busy(2)/PAKEParameterError(3)/
    /// WindowNotOpen(4)）。status は通常 STATUS_FAILURE。
    ClusterStatus {
        status: u8,
        cluster_status: u8,
    },
    Data {
        response_command: u32,
        fields_tlv: Vec<u8>,
    },
}

/// One cluster's server-side implementation on an endpoint. `read`/`invoke`
/// work in already-decoded terms (attribute/command ids, TLV element
/// bytes) — `Node` owns all IM wire framing.
///
/// `: Send` so `Node` (which owns `Box<dyn ClusterHandler>`s) can itself be
/// moved into a `tokio::spawn`ed task — the device runtime
/// (`net::runtime::run`) does exactly that. Every real implementation
/// already satisfies it: `DescriptorHandler`/`BasicInformationHandler` are
/// zero-sized, and `core::commissioning`'s two handlers only hold an
/// `Arc<Mutex<..>>` (already `Send` for the same reason, see that module's
/// doc comment).
pub trait ClusterHandler: Send {
    fn cluster_id(&self) -> u32;
    /// Every attribute id this cluster implements, *excluding* the global
    /// attributes (spec §7.13 — AttributeList/AcceptedCommandList/
    /// GeneratedCommandList/FeatureMap/ClusterRevision, ids 0xFFF8-0xFFFD).
    /// `Node::read_entries` uses this to expand a wildcard attribute path
    /// and to synthesize `AttributeList`'s own value; `Node` adds the
    /// global attributes on top, so implementations never list them here.
    fn attributes(&self) -> Vec<u32>;
    /// Reads one attribute. `Some` is one complete, well-formed TLV element
    /// tagged `Tag::Anonymous` (the attribute's `Data`); `None` means the
    /// attribute id is not implemented by this cluster (→
    /// `STATUS_UNSUPPORTED_ATTRIBUTE` on a concrete path, silently dropped
    /// on a wildcard-expanded one — see `Node::read_entries`). `ctx` carries
    /// the reading session's fabric index (Operational Credentials'
    /// `CurrentFabricIndex` is the only current consumer).
    fn read(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>>;
    /// Invokes one command. `fields_tlv` is the request's CommandFields (one
    /// complete TLV element, or empty if the command takes no fields).
    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply;
    /// Every request command id this cluster accepts —
    /// `AcceptedCommandList`'s value (spec §7.13), synthesized by `Node`
    /// the same way `AttributeList` is. Defaults to empty for
    /// attribute-only clusters (Descriptor, Basic Information).
    fn accepted_commands(&self) -> Vec<u32> {
        Vec::new()
    }
    /// Every response command id this cluster can generate —
    /// `GeneratedCommandList`'s value (spec §7.13).
    fn generated_commands(&self) -> Vec<u32> {
        Vec::new()
    }
    /// Writes one attribute. `data_tlv` is the AttributeDataIB's `Data`
    /// element (one complete, well-formed TLV element, `Tag::Anonymous`).
    /// `Ok(())` = accepted — the implementation must push the changed
    /// attribute id onto `ctx.changed` itself (mirrors `invoke`'s contract
    /// for `InvokeCtx::changed`; `Node::handle_write` pairs those ids with
    /// this cluster's `(endpoint, cluster)` and bumps DataVersion, same as
    /// `handle_invoke`). `Err(status)` is the IM status carried in the
    /// reply's `AttributeStatusIB` (spec §8.9.2.2).
    ///
    /// `list_append` is `true` when the request's AttributePathIB carried a
    /// `ListIndex` (spec §8.9.2.2) — the write targets one element of a
    /// list attribute (chip-tool-family controllers send a whole-list
    /// replace followed by a `ListIndex: null` append chunk train) rather
    /// than replacing the attribute wholesale. No cluster implemented so
    /// far has a list attribute's write, so the default below never
    /// needs to branch on it — it's threaded through purely so a future
    /// implementation doesn't need another wire-decode change to see it.
    ///
    /// Defaults to rejecting every write — matches every cluster
    /// implemented so far (all read-only or command-only).
    fn write(
        &mut self,
        _attribute: u32,
        _data_tlv: &[u8],
        _list_append: bool,
        _ctx: &mut InvokeCtx,
    ) -> Result<(), u8> {
        Err(im::STATUS_UNSUPPORTED_WRITE)
    }
    /// FeatureMap (spec §7.13, attribute id 0xFFFC) — which optional
    /// cluster features this endpoint's instance supports. Defaults to 0
    /// (no optional features) — every cluster implemented so far except
    /// NetworkCommissioning(Ethernet) (Task 4) reports no features.
    fn feature_map(&self) -> u32 {
        0
    }
    /// ClusterRevision (spec §7.13, id 0xFFFD). Real implementations return
    /// the revision the current spec (Matter 1.4) assigns their cluster —
    /// M2 hardcoded every cluster to 1, which this default preserves for
    /// tests that mock `ClusterHandler` without caring about the value.
    fn revision(&self) -> u16 {
        1
    }
    /// The privilege (spec §9.10.5) a session must hold over this
    /// `(endpoint, cluster)` to *read* `attribute` — `Node`'s dispatch
    /// checks it against the ACL before calling `read` (see
    /// `Node::read_entries`). The defaults below are the spec's own
    /// defaults for a cluster that says nothing else: View to read, Operate
    /// to write or invoke. Clusters whose spec access table differs
    /// override these (e.g. AccessControl's `ACL` attribute is Administer
    /// on both sides, Group Key Management's `KeySetWrite` is Administer).
    ///
    /// The five global attributes (spec §7.13, 0xFFF8-0xFFFD) never reach
    /// these methods — `Node` answers them itself and always requires View,
    /// so an override that returns Administer for `_` doesn't lock a
    /// controller out of a cluster's own metadata.
    fn read_privilege(&self, _attribute: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_VIEW
    }
    /// Write-side counterpart of [`read_privilege`], defaulting to Operate.
    ///
    /// [`read_privilege`]: Self::read_privilege
    fn write_privilege(&self, _attribute: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_OPERATE
    }
    /// Invoke-side counterpart of [`read_privilege`], defaulting to Operate
    /// — the privilege an ordinary "use the device" controller holds.
    ///
    /// [`read_privilege`]: Self::read_privilege
    fn invoke_privilege(&self, _command: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_OPERATE
    }
    /// Every event id this cluster can generate (spec §7.14) — the event
    /// counterpart of [`attributes`]. `Node` uses it to expand a wildcard
    /// event path and to decide whether a *concrete* event path resolves at
    /// all (an id not listed here is `STATUS_UNSUPPORTED_EVENT`). Defaults
    /// to empty: every cluster implemented so far generates no events.
    ///
    /// [`attributes`]: Self::attributes
    fn events(&self) -> Vec<u32> {
        Vec::new()
    }
    /// The privilege (spec §9.10.5) a session must hold over this
    /// `(endpoint, cluster)` to *receive* `event` — the event-side sibling
    /// of [`read_privilege`], with the same View default.
    ///
    /// [`read_privilege`]: Self::read_privilege
    fn event_privilege(&self, _event: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_VIEW
    }
    /// Applies an external stimulus (a simulated button press, a simulated
    /// sensor state change — see `core::stimulus`) to this cluster.
    /// Implementations mutate their own state and report what happened
    /// through `ctx` exactly as `invoke` does: changed attribute ids onto
    /// `ctx.changed`, emitted events onto `ctx.events`.
    ///
    /// `StimulusReply::Unsupported` (the default — no cluster reacts to a
    /// stimulus unless it says so) means "not my business": `Node::
    /// stimulate` moves on to the next cluster on the endpoint. `Rejected`
    /// means "mine, but not right now" and stops the search with an error.
    fn stimulate(&mut self, _stimulus: &Stimulus, _ctx: &mut InvokeCtx) -> StimulusReply {
        StimulusReply::Unsupported
    }
}

/// Interaction Model server-side dispatch errors: either a malformed
/// request payload, or an opcode this M1 skeleton doesn't implement yet.
#[derive(Debug)]
pub enum ImServerError {
    Decode(ImError),
    UnsupportedOpcode(u8),
    /// An inbound message that must *not* be answered at all — not a
    /// decode failure and not "can't handle this action", just "there is
    /// no reply to send back for this one". `handle_im`'s success shape is
    /// always "here is the (opcode, payload) to reply with", so "silently
    /// drop" has to come back as an error variant instead; callers that
    /// already treat any `Err` as "send nothing" (`net::runtime::
    /// serve_secured_message`) get the right behavior for free. Currently
    /// only produced by `handle_im`'s `OPCODE_STATUS_RESPONSE` arm — see
    /// its doc comment for why.
    NoReply,
}

impl std::fmt::Display for ImServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImServerError::Decode(e) => write!(f, "im: {e}"),
            ImServerError::UnsupportedOpcode(op) => {
                write!(f, "im: unsupported opcode 0x{op:02X}")
            }
            ImServerError::NoReply => write!(f, "im: no reply required"),
        }
    }
}

impl std::error::Error for ImServerError {}

impl From<ImError> for ImServerError {
    fn from(e: ImError) -> Self {
        ImServerError::Decode(e)
    }
}

/// The device's endpoint/cluster registry: dispatches incoming IM
/// requests (`handle_im`) to the matching `ClusterHandler`.
pub struct Node {
    endpoints: Vec<(u16, Vec<Box<dyn ClusterHandler>>)>,
    /// Per-`(endpoint, cluster)` DataVersion (spec §7.10.3). Absent = never
    /// changed = `INITIAL_DATA_VERSION`; `handle_invoke` inserts/bumps an
    /// entry the first time a command changes one of the cluster's values.
    /// A map (rather than a counter per `ClusterHandler`) keeps versioning
    /// entirely `Node`'s business — `ClusterHandler` implementations only
    /// ever say *what* changed (`InvokeCtx::changed`).
    versions: HashMap<(u16, u32), u32>,
    /// The DataVersion every `(endpoint, cluster)` not yet in `versions`
    /// reports (see `data_version`) and the value newly-bumped entries
    /// start from (see `handle_invoke`/`handle_write`). Defaults to
    /// `INITIAL_DATA_VERSION`; `set_data_version_base` overrides it —
    /// `device::Device::new` seeds it from `getrandom` at boot (spec
    /// §7.10.3) so a restarted node's DataVersions don't coincide with
    /// whatever a subscriber cached from the previous boot.
    version_base: u32,
    /// The ACL this node enforces read/write/invoke against (spec §9.10),
    /// wired by `set_acl_store` — `device::Device::new`'s assembly is the
    /// only caller. `None` (every `Node` built in a test or by a `core`
    /// unit test) means **no enforcement at all**: every path is allowed,
    /// which is what keeps this dispatch's own tests — and every cluster's
    /// — about the thing they actually test rather than about ACL wiring.
    acl: Option<crate::core::access_control::AclStore>,
    /// This node's event log (spec §7.14) — every event any cluster emits
    /// through `InvokeCtx::events` lands here, numbered node-wide. Defaults
    /// to `EventLog::default()` (numbering from 1, `DEFAULT_CAP` entries);
    /// `set_event_log` replaces it — `device::Device::new` seeds the first
    /// EventNumber the same way it seeds the DataVersion base.
    event_log: EventLog,
}

/// Bundles the (endpoint id, its clusters, the reading session's fabric
/// index) triple that flows unchanged through one wildcard-expansion branch
/// (`Node::expand_cluster`/`expand_attribute`/`read_attribute_value`) —
/// purely a parameter-count reduction (keeps those methods under clippy's
/// `too_many_arguments`), no behavior of its own.
struct ExpandCtx<'a> {
    endpoint: u16,
    clusters: &'a [Box<dyn ClusterHandler>],
    read_ctx: &'a ReadCtx,
}

/// `Node::invoke_on_endpoint`'s success case: the cluster's reply plus every
/// `(endpoint, cluster, attribute)` its command changed.
type InvokeOnEndpointOk = (InvokeReply, Vec<(u16, u32, u32)>);

impl Node {
    /// An empty node with no endpoints. Use `add_endpoint` to populate it,
    /// or `with_root_endpoint` for a node that already has the mandatory
    /// endpoint 0 (Descriptor + BasicInformation).
    pub fn new() -> Self {
        Self {
            endpoints: Vec::new(),
            versions: HashMap::new(),
            version_base: INITIAL_DATA_VERSION,
            acl: None,
            event_log: EventLog::default(),
        }
    }

    /// A node with endpoint 0 wired up: Descriptor (DeviceTypeList=
    /// RootNode, ServerList, PartsList) and BasicInformation
    /// (DataModelRevision, VendorID, ProductID, VendorName="mat",
    /// ProductName="matv") — the minimum a Matter node must expose.
    /// `vendor_id`/`product_id` come from the runtime's `DeviceConfig` (the
    /// same values advertised in mDNS TXT records and the commissioning
    /// QR/manual code) so BasicInformation doesn't drift from what the
    /// device actually announces itself as.
    ///
    /// Fixed UniqueID `"matv-dev"` — kept as the ~15 existing call sites'
    /// (mostly test) behavior; a real device wants a per-install UniqueID,
    /// which is what [`with_root_endpoint_unique`] is for.
    ///
    /// [`with_root_endpoint_unique`]: Self::with_root_endpoint_unique
    pub fn with_root_endpoint(vendor_id: u16, product_id: u16) -> Self {
        Self::with_root_endpoint_unique(vendor_id, product_id, "matv-dev")
    }

    /// Same as [`with_root_endpoint`], but with an explicit UniqueID (spec
    /// §11.1.6.15) rather than the fixed `"matv-dev"` fallback — what
    /// `device::Device::new` uses so BasicInformation's UniqueID is the
    /// per-install value persisted at `<store_dir>/unique_id`. Delegates to
    /// `with_root_endpoint_persisted_impl` with the spec-default
    /// NodeLabel/Location (`""`/`"XX"`) and no persist backend — the ~15
    /// existing call sites (mostly tests) never touch disk for
    /// NodeLabel/Location, same as before this task.
    ///
    /// [`with_root_endpoint`]: Self::with_root_endpoint
    pub fn with_root_endpoint_unique(vendor_id: u16, product_id: u16, unique_id: &str) -> Self {
        Self::with_root_endpoint_persisted_impl(
            vendor_id,
            product_id,
            unique_id,
            String::new(),
            "XX".to_string(),
            None,
        )
    }

    /// Same as [`with_root_endpoint_unique`], but with NodeLabel/Location
    /// seeded from whatever was last persisted (`device::Device::new` loads
    /// them via `net::store::load_basic_info`) and a `persist` backend the
    /// handler saves to on every future NodeLabel/Location write — the
    /// `AclPersist`/`FabricPersist` injection pattern, applied to
    /// BasicInformation's two writable attributes.
    ///
    /// [`with_root_endpoint_unique`]: Self::with_root_endpoint_unique
    pub fn with_root_endpoint_persisted(
        vendor_id: u16,
        product_id: u16,
        unique_id: &str,
        node_label: String,
        location: String,
        persist: Box<dyn BasicInfoPersist>,
    ) -> Self {
        Self::with_root_endpoint_persisted_impl(
            vendor_id,
            product_id,
            unique_id,
            node_label,
            location,
            Some(persist),
        )
    }

    /// Shared construction path [`with_root_endpoint_unique`] and
    /// [`with_root_endpoint_persisted`] both funnel through — the only
    /// difference between them is whether a persist backend is wired in.
    ///
    /// [`with_root_endpoint_unique`]: Self::with_root_endpoint_unique
    /// [`with_root_endpoint_persisted`]: Self::with_root_endpoint_persisted
    fn with_root_endpoint_persisted_impl(
        vendor_id: u16,
        product_id: u16,
        unique_id: &str,
        node_label: String,
        location: String,
        persist: Option<Box<dyn BasicInfoPersist>>,
    ) -> Self {
        let mut node = Self::new();
        node.add_endpoint(
            0,
            vec![
                Box::new(DescriptorHandler::for_device(im::DEVICE_TYPE_ROOT_NODE))
                    as Box<dyn ClusterHandler>,
                Box::new(BasicInformationHandler {
                    vendor_id,
                    product_id,
                    unique_id: unique_id.to_string(),
                    node_label,
                    location,
                    persist,
                }) as Box<dyn ClusterHandler>,
            ],
        );
        node
    }

    pub fn add_endpoint(&mut self, endpoint: u16, clusters: Vec<Box<dyn ClusterHandler>>) {
        self.endpoints.push((endpoint, clusters));
    }

    /// Appends `handler` to `endpoint`'s cluster list, creating the
    /// endpoint entry if it doesn't exist yet. Unlike [`add_endpoint`]
    /// (which always pushes a *new* `(endpoint, clusters)` entry, even if
    /// `endpoint` already has one — `expand_endpoint`/`handle_invoke` only
    /// ever look at the *first* matching entry via `Vec::iter().find`, so a
    /// second `add_endpoint(0, ..)` call would silently shadow the first),
    /// this lets more than one cluster be registered onto the same endpoint
    /// incrementally — e.g. `with_root_endpoint()`'s Descriptor/
    /// BasicInformation plus a device runtime's commissioning clusters, all
    /// on endpoint 0.
    ///
    /// [`add_endpoint`]: Self::add_endpoint
    pub fn add_cluster(&mut self, endpoint: u16, handler: Box<dyn ClusterHandler>) {
        if let Some((_, clusters)) = self.endpoints.iter_mut().find(|(id, _)| *id == endpoint) {
            clusters.push(handler);
        } else {
            self.endpoints.push((endpoint, vec![handler]));
        }
    }

    /// The current DataVersion (spec §7.10.3) of one `(endpoint, cluster)`
    /// — `version_base` until a command changes one of its values.
    pub fn data_version(&self, endpoint: u16, cluster: u32) -> u32 {
        self.versions
            .get(&(endpoint, cluster))
            .copied()
            .unwrap_or(self.version_base)
    }

    /// Sets the DataVersion every `(endpoint, cluster)` not yet changed
    /// reports, and the value newly-bumped entries start counting up from
    /// (spec §7.10.3: ブートごとに乱数初期化 — the initial value must be
    /// unpredictable at each boot so a subscriber's cached DataVersion from
    /// a previous boot never coincidentally matches). Call once, right
    /// after construction — `device::Device::new` seeds it from
    /// `getrandom`; tests that don't call this keep the fixed
    /// `INITIAL_DATA_VERSION` default.
    pub fn set_data_version_base(&mut self, base: u32) {
        self.version_base = base;
    }

    /// Turns on ACL enforcement (spec §9.10) for every read/write/invoke
    /// this node dispatches, against `store` — the same `AclStore` the EP0
    /// `AccessControlHandler` serves and `CommissioningServer` seeds from
    /// AddNOC. Call once, at assembly time and *before* the request loop
    /// starts (`device::Device::new`); a `Node` that never gets one keeps
    /// enforcement off (see the `acl` field's doc).
    pub fn set_acl_store(&mut self, store: crate::core::access_control::AclStore) {
        self.acl = Some(store);
    }

    /// The bookkeeping every mutation path shares once a handler has run:
    /// the bare attribute ids it pushed on `ctx.changed` become concrete
    /// paths, this cluster's DataVersion is bumped once if anything
    /// changed, and whatever it emitted goes into the event log
    /// (`drain_events`). Returns `(changed_paths, event_numbers)`.
    fn commit_changes(
        &mut self,
        endpoint: u16,
        cluster: u32,
        ctx: &mut InvokeCtx,
        system_timestamp_ms: u64,
    ) -> (Vec<(u16, u32, u32)>, Vec<u64>) {
        let changed: Vec<(u16, u32, u32)> = ctx
            .changed
            .drain(..)
            .map(|attribute| (endpoint, cluster, attribute))
            .collect();
        if !changed.is_empty() {
            let version = self
                .versions
                .entry((endpoint, cluster))
                .or_insert(self.version_base);
            *version = version.wrapping_add(1);
        }
        let event_numbers = self.drain_events(endpoint, cluster, ctx, system_timestamp_ms);
        (changed, event_numbers)
    }

    /// Dispatches one incoming IM message. Returns the reply to send back
    /// plus whatever changed while serving it (`ImOutcome`). `read_ctx`
    /// carries the requesting session's fabric index (see `ReadCtx`'s doc)
    /// — irrelevant to `InvokeRequest`, but threaded through every
    /// `ReadRequest`.
    pub fn handle_im(
        &mut self,
        opcode: u8,
        payload: &[u8],
        ctx: &mut InvokeCtx,
        read_ctx: &ReadCtx,
    ) -> Result<ImOutcome, ImServerError> {
        match opcode {
            im::OPCODE_READ_REQUEST => self.handle_read(payload, read_ctx),
            im::OPCODE_INVOKE_REQUEST => self.handle_invoke(payload, ctx),
            im::OPCODE_WRITE_REQUEST => self.handle_write(payload, ctx),
            // Inbound StatusResponse reaching the generic dispatch is not
            // an unhandled action to reject — it's the initiator's ack of
            // a ReportData chunk we just sent (`net::runtime`'s chunked
            // read and subscription priming flows), which normally
            // consumes it directly in `await_peer_status_ok` (whose
            // `recv_request` wait is what pulls it off the socket) and
            // never routes it through `handle_im` at all. But if one slips
            // through anyway — e.g. `serve_secured`'s buffered-request
            // drain replaying something left in `peer_initiated` through
            // this same dispatch — answering it with
            // `StatusResponse(INVALID_ACTION)` would be a protocol
            // violation mid-chunking that makes a real controller (chip)
            // abort the read. Drop it instead of answering (carried
            // finding from Task 5's review).
            im::OPCODE_STATUS_RESPONSE => Err(ImServerError::NoReply),
            // Timed Request Action（spec §8.9.4）: `StatusResponse(SUCCESS)`
            // を返すと initiator が同一 exchange で後続の timed
            // Invoke/Write を送ってくる。後続はステートレスに通常経路で
            // 処理される（この skeleton は timed access 必須のコマンドを
            // 持たないため、期限とフラグ整合の enforcement は M3 送り —
            // INVALID_ACTION で拒むと Google Play Services スタックの
            // commissioning がここで中断する）。
            im::OPCODE_TIMED_REQUEST => Ok(ImOutcome::unchanged(
                im::OPCODE_STATUS_RESPONSE,
                im::encode_status_response(im::STATUS_SUCCESS),
            )),
            // Any opcode this skeleton has no handler for (SubscribeRequest,
            // TimedRequest, ...) is answered — not
            // silently dropped, and not a hard error that kills the
            // exchange — with the IM status for "can't handle this action"
            // (spec §8.10.1). `ImServerError::UnsupportedOpcode` is no
            // longer produced here; it stays reserved for a payload that
            // can't even be decoded into a response we know how to send.
            _other => Ok(ImOutcome::unchanged(
                im::OPCODE_STATUS_RESPONSE,
                im::encode_status_response(im::STATUS_INVALID_ACTION),
            )),
        }
    }

    /// `handle_invoke` と `handle_group_invoke` の共通部: endpoint/cluster を
    /// 引き、ACL（`invoke_privilege`）を通し、`invoke` して changed を
    /// `(endpoint, cluster, attribute)` に写し DataVersion を bump する。
    /// `Err(status)` = endpoint/cluster 不在・ACL 拒否（呼び側が応答するか
    /// ログするかを決める）。
    fn invoke_on_endpoint(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields_tlv: &[u8],
        ctx: &mut InvokeCtx,
    ) -> Result<InvokeOnEndpointOk, u8> {
        let handler = handler_mut(&mut self.endpoints, endpoint, cluster)?;
        // ACL (spec §9.10): the command's required privilege comes from the
        // cluster (`invoke_privilege`, Operate unless overridden). Denied is
        // an `UNSUPPORTED_ACCESS` CommandStatusIB — same shape as the
        // `UNSUPPORTED_COMMAND` reply above it, so a controller sees a
        // per-command refusal rather than a dead exchange.
        if !acl_allows(
            &self.acl,
            ctx.fabric_index,
            ctx.subject,
            handler.invoke_privilege(command),
            endpoint,
            cluster,
        ) {
            return Err(im::STATUS_UNSUPPORTED_ACCESS);
        }
        // Each `invoke` gets a fresh change list: `ctx` is per-session
        // scratch (`attestation_challenge` outlives one command), so a
        // leftover `changed` from an earlier command in the same session
        // must not be re-reported as this one's. Same for `events`.
        ctx.changed.clear();
        ctx.events.clear();
        let reply = handler.invoke(command, fields_tlv, ctx);
        // The handler reports bare attribute ids (it only knows its own
        // cluster); pair them with the endpoint/cluster it was dispatched
        // to, and bump this cluster's DataVersion once if anything changed.
        // Timestamp 0: this dispatch has no clock — see `drain_events`' doc
        // (no cluster emits from `invoke` yet).
        let (changed, _) = self.commit_changes(endpoint, cluster, ctx, 0);
        Ok((reply, changed))
    }

    fn handle_invoke(
        &mut self,
        payload: &[u8],
        ctx: &mut InvokeCtx,
    ) -> Result<ImOutcome, ImServerError> {
        let req = im::decode_invoke_request(payload)?;
        let (reply, changed) = match self.invoke_on_endpoint(
            req.endpoint,
            req.cluster,
            req.command,
            &req.fields_tlv,
            ctx,
        ) {
            Ok(ok) => ok,
            Err(status) => {
                return Ok(ImOutcome::unchanged(
                    im::OPCODE_INVOKE_RESPONSE,
                    im::encode_invoke_response_status(
                        req.endpoint,
                        req.cluster,
                        req.command,
                        status,
                        None,
                    ),
                ));
            }
        };
        let resp_payload = match reply {
            InvokeReply::Status(status) => im::encode_invoke_response_status(
                req.endpoint,
                req.cluster,
                req.command,
                status,
                None,
            ),
            InvokeReply::ClusterStatus {
                status,
                cluster_status,
            } => im::encode_invoke_response_status(
                req.endpoint,
                req.cluster,
                req.command,
                status,
                Some(cluster_status),
            ),
            InvokeReply::Data {
                response_command,
                fields_tlv,
            } => im::encode_invoke_response_data(
                req.endpoint,
                req.cluster,
                response_command,
                &fields_tlv,
            ),
        };
        Ok(ImOutcome {
            opcode: im::OPCODE_INVOKE_RESPONSE,
            payload: resp_payload,
            changed,
        })
    }

    /// groupcast の Invoke（spec §8.2.5: 応答なし）: group の member
    /// `endpoints` × `invokes` の全組み合わせに `invoke_on_endpoint`。拒否・
    /// 不在・非 SUCCESS は debug ログのみ。戻りは購読の dirty に流す changed。
    pub fn handle_group_invoke(
        &mut self,
        endpoints: &[u16],
        invokes: &[crate::core::group_invoke::GroupInvokeIn],
        ctx: &mut InvokeCtx,
    ) -> Vec<(u16, u32, u32)> {
        let mut changed = Vec::new();
        for &endpoint in endpoints {
            for inv in invokes {
                match self.invoke_on_endpoint(
                    endpoint,
                    inv.cluster,
                    inv.command,
                    &inv.fields_tlv,
                    ctx,
                ) {
                    Ok((reply, mut c)) => {
                        if let InvokeReply::Status(status) = reply {
                            if status != im::STATUS_SUCCESS {
                                tracing::debug!(
                                    endpoint,
                                    cluster = inv.cluster,
                                    command = inv.command,
                                    status,
                                    "group invoke: command status"
                                );
                            }
                        }
                        changed.append(&mut c);
                    }
                    Err(status) => tracing::debug!(
                        endpoint,
                        cluster = inv.cluster,
                        command = inv.command,
                        status,
                        "group invoke: not applied"
                    ),
                }
            }
        }
        changed
    }

    /// Dispatches a `WriteRequest`'s attribute writes one at a time —
    /// mirrors `handle_invoke`'s endpoint/cluster resolution and
    /// DataVersion/`changed` bookkeeping, but per write entry rather than
    /// once (a `WriteRequest` can carry more than one `AttributeDataIB`,
    /// unlike M2-scope `InvokeRequest` which is always a single command).
    /// Every entry gets its own `AttributeStatusIB` in the reply (spec
    /// §8.9.2.4) — one bad path never fails the whole exchange.
    fn handle_write(
        &mut self,
        payload: &[u8],
        ctx: &mut InvokeCtx,
    ) -> Result<ImOutcome, ImServerError> {
        let req = im::decode_write_request(payload)?;
        let mut results: Vec<(u16, u32, u32, u8)> = Vec::with_capacity(req.writes.len());
        let mut changed: Vec<(u16, u32, u32)> = Vec::new();
        for write in &req.writes {
            // Every write must name a concrete (endpoint, cluster,
            // attribute) — this dispatch has no wildcard-write expansion
            // (spec §8.9.2.4 allows it, but no controller this skeleton
            // talks to sends one). A wildcard field here means the request
            // is malformed for this dispatch's purposes, not "not found".
            let (Some(endpoint), Some(cluster), Some(attribute)) =
                (write.endpoint, write.cluster, write.attribute)
            else {
                results.push((
                    write.endpoint.unwrap_or(0),
                    write.cluster.unwrap_or(0),
                    write.attribute.unwrap_or(0),
                    im::STATUS_INVALID_COMMAND,
                ));
                continue;
            };
            let handler = match handler_mut(&mut self.endpoints, endpoint, cluster) {
                Ok(handler) => handler,
                Err(status) => {
                    results.push((endpoint, cluster, attribute, status));
                    continue;
                }
            };
            // ACL (spec §9.10), per write entry: denied is this entry's own
            // `AttributeStatusIB(UNSUPPORTED_ACCESS)`, exactly like the
            // unresolvable-path statuses above — one refused attribute
            // never fails the other entries in the same WriteRequest.
            if !acl_allows(
                &self.acl,
                ctx.fabric_index,
                ctx.subject,
                handler.write_privilege(attribute),
                endpoint,
                cluster,
            ) {
                results.push((endpoint, cluster, attribute, im::STATUS_UNSUPPORTED_ACCESS));
                continue;
            }
            // Same rationale as `handle_invoke`: `ctx` is per-session
            // scratch, so a leftover `changed`/`events` from an earlier
            // write/invoke in the same session must not be re-reported as
            // this one's.
            ctx.changed.clear();
            ctx.events.clear();
            let status = match handler.write(attribute, &write.data_tlv, write.list_append, ctx) {
                Ok(()) => im::STATUS_SUCCESS,
                Err(status) => status,
            };
            // Same as the invoke path: events emitted by this write land in
            // the log next to its DataVersion bump, timestamp 0 (no clock
            // here — see `drain_events`).
            let (entry_changed, _) = self.commit_changes(endpoint, cluster, ctx, 0);
            changed.extend(entry_changed);
            results.push((endpoint, cluster, attribute, status));
        }
        Ok(ImOutcome {
            opcode: im::OPCODE_WRITE_RESPONSE,
            payload: im::encode_write_response(&results),
            changed,
        })
    }
}

impl Default for Node {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolves `(endpoint, cluster)` to its handler for a mutation
/// (`invoke_on_endpoint` / `handle_write`), with the IM status the caller
/// reports when either half is missing. A free function over the
/// `endpoints` field rather than a `Node` method so the caller can keep
/// borrowing `self.acl` / `self.versions` alongside the returned handler.
fn handler_mut(
    endpoints: &mut [(u16, Vec<Box<dyn ClusterHandler>>)],
    endpoint: u16,
    cluster: u32,
) -> Result<&mut dyn ClusterHandler, u8> {
    let Some((_, clusters)) = endpoints.iter_mut().find(|(id, _)| *id == endpoint) else {
        return Err(im::STATUS_UNSUPPORTED_ENDPOINT);
    };
    let Some(handler) = clusters.iter_mut().find(|h| h.cluster_id() == cluster) else {
        return Err(im::STATUS_UNSUPPORTED_CLUSTER);
    };
    Ok(handler.as_mut())
}

/// The ACL decision every dispatch path funnels through (spec §9.10):
/// - `ctx_fabric == 0` is a PASE session, which has no fabric and therefore
///   no ACL entries to match — spec §9.10.5 grants it implicit Administer
///   (it is how a commissioner writes the very first ACL entry), so it is
///   allowed unconditionally. `AclStore::check` deliberately answers `false`
///   for fabric 0, which makes this bypass the *caller's* responsibility —
///   here, in the one place it belongs.
/// - No store wired (`None`) = enforcement off, everything allowed (see
///   `Node::acl`).
/// - Otherwise the store decides.
///
/// A free function rather than a `Node` method so `handle_invoke`/
/// `handle_write` can call it while `self.endpoints` is mutably borrowed —
/// passing `&self.acl` keeps the borrow to that one field.
fn acl_allows(
    acl: &Option<crate::core::access_control::AclStore>,
    ctx_fabric: u8,
    subject: Subject,
    required: u8,
    endpoint: u16,
    cluster: u32,
) -> bool {
    if ctx_fabric == 0 {
        return true;
    }
    match acl {
        Some(store) => store.check(ctx_fabric, subject, required, endpoint, cluster),
        None => true,
    }
}

/// The five global attributes (spec §7.13, ids 0xFFF8-0xFFFD, EventList
/// 0xFFFA included even though this dispatch doesn't serve it) — answered by
/// `Node` itself, never by a `ClusterHandler`, and readable at View
/// regardless of what the cluster's own `read_privilege` says (see that
/// method's doc).
fn is_global_attribute(attribute: u32) -> bool {
    (0xFFF8..=0xFFFD).contains(&attribute)
}

/// The privilege needed to read `attribute` off `handler`: the cluster's own
/// answer, except for the global attributes (always View, see
/// `is_global_attribute`).
fn read_privilege_for(handler: &dyn ClusterHandler, attribute: u32) -> u8 {
    if is_global_attribute(attribute) {
        crate::core::access_control::PRIVILEGE_VIEW
    } else {
        handler.read_privilege(attribute)
    }
}

mod basic_information;
mod descriptor;
mod events;
mod read;

pub use basic_information::BasicInfoPersist;
pub use descriptor::DescriptorHandler;

use basic_information::BasicInformationHandler;

#[cfg(test)]
mod tests;

/// Pins the hand-written cluster/attribute id constants in
/// `mat_controller::im` (this module's only consumer of them) against
/// `mat_core::ids`'s generated CHIP data model table
/// (`crates/mat-core/src/ids_gen.rs`, regenerated from connectedhomeip by
/// `scripts/gen-ids.py`) — `mat-controller` doesn't depend on `mat-core`
/// (and `mat-device/core` can't, without breaking the `--no-default-
/// features` purity check), so `im.rs`'s own consts can't be *sourced from*
/// the generated table; this test only *checks* them against it. Gated on
/// the `net` feature (not just `test`) because `mat-core` is an optional
/// dependency enabled by `net` — `cargo test -p mat-device
/// --no-default-features` compiles this module without it. A future
/// `ids_gen.rs` regen that moves one of these ids fails a test here instead
/// of drifting silently.
#[cfg(all(test, feature = "net"))]
mod drift_guard {
    use mat_controller::im;
    use mat_core::ids::resolve_attribute;
    use mat_core::ids::resolve_cluster;

    #[test]
    fn descriptor_cluster_and_attrs_match_mat_core_ids() {
        assert_eq!(resolve_cluster("descriptor"), Some(im::CLUSTER_DESCRIPTOR));
        let attr = |name: &str| resolve_attribute(im::CLUSTER_DESCRIPTOR, name).unwrap().id;
        assert_eq!(attr("device-type-list"), im::ATTR_DEVICE_TYPE_LIST);
        assert_eq!(attr("server-list"), im::ATTR_SERVER_LIST);
        assert_eq!(attr("parts-list"), im::ATTR_PARTS_LIST);
    }

    #[test]
    fn basic_information_cluster_and_attrs_match_mat_core_ids() {
        assert_eq!(
            resolve_cluster("basicinformation"),
            Some(im::CLUSTER_BASIC_INFORMATION)
        );
        let attr = |name: &str| {
            resolve_attribute(im::CLUSTER_BASIC_INFORMATION, name)
                .unwrap()
                .id
        };
        assert_eq!(attr("data-model-revision"), im::ATTR_DATA_MODEL_REVISION);
        assert_eq!(attr("vendor-name"), im::ATTR_VENDOR_NAME);
        assert_eq!(attr("vendor-id"), im::ATTR_VENDOR_ID);
        assert_eq!(attr("product-name"), im::ATTR_PRODUCT_NAME);
        assert_eq!(attr("product-id"), im::ATTR_PRODUCT_ID);
        // Task 5 additions.
        assert_eq!(attr("node-label"), im::ATTR_BI_NODE_LABEL);
        assert_eq!(attr("location"), im::ATTR_BI_LOCATION);
        assert_eq!(attr("hardware-version"), im::ATTR_BI_HARDWARE_VERSION);
        assert_eq!(
            attr("hardware-version-string"),
            im::ATTR_BI_HARDWARE_VERSION_STRING
        );
        assert_eq!(attr("software-version"), im::ATTR_BI_SOFTWARE_VERSION);
        assert_eq!(
            attr("software-version-string"),
            im::ATTR_BI_SOFTWARE_VERSION_STRING
        );
        assert_eq!(attr("unique-id"), im::ATTR_BI_UNIQUE_ID);
        assert_eq!(attr("capability-minima"), im::ATTR_BI_CAPABILITY_MINIMA);
        assert_eq!(
            attr("specification-version"),
            im::ATTR_BI_SPECIFICATION_VERSION
        );
        assert_eq!(
            attr("max-paths-per-invoke"),
            im::ATTR_BI_MAX_PATHS_PER_INVOKE
        );
    }

    #[test]
    fn switch_and_boolean_state_ids_match_mat_core_ids() {
        assert_eq!(resolve_cluster("switch"), Some(im::CLUSTER_SWITCH));
        let attr = |name: &str| resolve_attribute(im::CLUSTER_SWITCH, name).unwrap().id;
        assert_eq!(
            attr("number-of-positions"),
            im::ATTR_SWITCH_NUMBER_OF_POSITIONS
        );
        assert_eq!(attr("current-position"), im::ATTR_SWITCH_CURRENT_POSITION);
        assert_eq!(attr("multi-press-max"), im::ATTR_SWITCH_MULTI_PRESS_MAX);
        assert_eq!(
            resolve_cluster("booleanstate"),
            Some(im::CLUSTER_BOOLEAN_STATE)
        );
        assert_eq!(
            resolve_attribute(im::CLUSTER_BOOLEAN_STATE, "state-value")
                .unwrap()
                .id,
            im::ATTR_BS_STATE_VALUE
        );
    }

    // `im::DEVICE_TYPE_ROOT_NODE` (RootNode device type, spec §9.2.2) is
    // intentionally not pinned here: `mat_core::ids`'s generated table
    // covers clusters/attributes/commands only, not device types — there is
    // no `mat_core` lookup to check it against. See the doc comment on the
    // constant itself.
}
