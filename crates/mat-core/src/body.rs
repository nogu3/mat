//! 成功 JSON body の単一ソース。
//!
//! `mat` 直経路(`commands/*` の `emit_*`)と `matd`(`server.rs`)の両方が
//! ここを呼ぶことで、同一 op の成功出力が経路によらず同形であることを
//! **構造的に**保証する(0.23.1 で踏んだ「sibling 関数への修正適用漏れ」
//! クラスの再発防止)。timestamp は含めない — 直経路は `output::emit`、
//! matd は応答 envelope が付与する。
//!
//! 直経路専用 op(open-window / diag thread / grant)の body もここに置く
//! (mat-native::op::run_node_op / runner::grant が経路によらず組むため)。
//! discover / commission / diag node / mesh は専用コマンド層(`mat/commands`)
//! が emit する。

use serde_json::{json, Value};

use crate::color::ResolvedColor;
use crate::parse::normalize_value;

/// groupcast は unacknowledged — "sent" 系 body 共通の注記。
const GROUPCAST_NOTE: &str = "unacknowledged groupcast; per-device delivery not confirmed";

/// `read` の成功 body。
pub fn read_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value: Value,
) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "attribute": attribute,
        "value": value,
    })
}

/// cluster wildcard `read`（`--attribute` 省略）の成功 body。`attributes` は
/// 属性名（chip-tool 表記、表に無い ID は 10 進文字列）→ 値。list/struct 値も
/// そのまま載せる。デバイスが持たない属性は返らない（wildcard は per-attribute
/// status を返さない）。
pub fn read_cluster_success(
    node_id: u64,
    endpoint: u16,
    cluster_in: &str,
    cluster: u32,
    rows: Vec<(u32, Value)>,
) -> Value {
    let def = crate::ids::find_cluster(cluster);
    let mut attrs = serde_json::Map::new();
    for (id, v) in rows {
        let key = def
            .and_then(|d| d.attrs.iter().find(|a| a.id == id))
            .map(|a| a.name.to_string())
            .unwrap_or_else(|| id.to_string());
        attrs.insert(key, v);
    }
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster_in,
        "attributes": Value::Object(attrs),
    })
}

/// `write` の成功 body。`value_in` は CLI/プロトコル入力の生文字列 —— read と
/// 型を揃えるため normalize_value で型推定してから載せる(両経路共通の規則)。
pub fn write_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value_in: &str,
) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "attribute": attribute,
        "value": normalize_value(value_in),
        "status": "success",
    })
}

/// `invoke` / `on` / `off` の成功 body。
pub fn invoke_success(node_id: u64, endpoint: u16, cluster: &str, command: &str) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "command": command,
        "status": "success",
    })
}

/// `color-temp` の成功 body。入力 kelvin と換算後 mireds を両方エコー
/// (`color-temperature-mireds` の読み返しと突合しやすくする)。
pub fn color_temp_success(
    node_id: u64,
    endpoint: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "colorcontrol",
        "command": "move-to-color-temperature",
        "kelvin": kelvin,
        "mireds": mireds,
        "transition": transition,
        "status": "success",
    })
}

/// `percent`(入力)と `level`(換算後 0–254)のエコーペア。隣接する同型 `u8`
/// 引数の取り違え(swap)をコンパイル時に防ぐ(v1 品質修正 8 — 任意項目)。
#[derive(Debug, Clone, Copy)]
pub struct LevelEcho {
    pub percent: u8,
    pub level: u8,
}

/// `level` の成功 body。入力 percent と換算後 level を両方エコー。
pub fn level_success(node_id: u64, endpoint: u16, echo: LevelEcho, transition: u16) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": echo.percent,
        "level": echo.level,
        "transition": transition,
        "status": "success",
    })
}

