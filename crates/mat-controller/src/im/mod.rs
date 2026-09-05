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

use crate::tlv::{copy_value, Reader, Tag, TlvError, Value, Writer};

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
use read::{decode_attribute_path_ib, decode_attribute_status_ib};

/// Reads the next element and requires it to be a struct start (every IM
/// message is a top-level anonymous struct).
pub(super) fn expect_struct_start(r: &mut Reader) -> Result<(), ImError> {
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
pub(super) fn skip_container(r: &mut Reader) -> Result<(), ImError> {
    crate::tlv::skip_container(r).map_err(|e| match e {
        TlvError::Truncated => ImError::Malformed("truncated container"),
        other => ImError::from(other),
    })
}

pub(super) fn value_to_im(v: Value) -> Result<ImValue, ImError> {
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
pub(super) fn encode_im_value(value: &ImValue) -> Vec<u8> {
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

/// CommandFields for colorcontrol MoveToHueAndSaturation (cluster spec
/// §3.2.11.7): `{0: hue, 1: saturation, 2: transition_time (0.1 s units),
/// 3: options_mask, 4: options_override}`. Options are fixed to 0 (execute
/// unconditionally), which is what chip-tool sends by default too.
pub fn encode_move_to_hue_and_saturation_fields(
    hue: u8,
    saturation: u8,
    transition_time_ds: u16,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(hue));
    w.put_uint(Tag::Context(1), u64::from(saturation));
    w.put_uint(Tag::Context(2), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(3), 0);
    w.put_uint(Tag::Context(4), 0);
    w.end_container();
    w.finish()
}

/// CommandFields for colorcontrol MoveToColorTemperature (cluster spec
/// §3.2.11.10): `{0: ColorTemperatureMireds(u16), 1: TransitionTime(u16,
/// 0.1 s units), 2: OptionsMask(u8), 3: OptionsOverride(u8)}`. Options are
/// fixed to 0 (execute per the device's Options attribute), matching what
/// chip-tool sends by default.
pub fn encode_move_to_color_temperature_fields(mireds: u16, transition_time_ds: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(mireds));
    w.put_uint(Tag::Context(1), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0);
    w.end_container();
    w.finish()
}

/// CommandFields for levelcontrol MoveToLevel (cluster spec §1.6.7.1):
/// `{0: Level(u8), 1: TransitionTime(u16, 0.1 s units), 2: OptionsMask(u8),
/// 3: OptionsOverride(u8)}`. Options are fixed to 0 (execute per the
/// device's Options attribute), matching what chip-tool sends by default.
pub fn encode_move_to_level_fields(level: u8, transition_time_ds: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(level));
    w.put_uint(Tag::Context(1), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0);
    w.end_container();
    w.finish()
}

/// CommandFields for GroupKeyManagement KeySetWrite (cluster spec §11.2.8.4):
/// `{0: GroupKeySetStruct{0: GroupKeySetID(u16), 1: GroupKeySecurityPolicy(0
/// = TrustFirst), 2: EpochKey0(16B octstr), 3: EpochStartTime0(u64, epoch-us),
/// 4: EpochKey1(null), 5: EpochStartTime1(null), 6: EpochKey2(null), 7:
/// EpochStartTime2(null)}}`. Only a single active epoch (0) is provisioned —
/// matches the chip-tool `groupkeymanagement key-set-write` JSON this mirrors
/// (`commands/group.rs`'s `key_set` object). `epochStartTime0` is fixed to 1
/// (matching `mat_core::group::EPOCH_START_TIME`, which the controller-side
/// `groupsettings add-keysets` validityTime must also match).
pub fn encode_key_set_write_fields(keyset_id: u16, epoch_key: &[u8; 16]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(0)); // GroupKeySet
    w.put_uint(Tag::Context(0), u64::from(keyset_id));
    w.put_uint(Tag::Context(1), 0); // GroupKeySecurityPolicy: TrustFirst
    w.put_bytes(Tag::Context(2), epoch_key);
    w.put_uint(Tag::Context(3), 1); // EpochStartTime0
    w.put_null(Tag::Context(4));
    w.put_null(Tag::Context(5));
    w.put_null(Tag::Context(6));
    w.put_null(Tag::Context(7));
    w.end_container();
    w.end_container();
    w.finish()
}

/// `group-key-map` attribute Data TLV (list of `GroupKeyMapStruct{1: GroupId,
/// 2: GroupKeySetID}`, spec §11.2.7.6) — the write is a **full replace**, so
/// callers must pass the final merged list (existing entries + the one being
/// added/updated), not just the delta. `fabricIndex` (field 254) is omitted:
/// the server substitutes it from the write's accessing fabric.
pub fn encode_group_key_map_tlv(entries: &[(u16, u16)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (group_id, keyset_id) in entries {
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(*group_id));
        w.put_uint(Tag::Context(2), u64::from(*keyset_id));
        w.end_container();
    }
    w.end_container();
    w.finish()
}

/// CommandFields for Groups AddGroup (cluster spec §1.3.6.1): `{0:
/// GroupID(u16), 1: GroupName(str, <= 16 chars per spec, unchecked here)}`.
pub fn encode_add_group_fields(group_id: u16, name: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(group_id));
    w.put_str(Tag::Context(1), name);
    w.end_container();
    w.finish()
}

