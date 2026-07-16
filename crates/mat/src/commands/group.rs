//! `mat group` — Matter wire group（groupcast）。
//!
//! 元の動機は「多数の照明を multicast 1発で同期 ON/OFF」（点灯のポップコーン現象の
//! 解消）。`mat` はプロトコルを直接喋らず、すべて `chip-tool` の group 機能に委譲する。
//!
//! group state（鍵束・GroupKeyMap）は `mat` 独自台帳を持たず、`chip-tool` の永続
//! ストレージ（store 配下）と `groupsettings` に委ねる（設計ルール4: credential KVS
//! 以外の state を持たない）。
//!
//! groupcast は **unacknowledged**。`invoke` / `color-temp` / `color` は応答を受け取れないため "sent" しか
//! 報告できない（per-device の配信成否は原理的に取れない）。

use std::path::Path;

use serde_json::json;

use crate::runner::ChipTool;
use mat_core::acl::{merge_group_entry, parse_acl_from_chip_log, to_chip_write_json};
use mat_core::color::ResolvedColor;
use mat_core::error::{ErrorKind, MatError};
use mat_core::group::{group_node_id, resolve_epoch_key, EPOCH_START_TIME, KEY_SECURITY_POLICY};
use mat_core::normalize::classify_failure;
use mat_core::output;
use mat_core::parse::operation_succeeded;
use mat_core::store::Store;

/// コントローラ側 group state（groupsettings 系: add-group → add-keysets →
/// （rebind: unbind-keyset）→ bind-keyset）。chip-tool 直経路の `provision` と
/// native 直経路（`native_direct::NativeOp::GroupProvision`）が共有する —
/// M8a では controller 側 group state（KVS 書込）は引き続き chip-tool 側の
/// 責務（KVS 書込所有の分割は M8c）。epoch key はここで解決（明示指定を検証
/// または未指定ならランダム生成）し hex 文字列で返す — デバイス側
/// KeySetWrite にも同じ鍵を使う必要があるため。
pub(crate) fn provision_controller_state(
    chip: &ChipTool,
    group_id: u16,
    keyset_id: u16,
    name: &str,
    epoch_key: Option<&str>,
    rebind: bool,
) -> Result<String, MatError> {
    // epoch key を決定: 明示指定があれば検証して採用、無ければランダム生成。
    let epoch_key = resolve_epoch_key(epoch_key)?;

    // add-group → add-keysets → bind-keyset。鍵はコントローラとデバイスで一致が必須。
    run_step(
        chip,
        vec![
            "groupsettings".into(),
            "add-group".into(),
            name.to_string(),
            group_id.to_string(),
        ],
        &format!("groupsettings add-group {name}"),
    )?;
    run_step(
        chip,
        // chip-tool の add-keysets は `<keysetId> <keyPolicy> <validityTime> <EpochKey>`
        // の4引数（add-group/bind-keyset と違い group 名は取らない。先頭に name を置くと
        // keysetId と誤読し `Invalid argument keysetId` で落ちる）。validityTime は
        // EPOCH_START_TIME（=デバイス側 epochStartTime0）と一致させる。実機 E2E で確定。
        vec![
            "groupsettings".into(),
            "add-keysets".into(),
            keyset_id.to_string(),
            KEY_SECURITY_POLICY.into(),
            EPOCH_START_TIME.into(),
            format!("hex:{epoch_key}"),
        ],
        &format!("groupsettings add-keysets {keyset_id}"),
    )?;
    if rebind {
        // 既存グループの keyset binding を解除してから bind し直す（issue #5:
        // controller 側 groupsettings は永続化されており、bind 済みだと bind-keyset
        // が Duplicate key id で落ちる）。unbind は best-effort: 「未 bind なのに
        // unbind」を区別せず失敗を無視する（unbind が本当に必要で失敗したケースは
        // 直後の bind-keyset が従来どおり落ちるので、検知はそちらに委ねる）。
        let out = chip.run(vec![
            "groupsettings".into(),
            "unbind-keyset".into(),
            group_id.to_string(),
            keyset_id.to_string(),
        ])?;
        if !out.success() {
            tracing::debug!(
                group_id,
                keyset_id,
                code = ?out.code,
                "groupsettings unbind-keyset failed; ignored (best-effort rebind)"
            );
        }
    }
    run_step(
        chip,
        vec![
            "groupsettings".into(),
            "bind-keyset".into(),
            group_id.to_string(),
            keyset_id.to_string(),
        ],
        &format!("groupsettings bind-keyset {group_id}"),
    )?;
    Ok(epoch_key)
}

