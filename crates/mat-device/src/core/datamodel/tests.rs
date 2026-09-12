use super::*;
use crate::core::access_control::{
    AclDeviceEntry, AclStore, AUTH_MODE_CASE, AUTH_MODE_GROUP, PRIVILEGE_MANAGE, PRIVILEGE_OPERATE,
    PRIVILEGE_VIEW,
};
use crate::core::stimulus::PressKind;
use mat_controller::im::{decode_invoke_response, decode_report_data_message, EventPriority};

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
/// so each test body is just the assertions. The default `InvokeCtx`
/// means fabric 0 (PASE), which the ACL enforcement below always
/// admits — the ACL tests use [`write_bi_str_as`] to write as a
/// specific fabric/subject instead.
fn write_bi_str(node: &mut Node, attribute: u32, value: &str) -> ImOutcome {
    write_bi_str_as(node, attribute, value, &mut InvokeCtx::default())
}

/// [`write_bi_str`] with the caller's own `InvokeCtx` — the ACL tests'
/// way of writing as a given `(fabric_index, subject)`.
fn write_bi_str_as(node: &mut Node, attribute: u32, value: &str, ctx: &mut InvokeCtx) -> ImOutcome {
    let mut data = Writer::new();
    data.put_str(Tag::Anonymous, value);
    let payload =
        im::encode_write_request_tlv(0, im::CLUSTER_BASIC_INFORMATION, attribute, &data.finish());
    node.handle_im(im::OPCODE_WRITE_REQUEST, &payload, ctx, &ReadCtx::default())
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
        fn invoke(&mut self, _command: u32, _fields: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
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
    let payload = encode_read_request_path(None, Some(im::CLUSTER_ON_OFF), Some(im::ATTR_ON_OFF));
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
    let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, None, false);
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
    let non_final_shape_len = im::encode_report_data_entries(&small_batch, false, None, true).len();
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
    let chunks = node.read_chunks(&paths, &read_ctx, budget, None, false);
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
    let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, None, false);
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
    let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, Some(0xABCD), false);
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

// ── Task 4: ACL enforcement (spec §9.10) ────────────────────────────

/// [`node_with_onoff`] plus an `AclStore` holding exactly one entry:
/// fabric 1, CASE `subject`, `privilege`, no target restriction. Every
/// other `Node` in this module is built *without* `set_acl_store`, which
/// is the "enforcement off" case (`node_without_an_acl_store_allows_
/// every_subject` pins it deliberately).
fn node_with_acl(privilege: u8, subject: u64) -> Node {
    let mut node = node_with_onoff();
    let store = AclStore::new();
    store.set_entries_for_test(
        1,
        vec![AclDeviceEntry {
            privilege,
            auth_mode: AUTH_MODE_CASE,
            subjects: vec![subject],
            targets_raw: None,
            fabric_index: 1,
        }],
    );
    node.set_acl_store(store);
    node
}

/// A `ReadCtx` as a CASE session on `fabric_index` authenticated as
/// `subject` — the pair every ACL decision is made against.
fn case_read_ctx(fabric_index: u8, subject: u64) -> ReadCtx {
    ReadCtx {
        fabric_index,
        subject: Subject::node(subject),
        ..ReadCtx::default()
    }
}

/// Invokes one command as `(fabric_index, subject)` and returns the
/// InvokeResponse's status.
fn invoke_status_as(
    node: &mut Node,
    fabric_index: u8,
    subject: u64,
    endpoint: u16,
    cluster: u32,
    command: u32,
) -> u8 {
    let req = im::encode_invoke_request(endpoint, cluster, command, None);
    let mut ctx = InvokeCtx {
        fabric_index,
        subject: Subject::node(subject),
        ..InvokeCtx::default()
    };
    let out = node
        .handle_im(
            im::OPCODE_INVOKE_REQUEST,
            &req,
            &mut ctx,
            &case_read_ctx(fabric_index, subject),
        )
        .unwrap();
    decode_invoke_response(&out.payload).unwrap().status
}

/// Reads one concrete `(endpoint, cluster, attribute)` path.
fn read_concrete(
    node: &Node,
    ctx: &ReadCtx,
    endpoint: u16,
    cluster: u32,
    attribute: u32,
) -> Vec<ReportEntryOut> {
    node.read_entries(
        &[AttrPathIn {
            endpoint: Some(endpoint),
            cluster: Some(cluster),
            attribute: Some(attribute),
        }],
        ctx,
    )
}

