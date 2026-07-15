//! Live E2E (M5): native groupcast against the real living_lights group.
//! matd_group_roundtrip: group off→on→color-temp、各ノードを unicast read で
//! 検証（= N/N 配達判定）。matd_group_after_restart: スクリプトが matd を
//! 再起動した後に呼び、jump-ahead 後も配達されることを検証（消灯で終わる）。
//! Run via `task e2e:m5`. Not in CI.
//!
//! Required env: MAT_E2E_SOCKET (matd socket path), MAT_E2E_GROUP_NODES
//! (csv node ids), MAT_E2E_ENDPOINT (default 1), MAT_E2E_GROUP_ID (default 10).

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn request(socket: &str, line: &str) -> serde_json::Value {
    let stream = UnixStream::connect(socket)
        .await
        .expect("connect matd socket");
    let (rd, mut wr) = stream.into_split();
    wr.write_all(line.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();
    let mut lines = BufReader::new(rd).lines();
    let resp = lines.next_line().await.unwrap().expect("response line");
    serde_json::from_str(&resp).expect("json response")
}

fn assert_ok(v: &serde_json::Value, ctx: &str) {
    assert!(v.get("error").is_none(), "{ctx}: error response: {v}");
}

fn group_nodes() -> Vec<u64> {
    std::env::var("MAT_E2E_GROUP_NODES")
        .expect("MAT_E2E_GROUP_NODES (csv node ids) required")
        .split(',')
        .map(|s| {
            let s = s.trim();
            match s.strip_prefix("0x") {
                Some(h) => u64::from_str_radix(h, 16).expect("hex id"),
                None => s.parse().expect("decimal id"),
            }
        })
        .collect()
}

async fn assert_all_onoff(socket: &str, nodes: &[u64], ep: u16, want: bool, ctx: &str) {
    for node in nodes {
        let read = format!(
            r#"{{"op":"read","node_id":{node},"endpoint":{ep},"cluster":"onoff","attribute":"on-off"}}"#
        );
        let r = request(socket, &read).await;
        assert_ok(&r, &format!("{ctx}: read node {node}"));
        assert_eq!(r["value"], serde_json::json!(want), "{ctx}: node {node}");
    }
}

#[tokio::test]
#[ignore = "requires a running native-enabled matd + a provisioned group (task e2e:m5)"]
async fn matd_group_roundtrip() {
    let socket = std::env::var("MAT_E2E_SOCKET").expect("MAT_E2E_SOCKET required");
    let gid: u16 = std::env::var("MAT_E2E_GROUP_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let ep: u16 = std::env::var("MAT_E2E_ENDPOINT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let nodes = group_nodes();

    let ginv = |cmd: &str| {
        format!(
            r#"{{"op":"group_invoke","group_id":{gid},"cluster":"onoff","command":"{cmd}","endpoint":{ep}}}"#
        )
    };
    // off → 全ノード消灯（groupcast の伝播を 2s 待つ）。
    let r = request(&socket, &ginv("off")).await;
    assert_ok(&r, "group off");
    assert_eq!(r["status"], "sent");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_all_onoff(&socket, &nodes, ep, false, "after group off").await;

    // on → 全ノード点灯。
    assert_ok(&request(&socket, &ginv("on")).await, "group on");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_all_onoff(&socket, &nodes, ep, true, "after group on").await;

    // color-temp 370 mireds → 各ノードの color-temperature-mireds が目標±8。
    let ct = format!(
        r#"{{"op":"group_color_temp","group_id":{gid},"mireds":370,"kelvin":2702,"transition":0,"endpoint":{ep}}}"#
    );
    assert_ok(&request(&socket, &ct).await, "group color-temp");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    for node in &nodes {
        let read = format!(
            r#"{{"op":"read","node_id":{node},"endpoint":{ep},"cluster":"colorcontrol","attribute":"color-temperature-mireds"}}"#
        );
        let r = request(&socket, &read).await;
        assert_ok(&r, &format!("read color-temperature-mireds node {node}"));
        let v = r["value"].as_i64().expect("numeric mireds");
        assert!((v - 370).abs() <= 8, "node {node}: mireds {v} not near 370");
    }
}

#[tokio::test]
#[ignore = "second phase of task e2e:m5 (after the script restarts matd)"]
async fn matd_group_after_restart() {
    let socket = std::env::var("MAT_E2E_SOCKET").expect("MAT_E2E_SOCKET required");
    let gid: u16 = std::env::var("MAT_E2E_GROUP_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let ep: u16 = std::env::var("MAT_E2E_ENDPOINT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let nodes = group_nodes();
    // 再起動後の fresh counter（jump-ahead）でも配達される = M5 受け入れ 8。
    let off = format!(
        r#"{{"op":"group_invoke","group_id":{gid},"cluster":"onoff","command":"off","endpoint":{ep}}}"#
    );
    assert_ok(&request(&socket, &off).await, "group off after restart");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_all_onoff(&socket, &nodes, ep, false, "after restart group off").await;
}
