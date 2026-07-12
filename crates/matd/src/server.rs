//! 上流ソケットサーバ。unix socket で newline-delimited JSON リクエストを受け、
//! warm な chip-tool セッション（[`ChipToolBackend`]）に中継して応答を返す。
//!
//! 応答は `mat` の one-shot CLI と同じく純粋な構造化 JSON（mat スキーマ + `timestamp`）。
//! 人間装飾は混ぜない。node_id の解決可否は毎リクエスト KVS で確認する（常駐中に
//! `mat commission` が台帳を更新しても拾えるよう、開きっぱなしにしない）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use mat_controller::im;
use mat_core::acl::{acl_entries_from_ws_value, merge_group_entry, to_chip_write_json};
use mat_core::error::{ErrorKind, MatError};
use mat_core::group::{group_node_id, resolve_epoch_key, EPOCH_START_TIME, KEY_SECURITY_POLICY};
use mat_core::normalize::classify_failure;
use mat_core::output::now_iso8601;
use mat_core::parse::normalize_value;
use mat_core::store::Store;

use crate::backend::ChipToolBackend;
use crate::native::NativeBackend;
use crate::protocol::{Op, Request};

/// ソケットを bind し、接続を受け付け続ける。`Ctrl-C` で抜ける。
pub async fn serve(
    socket_path: &Path,
    store_path: PathBuf,
    backend: Arc<ChipToolBackend>,
    native: Option<Arc<NativeBackend>>,
) -> std::io::Result<()> {
    tracing::info!(native = native.is_some(), "matd backends");
    // 前回の残骸を掃除してから bind。
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(socket = %socket_path.display(), "matd listening");

    // ControlPersist 風に、アイドルセッションを定期的に畳む reaper。チェック周期は
    // アイドル基準の 1/4（最短 1 秒）。
    let reaper = {
        let backend = Arc::clone(&backend);
        let tick = (backend.idle() / 4).max(std::time::Duration::from_secs(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                backend.reap_if_idle().await;
            }
        })
    };

    // アイドル中も ws を生かす keepalive。chip-tool interactive server は 180 秒
    // 無トラフィックで ws PING を送り、20 秒で PONG が無いと切断する（issue #7）。
    // matd はコマンド実行中しか ws を poll しないため、アイドル中はこちらから定期的に
    // 生存トラフィックを作る。reap とは独立（last_used は更新しない）。
    let keepalive = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(crate::backend::KEEPALIVE_INTERVAL).await;
                backend.keepalive_tick().await;
            }
        })
    };

    // shutdown op（`matd stop`）で serve ループを抜けるための通知。
    let shutdown = Arc::new(Notify::new());

    let store_path = Arc::new(store_path);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted?;
                let backend = Arc::clone(&backend);
                let native = native.clone();
                let store_path = Arc::clone(&store_path);
                let shutdown = Arc::clone(&shutdown);
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, backend, native, store_path, shutdown).await {
                        tracing::warn!(error = %e, "connection handler ended with error");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl-C, shutting down");
                break;
            }
            _ = shutdown.notified() => {
                tracing::info!("received shutdown op, shutting down");
                break;
            }
        }
    }

    // graceful shutdown: reaper/keepalive を止め、chip-tool セッションを畳み、socket を消す。
    reaper.abort();
    keepalive.abort();
    backend.shutdown().await;
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// 1 接続。複数行のリクエストを順に処理し、各行に 1 行 JSON で応答する。
async fn handle_conn(
    stream: UnixStream,
    backend: Arc<ChipToolBackend>,
    native: Option<Arc<NativeBackend>>,
    store_path: Arc<PathBuf>,
    shutdown: Arc<Notify>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let (response, is_shutdown) =
            dispatch(&line, &backend, native.as_deref(), &store_path).await;
        let mut buf = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
        buf.push(b'\n');
        write_half.write_all(&buf).await?;
        // 応答をワイヤに出し切ってから停止を発火する（クライアントが確実に受け取る）。
        write_half.flush().await?;
        if is_shutdown {
            shutdown.notify_one();
            break;
        }
    }
    Ok(())
}

/// 1 リクエスト行を処理して応答 JSON を組み立てる。戻り値の bool は shutdown 要求か。
async fn dispatch(
    line: &str,
    backend: &ChipToolBackend,
    native: Option<&NativeBackend>,
    store_path: &Path,
) -> (Value, bool) {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return (
                error_response(
                    None,
                    &MatError::parse_error(format!("invalid request JSON: {e}")),
                ),
                false,
            )
        }
    };
    let id = req.id.clone();
    let is_shutdown = matches!(req.op, Op::Shutdown);

    let body = match run_op(&req.op, backend, native, store_path).await {
        Ok(mut body) => {
            // id をエコーし、timestamp を必ず付ける（mat スキーマ規約）。
            if let Value::Object(map) = &mut body {
                if let Some(id) = id {
                    map.insert("id".into(), id);
                }
                map.entry("timestamp".to_string())
                    .or_insert_with(|| Value::String(now_iso8601()));
            }
            body
        }
        Err(e) => error_response(id, &e),
    };
    (body, is_shutdown)
}