/// The one status a denied path is expected to produce, asserted on the
/// single-entry reports the tests below issue.
fn only_status(entries: &[ReportEntryOut]) -> Option<u8> {
    match entries {
        [ReportEntryOut::Status { status, .. }] => Some(*status),
        _ => None,
    }
}

/// invoke は `ClusterHandler::invoke_privilege`（OnOff は default =
/// Operate）を `AclStore::check` に通す。fabric 0（PASE）は spec
/// §9.10.5 の implicit Administer で素通り。
#[test]
fn acl_gates_invoke_by_subject_fabric_and_privilege() {
    let mut node = node_with_acl(PRIVILEGE_OPERATE, 7);
    let toggle = |node: &mut Node, fabric_index: u8, subject: u64| {
        invoke_status_as(
            node,
            fabric_index,
            subject,
            1,
            im::CLUSTER_ON_OFF,
            im::CMD_ON_OFF_TOGGLE,
        )
    };
    // Operate を持つ subject 7 は通る。
    assert_eq!(toggle(&mut node, 1, 7), im::STATUS_SUCCESS);
    // エントリに載っていない subject 8 は拒否。
    assert_eq!(toggle(&mut node, 1, 8), im::STATUS_UNSUPPORTED_ACCESS);
    // 同じ subject でも別 fabric のセッションは拒否。
    assert_eq!(toggle(&mut node, 2, 7), im::STATUS_UNSUPPORTED_ACCESS);
    // PASE (fabric 0) は ACL を経由しない。
    assert_eq!(toggle(&mut node, 0, 0), im::STATUS_SUCCESS);
}

/// group invoke は member endpoint 全部に適用し、changed を endpoint 付きで
/// 返す。ACL は `Subject::group` × Group エントリで判定。
#[test]
fn handle_group_invoke_applies_to_every_member_endpoint_under_group_acl() {
    use crate::core::group_invoke::GroupInvokeIn;
    let mut node = node_with_onoff(); // endpoint 1 に OnOff
    let (onoff2, state2) = crate::core::onoff::OnOffHandler::new();
    node.add_endpoint(2, vec![Box::new(onoff2)]);
    let store = AclStore::new();
    store.set_entries_for_test(
        1,
        vec![AclDeviceEntry {
            privilege: PRIVILEGE_OPERATE,
            auth_mode: AUTH_MODE_GROUP,
            subjects: vec![10],
            targets_raw: None,
            fabric_index: 1,
        }],
    );
    node.set_acl_store(store);
    let toggle = [GroupInvokeIn {
        cluster: im::CLUSTER_ON_OFF,
        command: im::CMD_ON_OFF_TOGGLE,
        fields_tlv: Vec::new(),
    }];
    let mut ctx = InvokeCtx {
        fabric_index: 1,
        subject: Subject::group(10),
        ..InvokeCtx::default()
    };
    let changed = node.handle_group_invoke(&[1, 2], &toggle, &mut ctx);
    assert_eq!(
        changed,
        vec![
            (1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF),
            (2, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF)
        ]
    );
    assert!(state2.load(std::sync::atomic::Ordering::SeqCst));
    // 別 group / 別 fabric は ACL で落ちて無変化
    let mut other = InvokeCtx {
        fabric_index: 1,
        subject: Subject::group(11),
        ..InvokeCtx::default()
    };
    assert!(node
        .handle_group_invoke(&[1, 2], &toggle, &mut other)
        .is_empty());
    assert!(state2.load(std::sync::atomic::Ordering::SeqCst));
    // 存在しない endpoint / cluster は黙って skip
    let unknown = [GroupInvokeIn {
        cluster: 0x7FFF,
        command: 0,
        fields_tlv: Vec::new(),
    }];
    assert!(node
        .handle_group_invoke(&[1, 9], &unknown, &mut ctx)
        .is_empty());
}

