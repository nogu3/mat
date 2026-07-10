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
                        } else if line.contains("accesscontrol read acl") {
                            // 実機の数値キー形式（admin エントリのみ = ACL 未設定）。
                            json!([{"1":5,"2":2,"3":[112233],"4":null,"254":1}])
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

/// コマンド行を記録する fake ws。`accesscontrol read acl` には `acl_value` を返し、
/// それ以外は `true`。group_provision の ACL ステップ（read → 条件付き write）の
/// コマンド列を検証する。
async fn spawn_fake_ws_recording(acl_value: Value) -> (u16, Arc<tokio::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lines_log: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&lines_log);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            let acl_value = acl_value.clone();
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(line) = msg {
                        log.lock().await.push(line.clone());
                        let value = if line.contains("accesscontrol read acl") {
                            acl_value.clone()
                        } else {
                            json!(true)
                        };
                        let resp = json!({ "results": [{ "value": value }], "logs": [] });
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    (port, lines_log)
}

/// group の bind 状態を模した fake ws（issue #5 rebind 用）。コマンド行を記録しつつ:
/// - `groupsettings unbind-keyset`: bound なら成功（以後 unbound）、unbound なら FAILURE
/// - `groupsettings bind-keyset`: bound かつ未 unbind なら FAILURE（Duplicate 相当）、
///   それ以外は成功
/// - `accesscontrol read acl`: admin エントリのみの実機数値キー形式
/// - それ以外: 成功（value: true）
async fn spawn_fake_ws_group(bound: bool) -> (u16, Arc<tokio::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lines_log: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&lines_log);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                let mut bound = bound;
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(line) = msg {
                        log.lock().await.push(line.clone());
                        let resp = if line.contains("groupsettings unbind-keyset") {
                            if bound {
                                bound = false;
                                json!({ "results": [{ "value": true }], "logs": [] })
                            } else {
                                json!({ "results": [{ "error": "FAILURE" }], "logs": [] })
                            }
                        } else if line.contains("groupsettings bind-keyset") && bound {
                            // bind 済みのまま bind → Duplicate key id 相当の失敗。
                            json!({ "results": [{ "error": "FAILURE" }], "logs": [] })
                        } else if line.contains("accesscontrol read acl") {
                            json!({ "results": [{ "value": [{"1":5,"2":2,"3":[112233],"4":null,"254":1}] }], "logs": [] })
                        } else {
                            json!({ "results": [{ "value": true }], "logs": [] })
                        };
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    (port, lines_log)
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

/// color: ColorControl MoveToHueAndSaturation にマップされ、hue / saturation
/// （度・% と換算済み 0–254 生値）を応答へエコーする（直経路 `mat color` と同形）。
#[tokio::test]
async fn color_echoes_hue_and_saturation() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[
            json!({"id":1,"op":"color","node_id":1,"endpoint":1,"hue_raw":233,"saturation_raw":203,"hue":330,"saturation":80,"transition":30}),
            json!({"op":"color","node_id":99,"endpoint":1,"hue_raw":233,"saturation_raw":203,"hue":330,"saturation":80}),
        ],
    )
    .await;

    let r = &resps[0];
    assert_eq!(r["id"], json!(1));
    assert_eq!(r["cluster"], "colorcontrol");
    assert_eq!(r["command"], "move-to-hue-and-saturation");
    assert_eq!(r["hue"], json!(330));
    assert_eq!(r["saturation"], json!(80));
    assert_eq!(r["hue_raw"], json!(233));
    assert_eq!(r["saturation_raw"], json!(203));
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

/// group_color_temp: 換算済み mireds で groupcast し、kelvin / mireds をエコー、
/// status="sent"（unacknowledged; 直経路 `mat group color-temp` と同形）。
#[tokio::test]
async fn group_color_temp_reports_sent_with_echo() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({"id":1,"op":"group_color_temp","group_id":1,"mireds":370,"kelvin":2700,"transition":0,"endpoint":1})],
    )
    .await;
    assert_eq!(resps[0]["status"], "sent");
    assert_eq!(resps[0]["kelvin"], 2700);
    assert_eq!(resps[0]["mireds"], 370);
    assert_eq!(resps[0]["command"], "move-to-color-temperature");
    assert!(resps[0]["timestamp"].is_string());

    handle.abort();
}