/// 操作を実行し、mat スキーマの成功ボディ（timestamp 抜き）を返す。応答は `mat` の
/// one-shot CLI と同じ純粋スキーマで、chip-tool ws の生結果（`results`/`logs`）は
/// 添付しない（logs は backend で除去済み、CLAUDE.md ルール 2「素通し禁止」）。
async fn run_op(
    op: &Op,
    backend: &ChipToolBackend,
    native: Option<&NativeBackend>,
    store_path: &Path,
) -> Result<Value, MatError> {
    // native が有効かつホットパスなら native 経路（M4）。無効時は従来どおり全 op が
    // chip-tool へ通る。
    if let Some(native) = native {
        if is_native_hotpath(op) {
            return native_op(op, native, store_path).await;
        }
        if let Some((group_id, cluster, command, fields)) = native_group_params(op) {
            // chip-tool 経路と同じ前提チェック（store が開けること）。
            let _store = Store::open(store_path)?;
            match native
                .group_invoke(group_id, cluster, command, fields)
                .await?
            {
                crate::native::GroupOutcome::Sent => return Ok(group_sent_body(op)),
                crate::native::GroupOutcome::Unavailable(reason) => {
                    tracing::warn!(
                        %reason,
                        "native group send unavailable; falling back to chip-tool"
                    );
                }
            }
        }
    }
    match op {
        // Ping は chip-tool に触れず即応。
        Op::Ping => Ok(json!({ "pong": true })),
        // Shutdown は chip-tool に触れず即応。serve ループの終了は handle_conn が発火する。
        Op::Shutdown => Ok(json!({ "stopping": true })),
        Op::Describe { node_id } => {
            require_node(store_path, *node_id)?;
            describe(backend, *node_id).await
        }
        Op::GroupProvision { .. } => group_provision(op, backend, store_path).await,
        Op::GroupInvoke { .. } => group_invoke(op, backend, store_path).await,
        Op::GroupColorTemp { .. } | Op::GroupColor { .. } => {
            group_color_op(op, backend, store_path).await
        }
        // 単一 cmdline に展開できる素の op。
        _ => simple_op(op, backend, store_path).await,
    }
}

/// この op を native warm session で処理するか（ホットパス）。それ以外は
/// chip-tool ws にフォールバックする（M4 スコープ）。
pub(crate) fn is_native_hotpath(op: &Op) -> bool {
    match op {
        Op::On { .. } | Op::Off { .. } | Op::Color { .. } | Op::ColorTemp { .. } => true,
        // read は onoff on-off のみ native（汎用 attr 名→ID テーブルは未実装）。
        Op::Read {
            cluster, attribute, ..
        } => cluster == "onoff" && attribute == "on-off",
        _ => false,
    }
}

/// group 送信 op の native 適用判定。native で送れるなら
/// (group_id, cluster_id, command_id, fields) を返す。`GroupInvoke` は
/// onoff の引数なし on/off/toggle のみ（汎用の cluster/command 名→ID
/// テーブルは未実装 — M4 の Read 制限と同型）。None は chip-tool へ。
fn native_group_params(op: &Op) -> Option<(u16, u32, u32, Option<Vec<u8>>)> {
    match op {
        Op::GroupInvoke {
            group_id,
            cluster,
            command,
            args,
            ..
        } if cluster == "onoff" && args.is_empty() => {
            let cmd = match command.as_str() {
                "on" => im::CMD_ON_OFF_ON,
                "off" => im::CMD_ON_OFF_OFF,
                "toggle" => im::CMD_ON_OFF_TOGGLE,
                _ => return None,
            };
            Some((*group_id, im::CLUSTER_ON_OFF, cmd, None))
        }
        Op::GroupColorTemp {
            group_id,
            mireds,
            transition,
            ..
        } => Some((
            *group_id,
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_COLOR_TEMPERATURE,
            Some(im::encode_move_to_color_temperature_fields(
                *mireds,
                *transition,
            )),
        )),
        Op::GroupColor {
            group_id,
            hue_raw,
            saturation_raw,
            transition,
            ..
        } => Some((
            *group_id,
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(im::encode_move_to_hue_and_saturation_fields(
                *hue_raw,
                *saturation_raw,
                *transition,
            )),
        )),
        _ => None,
    }
}