/// Manage を要求する invoke（`IdentifyHandler`）は Operate だけの
/// subject には出せない — privilege lattice が read/write と同じく
/// invoke にも効いていることのピン。
#[test]
fn acl_invoke_privilege_override_needs_manage_for_identify() {
    let mut node = node_with_acl(PRIVILEGE_OPERATE, 7);
    let (identify, _state) = crate::core::identify::IdentifyHandler::new();
    node.add_cluster(1, Box::new(identify));
    let mut fields = Writer::new();
    fields.start_struct(Tag::Anonymous);
    fields.put_uint(Tag::Context(0), 5); // IdentifyTime
    fields.end_container();
    let fields = fields.finish();

    let invoke = |node: &mut Node, subject: u64| {
        let req =
            im::encode_invoke_request(1, im::CLUSTER_IDENTIFY, im::CMD_IDENTIFY, Some(&fields));
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            subject: Subject::node(subject),
            ..InvokeCtx::default()
        };
        let out = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &case_read_ctx(1, subject),
            )
            .unwrap();
        decode_invoke_response(&out.payload).unwrap().status
    };
    assert_eq!(invoke(&mut node, 7), im::STATUS_UNSUPPORTED_ACCESS);

    // 同じコマンドを Manage の subject で。
    let mut node = node_with_acl(PRIVILEGE_MANAGE, 7);
    let (identify, _state) = crate::core::identify::IdentifyHandler::new();
    node.add_cluster(1, Box::new(identify));
    assert_eq!(invoke(&mut node, 7), im::STATUS_SUCCESS);
}

/// 不許可の read は経路で応答が変わる（設計メモ）: **具体パス**は
/// `UNSUPPORTED_ACCESS` の status entry、**wildcard 由来のパス**は
/// 何も出さずに黙って落ちる（既存の UNSUPPORTED_ATTRIBUTE の
/// wildcard 扱いと同じ流儀 — wildcard read が権限の無いノードで
/// エラーだらけにならないため）。
#[test]
fn acl_denied_read_is_a_status_on_a_concrete_path_but_silent_under_a_wildcard() {
    let node = node_with_acl(PRIVILEGE_OPERATE, 7);
    let denied = case_read_ctx(1, 8);
    let wildcard = [AttrPathIn {
        endpoint: None,
        cluster: None,
        attribute: None,
    }];

    // (a) 具体パス: status entry になる。
    let entries = read_concrete(&node, &denied, 1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF);
    assert_eq!(only_status(&entries), Some(im::STATUS_UNSUPPORTED_ACCESS));

    // (b) wildcard: 1 件も出ない（status entry も出ない）。
    let entries = node.read_entries(&wildcard, &denied);
    assert!(
        entries.is_empty(),
        "wildcard read for a subject with no ACL entry must drop silently, got {entries:?}"
    );

    // (c) 許可 subject では同じ wildcard が実データを返す — (b) が
    //     「wildcard 展開が壊れている」ではなく「拒否で落ちている」
    //     ことの対照。
    let entries = node.read_entries(&wildcard, &case_read_ctx(1, 7));
    assert!(!entries.is_empty());
    assert!(entries.iter().all(|e| matches!(e, ReportEntryOut::Data(_))));
}

