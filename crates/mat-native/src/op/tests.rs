use super::*;
use crate::test_support::FakeConn;
use mat_controller::im::{self, CLUSTER_ON_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_TOGGLE};
use mat_core::error::ErrorKind;
use serde_json::json;

fn node(kind: NodeOpKind) -> NodeOp {
    NodeOp { node_id: 5, kind }
}

/// 単一ノードと groupcast のショートカットは同じワイヤ（監査 Tier 3）。
#[tokio::test]
async fn node_and_group_shortcuts_share_wire() {
    let color = mat_core::color::from_hue_sat(0, 100);
    let mut conn = FakeConn::default();
    run_node_op(
        &mut conn,
        &node(NodeOpKind::Color {
            endpoint: 1,
            color: color.clone(),
            transition: 3,
        }),
    )
    .await
    .unwrap();
    run_node_op(
        &mut conn,
        &node(NodeOpKind::color_temp(1, Some(2700), None, 3)),
    )
    .await
    .unwrap();
    run_node_op(&mut conn, &node(NodeOpKind::level(1, 50, 3)))
        .await
        .unwrap();
    let group = [
        GroupOpKind::Color {
            color,
            transition: 3,
        }
        .wire(),
        GroupOpKind::color_temp(Some(2700), None, 3).wire(),
        GroupOpKind::level(50, 3).wire(),
    ];
    for (i, (cluster, command, fields)) in group.into_iter().enumerate() {
        let (ep, c, cmd, f) = &conn.invoked_fields[i];
        assert_eq!((*ep, *c, *cmd), (1, cluster, command));
        assert_eq!(f, &fields.unwrap_or_default());
    }
}

#[test]
fn kelvin_2700_converts_to_370_mireds() {
    assert_eq!(units::resolve_color_temp(Some(2700), None), (370, 2700));
}

#[test]
fn kelvin_6500_rounds_to_154_mireds() {
    assert_eq!(units::resolve_color_temp(Some(6500), None), (154, 6500));
}

#[test]
fn mireds_direct_computes_kelvin_echo() {
    assert_eq!(units::resolve_color_temp(None, Some(370)), (370, 2703));
}

#[test]
fn resolve_level_rounds_percent_to_254_scale() {
    assert_eq!(units::resolve_level(0), 0);
    assert_eq!(units::resolve_level(1), 3);
    assert_eq!(units::resolve_level(50), 127);
    assert_eq!(units::resolve_level(100), 254);
}

#[test]
fn read_resolves_names_and_numeric_ids() {
    let k = NodeOpKind::read(1, "levelcontrol", "current-level").unwrap();
    assert!(matches!(
        k,
        NodeOpKind::Read {
            endpoint: 1,
            cluster: 0x0008,
            attribute: 0x0000,
            ..
        }
    ));
    let k = NodeOpKind::read(1, "0x0008", "0").unwrap();
    assert!(matches!(
        k,
        NodeOpKind::Read {
            cluster: 0x0008,
            attribute: 0,
            ..
        }
    ));
    let err = NodeOpKind::read(1, "nosuchcluster", "x").unwrap_err();
    assert_eq!(err.kind, ErrorKind::ParseError);
    assert!(
        err.detail.contains("numeric IDs are accepted"),
        "{}",
        err.detail
    );
}

#[test]
fn write_scalar_ok_bad_json_shape_rejected_unknown_unresolved() {
    let k = NodeOpKind::write(1, "levelcontrol", "on-level", "128", false).unwrap();
    assert!(matches!(
        k,
        NodeOpKind::Write {
            cluster: 0x0008,
            value: ArgValue::UInt(128),
            timed: false,
            ..
        }
    ));
    let err = NodeOpKind::write(1, "accesscontrol", "acl", "{}", false).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ParseError);
    assert!(
        err.detail.contains("expected a JSON array"),
        "{}",
        err.detail
    );
    let err = NodeOpKind::write(1, "nosuch", "x", "1", false).unwrap_err();
    assert!(
        err.detail.contains("numeric IDs are accepted"),
        "{}",
        err.detail
    );
}

