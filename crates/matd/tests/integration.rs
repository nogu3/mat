//! matd 統合テスト。実デバイスは使わず、`mat_native::test_support` の fake
//! establisher/conn で native backend を組み立て、unix socket → matd →
//! native → 応答の往復を検証する。
//!
//! M8c-3: matd は chip-tool を一切 spawn しない（native backend が唯一の
//! 実行経路）。詳細な native 実行ロジック（read/write/invoke の値符号化、
//! group provision の ACL マージ等）は `mat-native` / `matd::server` 自身の
//! 単体テストで担保する — ここは unix socket 越しのワイヤプロトコルが
//! native 一本化後も壊れていないことを確認する薄い層。

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use mat_core::error::MatError;
use mat_core::store::{NodeRecord, Store};
use mat_native::test_support::FakeEstablisher;
use mat_native::Establisher;

use matd::native::NativeBackend;
use matd::server::NativeState;

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
    // serve が bind するまで待つ。並列テスト（既定）でランタイムが飽和すると
    // spawn した serve タスクの socket bind が遅れるので、窓は広め（250×20ms=5s）に
    // 取る（テストの意味は不変、決定化のためだけ）。
    let mut stream = None;
    for _ in 0..250 {
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

/// serve に渡す broadcast と、その送信ハンドルを返す版。listen 系テストが
/// イベントを注入するのに使う。
async fn start_matd_with_events(
    store_path: PathBuf,
    native: NativeState,
    capacity: usize,
) -> (
    PathBuf,
    tokio::task::JoinHandle<()>,
    tokio::sync::broadcast::Sender<matd::subscription::Event>,
) {
    let socket = std::env::temp_dir().join(format!("matd-test-{}.sock", rand_suffix()));
    let (tx, _rx) = tokio::sync::broadcast::channel(capacity);
    let socket_clone = socket.clone();
    let tx2 = tx.clone();
    let handle = tokio::spawn(async move {
        let _ = matd::server::serve(&socket_clone, store_path, native, tx2).await;
    });
    (socket, handle, tx)
}

fn occupancy_event(node_id: u64) -> matd::subscription::Event {
    matd::subscription::Event {
        node_id,
        endpoint: 1,
        cluster: 0x0406,
        attribute: 0x0000,
        value: serde_json::json!(1),
        priming: false,
    }
}

/// matd の serve をバックグラウンドで起動し、socket path を返す。listen を使わない
/// 既存テストの利便ラッパー（events 送信ハンドルは使い捨て）。
async fn start_matd(
    store_path: PathBuf,
    native: NativeState,
) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let (socket, handle, _tx) = start_matd_with_events(store_path, native, 16).await;
    (socket, handle)
}

/// デフォルトの fake establisher（read_onoff は常に true、read_json は未登録なら
/// `json!(1)`、invoke/write_tlv は常に成功）で native backend を組んで起動する。
async fn start_matd_with_fake(store_path: PathBuf) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    start_matd(store_path, NativeState::Ready(Box::new(native))).await
}

/// テストごとに一意な socket 名サフィックスを作る。並列テスト（既定）が同時に
/// `start_matd` を呼ぶと、この環境の時計は分解能 100ns・タイトループでは 75% が同値を
/// 返すため、nanos だけでは socket path が衝突する。衝突すると別テストの serve が
/// `remove_file` で相手の socket を消してしまい「connect できない」flake になる。
/// プロセスグローバルな単調カウンタで衝突を根絶する（pid も混ぜて別プロセス間も分離）。
fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", std::process::id(), n)
}

#[tokio::test]
async fn read_write_invoke_on_ping_and_errors_roundtrip() {
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd_with_fake(store_path).await;

    let resps = roundtrip(
        &socket,
        &[
            json!({"id":1,"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"}),
            json!({"id":2,"op":"on","node_id":1,"endpoint":1}),
            json!({"id":3,"op":"write","node_id":1,"endpoint":1,"cluster":"levelcontrol","attribute":"on-level","value":"128"}),
            json!({"id":4,"op":"invoke","node_id":1,"endpoint":1,"cluster":"levelcontrol","command":"move-to-level","args":["128","0","0","0"]}),
            json!({"op":"ping"}),
            json!({"op":"read","node_id":99,"endpoint":1,"cluster":"onoff","attribute":"on-off"}),
        ],
    )
    .await;

    // read: native の成功値を mat スキーマへ乗せ、id/timestamp を付ける。生の
    // native/session 内部表現は素通ししない（CLAUDE.md ルール 2）。
    let r = &resps[0];
    assert_eq!(r["id"], json!(1));
    assert_eq!(r["node_id"], json!(1));
    assert_eq!(r["cluster"], "onoff");
    assert_eq!(r["value"], json!(true));
    assert!(r["timestamp"].is_string());

    // on: OnOff On invoke にマップされ status success。
    let r = &resps[1];
    assert_eq!(r["id"], json!(2));
    assert_eq!(r["status"], "success");
    assert_eq!(r["command"], "on");

    // write: 入力 "128" を read と揃えた数値型へ正規化して返す。
    let r = &resps[2];
    assert_eq!(r["id"], json!(3));
    assert_eq!(r["status"], "success");
    assert_eq!(r["value"], json!(128));

    // invoke: cluster/command をエコーし status success。
    let r = &resps[3];
    assert_eq!(r["id"], json!(4));
    assert_eq!(r["status"], "success");
    assert_eq!(r["command"], "move-to-level");

    // ping: native に触れず即応。
    assert_eq!(resps[4]["pong"], json!(true));

    // 未 commission node: node_not_commissioned エラー。
    assert_eq!(resps[5]["error"]["kind"], "node_not_commissioned");

    handle.abort();
}