/// native ホットパス op を warm session で実行し、成功 body を組む。
async fn native_op(op: &Op, native: &NativeBackend, store_path: &Path) -> Result<Value, MatError> {
    // commission 済みか毎回 KVS で確認する（chip-tool 経路と同じ挙動）。
    if let Some(node_id) = op.node_id() {
        require_node(store_path, node_id)?;
    }
    match op {
        Op::On { node_id, endpoint } => {
            native.on(*node_id, *endpoint).await?;
            Ok(hotpath_success_body(op, None))
        }
        Op::Off { node_id, endpoint } => {
            native.off(*node_id, *endpoint).await?;
            Ok(hotpath_success_body(op, None))
        }
        Op::Color {
            node_id,
            endpoint,
            hue_raw,
            saturation_raw,
            transition,
            ..
        } => {
            native
                .color(*node_id, *endpoint, *hue_raw, *saturation_raw, *transition)
                .await?;
            Ok(hotpath_success_body(op, None))
        }
        Op::ColorTemp {
            node_id,
            endpoint,
            mireds,
            transition,
            ..
        } => {
            native
                .color_temp(*node_id, *endpoint, *mireds, *transition)
                .await?;
            Ok(hotpath_success_body(op, None))
        }
        Op::Read {
            node_id, endpoint, ..
        } => {
            let v = native.read_onoff(*node_id, *endpoint).await?;
            Ok(hotpath_success_body(op, Some(Value::Bool(v))))
        }
        _ => unreachable!("native_op called with non-hotpath op"),
    }
}

/// native/chip-tool どちらの経路でも使う、ホットパス op の成功 body（timestamp 抜き）。
fn hotpath_success_body(op: &Op, read_value: Option<Value>) -> Value {
    match op {
        Op::On { node_id, endpoint } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "onoff", "command": "on", "status": "success",
        }),
        Op::Off { node_id, endpoint } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "onoff", "command": "off", "status": "success",
        }),
        Op::ColorTemp {
            node_id,
            endpoint,
            mireds,
            kelvin,
            transition,
        } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "colorcontrol", "command": "move-to-color-temperature",
            // 換算後 mireds と入力 kelvin を両方エコー（読み返し突合用; 直経路と同形）。
            "kelvin": kelvin, "mireds": mireds, "transition": transition,
            "status": "success",
        }),
        Op::Color {
            node_id,
            endpoint,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            name,
            rgb,
            transition,
        } => {
            let mut body = json!({
                "node_id": node_id, "endpoint": endpoint,
                "cluster": "colorcontrol", "command": "move-to-hue-and-saturation",
                // 入力の度 / % と換算後 0–254 生値を両方エコー（読み返し突合用; 直経路と同形）。
                "hue": hue, "saturation": saturation,
                "hue_raw": hue_raw, "saturation_raw": saturation_raw,
                "transition": transition,
                "status": "success",
            });
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            if let Some(r) = rgb {
                body["rgb"] = json!(r);
            }
            body
        }
        Op::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => json!({
            "node_id": node_id,
            "endpoint": endpoint,
            "cluster": cluster,
            "attribute": attribute,
            "value": read_value.unwrap_or(Value::Null),
        }),
        _ => unreachable!("hotpath_success_body called with non-hotpath op"),
    }
}

/// 単一 chip-tool コマンドに対応する op（read/write/invoke/on/off）を実行する。
async fn simple_op(
    op: &Op,
    backend: &ChipToolBackend,
    store_path: &Path,
) -> Result<Value, MatError> {
    // node_id が解決できるか（= commission 済みか）を毎回 KVS で確認する。
    if let Some(node_id) = op.node_id() {
        require_node(store_path, node_id)?;
    }

    let cmdline = op.to_cmdline().expect("simple op always has a cmdline");
    let result = backend.run_cmdline(&cmdline).await?;
    ensure_ok(&result)?;

    let body = match op {
        Op::Read {
            cluster, attribute, ..
        } => {
            let value = read_value(&result).ok_or_else(|| {
                MatError::parse_error(format!(
                    "no value in chip-tool ws result for read {cluster}/{attribute}"
                ))
            })?;
            hotpath_success_body(op, Some(value))
        }
        Op::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => json!({
            "node_id": node_id,
            "endpoint": endpoint,
            "cluster": cluster,
            "attribute": attribute,
            // mat write と同じく、入力文字列を read と揃えた型へ正規化して返す。
            "value": normalize_value(value),
            "status": "success",
        }),
        Op::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            ..
        } => json!({
            "node_id": node_id,
            "endpoint": endpoint,
            "cluster": cluster,
            "command": command,
            "status": "success",
        }),
        Op::On { .. } | Op::Off { .. } | Op::ColorTemp { .. } | Op::Color { .. } => {
            hotpath_success_body(op, None)
        }
        Op::Ping
        | Op::Describe { .. }
        | Op::GroupProvision { .. }
        | Op::GroupInvoke { .. }
        | Op::GroupColorTemp { .. }
        | Op::GroupColor { .. }
        | Op::Shutdown => {
            unreachable!("handled by run_op")
        }
    };
    Ok(body)
}