/// group_color: 換算済み raw で groupcast し、name / rgb / 度・% をエコー、
/// status="sent"（直経路 `mat group color` と同形）。
#[tokio::test]
async fn group_color_reports_sent_with_echo() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({"id":1,"op":"group_color","group_id":1,"hue_raw":169,"saturation_raw":254,"hue":240,"saturation":100,"name":"blue","rgb":"#0000ff","transition":0,"endpoint":1})],
    )
    .await;
    assert_eq!(resps[0]["status"], "sent");
    assert_eq!(resps[0]["name"], "blue");
    assert_eq!(resps[0]["rgb"], "#0000ff");
    assert_eq!(resps[0]["hue_raw"], 169);
    assert_eq!(resps[0]["command"], "move-to-hue-and-saturation");

    handle.abort();
}

/// 単体 color の name / rgb エコー（op に載せた任意フィールドが応答へ返る）。
#[tokio::test]
async fn color_echoes_optional_name_and_rgb() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({"id":1,"op":"color","node_id":1,"endpoint":1,"hue_raw":0,"saturation_raw":254,"hue":0,"saturation":100,"name":"red","rgb":"#ff0000","transition":0})],
    )
    .await;
    assert_eq!(resps[0]["status"], "success");
    assert_eq!(resps[0]["name"], "red");
    assert_eq!(resps[0]["rgb"], "#ff0000");

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

/// group provision の step 4: ACL read → 既存リスト + group エントリの全置換 write。
#[tokio::test]
async fn group_provision_appends_group_acl_entry() {
    let (port, log) =
        spawn_fake_ws_recording(json!([{"1":5,"2":2,"3":[112233],"4":null,"254":1}])).await;
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
            "epoch_key":"00112233445566778899aabbccddeeff"
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned", "{}", resps[0]);

    let lines = log.lock().await;
    assert!(
        lines.iter().any(|l| l == "accesscontrol read acl 1 0"),
        "acl read missing: {lines:?}"
    );
    let write = lines
        .iter()
        .find(|l| l.starts_with("accesscontrol write acl "))
        .expect("acl write missing");
    // compact JSON 1 引数 + 宛先。admin エントリ保全 + group 1 の Operate/Group。
    assert!(write.ends_with(" 1 0"), "{write}");
    assert!(write.contains("\"subjects\":[112233]"), "{write}");
    assert!(write.contains("\"authMode\":3"), "{write}");
    assert!(write.contains("\"subjects\":[1]"), "{write}");

    handle.abort();
}

/// 既に Group エントリがある → 冪等: write は送らない。
#[tokio::test]
async fn group_provision_skips_acl_write_when_entry_exists() {
    let (port, log) = spawn_fake_ws_recording(json!([
        {"1":5,"2":2,"3":[112233],"4":null,"254":1},
        {"1":3,"2":3,"3":[1],"4":null,"254":1}
    ]))
    .await;
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
            "epoch_key":"00112233445566778899aabbccddeeff"
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned", "{}", resps[0]);

    let lines = log.lock().await;
    assert!(lines.iter().any(|l| l == "accesscontrol read acl 1 0"));
    assert!(
        !lines.iter().any(|l| l.contains("accesscontrol write acl")),
        "must not write when the entry already exists: {lines:?}"
    );

    handle.abort();
}

/// ACL read の値が解釈不能 → parse_error で停止し、絶対に write しない。
#[tokio::test]
async fn group_provision_unparseable_acl_stops_with_parse_error() {
    let (port, log) = spawn_fake_ws_recording(json!(true)).await;
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
            "epoch_key":"00112233445566778899aabbccddeeff"
        })],
    )
    .await;
    assert_eq!(resps[0]["error"]["kind"], "parse_error", "{}", resps[0]);

    let lines = log.lock().await;
    assert!(
        !lines.iter().any(|l| l.contains("accesscontrol write acl")),
        "must never write after an unparseable read: {lines:?}"
    );

    handle.abort();
}

