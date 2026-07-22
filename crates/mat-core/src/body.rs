//! 成功 JSON body の単一ソース。
//!
//! `mat` 直経路(`commands/*` の `emit_*`)と `matd`(`server.rs`)の両方が
//! ここを呼ぶことで、同一 op の成功出力が経路によらず同形であることを
//! **構造的に**保証する(0.23.1 で踏んだ「sibling 関数への修正適用漏れ」
//! クラスの再発防止)。timestamp は含めない — 直経路は `output::emit`、
//! matd は応答 envelope が付与する。
//!
//! 対象は両経路に存在する op のみ。直経路専用 op(open-window / diag /
//! grant / discover / commission 等)の emit は重複が無いため `mat` 側に残す。

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

/// `level` の成功 body。入力 percent と換算後 level を両方エコー。
pub fn level_success(
    node_id: u64,
    endpoint: u16,
    percent: u8,
    level: u8,
    transition: u16,
) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": percent,
        "level": level,
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

/// `group invoke` の sent body。
pub fn group_invoke_sent(group_id: u16, cluster: &str, command: &str, endpoint: u16) -> Value {
    json!({
        "group_id": group_id,
        "cluster": cluster,
        "command": command,
        "endpoint": endpoint,
        "status": "sent",
        "note": GROUPCAST_NOTE,
    })
}

/// `group color-temp` の sent body。
pub fn group_color_temp_sent(
    group_id: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
    endpoint: u16,
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
        "note": GROUPCAST_NOTE,
    })
}

/// `group level` の sent body。
pub fn group_level_sent(
    group_id: u16,
    percent: u8,
    level: u8,
    transition: u16,
    endpoint: u16,
) -> Value {
    json!({
        "group_id": group_id,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": percent,
        "level": level,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": GROUPCAST_NOTE,
    })
}

/// `group color` の sent body。name / rgb は指定時のみキーが現れる。
pub fn group_color_sent(
    group_id: u16,
    color: &ResolvedColor,
    transition: u16,
    endpoint: u16,
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
            level_success(5, 1, 50, 127, 0),
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
            group_invoke_sent(10, "onoff", "on", 1),
            json!({
                "group_id": 10, "cluster": "onoff", "command": "on",
                "endpoint": 1, "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_color_temp_sent_shape() {
        assert_eq!(
            group_color_temp_sent(10, 2700, 370, 0, 1),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-color-temperature",
                "kelvin": 2700, "mireds": 370, "transition": 0,
                "endpoint": 1, "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_level_sent_shape() {
        assert_eq!(
            group_level_sent(10, 50, 127, 0, 1),
            json!({
                "group_id": 10, "cluster": "levelcontrol",
                "command": "move-to-level",
                "percent": 50, "level": 127, "transition": 0,
                "endpoint": 1, "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_color_sent_shape() {
        assert_eq!(
            group_color_sent(10, &color_fixture(), 0, 1),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 240, "saturation": 100,
                "hue_raw": 169, "saturation_raw": 254,
                "transition": 0, "endpoint": 1, "status": "sent",
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
            group_color_sent(10, &color, 0, 1),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 14, "saturation": 100,
                "hue_raw": 10, "saturation_raw": 254,
                "transition": 0, "endpoint": 1,
                "status": "sent", "note": GROUPCAST_NOTE,
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
}
