//! Live E2E (M4): drive a running native-enabled matd over its unix socket.
//! Verifies hotpath ops go through the native warm session (on/off/color/
//! color-temp/onoff read), a second same-node command reuses the session
//! (faster), and describe still works via chip-tool fallback.
//! Run via `task e2e:m4`. Not in CI.
//!
//! Required env: MAT_E2E_SOCKET (matd socket path), MAT_E2E_NODE_ID,
//! MAT_E2E_ENDPOINT (default 1).

use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn env_u64(name: &str) -> u64 {
    let s = std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).expect("hex id"),
        None => s.parse().expect("decimal id"),
    }
}

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

#[tokio::test]
#[ignore = "requires a running native-enabled matd + a commissioned device (task e2e:m4)"]
async fn matd_native_hotpath_roundtrip() {
    let socket = std::env::var("MAT_E2E_SOCKET").expect("MAT_E2E_SOCKET required");
    let node = env_u64("MAT_E2E_NODE_ID");
    let ep: u16 = std::env::var("MAT_E2E_ENDPOINT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    // 初回 on（mDNS+CASE を含む） → 応答の所要時間を測る。
    let on = format!(r#"{{"op":"on","node_id":{node},"endpoint":{ep}}}"#);
    let t0 = Instant::now();
    let r = request(&socket, &on).await;
    let cold = t0.elapsed();
    assert_ok(&r, "on (cold)");
    assert_eq!(r["command"], "on");

    // onoff read が on を反映。
    let read = format!(
        r#"{{"op":"read","node_id":{node},"endpoint":{ep},"cluster":"onoff","attribute":"on-off"}}"#
    );
    let r = request(&socket, &read).await;
    assert_ok(&r, "read on-off");
    assert_eq!(
        r["value"],
        serde_json::json!(true),
        "on-off should be true after on"
    );

    // 2 回目の on は warm セッション再利用で速いはず（mDNS+CASE を払わない）。
    let t1 = Instant::now();
    let r = request(&socket, &on).await;
    let warm = t1.elapsed();
    assert_ok(&r, "on (warm)");
    eprintln!("cold {cold:?} vs warm {warm:?}");
    assert!(
        warm < cold,
        "warm command must be faster than the cold one (session reuse)"
    );
    assert!(
        warm < Duration::from_millis(500),
        "warm command should be sub-500ms, got {warm:?}"
    );

    // 色・色温度のホットパス。
    let color = format!(
        r#"{{"op":"color","node_id":{node},"endpoint":{ep},"hue_raw":180,"saturation_raw":200,"hue":254,"saturation":78,"transition":0}}"#
    );
    assert_ok(&request(&socket, &color).await, "color");
    let ctemp = format!(
        r#"{{"op":"color_temp","node_id":{node},"endpoint":{ep},"mireds":370,"kelvin":2700,"transition":0}}"#
    );
    assert_ok(&request(&socket, &ctemp).await, "color_temp");

    // フォールバック: describe は chip-tool 経由で従来どおり動く。
    let describe = format!(r#"{{"op":"describe","node_id":{node}}}"#);
    let r = request(&socket, &describe).await;
    assert_ok(&r, "describe (chip-tool fallback)");
    assert!(r.get("endpoints").is_some(), "describe returns endpoints");

    // 後始末: off に戻す。
    let off = format!(r#"{{"op":"off","node_id":{node},"endpoint":{ep}}}"#);
    assert_ok(&request(&socket, &off).await, "off");
}