/// InvokeRequestMessage (spec §8.9.4) の共通本体。`timed` が TimedRequest
/// フィールド（タイムド呼び出し、spec §8.5）の値になる。公開関数
/// `encode_invoke_request` / `encode_invoke_request_timed` はどちらもこれを
/// 呼ぶだけの薄いラッパで、ワイヤ形状は完全に共有する。
///
/// `fields_tlv`, if given, must be one complete, well-formed TLV element
/// (any tag; it is re-tagged) holding the command's CommandFields struct.
/// M2's onoff commands (on/off/toggle) take no fields, so this is `None` in
/// practice; the parameter exists so the wire format doesn't have to change
/// when a fielded command is added later. Panics if `fields_tlv` is not
/// well-formed TLV — a caller/programmer error, not a device response to
/// validate defensively.
fn encode_invoke_request_inner(
    endpoint: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
    timed: bool,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse
    w.put_bool(Tag::Context(1), timed); // TimedRequest
    w.start_array(Tag::Context(2)); // InvokeRequests
    w.start_struct(Tag::Anonymous); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPath
    w.put_uint(Tag::Context(0), u64::from(endpoint));
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(command));
    w.end_container(); // CommandPath
    if let Some(fields) = fields_tlv {
        w.put_raw_element(Tag::Context(1), fields);
    }
    w.end_container(); // CommandDataIB
    w.end_container(); // InvokeRequests
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// InvokeRequestMessage (spec §8.9.4) for a single command. TimedRequest is
/// always `false` — see `encode_invoke_request_timed` for the timed variant
/// (spec §8.5, タイムド呼び出し).
pub fn encode_invoke_request(
    endpoint: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Vec<u8> {
    encode_invoke_request_inner(endpoint, cluster, command, fields_tlv, false)
}

/// InvokeRequestMessage (spec §8.9.4) with TimedRequest = true. Must be sent
/// on the same exchange as a preceding `encode_timed_request` whose
/// StatusResponse(SUCCESS) has already been received — the timeout window it
/// establishes covers exactly this InvokeRequest (spec §8.5.1). Same fields
/// contract as `encode_invoke_request` otherwise.
pub fn encode_invoke_request_timed(
    endpoint: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Vec<u8> {
    encode_invoke_request_inner(endpoint, cluster, command, fields_tlv, true)
}

/// TimedRequestMessage (spec §8.5.1, タイムド呼び出し): `{0: Timeout(u16,
/// ミリ秒), 255: InteractionModelRevision}`. Opens a timeout window during
/// which the immediately following InvokeRequest/WriteRequest (same
/// exchange, TimedRequest flag true) must arrive at the device, otherwise it
/// rejects the timed action. `mat-controller` only uses this ahead of a
/// timed invoke (`SecureSession::invoke_for_data`).
pub fn encode_timed_request(timeout_ms: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(timeout_ms));
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

/// InvokeRequestMessage for a groupcast command (spec §8.9.4): group
/// invokes carry no response, so SuppressResponse is true, and the
/// CommandPath is group-scoped (no endpoint — the device's group table
/// routes to its bound endpoints). Fields contract matches
/// `encode_invoke_request`.
pub fn encode_group_invoke_request(
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), true); // SuppressResponse
    w.put_bool(Tag::Context(1), false); // TimedRequest
    w.start_array(Tag::Context(2)); // InvokeRequests
    w.start_struct(Tag::Anonymous); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPath (group-scoped)
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(command));
    w.end_container();
    if let Some(fields) = fields_tlv {
        w.put_raw_element(Tag::Context(1), fields);
    }
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

/// Decoded InvokeRequestMessage for a single command: server-side
/// counterpart of `encode_invoke_request`/`encode_invoke_request_timed`.
/// `fields_tlv` is empty when the request carried no CommandFields.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeRequestIn {
    pub endpoint: u16,
    pub cluster: u32,
    pub command: u32,
    pub fields_tlv: Vec<u8>,
    pub suppress_response: bool,
    pub timed: bool,
}

/// `decode_request_command_data_ib`'s return: (endpoint, cluster, command,
/// fields_tlv).
type RequestCommandDataFields = (Option<u16>, Option<u32>, Option<u32>, Vec<u8>);

/// CommandDataIB (spec §8.9.4.2): `{0: CommandPath{0:endpoint,1:cluster,
/// 2:command}, 1: CommandFields}`, request-side variant that also extracts
/// the path (`decode_command_data_ib` only extracts fields, for the
/// response side where the path is already known to the caller). Assumes
/// the caller already consumed the anonymous `StructStart` opening this
/// CommandDataIB (an InvokeRequests entry).
fn decode_request_command_data_ib(r: &mut Reader) -> Result<RequestCommandDataFields, ImError> {
    let mut endpoint = None;
    let mut cluster = None;
    let mut command = None;
    let mut fields_tlv = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated command data ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => {
                // CommandPath
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated command path"))?;
                    match (e2.tag, e2.value) {
                        (_, Value::ContainerEnd) => break,
                        (Tag::Context(0), Value::Uint(v)) => {
                            endpoint = Some(u16::try_from(v).map_err(|_| {
                                ImError::Malformed("command path endpoint out of range")
                            })?);
                        }
                        (Tag::Context(1), Value::Uint(v)) => {
                            cluster = Some(u32::try_from(v).map_err(|_| {
                                ImError::Malformed("command path cluster out of range")
                            })?);
                        }
                        (Tag::Context(2), Value::Uint(v)) => {
                            command = Some(u32::try_from(v).map_err(|_| {
                                ImError::Malformed("command path command out of range")
                            })?);
                        }
                        (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                            skip_container(r)?;
                        }
                        _ => {}
                    }
                }
            }
            (Tag::Context(1), Value::StructStart) => {
                // CommandFields: re-tag to Anonymous, same convention as
                // `decode_command_data_ib`'s response-side echo.
                let mut w = Writer::new();
                copy_value(&mut w, r, Tag::Anonymous, Value::StructStart)?;
                fields_tlv = w.finish();
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok((endpoint, cluster, command, fields_tlv))
}

/// InvokeRequestMessage (spec §8.9.4): server-side decode of
/// `encode_invoke_request`/`encode_invoke_request_timed`'s payload. Only
/// the first InvokeRequestIB is interpreted (mirrors `decode_invoke_response`'s
/// single-command scope).
pub fn decode_invoke_request(payload: &[u8]) -> Result<InvokeRequestIn, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut suppress_response = false;
    let mut timed = false;
    let mut endpoint = None;
    let mut cluster = None;
    let mut command = None;
    let mut fields_tlv = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke request"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Bool(b)) => suppress_response = b,
            (Tag::Context(1), Value::Bool(b)) => timed = b,
            (Tag::Context(2), Value::ArrayStart) => {
                // InvokeRequests
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated invoke requests"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            let (ep, cl, cmd, fields) = decode_request_command_data_ib(&mut r)?;
                            endpoint = ep;
                            cluster = cl;
                            command = cmd;
                            fields_tlv = fields;
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed("unexpected element in invoke requests"))
                        }
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    Ok(InvokeRequestIn {
        endpoint: endpoint.ok_or(ImError::Malformed("invoke request without endpoint"))?,
        cluster: cluster.ok_or(ImError::Malformed("invoke request without cluster"))?,
        command: command.ok_or(ImError::Malformed("invoke request without command"))?,
        fields_tlv,
        suppress_response,
        timed,
    })
}