/// `describe` — Descriptor クラスタでノードを introspect する（`mat describe` 相当）。
///
/// エンドポイント 0 の `parts-list` で子エンドポイントを列挙し（0 自身を先頭に足す）、
/// 各エンドポイントの `server-list` でクラスタ ID を読む。warm session なので one-shot
/// の `mat describe` より各読み出しが速い。
async fn describe(backend: &ChipToolBackend, node_id: u64) -> Result<Value, MatError> {
    let parts = descriptor_list(backend, node_id, 0, "parts-list").await?;
    let mut endpoints: Vec<u16> = vec![0];
    for p in parts {
        if let Ok(ep) = u16::try_from(p) {
            if !endpoints.contains(&ep) {
                endpoints.push(ep);
            }
        }
    }

    let mut out_endpoints = Vec::new();
    for ep in endpoints {
        let clusters = descriptor_list(backend, node_id, ep, "server-list").await?;
        out_endpoints.push(json!({ "endpoint": ep, "clusters": clusters }));
    }

    Ok(json!({ "node_id": node_id, "endpoints": out_endpoints }))
}

/// `descriptor read <list> <node> <ep>` を ws で実行し、結果 value から ID 配列を取る。
async fn descriptor_list(
    backend: &ChipToolBackend,
    node_id: u64,
    endpoint: u16,
    list: &str,
) -> Result<Vec<u64>, MatError> {
    let result = backend
        .run_cmdline(&format!("descriptor read {list} {node_id} {endpoint}"))
        .await?;
    ensure_ok(&result)?;
    Ok(id_list(&result))
}

/// `group_provision` — group の鍵束・マッピングを各ノードへ焼き、コントローラ側 group
/// state も設定する（`mat group provision` 相当）。最初の失敗で停止する。
///
/// NOTE: 鍵束 / GroupKeyMap は compact JSON を ws コマンド行に載せて渡す。chip-tool
/// interactive server のトークナイザはこの compact JSON を 1 引数として扱う:
/// key-set-write の compact JSON object は chip-tool ログに `Command:
/// groupkeymanagement key-set-write {"epochKey0":..., "groupKeySetID":77} 5 0` と
/// 丸ごと 1 トークンで渡り解釈される（空白なしが前提）。
async fn group_provision(
    op: &Op,
    backend: &ChipToolBackend,
    store_path: &Path,
) -> Result<Value, MatError> {
    let Op::GroupProvision {
        group_id,
        node_ids,
        keyset_id,
        name,
        endpoint,
        epoch_key,
        rebind,
    } = op
    else {
        unreachable!("group_provision called with non-GroupProvision op");
    };

    let store = Store::open(store_path)?;
    // 全ノードが commission 済みか先に確認（1つでも未登録なら停止）。
    for &node_id in node_ids {
        store.require_node(node_id)?;
    }

    let epoch_key = resolve_epoch_key(epoch_key.as_deref())?;

    // 1) コントローラ側 group state（ローカル操作、ネットワーク不要）。
    group_step(
        backend,
        &format!("groupsettings add-group {name} {group_id}"),
    )
    .await?;
    // chip-tool の add-keysets は `<keysetId> <keyPolicy> <validityTime> <EpochKey>`
    // の4引数（add-group/bind-keyset と違い group 名は取らない。先頭に name を置くと
    // keysetId と誤読し `Invalid argument keysetId` で落ちる）。validityTime は
    // EPOCH_START_TIME（=デバイス側 epochStartTime0）と一致させる。実機 E2E で確定。
    group_step(
        backend,
        &format!(
            "groupsettings add-keysets {keyset_id} {KEY_SECURITY_POLICY} {EPOCH_START_TIME} hex:{epoch_key}"
        ),
    )
    .await?;
    if *rebind {
        // 既存グループの keyset binding を解除してから bind し直す（issue #5）。
        // best-effort: 「未 bind なのに unbind」を区別せず失敗を無視する（unbind が
        // 本当に必要で失敗したケースは直後の bind-keyset が従来どおり落ちるので、
        // 検知はそちらに委ねる）。
        if let Err(e) = group_step(
            backend,
            &format!("groupsettings unbind-keyset {group_id} {keyset_id}"),
        )
        .await
        {
            tracing::debug!(
                group_id,
                keyset_id,
                error = %e.detail,
                "groupsettings unbind-keyset failed; ignored (best-effort rebind)"
            );
        }
    }
    group_step(
        backend,
        &format!("groupsettings bind-keyset {group_id} {keyset_id}"),
    )
    .await?;

    // 2) 各デバイスへ provision（unicast, acknowledged）。
    for &node_id in node_ids {
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
        group_step(
            backend,
            &format!("groupkeymanagement key-set-write {key_set} {node_id} 0"),
        )
        .await?;

        let key_map = json!([{ "groupId": group_id, "groupKeySetID": keyset_id }]);
        group_step(
            backend,
            &format!("groupkeymanagement write group-key-map {key_map} {node_id} 0"),
        )
        .await?;

        group_step(
            backend,
            &format!("groups add-group {group_id} {name} {node_id} {endpoint}"),
        )
        .await?;

        // 4) ACL: groupcast は authMode=Group で届くため、Group エントリが無いと
        //    デバイスが黙って捨てる。read-merge-write（write は全置換なので
        //    「read できたリスト + 追記」のみ。read 解釈不能なら write しない）。
        let result = backend
            .run_cmdline(&format!("accesscontrol read acl {node_id} 0"))
            .await?;
        ensure_ok(&result)?;
        let value = read_value(&result).ok_or_else(|| {
            MatError::parse_error(format!(
                "no value in chip-tool ws result for acl read on node {node_id}"
            ))
        })?;
        let entries = acl_entries_from_ws_value(&value).map_err(|e| {
            MatError::new(e.kind, format!("acl read on node {node_id}: {}", e.detail))
        })?;
        if let Some(merged) = merge_group_entry(&entries, *group_id) {
            group_step(
                backend,
                &format!(
                    "accesscontrol write acl {} {node_id} 0",
                    to_chip_write_json(&merged)
                ),
            )
            .await?;
        }
    }

    Ok(json!({
        "group_id": group_id,
        "keyset_id": keyset_id,
        "name": name,
        "endpoint": endpoint,
        "nodes": node_ids,
        "status": "provisioned",
    }))
}

