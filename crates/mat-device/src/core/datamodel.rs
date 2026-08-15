//! Data model dispatch skeleton: endpoint/cluster registry (`Node`) and the
//! per-cluster handler trait (`ClusterHandler`) that serves incoming
//! Interaction Model requests. Pure — one opcode+payload in, one (opcode,
//! payload) out, no tokio, no sockets, no files (checked by `cargo check -p
//! mat-device --no-default-features` in CI). Wire codecs live in
//! `mat_controller::im` (this module only knows attribute/command
//! semantics, never TLV byte layout directly).
//!
//! M2 scope: `ReadRequest` (with wildcard endpoint/cluster/attribute
//! expansion — see `Node::read_entries`) and `InvokeRequest` (single
//! command) only — still no subscriptions, no writes. Every other opcode
//! gets `StatusResponse(STATUS_INVALID_ACTION)` (spec §8.10.1) rather than
//! being silently dropped or failing the whole exchange.

use mat_controller::im::{self, AttrPathIn, AttrReportOut, ImError, ReportEntryOut};
use mat_controller::tlv::{Tag, Writer};

/// Placeholder DataVersion for every attribute report — `mat-device` does
/// not yet track per-cluster data versions (bumped on write). A future task
/// wires real versioning once writes exist; until then every read reports
/// version 1.
const UNVERSIONED: u32 = 1;

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
#[derive(Debug, Clone, Default)]
pub struct InvokeCtx {
    pub attestation_challenge: [u8; 16],
}

/// Per-read scratch context threaded through `ClusterHandler::read` and
/// `Node::read_entries`. Carries the current secure session's fabric index
/// (spec §7.9, `FabricIndex`) — needed for fabric-scoped attributes like
/// Operational Credentials' `CurrentFabricIndex`. `0` (the default) is not a
/// valid fabric index (fabric indices start at 1) but matches what a PASE
/// session (no fabric yet) should report.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadCtx {
    pub fabric_index: u8,
}

/// A cluster's outcome for one invoked command: either a bare status (the
/// common case — most commands have no response payload) or response data
/// (a CommandDataIB, e.g. a cluster's declared response command).
#[derive(Debug, Clone, PartialEq)]
pub enum InvokeReply {
    Status(u8),
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
}

/// Interaction Model server-side dispatch errors: either a malformed
/// request payload, or an opcode this M1 skeleton doesn't implement yet.
#[derive(Debug)]
pub enum ImServerError {
    Decode(ImError),
    UnsupportedOpcode(u8),
}