/// StatusIB (spec §8.9.2.3) inside a CommandStatusIB: `{0: status, 1: cluster_status}`.
/// Assumes the caller already consumed the `StructStart` (tag 1) opening it.
fn decode_status_ib(r: &mut Reader) -> Result<(u8, Option<u8>), ImError> {
    let mut status = None;
    let mut cluster_status = None;
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated status ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                status = Some(
                    u8::try_from(v)
                        .map_err(|_| ImError::Malformed("command status code out of range"))?,
                );
            }
            (Tag::Context(1), Value::Uint(v)) => {
                cluster_status = Some(
                    u8::try_from(v)
                        .map_err(|_| ImError::Malformed("cluster status code out of range"))?,
                );
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    let status = status.ok_or(ImError::Malformed("status ib without status"))?;
    Ok((status, cluster_status))
}

/// CommandStatusIB (spec §8.9.4.2): `{0: CommandPath, 1: StatusIB}`.
/// Assumes the caller already consumed the `StructStart` (tag 1) that opens
/// this CommandStatusIB (InvokeResponseIB's `Status` field).
fn decode_command_status_ib(r: &mut Reader) -> Result<(u8, Option<u8>), ImError> {
    let mut result = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated command status ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::StructStart) => {
                result = Some(decode_status_ib(r)?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    result.ok_or(ImError::Malformed("command status ib without StatusIB"))
}

/// InvokeResponseIB (spec §8.9.4.2): `{0: CommandDataIB} | {1: CommandStatusIB}`.
/// Assumes the caller already consumed the anonymous `StructStart` opening
/// this InvokeResponseIB.
fn decode_invoke_response_ib(r: &mut Reader) -> Result<InvokeOutcome, ImError> {
    let mut outcome = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke response ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::StructStart) => {
                // Command (CommandDataIB): a response carrying data is a
                // successful invocation. M2's onoff commands never produce
                // one, but don't choke on a well-formed message that does.
                skip_container(r)?;
                outcome = Some(InvokeOutcome {
                    status: 0,
                    cluster_status: None,
                });
            }
            (Tag::Context(1), Value::StructStart) => {
                let (status, cluster_status) = decode_command_status_ib(r)?;
                outcome = Some(InvokeOutcome {
                    status,
                    cluster_status,
                });
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    outcome.ok_or(ImError::Malformed(
        "invoke response ib without Command or Status",
    ))
}

/// InvokeResponseMessage (spec §8.9.4). Only the first InvokeResponseIB is
/// interpreted (M2 invokes one command at a time).
pub fn decode_invoke_response(payload: &[u8]) -> Result<InvokeOutcome, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut outcome: Option<InvokeOutcome> = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::ArrayStart) => {
                // InvokeResponses
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated invoke responses"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            outcome = Some(decode_invoke_response_ib(&mut r)?);
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed(
                                "unexpected element in invoke responses",
                            ))
                        }
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    outcome.ok_or(ImError::Malformed(
        "invoke response without InvokeResponseIB",
    ))
}

/// CommandDataIB (spec §8.9.4.2): `{0: CommandPathIB, 1: CommandFields}`.
/// Assumes the caller already consumed the `StructStart` (tag 0) that opens
/// this CommandDataIB (InvokeResponseIB's `Command` field). Returns the
/// CommandFields struct (tag 1), if present, re-tagged to `Tag::Anonymous`
/// as one complete TLV element — the CommandPathIB (tag 0) is skipped since
/// `decode_invoke_response_data`'s callers only need the fields, not the
/// echoed path.
fn decode_command_data_ib(r: &mut Reader) -> Result<Option<Vec<u8>>, ImError> {
    let mut fields = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated command data ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::StructStart) => {
                // CommandFields: always a struct (cluster spec command
                // parameters). Re-tag to Anonymous, same convention as
                // `encode_invoke_request`'s fields_tlv splice.
                let mut w = Writer::new();
                copy_value(&mut w, r, Tag::Anonymous, Value::StructStart)?;
                fields = Some(w.finish());
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok(fields)
}

/// InvokeResponseIB (spec §8.9.4.2): `{0: CommandDataIB} | {1: CommandStatusIB}`,
/// decoded into `InvokeResponseData` (data-carrying variant of
/// `decode_invoke_response_ib`). Assumes the caller already consumed the
/// anonymous `StructStart` opening this InvokeResponseIB.
fn decode_invoke_response_ib_data(r: &mut Reader) -> Result<InvokeResponseData, ImError> {
    let mut result = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke response ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::StructStart) => {
                // Command (CommandDataIB): a response carrying data is a
                // successful invocation (status 0), possibly with fields.
                let fields_tlv = decode_command_data_ib(r)?;
                result = Some(InvokeResponseData {
                    status: 0,
                    cluster_status: None,
                    fields_tlv,
                });
            }
            (Tag::Context(1), Value::StructStart) => {
                let (status, cluster_status) = decode_command_status_ib(r)?;
                result = Some(InvokeResponseData {
                    status,
                    cluster_status,
                    fields_tlv: None,
                });
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    result.ok_or(ImError::Malformed(
        "invoke response ib without Command or Status",
    ))
}