#[test]
fn invoke_scalar_args_ok_struct_args_rejected() {
    let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
    let k = NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args, false).unwrap();
    assert!(matches!(
        k,
        NodeOpKind::Invoke {
            cluster: 0x0008,
            fields_tlv: Some(_),
            ..
        }
    ));
    let k = NodeOpKind::invoke(1, "onoff", "on", &[], false).unwrap();
    assert!(matches!(
        k,
        NodeOpKind::Invoke {
            cluster: CLUSTER_ON_OFF,
            command: CMD_ON_OFF_ON,
            fields_tlv: None,
            ..
        }
    ));
    let err = NodeOpKind::invoke(
        1,
        "groupkeymanagement",
        "key-set-write",
        &["{}".into()],
        false,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ParseError);
}

#[test]
fn timed_override_forces_true_but_never_false() {
    // 表 false + override → true。
    let k = NodeOpKind::invoke(1, "onoff", "on", &[], true).unwrap();
    assert!(matches!(k, NodeOpKind::Invoke { timed: true, .. }));
    // 数値 ID（表なし）+ override → true。
    let k = NodeOpKind::invoke(1, "6", "1", &[], true).unwrap();
    assert!(matches!(k, NodeOpKind::Invoke { timed: true, .. }));
    // 表 true は override false でも true のまま。
    let k = NodeOpKind::invoke(
        0,
        "administratorcommissioning",
        "revoke-commissioning",
        &[],
        false,
    )
    .unwrap();
    assert!(matches!(k, NodeOpKind::Invoke { timed: true, .. }));
    // write も同じ。
    let k = NodeOpKind::write(1, "levelcontrol", "on-level", "128", true).unwrap();
    assert!(matches!(k, NodeOpKind::Write { timed: true, .. }));
    let k = NodeOpKind::write(1, "levelcontrol", "on-level", "128", false).unwrap();
    assert!(matches!(k, NodeOpKind::Write { timed: false, .. }));
}

#[test]
fn group_invoke_resolves_like_node_invoke() {
    let k = GroupOpKind::invoke("onoff", "toggle", &[]).unwrap();
    assert!(matches!(
        k,
        GroupOpKind::Invoke {
            cluster: CLUSTER_ON_OFF,
            command: CMD_ON_OFF_TOGGLE,
            fields_tlv: None,
            ..
        }
    ));
    let err = GroupOpKind::invoke("onoff", "on", &["1".into()]).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ParseError);
    let err = GroupOpKind::invoke("onoff", "foo", &[]).unwrap_err();
    assert!(
        err.detail.contains("numeric IDs are accepted"),
        "{}",
        err.detail
    );
}

#[test]
fn color_temp_and_level_constructors_convert_units() {
    assert_eq!(
        NodeOpKind::color_temp(1, Some(2700), None, 0),
        NodeOpKind::ColorTemp {
            endpoint: 1,
            kelvin: 2700,
            mireds: 370,
            transition: 0
        }
    );
    assert_eq!(
        NodeOpKind::level(1, 50, 0),
        NodeOpKind::Level {
            endpoint: 1,
            percent: 50,
            level: 127,
            transition: 0
        }
    );
    assert_eq!(
        GroupOpKind::color_temp(None, Some(370), 5),
        GroupOpKind::ColorTemp {
            kelvin: 2703,
            mireds: 370,
            transition: 5
        }
    );
    assert_eq!(
        GroupOpKind::level(100, 0),
        GroupOpKind::Level {
            percent: 100,
            level: 254,
            transition: 0
        }
    );
}