/// `mat group provision` — 各ノードへ鍵束・マッピングを焼き、コントローラ側 group
/// state も設定する。
#[allow(clippy::too_many_arguments)]
pub fn provision(
    store_path: &Path,
    group_id: u16,
    node_ids: &[u64],
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    epoch_key: Option<&str>,
    rebind: bool,
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    // 全ノードが commission 済みであることを先に確認（1つでも未登録なら exit 11）。
    for &node_id in node_ids {
        store.require_node(node_id)?;
    }

    let chip = ChipTool::new(store.root());

    // 1) コントローラ側 group state（ローカル操作、ネットワーク不要）。
    let epoch_key =
        provision_controller_state(&chip, group_id, keyset_id, name, epoch_key, rebind)?;

    // 2) 各デバイスへ provision（unicast, acknowledged）。最初の失敗で停止する
    //    （部分結果を stdout に出さず、stdout の純度を保つ）。
    for &node_id in node_ids {
        // KeySetWrite: 鍵束をデバイスへ書く。
        let key_set = json!({
            "groupKeySetID": keyset_id,
            "groupKeySecurityPolicy": 0,
            "epochKey0": epoch_key,
            "epochStartTime0": 1,
            "epochKey1": null,
            "epochStartTime1": null,
            "epochKey2": null,
            "epochStartTime2": null,
        });
        run_node_step(
            &chip,
            vec![
                "groupkeymanagement".into(),
                "key-set-write".into(),
                key_set.to_string(),
                node_id.to_string(),
                "0".into(),
            ],
            node_id,
            "key-set-write",
        )?;

        // GroupKeyMap: group_id → keyset_id の対応をデバイスへ書く。
        let key_map = json!([{
            "groupId": group_id,
            "groupKeySetID": keyset_id,
        }]);
        run_node_step(
            &chip,
            vec![
                "groupkeymanagement".into(),
                "write".into(),
                "group-key-map".into(),
                key_map.to_string(),
                node_id.to_string(),
                "0".into(),
            ],
            node_id,
            "write group-key-map",
        )?;

        // AddGroup: 指定エンドポイントを group へ加える。
        run_node_step(
            &chip,
            vec![
                "groups".into(),
                "add-group".into(),
                group_id.to_string(),
                name.to_string(),
                node_id.to_string(),
                endpoint.to_string(),
            ],
            node_id,
            "groups add-group",
        )?;

        // ACL: groupcast は authMode=Group で届くため、Group エントリが無いと
        // デバイスが黙って捨てる（commissioning が作るのは CASE 管理者エントリだけ）。
        ensure_group_acl(&chip, node_id, group_id)?;
    }

    emit_provision_success(group_id, keyset_id, name, endpoint, node_ids, rebind);
    Ok(())
}

/// `provision` の出力部（直経路 native からも共有 — M8a Task9）。
pub(crate) fn emit_provision_success(
    group_id: u16,
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    node_ids: &[u64],
    rebind: bool,
) {
    let mut body = json!({
        "group_id": group_id,
        "keyset_id": keyset_id,
        "name": name,
        "endpoint": endpoint,
        "nodes": node_ids,
        "status": "provisioned",
    });
    if rebind {
        // 直経路の rebind は matd の warm chip-tool が旧 group 状態をメモリに
        // 持ったままになるため、稼働中なら再起動が要る（storage は更新済み）。
        body["note"] =
            json!("rebound keyset binding; if matd is running, restart it to reload group state");
    }
    output::emit(body);
}