/// InvokeResponseMessage (spec §8.9.4), data-carrying variant of
/// `decode_invoke_response`: a CommandDataIB response (status 0) yields its
/// CommandFields as `fields_tlv`; a CommandStatusIB response yields
/// `status`/`cluster_status` as today with `fields_tlv: None`. Only the
/// first InvokeResponseIB is interpreted (same single-command scope as
/// `decode_invoke_response`). Unlike `decode_invoke_response`, a non-zero
/// status is returned as data, not as `Err` — callers that want the
/// today's fail-on-error behavior should check `status` themselves (see
/// `SecureSession::invoke_for_data`).
pub fn decode_invoke_response_data(payload: &[u8]) -> Result<InvokeResponseData, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut result: Option<InvokeResponseData> = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::ArrayStart) => {
                // InvokeResponses
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated invoke responses"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            result = Some(decode_invoke_response_ib_data(&mut r)?);
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed(
                                "unexpected element in invoke responses",
                            ))
                        }
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    result.ok_or(ImError::Malformed(
        "invoke response without InvokeResponseIB",
    ))
}

/// InvokeResponseMessage (spec §8.9.4) for a single command's
/// CommandStatusIB (status, not data): server-side counterpart of
/// `decode_invoke_response`/`decode_invoke_response_data`. Echoes the
/// CommandPath (spec §8.9.4.2) so a well-behaved controller can correlate
/// the status against the command it invoked.
///
/// `SuppressResponse`（タグ 0, bool）は spec §8.9.4 で **mandatory**。
/// 自前の decoder は未知タグを読み飛ばすので欠けても往復は通るが、chip の
/// `CommandSender::ProcessInvokeResponse` は `GetSuppressResponse` を必ず
/// 引き、無ければ `CHIP Error 0x00000021: End of TLV` で invoke を失敗に
/// する（M2 ゲート 1 の実測 —
/// `docs/superpowers/plans/m2-chip-tool-probe.md`）。デバイス側の応答は
/// 常に `false`（応答を出している時点で抑制していない）。
pub fn encode_invoke_response_status(
    endpoint: u16,
    cluster: u32,
    command: u32,
    status: u8,
    cluster_status: Option<u8>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse — mandatory, see below
    w.start_array(Tag::Context(1)); // InvokeResponses
    w.start_struct(Tag::Anonymous); // InvokeResponseIB
    w.start_struct(Tag::Context(1)); // CommandStatusIB
    w.start_list(Tag::Context(0)); // CommandPath
    w.put_uint(Tag::Context(0), u64::from(endpoint));
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(command));
    w.end_container(); // CommandPath
    w.start_struct(Tag::Context(1)); // StatusIB
    w.put_uint(Tag::Context(0), u64::from(status));
    if let Some(cs) = cluster_status {
        w.put_uint(Tag::Context(1), u64::from(cs));
    }
    w.end_container(); // StatusIB
    w.end_container(); // CommandStatusIB
    w.end_container(); // InvokeResponseIB
    w.end_container(); // InvokeResponses
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// InvokeResponseMessage (spec §8.9.4) for a single command's CommandDataIB
/// (a successful invocation that returns data — e.g. a cluster's response
/// command). `fields_tlv` must be one complete, well-formed TLV element
/// (any top-level tag; re-tagged on splice) holding the response
/// CommandFields struct, or an empty slice for a data response with no
/// fields. `response_command` goes in the echoed CommandPath's CommandId,
/// same field `decode_command_data_ib`'s caller ignores today (it only
/// needs the fields) but that a spec-faithful controller would use to
/// distinguish response commands from the invoked one. `SuppressResponse`
/// は `encode_invoke_response_status` と同じ理由で常に書き出す（その doc
/// コメント参照）。
pub fn encode_invoke_response_data(
    endpoint: u16,
    cluster: u32,
    response_command: u32,
    fields_tlv: &[u8],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse — mandatory, see below
    w.start_array(Tag::Context(1)); // InvokeResponses
    w.start_struct(Tag::Anonymous); // InvokeResponseIB
    w.start_struct(Tag::Context(0)); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPath
    w.put_uint(Tag::Context(0), u64::from(endpoint));
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(response_command));
    w.end_container(); // CommandPath
    if !fields_tlv.is_empty() {
        w.put_raw_element(Tag::Context(1), fields_tlv); // CommandFields
    }
    w.end_container(); // CommandDataIB
    w.end_container(); // InvokeResponseIB
    w.end_container(); // InvokeResponses
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// StatusResponseMessage (spec §8.9.3): `{0: Status, 255: revision}`.
pub fn encode_status_response(status: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(status));
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

pub fn decode_status_response(payload: &[u8]) -> Result<u8, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated status response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                status = Some(
                    u8::try_from(v)
                        .map_err(|_| ImError::Malformed("status response code out of range"))?,
                );
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    status.ok_or(ImError::Malformed("status response without status"))
}