#[test]
fn budget_applies_only_to_single_node_hotpath_ops() {
    assert!(NodeOpKind::On { endpoint: 1 }.budget_applies());
    assert!(NodeOpKind::Off { endpoint: 1 }.budget_applies());
    assert!(NodeOpKind::level(1, 1, 0).budget_applies());
    assert!(NodeOpKind::color_temp(1, Some(2700), None, 0).budget_applies());
    assert!(NodeOpKind::read(1, "onoff", "on-off")
        .unwrap()
        .budget_applies());
    assert!(NodeOpKind::write(1, "onoff", "on-off", "true", false)
        .unwrap()
        .budget_applies());
    assert!(NodeOpKind::invoke(1, "onoff", "on", &[], false)
        .unwrap()
        .budget_applies());
    assert!(NodeOpKind::Describe.budget_applies());
    assert!(!NodeOpKind::DiagThread { endpoint: 0 }.budget_applies());
    assert!(!NodeOpKind::OpenWindow {
        timeout: 180,
        iteration: 1000,
        discriminator: 1
    }
    .budget_applies());
}

#[test]
fn names_are_snake_case_wire_tags() {
    assert_eq!(NodeOpKind::On { endpoint: 1 }.name(), "on");
    assert_eq!(
        NodeOpKind::color_temp(1, Some(2700), None, 0).name(),
        "color_temp"
    );
    assert_eq!(
        NodeOpKind::OpenWindow {
            timeout: 1,
            iteration: 1,
            discriminator: 1
        }
        .name(),
        "open_window"
    );
    assert_eq!(GroupOpKind::level(1, 0).name(), "group_level");
}

#[tokio::test]
async fn on_off_invoke_onoff_and_build_invoke_body() {
    let mut conn = FakeConn::default();
    let body = run_node_op(&mut conn, &node(NodeOpKind::On { endpoint: 1 }))
        .await
        .unwrap();
    assert_eq!(body, mat_core::body::invoke_success(5, 1, "onoff", "on"));
    let body = run_node_op(&mut conn, &node(NodeOpKind::Off { endpoint: 1 }))
        .await
        .unwrap();
    assert_eq!(body, mat_core::body::invoke_success(5, 1, "onoff", "off"));
    assert_eq!(
        conn.calls(),
        &[
            format!(
                "invoke(1,{:#06X},{:#06X})",
                im::CLUSTER_ON_OFF,
                im::CMD_ON_OFF_ON
            ),
            format!(
                "invoke(1,{:#06X},{:#06X})",
                im::CLUSTER_ON_OFF,
                im::CMD_ON_OFF_OFF
            ),
        ]
    );
}

#[tokio::test]
async fn color_color_temp_level_send_expected_commands() {
    let mut conn = FakeConn::default();
    let color = ResolvedColor {
        hue_raw: 233,
        sat_raw: 203,
        hue: 330,
        sat: 80,
        name: None,
        rgb: None,
    };
    let body = run_node_op(
        &mut conn,
        &node(NodeOpKind::Color {
            endpoint: 1,
            color: color.clone(),
            transition: 30,
        }),
    )
    .await
    .unwrap();
    assert_eq!(body, mat_core::body::color_success(5, 1, &color, 30));
    let body = run_node_op(
        &mut conn,
        &node(NodeOpKind::color_temp(1, Some(2700), None, 0)),
    )
    .await
    .unwrap();
    assert_eq!(body, mat_core::body::color_temp_success(5, 1, 2700, 370, 0));
    let body = run_node_op(&mut conn, &node(NodeOpKind::level(1, 50, 0)))
        .await
        .unwrap();
    assert_eq!(
        body,
        mat_core::body::level_success(
            5,
            1,
            mat_core::body::LevelEcho {
                percent: 50,
                level: 127
            },
            0
        )
    );
    assert_eq!(
        conn.calls(),
        &[
            format!(
                "invoke(1,{:#06X},{:#06X})",
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION
            ),
            format!(
                "invoke(1,{:#06X},{:#06X})",
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_COLOR_TEMPERATURE
            ),
            format!(
                "invoke(1,{:#06X},{:#06X})",
                im::CLUSTER_LEVEL_CONTROL,
                im::CMD_MOVE_TO_LEVEL
            ),
        ]
    );
}