/// groupcast の送信部（出力なし）。invoke / color-temp / color ショートカットで共有。
/// groupcast は unacknowledged で応答（SUCCESS 行）は返らないため operation_succeeded
/// は見ない。送信プロセスが正常終了したかだけで「送った」と判断する。
fn send(
    store_path: &Path,
    group_id: u16,
    cluster: &str,
    command: &str,
    args: &[String],
    endpoint: u16,
) -> Result<(), MatError> {
    // 特定 node 宛ではないので require_node はしないが、chip-tool の永続ストレージ
    // （焼いた group 鍵を含む）参照のため store は必要。
    let store = Store::open(store_path)?;
    let chip = ChipTool::new(store.root());

    let group_node_id = group_node_id(group_id);

    // invoke と同じ並び: `<cluster> <command> [args...] <宛先> <endpoint>`。
    // 宛先に group node-id を置くと chip-tool が multicast 送信する。
    let mut argv = vec![cluster.to_string(), command.to_string()];
    argv.extend(args.iter().cloned());
    argv.push(group_node_id.clone());
    argv.push(endpoint.to_string());

    let out = chip.run(argv)?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("group invoke {cluster}/{command} to group {group_id} failed"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!(
                "chip-tool group invoke exited with {:?} (group {group_id})",
                out.code
            ),
        ));
    }
    Ok(())
}

/// `mat group invoke` — group へ multicast でコマンドを送る。
pub fn invoke(
    store_path: &Path,
    group_id: u16,
    cluster: &str,
    command: &str,
    args: &[String],
    endpoint: u16,
) -> Result<(), MatError> {
    send(store_path, group_id, cluster, command, args, endpoint)?;
    emit_invoke_sent(group_id, cluster, command, endpoint);
    Ok(())
}

/// `invoke` の出力部（直経路 native からも共有 — M7 Task5）。
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

/// `mat group color-temp` — MoveToColorTemperature を groupcast する
/// （`mat color-temp` の group 版）。入力 kelvin と換算後 mireds を両方エコーする。
pub fn color_temp(
    store_path: &Path,
    group_id: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
    endpoint: u16,
) -> Result<(), MatError> {
    // MoveToColorTemperature の引数は <mireds> <transition> <optionsMask> <optionsOverride>。
    let args = [
        mireds.to_string(),
        transition.to_string(),
        "0".to_string(),
        "0".to_string(),
    ];
    send(
        store_path,
        group_id,
        "colorcontrol",
        "move-to-color-temperature",
        &args,
        endpoint,
    )?;
    emit_color_temp_sent(group_id, kelvin, mireds, transition, endpoint);
    Ok(())
}

/// `color_temp` の出力部（直経路 native からも共有 — M7 Task5）。
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

/// `mat group color` — MoveToHueAndSaturation を groupcast する（`mat color` の
/// group 版）。入力（name / rgb / 度・%）と換算後の 0–254 生値を両方エコーする。
pub fn color(
    store_path: &Path,
    group_id: u16,
    color: &ResolvedColor,
    transition: u16,
    endpoint: u16,
) -> Result<(), MatError> {
    // MoveToHueAndSaturation の引数は <hue> <saturation> <transition>
    // <optionsMask> <optionsOverride>。
    let args = [
        color.hue_raw.to_string(),
        color.sat_raw.to_string(),
        transition.to_string(),
        "0".to_string(),
        "0".to_string(),
    ];
    send(
        store_path,
        group_id,
        "colorcontrol",
        "move-to-hue-and-saturation",
        &args,
        endpoint,
    )?;
    emit_color_sent(group_id, color, transition, endpoint);
    Ok(())
}