/// WriteRequestMessage (spec §8.9.2.4) の共通本体。`timed` が TimedRequest
/// フィールドの値になる。公開関数 `encode_write_request_tlv` /
/// `encode_write_request_tlv_timed` はどちらもこれを呼ぶだけの薄いラッパで、
/// `encode_invoke_request` / `encode_invoke_request_timed` と同じ手筋。
fn encode_write_request_inner(
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    data_tlv: &[u8],
    timed: bool,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse
    w.put_bool(Tag::Context(1), timed); // TimedRequest
    w.start_array(Tag::Context(2)); // WriteRequests
    w.start_struct(Tag::Anonymous); // AttributeDataIB
    w.start_list(Tag::Context(1)); // AttributePathIB
    w.put_uint(Tag::Context(2), u64::from(endpoint));
    w.put_uint(Tag::Context(3), u64::from(cluster));
    w.put_uint(Tag::Context(4), u64::from(attribute));
    w.end_container(); // AttributePathIB
    w.put_raw_element(Tag::Context(2), data_tlv); // Data
    w.end_container(); // AttributeDataIB
    w.end_container(); // WriteRequests
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// WriteRequestMessage (spec §8.9.2.4) for a single attribute path.
/// TimedRequest is always `false` — see `encode_write_request_tlv_timed` for
/// the timed variant (spec §8.5, タイムド呼び出し). `data_tlv` must be one
/// complete, well-formed TLV element (any top-level tag; it is re-tagged) —
/// the attribute's `Data` value.
pub fn encode_write_request_tlv(
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    data_tlv: &[u8],
) -> Vec<u8> {
    encode_write_request_inner(endpoint, cluster, attribute, data_tlv, false)
}

/// WriteRequestMessage (spec §8.9.2.4) with TimedRequest = true. Must be
/// sent on the same exchange as a preceding `encode_timed_request` whose
/// StatusResponse(SUCCESS) has already been received (spec §8.5.1). Same
/// `data_tlv` contract as `encode_write_request_tlv`.
pub fn encode_write_request_tlv_timed(
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    data_tlv: &[u8],
) -> Vec<u8> {
    encode_write_request_inner(endpoint, cluster, attribute, data_tlv, true)
}

/// Scalar sugar over `encode_write_request_tlv`: encodes `value` as TLV and
/// splices it in as the `Data` element. M2-scope values only (see `ImValue`).
pub fn encode_write_request(
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    value: &ImValue,
) -> Vec<u8> {
    encode_write_request_tlv(endpoint, cluster, attribute, &encode_im_value(value))
}

/// Decoded AttributeDataIB (spec §8.9.2.2) from a WriteRequest's
/// `WriteRequests` array: server-side counterpart of `encode_write_request_tlv`/
/// `encode_write_request_tlv_timed`. `None` path fields are wildcards (omitted
/// on the wire, as `AttrPathIn`'s doc explains for reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAttrIn {
    pub endpoint: Option<u16>,
    pub cluster: Option<u32>,
    pub attribute: Option<u32>,
    /// The AttributeDataIB's `Data` element (tag 2), re-tagged to `Anonymous`
    /// as one complete, well-formed TLV element — same convention as
    /// `encode_write_request_tlv`'s `data_tlv` input, just decoded instead of
    /// encoded.
    pub data_tlv: Vec<u8>,
    /// Whether the path's AttributePathIB carried a `ListIndex` (spec
    /// §8.9.2.2) — i.e. this write targets one element of a list attribute
    /// rather than replacing the whole attribute. chip-tool-family
    /// controllers write a list attribute as "replace whole list" followed
    /// by a chunk train of `ListIndex: null` appends; `mat-device`'s data
    /// model dispatch has no list-attribute write implemented yet and so no
    /// consumer for this, but plumbing it through from
    /// `decode_attribute_path_ib` now means a future list-attribute
    /// `ClusterHandler::write` doesn't need another wire-decode change.
    pub list_append: bool,
}

/// Decoded WriteRequestMessage (spec §8.9.2.4): server-side counterpart of
/// `encode_write_request_tlv`/`encode_write_request_tlv_timed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequestIn {
    pub timed: bool,
    pub suppress_response: bool,
    pub writes: Vec<WriteAttrIn>,
}

/// AttributeDataIB (spec §8.9.2.2) as it appears inside a WriteRequest's
/// `WriteRequests` array: `{0: DataVersion?, 1: Path(list), 2: Data}`.
/// Assumes the caller already consumed the anonymous `StructStart` opening
/// this AttributeDataIB. Unlike `decode_attribute_data_ib` (M2, report-side,
/// scalar-only `ImValue`), this keeps `Data` as a raw re-tagged TLV element
/// (any shape) and also extracts the path — a write, unlike a report, always
/// carries both.
fn decode_write_attribute_data_ib(r: &mut Reader) -> Result<WriteAttrIn, ImError> {
    let mut endpoint = None;
    let mut cluster = None;
    let mut attribute = None;
    let mut data_tlv = None;
    let mut list_append = false;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::ListStart) => {
                let (ep, cl, attr, la) = decode_attribute_path_ib(r)?;
                endpoint = ep;
                cluster = cl;
                attribute = attr;
                list_append = la;
            }
            (Tag::Context(2), v) => {
                // Data: re-tag to Anonymous, same convention as
                // `encode_write_request_tlv`'s `data_tlv` input and
                // `decode_invoke_request`'s CommandFields echo.
                let mut w = Writer::new();
                copy_value(&mut w, r, Tag::Anonymous, v)?;
                data_tlv = Some(w.finish());
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok(WriteAttrIn {
        endpoint,
        cluster,
        attribute,
        data_tlv: data_tlv.ok_or(ImError::Malformed("write attribute without Data field"))?,
        list_append,
    })
}

/// `WriteRequests`（array[AttributeDataIB]）の中身を読む共通処理。呼び出し側は
/// array を開く `ArrayStart` を既に読んでいる前提で、対応する `ContainerEnd`
/// まで読み切る。`decode_attribute_requests`（read 側）の write 版。
fn decode_write_requests_array(r: &mut Reader) -> Result<Vec<WriteAttrIn>, ImError> {
    let mut writes = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated write requests"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::StructStart => writes.push(decode_write_attribute_data_ib(r)?),
            Value::ArrayStart | Value::ListStart => skip_container(r)?,
            _ => return Err(ImError::Malformed("unexpected element in write requests")),
        }
    }
    Ok(writes)
}

/// WriteRequestMessage (spec §8.9.2.4): server-side decode of
/// `encode_write_request_tlv`/`encode_write_request_tlv_timed`'s payload.
/// Returns every AttributeDataIB in `WriteRequests` (tag 2) — a device must
/// answer every write a controller asks for, mirroring `decode_read_request`.
pub fn decode_write_request(payload: &[u8]) -> Result<WriteRequestIn, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut suppress_response = false;
    let mut timed = false;
    let mut writes = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated write request"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Bool(b)) => suppress_response = b,
            (Tag::Context(1), Value::Bool(b)) => timed = b,
            (Tag::Context(2), Value::ArrayStart) => {
                writes = decode_write_requests_array(&mut r)?;
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    Ok(WriteRequestIn {
        timed,
        suppress_response,
        writes,
    })
}

/// WriteResponseMessage (spec §8.9.2.4): `{0: [AttributeStatusIB, ...], 255:
/// revision}`. Only the first `AttributeStatusIB`'s status is interpreted
/// (M8a scope: one attribute per write). Reuses `decode_attribute_status_ib`
/// (same `{0: Path, 1: StatusIB{0: status, ...}}` shape as a WriteResponses
/// entry).
pub fn decode_write_response(payload: &[u8]) -> Result<u8, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated write response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ArrayStart) => {
                // WriteResponses
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated write responses"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            status = Some(decode_attribute_status_ib(&mut r)?);
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed("unexpected element in write responses"))
                        }
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    status.ok_or(ImError::Malformed(
        "write response without AttributeStatusIB",
    ))
}

