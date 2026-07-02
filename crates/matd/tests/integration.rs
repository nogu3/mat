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

/// chip-tool interactive server を模した fake。受け取ったコマンド行に応じて
/// `results[0].value` を返す。冗長な `logs` も載せ、matd がそれを応答から落とすことを
/// 検証できるようにする。
/// - `descriptor read parts-list ...` → 子エンドポイント `[1]`
/// - `descriptor read server-list ...` → クラスタ `[6, 8]`（onoff, levelcontrol）
/// - それ以外 → `true`
async fn spawn_fake_ws() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(line) = msg {
                        let value = if line.contains("descriptor read parts-list") {
                            json!([1])
                        } else if line.contains("descriptor read server-list") {
                            json!([6, 8])
                        } else {
                            json!(true)
                        };
                        let resp = json!({
                            "cmd": line,
                            "results": [{ "value": value }],
                            "logs": ["dis9hcnt"],
                        });
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    port
}

/// オペレーショナル探索の timeout を模した fake。実機 chip-tool 同様、結果は汎用
/// `{"error":"FAILURE"}` だが、discovery timeout シグナルは base64 でくるんだ `logs`
/// にしか出ない（#1 の再現形状）。matd がこの logs を分類に活かせるか検証する。
async fn spawn_fake_ws_discovery_timeout() -> u16 {
    use base64::Engine as _;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(_line) = msg {
                        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
                        let resp = json!({
                            "results": [{ "error": "FAILURE" }],
                            "logs": [
                                b64("[DIS] Timeout waiting for mDNS resolution."),
                                b64("[DIS] operational discovery failed: \
                                     AddressResolve_DefaultImpl.cpp:124: \
                                     CHIP Error 0x00000032: Timeout"),
                            ],
                        });
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
    // テスト中に畳まれないよう idle は長めに。
    Arc::new(
        matd::backend::ChipToolBackend::connect(port, Duration::from_secs(300))
            .await
            .unwrap(),
    )
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
            json!({"id":3,"op":"write","node_id":1,"endpoint":1,"cluster":"levelcontrol","attribute":"on-level","value":"128"}),
            json!({"op":"ping"}),
            json!({"op":"read","node_id":99,"endpoint":1,"cluster":"onoff","attribute":"on-off"}),
        ],
    )
    .await;

    // read: results[0].value を取り出し、id/timestamp を載せる。生結果（result/logs）は
    // 素通ししない（mat スキーマと同形）。
    let r = &resps[0];
    assert_eq!(r["id"], json!(1));
    assert_eq!(r["node_id"], json!(1));
    assert_eq!(r["cluster"], "onoff");
    assert_eq!(r["value"], json!(true));
    assert!(r.get("result").is_none(), "raw ws result must not leak");
    assert!(r.get("logs").is_none(), "chip-tool logs must be dropped");
    assert!(r["timestamp"].is_string());

    // on: OnOff invoke にマップされ status success（cmdline 検証は protocol unit test）。
    let r = &resps[1];
    assert_eq!(r["id"], json!(2));
    assert_eq!(r["status"], "success");
    assert_eq!(r["command"], "on");
    assert!(r.get("result").is_none());

    // write: 入力 "128" を read と揃えた数値型へ正規化して返す。
    let r = &resps[2];
    assert_eq!(r["id"], json!(3));
    assert_eq!(r["status"], "success");
    assert_eq!(r["value"], json!(128));
    assert!(r.get("result").is_none());

    // ping: chip-tool に触れず即応。
    assert_eq!(resps[3]["pong"], json!(true));

    // 未 commission node: node_not_commissioned エラー。
    assert_eq!(resps[4]["error"]["kind"], "node_not_commissioned");

    handle.abort();
}

/// #1: matd 経由でも、汎用 `FAILURE` の正体が discovery timeout なら（logs に
/// `CHIP Error 0x00000032`）`timeout` に分類する。直叩き経路と一致させ、device_rejected
/// への誤分類を防ぐ。
#[tokio::test]
async fn discovery_timeout_failure_is_classified_as_timeout() {
    let port = spawn_fake_ws_discovery_timeout().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({"id":1,"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"})],
    )
    .await;

    assert_eq!(
        resps[0]["error"]["kind"], "timeout",
        "discovery timeout must not be misclassified as device_rejected: {}",
        resps[0]
    );
    // logs は応答に素通ししない（CLAUDE.md ルール2）。
    assert!(resps[0].get("logs").is_none());
    assert!(
        resps[0].get("diag").is_none(),
        "internal diag must not leak"
    );

    handle.abort();
}

