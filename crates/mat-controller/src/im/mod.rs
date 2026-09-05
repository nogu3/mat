//! Interaction Model payloads (Matter Core Spec 1.4, Chapter 8).
//!
//! Layout: this file holds the opcode / cluster / attribute / command /
//! status constants, the shared value & error types (`ImValue`, `ImError`,
//! `ReportData`, `InvokeOutcome`, `InvokeResponseData`) and the private TLV
//! helpers every codec uses. The codecs live in one submodule per
//! interaction, re-exported flat so callers keep writing `im::<name>`:
//! `read` (ReadRequest / ReportData), `subscribe`, `invoke` (Invoke /
//! Timed / StatusResponse), `write`, `cmdfields` (per-command CommandFields
//! encoders) and `json` (TLV → JSON).

use crate::tlv::{Reader, Tag, TlvError, Value, Writer};

pub const PROTOCOL_ID_IM: u16 = crate::message::PROTOCOL_ID_INTERACTION_MODEL;
pub const OPCODE_STATUS_RESPONSE: u8 = 0x01;
pub const OPCODE_READ_REQUEST: u8 = 0x02;
pub const OPCODE_SUBSCRIBE_REQUEST: u8 = 0x03;
pub const OPCODE_SUBSCRIBE_RESPONSE: u8 = 0x04;
pub const OPCODE_REPORT_DATA: u8 = 0x05;
pub const OPCODE_WRITE_REQUEST: u8 = 0x06;
pub const OPCODE_WRITE_RESPONSE: u8 = 0x07;
pub const OPCODE_INVOKE_REQUEST: u8 = 0x08;
pub const OPCODE_INVOKE_RESPONSE: u8 = 0x09;
pub const OPCODE_TIMED_REQUEST: u8 = 0x0A;
pub const IM_REVISION: u8 = 12;
pub const CLUSTER_ON_OFF: u32 = 0x0006;
pub const ATTR_ON_OFF: u32 = 0x0000;
pub const CMD_ON_OFF_OFF: u32 = 0x00;
pub const CMD_ON_OFF_ON: u32 = 0x01;
pub const CMD_ON_OFF_TOGGLE: u32 = 0x02;
/// Identify cluster (spec §1.2) — mandatory on every application device
/// type (e.g. On/Off Light, Device Library §4.1).
pub const CLUSTER_IDENTIFY: u32 = 0x0003;
pub const ATTR_IDENTIFY_TIME: u32 = 0x0000;
pub const ATTR_IDENTIFY_TYPE: u32 = 0x0001;
pub const CMD_IDENTIFY: u32 = 0x00;
pub const CMD_IDENTIFY_TRIGGER_EFFECT: u32 = 0x40;
pub const CLUSTER_BASIC_INFORMATION: u32 = 0x0028;
pub const ATTR_DATA_MODEL_REVISION: u32 = 0x0000;
pub const CLUSTER_COLOR_CONTROL: u32 = 0x0300;
pub const ATTR_CURRENT_HUE: u32 = 0x0000;
pub const ATTR_CURRENT_SATURATION: u32 = 0x0001;
pub const ATTR_COLOR_TEMPERATURE_MIREDS: u32 = 0x0007;
pub const CMD_MOVE_TO_HUE_AND_SATURATION: u32 = 0x06;
pub const CMD_MOVE_TO_COLOR_TEMPERATURE: u32 = 0x0A;
pub const CLUSTER_LEVEL_CONTROL: u32 = 0x0008;
pub const ATTR_CURRENT_LEVEL: u32 = 0x0000;
pub const CMD_MOVE_TO_LEVEL: u32 = 0x00;
/// GroupKeyManagement cluster (spec §11.2.7). `KeySetWrite` provisions a
/// device's epoch key for a `GroupKeySetID`.
pub const CLUSTER_GROUP_KEY_MANAGEMENT: u32 = 0x003F;
pub const CMD_KEY_SET_WRITE: u32 = 0x00;
/// `group-key-map` attribute (list of `GroupKeyMapStruct`): maps a `GroupId`
/// to the `GroupKeySetID` used to decrypt its groupcast traffic.
pub const ATTR_GROUP_KEY_MAP: u32 = 0x0000;
/// `group-table` attribute (spec §11.2.7.5, Table 89): read-only —
/// mat-device (Task 4) reports it always empty; no groupcast keying is
/// actually configured yet.
pub const ATTR_GROUP_TABLE: u32 = 0x0001;
pub const ATTR_MAX_GROUPS_PER_FABRIC: u32 = 0x0002;
pub const ATTR_MAX_GROUP_KEYS_PER_FABRIC: u32 = 0x0003;
/// NetworkCommissioning cluster (spec §11.9) — root-node mandatory (Device
/// Library §9.2.2). mat-device (Task 4) only implements the read-only
/// Ethernet shape (`FeatureMap`=Ethernet(0x04), no commands): this is a
/// wired virtual device, so there is nothing to scan/connect at runtime —
/// the single "network" it ever reports is its own egress interface.
pub const CLUSTER_NETWORK_COMMISSIONING: u32 = 0x0031;
pub const ATTR_NC_MAX_NETWORKS: u32 = 0x0000;
pub const ATTR_NC_NETWORKS: u32 = 0x0001;
pub const ATTR_NC_INTERFACE_ENABLED: u32 = 0x0004;
pub const ATTR_NC_LAST_NETWORKING_STATUS: u32 = 0x0005;
pub const ATTR_NC_LAST_NETWORK_ID: u32 = 0x0006;
pub const ATTR_NC_LAST_CONNECT_ERROR_VALUE: u32 = 0x0007;
/// GeneralDiagnostics cluster (spec §11.11) — root-node mandatory (Device
/// Library §9.2.2). mat-device (Task 4) implements the read-only status
/// attributes plus `TestEventTrigger`, which this device always rejects
/// (see `core::general_diagnostics`'s module doc for why).
pub const CLUSTER_GENERAL_DIAGNOSTICS: u32 = 0x0033;
pub const ATTR_GD_NETWORK_INTERFACES: u32 = 0x0000;
pub const ATTR_GD_REBOOT_COUNT: u32 = 0x0001;
pub const ATTR_GD_UP_TIME: u32 = 0x0002;
pub const ATTR_GD_TEST_EVENT_TRIGGERS_ENABLED: u32 = 0x0008;
pub const CMD_TEST_EVENT_TRIGGER: u32 = 0x00;
/// Groups cluster (spec §1.3). `AddGroup` binds an endpoint into a group.
pub const CLUSTER_GROUPS: u32 = 0x0004;
pub const CMD_ADD_GROUP: u32 = 0x00;
pub const CMD_VIEW_GROUP: u32 = 0x01;
pub const CMD_GET_GROUP_MEMBERSHIP: u32 = 0x02;
pub const CMD_REMOVE_GROUP: u32 = 0x03;
pub const CMD_REMOVE_ALL_GROUPS: u32 = 0x04;
pub const CMD_ADD_GROUP_IF_IDENTIFYING: u32 = 0x05;
/// `NameSupport` (spec §1.3.6.1): bit 7 set = group names stored. mat-device
/// reports 0 (no name storage — matches FeatureMap GN=0).
pub const ATTR_GROUPS_NAME_SUPPORT: u32 = 0x0000;
/// AccessControl cluster (spec §11.1). `acl` is the fabric-scoped list of
/// `AccessControlEntryStruct` a device consults to authorize incoming
/// requests (including groupcast, which arrives with `authMode = Group`).
pub const CLUSTER_ACCESS_CONTROL: u32 = 0x001F;
pub const ATTR_ACL: u32 = 0x0000;
/// Fixed capacity attributes (spec §9.10.5) — `mat-device`'s
/// `AccessControlHandler` reports constant values, no real per-table
/// tracking (M2/M3 scope has never come close to any of these limits).
pub const ATTR_ACL_SUBJECTS_PER_ENTRY: u32 = 0x0002;
pub const ATTR_ACL_TARGETS_PER_ENTRY: u32 = 0x0003;
pub const ATTR_ACL_ENTRIES_PER_FABRIC: u32 = 0x0004;
/// Descriptor cluster (spec §9.5). Every endpoint's mandatory
/// "what am I / what's under me" cluster — `mat-device`'s `datamodel`
/// serves it on endpoint 0.
///
/// Hand-written, not sourced from `mat_core::ids`'s generated CHIP table:
/// `mat-controller` has never depended on `mat-core` (and `mat-device` only
/// gets it behind its `net` feature, which `mat-device/core` — this
/// constant's main consumer — must not depend on). Pinned against
/// `mat_core::ids` by `mat-device`'s
/// `core::datamodel::drift_guard::descriptor_cluster_and_attrs_match_mat_core_ids`
/// test (runs under the `net` feature, where both crates are reachable) —
/// a future `ids_gen.rs` regen that moves one of these ids fails that test
/// instead of drifting silently.
pub const CLUSTER_DESCRIPTOR: u32 = 0x001D;
pub const ATTR_DEVICE_TYPE_LIST: u32 = 0x0000;
pub const ATTR_SERVER_LIST: u32 = 0x0001;
pub const ATTR_PARTS_LIST: u32 = 0x0003;
/// RootNode device type (spec §9.2.2), the `DeviceTypeList` entry for
/// endpoint 0. Not pinned by the drift-guard test above: `mat_core::ids`'s
/// generated table only covers clusters/attributes/commands, not device
/// types, so there is no generated source of truth to check this against.
pub const DEVICE_TYPE_ROOT_NODE: u32 = 0x0016;
/// On/Off Light device type (spec §Device Library 4.1), the `DeviceTypeList`
/// entry `mat-device` puts on endpoint 1 (M2's single virtual OnOff
/// device). Same drift-guard exemption as `DEVICE_TYPE_ROOT_NODE` above:
/// `mat_core::ids`'s generated table has no device-type entries to check
/// this against.
pub const DEVICE_TYPE_ON_OFF_LIGHT: u32 = 0x0100;
/// Aggregator device type (Device Library §11.2), the `DeviceTypeList`
/// entry for the bridge's own endpoint (M3's `mat-device` endpoint hosting
/// the bridged devices below it in `PartsList`). Same drift-guard
/// exemption as `DEVICE_TYPE_ROOT_NODE` above.
pub const DEVICE_TYPE_AGGREGATOR: u32 = 0x000E;
/// Bridged Node device type (Device Library §11.1), the `DeviceTypeList`
/// entry every bridged (non-Aggregator) endpoint carries in addition to
/// its own application device type. Same drift-guard exemption as
/// `DEVICE_TYPE_ROOT_NODE` above.
pub const DEVICE_TYPE_BRIDGED_NODE: u32 = 0x0013;
/// BasicInformation cluster attributes (spec §11.1). Same drift-guard
/// coverage as `CLUSTER_DESCRIPTOR` above, via
/// `core::datamodel::drift_guard::basic_information_attrs_match_mat_core_ids`.
pub const ATTR_VENDOR_NAME: u32 = 0x0001;
pub const ATTR_VENDOR_ID: u32 = 0x0002;
pub const ATTR_PRODUCT_NAME: u32 = 0x0003;
pub const ATTR_PRODUCT_ID: u32 = 0x0004;
/// BasicInformation's remaining mandatory attributes (spec §11.1.6 —
/// Apple Home's post-commissioning interview reads all of these; Task 5
/// fills them in alongside the identity attributes above).
pub const ATTR_BI_NODE_LABEL: u32 = 0x0005;
pub const ATTR_BI_LOCATION: u32 = 0x0006;
pub const ATTR_BI_HARDWARE_VERSION: u32 = 0x0007;
pub const ATTR_BI_HARDWARE_VERSION_STRING: u32 = 0x0008;
pub const ATTR_BI_SOFTWARE_VERSION: u32 = 0x0009;
pub const ATTR_BI_SOFTWARE_VERSION_STRING: u32 = 0x000A;
pub const ATTR_BI_UNIQUE_ID: u32 = 0x0012;
pub const ATTR_BI_CAPABILITY_MINIMA: u32 = 0x0013;
pub const ATTR_BI_SPECIFICATION_VERSION: u32 = 0x0015;
pub const ATTR_BI_MAX_PATHS_PER_INVOKE: u32 = 0x0016;
/// BridgedDeviceBasicInformation cluster (spec §9.13) — every bridged
/// (non-Aggregator) endpoint's identity/status cluster, the bridged-side
/// counterpart of BasicInformation above. NodeLabel/UniqueID are shared
/// with BasicInformation's `ATTR_BI_NODE_LABEL`/`ATTR_BI_UNIQUE_ID` above
/// (same attribute ids, distinct cluster).
pub const CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION: u32 = 0x0039;
/// `Reachable` (spec §9.13.4): whether the bridged endpoint's real device
/// is currently reachable over its native protocol.
pub const ATTR_BDBI_REACHABLE: u32 = 0x0011;