#[tokio::test]
async fn read_onoff_uses_bool_fast_path_and_generic_read_uses_json() {
    // FakeConn::read_onoff は常に true、read_json は登録値（未登録は 1）。
    let mut conn = FakeConn::scripted().with_read(1, 0x0008, 0x0000, json!(200));
    let body = run_node_op(
        &mut conn,
        &node(NodeOpKind::read(1, "onoff", "on-off").unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(
        body,
        mat_core::body::read_success(5, 1, "onoff", "on-off", json!(true))
    );
    let body = run_node_op(
        &mut conn,
        &node(NodeOpKind::read(1, "levelcontrol", "current-level").unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(body["value"], json!(200));
    assert_eq!(body["cluster"], "levelcontrol");
    assert_eq!(body["attribute"], "current-level");
}

#[tokio::test]
async fn write_encodes_scalar_tlv_and_echoes_normalized_value() {
    let mut conn = FakeConn::default();
    let op = node(NodeOpKind::write(1, "levelcontrol", "on-level", "128", false).unwrap());
    let body = run_node_op(&mut conn, &op).await.unwrap();
    assert_eq!(
        body,
        mat_core::body::write_success(5, 1, "levelcontrol", "on-level", "128")
    );
    let (ep, cluster, attr, tlv) = &conn.written_tlv()[0];
    assert_eq!((*ep, *cluster, *attr), (1, 0x0008, 0x0011));
    assert_eq!(tlv, &arg_value_to_tlv(&ArgValue::UInt(128)));
}

#[tokio::test]
async fn invoke_generic_forwards_ids_and_builds_body() {
    let mut conn = FakeConn::default();
    let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
    let op = node(NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args, false).unwrap());
    let body = run_node_op(&mut conn, &op).await.unwrap();
    assert_eq!(
        body,
        mat_core::body::invoke_success(5, 1, "levelcontrol", "move-to-level")
    );
    assert_eq!(
        conn.calls(),
        &[format!(
            "invoke(1,{:#06X},{:#06X})",
            im::CLUSTER_LEVEL_CONTROL,
            im::CMD_MOVE_TO_LEVEL
        )]
    );
}

#[tokio::test]
async fn describe_diag_thread_and_open_window_build_bodies() {
    let mut conn = FakeConn::scripted().with_cluster(
        0,
        0x0035,
        vec![(0x0007, json!([{"5": 200}, {"5": 100}]))],
    );
    let body = run_node_op(&mut conn, &node(NodeOpKind::Describe))
        .await
        .unwrap();
    assert_eq!(body["node_id"], 5);
    assert!(body["endpoints"].is_array());

    let body = run_node_op(&mut conn, &node(NodeOpKind::DiagThread { endpoint: 0 }))
        .await
        .unwrap();
    assert_eq!(body["endpoint"], 0);
    assert!(body["thread"].is_object());

    let body = run_node_op(
        &mut conn,
        &node(NodeOpKind::OpenWindow {
            timeout: 180,
            iteration: 1000,
            discriminator: 3840,
        }),
    )
    .await
    .unwrap();
    assert_eq!(body["manual_code"], "34970112332");
    assert!(body["qr_payload"].as_str().unwrap().starts_with("MT:"));
    assert!(body["expires_at"].is_string());
}

fn noc_response_tlv(status: u8, fabric_index: Option<u8>) -> Vec<u8> {
    use mat_controller::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(status));
    if let Some(idx) = fabric_index {
        w.put_uint(Tag::Context(1), u64::from(idx));
    }
    w.end_container();
    w.finish()
}

#[tokio::test]
async fn remove_fabric_reads_current_index_then_invokes_and_reports_it() {
    let mut conn = FakeConn::scripted()
        .with_read(0, 0x003E, 0x0005, serde_json::json!(2))
        .with_invoke_response(0, 0x003E, 0x0A, noc_response_tlv(0, Some(2)));
    let body = run_node_op(&mut conn, &node(NodeOpKind::RemoveFabric))
        .await
        .unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "removed": true, "fabric_index": 2 })
    );
    assert_eq!(
        conn.calls(),
        &["invoke_for_data(0,0x003E,0x000A)".to_string()]
    );
}