/// `color` の出力部（直経路 native からも共有 — M7 Task5）。
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

/// `mat group grant` — provision 済みグループの ACL 欠落を修復する。各ノードへ
/// ACL の read-merge-write（provision の step 4 と同じ処理）だけを実行する。
/// ノードごとに fail-fast（provision と同じ方針。部分結果は stdout に出さない）。
pub fn grant(store_path: &Path, group_id: u16, node_ids: &[u64]) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    // 全ノードが commission 済みであることを先に確認（1つでも未登録なら exit 11）。
    for &node_id in node_ids {
        store.require_node(node_id)?;
    }
    let chip = ChipTool::new(store.root());

    let mut updated: Vec<u64> = Vec::new();
    let mut unchanged: Vec<u64> = Vec::new();
    for &node_id in node_ids {
        if ensure_group_acl(&chip, node_id, group_id)? {
            updated.push(node_id);
        } else {
            unchanged.push(node_id);
        }
    }

    emit_grant_success(group_id, node_ids, &updated, &unchanged);
    Ok(())
}

/// `grant` の出力部（直経路 native からも共有 — M8a Task9）。
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

/// ローカル group state ステップ（groupsettings 系）を実行し、失敗を分類する。
fn run_step(chip: &ChipTool, argv: Vec<String>, what: &str) -> Result<(), MatError> {
    let out = chip.run(argv)?;
    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(kind, format!("{what} failed")));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!(
                "{what} did not succeed (chip-tool exited with {:?})",
                out.code
            ),
        ));
    }
    Ok(())
}

/// デバイス向け provision ステップ。失敗時はどの node のどの step かを detail に残す
/// （AI が再試行判断できる粒度）。
fn run_node_step(
    chip: &ChipTool,
    argv: Vec<String>,
    node_id: u64,
    step: &str,
) -> Result<(), MatError> {
    let out = chip.run(argv)?;
    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("provision step '{step}' failed on node {node_id}"),
        ));
    }
    if !out.success() || !operation_succeeded(&out.stdout) {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!("provision step '{step}' on node {node_id} did not report success"),
        ));
    }
    Ok(())
}

/// ACL の read-merge-write（provision の step 4 / `mat group grant` の本体）。
/// 戻り値: write した = true / 既に Group エントリがあり skip = false（冪等）。
///
/// ACL の attribute write は**全置換**なので、write は必ず「read できたリスト +
/// 追記」のみ。read が失敗・解釈不能なら絶対に write しない（管理者エントリを
/// 失うとデバイスが管理不能になり工場リセット行きのため）。
fn ensure_group_acl(chip: &ChipTool, node_id: u64, group_id: u16) -> Result<bool, MatError> {
    // read。属性 read は成功時に status 行を出さない（operation_succeeded が偽に
    // なる）ため run_node_step は使わず、分類 + パースで成否を判定する。
    let out = chip.run(vec![
        "accesscontrol".to_string(),
        "read".into(),
        "acl".into(),
        node_id.to_string(),
        "0".into(),
    ])?;
    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("provision step 'acl read' failed on node {node_id}"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!("provision step 'acl read' on node {node_id} did not succeed"),
        ));
    }
    let entries = parse_acl_from_chip_log(&out.stdout)
        .map_err(|e| MatError::new(e.kind, format!("acl read on node {node_id}: {}", e.detail)))?;

    let Some(merged) = merge_group_entry(&entries, group_id) else {
        return Ok(false); // 既に Group エントリがある。write 不要（冪等）。
    };
    run_node_step(
        chip,
        vec![
            "accesscontrol".to_string(),
            "write".into(),
            "acl".into(),
            to_chip_write_json(&merged),
            node_id.to_string(),
            "0".into(),
        ],
        node_id,
        "acl write",
    )?;
    Ok(true)
}