/// spec §8.10 / chip `ParseAttributePaths`: 購読の受理判定。wildcard
/// パスは ACL 込みで 1 つでも読める属性に展開されれば有効、具体パスは
/// （存在・権限に関わらず）常に有効 — その拒否は priming の status
/// entry として伝わる。paths 空は無効。
#[test]
fn has_readable_path_follows_the_subscription_validity_rule() {
    let node = node_with_acl(PRIVILEGE_OPERATE, 7);
    let allowed = case_read_ctx(1, 7);
    let denied = case_read_ctx(1, 8);
    let full_wildcard = AttrPathIn {
        endpoint: None,
        cluster: None,
        attribute: None,
    };
    let onoff_wildcard = AttrPathIn {
        endpoint: None,
        cluster: Some(im::CLUSTER_ON_OFF),
        attribute: None,
    };
    let concrete = AttrPathIn {
        endpoint: Some(1),
        cluster: Some(im::CLUSTER_ON_OFF),
        attribute: Some(im::ATTR_ON_OFF),
    };
    let missing_cluster_wildcard = AttrPathIn {
        endpoint: None,
        cluster: Some(0x7FFF),
        attribute: None,
    };
    // wildcard endpoint + 具体 cluster/attribute: read_allowed だけでは
    // 存在しない attribute id も通ってしまう罠を塞ぐケア。
    let wildcard_endpoint_concrete_attr = AttrPathIn {
        endpoint: None,
        cluster: Some(im::CLUSTER_ON_OFF),
        attribute: Some(im::ATTR_ON_OFF),
    };
    let wildcard_endpoint_missing_attr = AttrPathIn {
        endpoint: None,
        cluster: Some(im::CLUSTER_ON_OFF),
        attribute: Some(0x7FFF),
    };

    assert!(node.has_readable_path(&[full_wildcard], &allowed));
    assert!(node.has_readable_path(&[onoff_wildcard], &allowed));
    assert!(!node.has_readable_path(&[full_wildcard], &denied));
    assert!(!node.has_readable_path(&[onoff_wildcard], &denied));
    assert!(!node.has_readable_path(&[missing_cluster_wildcard], &allowed));
    // 具体パスは拒否 subject でも「有効」（status entry で答える経路）。
    assert!(node.has_readable_path(&[concrete], &denied));
    // 1 つでも有効なら全体は有効。
    assert!(node.has_readable_path(&[full_wildcard, concrete], &denied));
    assert!(!node.has_readable_path(&[], &allowed));
    // wildcard endpoint + 具体 attribute: ACL は同じく効く。
    assert!(node.has_readable_path(&[wildcard_endpoint_concrete_attr], &allowed));
    assert!(!node.has_readable_path(&[wildcard_endpoint_concrete_attr], &denied));
    // 許可 subject でも、存在しない attribute id は値が解決できず無効。
    assert!(!node.has_readable_path(&[wildcard_endpoint_missing_attr], &allowed));
}

/// Groups の **mutating** コマンド（AddGroup / RemoveGroup /
/// RemoveAllGroups / AddGroupIfIdentifying）は spec §1.3.5 で Manage —
/// Operate だけの fabric メンバーにグループ台帳を書き換え／全消しさせ
/// ない。読み取り系（ViewGroup / GetGroupMembership）は Operate のまま
/// なので、同じ subject が ViewGroup は通せることも併せて固定する。
#[test]
fn acl_groups_mutating_commands_need_manage_but_reads_stay_operate() {
    let add_group_fields = |group_id: u16| {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(group_id));
        w.put_str(Tag::Context(1), "");
        w.end_container();
        w.finish()
    };
    // `(IM status, クラスタ応答の Status フィールド)`。拒否は
    // CommandStatusIB（fields なし）なので後者は `None` になり、
    // 許可された AddGroup/ViewGroup は Data 応答（IM status 0）の中に
    // クラスタ側の Status を持つ — 「通った」と「実際に台帳が動いた」
    // を取り違えないための 2 値。
    let invoke = |node: &mut Node, command: u32, subject: u64| -> (u8, Option<u8>) {
        let req =
            im::encode_invoke_request(1, im::CLUSTER_GROUPS, command, Some(&add_group_fields(5)));
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            subject: Subject::node(subject),
            ..InvokeCtx::default()
        };
        let out = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &case_read_ctx(1, subject),
            )
            .unwrap();
        let resp = im::decode_invoke_response_data(&out.payload).unwrap();
        let cluster_status = resp.fields_tlv.as_deref().and_then(|fields| {
            // `{0: Status, 1: GroupID}` の Status だけ拾う。
            let mut r = Reader::new(fields);
            r.next().ok()??;
            loop {
                match r.next().ok()?? {
                    el if el.value == Value::ContainerEnd => return None,
                    el => {
                        if let (Tag::Context(0), Value::Uint(v)) = (el.tag, el.value) {
                            return u8::try_from(v).ok();
                        }
                    }
                }
            }
        });
        (resp.status, cluster_status)
    };
    let with_groups = |privilege: u8| {
        let mut node = node_with_acl(privilege, 7);
        let (identify, state) = crate::core::identify::IdentifyHandler::new();
        node.add_cluster(1, Box::new(identify));
        node.add_cluster(
            1,
            Box::new(crate::core::groups::GroupsHandler::new(
                state,
                crate::core::group_membership::GroupMembershipStore::new(),
                1,
            )),
        );
        node
    };

    // Operate だけの subject: 4 つの mutating コマンドは全て拒否。
    let mut node = with_groups(PRIVILEGE_OPERATE);
    for command in [
        im::CMD_ADD_GROUP,
        im::CMD_REMOVE_GROUP,
        im::CMD_REMOVE_ALL_GROUPS,
        im::CMD_ADD_GROUP_IF_IDENTIFYING,
    ] {
        assert_eq!(
            invoke(&mut node, command, 7),
            (im::STATUS_UNSUPPORTED_ACCESS, None),
            "command 0x{command:02X} must need Manage"
        );
    }
    // 同じ Operate の subject でも読み取り系は通る — そして拒否された
    // AddGroup が台帳に何も足していないことがここで分かる
    // （ViewGroup のクラスタ Status が NOT_FOUND）。
    assert_eq!(
        invoke(&mut node, im::CMD_VIEW_GROUP, 7),
        (im::STATUS_SUCCESS, Some(im::STATUS_NOT_FOUND)),
        "ViewGroup must stay at Operate, and group 5 must not exist"
    );

    // Manage を持つ subject なら AddGroup が通り、実際に台帳に載る。
    let mut node = with_groups(PRIVILEGE_MANAGE);
    assert_eq!(
        invoke(&mut node, im::CMD_ADD_GROUP, 7),
        (im::STATUS_SUCCESS, Some(im::STATUS_SUCCESS))
    );
    assert_eq!(
        invoke(&mut node, im::CMD_VIEW_GROUP, 7),
        (im::STATUS_SUCCESS, Some(im::STATUS_SUCCESS))
    );
}