#[tokio::test]
async fn remove_fabric_non_zero_status_is_device_rejected() {
    let mut conn = FakeConn::scripted()
        .with_read(0, 0x003E, 0x0005, serde_json::json!(2))
        .with_invoke_response(0, 0x003E, 0x0A, noc_response_tlv(0x0B, None));
    let err = run_node_op(&mut conn, &node(NodeOpKind::RemoveFabric))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::DeviceRejected);
    assert!(err.detail.contains("0x0b"), "{}", err.detail);
}

#[test]
fn remove_fabric_name_and_budget() {
    assert_eq!(NodeOpKind::RemoveFabric.name(), "remove_fabric");
    assert!(NodeOpKind::RemoveFabric.budget_applies());
}

#[tokio::test]
async fn conn_error_propagates_unchanged() {
    let mut conn = FakeConn {
        fail_first_send: true,
        fail_kind: ErrorKind::Timeout,
        ..FakeConn::default()
    };
    let err = run_node_op(
        &mut conn,
        &node(NodeOpKind::read(1, "onoff", "on-off").unwrap()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
}

#[test]
fn group_wire_and_sent_body_for_shortcuts() {
    let ct = GroupOp {
        group_id: 10,
        endpoint: 1,
        kind: GroupOpKind::color_temp(Some(2702), None, 0),
    };
    let (cluster, command, fields) = ct.kind.wire();
    assert_eq!(
        (cluster, command),
        (im::CLUSTER_COLOR_CONTROL, im::CMD_MOVE_TO_COLOR_TEMPERATURE)
    );
    assert_eq!(
        fields.unwrap(),
        im::encode_move_to_color_temperature_fields(370, 0)
    );
    assert_eq!(
        ct.sent_body(&["eth0".into()]),
        mat_core::body::group_color_temp_sent(10, 2702, 370, 0, 1, &["eth0".to_string()])
    );

    let lv = GroupOp {
        group_id: 10,
        endpoint: 1,
        kind: GroupOpKind::level(100, 0),
    };
    let (cluster, command, fields) = lv.kind.wire();
    assert_eq!(
        (cluster, command),
        (im::CLUSTER_LEVEL_CONTROL, im::CMD_MOVE_TO_LEVEL)
    );
    assert_eq!(fields.unwrap(), im::encode_move_to_level_fields(254, 0));

    let color = ResolvedColor {
        hue_raw: 180,
        sat_raw: 200,
        hue: 254,
        sat: 78,
        name: None,
        rgb: None,
    };
    let c = GroupOp {
        group_id: 10,
        endpoint: 1,
        kind: GroupOpKind::Color {
            color: color.clone(),
            transition: 0,
        },
    };
    let (cluster, command, fields) = c.kind.wire();
    assert_eq!(
        (cluster, command),
        (
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_HUE_AND_SATURATION
        )
    );
    assert_eq!(
        fields.unwrap(),
        im::encode_move_to_hue_and_saturation_fields(180, 200, 0)
    );
    assert_eq!(
        c.sent_body(&[]),
        mat_core::body::group_color_sent(10, &color, 0, 1, &[])
    );

    let inv = GroupOp {
        group_id: 10,
        endpoint: 1,
        kind: GroupOpKind::invoke("onoff", "on", &[]).unwrap(),
    };
    assert_eq!(
        inv.kind.wire(),
        (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
    );
    assert_eq!(
        inv.sent_body(&[]),
        mat_core::body::group_invoke_sent(10, "onoff", "on", 1, &[])
    );
}

#[tokio::test]
async fn group_op_hard_errors_when_engine_group_ctx_unconfigured() {
    use crate::test_support::FakeEstablisher;
    let engine = crate::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
    let op = GroupOp {
        group_id: 10,
        endpoint: 1,
        kind: GroupOpKind::invoke("onoff", "toggle", &[]).unwrap(),
    };
    let err = run_group_op(&engine, &op)
        .await
        .expect_err("group ctx unconfigured must hard-error");
    assert_eq!(err.kind, ErrorKind::Other);
    let err = run_group_bump(&engine)
        .await
        .expect_err("bump without ctx must hard-error");
    assert_eq!(err.kind, ErrorKind::Other);
}

#[tokio::test]
async fn group_bump_advances_counter_via_engine() {
    // 旧 native_direct::tests::group_bump_advances_counter_via_engine の移植。
    use crate::group::GroupCtx;
    use crate::test_support::{write_group_fixture_ini, FakeEstablisher};
    use mat_controller::transport::UdpTransport;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("chip_tool_config.ini");
    write_group_fixture_ini(&ini);
    let counter_path = dir.path().join("native_group_counter");
    let transport = Arc::new(UdpTransport::bind().await.unwrap());
    let group_ctx = GroupCtx {
        main_ini: ini,
        counter_path: counter_path.clone(),
        fabric_index: 2,
        fabric_id: 1,
        node_id: 0x0001_0001,
        egress: vec![mat_controller::group::GroupEgress {
            iface: "lo".into(),
            transport,
            scope_id: 1,
        }],
        dest_port: 5540,
        op_iface: "lo".into(),
        thread_retry: false,
        sender: Mutex::new(None),
    };
    let engine = crate::Engine::with_parts(Box::new(FakeEstablisher::default()), Some(group_ctx));
    assert!(!counter_path.exists());
    let body = run_group_bump(&engine)
        .await
        .expect("bump must succeed when ctx is configured");
    assert!(body["group_counter"]["from"].is_number());
    assert!(body["group_counter"]["to"].is_number());
    assert!(
        counter_path.exists(),
        "counter file must be created/advanced by bump"
    );
}

#[tokio::test]
async fn read_cluster_maps_rows_to_attributes_object() {
    let mut conn = FakeConn::scripted().with_cluster(
        1,
        0x0006,
        vec![
            (0x0000, serde_json::json!(true)),
            (0x4000, serde_json::json!(false)),
        ],
    );
    let k = NodeOpKind::read_cluster(1, "onoff").unwrap();
    assert_eq!(k.name(), "read_cluster");
    let body = run_node_op(&mut conn, &node(k)).await.unwrap();
    assert_eq!(body["cluster"], "onoff");
    assert_eq!(body["attributes"]["on-off"], true);
    assert_eq!(body["attributes"]["global-scene-control"], false);
    assert!(body.get("attribute").is_none());
}

#[test]
fn read_cluster_unknown_name_is_unresolved_op() {
    let err = NodeOpKind::read_cluster(1, "nosuchcluster").unwrap_err();
    assert_eq!(err.kind, ErrorKind::ParseError);
}

#[test]
fn arg_value_conversions() {
    use mat_core::ids::ArgValue as V;
    // arg_value_to_tlv は Reader で読み戻して値一致を確認。
    let b = arg_value_to_tlv(&V::Str("x".into()));
    let mut r = mat_controller::tlv::Reader::new(&b);
    assert!(matches!(
        r.next().unwrap().unwrap().value,
        mat_controller::tlv::Value::Utf8("x")
    ));

    let b = arg_value_to_tlv(&V::F64(0.5));
    let mut r = mat_controller::tlv::Reader::new(&b);
    assert!(matches!(
        r.next().unwrap().unwrap().value,
        mat_controller::tlv::Value::F64(f) if f == 0.5
    ));

    // write 経路の float 要素型: single = 0x0A, double = 0x0B（anonymous tag → control byte のみ）。
    assert_eq!(arg_value_to_tlv(&V::F32(1.5))[0] & 0x1F, 0x0A);
    assert_eq!(arg_value_to_tlv(&V::F64(1.5))[0] & 0x1F, 0x0B);
}

#[test]
fn put_value_encodes_list_of_struct_as_tlv_array_and_roundtrips_to_read_json() {
    use mat_core::ids::{parse_value_typed, resolve_attribute};
    let ty = resolve_attribute(0x001F, "acl").unwrap().def.unwrap().ty;
    let v = parse_value_typed(
        r#"[{"privilege":5,"auth-mode":2,"subjects":[112233],"targets":null,"fabric-index":1}]"#,
        &ty,
    )
    .unwrap();
    let tlv = arg_value_to_tlv(&v);
    // 先頭要素は TLV Array（0x16、anonymous）。
    assert_eq!(tlv[0], 0x16);
    // read 側の JSON 化（番号キー）に戻ると同じ内容。
    let j = mat_controller::im::tlv_to_json(&tlv).unwrap();
    assert_eq!(
        j,
        serde_json::json!([{"1":5,"2":2,"3":[112233],"4":null,"254":1}])
    );
}

#[test]
fn generic_acl_encoding_matches_dedicated_encoder() {
    use mat_core::acl::{AclEntry, AclTarget};
    use mat_core::ids::{parse_value_typed, resolve_attribute};
    let entries = vec![
        AclEntry {
            privilege: 5,
            auth_mode: 2,
            subjects: vec![112233, 0x1122],
            targets: None,
            fabric_index: 1,
        },
        AclEntry {
            privilege: 3,
            auth_mode: 3,
            subjects: vec![0xFFFF_FFFF_FFFF_0001],
            targets: Some(vec![AclTarget {
                cluster: Some(6),
                endpoint: None,
                device_type: None,
            }]),
            fabric_index: 1,
        },
    ];
    let dedicated = crate::ops::encode_acl_entries_tlv(&entries);
    let ty = resolve_attribute(0x001F, "acl").unwrap().def.unwrap().ty;
    let generic = arg_value_to_tlv(
        &parse_value_typed(
            r#"[
              {"privilege":5,"auth-mode":2,"subjects":[112233,4386],"targets":null,"fabric-index":1},
              {"privilege":3,"auth-mode":3,"subjects":["0xFFFFFFFFFFFF0001"],
               "targets":[{"cluster":6,"endpoint":null,"device-type":null}],"fabric-index":1}
            ]"#,
            &ty,
        )
        .unwrap(),
    );
    assert_eq!(generic, dedicated);
}

#[test]
fn generic_group_key_map_encoding_matches_dedicated_encoder() {
    use mat_core::ids::{parse_value_typed, resolve_attribute};
    let dedicated = mat_controller::im::encode_group_key_map_tlv(&[(1, 2), (0x0101, 7)]);
    let ty = resolve_attribute(0x003F, "group-key-map")
        .unwrap()
        .def
        .unwrap()
        .ty;
    let generic = arg_value_to_tlv(
        &parse_value_typed(
            r#"[{"group-id":1,"group-key-set-id":2},{"1":257,"2":7}]"#,
            &ty,
        )
        .unwrap(),
    );
    assert_eq!(generic, dedicated);
}

#[test]
fn generic_key_set_write_encoding_matches_dedicated_encoder() {
    use mat_core::ids::{classify_invoke, InvokeClass};
    let key: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let dedicated = mat_controller::im::encode_key_set_write_fields(1, &key);
    let j = r#"{"group-key-set-id":1,"group-key-security-policy":0,"epoch-key0":"hex:00112233445566778899aabbccddeeff","epoch-start-time0":1,"epoch-key1":null,"epoch-start-time1":null,"epoch-key2":null,"epoch-start-time2":null}"#;
    let InvokeClass::Native { fields, .. } =
        classify_invoke("groupkeymanagement", "key-set-write", &[j.into()])
    else {
        panic!("expected Native");
    };
    assert_eq!(encode_command_fields(&fields), dedicated);
}

#[test]
fn encode_command_fields_uses_positional_context_tags() {
    use mat_core::ids::ArgValue as V;
    let tlv = encode_command_fields(&[V::UInt(128), V::UInt(0)]);
    let mut r = mat_controller::tlv::Reader::new(&tlv);
    let el = r.next().unwrap().unwrap();
    assert!(matches!(el.value, mat_controller::tlv::Value::StructStart));
    // 空引数は空 struct（要素 0 個）にエンコードされる。
    let empty = encode_command_fields(&[]);
    let mut r2 = mat_controller::tlv::Reader::new(&empty);
    assert!(r2.next().unwrap().is_some());
}
