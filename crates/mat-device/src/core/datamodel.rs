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
//! `WriteRequest` (`Node::handle_write`) are all handled — still no
//! subscriptions. Every other opcode gets
//! `StatusResponse(STATUS_INVALID_ACTION)` (spec §8.10.1) rather than being
//! silently dropped or failing the whole exchange.

use std::collections::HashMap;

use mat_controller::im::{self, AttrPathIn, AttrReportOut, ImError, ReportEntryOut};
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
}

impl Default for ReadCtx {
    fn default() -> Self {
        Self {
            fabric_index: 0,
            fabric_filtered: true,
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

impl Node {
    /// An empty node with no endpoints. Use `add_endpoint` to populate it,
    /// or `with_root_endpoint` for a node that already has the mandatory
    /// endpoint 0 (Descriptor + BasicInformation).
    pub fn new() -> Self {
        Self {
            endpoints: Vec::new(),
            versions: HashMap::new(),
            version_base: INITIAL_DATA_VERSION,
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
    /// [`with_root_endpoint_persisted_impl`] with the spec-default
    /// NodeLabel/Location (`""`/`"XX"`) and no persist backend — the ~15
    /// existing call sites (mostly tests) never touch disk for
    /// NodeLabel/Location, same as before this task.
    ///
    /// [`with_root_endpoint`]: Self::with_root_endpoint
    /// [`with_root_endpoint_persisted_impl`]: Self::with_root_endpoint_persisted_impl
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

    fn handle_read(&self, payload: &[u8], read_ctx: &ReadCtx) -> Result<ImOutcome, ImServerError> {
        let paths = im::decode_read_request(payload)?;
        // `usize::MAX` never triggers a split, so this always yields
        // exactly one chunk — the same single-message
        // `more_chunks=false, suppress_response=true` reply `handle_read`
        // has always returned. `handle_im` is only reached by direct unit
        // tests and any opcode still routed through the generic dispatch;
        // the real chunk-aware flow (`read_chunks` with `net::runtime`'s
        // `REPORT_CHUNK_BUDGET`) is `net::runtime::serve_secured_message`'s
        // job (Task 6) — it bypasses `handle_im` for `OPCODE_READ_REQUEST`
        // entirely so it can drive the multi-chunk StatusResponse
        // round-trip.
        let chunks = self.read_chunks(&paths, read_ctx, usize::MAX, None);
        Ok(ImOutcome::unchanged(
            im::OPCODE_REPORT_DATA,
            chunks
                .into_iter()
                .next()
                .expect("read_chunks always yields at least one chunk"),
        ))
    }

    /// Splits `read_entries(paths, read_ctx)`'s report into one or more
    /// encoded `ReportData` payloads, each at most `budget` bytes — except
    /// a single entry that alone exceeds `budget`, which is never split
    /// (no sub-report structure to split at this layer) and goes out alone
    /// in its own over-budget chunk. Greedy: entries are appended to the
    /// current chunk one at a time; the first one that would push the
    /// *non-final-shape* encoded length (`more_chunks=true` — see the
    /// probe comment inline) over `budget` starts a new chunk instead of
    /// splitting mid-report — simplicity over packing efficiency, since
    /// this runs once per read/priming, not in a hot loop.
    ///
    /// Every chunk but the last is encoded `more_chunks=true,
    /// suppress_response=false` — the receiver must answer
    /// `StatusResponse(0)` on the same exchange before the next chunk
    /// (spec §8.9.2.3's chunk handshake; `net::runtime::
    /// serve_read_request_chunked` drives it, mirroring `SecureSession::
    /// subscribe_wildcard`'s priming-report loop on the initiator side).
    ///
    /// `subscription_id` says which of the two callers this is, and changes
    /// the *last* chunk's shape accordingly:
    /// - `None` (a plain `ReadRequest`): the last chunk is `more_chunks=
    ///   false, suppress_response=true` — identical to what a single-chunk
    ///   read has always encoded, so a read that fits in one chunk is
    ///   unchanged (no regression). The read interaction ends there.
    /// - `Some(id)` (a subscription's priming report, Task 12): every chunk
    ///   including the last carries the SubscriptionId and
    ///   `suppress_response=false`, because the priming report is *not* the
    ///   end of the interaction — a `SubscribeResponse` follows on the same
    ///   exchange (spec §8.10), and the initiator answers every priming
    ///   chunk with `StatusResponse(0)` first (`SecureSession::
    ///   subscribe_wildcard`'s loop does exactly that).
    ///
    /// Always returns at least one chunk, even for zero entries (an empty
    /// `ReportData`, matching the pre-Task-6 always-one-chunk behavior for
    /// a read that matches nothing).
    pub fn read_chunks(
        &self,
        paths: &[AttrPathIn],
        read_ctx: &ReadCtx,
        budget: usize,
        subscription_id: Option<u32>,
    ) -> Vec<Vec<u8>> {
        let entries = self.read_entries(paths, read_ctx);
        let mut batches: Vec<Vec<ReportEntryOut>> = Vec::new();
        let mut current: Vec<ReportEntryOut> = Vec::new();
        for entry in entries {
            let mut candidate = current.clone();
            candidate.push(entry.clone());
            // Probe with `more_chunks=true` (the shape every non-final
            // batch is actually encoded with below — `MoreChunkedMessages`
            // adds a 2-byte TLV element that `more_chunks=false` doesn't
            // have) rather than the smaller final-chunk shape. Probing
            // with the smaller shape could let a batch through whose real
            // non-final encoding then lands 1-2 bytes over `budget` (fix
            // round 1, code review). Only the *last* batch ends up encoded
            // smaller (`more_chunks=false`) than what it was probed at —
            // strictly safe, since a batch that already fit the larger
            // probed shape still fits the smaller final one.
            let candidate_len =
                im::encode_report_data_entries(&candidate, false, subscription_id, true).len();
            if candidate_len > budget && !current.is_empty() {
                batches.push(std::mem::take(&mut current));
                current.push(entry);
            } else {
                current = candidate;
            }
        }
        batches.push(current); // always ≥1 batch, even for zero entries

        let last = batches.len() - 1;
        batches
            .into_iter()
            .enumerate()
            .map(|(i, batch)| {
                let is_last = i == last;
                // Priming (`subscription_id.is_some()`): never suppress —
                // the SubscribeResponse still has to follow on this
                // exchange, so the initiator must answer even the last
                // chunk with `StatusResponse(0)`.
                let suppress = is_last && subscription_id.is_none();
                im::encode_report_data_entries(&batch, suppress, subscription_id, !is_last)
            })
            .collect()
    }

    /// Expands every `AttrPathIn` in `paths` (wildcard endpoint/cluster/
    /// attribute fields included) against the registry into concrete
    /// report entries. Also the entry point Task 6/12 (subscriptions) will
    /// reuse for priming/dirty reports.
    ///
    /// Wildcard expansion rule (mirrors spec §8.9.2.3's path-resolution
    /// semantics at the level this skeleton needs): a field left wildcard
    /// (`None`) always expands to every matching registry entry, silently
    /// (no report, no error) when a resolved-but-lower-level lookup comes
    /// up empty — e.g. a wildcard-cluster expansion landing on an endpoint
    /// that doesn't implement some concrete attribute just contributes
    /// nothing for that combination. A field that was itself concrete
    /// (`Some`) and fails to resolve, with every *more significant* field
    /// in the same path also concrete, is reported as a per-path
    /// `ReportEntryOut::Status` instead: `UNSUPPORTED_ENDPOINT` /
    /// `UNSUPPORTED_CLUSTER` / `UNSUPPORTED_ATTRIBUTE`. Global attributes
    /// (`ATTR_CLUSTER_REVISION` etc., spec §7.13) are only ever answered
    /// when concretely requested — wildcard attribute expansion enumerates
    /// `ClusterHandler::attributes()` alone, keeping full-wildcard reads
    /// from ballooning (chip-tool/Echo commonly issue one).
    pub fn read_entries(&self, paths: &[AttrPathIn], read_ctx: &ReadCtx) -> Vec<ReportEntryOut> {
        let mut out = Vec::new();
        for path in paths {
            self.expand_endpoint(path, read_ctx, &mut out);
        }
        out
    }

    fn expand_endpoint(
        &self,
        path: &AttrPathIn,
        read_ctx: &ReadCtx,
        out: &mut Vec<ReportEntryOut>,
    ) {
        match path.endpoint {
            Some(endpoint) => match self.endpoints.iter().find(|(id, _)| *id == endpoint) {
                Some((_, clusters)) => {
                    let ectx = ExpandCtx {
                        endpoint,
                        clusters,
                        read_ctx,
                    };
                    self.expand_cluster(&ectx, path, true, out)
                }
                None => out.push(ReportEntryOut::Status {
                    endpoint,
                    cluster: path.cluster.unwrap_or(0),
                    attribute: path.attribute.unwrap_or(0),
                    status: im::STATUS_UNSUPPORTED_ENDPOINT,
                }),
            },
            None => {
                for (endpoint, clusters) in &self.endpoints {
                    let ectx = ExpandCtx {
                        endpoint: *endpoint,
                        clusters,
                        read_ctx,
                    };
                    self.expand_cluster(&ectx, path, false, out);
                }
            }
        }
    }

    /// `endpoint_concrete`: whether `ectx.endpoint` was resolved from a
    /// concrete path field (`true`) or a wildcard expansion (`false`) —
    /// determines whether an unresolvable *concrete* cluster on this
    /// endpoint is a per-path error (`endpoint_concrete` path) or just
    /// skipped (wildcard endpoint expansion landing on an endpoint without
    /// this cluster).
    fn expand_cluster(
        &self,
        ectx: &ExpandCtx,
        path: &AttrPathIn,
        endpoint_concrete: bool,
        out: &mut Vec<ReportEntryOut>,
    ) {
        match path.cluster {
            Some(cluster) => match ectx.clusters.iter().find(|h| h.cluster_id() == cluster) {
                Some(handler) => {
                    self.expand_attribute(ectx, handler.as_ref(), path, endpoint_concrete, out)
                }
                None => {
                    if endpoint_concrete {
                        out.push(ReportEntryOut::Status {
                            endpoint: ectx.endpoint,
                            cluster,
                            attribute: path.attribute.unwrap_or(0),
                            status: im::STATUS_UNSUPPORTED_CLUSTER,
                        });
                    }
                }
            },
            None => {
                for handler in ectx.clusters {
                    self.expand_attribute(ectx, handler.as_ref(), path, false, out);
                }
            }
        }
    }

    /// `concrete_so_far`: `true` only when both `ectx.endpoint` and the
    /// cluster were resolved from concrete path fields — the precondition
    /// (spec §8.9.2.3) for a missing concrete attribute to be a per-path
    /// `UNSUPPORTED_ATTRIBUTE` rather than silently dropped.
    fn expand_attribute(
        &self,
        ectx: &ExpandCtx,
        handler: &dyn ClusterHandler,
        path: &AttrPathIn,
        concrete_so_far: bool,
        out: &mut Vec<ReportEntryOut>,
    ) {
        let cluster = handler.cluster_id();
        match path.attribute {
            Some(attribute) => match self.read_attribute_value(ectx, handler, attribute) {
                Some(value_tlv) => out.push(ReportEntryOut::Data(AttrReportOut {
                    endpoint: ectx.endpoint,
                    cluster,
                    attribute,
                    data_version: self.data_version(ectx.endpoint, cluster),
                    value_tlv,
                })),
                None if concrete_so_far => out.push(ReportEntryOut::Status {
                    endpoint: ectx.endpoint,
                    cluster,
                    attribute,
                    status: im::STATUS_UNSUPPORTED_ATTRIBUTE,
                }),
                // Wildcard-expanded attribute that resolved to nothing:
                // shouldn't happen (only ids from `attributes()` reach here
                // in the wildcard branch below), but dropped defensively
                // rather than reported.
                None => {}
            },
            None => {
                // Wildcard attribute: enumerate the cluster's own
                // attributes only — global attributes are deliberately
                // excluded from wildcard expansion (see `read_entries`'s
                // doc).
                for attribute in handler.attributes() {
                    if let Some(value_tlv) = self.read_attribute_value(ectx, handler, attribute) {
                        out.push(ReportEntryOut::Data(AttrReportOut {
                            endpoint: ectx.endpoint,
                            cluster,
                            attribute,
                            data_version: self.data_version(ectx.endpoint, cluster),
                            value_tlv,
                        }));
                    }
                    // `None`: `attributes()` promised this id but `read`
                    // disagrees — dropped silently (brief: defensive, not
                    // expected to happen).
                }
            }
        }
    }

    /// Reads one concrete (endpoint, cluster, attribute), intercepting the
    /// handful of attributes `Node` — not the per-cluster handler — owns:
    /// ServerList/endpoint-0 PartsList (need registry-wide visibility, see
    /// below) and the five global attributes (spec §7.13, synthesized
    /// uniformly for every cluster rather than duplicated into each
    /// `ClusterHandler::read`).
    fn read_attribute_value(
        &self,
        ectx: &ExpandCtx,
        handler: &dyn ClusterHandler,
        attribute: u32,
    ) -> Option<Vec<u8>> {
        let cluster = handler.cluster_id();
        // ServerList (spec §9.5) must reflect the clusters actually
        // registered on this endpoint — including ones added after
        // `with_root_endpoint` (e.g. a device runtime's commissioning
        // clusters via `add_cluster`), so it's derived here from the
        // registry rather than left to `DescriptorHandler`'s own `read`
        // (which has no visibility into its siblings). Endpoint 0's
        // PartsList (spec §9.5, "the endpoint composition tree") is the
        // same story one level up: it must list every *other* endpoint
        // registered on this `Node`, which `DescriptorHandler` — scoped to
        // a single endpoint's own cluster list — has no way to see either.
        // Non-0 endpoints (M2: only endpoint 1) have no children of their
        // own, so their PartsList stays `DescriptorHandler`'s own
        // always-empty answer.
        if cluster == im::CLUSTER_DESCRIPTOR && attribute == im::ATTR_SERVER_LIST {
            return Some(encode_server_list(ectx.clusters));
        }
        if cluster == im::CLUSTER_DESCRIPTOR
            && attribute == im::ATTR_PARTS_LIST
            && ectx.endpoint == 0
        {
            return Some(encode_parts_list(&self.endpoints));
        }
        match attribute {
            im::ATTR_CLUSTER_REVISION => Some(uint_value(u64::from(handler.revision()))),
            im::ATTR_FEATURE_MAP => Some(uint_value(u64::from(handler.feature_map()))),
            im::ATTR_ATTRIBUTE_LIST => Some(encode_attribute_list(handler)),
            im::ATTR_ACCEPTED_COMMAND_LIST => {
                Some(encode_command_list(&handler.accepted_commands()))
            }
            im::ATTR_GENERATED_COMMAND_LIST => {
                Some(encode_command_list(&handler.generated_commands()))
            }
            _ => handler.read(attribute, ectx.read_ctx),
        }
    }

    fn handle_invoke(
        &mut self,
        payload: &[u8],
        ctx: &mut InvokeCtx,
    ) -> Result<ImOutcome, ImServerError> {
        let req = im::decode_invoke_request(payload)?;
        let Some((_, clusters)) = self
            .endpoints
            .iter_mut()
            .find(|(id, _)| *id == req.endpoint)
        else {
            return Ok(ImOutcome::unchanged(
                im::OPCODE_INVOKE_RESPONSE,
                im::encode_invoke_response_status(
                    req.endpoint,
                    req.cluster,
                    req.command,
                    im::STATUS_UNSUPPORTED_ENDPOINT,
                    None,
                ),
            ));
        };
        let Some(handler) = clusters.iter_mut().find(|h| h.cluster_id() == req.cluster) else {
            return Ok(ImOutcome::unchanged(
                im::OPCODE_INVOKE_RESPONSE,
                im::encode_invoke_response_status(
                    req.endpoint,
                    req.cluster,
                    req.command,
                    im::STATUS_UNSUPPORTED_CLUSTER,
                    None,
                ),
            ));
        };
        // Each `invoke` gets a fresh change list: `ctx` is per-session
        // scratch (`attestation_challenge` outlives one command), so a
        // leftover `changed` from an earlier command in the same session
        // must not be re-reported as this one's.
        ctx.changed.clear();
        let reply = handler.invoke(req.command, &req.fields_tlv, ctx);
        // The handler reports bare attribute ids (it only knows its own
        // cluster); pair them with the endpoint/cluster it was dispatched
        // to, and bump this cluster's DataVersion once if anything changed.
        let changed: Vec<(u16, u32, u32)> = ctx
            .changed
            .drain(..)
            .map(|attribute| (req.endpoint, req.cluster, attribute))
            .collect();
        if !changed.is_empty() {
            let version = self
                .versions
                .entry((req.endpoint, req.cluster))
                .or_insert(self.version_base);
            *version = version.wrapping_add(1);
        }
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
            let Some((_, clusters)) = self.endpoints.iter_mut().find(|(id, _)| *id == endpoint)
            else {
                results.push((
                    endpoint,
                    cluster,
                    attribute,
                    im::STATUS_UNSUPPORTED_ENDPOINT,
                ));
                continue;
            };
            let Some(handler) = clusters.iter_mut().find(|h| h.cluster_id() == cluster) else {
                results.push((endpoint, cluster, attribute, im::STATUS_UNSUPPORTED_CLUSTER));
                continue;
            };
            // Same rationale as `handle_invoke`: `ctx` is per-session
            // scratch, so a leftover `changed` from an earlier write/invoke
            // in the same session must not be re-reported as this one's.
            ctx.changed.clear();
            let status = match handler.write(attribute, &write.data_tlv, write.list_append, ctx) {
                Ok(()) => im::STATUS_SUCCESS,
                Err(status) => status,
            };
            let entry_changed: Vec<(u16, u32, u32)> = ctx
                .changed
                .drain(..)
                .map(|attr| (endpoint, cluster, attr))
                .collect();
            if !entry_changed.is_empty() {
                let version = self
                    .versions
                    .entry((endpoint, cluster))
                    .or_insert(self.version_base);
                *version = version.wrapping_add(1);
            }
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

/// Encodes a scalar as one standalone, `Tag::Anonymous`-tagged TLV element
/// (the `ClusterHandler::read` contract).
fn uint_value(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_uint(Tag::Anonymous, v);
    w.finish()
}

fn str_value(v: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_str(Tag::Anonymous, v);
    w.finish()
}

/// Encodes the Descriptor cluster's `ServerList` (spec §9.5) from the
/// clusters actually registered on the endpoint — see
/// `Node::read_attribute_value`'s override for why this lives here rather
/// than in `DescriptorHandler`.
fn encode_server_list(clusters: &[Box<dyn ClusterHandler>]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for handler in clusters {
        w.put_uint(Tag::Anonymous, u64::from(handler.cluster_id()));
    }
    w.end_container();
    w.finish()
}

/// Encodes the Descriptor cluster's `PartsList` (spec §9.5) for endpoint 0:
/// every *other* endpoint id registered on the `Node`, in registration
/// order — see `Node::read_attribute_value`'s override for why this lives
/// here rather than in `DescriptorHandler` (which only knows its own
/// endpoint).
fn encode_parts_list(endpoints: &[(u16, Vec<Box<dyn ClusterHandler>>)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (id, _) in endpoints {
        if *id != 0 {
            w.put_uint(Tag::Anonymous, u64::from(*id));
        }
    }
    w.end_container();
    w.finish()
}

/// Encodes the global `AttributeList` attribute (spec §7.13, id
/// `ATTR_ATTRIBUTE_LIST`): every attribute id the cluster serves —
/// `handler.attributes()`'s cluster-specific ids plus the five global ids
/// every cluster carries (including `AttributeList`'s own id).
fn encode_attribute_list(handler: &dyn ClusterHandler) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for id in handler.attributes() {
        w.put_uint(Tag::Anonymous, u64::from(id));
    }
    for id in GLOBAL_ATTRIBUTE_IDS {
        w.put_uint(Tag::Anonymous, u64::from(id));
    }
    w.end_container();
    w.finish()
}

/// The five global attributes (spec §7.13) every cluster carries, in
/// addition to whatever `ClusterHandler::attributes()` declares.
const GLOBAL_ATTRIBUTE_IDS: [u32; 5] = [
    im::ATTR_GENERATED_COMMAND_LIST,
    im::ATTR_ACCEPTED_COMMAND_LIST,
    im::ATTR_ATTRIBUTE_LIST,
    im::ATTR_FEATURE_MAP,
    im::ATTR_CLUSTER_REVISION,
];

/// `AcceptedCommandList`/`GeneratedCommandList` (spec §7.13): the cluster's
/// `ClusterHandler::accepted_commands`/`generated_commands` answer, as a
/// TLV array of command ids.
fn encode_command_list(ids: &[u32]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for id in ids {
        w.put_uint(Tag::Anonymous, u64::from(*id));
    }
    w.end_container();
    w.finish()
}

/// Descriptor cluster (spec §9.5), mandatory on every endpoint. Carries the
/// endpoint's `DeviceTypeList` entries (`device_types` — `DEVICE_TYPE_ROOT_NODE`
/// on endpoint 0, `DEVICE_TYPE_ON_OFF_LIGHT` on endpoint 1; a bridged endpoint
/// carries two entries — its "real" device type plus `DEVICE_TYPE_BRIDGED_NODE`,
/// spec §9.13) since that's the one piece of per-endpoint Descriptor state
/// this flat, non-composed data model needs; `ServerList`/endpoint-0
/// `PartsList` are derived from the registry by `Node::read_attribute_value`
/// instead (see its doc comment) because they depend on sibling/other-endpoint
/// state this handler can't see.
pub struct DescriptorHandler {
    device_types: Vec<u32>,
    /// この endpoint 自身の PartsList（EP0 以外用 — EP0 は従来どおり
    /// `Node::read_attribute_value` が registry から導出して intercept）。
    /// EP1 Aggregator が bridged EP 群を静的に持つ（設定反映は再起動のみ
    /// なので動的導出は不要 — YAGNI）。
    parts: Vec<u16>,
}

impl DescriptorHandler {
    /// A Descriptor handler for an endpoint whose `DeviceTypeList` is the
    /// single entry `device_type` (revision 1 — M2 scope has no device type
    /// revisions beyond the first), with an empty `PartsList`.
    pub fn for_device(device_type: u32) -> Self {
        Self {
            device_types: vec![device_type],
            parts: Vec::new(),
        }
    }

    /// A Descriptor handler for an endpoint whose `DeviceTypeList` carries
    /// multiple entries (each revision 1) — a bridged endpoint's "real"
    /// device type plus `DEVICE_TYPE_BRIDGED_NODE`, per spec §9.13.
    pub fn for_device_types(device_types: &[u32]) -> Self {
        Self {
            device_types: device_types.to_vec(),
            parts: Vec::new(),
        }
    }

    /// Builder: sets a static `PartsList` — the Aggregator endpoint's
    /// bridged children. Only meaningful on a non-zero endpoint (endpoint
    /// 0's `PartsList` is always derived by `Node::read_attribute_value`,
    /// which intercepts it before this handler's `read` runs).
    pub fn with_parts(mut self, parts: Vec<u16>) -> Self {
        self.parts = parts;
        self
    }
}

impl ClusterHandler for DescriptorHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_DESCRIPTOR
    }

    /// ClusterRevision (spec §7.13): Descriptor cluster spec revision 2
    /// (Matter 1.4).
    fn revision(&self) -> u16 {
        2
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_DEVICE_TYPE_LIST,
            im::ATTR_SERVER_LIST,
            im::ATTR_PARTS_LIST,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_DEVICE_TYPE_LIST => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                for device_type in &self.device_types {
                    w.start_struct(Tag::Anonymous); // DeviceTypeStruct
                    w.put_uint(Tag::Context(0), u64::from(*device_type));
                    w.put_uint(Tag::Context(1), 1); // Revision
                    w.end_container();
                }
                w.end_container();
                Some(w.finish())
            }
            // ATTR_SERVER_LIST, and ATTR_PARTS_LIST on endpoint 0, are
            // intercepted and derived from the `Node`'s registry by
            // `Node::read_attribute_value` — never reach here (see that
            // override's doc comment). This is endpoint != 0's PartsList
            // (`self.parts` — empty unless `with_parts` set it, as on the
            // EP1 Aggregator) and endpoint 0's own fallback, which
            // `read_attribute_value` never takes.
            im::ATTR_PARTS_LIST => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                for id in &self.parts {
                    w.put_uint(Tag::Anonymous, u64::from(*id));
                }
                w.end_container();
                Some(w.finish())
            }
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // Descriptor declares no commands.
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }
}

/// Persistence boundary `core` calls through instead of touching a
/// filesystem directly — same shape as `core::access_control::AclPersist`/
/// `core::fabric_store::FabricPersist` (see those traits' docs for the
/// `: Send`/object-safety rationale), but save-only: NodeLabel/Location's
/// *initial* value is whatever the constructor is handed (loaded by the
/// caller, e.g. `net::store::load_basic_info`), not something this trait
/// loads itself. A save failure is logged and ignored, same disposition as
/// `AclPersist` (see `BasicInformationHandler::persist_state`'s doc).
pub trait BasicInfoPersist: Send {
    fn save(&self, node_label: &str, location: &str) -> Result<(), String>;
}

/// BasicInformation cluster (spec §11.1), mandatory on endpoint 0. Task 5
/// filled in every attribute Apple Home's post-commissioning interview
/// reads (beyond the identity attributes M1 already served); Task 10 adds
/// disk persistence for the two writable ones (NodeLabel/Location) via an
/// injected `BasicInfoPersist` — everything else here is either a fixed
/// value or, for UniqueID, whatever the constructor was handed
/// (`device::Device::new` threads through the value persisted at
/// `<store_dir>/unique_id`, a separate file predating this task).
struct BasicInformationHandler {
    vendor_id: u16,
    product_id: u16,
    unique_id: String,
    /// NodeLabel (spec §11.1.6.2) — writable, persisted via `persist` on
    /// change. `write` mutates it directly since `Node::handle_write`
    /// already gives every `ClusterHandler::write` call `&mut self`.
    node_label: String,
    /// Location (spec §11.1.6.6, `CountryCode`) — writable, persisted via
    /// `persist` on change. Spec default `"XX"` (unknown/unset).
    location: String,
    /// Save backend for NodeLabel/Location — `None` for the ~15 existing
    /// `with_root_endpoint`/`with_root_endpoint_unique` call sites that
    /// never asked for persistence.
    persist: Option<Box<dyn BasicInfoPersist>>,
}

impl BasicInformationHandler {
    /// Saves the current NodeLabel/Location to `persist`, if any (no-op for
    /// the non-persisted constructors). Called only when a write actually
    /// changes a value — dedup writes never reach here. A save failure is
    /// `tracing::warn`ed and otherwise ignored: the write that triggered it
    /// has already succeeded and updated in-memory state, which stays
    /// authoritative; the next write to either attribute retries the save
    /// (same disposition `AclStore::save` documents for `AclPersist`).
    fn persist_state(&self) {
        if let Some(persist) = &self.persist {
            if let Err(e) = persist.save(&self.node_label, &self.location) {
                tracing::warn!("basic information store save failed: {e}");
            }
        }
    }
}

/// CaseSessionsPerFabric/SubscriptionsPerFabric (spec §11.1.6.16,
/// CapabilityMinimaStruct fields, context tags 0/1) — fixed floor values
/// `mat-device` comfortably supports; not tracked against any real
/// resource-exhaustion path (M2/M3 scope never gets close).
const CAPABILITY_MINIMA_CASE_SESSIONS_PER_FABRIC: u64 = 3;
const CAPABILITY_MINIMA_SUBSCRIPTIONS_PER_FABRIC: u64 = 3;

/// SpecificationVersion (spec §11.1.6.18, attribute id 0x0015): Matter 1.4,
/// encoded per spec §7.1.9 as `(major << 24) | (minor << 16)`.
const SPECIFICATION_VERSION: u64 = 0x0104_0000;

/// NodeLabel's upper bound (spec §11.1.6.2, `string32`) — measured in UTF-8
/// **characters**, not bytes.
const NODE_LABEL_MAX_CHARS: usize = 32;

/// Location's fixed length (spec §11.1.6.6, `CountryCode` — ISO 3166-1
/// alpha-2, or `"XX"` for unset/unknown) — measured in UTF-8 **characters**,
/// not bytes. Unlike NodeLabel this isn't an upper bound: any length other
/// than exactly 2 is rejected.
const LOCATION_CHARS: usize = 2;

/// Decodes a write payload expected to be a single anonymous UTF-8 TLV
/// element — the shape both NodeLabel and Location writes take. Shared by
/// `BasicInformationHandler::write`'s two branches so the
/// malformed-TLV/wrong-type rejection (`STATUS_CONSTRAINT_ERROR`) is
/// written once.
fn decode_utf8_write(data_tlv: &[u8]) -> Result<String, u8> {
    let mut r = Reader::new(data_tlv);
    let Ok(Some(element)) = r.next() else {
        return Err(im::STATUS_CONSTRAINT_ERROR);
    };
    let Value::Utf8(s) = element.value else {
        return Err(im::STATUS_CONSTRAINT_ERROR);
    };
    Ok(s.to_string())
}

impl ClusterHandler for BasicInformationHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_BASIC_INFORMATION
    }

    /// ClusterRevision (spec §7.13): Basic Information cluster spec
    /// revision 3 (Matter 1.4).
    fn revision(&self) -> u16 {
        3
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_DATA_MODEL_REVISION,
            im::ATTR_VENDOR_ID,
            im::ATTR_PRODUCT_ID,
            im::ATTR_VENDOR_NAME,
            im::ATTR_PRODUCT_NAME,
            im::ATTR_BI_NODE_LABEL,
            im::ATTR_BI_LOCATION,
            im::ATTR_BI_HARDWARE_VERSION,
            im::ATTR_BI_HARDWARE_VERSION_STRING,
            im::ATTR_BI_SOFTWARE_VERSION,
            im::ATTR_BI_SOFTWARE_VERSION_STRING,
            im::ATTR_BI_UNIQUE_ID,
            im::ATTR_BI_CAPABILITY_MINIMA,
            im::ATTR_BI_SPECIFICATION_VERSION,
            im::ATTR_BI_MAX_PATHS_PER_INVOKE,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_DATA_MODEL_REVISION => Some(uint_value(DATA_MODEL_REVISION)),
            im::ATTR_VENDOR_ID => Some(uint_value(u64::from(self.vendor_id))),
            im::ATTR_PRODUCT_ID => Some(uint_value(u64::from(self.product_id))),
            im::ATTR_VENDOR_NAME => Some(str_value("mat")),
            im::ATTR_PRODUCT_NAME => Some(str_value("matv")),
            im::ATTR_BI_NODE_LABEL => Some(str_value(&self.node_label)),
            im::ATTR_BI_LOCATION => Some(str_value(&self.location)),
            im::ATTR_BI_HARDWARE_VERSION => Some(uint_value(1)),
            im::ATTR_BI_HARDWARE_VERSION_STRING => Some(str_value("matv")),
            im::ATTR_BI_SOFTWARE_VERSION => Some(uint_value(1)),
            im::ATTR_BI_SOFTWARE_VERSION_STRING => Some(str_value(env!("CARGO_PKG_VERSION"))),
            im::ATTR_BI_UNIQUE_ID => Some(str_value(&self.unique_id)),
            im::ATTR_BI_CAPABILITY_MINIMA => Some(encode_capability_minima()),
            im::ATTR_BI_SPECIFICATION_VERSION => Some(uint_value(SPECIFICATION_VERSION)),
            im::ATTR_BI_MAX_PATHS_PER_INVOKE => Some(uint_value(1)),
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // BasicInformation declares no commands.
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }

    /// NodeLabel and Location are the two writable attributes (spec
    /// §11.1.6.2/§11.1.6.6); every other BasicInformation attribute keeps
    /// the default `write` (`STATUS_UNSUPPORTED_WRITE`). Both branches
    /// dedup (a write equal to the current value is `Ok(())` but neither
    /// reports `ctx.changed` nor calls `persist.save` — Apple Home's
    /// post-commissioning interview writes these unconditionally on every
    /// connect, and a same-value write is not a change worth a dirty
    /// report or a disk write) and, on an actual change, persist the new
    /// `(node_label, location)` pair (`persist_state`'s doc covers save
    /// failure handling). `list_append` is irrelevant to either — neither
    /// is a list — so it's ignored, matching the trait doc's guidance for
    /// clusters with no list attribute.
    fn write(
        &mut self,
        attribute: u32,
        data_tlv: &[u8],
        _list_append: bool,
        ctx: &mut InvokeCtx,
    ) -> Result<(), u8> {
        match attribute {
            im::ATTR_BI_NODE_LABEL => {
                let s = decode_utf8_write(data_tlv)?;
                if s.chars().count() > NODE_LABEL_MAX_CHARS {
                    return Err(im::STATUS_CONSTRAINT_ERROR);
                }
                if s == self.node_label {
                    return Ok(());
                }
                self.node_label = s;
                ctx.changed.push(im::ATTR_BI_NODE_LABEL);
                self.persist_state();
                Ok(())
            }
            im::ATTR_BI_LOCATION => {
                let s = decode_utf8_write(data_tlv)?;
                if s.chars().count() != LOCATION_CHARS {
                    return Err(im::STATUS_CONSTRAINT_ERROR);
                }
                if s == self.location {
                    return Ok(());
                }
                self.location = s;
                ctx.changed.push(im::ATTR_BI_LOCATION);
                self.persist_state();
                Ok(())
            }
            _ => Err(im::STATUS_UNSUPPORTED_WRITE),
        }
    }
}

/// CapabilityMinima (spec §11.1.6.16, attribute id 0x0013): a
/// `CapabilityMinimaStruct{CaseSessionsPerFabric: uint16, Subscriptions
/// PerFabric: uint16}`, context tags 0/1 in field-declaration order.
fn encode_capability_minima() -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), CAPABILITY_MINIMA_CASE_SESSIONS_PER_FABRIC);
    w.put_uint(Tag::Context(1), CAPABILITY_MINIMA_SUBSCRIPTIONS_PER_FABRIC);
    w.end_container();
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::im::{decode_invoke_response, decode_report_data_message};

    /// `handle_im` with the default contexts, unwrapped down to the
    /// `(opcode, payload)` pair almost every test here asserts on — these
    /// tests predate `ImOutcome`'s `changed` field (Task 12) and none of
    /// them are about it, so they keep reading as the two-value assertions
    /// they always were. The change-set-aware tests below call `handle_im`
    /// directly instead.
    fn handle_im_ok(node: &mut Node, opcode: u8, payload: &[u8]) -> (u8, Vec<u8>) {
        let out = node
            .handle_im(
                opcode,
                payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        (out.opcode, out.payload)
    }

    #[test]
    fn read_basic_information_data_model_revision() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_read_request(
            0,
            im::CLUSTER_BASIC_INFORMATION,
            im::ATTR_DATA_MODEL_REVISION,
        );
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        assert_eq!(opcode, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(msg.reports[0].endpoint, Some(0));
        assert_eq!(msg.reports[0].cluster, Some(im::CLUSTER_BASIC_INFORMATION));
        assert_eq!(msg.reports[0].attribute, Some(im::ATTR_DATA_MODEL_REVISION));
        assert_eq!(
            msg.reports[0].data,
            Some(serde_json::json!(DATA_MODEL_REVISION))
        );
    }

    /// Task 5: BasicInformation's remaining mandatory attributes (spec
    /// §11.1.6) — Apple Home's post-commissioning interview reads every one
    /// of these, not just the identity attributes the earlier test above
    /// covers.
    #[test]
    fn read_basic_information_task5_attributes_have_required_values() {
        let mut node = Node::with_root_endpoint_unique(0xFFF1, 0x8000, "unique-abc123");
        let read = |node: &mut Node, attribute: u32| -> serde_json::Value {
            let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, attribute);
            let (_, payload) = handle_im_ok(node, im::OPCODE_READ_REQUEST, &req);
            let msg = decode_report_data_message(&payload).unwrap();
            msg.reports[0].data.clone().unwrap()
        };

        assert_eq!(
            read(&mut node, im::ATTR_BI_NODE_LABEL),
            serde_json::json!("")
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_LOCATION),
            serde_json::json!("XX")
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_HARDWARE_VERSION),
            serde_json::json!(1)
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_HARDWARE_VERSION_STRING),
            serde_json::json!("matv")
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_SOFTWARE_VERSION),
            serde_json::json!(1)
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_SOFTWARE_VERSION_STRING),
            serde_json::json!(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_UNIQUE_ID),
            serde_json::json!("unique-abc123")
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_CAPABILITY_MINIMA),
            serde_json::json!({"0": 3, "1": 3})
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_SPECIFICATION_VERSION),
            serde_json::json!(0x0104_0000u32)
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_MAX_PATHS_PER_INVOKE),
            serde_json::json!(1)
        );
    }

    /// `with_root_endpoint` (existing signature, ~15 call sites) delegates
    /// to `with_root_endpoint_unique(.., "matv-dev")` — same fixed UniqueID
    /// as before this task, just no longer the only way to set it.
    #[test]
    fn with_root_endpoint_uses_fixed_fallback_unique_id() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_UNIQUE_ID);
        let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!("matv-dev")));
    }

    /// NodeLabel is the one BasicInformation attribute Apple Home writes
    /// (spec §11.1.6.2) — a write must both be readable back and reported
    /// via `ImOutcome::changed` (mirrors the OnOff `changed` contract in
    /// the Task 12 tests above).
    #[test]
    fn node_label_write_then_read_reflects_change_and_reports_changed() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let mut data = Writer::new();
        data.put_str(Tag::Anonymous, "Living Room");
        let payload = im::encode_write_request_tlv(
            0,
            im::CLUSTER_BASIC_INFORMATION,
            im::ATTR_BI_NODE_LABEL,
            &data.finish(),
        );
        let outcome = node
            .handle_im(
                im::OPCODE_WRITE_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert_eq!(
            im::decode_write_response(&outcome.payload).unwrap(),
            im::STATUS_SUCCESS
        );
        assert_eq!(
            outcome.changed,
            vec![(0u16, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_NODE_LABEL)]
        );

        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_NODE_LABEL);
        let (_, read_payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&read_payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!("Living Room")));
    }

    /// NodeLabel is capped at 32 UTF-8 *characters* (spec §11.1.6.2's
    /// `string32` type) — a 33-character write is rejected wholesale
    /// (CONSTRAINT_ERROR), not truncated.
    #[test]
    fn node_label_write_over_32_chars_is_constraint_error() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let mut data = Writer::new();
        data.put_str(Tag::Anonymous, &"a".repeat(33));
        let payload = im::encode_write_request_tlv(
            0,
            im::CLUSTER_BASIC_INFORMATION,
            im::ATTR_BI_NODE_LABEL,
            &data.finish(),
        );
        let (_, resp) = handle_im_ok(&mut node, im::OPCODE_WRITE_REQUEST, &payload);
        assert_eq!(
            im::decode_write_response(&resp).unwrap(),
            im::STATUS_CONSTRAINT_ERROR
        );
    }

    /// Writes a single anonymous UTF-8 TLV element to `attribute` and
    /// returns the resulting `ImOutcome` — shared by the Task 10
    /// NodeLabel/Location tests below (dedup, Location constraint, persist)
    /// so each test body is just the assertions.
    fn write_bi_str(node: &mut Node, attribute: u32, value: &str) -> ImOutcome {
        let mut data = Writer::new();
        data.put_str(Tag::Anonymous, value);
        let payload = im::encode_write_request_tlv(
            0,
            im::CLUSTER_BASIC_INFORMATION,
            attribute,
            &data.finish(),
        );
        node.handle_im(
            im::OPCODE_WRITE_REQUEST,
            &payload,
            &mut InvokeCtx::default(),
            &ReadCtx::default(),
        )
        .unwrap()
    }

    /// A write equal to the current NodeLabel is `Ok` but must not appear
    /// in `ImOutcome::changed` — Apple Home's post-commissioning interview
    /// writes NodeLabel unconditionally on every connect, and a same-value
    /// write is not a change worth a dirty report (brief's "無変化 dirty
    /// レポートの抑止").
    #[test]
    fn node_label_write_same_value_is_dedup_noop() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let first = write_bi_str(&mut node, im::ATTR_BI_NODE_LABEL, "Living Room");
        assert_eq!(
            first.changed,
            vec![(0u16, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_NODE_LABEL)]
        );
        let second = write_bi_str(&mut node, im::ATTR_BI_NODE_LABEL, "Living Room");
        assert_eq!(
            im::decode_write_response(&second.payload).unwrap(),
            im::STATUS_SUCCESS
        );
        assert!(second.changed.is_empty());
    }

    /// Location (spec §11.1.6.6) is writable, reflects on read, and reports
    /// `ImOutcome::changed` on an actual change — same contract as
    /// NodeLabel's existing test above.
    #[test]
    fn location_write_then_read_reflects_change_and_reports_changed() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let outcome = write_bi_str(&mut node, im::ATTR_BI_LOCATION, "JP");
        assert_eq!(
            im::decode_write_response(&outcome.payload).unwrap(),
            im::STATUS_SUCCESS
        );
        assert_eq!(
            outcome.changed,
            vec![(0u16, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_LOCATION)]
        );

        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_LOCATION);
        let (_, read_payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&read_payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!("JP")));
    }

    /// Location must be exactly 2 UTF-8 characters (spec §11.1.6.6
    /// `CountryCode`) — both a 3-character and a 1-character write are
    /// rejected wholesale (CONSTRAINT_ERROR), not truncated/padded.
    #[test]
    fn location_write_wrong_length_is_constraint_error() {
        for value in ["JPN", "J"] {
            let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
            let outcome = write_bi_str(&mut node, im::ATTR_BI_LOCATION, value);
            assert_eq!(
                im::decode_write_response(&outcome.payload).unwrap(),
                im::STATUS_CONSTRAINT_ERROR,
                "value {value:?} should be rejected"
            );
        }
    }

    /// Same dedup contract as `node_label_write_same_value_is_dedup_noop`,
    /// for Location.
    #[test]
    fn location_write_same_value_is_dedup_noop() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let first = write_bi_str(&mut node, im::ATTR_BI_LOCATION, "JP");
        assert_eq!(
            first.changed,
            vec![(0u16, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_LOCATION)]
        );
        let second = write_bi_str(&mut node, im::ATTR_BI_LOCATION, "JP");
        assert_eq!(
            im::decode_write_response(&second.payload).unwrap(),
            im::STATUS_SUCCESS
        );
        assert!(second.changed.is_empty());
    }

    /// Test-only `BasicInfoPersist`: records every `save` call's
    /// `(node_label, location)` pair — lets a test assert both "a real
    /// change delivers the new values" and "a dedup write never calls
    /// save" against the same backing `Vec`.
    struct MemBasicInfoPersist(std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>);

    impl BasicInfoPersist for MemBasicInfoPersist {
        fn save(&self, node_label: &str, location: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push((node_label.to_string(), location.to_string()));
            Ok(())
        }
    }

    /// `with_root_endpoint_persisted` wires NodeLabel/Location writes to
    /// the injected `BasicInfoPersist`: a real change calls `save` with the
    /// new values, and a same-value (dedup) write calls it zero additional
    /// times — persist only fires on an actual change (brief).
    #[test]
    fn basic_info_persist_receives_changes_but_not_dedup_writes() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut node = Node::with_root_endpoint_persisted(
            0xFFF1,
            0x8000,
            "unique-abc123",
            String::new(),
            "XX".to_string(),
            Box::new(MemBasicInfoPersist(std::sync::Arc::clone(&calls))),
        );

        write_bi_str(&mut node, im::ATTR_BI_NODE_LABEL, "Living Room");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("Living Room".to_string(), "XX".to_string())]
        );

        write_bi_str(&mut node, im::ATTR_BI_LOCATION, "JP");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                ("Living Room".to_string(), "XX".to_string()),
                ("Living Room".to_string(), "JP".to_string()),
            ]
        );

        // Same-value writes to both attributes: no additional save calls.
        write_bi_str(&mut node, im::ATTR_BI_NODE_LABEL, "Living Room");
        write_bi_str(&mut node, im::ATTR_BI_LOCATION, "JP");
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    /// `with_root_endpoint_persisted` seeds NodeLabel/Location from its
    /// `node_label`/`location` arguments (what `device::Device::new` loads
    /// via `net::store::load_basic_info`) rather than the `""`/`"XX"`
    /// spec-default fallback `with_root_endpoint_unique` uses.
    #[test]
    fn with_root_endpoint_persisted_seeds_initial_node_label_and_location() {
        let mut node = Node::with_root_endpoint_persisted(
            0xFFF1,
            0x8000,
            "unique-abc123",
            "Living Room".to_string(),
            "JP".to_string(),
            Box::new(MemBasicInfoPersist(std::sync::Arc::new(
                std::sync::Mutex::new(Vec::new()),
            ))),
        );
        let read = |node: &mut Node, attribute: u32| -> serde_json::Value {
            let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, attribute);
            let (_, payload) = handle_im_ok(node, im::OPCODE_READ_REQUEST, &req);
            let msg = decode_report_data_message(&payload).unwrap();
            msg.reports[0].data.clone().unwrap()
        };
        assert_eq!(
            read(&mut node, im::ATTR_BI_NODE_LABEL),
            serde_json::json!("Living Room")
        );
        assert_eq!(
            read(&mut node, im::ATTR_BI_LOCATION),
            serde_json::json!("JP")
        );
    }

    /// ClusterRevision (spec §7.13, id 0xFFFD) must reflect the handler's
    /// own `revision()`, not the M2-era hardcoded 1 — Descriptor's current
    /// revision is 2.
    #[test]
    fn cluster_revision_reflects_handler_value() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_read_request(0, im::CLUSTER_DESCRIPTOR, im::ATTR_CLUSTER_REVISION);
        let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(2)));
    }

    /// DataVersion (spec §7.10.3) starts at whatever base
    /// `set_data_version_base` seeds — not the fixed `INITIAL_DATA_VERSION`
    /// — and still bumps by 1 (wrapping) on the first change, via the
    /// existing write→changed path.
    #[test]
    fn data_version_base_seeds_initial_version_and_bump() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        node.set_data_version_base(0xDEAD_BEEF);
        assert_eq!(
            node.data_version(0, im::CLUSTER_BASIC_INFORMATION),
            0xDEAD_BEEF
        );
        let mut w = Writer::new();
        w.put_str(Tag::Anonymous, "x");
        let req = im::encode_write_request_tlv(
            0,
            im::CLUSTER_BASIC_INFORMATION,
            im::ATTR_BI_NODE_LABEL,
            &w.finish(),
        );
        let _ = handle_im_ok(&mut node, im::OPCODE_WRITE_REQUEST, &req);
        assert_eq!(
            node.data_version(0, im::CLUSTER_BASIC_INFORMATION),
            0xDEAD_BEEF_u32.wrapping_add(1)
        );
    }

    #[test]
    fn read_descriptor_device_type_list_is_root_node() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_read_request(0, im::CLUSTER_DESCRIPTOR, im::ATTR_DEVICE_TYPE_LIST);
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        assert_eq!(opcode, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(
            msg.reports[0].data,
            Some(serde_json::json!([{"0": im::DEVICE_TYPE_ROOT_NODE, "1": 1}]))
        );
    }

    /// M2 behavior (unlike M1): a concrete-path attribute miss no longer
    /// fails the whole read with a top-level `StatusResponse` — it's a
    /// per-path `AttributeStatusIB` inside a normal `ReportData`, so a
    /// batched read can mix successes and failures (spec §8.9.6).
    #[test]
    fn read_unknown_attribute_yields_per_path_status_ib() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, 0xFFFF);
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        assert_eq!(opcode, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(msg.reports[0].data, None);
        assert_eq!(
            msg.reports[0].status,
            Some(im::STATUS_UNSUPPORTED_ATTRIBUTE)
        );
    }

    #[test]
    fn invoke_unknown_cluster_returns_unsupported_cluster() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_invoke_request(0, 0x9999, 0, None);
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_INVOKE_REQUEST, &req);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_UNSUPPORTED_CLUSTER);
    }

    #[test]
    fn invoke_known_cluster_unknown_command_returns_unsupported_command() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_invoke_request(0, im::CLUSTER_BASIC_INFORMATION, 0x7F, None);
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_INVOKE_REQUEST, &req);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_UNSUPPORTED_COMMAND);
    }

    /// クラスタ固有ステータス（spec §8.10.1 の cluster-status フィールド）を
    /// 返せること。AdministratorCommissioning の Busy(2) 等が使う。
    #[test]
    fn invoke_reply_cluster_status_encodes_cluster_specific_code() {
        struct Failing;
        impl ClusterHandler for Failing {
            fn cluster_id(&self) -> u32 {
                0x9999_0003
            }
            fn attributes(&self) -> Vec<u32> {
                vec![]
            }
            fn read(&self, _attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
                None
            }
            fn invoke(
                &mut self,
                _command: u32,
                _fields: &[u8],
                _ctx: &mut InvokeCtx,
            ) -> InvokeReply {
                InvokeReply::ClusterStatus {
                    status: im::STATUS_FAILURE,
                    cluster_status: 2, // Busy
                }
            }
        }
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        node.add_cluster(0, Box::new(Failing));
        let req = im::encode_invoke_request(0, 0x9999_0003, 0, None);
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_INVOKE_REQUEST, &req);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_FAILURE);
        assert_eq!(out.cluster_status, Some(2));
    }

    /// M2 behavior (unlike M1): an opcode this skeleton doesn't implement
    /// (`SubscribeRequest` etc. — `WriteRequest` got its own dispatch, see
    /// `write_to_read_only_attribute_reports_unsupported_write`) is
    /// answered with `StatusResponse(STATUS_INVALID_ACTION)`, not a hard
    /// `Err` — chip-tool/Echo probing an unimplemented feature shouldn't
    /// look like a dropped/malformed exchange.
    #[test]
    fn unsupported_opcode_returns_invalid_action_status() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_SUBSCRIBE_REQUEST, &[]);
        assert_eq!(opcode, im::OPCODE_STATUS_RESPONSE);
        let status = im::decode_status_response(&payload).unwrap();
        assert_eq!(status, im::STATUS_INVALID_ACTION);
    }

    /// Timed Request Action（spec §8.9.4）: TimedRequest には
    /// `StatusResponse(SUCCESS)` を返し、initiator は同一 exchange で
    /// 後続の timed Invoke/Write を送ってくる（後続はステートレスに通常
    /// 経路で処理される）。INVALID_ACTION で返すと Google Play Services
    /// スタック（Android の HA アプリ / Google Home 経由の commissioning）
    /// がそこで中断する（2026-08-18 実測）。期限とフラグ整合の enforcement
    /// は M3 送り。
    #[test]
    fn timed_request_is_acknowledged_with_success_status() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        // TimedRequest: struct{0: timeout-ms}
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 300);
        w.end_container();
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_TIMED_REQUEST, &w.finish());
        assert_eq!(opcode, im::OPCODE_STATUS_RESPONSE);
        let status = im::decode_status_response(&payload).unwrap();
        assert_eq!(status, im::STATUS_SUCCESS);
    }

    #[test]
    fn read_basic_information_vendor_and_product_id() {
        let mut node = Node::with_root_endpoint(0x1234, 0x5678);
        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_VENDOR_ID);
        let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(0x1234)));

        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_PRODUCT_ID);
        let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(0x5678)));
    }

    /// Descriptor's ServerList (spec §9.5) must include every cluster
    /// actually registered on the endpoint — not just the two
    /// `with_root_endpoint` starts with — once a device runtime's
    /// commissioning clusters (`device.rs`: GeneralCommissioning,
    /// OperationalCredentials) are added via `add_cluster`.
    #[test]
    fn descriptor_server_list_reflects_registered_clusters() {
        use mat_controller::commissioning::{
            CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS,
        };

        struct DummyHandler(u32);
        impl ClusterHandler for DummyHandler {
            fn cluster_id(&self) -> u32 {
                self.0
            }
            fn attributes(&self) -> Vec<u32> {
                Vec::new()
            }
            fn read(&self, _attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
                None
            }
            fn invoke(
                &mut self,
                _command: u32,
                _fields_tlv: &[u8],
                _ctx: &mut InvokeCtx,
            ) -> InvokeReply {
                InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
            }
        }

        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        node.add_cluster(0, Box::new(DummyHandler(CLUSTER_GENERAL_COMMISSIONING)));
        node.add_cluster(0, Box::new(DummyHandler(CLUSTER_OPERATIONAL_CREDENTIALS)));

        let req = im::encode_read_request(0, im::CLUSTER_DESCRIPTOR, im::ATTR_SERVER_LIST);
        let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        assert_eq!(opcode, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports.len(), 1);
        let ids: Vec<u64> = msg.reports[0]
            .data
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec![
                u64::from(im::CLUSTER_DESCRIPTOR),
                u64::from(im::CLUSTER_BASIC_INFORMATION),
                u64::from(CLUSTER_GENERAL_COMMISSIONING),
                u64::from(CLUSTER_OPERATIONAL_CREDENTIALS),
            ]
        );
    }

    #[test]
    fn root_parts_list_reflects_registered_endpoints() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let (onoff, _state) = crate::core::onoff::OnOffHandler::new();
        node.add_endpoint(
            1,
            vec![
                Box::new(DescriptorHandler::for_device(im::DEVICE_TYPE_ON_OFF_LIGHT)),
                Box::new(onoff),
            ],
        );
        let req = im::encode_read_request(0, im::CLUSTER_DESCRIPTOR, im::ATTR_PARTS_LIST);
        let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!([1])));
    }

    #[test]
    fn endpoint1_device_type_is_on_off_light() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let (onoff, _state) = crate::core::onoff::OnOffHandler::new();
        node.add_endpoint(
            1,
            vec![
                Box::new(DescriptorHandler::for_device(im::DEVICE_TYPE_ON_OFF_LIGHT)),
                Box::new(onoff),
            ],
        );
        let req = im::encode_read_request(1, im::CLUSTER_DESCRIPTOR, im::ATTR_DEVICE_TYPE_LIST);
        let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(
            msg.reports[0].data,
            Some(serde_json::json!([{"0": im::DEVICE_TYPE_ON_OFF_LIGHT, "1": 1}]))
        );
    }

    /// M3 bridged topology: EP2 is a bridged On/Off Light that also carries
    /// `DEVICE_TYPE_BRIDGED_NODE` (spec §9.13 — every bridged endpoint's
    /// `DeviceTypeList` includes it alongside its "real" device type), and
    /// EP1 is the Aggregator whose static `PartsList` names EP2 as its one
    /// bridged child.
    #[test]
    fn descriptor_multi_device_types_and_static_parts() {
        let mut node = Node::new();
        node.add_endpoint(
            2,
            vec![Box::new(DescriptorHandler::for_device_types(&[
                im::DEVICE_TYPE_ON_OFF_LIGHT,
                im::DEVICE_TYPE_BRIDGED_NODE,
            ]))],
        );
        node.add_endpoint(
            1,
            vec![Box::new(
                DescriptorHandler::for_device(im::DEVICE_TYPE_AGGREGATOR).with_parts(vec![2]),
            )],
        );

        let req = im::encode_read_request(2, im::CLUSTER_DESCRIPTOR, im::ATTR_DEVICE_TYPE_LIST);
        let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(
            msg.reports[0].data,
            Some(serde_json::json!([
                {"0": im::DEVICE_TYPE_ON_OFF_LIGHT, "1": 1},
                {"0": im::DEVICE_TYPE_BRIDGED_NODE, "1": 1},
            ]))
        );

        let req = im::encode_read_request(1, im::CLUSTER_DESCRIPTOR, im::ATTR_PARTS_LIST);
        let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!([2])));
    }

    /// A `Node` with the standard root endpoint (Descriptor + BasicInfo)
    /// plus endpoint 1 (Descriptor + OnOff) — the fixture the wildcard
    /// expansion tests below read against, mirroring `device.rs`'s real
    /// endpoint 1 wiring without the commissioning clusters (which live in
    /// `core::commissioning` and have their own wildcard-expansion tests).
    fn node_with_onoff() -> Node {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let (onoff, _state) = crate::core::onoff::OnOffHandler::new();
        node.add_endpoint(
            1,
            vec![
                Box::new(DescriptorHandler::for_device(im::DEVICE_TYPE_ON_OFF_LIGHT)),
                Box::new(onoff),
            ],
        );
        node
    }

    /// Write 未対応クラスタへの write は AttributeStatusIB(UNSUPPORTED_WRITE) で
    /// 応答する（StatusResponse で会話全体を落とさない）。
    #[test]
    fn write_to_read_only_attribute_reports_unsupported_write() {
        let mut node = node_with_onoff();
        let mut data = Writer::new();
        data.put_bool(Tag::Anonymous, true);
        let payload =
            im::encode_write_request_tlv(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &data.finish());
        let (op, resp) = handle_im_ok(&mut node, im::OPCODE_WRITE_REQUEST, &payload);
        assert_eq!(op, im::OPCODE_WRITE_RESPONSE);
        assert_eq!(
            im::decode_write_response(&resp).unwrap(),
            im::STATUS_UNSUPPORTED_WRITE
        );
    }

    /// 未知 endpoint / 未知 cluster は per-path の status で応答する。
    #[test]
    fn write_to_unknown_paths_reports_path_scoped_status() {
        let mut node = node_with_onoff();
        let mut data = Writer::new();
        data.put_uint(Tag::Anonymous, 1);
        let payload =
            im::encode_write_request_tlv(9, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &data.finish());
        let (_, resp) = handle_im_ok(&mut node, im::OPCODE_WRITE_REQUEST, &payload);
        assert_eq!(
            im::decode_write_response(&resp).unwrap(),
            im::STATUS_UNSUPPORTED_ENDPOINT
        );
    }

    /// FeatureMap はハンドラ申告値になる（Task 4 の NetworkCommissioning=ET 用の座金）。
    #[test]
    fn feature_map_global_reflects_the_handler() {
        struct FmHandler;
        impl ClusterHandler for FmHandler {
            fn cluster_id(&self) -> u32 {
                0x0031
            }
            fn attributes(&self) -> Vec<u32> {
                vec![]
            }
            fn read(&self, _: u32, _: &ReadCtx) -> Option<Vec<u8>> {
                None
            }
            fn invoke(&mut self, _: u32, _: &[u8], _: &mut InvokeCtx) -> InvokeReply {
                InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
            }
            fn feature_map(&self) -> u32 {
                0x04
            }
        }
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        node.add_cluster(0, Box::new(FmHandler));
        let payload = encode_read_request_path(Some(0), Some(0x0031), Some(im::ATTR_FEATURE_MAP));
        let (_, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(4)));
    }

    /// Test-only ReadRequest encoder generalizing `im::encode_read_request`/
    /// `encode_read_request_cluster` to any combination of wildcard
    /// (`None`) endpoint/cluster/attribute fields, and to more than one
    /// path per request.
    fn encode_read_request_paths(paths: &[(Option<u16>, Option<u32>, Option<u32>)]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(0)); // AttributeRequests
        for (endpoint, cluster, attribute) in paths {
            w.start_list(Tag::Anonymous); // AttributePathIB
            if let Some(e) = endpoint {
                w.put_uint(Tag::Context(2), u64::from(*e));
            }
            if let Some(c) = cluster {
                w.put_uint(Tag::Context(3), u64::from(*c));
            }
            if let Some(a) = attribute {
                w.put_uint(Tag::Context(4), u64::from(*a));
            }
            w.end_container(); // AttributePathIB
        }
        w.end_container(); // AttributeRequests
        w.put_bool(Tag::Context(3), true); // IsFabricFiltered
        w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
        w.end_container(); // outer struct
        w.finish()
    }

    fn encode_read_request_path(
        endpoint: Option<u16>,
        cluster: Option<u32>,
        attribute: Option<u32>,
    ) -> Vec<u8> {
        encode_read_request_paths(&[(endpoint, cluster, attribute)])
    }

    #[test]
    fn wildcard_endpoint_read_expands_to_all_endpoints() {
        let mut node = node_with_onoff();
        let payload = encode_read_request_path(
            None,
            Some(im::CLUSTER_DESCRIPTOR),
            Some(im::ATTR_DEVICE_TYPE_LIST),
        );
        let (op, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        assert_eq!(op, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports.len(), 2); // endpoint 0 と 1
        let endpoints: Vec<u16> = msg.reports.iter().map(|r| r.endpoint.unwrap()).collect();
        assert_eq!(endpoints, vec![0, 1]);
    }

    #[test]
    fn full_wildcard_read_reports_every_attribute_without_error() {
        let mut node = node_with_onoff();
        let payload = encode_read_request_path(None, None, None);
        let (op, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        assert_eq!(op, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&resp).unwrap();
        assert!(msg.reports.len() >= 10); // descriptor×2 + basicinfo×5 + onoff×1 + ...
        assert!(msg.reports.iter().all(|r| r.status.is_none()));
    }

    /// Wildcard attribute expansion must not pull in the global attributes
    /// (spec §7.13 ids `0xFFF8`-`0xFFFD`) — only `ClusterHandler::
    /// attributes()`'s own ids (see `read_entries`'s doc: chip-tool/Echo
    /// full-wildcard reads would balloon otherwise).
    #[test]
    fn full_wildcard_read_excludes_global_attributes() {
        let mut node = node_with_onoff();
        let payload = encode_read_request_path(None, None, None);
        let (_, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        let msg = decode_report_data_message(&resp).unwrap();
        assert!(!msg
            .reports
            .iter()
            .any(|r| r.attribute == Some(im::ATTR_CLUSTER_REVISION)));
    }

    /// A concretely-requested global attribute (not reached via wildcard
    /// expansion) is answered — `ClusterRevision`/`FeatureMap` are
    /// synthesized by `Node`, not any `ClusterHandler::read`.
    #[test]
    fn concrete_global_attribute_is_answered() {
        let mut node = node_with_onoff();
        let payload = encode_read_request_path(
            Some(1),
            Some(im::CLUSTER_ON_OFF),
            Some(im::ATTR_CLUSTER_REVISION),
        );
        let (op, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        assert_eq!(op, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports.len(), 1);
        // OnOff's real ClusterRevision (6) — not the M2-era hardcoded 1.
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(6)));
    }

    /// AcceptedCommandList/GeneratedCommandList (spec §7.13) must reflect
    /// the cluster's real command set — an all-empty AcceptedCommandList
    /// claims an OnOff light that no command can control, which a
    /// conformance-checking controller (Apple Home's post-commissioning
    /// interview) treats as a broken device.
    #[test]
    fn command_list_globals_reflect_the_handler() {
        let mut node = node_with_onoff();
        let payload = encode_read_request_path(
            Some(1),
            Some(im::CLUSTER_ON_OFF),
            Some(im::ATTR_ACCEPTED_COMMAND_LIST),
        );
        let (_, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(
            msg.reports[0].data,
            Some(serde_json::json!([
                im::CMD_ON_OFF_OFF,
                im::CMD_ON_OFF_ON,
                im::CMD_ON_OFF_TOGGLE
            ]))
        );

        // OnOff declares no response commands, so GeneratedCommandList
        // stays empty — but as the cluster's answer, not a Node-wide stub.
        let payload = encode_read_request_path(
            Some(1),
            Some(im::CLUSTER_ON_OFF),
            Some(im::ATTR_GENERATED_COMMAND_LIST),
        );
        let (_, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!([])));
    }

    #[test]
    fn unknown_concrete_attribute_reports_status_ib_not_global_error() {
        let mut node = node_with_onoff();
        // 実在 path と不在 path の 2 本読み
        let payload = encode_read_request_paths(&[
            (
                Some(0),
                Some(im::CLUSTER_BASIC_INFORMATION),
                Some(im::ATTR_VENDOR_ID),
            ),
            (Some(0), Some(im::CLUSTER_BASIC_INFORMATION), Some(0x7777)),
        ]);
        let (op, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        assert_eq!(op, im::OPCODE_REPORT_DATA); // StatusResponse ではない
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports.len(), 2);
        let vendor_id = msg
            .reports
            .iter()
            .find(|r| r.attribute == Some(im::ATTR_VENDOR_ID))
            .expect("vendor id report present");
        assert_eq!(vendor_id.data, Some(serde_json::json!(0xFFF1)));
        assert_eq!(vendor_id.status, None);
        let missing = msg
            .reports
            .iter()
            .find(|r| r.attribute == Some(0x7777))
            .expect("status report present for unknown attribute");
        assert_eq!(missing.status, Some(im::STATUS_UNSUPPORTED_ATTRIBUTE));
        assert_eq!(missing.data, None);
    }

    #[test]
    fn concrete_unknown_endpoint_reports_status_ib() {
        let mut node = node_with_onoff();
        let payload = encode_read_request_path(Some(9), None, None);
        let (op, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        assert_eq!(op, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(msg.reports[0].endpoint, Some(9));
        assert_eq!(msg.reports[0].status, Some(im::STATUS_UNSUPPORTED_ENDPOINT));
    }

    #[test]
    fn concrete_unknown_cluster_on_concrete_endpoint_reports_status_ib() {
        let mut node = node_with_onoff();
        let payload = encode_read_request_path(Some(0), Some(0x9999), None);
        let (op, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        assert_eq!(op, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(msg.reports[0].status, Some(im::STATUS_UNSUPPORTED_CLUSTER));
    }

    /// Wildcard-endpoint expansion landing on an endpoint that doesn't
    /// implement a concretely-requested cluster must *not* generate a
    /// status entry for that endpoint (spec §8.9.2.3: only a fully concrete
    /// path reports a per-path error) — endpoint 0 has no OnOff cluster,
    /// endpoint 1 does.
    #[test]
    fn wildcard_endpoint_with_concrete_cluster_skips_non_matching_endpoints() {
        let mut node = node_with_onoff();
        let payload =
            encode_read_request_path(None, Some(im::CLUSTER_ON_OFF), Some(im::ATTR_ON_OFF));
        let (op, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
        assert_eq!(op, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(msg.reports[0].endpoint, Some(1));
        assert_eq!(msg.reports[0].status, None);
    }

    // ── Task 6: chunked read (`Node::read_chunks`) ─────────────────────

    /// `node_with_onoff()` plus endpoint 2, carrying three fake clusters
    /// that each expose one ~600B attribute — big enough that two of them
    /// together already blow past a 900B chunk budget, forcing
    /// `read_chunks` to split. Cluster ids are well outside any real
    /// (spec-assigned or manufacturer-specific) cluster id range — these
    /// clusters exist only to be oversized, not to look like a real
    /// device.
    fn node_with_onoff_and_fat_attribute() -> Node {
        struct FatHandler {
            cluster: u32,
            attribute: u32,
        }
        impl ClusterHandler for FatHandler {
            fn cluster_id(&self) -> u32 {
                self.cluster
            }
            fn attributes(&self) -> Vec<u32> {
                vec![self.attribute]
            }
            fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
                if attribute != self.attribute {
                    return None;
                }
                let mut w = Writer::new();
                w.put_bytes(Tag::Anonymous, &[0xAB; 600]);
                Some(w.finish())
            }
            fn invoke(
                &mut self,
                _command: u32,
                _fields_tlv: &[u8],
                _ctx: &mut InvokeCtx,
            ) -> InvokeReply {
                InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
            }
        }

        let mut node = node_with_onoff();
        for i in 0..3u32 {
            node.add_endpoint(
                2,
                vec![Box::new(FatHandler {
                    cluster: 0x9999_0000 + i,
                    attribute: 1,
                }) as Box<dyn ClusterHandler>],
            );
        }
        node
    }

    #[test]
    fn read_chunks_splits_when_over_budget_and_marks_more_chunks() {
        let node = node_with_onoff_and_fat_attribute();
        let paths = [AttrPathIn {
            endpoint: None,
            cluster: None,
            attribute: None,
        }];
        let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, None);
        assert!(chunks.len() >= 2);
        for (i, c) in chunks.iter().enumerate() {
            let msg = decode_report_data_message(c).unwrap();
            let last = i == chunks.len() - 1;
            assert_eq!(msg.more_chunks, !last);
            assert_eq!(msg.suppress_response, last);
            // budget 超過が許されるのは「1 レポート単体が budget 超過で、
            // 分割せず単独チャンクに出た」場合のみ（read_chunks の docstring）
            // — 2 レポート以上を含むチャンクは厳密に budget 以内でなければ
            // ならない（fix round 1: greedy probe が非最終チャンクの実
            // エンコード形状〈more_chunks=true〉と不一致で最大 2 バイト超過
            // しうるバグがあった。900+64 の緩い許容がそれを隠していた）。
            if msg.reports.len() > 1 {
                assert!(
                    c.len() <= 900,
                    "chunk {i} ({} reports) exceeded budget: {} bytes",
                    msg.reports.len(),
                    c.len()
                );
            }
            // このフィクスチャの fat 属性（600B）はどれも単体では 900B を
            // 超えないため、単一レポートのチャンク（fat 属性それぞれ）も
            // 実際には budget 以内に収まる — 上の緩和は一般則としての
            // 記録であり、このテストで例外を意図的に踏んでいるわけではない。
            else {
                assert!(
                    c.len() <= 900,
                    "solo-report chunk {i} unexpectedly exceeded budget: {} bytes",
                    c.len()
                );
            }
        }
    }

    /// Fix round 1 (code review): pins the exact boundary the bug lived
    /// in. `read_chunks`' greedy probe used to check candidates against
    /// the *final*-chunk wire shape (`more_chunks=false`), 2 bytes smaller
    /// than the shape a non-final chunk is actually encoded with
    /// (`more_chunks=true` adds a `MoreChunkedMessages` TLV element). A
    /// batch whose final-shape length was `<= budget` but whose
    /// non-final-shape length was `budget+1..=budget+2` used to be let
    /// through un-split, then get encoded non-final (because a later fat
    /// entry forced a split after it) 1-2 bytes over `budget` — silently
    /// violating the `REPORT_CHUNK_BUDGET` contract.
    ///
    /// Builds the adversarial case directly: takes the fixture's small
    /// (non-fat) entries as one candidate batch, computes its final-shape
    /// length exactly (`im::encode_report_data_entries(..., more_chunks:
    /// false)`), and sets `budget` to exactly that value — the tightest
    /// possible budget that still lets the *old* code accept this batch
    /// without splitting. Every fat entry after it then forces this batch
    /// to end up non-final, so its real encoded length must not exceed
    /// `budget` — which only holds if the probe already accounted for the
    /// non-final shape.
    #[test]
    fn read_chunks_probe_accounts_for_non_final_more_chunks_overhead_at_the_boundary() {
        let node = node_with_onoff_and_fat_attribute();
        let paths = [AttrPathIn {
            endpoint: None,
            cluster: None,
            attribute: None,
        }];
        let read_ctx = ReadCtx::default();
        let entries = node.read_entries(&paths, &read_ctx);

        // The fixture's fat clusters all use ids `0x9999_0000..=0x9999_0002`
        // (see `node_with_onoff_and_fat_attribute`'s doc) — everything else
        // is the small root-endpoint/OnOff data this boundary test packs
        // into one candidate batch.
        let small_batch: Vec<ReportEntryOut> = entries
            .iter()
            .filter(|e| match e {
                ReportEntryOut::Data(r) => !(0x9999_0000..=0x9999_0002).contains(&r.cluster),
                ReportEntryOut::Status { cluster, .. } => {
                    !(0x9999_0000..=0x9999_0002).contains(cluster)
                }
            })
            .cloned()
            .collect();
        assert!(
            !small_batch.is_empty(),
            "fixture must have at least one non-fat entry to build the boundary batch"
        );

        let final_shape_len = im::encode_report_data_entries(&small_batch, true, None, false).len();
        let non_final_shape_len =
            im::encode_report_data_entries(&small_batch, false, None, true).len();
        assert!(
            non_final_shape_len > final_shape_len,
            "MoreChunkedMessages must cost extra bytes for this boundary to be meaningful"
        );

        // Exactly the boundary: old (buggy) code's probe — final shape —
        // sees `final_shape_len <= budget` and never splits this batch;
        // fixed code's probe — non-final shape — sees
        // `non_final_shape_len > budget` and splits it off before adding
        // any fat entry.
        let budget = final_shape_len;
        let chunks = node.read_chunks(&paths, &read_ctx, budget, None);
        assert!(
            chunks.len() >= 2,
            "fat entries after the small batch must still force a split"
        );
        for (i, c) in chunks.iter().enumerate() {
            let msg = decode_report_data_message(c).unwrap();
            if msg.reports.len() > 1 {
                assert!(
                    c.len() <= budget,
                    "chunk {i} ({} reports) exceeded budget {budget}: {} bytes \
                     (this is exactly the fix-round-1 off-by-up-to-2-bytes bug)",
                    msg.reports.len(),
                    c.len()
                );
            }
        }
    }

    /// 1 チャンクで収まる読み取りは従来と同一挙動（回帰なし）: 単一チャンク、
    /// `more_chunks=false, suppress_response=true` — `handle_read`（budget=
    /// `usize::MAX`）が返すものと同じ形。
    #[test]
    fn read_chunks_single_chunk_matches_legacy_single_message_shape() {
        let node = node_with_onoff();
        let paths = [AttrPathIn {
            endpoint: None,
            cluster: None,
            attribute: None,
        }];
        let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, None);
        assert_eq!(chunks.len(), 1);
        let msg = decode_report_data_message(&chunks[0]).unwrap();
        assert!(!msg.more_chunks);
        assert!(msg.suppress_response);
    }

    // ── Task 12: change reporting + DataVersion ─────────────────────────

    /// An invoke that actually changes a value reports the full path
    /// (`ImOutcome::changed`) and bumps *that* `(endpoint, cluster)`'s
    /// DataVersion — which subsequent reads of the cluster then carry
    /// (`AttrReportOut::data_version`), because a subscribing controller
    /// keys its own dirty tracking off that field.
    #[test]
    fn invoke_on_off_reports_changed_path_and_bumps_data_version() {
        let mut node = node_with_onoff();
        assert_eq!(node.data_version(1, im::CLUSTER_ON_OFF), 1);

        let req = im::encode_invoke_request(1, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None);
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert_eq!(outcome.opcode, im::OPCODE_INVOKE_RESPONSE);
        assert_eq!(
            outcome.changed,
            vec![(1u16, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF)]
        );
        assert_eq!(node.data_version(1, im::CLUSTER_ON_OFF), 2);
        // The bump is per-(endpoint, cluster): endpoint 1's Descriptor is
        // untouched.
        assert_eq!(node.data_version(1, im::CLUSTER_DESCRIPTOR), 1);

        // ...and it shows up in what a read of the cluster reports.
        let entries = node.read_entries(
            &[AttrPathIn {
                endpoint: Some(1),
                cluster: Some(im::CLUSTER_ON_OFF),
                attribute: Some(im::ATTR_ON_OFF),
            }],
            &ReadCtx::default(),
        );
        match &entries[0] {
            ReportEntryOut::Data(r) => assert_eq!(r.data_version, 2),
            other => panic!("expected an OnOff data report, got {other:?}"),
        }
    }

    /// An invoke that leaves the value where it already was (On on an
    /// already-on light) is *not* a change: nothing to report, no version
    /// bump — otherwise every redundant command would wake every subscriber
    /// (spec §8.10's reporting is value-change driven). Also pins that
    /// `changed` doesn't accumulate across commands sharing one `InvokeCtx`
    /// (the runtime builds one per request, but `core`'s contract shouldn't
    /// depend on that).
    #[test]
    fn invoke_without_a_value_change_reports_nothing_and_keeps_the_version() {
        let mut node = node_with_onoff();
        let req = im::encode_invoke_request(1, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None);
        let mut ctx = InvokeCtx::default();
        node.handle_im(
            im::OPCODE_INVOKE_REQUEST,
            &req,
            &mut ctx,
            &ReadCtx::default(),
        )
        .unwrap();
        assert_eq!(node.data_version(1, im::CLUSTER_ON_OFF), 2);

        // Second On on an already-on light.
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &ReadCtx::default(),
            )
            .unwrap();
        assert!(outcome.changed.is_empty(), "no value changed");
        assert_eq!(node.data_version(1, im::CLUSTER_ON_OFF), 2);
    }

    /// A read never reports changes, and neither does a rejected command.
    #[test]
    fn read_and_rejected_invoke_report_no_changes() {
        let mut node = node_with_onoff();
        let read = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_VENDOR_ID);
        let outcome = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &read,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert!(outcome.changed.is_empty());

        let bad = im::encode_invoke_request(1, im::CLUSTER_ON_OFF, 0x7F, None);
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &bad,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert!(outcome.changed.is_empty());
        assert_eq!(node.data_version(1, im::CLUSTER_ON_OFF), 1);
    }

    /// Priming (`subscription_id = Some`) differs from a plain read in two
    /// ways on the wire: every chunk carries the SubscriptionId, and even
    /// the *last* one keeps `suppress_response=false`, because a
    /// SubscribeResponse still has to follow on the same exchange (spec
    /// §8.10) and the initiator answers every chunk with StatusResponse(0).
    #[test]
    fn read_chunks_for_priming_carries_the_subscription_id_and_never_suppresses() {
        let node = node_with_onoff_and_fat_attribute();
        let paths = [AttrPathIn {
            endpoint: None,
            cluster: None,
            attribute: None,
        }];
        let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, Some(0xABCD));
        assert!(chunks.len() >= 2, "fixture must force a split");
        for (i, c) in chunks.iter().enumerate() {
            let msg = decode_report_data_message(c).unwrap();
            assert_eq!(msg.subscription_id, Some(0xABCD));
            assert!(
                !msg.suppress_response,
                "priming chunk {i} must not suppress"
            );
            assert_eq!(msg.more_chunks, i != chunks.len() - 1);
        }
    }

    /// Carried finding from Task 5's review: an inbound `StatusResponse`
    /// reaching the generic opcode dispatch (e.g. via `serve_secured`'s
    /// buffered-request drain, instead of the chunk-wait `session.recv`
    /// that's supposed to consume it) must be silently dropped, not
    /// answered with `StatusResponse(INVALID_ACTION)` — see `handle_im`'s
    /// `OPCODE_STATUS_RESPONSE` arm doc comment.
    #[test]
    fn handle_im_drops_inbound_status_response_without_replying() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let payload = im::encode_status_response(0);
        let result = node.handle_im(
            im::OPCODE_STATUS_RESPONSE,
            &payload,
            &mut InvokeCtx::default(),
            &ReadCtx::default(),
        );
        assert!(matches!(result, Err(ImServerError::NoReply)));
    }
}

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

    // `im::DEVICE_TYPE_ROOT_NODE` (RootNode device type, spec §9.2.2) is
    // intentionally not pinned here: `mat_core::ids`'s generated table
    // covers clusters/attributes/commands only, not device types — there is
    // no `mat_core` lookup to check it against. See the doc comment on the
    // constant itself.
}