/// read の必要 privilege は属性ごと（`AccessControlHandler` の
/// override）: `ATTR_ACL` は Administer、容量属性と global 属性
/// (spec §7.13) は View。View だけの subject では wildcard 展開から
/// `ATTR_ACL` **だけ**が落ちる。
#[test]
fn acl_read_privilege_is_per_attribute_and_global_attributes_stay_view() {
    let store = AclStore::new();
    store.set_entries_for_test(
        1,
        vec![AclDeviceEntry {
            privilege: PRIVILEGE_VIEW,
            auth_mode: AUTH_MODE_CASE,
            subjects: vec![7],
            targets_raw: None,
            fabric_index: 1,
        }],
    );
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    node.add_cluster(
        0,
        Box::new(crate::core::access_control::AccessControlHandler::new(
            store.clone(),
        )),
    );
    node.set_acl_store(store);
    let ctx = case_read_ctx(1, 7);

    // ATTR_ACL は Administer 要求 — View だけでは読めない。
    let entries = read_concrete(&node, &ctx, 0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL);
    assert_eq!(only_status(&entries), Some(im::STATUS_UNSUPPORTED_ACCESS));

    // 容量属性は View のまま。
    let entries = read_concrete(
        &node,
        &ctx,
        0,
        im::CLUSTER_ACCESS_CONTROL,
        im::ATTR_ACL_SUBJECTS_PER_ENTRY,
    );
    assert!(matches!(&entries[..], [ReportEntryOut::Data(_)]));

    // global 属性 (ClusterRevision) も View 固定 — クラスタの
    // `read_privilege` override に引きずられない。
    let entries = read_concrete(
        &node,
        &ctx,
        0,
        im::CLUSTER_ACCESS_CONTROL,
        im::ATTR_CLUSTER_REVISION,
    );
    assert!(matches!(&entries[..], [ReportEntryOut::Data(_)]));

    // wildcard 属性展開では ATTR_ACL だけが黙って落ちる。
    let entries = node.read_entries(
        &[AttrPathIn {
            endpoint: Some(0),
            cluster: Some(im::CLUSTER_ACCESS_CONTROL),
            attribute: None,
        }],
        &ctx,
    );
    let reported: Vec<u32> = entries
        .iter()
        .map(|e| match e {
            ReportEntryOut::Data(r) => r.attribute,
            other => panic!("expected data entries only, got {other:?}"),
        })
        .collect();
    assert_eq!(
        reported,
        vec![
            im::ATTR_ACL_SUBJECTS_PER_ENTRY,
            im::ATTR_ACL_TARGETS_PER_ENTRY,
            im::ATTR_ACL_ENTRIES_PER_FABRIC,
        ]
    );
}