/// IM status codes (spec §8.10.1, Table "Status Code Table"). Only the
/// values `mat-device`'s data model dispatch actually returns today.
pub const STATUS_SUCCESS: u8 = 0x00;
pub const STATUS_FAILURE: u8 = 0x01;
/// "The sender of the action does not have authorization or access
/// privilege to carry out the operation" (spec §8.10.1 Table 8-19).
pub const STATUS_UNSUPPORTED_ACCESS: u8 = 0x7E;
pub const STATUS_UNSUPPORTED_ENDPOINT: u8 = 0x7F;
pub const STATUS_UNSUPPORTED_COMMAND: u8 = 0x81;
pub const STATUS_UNSUPPORTED_ATTRIBUTE: u8 = 0x86;
/// "A constraint was violated" (spec §8.10.1 Table 8-19) — e.g. Groups'
/// `AddGroup` with the reserved group id 0 (carried inside
/// `AddGroupResponse.status`, not as an IM-level status).
pub const STATUS_CONSTRAINT_ERROR: u8 = 0x87;
/// "Resource exhausted" — Groups' `AddGroup` when the group table is full.
pub const STATUS_RESOURCE_EXHAUSTED: u8 = 0x89;
/// "The indicated data field or entry could not be found" — Groups'
/// `ViewGroup`/`RemoveGroup` for a group the endpoint is not a member of.
pub const STATUS_NOT_FOUND: u8 = 0x8B;
pub const STATUS_UNSUPPORTED_CLUSTER: u8 = 0xC3;
/// "The receiver requires a fail-safe timer to be armed to accept this
/// action" (spec §8.10.1 Table 8-19). `mat-device`'s commissioning server
/// (Task 9) returns this for `AddNOC` when no `ArmFailSafe` is currently in
/// effect — the command has no fabric context to build a `NOCResponse`
/// against, so it fails at the IM status level rather than inside a
/// `NOCResponse.statusCode`.
pub const STATUS_FAILSAFE_REQUIRED: u8 = 0xCA;
/// "The action is malformed and does not meet the specification and as
/// such produces some error not covered by other status codes in this
/// table" (spec §8.10.1 Table 8-19). `mat-device`'s commissioning server
/// (Task 9) returns this when a command's `CommandFields` TLV fails to
/// decode.
pub const STATUS_INVALID_COMMAND: u8 = 0x85;
/// "The received request cannot be handled" (spec §8.10.1 Table 8-19) —
/// `mat-device`'s data model dispatch (`core::datamodel::Node::handle_im`)
/// returns this `StatusResponse` for any opcode it has no handler for,
/// instead of silently dropping the request. `ReadRequest`/`InvokeRequest`/
/// `WriteRequest` are implemented there. `SubscribeRequest` is implemented
/// too, but one layer up: `mat-device`'s `net::runtime` intercepts it
/// before `handle_im` ever sees it (its own multi-message flow — priming
/// chunks + the subscription's lifetime — doesn't fit a single
/// `StatusResponse`), so it no longer lands here.
pub const STATUS_INVALID_ACTION: u8 = 0x80;
/// "The specified action can't be performed" (spec §8.10.1 Table 8-19) for
/// a `WriteRequest` targeting an attribute the cluster doesn't accept
/// writes to — `mat-device`'s data model dispatch
/// (`core::datamodel::ClusterHandler::write`'s default implementation)
/// returns this for every cluster that doesn't override `write` (as of
/// this writing, `core::access_control::AccessControlHandler` and
/// `core::datamodel::BasicInformationHandler` do; every other cluster still
/// uses the default).
pub const STATUS_UNSUPPORTED_WRITE: u8 = 0x88;

