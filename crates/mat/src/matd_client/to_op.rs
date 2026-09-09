//! サブコマンド（`DeviceOp`）→ matd の op JSON 変換。直経路専用 op はここで
//! 弾く（matd プロトコルに op を足さない）。

use serde_json::{json, Value};

use crate::device_op::DeviceOp;
use mat_native::op::{GroupOpKind, NodeOpKind};

use super::unsupported_detail;

/// `DeviceOp` を matd の op JSON に変換する。wire は名前のまま（`*_in`）—
/// 契約不変。直経路専用 op は `Err(detail)`。
pub(super) fn to_op(op: &DeviceOp) -> Result<Value, String> {
    Ok(match op {
        DeviceOp::Node(n) => {
            let node_id = n.node_id;
            match &n.kind {
                NodeOpKind::Read {
                    endpoint,
                    cluster_in,
                    attribute_in,
                    ..
                } => json!({
                    "op": "read", "node_id": node_id, "endpoint": endpoint,
                    "cluster": cluster_in, "attribute": attribute_in,
                }),
                NodeOpKind::ReadCluster {
                    endpoint,
                    cluster_in,
                    ..
                } => json!({
                    "op": "read", "node_id": node_id, "endpoint": endpoint,
                    "cluster": cluster_in,
                }),
                NodeOpKind::Write {
                    endpoint,
                    cluster_in,
                    attribute_in,
                    value_in,
                    timed,
                    ..
                } => {
                    let mut op = json!({
                        "op": "write", "node_id": node_id, "endpoint": endpoint,
                        "cluster": cluster_in, "attribute": attribute_in, "value": value_in,
                    });
                    if *timed {
                        op["timed"] = json!(true);
                    }
                    op
                }
                NodeOpKind::Invoke {
                    endpoint,
                    cluster_in,
                    command_in,
                    args_in,
                    timed,
                    ..
                } => {
                    let mut op = json!({
                        "op": "invoke", "node_id": node_id, "endpoint": endpoint,
                        "cluster": cluster_in, "command": command_in, "args": args_in,
                    });
                    if *timed {
                        op["timed"] = json!(true);
                    }
                    op
                }
                NodeOpKind::Describe => json!({ "op": "describe", "node_id": node_id }),
                NodeOpKind::On { endpoint } => {
                    json!({ "op": "on", "node_id": node_id, "endpoint": endpoint })
                }
                NodeOpKind::Off { endpoint } => {
                    json!({ "op": "off", "node_id": node_id, "endpoint": endpoint })
                }
                // 換算済み値を渡し、kelvin / percent / 度 / % / name / rgb は
                // 応答エコー用（matd 側で逆算すると丸めで入力とずれる）。
                NodeOpKind::ColorTemp {
                    endpoint,
                    kelvin,
                    mireds,
                    transition,
                } => json!({
                    "op": "color_temp", "node_id": node_id, "endpoint": endpoint,
                    "mireds": mireds, "kelvin": kelvin, "transition": transition,
                }),
                NodeOpKind::Level {
                    endpoint,
                    percent,
                    level,
                    transition,
                } => json!({
                    "op": "level", "node_id": node_id, "endpoint": endpoint,
                    "level": level, "percent": percent, "transition": transition,
                }),
                NodeOpKind::Color {
                    endpoint,
                    color,
                    transition,
                } => {
                    let mut op = json!({
                        "op": "color", "node_id": node_id, "endpoint": endpoint,
                        "hue_raw": color.hue_raw, "saturation_raw": color.sat_raw,
                        "hue": color.hue, "saturation": color.sat, "transition": transition,
                    });
                    if let Some(name) = &color.name {
                        op["name"] = json!(name);
                    }
                    if let Some(rgb) = &color.rgb {
                        op["rgb"] = json!(rgb);
                    }
                    op
                }
                // matd は warm CASE セッション層。これらは直経路でしか実行できない。
                NodeOpKind::DiagThread { .. } => return Err(unsupported_detail("diag")),
                NodeOpKind::OpenWindow { .. } => return Err(unsupported_detail("open-window")),
                NodeOpKind::RemoveFabric => return Err(unsupported_detail("unpair")),
            }
        }
        DeviceOp::Group(g) => {
            let (group_id, endpoint) = (g.group_id, g.endpoint);
            match &g.kind {
                GroupOpKind::Invoke {
                    cluster_in,
                    command_in,
                    args_in,
                    ..
                } => json!({
                    "op": "group_invoke", "group_id": group_id, "cluster": cluster_in,
                    "command": command_in, "args": args_in, "endpoint": endpoint,
                }),
                GroupOpKind::ColorTemp {
                    kelvin,
                    mireds,
                    transition,
                } => json!({
                    "op": "group_color_temp", "group_id": group_id,
                    "mireds": mireds, "kelvin": kelvin,
                    "transition": transition, "endpoint": endpoint,
                }),
                GroupOpKind::Level {
                    percent,
                    level,
                    transition,
                } => json!({
                    "op": "group_level", "group_id": group_id,
                    "level": level, "percent": percent,
                    "transition": transition, "endpoint": endpoint,
                }),
                GroupOpKind::Color { color, transition } => {
                    let mut op = json!({
                        "op": "group_color", "group_id": group_id,
                        "hue_raw": color.hue_raw, "saturation_raw": color.sat_raw,
                        "hue": color.hue, "saturation": color.sat,
                        "transition": transition, "endpoint": endpoint,
                    });
                    if let Some(name) = &color.name {
                        op["name"] = json!(name);
                    }
                    if let Some(rgb) = &color.rgb {
                        op["rgb"] = json!(rgb);
                    }
                    op
                }
            }
        }
        DeviceOp::GroupProvision(p) => json!({
            "op": "group_provision", "group_id": p.group_id, "node_ids": p.node_ids,
            "keyset_id": p.keyset_id, "name": p.name, "endpoint": p.endpoint,
            "epoch_key": p.epoch_key, "rebind": p.rebind,
        }),
        DeviceOp::GroupBump => json!({ "op": "group_bump" }),
        // grant は稀な修復操作で warm session の恩恵が小さく、mat/matd の
        // バージョンスキューにも安全なため直経路のみ。
        DeviceOp::GroupGrant { .. } => return Err(unsupported_detail("group grant")),
        // remove も同じ理由（稀な撤収操作 + コントローラ KVS の所有者は mat）。
        DeviceOp::GroupRemove { .. } => return Err(unsupported_detail("group remove")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_op::DeviceOp;
    use mat_native::op::{GroupOp, GroupOpKind, NodeOp, NodeOpKind, ProvisionParams};

    fn node(node_id: u64, kind: NodeOpKind) -> DeviceOp {
        DeviceOp::Node(NodeOp { node_id, kind })
    }

    fn group(group_id: u16, endpoint: u16, kind: GroupOpKind) -> DeviceOp {
        DeviceOp::Group(GroupOp {
            group_id,
            endpoint,
            kind,
        })
    }

    #[test]
    fn read_maps_to_read_op() {
        let op = node(1, NodeOpKind::read(2, "onoff", "on-off").unwrap());
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"read","node_id":1,"endpoint":2,"cluster":"onoff","attribute":"on-off"})
        );
    }

    #[test]
    fn on_maps_to_on_op_with_endpoint() {
        let op = node(3, NodeOpKind::On { endpoint: 1 });
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"on","node_id":3,"endpoint":1})
        );
    }

    #[test]
    fn color_temp_kelvin_maps_to_color_temp_op_with_converted_mireds() {
        let op = node(6, NodeOpKind::color_temp(1, Some(2700), None, 30));
        // 換算（2700K → 370 mireds）は mat 側で行い、kelvin はエコー用に併送する。
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"color_temp","node_id":6,"endpoint":1,"mireds":370,"kelvin":2700,"transition":30})
        );
    }

    #[test]
    fn color_temp_mireds_maps_with_computed_kelvin_echo() {
        let op = node(6, NodeOpKind::color_temp(1, None, Some(370), 0));
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"color_temp","node_id":6,"endpoint":1,"mireds":370,"kelvin":2703,"transition":0})
        );
    }

    #[test]
    fn color_maps_to_color_op_with_converted_values() {
        let op = node(
            6,
            NodeOpKind::Color {
                endpoint: 1,
                color: mat_core::color::resolve_spec(None, None, Some(330), Some(80)).unwrap(),
                transition: 30,
            },
        );
        // 換算（330° → 233、80% → 203）は mat 側で行い、度 / % はエコー用に併送する。
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"color","node_id":6,"endpoint":1,
                "hue_raw":233,"saturation_raw":203,
                "hue":330,"saturation":80,"transition":30
            })
        );
    }

    #[test]
    fn color_name_op_includes_name_and_rgb_echo() {
        // resolve 層通過後の形（name あり + 正規化済み rgb）。
        let op = node(
            6,
            NodeOpKind::Color {
                endpoint: 1,
                color: mat_core::color::resolve_spec(Some("red"), Some("#ff0000"), None, None)
                    .unwrap(),
                transition: 0,
            },
        );
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"color","node_id":6,"endpoint":1,
                "hue_raw":0,"saturation_raw":254,
                "hue":0,"saturation":100,"transition":0,
                "name":"red","rgb":"#ff0000"
            })
        );
    }

    #[test]
    fn group_provision_fills_default_name_and_keeps_null_epoch() {
        // name 補完（grp<group_id>）は Task 8 の classify のテストで担保。
        let op = DeviceOp::GroupProvision(ProvisionParams {
            group_id: 7,
            node_ids: vec![1, 2],
            keyset_id: 42,
            name: "grp7".into(),
            endpoint: 1,
            epoch_key: None,
            rebind: false,
        });
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"group_provision","group_id":7,"node_ids":[1,2],
                "keyset_id":42,"name":"grp7","endpoint":1,"epoch_key":null,
                "rebind":false
            })
        );
    }

    #[test]
    fn group_bump_maps_to_group_bump_op() {
        assert_eq!(
            to_op(&DeviceOp::GroupBump).unwrap(),
            json!({ "op": "group_bump" })
        );
    }

    #[test]
    fn write_invoke_and_group_invoke_keep_names_and_args_on_the_wire() {
        let w = node(
            1,
            NodeOpKind::write(1, "levelcontrol", "on-level", "128", false).unwrap(),
        );
        assert_eq!(
            to_op(&w).unwrap(),
            json!({"op":"write","node_id":1,"endpoint":1,"cluster":"levelcontrol","attribute":"on-level","value":"128"})
        );
        let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
        let i = node(
            1,
            NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args, false).unwrap(),
        );
        assert_eq!(
            to_op(&i).unwrap(),
            json!({"op":"invoke","node_id":1,"endpoint":1,"cluster":"levelcontrol","command":"move-to-level","args":["128","0","0","0"]})
        );
        let g = group(10, 1, GroupOpKind::invoke("onoff", "on", &[]).unwrap());
        assert_eq!(
            to_op(&g).unwrap(),
            json!({"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","args":[],"endpoint":1})
        );
        assert_eq!(
            to_op(&node(1, NodeOpKind::Describe)).unwrap(),
            json!({"op":"describe","node_id":1})
        );
    }

    #[test]
    fn timed_true_is_sent_on_wire_and_false_is_omitted() {
        let op = node(1, NodeOpKind::invoke(1, "onoff", "on", &[], true).unwrap());
        assert_eq!(to_op(&op).unwrap()["timed"], json!(true));
        let op = node(1, NodeOpKind::invoke(1, "onoff", "on", &[], false).unwrap());
        assert!(to_op(&op).unwrap().get("timed").is_none());
        let op = node(
            1,
            NodeOpKind::write(1, "levelcontrol", "on-level", "128", true).unwrap(),
        );
        assert_eq!(to_op(&op).unwrap()["timed"], json!(true));
    }

    #[test]
    fn group_grant_is_unsupported_via_matd() {
        // grant は稀な修復操作で warm session の恩恵が小さく、mat/matd バージョン
        // スキューにも安全なため直経路のみ（matd プロトコルに op を足さない）。
        let op = DeviceOp::GroupGrant {
            group_id: 1,
            node_ids: vec![5],
        };
        assert!(to_op(&op).unwrap_err().contains("group grant"));
    }

    #[test]
    fn group_color_temp_maps_to_group_color_temp_op() {
        let op = group(1, 1, GroupOpKind::color_temp(Some(2700), None, 0));
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"group_color_temp","group_id":1,
                "mireds":370,"kelvin":2700,"transition":0,"endpoint":1
            })
        );
    }

    #[test]
    fn group_level_maps_to_group_level_op() {
        let op = group(1, 1, GroupOpKind::level(50, 0));
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"group_level","group_id":1,
                "level":127,"percent":50,"transition":0,"endpoint":1
            })
        );
    }

    #[test]
    fn group_color_maps_to_group_color_op_with_echo() {
        let op = group(
            1,
            1,
            GroupOpKind::Color {
                color: mat_core::color::resolve_spec(Some("blue"), Some("#0000ff"), None, None)
                    .unwrap(),
                transition: 0,
            },
        );
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"group_color","group_id":1,
                "hue_raw":169,"saturation_raw":254,
                "hue":240,"saturation":100,"transition":0,"endpoint":1,
                "name":"blue","rgb":"#0000ff"
            })
        );
    }

    /// 直経路専用 op は matd へ送らない（文言はサブコマンド名入り）。
    #[test]
    fn direct_only_ops_are_unsupported_via_matd() {
        let dt = node(1, NodeOpKind::DiagThread { endpoint: 0 });
        assert!(to_op(&dt).unwrap_err().contains("diag"));
        let ow = node(
            1,
            NodeOpKind::OpenWindow {
                timeout: 180,
                iteration: 1000,
                discriminator: 1,
            },
        );
        assert!(to_op(&ow).unwrap_err().contains("open-window"));
    }

    #[test]
    fn read_cluster_maps_to_read_op_without_attribute_key() {
        let op = node(1, NodeOpKind::read_cluster(2, "onoff").unwrap());
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"read","node_id":1,"endpoint":2,"cluster":"onoff"})
        );
    }
}