#[tokio::test]
async fn describe_builds_endpoints_from_native_descriptor_read() {
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd_with_fake(store_path).await;

    let resps = roundtrip(&socket, &[json!({"op":"describe","node_id":1})]).await;
    let r = &resps[0];
    assert_eq!(r["node_id"], json!(1));
    let endpoints = r["endpoints"].as_array().expect("endpoints array");
    assert!(!endpoints.is_empty());
    assert!(endpoints[0].get("endpoint").is_some());
    assert!(endpoints[0]["clusters"].is_array());

    handle.abort();
}

/// M8c-3: cluster/attribute/command 名が mat-core::ids で解決できない op は、
/// chip-tool へのフォールバック先が無いため即 parse_error（数値 ID は影響しない）。
#[tokio::test]
async fn unresolved_names_return_parse_error() {
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd_with_fake(store_path).await;

    let resps = roundtrip(
        &socket,
        &[
            json!({"op":"read","node_id":1,"endpoint":1,"cluster":"nosuchcluster","attribute":"x"}),
            json!({"op":"write","node_id":1,"endpoint":1,"cluster":"nosuchcluster","attribute":"x","value":"1"}),
            json!({"op":"invoke","node_id":1,"endpoint":1,"cluster":"nosuchcluster","command":"x"}),
            json!({"op":"group_invoke","group_id":1,"cluster":"nosuchcluster","command":"x","endpoint":1}),
        ],
    )
    .await;

    for r in &resps {
        assert_eq!(r["error"]["kind"], "parse_error", "got: {r}");
    }

    handle.abort();
}

#[tokio::test]
async fn invalid_request_json_is_parse_error() {
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd_with_fake(store_path).await;

    let mut stream = None;
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(&socket).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stream = stream.expect("could not connect to matd socket");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    write_half.write_all(b"not json\n").await.unwrap();
    let resp = lines.next_line().await.unwrap().expect("no response line");
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["kind"], "parse_error");

    handle.abort();
}

#[tokio::test]
async fn shutdown_op_stops_server() {
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd_with_fake(store_path).await;

    let resps = roundtrip(&socket, &[json!({"op":"shutdown"})]).await;
    assert_eq!(resps[0]["stopping"], json!(true));

    // serve ループが抜けて join できる（タイムアウトすれば shutdown が効いていない）。
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("serve loop did not exit after shutdown op")
        .unwrap();
}

/// group ctx（group 送信用の native コンテキスト）が未構成の native backend は
/// `mat_native::group::send` が `Unavailable` を返す。M8c-3: フォールバック無しで
/// 即 store_parse（`mat group provision` への誘導 detail 付き）。
#[tokio::test]
async fn group_invoke_without_group_ctx_returns_store_parse() {
    let (_dir, store_path) = make_store();
    let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), None);
    let (socket, handle) = start_matd(store_path, NativeState::Ready(Box::new(native))).await;

    let resps = roundtrip(
        &socket,
        &[json!({"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","endpoint":1})],
    )
    .await;
    assert_eq!(resps[0]["error"]["kind"], "store_parse");
    assert!(resps[0]["error"]["detail"]
        .as_str()
        .unwrap()
        .contains("native group send unavailable"));

    handle.abort();
}

/// group provision の 2 ステップ（controller 側 group state / デバイス側 4
/// ステップ）ともに native で完遂することを socket 越しに確認する
/// （個々の native ロジック — ACL マージや rebind 等 — は mat-native/matd::server
/// の単体テストで担保済み。ここはワイヤプロトコルの通し確認）。
#[tokio::test]
async fn group_provision_roundtrip_writes_kvs_and_reports_provisioned() {
    struct ScriptedEstablisher;
    #[async_trait::async_trait]
    impl Establisher for ScriptedEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            Ok(Box::new(
                mat_native::test_support::FakeConn::scripted()
                    .with_read(0, 0x003F, 0x0000, json!([]))
                    .with_read(
                        0,
                        0x001F,
                        0x0000,
                        json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]),
                    ),
            ))
        }
    }

    let (_dir, store_path) = make_store();
    let ini = store_path.join("chip_tool_config.ini");
    std::fs::write(&ini, "[Default]\n").unwrap();
    let gs = mat_native::group_settings::GroupSettingsCtx {
        main_ini: ini.clone(),
        fabric_index: 2,
        cfid: [7u8; 8],
    };
    let native = NativeBackend::with_parts_gs(Box::new(ScriptedEstablisher), None, Some(gs));
    let (socket, handle) = start_matd(store_path, NativeState::Ready(Box::new(native))).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op": "group_provision",
            "group_id": 99,
            "node_ids": [1],
            "keyset_id": 99,
            "name": "e2e",
            "endpoint": 1,
            "epoch_key": "00112233445566778899aabbccddeeff",
        })],
    )
    .await;

    let r = &resps[0];
    assert_eq!(r["status"], "provisioned");
    assert_eq!(r["nodes"], json!([1]));
    assert!(mat_controller::kvs::read_group_credentials(&ini, 2, 99).is_ok());

    handle.abort();
}