/// write は per-entry の `AttributeStatusIB` で拒否する（既存の
/// UNSUPPORTED_WRITE と同じ経路）。BasicInformation の NodeLabel は
/// Manage 要求（`write_privilege` override）なので Operate だけの
/// subject では書けず、値も変わらない。
#[test]
fn acl_gates_write_with_a_per_entry_unsupported_access_status() {
    let mut node = node_with_acl(PRIVILEGE_OPERATE, 7);
    let mut ctx = InvokeCtx {
        fabric_index: 1,
        subject: Subject::node(7),
        ..InvokeCtx::default()
    };
    let outcome = write_bi_str_as(&mut node, im::ATTR_BI_NODE_LABEL, "Living Room", &mut ctx);
    assert_eq!(
        im::decode_write_response(&outcome.payload).unwrap(),
        im::STATUS_UNSUPPORTED_ACCESS
    );
    assert!(outcome.changed.is_empty());
    let entries = read_concrete(
        &node,
        &case_read_ctx(1, 7),
        0,
        im::CLUSTER_BASIC_INFORMATION,
        im::ATTR_BI_NODE_LABEL,
    );
    match &entries[..] {
        [ReportEntryOut::Data(r)] => {
            assert_eq!(
                r.value_tlv,
                tlv_value::str(""),
                "NodeLabel must be untouched"
            )
        }
        other => panic!("expected a NodeLabel data report, got {other:?}"),
    }

    // Manage を持つ subject なら同じ write が通る。
    let mut node = node_with_acl(PRIVILEGE_MANAGE, 7);
    let outcome = write_bi_str_as(&mut node, im::ATTR_BI_NODE_LABEL, "Living Room", &mut ctx);
    assert_eq!(
        im::decode_write_response(&outcome.payload).unwrap(),
        im::STATUS_SUCCESS
    );
    assert_eq!(
        outcome.changed,
        vec![(0u16, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_NODE_LABEL)]
    );
}

/// `set_acl_store` を呼ばない `Node`（このモジュールの他のテスト全部と
/// `core` の各クラスタ単体テストが組む形）は enforcement 無効 = 全許可
/// — CASE セッション相当の fabric/subject でも素通りする。
#[test]
fn node_without_an_acl_store_allows_every_subject() {
    let mut node = node_with_onoff();
    assert_eq!(
        invoke_status_as(
            &mut node,
            1,
            999,
            1,
            im::CLUSTER_ON_OFF,
            im::CMD_ON_OFF_TOGGLE
        ),
        im::STATUS_SUCCESS
    );
    let entries = read_concrete(
        &node,
        &case_read_ctx(1, 999),
        1,
        im::CLUSTER_ON_OFF,
        im::ATTR_ON_OFF,
    );
    assert!(matches!(&entries[..], [ReportEntryOut::Data(_)]));
}

/// stimulate を実装するテスト用クラスタ: SetState で属性 0 を変え、
/// イベント 0 を出す。
struct StimHandler {
    state: bool,
}

impl ClusterHandler for StimHandler {
    fn cluster_id(&self) -> u32 {
        0xFC01
    }
    fn attributes(&self) -> Vec<u32> {
        vec![0]
    }
    fn events(&self) -> Vec<u32> {
        vec![0]
    }
    fn read(&self, a: u32, _: &ReadCtx) -> Option<Vec<u8>> {
        (a == 0).then(|| tlv_value::bool(self.state))
    }
    fn invoke(&mut self, _: u32, _: &[u8], _: &mut InvokeCtx) -> InvokeReply {
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }
    fn stimulate(&mut self, s: &Stimulus, ctx: &mut InvokeCtx) -> StimulusReply {
        match s {
            Stimulus::SetState(v) => {
                if *v != self.state {
                    self.state = *v;
                    ctx.changed.push(0);
                    ctx.events.push(EmittedEvent {
                        event: 0,
                        priority: EventPriority::Info,
                        data_tlv: None,
                    });
                }
                StimulusReply::Applied
            }
            Stimulus::Press(_) => StimulusReply::Unsupported,
        }
    }
}

fn node_with_stim() -> Node {
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    node.add_endpoint(2, vec![Box::new(StimHandler { state: false })]);
    node.set_event_log(EventLog::new(100, 8));
    node
}