/// bind 済み controller でも rebind:true なら unbind → bind の順で成功する（issue #5）。
#[tokio::test]
async fn group_provision_rebind_unbinds_before_bind() {
    let (port, lines) = spawn_fake_ws_group(true).await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,
            "name":"living","endpoint":1,"rebind":true,
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned", "{}", resps[0]);
    // matd 経路は warm chip-tool 自身が状態更新するので再起動 note は出ない。
    assert!(resps[0].get("note").is_none(), "{}", resps[0]);

    let recorded = lines.lock().await.clone();
    let unbind = recorded
        .iter()
        .position(|l| l.contains("groupsettings unbind-keyset 1 42"))
        .expect("unbind-keyset line missing");
    let bind = recorded
        .iter()
        .position(|l| l.contains("groupsettings bind-keyset 1 42"))
        .expect("bind-keyset line missing");
    assert!(unbind < bind, "unbind must run before bind: {recorded:?}");
    handle.abort();
}

/// 未 bind（新規グループ）でも rebind:true は成功する（unbind の失敗を無視 = 冪等）。
#[tokio::test]
async fn group_provision_rebind_on_unbound_group_succeeds() {
    let (port, _lines) = spawn_fake_ws_group(false).await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,
            "name":"living","endpoint":1,"rebind":true,
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned", "{}", resps[0]);
    handle.abort();
}

/// rebind 無しの既存挙動は不変: bind 済みなら bind-keyset の失敗で止まる。
#[tokio::test]
async fn group_provision_without_rebind_fails_on_bound_group() {
    let (port, _lines) = spawn_fake_ws_group(true).await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,
            "name":"living","endpoint":1,
        })],
    )
    .await;
    assert!(
        resps[0]["error"]["kind"].is_string(),
        "must fail on bound group without rebind: {}",
        resps[0]
    );
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

/// 各コマンドへ応答した直後に接続を閉じる fake ws。matd 側から見ると「次の送信時には
/// ソケットが死んでいる」状況を毎回作る（issue #7 の決定論的再現形状）。
/// accept ループは生きているので、張り直せば次のコマンドは通る。
async fn spawn_fake_ws_close_after_reply() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                if let Some(Ok(Message::Text(line))) = ws.next().await {
                    let resp = json!({ "cmd": line, "results": [{ "value": true }], "logs": [] });
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                    let _ = ws.close(None).await;
                }
            });
        }
    });
    port
}

