//! `mat group` — Matter wire group（groupcast）。
//!
//! 元の動機は「多数の照明を multicast 1発で同期 ON/OFF」（点灯のポップコーン現象の
//! 解消）。`mat` はプロトコルを直接喋らず、すべて `chip-tool` の group 機能に委譲する。
//!
//! group state（鍵束・GroupKeyMap）は `mat` 独自台帳を持たず、`chip-tool` の永続
//! ストレージ（store 配下）と `groupsettings` に委ねる（設計ルール4: credential KVS
//! 以外の state を持たない）。
//!
//! groupcast は **unacknowledged**。`invoke` は応答を受け取れないため "sent" しか
//! 報告できない（per-device の配信成否は原理的に取れない）。

use std::path::Path;

use serde_json::json;

use crate::runner::ChipTool;
use mat_core::error::{ErrorKind, MatError};
use mat_core::group::{group_node_id, resolve_epoch_key, KEY_SECURITY_POLICY};
use mat_core::normalize::classify_failure;
use mat_core::output;
use mat_core::parse::operation_succeeded;
use mat_core::store::Store;

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
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    // 全ノードが commission 済みであることを先に確認（1つでも未登録なら exit 11）。
    for &node_id in node_ids {
        store.require_node(node_id)?;
    }

    // epoch key を決定: 明示指定があれば検証して採用、無ければランダム生成。
    let epoch_key = resolve_epoch_key(epoch_key)?;

    let chip = ChipTool::new(store.root());

    // 1) コントローラ側 group state（ローカル操作、ネットワーク不要）。
    //    add-group → add-keysets → bind-keyset。鍵はコントローラとデバイスで一致が必須。
    run_step(
        &chip,
        vec![
            "groupsettings".into(),
            "add-group".into(),
            name.to_string(),
            group_id.to_string(),
        ],
        &format!("groupsettings add-group {name}"),
    )?;
    run_step(
        &chip,
        vec![
            "groupsettings".into(),
            "add-keysets".into(),
            name.to_string(),
            keyset_id.to_string(),
            KEY_SECURITY_POLICY.into(),
            format!("hex:{epoch_key}"),
        ],
        &format!("groupsettings add-keysets {keyset_id}"),
    )?;
    run_step(
        &chip,
        vec![
            "groupsettings".into(),
            "bind-keyset".into(),
            group_id.to_string(),
            keyset_id.to_string(),
        ],
        &format!("groupsettings bind-keyset {group_id}"),
    )?;

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
    }

    output::emit(json!({
        "group_id": group_id,
        "keyset_id": keyset_id,
        "name": name,
        "endpoint": endpoint,
        "nodes": node_ids,
        "status": "provisioned",
    }));
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
    // groupcast は unacknowledged。応答（SUCCESS 行）は返らないため operation_succeeded
    // は見ない。送信プロセスが正常終了したかだけで「送った」と判断する。
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!(
                "chip-tool group invoke exited with {:?} (group {group_id})",
                out.code
            ),
        ));
    }

    output::emit(json!({
        "group_id": group_id,
        "cluster": cluster,
        "command": command,
        "endpoint": endpoint,
        "status": "sent",
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    }));
    Ok(())
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