#[test]
fn stimulate_appends_events_bumps_data_version_and_reports_changed() {
    let mut node = node_with_stim();
    let before = node.data_version(2, 0xFC01);
    let out = node.stimulate(2, &Stimulus::SetState(true), 777).unwrap();
    assert_eq!(out.changed, vec![(2, 0xFC01, 0)]);
    assert_eq!(out.event_numbers, vec![100]);
    assert_eq!(node.data_version(2, 0xFC01), before.wrapping_add(1));
    assert_eq!(node.next_event_number(), 101);
    let ev = &node.recent_events(0)[0];
    assert_eq!(
        (ev.endpoint, ev.cluster, ev.event, ev.system_timestamp_ms),
        (2, 0xFC01, 0, 777)
    );
    // 同値: 変化なし、イベントなし、Applied。
    let out = node.stimulate(2, &Stimulus::SetState(true), 778).unwrap();
    assert!(out.changed.is_empty() && out.event_numbers.is_empty());
}

#[test]
fn stimulate_errors_for_unknown_endpoint_and_unsupported_stimulus() {
    let mut node = node_with_stim();
    assert_eq!(
        node.stimulate(9, &Stimulus::SetState(true), 0),
        Err(StimulusError::UnknownEndpoint)
    );
    assert_eq!(
        node.stimulate(2, &Stimulus::Press(PressKind::Short), 0),
        Err(StimulusError::Unsupported)
    );
    // 刺激を受けない endpoint 0（Descriptor/BasicInformation のみ）も
    // Unsupported。
    assert_eq!(
        node.stimulate(0, &Stimulus::SetState(true), 0),
        Err(StimulusError::Unsupported)
    );
}

#[test]
fn event_entries_expand_wildcards_and_honor_event_min() {
    let mut node = node_with_stim();
    node.stimulate(2, &Stimulus::SetState(true), 1).unwrap(); // #100
    node.stimulate(2, &Stimulus::SetState(false), 2).unwrap(); // #101
    let all = node.event_entries(&[EventPathIn::WILDCARD_URGENT], 0, &ReadCtx::default());
    assert_eq!(all.len(), 2);
    let tail = node.event_entries(&[EventPathIn::WILDCARD_URGENT], 101, &ReadCtx::default());
    assert!(
        matches!(&tail[..], [EventEntryOut::Data(d)] if d.event_number == 101 && d.system_timestamp_ms == 2)
    );
    // 別クラスタの wildcard は黙る。
    let other = node.event_entries(
        &[EventPathIn {
            cluster: Some(0xFC02),
            ..EventPathIn::default()
        }],
        0,
        &ReadCtx::default(),
    );
    assert!(other.is_empty());
}

/// `event_entries` の ACL ゲート（spec §9.10）。`ReadCtx::default()` は
/// fabric 0 = ACL 短絡なので、他のイベントテストはゲートを通っていない
/// — ここは CASE の ReadCtx（fabric 1 + subject）で見る。
/// `StimHandler` は `event_privilege` を既定（View）のままにしてある
/// ので、落としているのは ACL そのもの。
#[test]
fn event_entries_hides_events_the_acl_grants_no_view_on() {
    // target を OnOff だけに絞った View エントリ: 発生クラスタ
    // 0xFC01 には効かない（`targets_match`）。
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(im::CLUSTER_ON_OFF));
    w.end_container();
    w.end_container();
    let other_cluster_only = w.finish();

    let view_entry = |targets_raw: Option<Vec<u8>>| AclDeviceEntry {
        privilege: PRIVILEGE_VIEW,
        auth_mode: AUTH_MODE_CASE,
        subjects: vec![7],
        targets_raw,
        fabric_index: 1,
    };
    let node_with = |targets_raw: Option<Vec<u8>>| {
        let mut node = node_with_stim();
        let store = AclStore::new();
        store.set_entries_for_test(1, vec![view_entry(targets_raw)]);
        node.set_acl_store(store);
        node.stimulate(2, &Stimulus::SetState(true), 1).unwrap();
        node
    };
    let all =
        |node: &Node, ctx: &ReadCtx| node.event_entries(&[EventPathIn::WILDCARD_URGENT], 0, ctx);

    let denied = node_with(Some(other_cluster_only));
    assert!(all(&denied, &case_read_ctx(1, 7)).is_empty());
    // fabric 0（PASE）は ACL を通らない既存の短絡: 同じログでも見える。
    assert_eq!(all(&denied, &ReadCtx::default()).len(), 1);
    // 別 subject も同様に落ちる（一致するエントリが無い）。
    assert!(all(&denied, &case_read_ctx(1, 8)).is_empty());

    // 制限なしの View なら出る。
    let granted = node_with(None);
    assert_eq!(all(&granted, &case_read_ctx(1, 7)).len(), 1);
}