/// Global attribute ids every cluster exposes (spec §7.13, Table
/// "Global Attributes"). `core::datamodel::Node` synthesizes these itself
/// (`ClusterHandler::attributes()` only enumerates cluster-specific
/// attributes) — see that module's `read_attribute_value`.
pub const ATTR_GENERATED_COMMAND_LIST: u32 = 0xFFF8;
pub const ATTR_ACCEPTED_COMMAND_LIST: u32 = 0xFFF9;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0xFFFB;
pub const ATTR_FEATURE_MAP: u32 = 0xFFFC;
pub const ATTR_CLUSTER_REVISION: u32 = 0xFFFD;

/// A decoded scalar attribute/data value. Containers are not supported (M2
/// scope is single scalar attributes such as onoff's `OnOff` bool).
#[derive(Debug, Clone, PartialEq)]
pub enum ImValue {
    Bool(bool),
    Uint(u64),
    Int(i64),
    F32(f32),
    F64(f64),
    Utf8(String),
    Bytes(Vec<u8>),
    Null,
}

/// Decoded ReportData for a single-attribute read (first AttributeReportIB
/// only; see module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportData {
    pub suppress_response: bool,
    pub value: Option<ImValue>,
    pub status: Option<u8>,
}

/// Decoded InvokeResponse outcome for a single command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvokeOutcome {
    pub status: u8,
    pub cluster_status: Option<u8>,
}

