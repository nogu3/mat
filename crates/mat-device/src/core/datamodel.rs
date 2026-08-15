//! Data model dispatch skeleton: endpoint/cluster registry (`Node`) and the
//! per-cluster handler trait (`ClusterHandler`) that serves incoming
//! Interaction Model requests. Pure — one opcode+payload in, one (opcode,
//! payload) out, no tokio, no sockets, no files (checked by `cargo check -p
//! mat-device --no-default-features` in CI). Wire codecs live in
//! `mat_controller::im` (this module only knows attribute/command
//! semantics, never TLV byte layout directly).
//!
//! M1 scope: `ReadRequest` (single concrete attribute paths) and
//! `InvokeRequest` (single command) only — no subscriptions, no writes, no
//! wildcard attribute enumeration (see `Node::resolve_read`'s doc). Every
//! other opcode is rejected with `ImServerError::UnsupportedOpcode`.

use mat_controller::im::{self, AttrPathIn, AttrReportOut, ImError};
use mat_controller::tlv::{Tag, Writer};

/// Placeholder DataVersion for every attribute report — `mat-device` does
/// not yet track per-cluster data versions (bumped on write). A future task
/// wires real versioning once writes exist; until then every read reports
/// version 1.
const UNVERSIONED: u32 = 1;

/// Matter test vendor ID range (spec §2.5.2): 0xFFF1-0xFFF4 are reserved
/// for testing and never assigned to a real vendor.
const TEST_VENDOR_ID: u64 = 0xFFF1;
/// Arbitrary product ID within the test vendor's namespace.
const TEST_PRODUCT_ID: u64 = 0x8000;
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
    /// Reads one attribute. `Some` is one complete, well-formed TLV element
    /// tagged `Tag::Anonymous` (the attribute's `Data`); `None` means the
    /// attribute id is not implemented by this cluster (→
    /// `STATUS_UNSUPPORTED_ATTRIBUTE`).
    fn read(&self, attribute: u32) -> Option<Vec<u8>>;
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
    pub fn with_root_endpoint() -> Self {
        let mut node = Self::new();
        node.add_endpoint(
            0,
            vec![
                Box::new(DescriptorHandler) as Box<dyn ClusterHandler>,
                Box::new(BasicInformationHandler) as Box<dyn ClusterHandler>,
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
    /// `endpoint` already has one — `resolve_read`/`handle_invoke` only
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
    pub fn handle_im(
        &mut self,
        opcode: u8,
        payload: &[u8],
        ctx: &mut InvokeCtx,
    ) -> Result<(u8, Vec<u8>), ImServerError> {
        match opcode {
            im::OPCODE_READ_REQUEST => self.handle_read(payload),
            im::OPCODE_INVOKE_REQUEST => self.handle_invoke(payload, ctx),
            other => Err(ImServerError::UnsupportedOpcode(other)),
        }
    }

    fn handle_read(&self, payload: &[u8]) -> Result<(u8, Vec<u8>), ImServerError> {
        let paths = im::decode_read_request(payload)?;
        let mut reports = Vec::new();
        for path in &paths {
            match self.resolve_read(path) {
                Ok(Some(report)) => reports.push(report),
                // Wildcard field (endpoint/cluster/attribute omitted) with
                // no concrete resolution: M1 doesn't enumerate attributes,
                // so the path simply contributes no report (not an error —
                // see `resolve_read`'s doc).
                Ok(None) => {}
                Err(status) => {
                    // M1 scope: a single unresolved concrete path fails the
                    // whole read with a StatusResponse. `encode_report_data`
                    // only carries data (not per-path AttributeStatusIB)
                    // reports today, so a mixed success/failure batch isn't
                    // representable yet.
                    return Ok((
                        im::OPCODE_STATUS_RESPONSE,
                        im::encode_status_response(status),
                    ));
                }
            }
        }
        Ok((
            im::OPCODE_REPORT_DATA,
            im::encode_report_data(&reports, true),
        ))
    }

    /// Resolves one AttributePathIB against the registry.
    ///
    /// - A wildcard endpoint/cluster/attribute (`None`) that can't be
    ///   resolved to exactly one concrete target yields `Ok(None)` (no
    ///   report, not an error) — M1's `ClusterHandler` has no attribute
    ///   enumeration API, so a device can't yet answer "everything under
    ///   this wildcard".
    /// - A concrete endpoint/cluster/attribute that doesn't exist is an
    ///   error carrying the IM status to report.
    fn resolve_read(&self, path: &AttrPathIn) -> Result<Option<AttrReportOut>, u8> {
        let Some(endpoint) = path.endpoint else {
            return Ok(None);
        };
        let Some((_, clusters)) = self.endpoints.iter().find(|(id, _)| *id == endpoint) else {
            return Err(im::STATUS_UNSUPPORTED_ENDPOINT);
        };
        let Some(cluster) = path.cluster else {
            return Ok(None);
        };
        let Some(handler) = clusters.iter().find(|h| h.cluster_id() == cluster) else {
            return Err(im::STATUS_UNSUPPORTED_CLUSTER);
        };
        let Some(attribute) = path.attribute else {
            return Ok(None);
        };
        let Some(value_tlv) = handler.read(attribute) else {
            return Err(im::STATUS_UNSUPPORTED_ATTRIBUTE);
        };
        Ok(Some(AttrReportOut {
            endpoint,
            cluster,
            attribute,
            data_version: UNVERSIONED,
            value_tlv,
        }))
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

/// Descriptor cluster (spec §9.5), mandatory on every endpoint. `mat-device`
/// serves it only on endpoint 0 for now (a flat, single-endpoint node —
/// `PartsList` is always empty).
struct DescriptorHandler;

impl ClusterHandler for DescriptorHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_DESCRIPTOR
    }

    fn read(&self, attribute: u32) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_DEVICE_TYPE_LIST => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                w.start_struct(Tag::Anonymous); // DeviceTypeStruct
                w.put_uint(Tag::Context(0), u64::from(im::DEVICE_TYPE_ROOT_NODE));
                w.put_uint(Tag::Context(1), 1); // Revision
                w.end_container();
                w.end_container();
                Some(w.finish())
            }
            im::ATTR_SERVER_LIST => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                w.put_uint(Tag::Anonymous, u64::from(im::CLUSTER_DESCRIPTOR));
                w.put_uint(Tag::Anonymous, u64::from(im::CLUSTER_BASIC_INFORMATION));
                w.end_container();
                Some(w.finish())
            }
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
struct BasicInformationHandler;

impl ClusterHandler for BasicInformationHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_BASIC_INFORMATION
    }

    fn read(&self, attribute: u32) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_DATA_MODEL_REVISION => Some(uint_value(DATA_MODEL_REVISION)),
            im::ATTR_VENDOR_ID => Some(uint_value(TEST_VENDOR_ID)),
            im::ATTR_PRODUCT_ID => Some(uint_value(TEST_PRODUCT_ID)),
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
        let mut node = Node::with_root_endpoint();
        let req = im::encode_read_request(
            0,
            im::CLUSTER_BASIC_INFORMATION,
            im::ATTR_DATA_MODEL_REVISION,
        );
        let (opcode, payload) = node
            .handle_im(im::OPCODE_READ_REQUEST, &req, &mut InvokeCtx::default())
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
        let mut node = Node::with_root_endpoint();
        let req = im::encode_read_request(0, im::CLUSTER_DESCRIPTOR, im::ATTR_DEVICE_TYPE_LIST);
        let (opcode, payload) = node
            .handle_im(im::OPCODE_READ_REQUEST, &req, &mut InvokeCtx::default())
            .unwrap();
        assert_eq!(opcode, im::OPCODE_REPORT_DATA);
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert_eq!(
            msg.reports[0].data,
            Some(serde_json::json!([{"0": im::DEVICE_TYPE_ROOT_NODE, "1": 1}]))
        );
    }

    #[test]
    fn read_unknown_attribute_yields_status_response() {
        let mut node = Node::with_root_endpoint();
        let req = im::encode_read_request(0, im::CLUSTER_BASIC_INFORMATION, 0xFFFF);
        let (opcode, payload) = node
            .handle_im(im::OPCODE_READ_REQUEST, &req, &mut InvokeCtx::default())
            .unwrap();
        assert_eq!(opcode, im::OPCODE_STATUS_RESPONSE);
        let status = im::decode_status_response(&payload).unwrap();
        assert_eq!(status, im::STATUS_UNSUPPORTED_ATTRIBUTE);
    }

    #[test]
    fn invoke_unknown_cluster_returns_unsupported_cluster() {
        let mut node = Node::with_root_endpoint();
        let req = im::encode_invoke_request(0, 0x9999, 0, None);
        let (opcode, payload) = node
            .handle_im(im::OPCODE_INVOKE_REQUEST, &req, &mut InvokeCtx::default())
            .unwrap();
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_UNSUPPORTED_CLUSTER);
    }

    #[test]
    fn invoke_known_cluster_unknown_command_returns_unsupported_command() {
        let mut node = Node::with_root_endpoint();
        let req = im::encode_invoke_request(0, im::CLUSTER_BASIC_INFORMATION, 0x7F, None);
        let (opcode, payload) = node
            .handle_im(im::OPCODE_INVOKE_REQUEST, &req, &mut InvokeCtx::default())
            .unwrap();
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_UNSUPPORTED_COMMAND);
    }

    #[test]
    fn unsupported_opcode_is_rejected() {
        let mut node = Node::with_root_endpoint();
        let err = node
            .handle_im(im::OPCODE_WRITE_REQUEST, &[], &mut InvokeCtx::default())
            .unwrap_err();
        assert!(
            matches!(err, ImServerError::UnsupportedOpcode(op) if op == im::OPCODE_WRITE_REQUEST)
        );
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