impl std::fmt::Display for ImServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImServerError::Decode(e) => write!(f, "im: {e}"),
            ImServerError::UnsupportedOpcode(op) => {
                write!(f, "im: unsupported opcode 0x{op:02X}")
            }
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
    pub fn with_root_endpoint(vendor_id: u16, product_id: u16) -> Self {
        let mut node = Self::new();
        node.add_endpoint(
            0,
            vec![
                Box::new(DescriptorHandler::for_device(im::DEVICE_TYPE_ROOT_NODE))
                    as Box<dyn ClusterHandler>,
                Box::new(BasicInformationHandler {
                    vendor_id,
                    product_id,
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

    /// Dispatches one incoming IM message. Returns the response opcode and
    /// payload to send back (already IM-wire-encoded via `mat_controller::im`).
    /// `read_ctx` carries the requesting session's fabric index (see
    /// `ReadCtx`'s doc) — irrelevant to `InvokeRequest`, but threaded
    /// through every `ReadRequest`.
    pub fn handle_im(
        &mut self,
        opcode: u8,
        payload: &[u8],
        ctx: &mut InvokeCtx,
        read_ctx: &ReadCtx,
    ) -> Result<(u8, Vec<u8>), ImServerError> {
        match opcode {
            im::OPCODE_READ_REQUEST => self.handle_read(payload, read_ctx),
            im::OPCODE_INVOKE_REQUEST => self.handle_invoke(payload, ctx),
            // Any opcode this skeleton has no handler for (WriteRequest,
            // SubscribeRequest, TimedRequest, ...) is answered — not
            // silently dropped, and not a hard error that kills the
            // exchange — with the IM status for "can't handle this action"
            // (spec §8.10.1). `ImServerError::UnsupportedOpcode` is no
            // longer produced here; it stays reserved for a payload that
            // can't even be decoded into a response we know how to send.
            _other => Ok((
                im::OPCODE_STATUS_RESPONSE,
                im::encode_status_response(im::STATUS_INVALID_ACTION),
            )),
        }
    }

    fn handle_read(
        &self,
        payload: &[u8],
        read_ctx: &ReadCtx,
    ) -> Result<(u8, Vec<u8>), ImServerError> {
        let paths = im::decode_read_request(payload)?;
        let entries = self.read_entries(&paths, read_ctx);
        Ok((
            im::OPCODE_REPORT_DATA,
            im::encode_report_data_entries(&entries, true, None, false),
        ))
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
                    data_version: UNVERSIONED,
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
                            data_version: UNVERSIONED,
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
            im::ATTR_CLUSTER_REVISION => Some(uint_value(1)),
            im::ATTR_FEATURE_MAP => Some(uint_value(0)),
            im::ATTR_ATTRIBUTE_LIST => Some(encode_attribute_list(handler)),
            im::ATTR_ACCEPTED_COMMAND_LIST | im::ATTR_GENERATED_COMMAND_LIST => {
                Some(encode_empty_list())
            }
            _ => handler.read(attribute, ectx.read_ctx),
        }
    }

    fn handle_invoke(
        &mut self,
        payload: &[u8],
        ctx: &mut InvokeCtx,
    ) -> Result<(u8, Vec<u8>), ImServerError> {
        let req = im::decode_invoke_request(payload)?;
        let Some((_, clusters)) = self
            .endpoints
            .iter_mut()
            .find(|(id, _)| *id == req.endpoint)
        else {
            return Ok((
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
            return Ok((
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
        let reply = handler.invoke(req.command, &req.fields_tlv, ctx);
        let resp_payload = match reply {
            InvokeReply::Status(status) => im::encode_invoke_response_status(
                req.endpoint,
                req.cluster,
                req.command,
                status,
                None,
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
        Ok((im::OPCODE_INVOKE_RESPONSE, resp_payload))
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

/// Encodes an empty TLV array — `AcceptedCommandList`/`GeneratedCommandList`
/// (spec §7.13): `mat-device` has no command-enumeration API yet (M2
/// scope), so every cluster reports "no commands" rather than omitting the
/// attribute; chip tolerates this (see `read_attribute_value`'s caller).
fn encode_empty_list() -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    w.end_container();
    w.finish()
}

/// Descriptor cluster (spec §9.5), mandatory on every endpoint. Carries the
/// endpoint's `DeviceTypeList` entry (`device_type` — `DEVICE_TYPE_ROOT_NODE`
/// on endpoint 0, `DEVICE_TYPE_ON_OFF_LIGHT` on endpoint 1) since that's the
/// one piece of per-endpoint Descriptor state this flat, non-composed data
/// model needs; `ServerList`/endpoint-0 `PartsList` are derived from the
/// registry by `Node::read_attribute_value` instead (see its doc comment)
/// because they depend on sibling/other-endpoint state this handler can't
/// see.
pub struct DescriptorHandler {
    device_type: u32,
}

impl DescriptorHandler {
    /// A Descriptor handler for an endpoint whose `DeviceTypeList` is the
    /// single entry `device_type` (revision 1 — M2 scope has no device type
    /// revisions beyond the first).
    pub fn for_device(device_type: u32) -> Self {
        Self { device_type }
    }
}

impl ClusterHandler for DescriptorHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_DESCRIPTOR
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
                w.start_struct(Tag::Anonymous); // DeviceTypeStruct
                w.put_uint(Tag::Context(0), u64::from(self.device_type));
                w.put_uint(Tag::Context(1), 1); // Revision
                w.end_container();
                w.end_container();
                Some(w.finish())
            }
            // ATTR_SERVER_LIST, and ATTR_PARTS_LIST on endpoint 0, are
            // intercepted and derived from the `Node`'s registry by
            // `Node::read_attribute_value` — never reach here (see that
            // override's doc comment). This is endpoint != 0's PartsList
            // (always empty: M2 endpoints have no children) and endpoint
            // 0's own fallback, which `read_attribute_value` never takes.
            im::ATTR_PARTS_LIST => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
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

/// BasicInformation cluster (spec §11.1), mandatory on endpoint 0. M1 only
/// serves the identity attributes a controller reads during commissioning;
/// the rest of the cluster (writable NodeLabel, etc.) is future scope.
struct BasicInformationHandler {
    vendor_id: u16,
    product_id: u16,
}

impl ClusterHandler for BasicInformationHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_BASIC_INFORMATION
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_DATA_MODEL_REVISION,
            im::ATTR_VENDOR_ID,
            im::ATTR_PRODUCT_ID,
            im::ATTR_VENDOR_NAME,
            im::ATTR_PRODUCT_NAME,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_DATA_MODEL_REVISION => Some(uint_value(DATA_MODEL_REVISION)),
            im::ATTR_VENDOR_ID => Some(uint_value(u64::from(self.vendor_id))),
            im::ATTR_PRODUCT_ID => Some(uint_value(u64::from(self.product_id))),
            im::ATTR_VENDOR_NAME => Some(str_value("mat")),
            im::ATTR_PRODUCT_NAME => Some(str_value("matv")),
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // BasicInformation declares no commands.
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::im::{decode_invoke_response, decode_report_data_message};

    #[test]
    fn read_basic_information_data_model_revision() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_read_request(
            0,
            im::CLUSTER_BASIC_INFORMATION,
            im::ATTR_DATA_MODEL_REVISION,
        );
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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

    #[test]
    fn read_descriptor_device_type_list_is_root_node() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_read_request(0, im::CLUSTER_DESCRIPTOR, im::ATTR_DEVICE_TYPE_LIST);
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_UNSUPPORTED_CLUSTER);
    }

    #[test]
    fn invoke_known_cluster_unknown_command_returns_unsupported_command() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let req = im::encode_invoke_request(0, im::CLUSTER_BASIC_INFORMATION, 0x7F, None);
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_UNSUPPORTED_COMMAND);
    }

    /// M2 behavior (unlike M1): an opcode this skeleton doesn't implement
    /// (`WriteRequest` etc.) is answered with `StatusResponse
    /// (STATUS_INVALID_ACTION)`, not a hard `Err` — chip-tool/Echo probing
    /// an unimplemented feature shouldn't look like a dropped/malformed
    /// exchange.
    #[test]
    fn unsupported_opcode_returns_invalid_action_status() {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_WRITE_REQUEST,
                &[],
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert_eq!(opcode, im::OPCODE_STATUS_RESPONSE);
        let status = im::decode_status_response(&payload).unwrap();
        assert_eq!(status, im::STATUS_INVALID_ACTION);
    }

    #[test]
    fn read_basic_information_vendor_and_product_id() {
        let mut node = Node::with_root_endpoint(0x1234, 0x5678);
        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_VENDOR_ID);
        let (_, payload) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(0x1234)));

        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_PRODUCT_ID);
        let (_, payload) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (opcode, payload) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (_, payload) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (_, payload) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(
            msg.reports[0].data,
            Some(serde_json::json!([{"0": im::DEVICE_TYPE_ON_OFF_LIGHT, "1": 1}]))
        );
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
        let (op, resp) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (op, resp) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (_, resp) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (op, resp) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert_eq!(op, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(1)));
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
        let (op, resp) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (op, resp) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (op, resp) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
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
        let (op, resp) = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &payload,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .unwrap();
        assert_eq!(op, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&resp).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(msg.reports[0].endpoint, Some(1));
        assert_eq!(msg.reports[0].status, None);
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
    }

    // `im::DEVICE_TYPE_ROOT_NODE` (RootNode device type, spec §9.2.2) is
    // intentionally not pinned here: `mat_core::ids`'s generated table
    // covers clusters/attributes/commands only, not device types — there is
    // no `mat_core` lookup to check it against. See the doc comment on the
    // constant itself.
}