/// Decoded InvokeResponse for a single command, including any command-data
/// fields (spec §8.9.4.2: a successful response may carry a CommandDataIB
/// instead of a bare CommandStatusIB — e.g. commands that return a value).
/// `fields_tlv`, when present, is one complete, well-formed TLV element (its
/// top-level tag re-written to `Tag::Anonymous`) holding the response
/// CommandFields struct, ready to hand to a cluster-specific decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeResponseData {
    pub status: u8,
    pub cluster_status: Option<u8>,
    pub fields_tlv: Option<Vec<u8>>,
}

/// Interaction Model level errors (decode failures and device-reported
/// rejections carried in IM status codes, spec §8.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImError {
    Tlv(TlvError),
    Malformed(&'static str),
    UnsupportedValue,
    AttributeStatus(u8),
    StatusResponse(u8),
    CommandStatus {
        status: u8,
        cluster_status: Option<u8>,
    },
}

impl std::fmt::Display for ImError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImError::Tlv(e) => write!(f, "malformed interaction model TLV: {e}"),
            ImError::Malformed(m) => write!(f, "malformed interaction model payload: {m}"),
            ImError::UnsupportedValue => write!(f, "unsupported attribute value encoding"),
            ImError::AttributeStatus(s) => write!(f, "device rejected read: IM status 0x{s:02X}"),
            ImError::StatusResponse(s) => {
                write!(f, "device sent StatusResponse: IM status 0x{s:02X}")
            }
            ImError::CommandStatus {
                status,
                cluster_status: Some(cs),
            } => write!(
                f,
                "device rejected command: IM status 0x{status:02X} (cluster status 0x{cs:02X})"
            ),
            ImError::CommandStatus {
                status,
                cluster_status: None,
            } => write!(f, "device rejected command: IM status 0x{status:02X}"),
        }
    }
}

