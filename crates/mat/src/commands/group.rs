//! `mat group` — Matter wire group（groupcast）。
//!
//! 元の動機は「多数の照明を multicast 1発で同期 ON/OFF」（点灯のポップコーン現象の
//! 解消）。バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で
//! chip-tool 経路は撤去 — provision の KVS 書込は `mat-controller::group_settings`、
//! groupcast 送出は `mat-native::group`）。このモジュールは native 経路から呼ばれる
//! 成功 JSON の emit のみを持つ（スキーマの単一ソース）。
//!
//! groupcast は **unacknowledged**。`invoke` / `color-temp` / `level` / `color` は応答を
//! 受け取れないため "sent" しか報告できない（per-device の配信成否は原理的に取れない）。

use serde_json::json;

use mat_core::color::ResolvedColor;
use mat_core::output;

/// `provision` の出力部（native 直経路の単一ソース — M8a Task9、M8c-2 Task5 で
/// `native_kvs` を追加）。`native_kvs=true` はコントローラ側 group state を
/// native KVS 直書きで済ませた経路（`native_direct::NativeOp::GroupProvision`）
/// —— rebind の有無によらず常に同じ note（KVS を直接触った旨と matd 再起動の
/// 案内）。
pub(crate) fn emit_provision_success(
    group_id: u16,
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    node_ids: &[u64],
    rebind: bool,
    native_kvs: bool,
) {
    let mut body = json!({
        "group_id": group_id,
        "keyset_id": keyset_id,
        "name": name,
        "endpoint": endpoint,
        "nodes": node_ids,
        "status": "provisioned",
    });
    if native_kvs {
        // native は rebind の有無によらず KVS を直接書くので常にこの note。
        body["note"] = json!(
            "controller group state written natively to kvs; if matd is running, restart it to reload group state"
        );
    } else if rebind {
        // 直経路の rebind は matd の warm セッションが旧 group 状態をメモリに
        // 持ったままになるため、稼働中なら再起動が要る（storage は更新済み）。
        body["note"] =
            json!("rebound keyset binding; if matd is running, restart it to reload group state");
    }
    output::emit(body);
}

/// `invoke` の出力部（native 直経路の単一ソース — M7 Task5）。
pub(crate) fn emit_invoke_sent(group_id: u16, cluster: &str, command: &str, endpoint: u16) {
    output::emit(json!({
        "group_id": group_id,
        "cluster": cluster,
        "command": command,
        "endpoint": endpoint,
        "status": "sent",
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    }));
}

/// `color_temp` の出力部（native 直経路の単一ソース — M7 Task5）。
pub(crate) fn emit_color_temp_sent(
    group_id: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
    endpoint: u16,
) {
    output::emit(json!({
        "group_id": group_id,
        "cluster": "colorcontrol",
        "command": "move-to-color-temperature",
        "kelvin": kelvin,
        "mireds": mireds,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    }));
}

/// `level` の出力部（native 直経路の単一ソース）。
pub(crate) fn emit_level_sent(
    group_id: u16,
    percent: u8,
    level: u8,
    transition: u16,
    endpoint: u16,
) {
    output::emit(json!({
        "group_id": group_id,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": percent,
        "level": level,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    }));
}

/// `color` の出力部（native 直経路の単一ソース — M7 Task5）。
pub(crate) fn emit_color_sent(
    group_id: u16,
    color: &ResolvedColor,
    transition: u16,
    endpoint: u16,
) {
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
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    });
    if let Some(name) = &color.name {
        body["name"] = json!(name);
    }
    if let Some(rgb) = &color.rgb {
        body["rgb"] = json!(rgb);
    }
    output::emit(body);
}

/// `grant` の出力部（native 直経路の単一ソース — M8a Task9）。
pub(crate) fn emit_grant_success(
    group_id: u16,
    node_ids: &[u64],
    updated: &[u64],
    unchanged: &[u64],
) {
    output::emit(json!({
        "group_id": group_id,
        "nodes": node_ids,
        "updated": updated,
        "unchanged": unchanged,
        "status": "granted",
    }));
}