/// group provision の 1 ステップ。失敗（results 内エラー）なら分類して返す。
async fn group_step(backend: &ChipToolBackend, line: &str) -> Result<(), MatError> {
    let result = backend.run_cmdline(line).await?;
    ensure_ok(&result)
}

/// `group_invoke` — group へ multicast でコマンドを送る（`mat group invoke` 相当）。
///
/// groupcast は unacknowledged。応答（per-device の成否）は取れないため、ws 応答が
/// 返れば「送った」とだけ報告する（`ensure_ok` はしない）。
async fn group_invoke(
    op: &Op,
    backend: &ChipToolBackend,
    store_path: &Path,
) -> Result<Value, MatError> {
    let Op::GroupInvoke {
        group_id,
        cluster,
        command,
        args,
        endpoint,
    } = op
    else {
        unreachable!("group_invoke called with non-GroupInvoke op");
    };

    // 特定 node 宛ではないが、chip-tool の永続ストレージ（焼いた group 鍵）参照のため
    // store は必要。
    let _store = Store::open(store_path)?;

    // invoke と同じ並び: `<cluster> <command> [args...] <宛先> <endpoint>`。宛先に
    // group node-id を置くと chip-tool が multicast 送信する。
    let mut parts = vec![cluster.clone(), command.clone()];
    parts.extend(args.iter().cloned());
    parts.push(group_node_id(*group_id));
    parts.push(endpoint.to_string());
    let _ = backend.run_cmdline(&parts.join(" ")).await?;

    Ok(group_sent_body(op))
}