impl std::error::Error for ImError {}

impl From<TlvError> for ImError {
    fn from(e: TlvError) -> Self {
        ImError::Tlv(e)
    }
}

mod json;
pub use json::*;
mod subscribe;
pub use subscribe::*;
mod read;
pub use read::*;
mod cmdfields;
pub use cmdfields::*;
mod invoke;
pub use invoke::*;
mod write;
pub use write::*;

/// Reads the next element and requires it to be a struct start (every IM
/// message is a top-level anonymous struct).
fn expect_struct_start(r: &mut Reader) -> Result<(), ImError> {
    match r.next()?.ok_or(ImError::Malformed("empty payload"))?.value {
        Value::StructStart => Ok(()),
        _ => Err(ImError::Malformed("expected struct")),
    }
}

/// Consumes the rest of a container whose `*Start` element has already been
/// read (depth 1), including its matching `ContainerEnd`. Used to skip
/// unknown tags/containers and additional report/response entries beyond
/// the first (M2 only interprets a single attribute/command per message).
/// Delegates to `tlv::skip_container`, restoring this module's error wording.
fn skip_container(r: &mut Reader) -> Result<(), ImError> {
    crate::tlv::skip_container(r).map_err(|e| match e {
        TlvError::Truncated => ImError::Malformed("truncated container"),
        other => ImError::from(other),
    })
}