/// 起動時の native 構築失敗（KVS 資材が読めない等）は matd を落とさない
/// （M8c-3: `mat fabric init` 誘導のため常駐し続ける）。Ping/Shutdown 以外の
/// 全 op は、その構築エラーをそのまま返す（一律 store_missing/store_parse —
/// Task 9 の mat 直経路と同じ精度）。
#[tokio::test]
async fn native_unavailable_answers_every_op_with_build_error_but_keeps_serving() {
    let (_dir, store_path) = make_store();
    let build_err = MatError::store_missing("no KVS materials for native backend");
    let (socket, handle) = start_matd(store_path, NativeState::Unavailable(build_err)).await;

    let resps = roundtrip(
        &socket,
        &[
            json!({"op":"on","node_id":1,"endpoint":1}),
            json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"}),
            json!({"op":"group_invoke","group_id":1,"cluster":"onoff","command":"on","endpoint":1}),
            json!({"op":"ping"}),
        ],
    )
    .await;

    assert_eq!(resps[0]["error"]["kind"], "store_missing");
    assert_eq!(resps[1]["error"]["kind"], "store_missing");
    assert_eq!(resps[2]["error"]["kind"], "store_missing");
    // Ping だけは native に触れず常に成功する。
    assert_eq!(resps[3]["pong"], json!(true));

    handle.abort();
}

/// listen: ack 1 行の後、フィルタ一致イベントだけが同接続に NDJSON で流れる。
#[tokio::test]
async fn listen_acks_then_streams_filtered_events() {
    let (_dir, store_path) = make_store();
    let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    let (socket, handle, tx) =
        start_matd_with_events(store_path, NativeState::Ready(Box::new(native)), 16).await;

    // 接続して listen（node 21 のみ）
    let mut stream = None;
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(&socket).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stream = stream.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    write_half
        .write_all(b"{\"id\":9,\"op\":\"listen\",\"node_id\":21}\n")
        .await
        .unwrap();

    // ack 1 行
    let ack: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ack["listening"], json!(true));
    assert_eq!(ack["id"], json!(9));
    assert!(ack["timestamp"].is_string());

    // フィルタ不一致（node 22）→ 届かない。一致（node 21）→ 届く。
    tx.send(occupancy_event(22)).unwrap();
    tx.send(occupancy_event(21)).unwrap();
    let ev: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ev["node_id"], json!(21));
    assert_eq!(ev["cluster"], "occupancysensing");
    assert_eq!(ev["attribute"], "occupancy");
    assert_eq!(ev["value"], json!(1));
    assert_eq!(ev["priming"], json!(false));

    handle.abort();
}

/// listen: broadcast の容量超過で listener が lag すると、エラー行を送ってから
/// 切断する（黙って欠落させない — spec ②）。
#[tokio::test]
async fn lagged_listener_gets_error_line_and_disconnect() {
    let (_dir, store_path) = make_store();
    let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    // capacity 1: listener が読む前に多数流すと必ず lag する。
    let (socket, handle, tx) =
        start_matd_with_events(store_path, NativeState::Ready(Box::new(native)), 1).await;

    let mut stream = None;
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(&socket).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stream = stream.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    write_half
        .write_all(b"{\"op\":\"listen\"}\n")
        .await
        .unwrap();
    let _ack = lines.next_line().await.unwrap().unwrap();

    // 大量送信で lag を起こす（handler が 1 通処理する間に capacity 超過させる）。
    for _ in 0..64 {
        let _ = tx.send(occupancy_event(21));
    }
    // どこかで lag エラー行が来て、その後 EOF（切断）。
    let mut saw_lag = false;
    while let Some(line) = lines.next_line().await.unwrap() {
        let v: Value = serde_json::from_str(&line).unwrap();
        if v.get("error").is_some() {
            assert_eq!(v["error"]["kind"], "other");
            assert!(v["error"]["detail"].as_str().unwrap().contains("lagged"));
            saw_lag = true;
            break;
        }
    }
    assert!(saw_lag, "expected lag error line");
    // 切断される（次の read は EOF）。
    assert!(lines.next_line().await.unwrap().is_none());

    handle.abort();
}