/// group 送信 op（`GroupInvoke`/`GroupColorTemp`/`GroupColor`）の成功 body。
/// chip-tool 経路（[`group_invoke`]/[`group_color_op`]）と native 経路
/// （`run_op` の native group 分岐）の両方から呼ぶ — 応答スキーマは経路に
/// よらず同一（DRY、CLAUDE.md ルール「素通し禁止」とは別に schema 安定が要件）。
fn group_sent_body(op: &Op) -> Value {
    match op {
        Op::GroupInvoke {
            group_id,
            cluster,
            command,
            endpoint,
            ..
        } => json!({
            "group_id": group_id,
            "cluster": cluster,
            "command": command,
            "endpoint": endpoint,
            "status": "sent",
            "note": "unacknowledged groupcast; per-device delivery not confirmed",
        }),
        Op::GroupColorTemp {
            group_id,
            mireds,
            kelvin,
            transition,
            endpoint,
        } => json!({
            "group_id": group_id, "cluster": "colorcontrol",
            "command": "move-to-color-temperature",
            "kelvin": kelvin, "mireds": mireds, "transition": transition,
            "endpoint": endpoint, "status": "sent",
            "note": "unacknowledged groupcast; per-device delivery not confirmed",
        }),
        Op::GroupColor {
            group_id,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            name,
            rgb,
            transition,
            endpoint,
        } => {
            let mut body = json!({
                "group_id": group_id, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": hue, "saturation": saturation,
                "hue_raw": hue_raw, "saturation_raw": saturation_raw,
                "transition": transition, "endpoint": endpoint,
                "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            });
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            if let Some(r) = rgb {
                body["rgb"] = json!(r);
            }
            body
        }
        _ => unreachable!("group_sent_body called with non group-send op"),
    }
}

/// group 版 color-temp / color ショートカット（`mat group color-temp` / `mat group
/// color` 相当）。groupcast なので group_invoke と同じく unacknowledged（ws 応答が
/// 返れば "sent"）。換算は mat 側で済んでおり、ここは送信とエコーのみ。
async fn group_color_op(
    op: &Op,
    backend: &ChipToolBackend,
    store_path: &Path,
) -> Result<Value, MatError> {
    // 特定 node 宛ではないが、chip-tool の永続ストレージ（焼いた group 鍵）参照の
    // ため store は必要。
    let _store = Store::open(store_path)?;
    match op {
        Op::GroupColorTemp {
            group_id,
            mireds,
            transition,
            endpoint,
            ..
        } => {
            let line = format!(
                "colorcontrol move-to-color-temperature {mireds} {transition} 0 0 {} {endpoint}",
                group_node_id(*group_id)
            );
            let _ = backend.run_cmdline(&line).await?;
            Ok(group_sent_body(op))
        }
        Op::GroupColor {
            group_id,
            hue_raw,
            saturation_raw,
            transition,
            endpoint,
            ..
        } => {
            let line = format!(
                "colorcontrol move-to-hue-and-saturation {hue_raw} {saturation_raw} {transition} 0 0 {} {endpoint}",
                group_node_id(*group_id)
            );
            let _ = backend.run_cmdline(&line).await?;
            Ok(group_sent_body(op))
        }
        _ => unreachable!("group_color_op called with non group color op"),
    }
}

/// store を開いて node_id が commission 済みか確認する（常駐中の台帳更新を拾うよう
/// 毎回開き直す）。
fn require_node(store_path: &Path, node_id: u64) -> Result<(), MatError> {
    Store::open(store_path)?.require_node(node_id)?;
    Ok(())
}

/// chip-tool ws 結果 `results[0].value` を取り出す（実機 E2E で確定済みの形状）。
fn read_value(result: &Value) -> Option<Value> {
    result
        .get("results")
        .and_then(|r| r.get(0))
        .and_then(|e| e.get("value"))
        .cloned()
}

/// `results[0].value` を ID 配列（u64）として読む。descriptor の list 属性用。
fn id_list(result: &Value) -> Vec<u64> {
    read_value(result)
        .as_ref()
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// chip-tool ws 結果にデバイス側エラーが載っていれば分類して `Err` にする。
///
/// 成功時 `results[i]` には `error` が無いか success ステータスが入る。失敗時の
/// `error` 値（status 名/コード）を既存のテキスト分類 [`classify_failure`] に通し、
/// 未知なら `device_rejected` にフォールバックする。
///
/// 失敗形状: 失敗した groupkeymanagement key-set-write は
/// `{"results":[{"error":"FAILURE"}],"logs":[...]}` を返す。`error` は status 名の
/// **文字列**（数値ではない）、キー名は `error`。`"FAILURE"` は `classify_failure`
/// 未一致のため `device_rejected` に落ちる。
///
/// `FAILURE` の正体が discovery/resolution timeout のことがある（実体は探索段階で
/// 落ちている）。そのシグナル（`CHIP Error 0x00000032` 等）は results ではなく ws の
/// `logs` にしか出ないため、backend が落とす前にデコードして添えた `diag` を
/// 分類器の stderr 側へ合流させ、直叩き経路と分類を一致させる（#1）。
fn ensure_ok(result: &Value) -> Result<(), MatError> {
    let Some(arr) = result.get("results").and_then(Value::as_array) else {
        return Ok(());
    };
    // backend が logs から抽出した分類用テキスト（無ければ空）。
    let diag = result.get("diag").and_then(Value::as_str).unwrap_or("");
    for entry in arr {
        if let Some(err) = entry.get("error") {
            if is_success_status(err) {
                continue;
            }
            // 文字列なら status 名そのもの（例 `FAILURE`）を detail に使う。JSON の
            // クオートを残さないことで AI 可読性を上げる（数値等は JSON 表現のまま）。
            let detail = err
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| err.to_string());
            let kind = classify_failure(&detail, diag).unwrap_or(ErrorKind::DeviceRejected);
            return Err(MatError::new(
                kind,
                format!("chip-tool reported an error in the result: {detail}"),
            ));
        }
    }
    Ok(())
}

/// `error` フィールドが「成功」を意味するか（null / 0 / "SUCCESS"）。
fn is_success_status(err: &Value) -> bool {
    match err {
        Value::Null => true,
        Value::Number(n) => n.as_u64() == Some(0),
        Value::String(s) => {
            let u = s.to_ascii_uppercase();
            matches!(u.as_str(), "0" | "0X0" | "0X00" | "SUCCESS")
        }
        _ => false,
    }
}

/// エラー応答 `{"error":{"kind","detail"}, "id"?, "timestamp"}`。
fn error_response(id: Option<Value>, e: &MatError) -> Value {
    let mut body = json!({
        "error": { "kind": e.kind, "detail": e.detail },
        "timestamp": now_iso8601(),
    });
    if let (Value::Object(map), Some(id)) = (&mut body, id) {
        map.insert("id".into(), id);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Op;

    #[test]
    fn hotpath_routing_selects_native_ops() {
        // native で処理するホットパス。
        assert!(is_native_hotpath(&Op::On {
            node_id: 1,
            endpoint: 1
        }));
        assert!(is_native_hotpath(&Op::Off {
            node_id: 1,
            endpoint: 1
        }));
        assert!(is_native_hotpath(&Op::ColorTemp {
            node_id: 1,
            endpoint: 1,
            mireds: 370,
            kelvin: 2700,
            transition: 0
        }));
        assert!(is_native_hotpath(&Op::Color {
            node_id: 1,
            endpoint: 1,
            hue_raw: 0,
            saturation_raw: 254,
            hue: 0,
            saturation: 100,
            name: None,
            rgb: None,
            transition: 0
        }));
        // onoff on-off の read だけ native。
        assert!(is_native_hotpath(&Op::Read {
            node_id: 1,
            endpoint: 1,
            cluster: "onoff".into(),
            attribute: "on-off".into()
        }));
    }

    #[test]
    fn hotpath_routing_leaves_others_to_chip_tool() {
        // 別 cluster/attr の read は chip-tool へ。
        assert!(!is_native_hotpath(&Op::Read {
            node_id: 1,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "current-level".into()
        }));
        assert!(!is_native_hotpath(&Op::Write {
            node_id: 1,
            endpoint: 1,
            cluster: "onoff".into(),
            attribute: "on-off".into(),
            value: "1".into()
        }));
        assert!(!is_native_hotpath(&Op::Describe { node_id: 1 }));
        assert!(!is_native_hotpath(&Op::Invoke {
            node_id: 1,
            endpoint: 1,
            cluster: "identify".into(),
            command: "identify".into(),
            args: vec![]
        }));
        assert!(!is_native_hotpath(&Op::GroupInvoke {
            group_id: 1,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec![],
            endpoint: 1
        }));
        assert!(!is_native_hotpath(&Op::Ping));
    }

    #[test]
    fn ensure_ok_passes_on_empty_results() {
        // controller groupsettings 成功時の実機形（add-group/bind-keyset）。
        let v = json!({ "results": [], "logs": [] });
        assert!(ensure_ok(&v).is_ok());
    }

    #[test]
    fn ensure_ok_passes_on_success_statuses() {
        for ok in [json!(null), json!(0), json!("0x0"), json!("SUCCESS")] {
            let v = json!({ "results": [{ "error": ok }] });
            assert!(ensure_ok(&v).is_ok(), "expected success for {v}");
        }
    }

    #[test]
    fn ensure_ok_classifies_real_device_failure_shape() {
        // 失敗 ws 形状: 失敗した key-set-write が `error` に status 名の**文字列**を返す。
        let v = json!({ "results": [{ "error": "FAILURE" }], "logs": [] });
        let err = ensure_ok(&v).expect_err("FAILURE must be an error");
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        // 文字列値はクオートを剥がして detail に入れる。
        assert!(
            err.detail.contains("FAILURE") && !err.detail.contains("\"FAILURE\""),
            "detail should carry bare status name, got: {}",
            err.detail
        );
    }

    #[test]
    fn ensure_ok_reclassifies_failure_with_discovery_timeout_diag() {
        // #1: matd 経由でも、results の `error` が汎用 `FAILURE` のときは backend が
        // logs から抽出した分類用テキスト `diag` を参照し、discovery timeout を
        // device_rejected ではなく timeout に分類する（直叩きと一致させる）。
        let v = json!({
            "results": [{ "error": "FAILURE" }],
            "diag": "[DIS] operational discovery failed: \
                     AddressResolve_DefaultImpl.cpp:124: CHIP Error 0x00000032: Timeout",
        });
        let err = ensure_ok(&v).expect_err("FAILURE must be an error");
        assert_eq!(err.kind, ErrorKind::Timeout);
    }

    #[test]
    fn ensure_ok_keeps_device_rejected_without_diag_signal() {
        // diag があっても探索/解決の失敗シグナルが無ければ従来どおり device_rejected。
        let v = json!({
            "results": [{ "error": "FAILURE" }],
            "diag": "[IM] some unrelated chatter",
        });
        let err = ensure_ok(&v).expect_err("FAILURE must be an error");
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
    }

    #[test]
    fn native_group_params_maps_onoff_and_shortcuts() {
        let on = Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec![],
            endpoint: 1,
        };
        let (gid, cluster, command, fields) = native_group_params(&on).unwrap();
        assert_eq!(
            (gid, cluster, command),
            (10, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON)
        );
        assert!(fields.is_none());

        // 引数付き・onoff 以外・未知コマンドは native 対象外（chip-tool へ）。
        let with_args = Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec!["1".into()],
            endpoint: 1,
        };
        assert!(native_group_params(&with_args).is_none());
        let other_cluster = Op::GroupInvoke {
            group_id: 10,
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec![],
            endpoint: 1,
        };
        assert!(native_group_params(&other_cluster).is_none());

        let ct = Op::GroupColorTemp {
            group_id: 10,
            mireds: 370,
            kelvin: 2702,
            transition: 0,
            endpoint: 1,
        };
        let (_, cluster, command, fields) = native_group_params(&ct).unwrap();
        assert_eq!(cluster, im::CLUSTER_COLOR_CONTROL);
        assert_eq!(command, im::CMD_MOVE_TO_COLOR_TEMPERATURE);
        assert_eq!(
            fields.unwrap(),
            im::encode_move_to_color_temperature_fields(370, 0)
        );

        let color = Op::GroupColor {
            group_id: 10,
            hue_raw: 180,
            saturation_raw: 200,
            hue: 254,
            saturation: 78,
            name: None,
            rgb: None,
            transition: 0,
            endpoint: 1,
        };
        let (_, cluster, command, fields) = native_group_params(&color).unwrap();
        assert_eq!(cluster, im::CLUSTER_COLOR_CONTROL);
        assert_eq!(command, im::CMD_MOVE_TO_HUE_AND_SATURATION);
        assert_eq!(
            fields.unwrap(),
            im::encode_move_to_hue_and_saturation_fields(180, 200, 0)
        );

        // GroupProvision は常に chip-tool。
        assert!(native_group_params(&Op::Ping).is_none());
    }

    use crate::native::test_support::{write_group_fixture_ini, FakeEstablisher};
    use std::path::PathBuf;

    fn group_on_op() -> Op {
        Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec![],
            endpoint: 1,
        }
    }

    /// 接続先の無い lazy backend（触られたら必ず接続エラー）。
    async fn dead_backend() -> ChipToolBackend {
        ChipToolBackend::connect(1, std::time::Duration::from_secs(30))
            .await
            .unwrap()
    }

    fn make_store() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 1,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-06-08T00:00:00+09:00".into(),
            })
            .unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[tokio::test]
    async fn group_op_routes_native_when_available() {
        let (_dir, store_path) = make_store();
        let ini = store_path.join("chip_tool_config.ini");
        write_group_fixture_ini(&ini);

        // `lo` lacks IFF_MULTICAST; reuse the runtime interface-discovery
        // helper shared with native.rs's own multicast test.
        let mut sent = false;
        for cand in crate::native::test_support::multicast_capable_interfaces() {
            let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
            let port = recv.local_addr().unwrap().port();
            if recv
                .join_multicast_v6(
                    &mat_controller::group::group_multicast_addr(1, 10),
                    cand.index,
                )
                .is_err()
            {
                continue;
            }
            let transport = std::sync::Arc::new(
                mat_controller::transport::UdpTransport::bind()
                    .await
                    .unwrap(),
            );
            let ctx = crate::native::GroupCtx {
                main_ini: ini.clone(),
                counter_path: store_path.join(format!("native_group_counter-{}", cand.index)),
                fabric_index: 2,
                fabric_id: 1,
                node_id: 0x0001_0001,
                scope_id: cand.index,
                dest_port: port,
                transport,
                sender: tokio::sync::Mutex::new(None),
            };
            let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), Some(ctx));
            let backend = dead_backend().await;

            let body = run_op(&group_on_op(), &backend, Some(&native), &store_path)
                .await
                .unwrap();
            assert_eq!(body["status"], "sent"); // native 経路で chip-tool 不要のまま成功
            let mut buf = [0u8; 1280];
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                recv.recv_from(&mut buf),
            )
            .await;
            if result.is_ok() {
                sent = true;
                break;
            }
        }
        assert!(
            sent,
            "no multicast-capable interface delivered the groupcast datagram \
             (lo excluded — it lacks IFF_MULTICAST on Linux)"
        );
    }

    #[tokio::test]
    async fn group_op_falls_back_to_chip_tool_when_unavailable() {
        let (_dir, store_path) = make_store();
        // group ctx なしの native → Unavailable → chip-tool 経路へ。dead backend が
        // エラーを返すこと自体が「フォールバックが試みられた」証拠。
        let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), None);
        let backend = dead_backend().await;
        let err = run_op(&group_on_op(), &backend, Some(&native), &store_path)
            .await
            .unwrap_err();
        assert_ne!(
            err.kind,
            ErrorKind::Unreachable,
            "native 送出エラーではなく chip-tool 接続系のエラーになる"
        );
    }
}
