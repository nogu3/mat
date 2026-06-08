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

use mat_core::error::MatError;
use mat_core::output::now_iso8601;
use mat_core::parse::normalize_value;
use mat_core::store::Store;

use crate::backend::ChipToolBackend;
use crate::protocol::{Op, Request};

/// ソケットを bind し、接続を受け付け続ける。`Ctrl-C` で抜ける。
pub async fn serve(
    socket_path: &Path,
    store_path: PathBuf,
    backend: Arc<ChipToolBackend>,
) -> std::io::Result<()> {
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

    let store_path = Arc::new(store_path);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted?;
                let backend = Arc::clone(&backend);
                let store_path = Arc::clone(&store_path);
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, backend, store_path).await {
                        tracing::warn!(error = %e, "connection handler ended with error");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl-C, shutting down");
                break;
            }
        }
    }

    // graceful shutdown: reaper を止め、chip-tool セッションを畳み、socket を消す。
    reaper.abort();
    backend.shutdown().await;
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// 1 接続。複数行のリクエストを順に処理し、各行に 1 行 JSON で応答する。
async fn handle_conn(
    stream: UnixStream,
    backend: Arc<ChipToolBackend>,
    store_path: Arc<PathBuf>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = dispatch(&line, &backend, &store_path).await;
        let mut buf = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
        buf.push(b'\n');
        write_half.write_all(&buf).await?;
    }
    Ok(())
}

/// 1 リクエスト行を処理して応答 JSON を組み立てる。
async fn dispatch(line: &str, backend: &ChipToolBackend, store_path: &Path) -> Value {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                None,
                &MatError::parse_error(format!("invalid request JSON: {e}")),
            )
        }
    };
    let id = req.id.clone();

    match run_op(&req.op, backend, store_path).await {
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
    }
}

/// 操作を実行し、mat スキーマの成功ボディ（timestamp 抜き）を返す。
async fn run_op(op: &Op, backend: &ChipToolBackend, store_path: &Path) -> Result<Value, MatError> {
    // Ping は chip-tool に触れず即応。
    if matches!(op, Op::Ping) {
        return Ok(json!({ "pong": true }));
    }

    // node_id が解決できるか（= commission 済みか）を毎回 KVS で確認する。
    if let Some(node_id) = op.node_id() {
        let store = Store::open(store_path)?;
        store.require_node(node_id)?;
    }

    let cmdline = op.to_cmdline().expect("non-Ping ops always have a cmdline");
    let result = backend.run_cmdline(&cmdline).await?;

    // NOTE: chip-tool ws の結果 JSON 構造は実機 E2E で確定する。確定するまでは生結果を
    // `result` に添付し、read の `value` はベストエフォート抽出にとどめる。
    let body = match op {
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
            "value": pick_read_value(&result),
            "result": result,
        }),
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
            "result": result,
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
            "result": result,
        }),
        Op::On { node_id, endpoint } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "onoff", "command": "on", "status": "success", "result": result,
        }),
        Op::Off { node_id, endpoint } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "onoff", "command": "off", "status": "success", "result": result,
        }),
        Op::Ping => unreachable!("handled above"),
    };
    Ok(body)
}

/// chip-tool ws 結果から read 値をベストエフォート抽出する（実機 E2E で確定予定）。
fn pick_read_value(result: &Value) -> Value {
    result
        .get("results")
        .and_then(|r| r.get(0))
        .and_then(|e| e.get("value"))
        .cloned()
        .unwrap_or(Value::Null)
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