/// `color` の成功 body。入力(name / rgb / 度・%)と換算後 0–254 生値を両方
/// エコー。name / rgb は指定時のみキーが現れる(省略時キー無し — 既存形)。
pub fn color_success(node_id: u64, endpoint: u16, color: &ResolvedColor, transition: u16) -> Value {
    let mut body = json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "colorcontrol",
        "command": "move-to-hue-and-saturation",
        "hue": color.hue,
        "saturation": color.sat,
        "hue_raw": color.hue_raw,
        "saturation_raw": color.sat_raw,
        "transition": transition,
        "status": "success",
    });
    if let Some(name) = &color.name {
        body["name"] = json!(name);
    }
    if let Some(rgb) = &color.rgb {
        body["rgb"] = json!(rgb);
    }
    body
}

/// `describe` の成功 body。クラスタは数値 ID の配列(名前解決は `mat` の責務外)。
pub fn describe_success(node_id: u64, endpoints: &[(u16, Vec<u64>)]) -> Value {
    let out_endpoints: Vec<Value> = endpoints
        .iter()
        .map(|(ep, clusters)| json!({ "endpoint": ep, "clusters": clusters }))
        .collect();
    json!({
        "node_id": node_id,
        "endpoints": out_endpoints,
    })
}

/// `group invoke` の sent body。`egress` は実際に送出した iface 名の配列
/// (LAN 単独でも常に出す — 後方互換な追加フィールド)。
pub fn group_invoke_sent(
    group_id: u16,
    cluster: &str,
    command: &str,
    endpoint: u16,
    egress: &[String],
) -> Value {
    json!({
        "group_id": group_id,
        "cluster": cluster,
        "command": command,
        "endpoint": endpoint,
        "status": "sent",
        "egress": egress,
        "note": GROUPCAST_NOTE,
    })
}

/// `group bump` の成功 body。counter 窓ジャンプ（Issue #14 応急コマンド —
/// 受信側リプレイ窓が送信系列より先行した状態を matd 再起動なしで回復する）。
pub fn group_bump(from: u32, to: u32) -> Value {
    json!({ "group_counter": { "from": from, "to": to } })
}

/// `group color-temp` の sent body。`egress` は `group_invoke_sent` と同義。
pub fn group_color_temp_sent(
    group_id: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
    endpoint: u16,
    egress: &[String],
) -> Value {
    json!({
        "group_id": group_id,
        "cluster": "colorcontrol",
        "command": "move-to-color-temperature",
        "kelvin": kelvin,
        "mireds": mireds,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "egress": egress,
        "note": GROUPCAST_NOTE,
    })
}

/// `group level` の sent body。`egress` は `group_invoke_sent` と同義。
pub fn group_level_sent(
    group_id: u16,
    echo: LevelEcho,
    transition: u16,
    endpoint: u16,
    egress: &[String],
) -> Value {
    json!({
        "group_id": group_id,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": echo.percent,
        "level": echo.level,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "egress": egress,
        "note": GROUPCAST_NOTE,
    })
}

/// `group color` の sent body。name / rgb は指定時のみキーが現れる。`egress`
/// は `group_invoke_sent` と同義。
pub fn group_color_sent(
    group_id: u16,
    color: &ResolvedColor,
    transition: u16,
    endpoint: u16,
    egress: &[String],
) -> Value {
    let mut body = json!({
        "group_id": group_id,
        "cluster": "colorcontrol",
        "command": "move-to-hue-and-saturation",
        "hue": color.hue,
        "saturation": color.sat,
        "hue_raw": color.hue_raw,
        "saturation_raw": color.sat_raw,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "egress": egress,
        "note": GROUPCAST_NOTE,
    });
    if let Some(name) = &color.name {
        body["name"] = json!(name);
    }
    if let Some(rgb) = &color.rgb {
        body["rgb"] = json!(rgb);
    }
    body
}

/// `group provision` の成功 body。`note` は経路差のある案内文(直経路 native は
/// KVS 直書き+matd 再起動案内、matd 経路は無し)— 文言の決定は呼び出し側の責務。
pub fn group_provision_success(
    group_id: u16,
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    nodes: &[u64],
    note: Option<&str>,
) -> Value {
    let mut body = json!({
        "group_id": group_id,
        "keyset_id": keyset_id,
        "name": name,
        "endpoint": endpoint,
        "nodes": nodes,
        "status": "provisioned",
    });
    if let Some(note) = note {
        body["note"] = json!(note);
    }
    body
}