/// アイドル畳み込み後にコマンドが来たら ws を張り直す（Connect モードは再接続のみ）。
#[tokio::test]
async fn idle_teardown_then_reconnect() {
    let port = spawn_fake_ws().await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_millis(150))
        .await
        .unwrap();

    let v1 = backend.run_cmdline("first cmd").await.unwrap();
    assert_eq!(v1["cmd"], "first cmd");

    // アイドル基準を超えてから reaper 相当を呼ぶ → セッションが畳まれる。
    tokio::time::sleep(Duration::from_millis(220)).await;
    backend.reap_if_idle().await;

    // 次コマンドで遅延再接続され、fake-ws の 2 本目の接続で応答が返る。
    let v2 = backend.run_cmdline("after reconnect").await.unwrap();
    assert_eq!(v2["cmd"], "after reconnect");
}

/// color_temp: ColorControl MoveToColorTemperature にマップされ、mireds / kelvin /
/// transition を応答へエコーする（直経路 `mat color-temp` と同形）。
#[tokio::test]
async fn color_temp_echoes_kelvin_and_mireds() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[
            json!({"id":1,"op":"color_temp","node_id":1,"endpoint":1,"mireds":370,"kelvin":2700,"transition":30}),
            json!({"op":"color_temp","node_id":99,"endpoint":1,"mireds":370,"kelvin":2700}),
        ],
    )
    .await;

    let r = &resps[0];
    assert_eq!(r["id"], json!(1));
    assert_eq!(r["cluster"], "colorcontrol");
    assert_eq!(r["command"], "move-to-color-temperature");
    assert_eq!(r["kelvin"], json!(2700));
    assert_eq!(r["mireds"], json!(370));
    assert_eq!(r["transition"], json!(30));
    assert_eq!(r["status"], "success");
    assert!(r.get("result").is_none(), "raw ws result must not leak");

    // 未 commission node は他 op 同様 node_not_commissioned。
    assert_eq!(resps[1]["error"]["kind"], "node_not_commissioned");

    handle.abort();
}

/// describe: parts-list → 子エンドポイント、各 ep の server-list → クラスタ ID を組む。
#[tokio::test]
async fn describe_builds_endpoints_from_descriptor() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(&socket, &[json!({"id":9,"op":"describe","node_id":1})]).await;
    let r = &resps[0];
    assert_eq!(r["id"], json!(9));
    assert_eq!(r["node_id"], json!(1));

    // fake は parts-list=[1] を返す → endpoints は 0（自身）と 1。各 ep の server-list=[6,8]。
    let eps = r["endpoints"].as_array().unwrap();
    assert_eq!(eps.len(), 2);
    assert_eq!(eps[0]["endpoint"], json!(0));
    assert_eq!(eps[0]["clusters"], json!([6, 8]));
    assert_eq!(eps[1]["endpoint"], json!(1));
    assert_eq!(eps[1]["clusters"], json!([6, 8]));
    assert!(r.get("result").is_none());

    handle.abort();
}

/// group invoke: unacknowledged なので応答が返れば status="sent" を報告する。
#[tokio::test]
async fn group_invoke_reports_sent() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({"op":"group_invoke","group_id":1,"cluster":"onoff","command":"on","endpoint":1})],
    )
    .await;
    assert_eq!(resps[0]["status"], "sent");
    assert_eq!(resps[0]["group_id"], json!(1));
    assert_eq!(resps[0]["command"], "on");

    handle.abort();
}

/// group provision: 全ステップが results にエラーを返さなければ provisioned を報告する。
#[tokio::test]
async fn group_provision_reports_provisioned() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision",
            "group_id":1,
            "node_ids":[1],
            "keyset_id":42,
            "name":"living",
            "endpoint":1,
            // 乱数を避け cmdline を決定的にするため epoch key を明示。
            "epoch_key":"00112233445566778899aabbccddeeff"
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned");
    assert_eq!(resps[0]["nodes"], json!([1]));
    assert_eq!(resps[0]["keyset_id"], json!(42));

    handle.abort();
}

/// group provision は未 commission node を含むと node_not_commissioned で止まる。
#[tokio::test]
async fn group_provision_rejects_uncommissioned_node() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision",
            "group_id":1,
            "node_ids":[1, 99],
            "keyset_id":42,
            "name":"living",
            "endpoint":1
        })],
    )
    .await;
    assert_eq!(resps[0]["error"]["kind"], "node_not_commissioned");

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

/// `matd stop` 相当: shutdown op を送ると `{"stopping":true}` が返り、serve ループが
/// 自然終了する（abort ではなく JoinHandle が完了する）。
#[tokio::test]
async fn shutdown_op_stops_server() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(&socket, &[json!({"id":1,"op":"shutdown"})]).await;
    assert_eq!(resps[0]["stopping"], json!(true));
    assert_eq!(resps[0]["id"], json!(1));

    // serve ループが break して JoinHandle が完了する。
    let ended = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(ended.is_ok(), "serve did not shut down after shutdown op");
}