fn value_to_im(v: Value) -> Result<ImValue, ImError> {
    match v {
        Value::Bool(b) => Ok(ImValue::Bool(b)),
        Value::Uint(u) => Ok(ImValue::Uint(u)),
        Value::Int(i) => Ok(ImValue::Int(i)),
        Value::Utf8(s) => Ok(ImValue::Utf8(s.to_string())),
        Value::Bytes(b) => Ok(ImValue::Bytes(b.to_vec())),
        Value::F32(f) => Ok(ImValue::F32(f)),
        Value::F64(f) => Ok(ImValue::F64(f)),
        Value::Null => Ok(ImValue::Null),
        Value::StructStart | Value::ArrayStart | Value::ListStart => Err(ImError::UnsupportedValue),
        Value::ContainerEnd => Err(ImError::Malformed("unexpected container end as data value")),
    }
}

/// Encodes an `ImValue` scalar as one standalone, well-formed TLV element
/// (tag is discarded by the caller — `encode_write_request` immediately
/// splices it via `Writer::put_raw_element`).
fn encode_im_value(value: &ImValue) -> Vec<u8> {
    let mut w = Writer::new();
    match value {
        ImValue::Bool(b) => w.put_bool(Tag::Anonymous, *b),
        ImValue::Uint(u) => w.put_uint(Tag::Anonymous, *u),
        ImValue::Int(i) => w.put_int(Tag::Anonymous, *i),
        ImValue::F32(f) => w.put_f32(Tag::Anonymous, *f),
        ImValue::F64(f) => w.put_f64(Tag::Anonymous, *f),
        ImValue::Utf8(s) => w.put_str(Tag::Anonymous, s),
        ImValue::Bytes(b) => w.put_bytes(Tag::Anonymous, b),
        ImValue::Null => w.put_null(Tag::Anonymous),
    }
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_fields_splice_into_invoke_request() {
        // fields_tlv スプライス経路（well-formed 1 要素として受理され panic しない）
        let fields = encode_move_to_hue_and_saturation_fields(1, 2, 3);
        let req = encode_invoke_request(
            1,
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(&fields),
        );
        assert!(!req.is_empty());
    }

    #[test]
    fn group_invoke_request_carries_fields() {
        let fields = encode_move_to_color_temperature_fields(370, 0);
        let got = encode_group_invoke_request(
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_COLOR_TEMPERATURE,
            Some(&fields),
        );
        // fields が ctx1 で CommandDataIB に入ること（unicast 版と同じ再タグ規約）。
        // 厳密比較: unicast 版のテストに倣い Writer で期待列を組む。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), true);
        w.put_bool(Tag::Context(1), false);
        w.start_array(Tag::Context(2));
        w.start_struct(Tag::Anonymous);
        w.start_list(Tag::Context(0));
        w.put_uint(Tag::Context(1), u64::from(CLUSTER_COLOR_CONTROL));
        w.put_uint(Tag::Context(2), u64::from(CMD_MOVE_TO_COLOR_TEMPERATURE));
        w.end_container();
        w.start_struct(Tag::Context(1));
        w.put_uint(Tag::Context(0), 370);
        w.put_uint(Tag::Context(1), 0);
        w.put_uint(Tag::Context(2), 0);
        w.put_uint(Tag::Context(3), 0);
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
        w.end_container();
        assert_eq!(got, w.finish());
    }

    #[test]
    fn im_value_floats_roundtrip_through_encode_and_decode() {
        for v in [ImValue::F32(1.5), ImValue::F64(-2.25)] {
            let tlv = encode_im_value(&v);
            // 要素型: single = 0x0A, double = 0x0B（anonymous tag → control byte だけ）。
            let expect = if matches!(v, ImValue::F32(_)) {
                0x0A
            } else {
                0x0B
            };
            assert_eq!(tlv[0] & 0x1F, expect, "{v:?}");
            let mut r = Reader::new(&tlv);
            let el = r.next().unwrap().unwrap();
            assert_eq!(value_to_im(el.value).unwrap(), v);
        }
    }
}