/// `diag thread` の成功 body。`unavailable` は (chip-tool 属性名, kind)
/// — 空なら `unavailable` キー自体を出さない（既存形）。
pub fn diag_thread_success(
    node_id: u64,
    endpoint: u16,
    thread: serde_json::Map<String, Value>,
    unavailable: &[(String, crate::error::ErrorKind)],
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("node_id".to_string(), json!(node_id));
    body.insert("endpoint".to_string(), json!(endpoint));
    body.insert("thread".to_string(), Value::Object(thread));
    if !unavailable.is_empty() {
        let rows: Vec<Value> = unavailable
            .iter()
            .map(|(attr, kind)| {
                json!({
                    "attribute": attr,
                    "kind": serde_json::to_value(kind).unwrap_or(Value::Null),
                })
            })
            .collect();
        body.insert("unavailable".to_string(), Value::Array(rows));
    }
    Value::Object(body)
}

/// `open-window` の成功 body。`expires_at` は timeout 秒後の ISO 8601。
pub fn open_window_success(
    node_id: u64,
    manual_code: &str,
    qr_payload: &str,
    timeout: u32,
) -> Value {
    json!({
        "node_id": node_id,
        "manual_code": manual_code,
        "qr_payload": qr_payload,
        "expires_at": crate::output::expires_in(i64::from(timeout)),
    })
}

/// `unpair` のデバイス側成功（出力 JSON の `device` オブジェクト）。
pub fn unpair_device(fabric_index: u8) -> Value {
    json!({ "removed": true, "fabric_index": fabric_index })
}

/// `group list` の成功 body。`groups` = (group_id, name, keyset_id)、
/// `keysets` = (keyset_id, bound_groups)。鍵素材は含めない。
pub fn group_list_success(
    fabric_index: u8,
    groups: &[(u16, &str, Option<u16>)],
    keysets: &[(u16, &[u16])],
) -> Value {
    json!({
        "fabric_index": fabric_index,
        "groups": groups.iter().map(|(id, name, ks)| json!({
            "group_id": id, "name": name, "keyset_id": ks,
        })).collect::<Vec<_>>(),
        "keysets": keysets.iter().map(|(id, bound)| json!({
            "keyset_id": id, "bound_groups": bound,
        })).collect::<Vec<_>>(),
    })
}

/// `fabric list` の成功 body。`fabrics` の各要素は呼び手が組む
/// （fabric_index / fabric_id / admin_node_id / compressed_fabric_id / ipk_epoch / current）。
pub fn fabric_list_success(store: &str, fabrics: Vec<Value>) -> Value {
    json!({ "store": store, "fabrics": fabrics })
}

/// `group grant` の成功 body。
pub fn group_grant_success(
    group_id: u16,
    node_ids: &[u64],
    updated: &[u64],
    unchanged: &[u64],
) -> Value {
    json!({
        "group_id": group_id,
        "nodes": node_ids,
        "updated": updated,
        "unchanged": unchanged,
        "status": "granted",
    })
}