/// 受け取ったコマンド行を記録し、`fail_first` 回までは応答せずに接続を閉じる fake ws。
/// それ以降の接続では普通に応答する。受信失敗（実行されたかもしれない）の再現用。
async fn spawn_fake_ws_no_reply_then_ok(
    fail_first: usize,
) -> (u16, Arc<tokio::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lines_log: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&lines_log);
    tokio::spawn(async move {
        let mut conn_no = 0usize;
        while let Ok((stream, _)) = listener.accept().await {
            conn_no += 1;
            // 1 本目は new() の早期接続だが、最初のコマンドはその接続上で走るので
            // 「接続 n 本目まで失敗」= 「コマンド n 回目まで失敗」に一致する。
            let fail = conn_no <= fail_first;
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(line) = msg {
                        log.lock().await.push(line.clone());
                        if fail {
                            let _ = ws.close(None).await;
                            return;
                        }
                        let resp =
                            json!({ "cmd": line, "results": [{ "value": true }], "logs": [] });
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    (port, lines_log)
}

/// 受信失敗（応答なしで切断）はコマンドを再送しない（toggle 二重実行防止）。
/// エラーは 1 回返り、次のコマンドは遅延再接続で成功する。
#[tokio::test]
async fn recv_failure_is_not_retried_and_recovers_lazily() {
    let (port, log) = spawn_fake_ws_no_reply_then_ok(1).await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_secs(300))
        .await
        .unwrap();

    // 1 回目: fake は受信を記録して応答せず切断 → エラーが返る。
    let err = backend.run_cmdline("onoff toggle 5 1").await.unwrap_err();
    assert_eq!(err.kind, mat_core::error::ErrorKind::ChildFailed);
    // 着信は正確に 1 回 = 再送していない。
    assert_eq!(log.lock().await.len(), 1);

    // 2 回目: 遅延再接続で成功する。
    let v = backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert_eq!(v["results"][0]["value"], json!(true));
    assert_eq!(log.lock().await.len(), 2);
}

/// Spawn モード検証用の fake chip-tool（寝るだけのシェルスクリプト）を用意し、
/// MAT_CHIP_TOOL_BIN に設定する。ws は fake サーバが別途待ち受けるので、子は引数を
/// 無視して寝ていればよい。
///
/// env はプロセスグローバルなので、パスは**固定**（temp_dir 直下）・内容は全テスト共通・
/// 削除しない。これで Spawn モードの複数テストが並行しても互いに無害（同じ値を
/// set_var し合うだけ）。
///
/// `exec` 必須: sh の子として sleep を残すと、子（sh）を kill したとき orphan の
/// sleep がテストバイナリの stdout パイプを掴んだまま 300 秒生き残り、
/// `cargo test | ...` のパイプ読み手が EOF を貰えず固まる。
fn setup_fake_child_bin() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join("matd-test-fake-chip-tool.sh");
    std::fs::write(&path, "#!/bin/sh\nexec sleep 300\n").unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    std::env::set_var("MAT_CHIP_TOOL_BIN", &path);
    path
}

/// 受信失敗 1 回では子プロセスを殺さない（warm CASE 温存）。連続 2 回で子ごと畳み、
/// 子が死んでいたら次の ensure で respawn する。
///
/// MAT_CHIP_TOOL_BIN はプロセスグローバルなので、Spawn モードを使うテストはこの 1 本に
/// まとめる（並行テストとの env 競合を避ける）。
#[tokio::test]
async fn spawn_mode_preserves_child_and_respawns_dead_child() {
    let tmp = tempfile::tempdir().unwrap();
    setup_fake_child_bin();

    // 1 回だけ応答なし切断 → 以降正常、の fake（Task 2 のものを流用）。
    let (port, _log) = spawn_fake_ws_no_reply_then_ok(1).await;
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend = matd::backend::ChipToolBackend::spawn(&store, port, Duration::from_secs(300))
        .await
        .unwrap();

    let pid1 = backend.child_pid().await.expect("child spawned");

    // 受信失敗 1 回: エラーは返るが子は温存される。
    let _ = backend.run_cmdline("onoff toggle 5 1").await.unwrap_err();
    assert_eq!(
        backend.child_pid().await,
        Some(pid1),
        "receive failure must not kill the healthy child"
    );
    assert!(!backend.ws_connected().await, "broken ws must be dropped");

    // 次のコマンドは遅延再接続で成功し、失敗カウンタが 0 に戻る。
    backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert_eq!(backend.child_pid().await, Some(pid1));

    // 子を外から殺す → 次の ensure（ws を落としてから）で respawn される。
    let kill = std::process::Command::new("kill")
        .arg(pid1.to_string())
        .status()
        .unwrap();
    assert!(kill.success());
    // 子の exit を待つ。kill(2) は送出成功しか保証しない。子はこのプロセスの子なので
    // reap（backend 側の try_wait）までは zombie として残る — /proc の状態が Z になるか
    // エントリが消えた時点で try_wait が exit を拾える。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid1}/stat")).unwrap_or_default();
        let dead = stat.is_empty() || stat.split_whitespace().nth(2) == Some("Z");
        if dead {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child {pid1} did not exit within 5s after kill"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    backend.shutdown_ws_for_test().await; // ws だけ落とすヘルパ
    backend.run_cmdline("onoff on 5 1").await.unwrap();
    let pid2 = backend.child_pid().await.expect("respawned");
    assert_ne!(pid2, pid1, "dead child must be respawned");
}

/// 保険の検証: 受信失敗が 2 連続したら従来どおり子ごと畳む（wedge した chip-tool を
/// 温存し続けて永久に timeout し続けるのを防ぐ）。
#[tokio::test]
async fn two_consecutive_recv_failures_tear_down_child() {
    let tmp = tempfile::tempdir().unwrap();
    setup_fake_child_bin();

    let (port, _log) = spawn_fake_ws_no_reply_then_ok(2).await;
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend = matd::backend::ChipToolBackend::spawn(&store, port, Duration::from_secs(300))
        .await
        .unwrap();
    let pid1 = backend.child_pid().await.expect("child spawned");

    // 1 回目の受信失敗: 子は温存。
    let _ = backend.run_cmdline("onoff on 5 1").await.unwrap_err();
    assert_eq!(backend.child_pid().await, Some(pid1));

    // 2 回目の受信失敗: 連続 2 回で子ごと teardown。
    let _ = backend.run_cmdline("onoff on 5 1").await.unwrap_err();
    assert_eq!(
        backend.child_pid().await,
        None,
        "2nd consecutive failure tears down"
    );

    // 3 回目: フル再確立（respawn）で成功し、カウンタも 0 に戻っている。
    backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert!(backend.child_pid().await.is_some());
}

/// 受信失敗で ws だけ捨てた「ws=None・子生存」の状態でも、アイドル超過の reap は
/// 子ごと畳む（chip-tool は生きている限り CPU を焼き続けるため、reap が ws の有無に
/// 依存すると #8 の焼きが止まらない）。
#[tokio::test]
async fn reap_kills_child_even_without_ws() {
    let tmp = tempfile::tempdir().unwrap();
    setup_fake_child_bin();

    let (port, _log) = spawn_fake_ws_no_reply_then_ok(1).await;
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend = matd::backend::ChipToolBackend::spawn(&store, port, Duration::from_millis(100))
        .await
        .unwrap();
    let pid1 = backend.child_pid().await.expect("child spawned");

    // 受信失敗 → ws=None・子は温存。
    let _ = backend.run_cmdline("onoff on 5 1").await.unwrap_err();
    assert_eq!(backend.child_pid().await, Some(pid1));
    assert!(!backend.ws_connected().await);

    // アイドル超過の reap が子を畳む。
    tokio::time::sleep(Duration::from_millis(150)).await;
    backend.reap_if_idle().await;
    assert_eq!(
        backend.child_pid().await,
        None,
        "idle reap must kill the preserved child"
    );
}

/// Connect モードでは ws=None なら reap は何もしない（無害）。子ごと畳む実測は
/// `reap_kills_child_even_without_ws` が担う。
#[tokio::test]
async fn reap_fires_even_when_ws_already_dropped() {
    let (port, _log) = spawn_fake_ws_no_reply_then_ok(1).await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_millis(100))
        .await
        .unwrap();
    // 受信失敗 → ws=None（Connect モードなので子は元々無い）。
    let _ = backend.run_cmdline("onoff on 5 1").await.unwrap_err();
    tokio::time::sleep(Duration::from_millis(150)).await;
    backend.reap_if_idle().await; // ws=None でも panic せず normally 完走すること
                                  // reap 後もフル再確立で次のコマンドが通る。
    let v = backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert_eq!(v["results"][0]["value"], json!(true));
}

/// サーバ側切断のあと、失敗はあっても 1 回で、次のコマンドは必ず成功する。
/// 送信失敗として観測されれば透過リトライで 0 回、受信失敗として観測されれば
/// リトライなしの 1 回（二重実行防止）— どちらのタイミングでも 2 連続失敗はしない。
/// （送信失敗の分類そのものは backend.rs の duplex ユニットテストが決定論的に担保。）
#[tokio::test]
async fn server_close_costs_at_most_one_failure() {
    let port = spawn_fake_ws_close_after_reply().await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_secs(300))
        .await
        .unwrap();

    let v1 = backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert_eq!(v1["results"][0]["value"], json!(true));
    tokio::time::sleep(Duration::from_millis(100)).await; // close がワイヤに乗るのを待つ

    // 2 回目: 送信失敗なら透過リトライで成功、受信失敗ならエラー 1 回。
    match backend.run_cmdline("onoff off 5 1").await {
        Ok(v) => assert!(v["cmd"].as_str().unwrap().contains("onoff off")),
        Err(_) => {
            // 失敗は 1 回で止まり、次は必ず成功する。
            let v = backend.run_cmdline("onoff off 5 1").await.unwrap();
            assert!(v["cmd"].as_str().unwrap().contains("onoff off"));
        }
    }
}
