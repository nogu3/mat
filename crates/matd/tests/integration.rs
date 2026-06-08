//! matd 統合テスト。実 chip-tool は使わず、chip-tool の ws サーバを模した
//! fake echo サーバを立て、unix socket → matd → ws → 応答の往復を検証する。
//!
//! 実 chip-tool に依存しないので CI で常時走る。実機相手の CASE 確立や ws 結果
//! JSON の確定は別途 E2E（Phase 4 後続）で行う。

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use mat_core::store::{NodeRecord, Store};

/// chip-tool interactive server を模した fake。受け取ったコマンド行をエコーし、
/// `results[0].value = true` を載せた JSON を 1 メッセージで返す。
async fn spawn_fake_ws() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(line) = msg {
                        let resp = json!({ "cmd": line, "results": [{ "value": true }] });
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    port
}

/// store を tempdir に作り、node 1 を commission 済みにして path を返す。
fn make_store() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_or_init(dir.path()).unwrap();
    store
        .upsert_node(NodeRecord {
            node_id: 1,
            address: Some("192.0.2.10".into()),
            commissioned_at: "2026-06-08T00:00:00+09:00".into(),
        })
        .unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

/// 1 接続で複数リクエスト行を送り、各行の応答 JSON を順に返す。
async fn roundtrip(socket: &std::path::Path, requests: &[Value]) -> Vec<Value> {
    // serve が bind するまで少し待つ。
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(socket).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stream = stream.expect("could not connect to matd socket");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let mut out = Vec::new();
    for req in requests {
        let mut line = serde_json::to_vec(req).unwrap();
        line.push(b'\n');
        write_half.write_all(&line).await.unwrap();
        let resp = lines.next_line().await.unwrap().expect("no response line");
        out.push(serde_json::from_str(&resp).unwrap());
    }
    out
}

/// matd の serve をバックグラウンドで起動し、socket path を返す。
async fn start_matd(store_path: PathBuf, port: u16) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let backend = matd_backend_connect(port).await;
    let socket = std::env::temp_dir().join(format!("matd-test-{}.sock", rand_suffix()));
    let socket_clone = socket.clone();
    let handle = tokio::spawn(async move {
        matd_serve(&socket_clone, store_path, backend).await;
    });
    (socket, handle)
}

// テストから matd の内部を叩くための薄いラッパ（crate 内 API を再公開していないため、
// バイナリ crate の関数はテストから直接見えない → ここで同等の起動経路を組む）。
// 実体は matd の lib 経由で呼ぶ。
use std::sync::Arc;
async fn matd_backend_connect(port: u16) -> Arc<matd::backend::ChipToolBackend> {
    Arc::new(matd::backend::ChipToolBackend::connect(port).await.unwrap())
}
async fn matd_serve(
    socket: &std::path::Path,
    store_path: PathBuf,
    backend: Arc<matd::backend::ChipToolBackend>,
) {
    let _ = matd::server::serve(socket, store_path, backend).await;
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

#[tokio::test]
async fn read_invoke_ping_and_errors() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[
            json!({"id":1,"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"}),
            json!({"id":2,"op":"on","node_id":1,"endpoint":1}),
            json!({"op":"ping"}),
            json!({"op":"read","node_id":99,"endpoint":1,"cluster":"onoff","attribute":"on-off"}),
        ],
    )
    .await;

    // read: value をベストエフォート抽出し、id/timestamp/result を載せる。
    let r = &resps[0];
    assert_eq!(r["id"], json!(1));
    assert_eq!(r["node_id"], json!(1));
    assert_eq!(r["cluster"], "onoff");
    assert_eq!(r["value"], json!(true));
    assert_eq!(r["result"]["cmd"], "onoff read on-off 1 1");
    assert!(r["timestamp"].is_string());

    // on: OnOff invoke にマップされ、cmdline は `onoff on 1 1`。
    let r = &resps[1];
    assert_eq!(r["id"], json!(2));
    assert_eq!(r["status"], "success");
    assert_eq!(r["command"], "on");
    assert_eq!(r["result"]["cmd"], "onoff on 1 1");

    // ping: chip-tool に触れず即応。
    assert_eq!(resps[2]["pong"], json!(true));

    // 未 commission node: node_not_commissioned エラー。
    assert_eq!(resps[3]["error"]["kind"], "node_not_commissioned");

    handle.abort();
}

#[tokio::test]
async fn invalid_request_json_is_parse_error() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    // 生の壊れた行を送る（roundtrip は valid JSON 前提なので直接書く）。
    let stream = {
        let mut s = None;
        for _ in 0..50 {
            if let Ok(c) = UnixStream::connect(&socket).await {
                s = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        s.unwrap()
    };
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    write_half.write_all(b"{ not json\n").await.unwrap();
    let resp: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(resp["error"]["kind"], "parse_error");

    handle.abort();
}