/// 発生元 `(endpoint, cluster)` がもう解決できないログ項目は黙って
/// 落ちる — privilege を訊く相手が居ない以上 ACL を素通しさせられない
/// （`event_entries` の doc）。panic もせず Status も出さない。
/// `Node` にクラスタ削除 API は無いので、存在しない endpoint 9 の項目を
/// 持つログを `set_event_log` で直接差し込んで作る。
#[test]
fn event_entries_drops_entries_whose_emitting_cluster_is_gone() {
    let ev = || EmittedEvent {
        event: 0,
        priority: EventPriority::Info,
        data_tlv: None,
    };
    let mut log = EventLog::new(100, 8);
    log.append(9, 0xFC01, ev(), 5); // #100: endpoint 9 は存在しない
    log.append(2, 0xFC02, ev(), 6); // #101: endpoint 2 にこのクラスタは無い
    log.append(2, 0xFC01, ev(), 7); // #102: 生きている
    let mut node = node_with_stim();
    node.set_event_log(log);

    let out = node.event_entries(&[EventPathIn::WILDCARD_URGENT], 0, &ReadCtx::default());
    assert!(
        matches!(&out[..], [EventEntryOut::Data(d)] if d.event_number == 102 && d.endpoint == 2),
        "解決できない 2 件は落ち、Status も出ない: {out:?}"
    );
}

#[test]
fn concrete_unresolvable_event_paths_report_status() {
    let node = node_with_stim();
    let st = |p: EventPathIn| node.event_entries(&[p], 0, &ReadCtx::default());
    assert!(matches!(
        &st(EventPathIn {
            endpoint: Some(9),
            cluster: Some(0xFC01),
            event: Some(0),
            urgent: false
        })[..],
        [EventEntryOut::Status { status, .. }] if *status == im::STATUS_UNSUPPORTED_ENDPOINT
    ));
    assert!(matches!(
        &st(EventPathIn {
            endpoint: Some(2),
            cluster: Some(0xFC02),
            event: Some(0),
            urgent: false
        })[..],
        [EventEntryOut::Status { status, .. }] if *status == im::STATUS_UNSUPPORTED_CLUSTER
    ));
    assert!(matches!(
        &st(EventPathIn {
            endpoint: Some(2),
            cluster: Some(0xFC01),
            event: Some(5),
            urgent: false
        })[..],
        [EventEntryOut::Status { status, .. }] if *status == im::STATUS_UNSUPPORTED_EVENT
    ));
    // 解決できる具体 path でログが空なら何も返さない（status ではない）。
    assert!(st(EventPathIn {
        endpoint: Some(2),
        cluster: Some(0xFC01),
        event: Some(0),
        urgent: false
    })
    .is_empty());
}

#[test]
fn has_readable_event_path_accepts_wildcards_only_when_some_cluster_has_events() {
    let node = node_with_stim();
    assert!(node.has_readable_event_path(&[EventPathIn::WILDCARD_URGENT], &ReadCtx::default()));
    assert!(!node.has_readable_event_path(
        &[EventPathIn {
            cluster: Some(0xFC02),
            ..EventPathIn::default()
        }],
        &ReadCtx::default()
    ));
    // 具体 path は常に true（status で答える）。
    assert!(node.has_readable_event_path(
        &[EventPathIn {
            endpoint: Some(9),
            cluster: Some(1),
            event: Some(1),
            urgent: false
        }],
        &ReadCtx::default()
    ));
    assert!(!node.has_readable_event_path(&[], &ReadCtx::default()));
}

#[test]
fn read_chunks_trailer_follows_marks_the_last_chunk_more() {
    let node = node_with_stim();
    let paths = vec![AttrPathIn {
        endpoint: Some(2),
        cluster: Some(0xFC01),
        attribute: Some(0),
    }];
    let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, Some(1), true);
    assert_eq!(chunks.len(), 1);
    assert!(
        im::decode_report_data_message(&chunks[0])
            .unwrap()
            .more_chunks
    );
    let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, Some(1), false);
    assert!(
        !im::decode_report_data_message(&chunks[0])
            .unwrap()
            .more_chunks
    );
}