/// `group remove` の成功 body。`nodes` = (node_id, acl_removed, group_removed, keymap_removed, keyset_removed)。
pub fn group_remove_success(
    group_id: u16,
    endpoint: u16,
    nodes: &[(u64, bool, bool, bool, bool)],
    controller_group_removed: bool,
    controller_keyset_removed: bool,
) -> Value {
    json!({
        "group_id": group_id,
        "endpoint": endpoint,
        "nodes": nodes.iter().map(|(id, acl, grp, map, ks)| json!({
            "node_id": id, "acl_removed": acl, "group_removed": grp,
            "keymap_removed": map, "keyset_removed": ks,
        })).collect::<Vec<_>>(),
        "controller": { "group_removed": controller_group_removed, "keyset_removed": controller_keyset_removed },
        "status": "removed",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn color_fixture() -> ResolvedColor {
        ResolvedColor {
            hue_raw: 169,
            sat_raw: 254,
            hue: 240,
            sat: 100,
            name: Some("blue".into()),
            rgb: None,
        }
    }

    #[test]
    fn read_success_shape() {
        assert_eq!(
            read_success(5, 1, "onoff", "on-off", json!(true)),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "onoff",
                "attribute": "on-off", "value": true,
            })
        );
    }

    #[test]
    fn write_success_normalizes_value_like_read() {
        // "100" → 100(read と型を揃える normalize_value 内包)。
        assert_eq!(
            write_success(5, 1, "levelcontrol", "on-level", "100"),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "levelcontrol",
                "attribute": "on-level", "value": 100, "status": "success",
            })
        );
    }

    #[test]
    fn invoke_success_shape() {
        assert_eq!(
            invoke_success(5, 1, "onoff", "on"),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "onoff",
                "command": "on", "status": "success",
            })
        );
    }

    #[test]
    fn color_temp_success_shape() {
        assert_eq!(
            color_temp_success(5, 1, 2700, 370, 0),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "colorcontrol",
                "command": "move-to-color-temperature",
                "kelvin": 2700, "mireds": 370, "transition": 0,
                "status": "success",
            })
        );
    }

    #[test]
    fn level_success_shape() {
        assert_eq!(
            level_success(
                5,
                1,
                LevelEcho {
                    percent: 50,
                    level: 127
                },
                0
            ),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "levelcontrol",
                "command": "move-to-level",
                "percent": 50, "level": 127, "transition": 0,
                "status": "success",
            })
        );
    }

    #[test]
    fn color_success_includes_optional_name_and_omits_absent_rgb() {
        assert_eq!(
            color_success(5, 1, &color_fixture(), 0),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 240, "saturation": 100,
                "hue_raw": 169, "saturation_raw": 254,
                "transition": 0, "status": "success",
                "name": "blue",
            })
        );
    }

    /// name / rgb 指定時のみキーが現れる分岐の Some 側(rgb=Some)。None 側は
    /// キー不在が既存テストで担保済み — Some 側の形状はここで初めてピン留め。
    #[test]
    fn color_success_includes_name_and_rgb_when_present() {
        let color = ResolvedColor {
            hue_raw: 10,
            sat_raw: 254,
            hue: 14,
            sat: 100,
            name: Some("red".to_string()),
            rgb: Some("#ff0000".to_string()),
        };
        assert_eq!(
            color_success(5, 1, &color, 0),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 14, "saturation": 100,
                "hue_raw": 10, "saturation_raw": 254,
                "transition": 0, "status": "success",
                "name": "red", "rgb": "#ff0000",
            })
        );
    }

    #[test]
    fn describe_success_shape() {
        assert_eq!(
            describe_success(5, &[(1, vec![6, 8])]),
            json!({
                "node_id": 5,
                "endpoints": [{ "endpoint": 1, "clusters": [6, 8] }],
            })
        );
    }

    #[test]
    fn group_invoke_sent_shape() {
        assert_eq!(
            group_invoke_sent(10, "onoff", "on", 1, &["eth0".into(), "wpan0".into()]),
            json!({
                "group_id": 10, "cluster": "onoff", "command": "on",
                "endpoint": 1, "status": "sent",
                "egress": ["eth0", "wpan0"],
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_color_temp_sent_shape() {
        assert_eq!(
            group_color_temp_sent(10, 2700, 370, 0, 1, &["eth0".into(), "wpan0".into()]),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-color-temperature",
                "kelvin": 2700, "mireds": 370, "transition": 0,
                "endpoint": 1, "status": "sent",
                "egress": ["eth0", "wpan0"],
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_level_sent_shape() {
        assert_eq!(
            group_level_sent(
                10,
                LevelEcho {
                    percent: 50,
                    level: 127
                },
                0,
                1,
                &["eth0".into(), "wpan0".into()]
            ),
            json!({
                "group_id": 10, "cluster": "levelcontrol",
                "command": "move-to-level",
                "percent": 50, "level": 127, "transition": 0,
                "endpoint": 1, "status": "sent",
                "egress": ["eth0", "wpan0"],
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_color_sent_shape() {
        assert_eq!(
            group_color_sent(10, &color_fixture(), 0, 1, &["eth0".into(), "wpan0".into()]),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 240, "saturation": 100,
                "hue_raw": 169, "saturation_raw": 254,
                "transition": 0, "endpoint": 1, "status": "sent",
                "egress": ["eth0", "wpan0"],
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
                "name": "blue",
            })
        );
    }

    #[test]
    fn group_color_sent_includes_name_and_rgb_when_present() {
        let color = ResolvedColor {
            hue_raw: 10,
            sat_raw: 254,
            hue: 14,
            sat: 100,
            name: Some("red".to_string()),
            rgb: Some("#ff0000".to_string()),
        };
        assert_eq!(
            group_color_sent(10, &color, 0, 1, &["eth0".into()]),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 14, "saturation": 100,
                "hue_raw": 10, "saturation_raw": 254,
                "transition": 0, "endpoint": 1,
                "status": "sent", "note": GROUPCAST_NOTE,
                "egress": ["eth0"],
                "name": "red", "rgb": "#ff0000",
            })
        );
    }

    #[test]
    fn group_provision_success_with_and_without_note() {
        assert_eq!(
            group_provision_success(10, 60, "living", 1, &[5, 6], None),
            json!({
                "group_id": 10, "keyset_id": 60, "name": "living",
                "endpoint": 1, "nodes": [5, 6], "status": "provisioned",
            })
        );
        let with_note = group_provision_success(10, 60, "living", 1, &[5], Some("x"));
        assert_eq!(with_note["note"], json!("x"));
    }

    #[test]
    fn diag_thread_success_omits_empty_unavailable() {
        let mut thread = serde_json::Map::new();
        thread.insert("channel".into(), json!(15));
        let body = diag_thread_success(5, 0, thread.clone(), &[]);
        assert_eq!(
            body,
            json!({ "node_id": 5, "endpoint": 0, "thread": { "channel": 15 } })
        );
        let body = diag_thread_success(
            5,
            0,
            thread,
            &[("rloc16".to_string(), crate::error::ErrorKind::Timeout)],
        );
        assert_eq!(
            body["unavailable"],
            json!([{ "attribute": "rloc16", "kind": "timeout" }])
        );
    }

    #[test]
    fn open_window_success_shape() {
        let body = open_window_success(5, "34970112332", "MT:ABC", 180);
        assert_eq!(body["node_id"], 5);
        assert_eq!(body["manual_code"], "34970112332");
        assert_eq!(body["qr_payload"], "MT:ABC");
        assert!(body["expires_at"].is_string());
    }

    #[test]
    fn group_list_success_shape() {
        let body = group_list_success(2, &[(1, "desk", Some(42))], &[(42, &[1])]);
        assert_eq!(
            body,
            json!({ "fabric_index": 2,
                    "groups": [{ "group_id": 1, "name": "desk", "keyset_id": 42 }],
                    "keysets": [{ "keyset_id": 42, "bound_groups": [1] }] })
        );
    }

    #[test]
    fn group_grant_success_shape() {
        assert_eq!(
            group_grant_success(10, &[5, 6], &[5], &[6]),
            json!({
                "group_id": 10, "nodes": [5, 6], "updated": [5],
                "unchanged": [6], "status": "granted",
            })
        );
    }

    #[test]
    fn group_remove_success_shape() {
        let body = group_remove_success(1, 1, &[(5, true, true, true, false)], true, false);
        assert_eq!(
            body,
            json!({ "group_id": 1, "endpoint": 1,
                    "nodes": [{ "node_id": 5, "acl_removed": true, "group_removed": true, "keymap_removed": true, "keyset_removed": false }],
                    "controller": { "group_removed": true, "keyset_removed": false },
                    "status": "removed" })
        );
    }

    #[test]
    fn read_cluster_success_keys_by_name_and_falls_back_to_decimal_id() {
        let body = read_cluster_success(
            5,
            1,
            "onoff",
            0x0006,
            vec![
                (0x0000, json!(true)),
                (0x4001, json!(0)),
                (0xFFFD, json!(5)),
                (0x7777, json!(1)),
            ],
        );
        assert_eq!(
            body,
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "onoff",
                "attributes": { "on-off": true, "on-time": 0, "cluster-revision": 5, "30583": 1 }
            })
        );
    }
}