/// WriteResponseMessage (spec §8.9.2.4): encodes one `AttributeStatusIB` per
/// `results` entry `(endpoint, cluster, attribute, status)`. Device-side
/// counterpart of `decode_write_response` — produces exactly the shape that
/// function reads: `{0: [AttributeStatusIB{0: Path(list), 1: StatusIB{0:
/// status}}, ...], 255: revision}`.
pub fn encode_write_response(results: &[(u16, u32, u32, u8)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(0)); // WriteResponses
    for &(endpoint, cluster, attribute, status) in results {
        w.start_struct(Tag::Anonymous); // AttributeStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.put_uint(Tag::Context(2), u64::from(endpoint));
        w.put_uint(Tag::Context(3), u64::from(cluster));
        w.put_uint(Tag::Context(4), u64::from(attribute));
        w.end_container(); // Path
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), u64::from(status));
        w.end_container(); // StatusIB
        w.end_container(); // AttributeStatusIB
    }
    w.end_container(); // WriteResponses
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn invoke_request_and_response_roundtrip_shapes() {
        let buf = encode_invoke_request(1, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None);
        let mut r = Reader::new(&buf);
        let mut els = Vec::new();
        while let Some(e) = r.next().unwrap() {
            els.push(e);
        }
        assert_eq!(
            (els[1].tag, els[1].value),
            (Tag::Context(0), Value::Bool(false))
        );
        assert_eq!(
            (els[2].tag, els[2].value),
            (Tag::Context(1), Value::Bool(false))
        );
        assert_eq!(
            (els[3].tag, els[3].value),
            (Tag::Context(2), Value::ArrayStart)
        );
        // CommandDataIB struct → path list {0:1, 1:6, 2:2}
        assert_eq!(els[4].value, Value::StructStart);
        assert_eq!(
            (els[5].tag, els[5].value),
            (Tag::Context(0), Value::ListStart)
        );
        assert_eq!(els[6].value, Value::Uint(1));
        assert_eq!(els[7].value, Value::Uint(6));
        assert_eq!(els[8].value, Value::Uint(2));

        // InvokeResponse: Status(成功)
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let out = decode_invoke_response(&w.finish()).unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.cluster_status, None);
    }

    #[test]
    fn decodes_invoke_response_nonzero_status_with_cluster_status() {
        // CommandStatusIB carrying StatusIB{0: 0x81 UNSUPPORTED_COMMAND, 1: 0x42}.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0x81);
        w.put_uint(Tag::Context(1), 0x42);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let out = decode_invoke_response(&w.finish()).unwrap();
        assert_eq!(out.status, 0x81);
        assert_eq!(out.cluster_status, Some(0x42));
    }

    #[test]
    fn encode_invoke_request_splices_fields_tlv() {
        // A one-field CommandFields struct: { 0: 128 }.
        let mut fw = Writer::new();
        fw.start_struct(Tag::Anonymous);
        fw.put_uint(Tag::Context(0), 128);
        fw.end_container();
        let fields = fw.finish();

        let buf = encode_invoke_request(1, CLUSTER_ON_OFF, CMD_ON_OFF_ON, Some(&fields));
        let mut r = Reader::new(&buf);
        let mut els = Vec::new();
        while let Some(e) = r.next().unwrap() {
            els.push(e);
        }
        // struct{ 0: false, 1: false, 2: array[ struct{ 0: list{1,6,1}, <fields> } ], 255: 12 }
        assert_eq!(els[4].value, Value::StructStart); // CommandDataIB
        assert_eq!(els[5].value, Value::ListStart); // CommandPath
        assert_eq!(els[9].value, Value::ContainerEnd); // end of CommandPath list
                                                       // The spliced fields struct, retagged to Context(1) inside CommandDataIB.
        assert_eq!(
            (els[10].tag, els[10].value),
            (Tag::Context(1), Value::StructStart)
        );
        assert_eq!(
            (els[11].tag, els[11].value),
            (Tag::Context(0), Value::Uint(128))
        );
        assert_eq!(els[12].value, Value::ContainerEnd); // end of fields struct
        assert_eq!(els[13].value, Value::ContainerEnd); // end of CommandDataIB
    }

    #[test]
    fn status_response_roundtrip() {
        assert_eq!(
            decode_status_response(&encode_status_response(0)).unwrap(),
            0
        );
        assert_eq!(
            decode_status_response(&encode_status_response(0x7E)).unwrap(),
            0x7E
        );
    }

    #[test]
    fn move_to_hue_and_saturation_fields_shape() {
        let fields = encode_move_to_hue_and_saturation_fields(200, 254, 10);
        let mut r = Reader::new(&fields);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let expect = [
            (0u8, 200u64), // hue
            (1, 254),      // saturation
            (2, 10),       // transition time (0.1s 単位)
            (3, 0),        // options mask
            (4, 0),        // options override
        ];
        for (tag, val) in expect {
            let el = r.next().unwrap().unwrap();
            assert_eq!((el.tag, el.value), (Tag::Context(tag), Value::Uint(val)));
        }
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        assert!(r.next().unwrap().is_none());
    }

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
    fn move_to_color_temperature_fields_match_wire_shape() {
        // CommandFields (colorcontrol MoveToColorTemperature, cluster §3.2.11.10):
        // {0: ColorTemperatureMireds(u16), 1: TransitionTime(u16 0.1s),
        //  2: OptionsMask(u8)=0, 3: OptionsOverride(u8)=0}.
        // MoveToHueAndSaturation エンコーダと同じ手筋（anonymous struct + context tags）。
        let bytes = encode_move_to_color_temperature_fields(370, 30);
        // anonymous struct open (0x15) ... context-tagged uints ... close (0x18)
        assert_eq!(bytes.first(), Some(&0x15), "opens anonymous struct");
        assert_eq!(bytes.last(), Some(&0x18), "closes container");
        // mireds=370=0x0172 が context tag 0 の u16 として載る（0x25 = ctx-tag u16）
        assert!(
            bytes.windows(4).any(|w| w == [0x25, 0x00, 0x72, 0x01]),
            "mireds 370 as ctx-tag-0 u16 little-endian, got {bytes:02X?}"
        );
        // transition=30=0x1E が context tag 1 の u8 として載る（0x24 = ctx-tag u8）
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x01, 0x1E]),
            "transition 30 as ctx-tag-1 u8, got {bytes:02X?}"
        );
    }

    #[test]
    fn move_to_level_fields_match_wire_shape() {
        // CommandFields (levelcontrol MoveToLevel, cluster spec §1.6.7.1):
        // {0: Level(u8), 1: TransitionTime(u16 0.1s),
        //  2: OptionsMask(u8)=0, 3: OptionsOverride(u8)=0}.
        // MoveToColorTemperature エンコーダと同じ手筋。
        let bytes = encode_move_to_level_fields(127, 30);
        assert_eq!(bytes.first(), Some(&0x15), "opens anonymous struct");
        assert_eq!(bytes.last(), Some(&0x18), "closes container");
        // level=127=0x7F が context tag 0 の u8 として載る（0x24 = ctx-tag u8）
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x00, 0x7F]),
            "level 127 as ctx-tag-0 u8, got {bytes:02X?}"
        );
        // transition=30=0x1E が context tag 1 の u8 として載る
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x01, 0x1E]),
            "transition 30 as ctx-tag-1 u8, got {bytes:02X?}"
        );
    }

    #[test]
    fn group_invoke_request_suppresses_response_and_omits_endpoint() {
        let got = encode_group_invoke_request(CLUSTER_ON_OFF, CMD_ON_OFF_ON, None);
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), true); // SuppressResponse: group は応答なし
        w.put_bool(Tag::Context(1), false); // TimedRequest
        w.start_array(Tag::Context(2));
        w.start_struct(Tag::Anonymous);
        w.start_list(Tag::Context(0)); // CommandPath: group-scoped、endpoint なし
        w.put_uint(Tag::Context(1), u64::from(CLUSTER_ON_OFF));
        w.put_uint(Tag::Context(2), u64::from(CMD_ON_OFF_ON));
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
        w.end_container();
        assert_eq!(got, w.finish());
    }

    #[test]
    fn timed_request_shape() {
        let p = encode_timed_request(10_000);
        let mut r = Reader::new(&p);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(0));
        assert!(matches!(e.value, Value::Uint(10_000)));
    }

    #[test]
    fn invoke_request_timed_sets_flag() {
        let p = encode_invoke_request_timed(0, 0x3E, 0x00, None);
        let mut r = Reader::new(&p);
        r.next().unwrap(); // struct
        r.next().unwrap(); // SuppressResponse
        let e = r.next().unwrap().unwrap(); // TimedRequest
        assert!(matches!(e.value, Value::Bool(true)));
    }

    #[test]
    fn decode_invoke_response_with_command_fields() {
        // InvokeResponseMessage { 1: [ { 0: CommandDataIB { 0: path, 1: fields } } ] }
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous); // InvokeResponseIB
        w.start_struct(Tag::Context(0)); // CommandDataIB
        w.start_list(Tag::Context(0)); // CommandPathIB
        w.put_uint(Tag::Context(0), 0);
        w.put_uint(Tag::Context(1), 0x3E);
        w.put_uint(Tag::Context(2), 0x01);
        w.end_container();
        w.start_struct(Tag::Context(1)); // CommandFields
        w.put_bytes(Tag::Context(0), b"elements");
        w.put_bytes(Tag::Context(1), &[0xAB; 64]);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let d = decode_invoke_response_data(&w.finish()).unwrap();
        assert_eq!(d.status, 0);
        let fields = d.fields_tlv.unwrap();
        let mut fr = Reader::new(&fields);
        assert!(matches!(
            fr.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        assert!(matches!(fr.next().unwrap().unwrap().value, Value::Bytes(b) if b == b"elements"));
    }

    #[test]
    fn decode_invoke_response_data_status_form() {
        // 既存 decode_invoke_response の「nonzero status + cluster status」
        // ケース (decodes_invoke_response_nonzero_status_with_cluster_status)
        // と同じ CommandStatusIB 形（InvokeResponseIB{1: CommandStatusIB}）で
        // 合成し、status/cluster_status が透過し fields_tlv は None になる
        // ことを確認する。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0x81);
        w.put_uint(Tag::Context(1), 0x42);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let d = decode_invoke_response_data(&w.finish()).unwrap();
        assert_eq!(d.status, 0x81);
        assert_eq!(d.cluster_status, Some(0x42));
        assert_eq!(d.fields_tlv, None);
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
    fn write_request_roundtrip_scalar() {
        let b = encode_write_request(1, 0x0008, 0x0011, &ImValue::Uint(128));
        // 形の検証: WriteRequests(2) 配列の中に AttributeDataIB があり、
        // path(ep=1, cluster=8, attr=0x11) と Data(Context2)=128 を含む。
        let mut r = Reader::new(&b);
        let (mut saw_ep, mut saw_data) = (false, false);
        while let Some(el) = r.next().unwrap() {
            if el.tag == Tag::Context(2) && el.value == Value::Uint(128) {
                saw_data = true;
            }
            if el.tag == Tag::Context(2) && el.value == Value::Uint(1) {
                saw_ep = true;
            }
        }
        assert!(saw_ep && saw_data);
    }

    #[test]
    fn decode_write_response_returns_first_status() {
        // WriteResponse { 0: [ AttrStatusIB{0: path, 1: StatusIB{0: 0}} ] }
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(0));
        w.start_struct(Tag::Anonymous);
        w.start_list(Tag::Context(0)); // path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0);
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        assert_eq!(decode_write_response(&w.finish()).unwrap(), 0);
    }

    #[test]
    fn write_request_roundtrips_with_device_side_decoder() {
        let mut w = Writer::new();
        w.put_uint(Tag::Anonymous, 42);
        let data = w.finish();
        let payload = encode_write_request_tlv(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, &data);
        let req = decode_write_request(&payload).unwrap();
        assert!(!req.timed);
        assert_eq!(req.writes.len(), 1);
        let wr = &req.writes[0];
        assert_eq!(
            (wr.endpoint, wr.cluster, wr.attribute),
            (Some(0), Some(CLUSTER_ACCESS_CONTROL), Some(ATTR_ACL))
        );
        assert!(!wr.list_append);
        let mut r = Reader::new(&wr.data_tlv);
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Anonymous);
        assert_eq!(el.value, Value::Uint(42));
    }

    #[test]
    fn write_response_encodes_attribute_status_ibs() {
        let payload = encode_write_response(&[(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, 0x00)]);
        // 自前 decoder（decode_attribute_status_ib 経由の decode_write_response）で読み戻す
        assert_eq!(decode_write_response(&payload).unwrap(), 0x00);
    }

    // M8a Task9: group provision デバイス側専用エンコーダ。

    #[test]
    fn key_set_write_fields_shape() {
        let f = encode_key_set_write_fields(60, &[0xAB; 16]);
        let mut r = Reader::new(&f);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        // field 0 = GroupKeySetStruct: {0: 60, 1: 0, 2: "abab..", 3: 1, 4..7: null}
        assert_eq!(j["0"]["0"], serde_json::json!(60));
        assert_eq!(j["0"]["1"], serde_json::json!(0));
        assert_eq!(j["0"]["2"], serde_json::json!("ab".repeat(16)));
        assert_eq!(j["0"]["3"], serde_json::json!(1));
        assert!(j["0"]["4"].is_null() && j["0"]["7"].is_null());
    }

    #[test]
    fn group_key_map_tlv_is_list_of_structs() {
        let t = encode_group_key_map_tlv(&[(10, 60), (11, 61)]);
        let mut r = Reader::new(&t);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        assert_eq!(
            j,
            serde_json::json!([{"1": 10, "2": 60}, {"1": 11, "2": 61}])
        );
    }

    #[test]
    fn add_group_fields_shape() {
        let f = encode_add_group_fields(10, "grp10");
        let mut r = Reader::new(&f);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        assert_eq!(j["0"], serde_json::json!(10));
        assert_eq!(j["1"], serde_json::json!("grp10"));
    }

    // Task 7: server-direction codecs, checked against the pre-existing
    // client-direction halves (not just self-inverse).

    #[test]
    fn invoke_request_roundtrip() {
        let payload = encode_invoke_request(1, 0x0006, 1, None);
        let req = decode_invoke_request(&payload).unwrap();
        assert_eq!((req.endpoint, req.cluster, req.command), (1, 0x0006, 1));
        assert!(req.fields_tlv.is_empty());
        assert!(!req.suppress_response);
        assert!(!req.timed);
    }

    #[test]
    fn invoke_request_roundtrip_with_fields() {
        let mut fw = Writer::new();
        fw.start_struct(Tag::Anonymous);
        fw.put_uint(Tag::Context(0), 42);
        fw.end_container();
        let fields = fw.finish();
        let payload =
            encode_invoke_request(1, CLUSTER_LEVEL_CONTROL, CMD_MOVE_TO_LEVEL, Some(&fields));
        let req = decode_invoke_request(&payload).unwrap();
        assert_eq!(
            (req.endpoint, req.cluster, req.command),
            (1, CLUSTER_LEVEL_CONTROL, CMD_MOVE_TO_LEVEL)
        );
        let mut r = Reader::new(&req.fields_tlv);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        assert_eq!(j["0"], serde_json::json!(42));
    }

    #[test]
    fn invoke_response_status_decodes_with_client_decoder() {
        let payload = encode_invoke_response_status(1, 0x0006, 1, 0, None);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, 0);
    }

    /// `SuppressResponse`（タグ 0, bool）は InvokeResponseMessage の
    /// **mandatory** フィールド（spec §8.9.4）。自前の decoder は未知タグを
    /// 読み飛ばすので欠けていても往復テストは通るが、chip の
    /// `CommandSender::ProcessInvokeResponse` は `GetSuppressResponse` で
    /// タグを引きに行き、無ければ `CHIP Error 0x00000021: End of TLV` を
    /// 返して invoke ごと失敗にする（M2 ゲート 1 の実測 —
    /// `docs/superpowers/plans/m2-chip-tool-probe.md`）。ワイヤ形状を直接
    /// 検査する。
    #[test]
    fn invoke_responses_always_carry_suppress_response() {
        for payload in [
            encode_invoke_response_status(1, 0x0006, 1, 0, None),
            encode_invoke_response_data(1, CLUSTER_ON_OFF, 0x00, &[]),
        ] {
            let mut r = Reader::new(&payload);
            expect_struct_start(&mut r).unwrap();
            let first = r.next().unwrap().unwrap();
            assert_eq!(
                (first.tag, first.value),
                (Tag::Context(0), Value::Bool(false)),
                "InvokeResponseMessage must open with SuppressResponse=false: {payload:02X?}"
            );
        }
    }

    #[test]
    fn invoke_response_status_carries_cluster_status() {
        let payload =
            encode_invoke_response_status(1, 0x0006, 1, STATUS_UNSUPPORTED_COMMAND, Some(0x42));
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, STATUS_UNSUPPORTED_COMMAND);
        assert_eq!(out.cluster_status, Some(0x42));
        let data = decode_invoke_response_data(&payload).unwrap();
        assert_eq!(data.status, STATUS_UNSUPPORTED_COMMAND);
        assert_eq!(data.cluster_status, Some(0x42));
        assert!(data.fields_tlv.is_none());
    }

    #[test]
    fn invoke_response_data_decodes_with_client_decoder() {
        let mut fw = Writer::new();
        fw.start_struct(Tag::Anonymous);
        fw.put_bool(Tag::Context(0), true);
        fw.end_container();
        let fields = fw.finish();
        let payload = encode_invoke_response_data(1, CLUSTER_ON_OFF, 0x00, &fields);
        let data = decode_invoke_response_data(&payload).unwrap();
        assert_eq!(data.status, 0);
        let fields_tlv = data.fields_tlv.expect("expected CommandFields");
        let mut r = Reader::new(&fields_tlv);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        assert_eq!(j["0"], serde_json::json!(true));
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
