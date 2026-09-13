//! Shared helpers for the live E2E tests (`live_*.rs`, all `#[ignore]`d, not
//! run in CI). Each test binary is its own compilation unit, so every
//! `live_*.rs` that needs these does `mod common;` and gets its own copy —
//! a file that only uses some of the helpers marks the rest
//! `#[allow(dead_code)]` here rather than pull in an extra dev-dependency.

/// Required env var, or panic naming it.
#[allow(dead_code)]
pub fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} required"))
}

/// Required env var parsed as `u64` — `0x`-prefixed hex or decimal.
#[allow(dead_code)]
pub fn env_u64(name: &str) -> u64 {
    let s = env(name);
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).expect("hex id"),
        None => s.parse().expect("decimal id"),
    }
}

/// `MAT_E2E_NODE_ID`, hex (`0x...`) or decimal — the target device node id
/// most live tests commission/control.
#[allow(dead_code)]
pub fn env_node_id() -> u64 {
    env_u64("MAT_E2E_NODE_ID")
}

/// Sends one JSON line to a `matd` unix socket and parses the response line.
#[allow(dead_code)]
pub async fn request(socket: &str, line: &str) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

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

/// Asserts a `matd` JSON response has no top-level `error` field.
#[allow(dead_code)]
pub fn assert_ok(v: &serde_json::Value, ctx: &str) {
    assert!(v.get("error").is_none(), "{ctx}: error response: {v}");
}
