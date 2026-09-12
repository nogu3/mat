//! `mat listen`: matd の常駐 Subscribe に接続してイベント行をストリームする
//! （matd-only、直経路フォールバック無し）。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{json, Value};

use mat_core::error::{ErrorKind, MatError};

use super::connect_candidates;

/// listen リクエスト行を組む（None フィルタは省略 — `event` 省略は旧 matd
/// 互換のため必須。bare `--event` は clap 側で `"*"` に落ちている）。
fn listen_request_json(
    node: Option<u64>,
    endpoint: Option<u16>,
    cluster: &Option<String>,
    attribute: &Option<String>,
    event: &Option<String>,
) -> Value {
    let mut op = json!({ "op": "listen" });
    if let Some(n) = node {
        op["node_id"] = json!(n);
    }
    if let Some(e) = endpoint {
        op["endpoint"] = json!(e);
    }
    if let Some(c) = cluster {
        op["cluster"] = json!(c);
    }
    if let Some(a) = attribute {
        op["attribute"] = json!(a);
    }
    if let Some(e) = event {
        op["event"] = json!(e);
    }
    op
}

/// `mat listen` の引数（alias は main で数値に確定済み）。`Command::Listen` を
/// ここで再分解しないため、「非 Listen command が来た」「未解決 alias が届いた」
/// の internal-bug アームが不要になる。
pub struct ListenParams {
    pub node: Option<u64>,
    pub endpoint: Option<u16>,
    pub cluster: Option<String>,
    pub attribute: Option<String>,
    pub event: Option<String>,
    pub count: u32,
    pub timeout_ms: u64,
    pub reconnect: bool,
}

/// `mat listen`: matd へ接続し、ack 後のイベント行をそのまま stdout へ流す。
/// count/timeout は mat 側制御（enl listen と同じ UX）。matd 不在・応答なし・
/// ストリーム途中の matd 落ちは `matd_unavailable`（exit 13）。`--reconnect`
/// 指定時はその喪失を backoff 再接続で跨ぐ（count 累積・deadline 1 本）。
pub fn dispatch_listen(sockets: &[PathBuf], p: &ListenParams) -> ExitCode {
    let op = listen_request_json(p.node, p.endpoint, &p.cluster, &p.attribute, &p.event);
    if p.reconnect {
        return run_listen_reconnecting(sockets, &op, p.count, p.timeout_ms);
    }
    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            return MatError::new(
                ErrorKind::MatdUnavailable,
                format!("{detail}; `mat listen` requires a running matd"),
            )
            .emit_exit();
        }
    };
    tracing::info!(socket = %socket.display(), "listening via matd");
    match run_listen_stream(stream, &op, p.count, p.timeout_ms) {
        Ok(code) => code,
        Err(detail) => MatError::new(ErrorKind::MatdUnavailable, &detail).emit_exit(),
    }
}

/// 従来の単一接続 listen（`--reconnect` 無し）。ack → イベント行ループ。
/// 戻り値 Ok(exit code) / Err(detail) = matd 落ち扱い。
fn run_listen_stream(
    stream: UnixStream,
    op: &Value,
    count: u32,
    timeout_ms: u64,
) -> Result<ExitCode, String> {
    let deadline = listen_deadline(timeout_ms);
    let mut received = 0u32;
    stream_events(BufReader::new(stream), op, count, deadline, &mut received)
}

/// `--timeout-ms` を締切に変換（0 = 無期限）。
fn listen_deadline(timeout_ms: u64) -> Option<std::time::Instant> {
    (timeout_ms > 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms))
}

/// 1 接続分: listen 要求送信 → ack → イベント行ループ。`Ok(code)` = 終端
/// （count 到達 / timeout / エラー行）、`Err(detail)` = 接続喪失（EOF・read
/// エラー・非 JSON 行・ack 不正）。`received` は呼び手が再接続を跨いで持つ。
fn stream_events(
    mut reader: BufReader<UnixStream>,
    op: &Value,
    count: u32,
    deadline: Option<std::time::Instant>,
    received: &mut u32,
) -> Result<ExitCode, String> {
    use std::time::Instant;

    let mut line = serde_json::to_vec(op).map_err(|e| format!("failed to encode request: {e}"))?;
    line.push(b'\n');
    // UnixStream は &mut で write 可（BufReader は読み側だけを包む）。
    reader
        .get_mut()
        .write_all(&line)
        .map_err(|e| format!("failed to send listen request to matd: {e}"))?;

    let mut first = true; // 1 行目は ack（または即エラー）

    loop {
        // 残り時間を socket の read timeout に反映（0 = 無期限）。
        if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(finish_on_timeout(*received));
            }
            reader
                .get_ref()
                .set_read_timeout(Some(remaining))
                .map_err(|e| format!("failed to set read timeout: {e}"))?;
        }
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => {
                // EOF = matd がストリーム途中で落ちた（出力済みイベントはそのまま）。
                return Err("matd closed the event stream".to_string());
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(finish_on_timeout(*received));
            }
            Err(e) => return Err(format!("failed to read from matd: {e}")),
        }
        let v: Value = serde_json::from_str(&buf)
            .map_err(|e| format!("matd sent non-JSON line: {e}; body={buf}"))?;
        if let Some(err) = v.get("error") {
            // ack 前のエラー（フィルタ不正等）/ ストリーム中の lag 切断。
            eprintln!("{v}");
            let kind = err
                .get("kind")
                .and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok())
                .unwrap_or(ErrorKind::Other);
            return Ok(ExitCode::from(kind.exit_code()));
        }
        if first {
            // ack 行 `{"listening":true}` は出力せず読み捨てる。
            first = false;
            if v.get("listening").is_none() {
                return Err(format!("matd listen ack malformed: {v}"));
            }
            continue;
        }
        println!("{v}");
        *received += 1;
        if count > 0 && *received >= count {
            return Ok(ExitCode::SUCCESS);
        }
    }
}

/// `--reconnect`: 接続失敗 / 切断を backoff（1s→2s→…→30s 上限、成功でリセット）
/// で再接続し続ける。deadline は 1 本（再接続待ちも含む）、count は累積。
fn run_listen_reconnecting(
    sockets: &[PathBuf],
    op: &Value,
    count: u32,
    timeout_ms: u64,
) -> ExitCode {
    use std::time::{Duration, Instant};

    let deadline = listen_deadline(timeout_ms);
    let mut received = 0u32;
    let mut backoff = Duration::from_secs(1);
    let mut attempt: u32 = 0;
    loop {
        match connect_candidates(sockets) {
            Ok((stream, socket)) => {
                if attempt == 0 {
                    tracing::info!(socket = %socket.display(), "listening via matd");
                } else {
                    tracing::info!(socket = %socket.display(), attempt, "matd reconnected");
                }
                backoff = Duration::from_secs(1);
                match stream_events(BufReader::new(stream), op, count, deadline, &mut received) {
                    Ok(code) => return code,
                    Err(detail) => tracing::warn!(error = %detail, "matd lost; reconnecting"),
                }
            }
            Err(detail) => tracing::warn!(error = %detail, "matd unreachable; reconnecting"),
        }
        attempt += 1;
        let wait = match deadline {
            Some(dl) => {
                let remaining = dl.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return finish_on_timeout(received);
                }
                backoff.min(remaining)
            }
            None => backoff,
        };
        tracing::warn!(
            attempt,
            backoff_ms = wait.as_millis() as u64,
            "reconnecting to matd"
        );
        std::thread::sleep(wait);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// timeout 打ち切り: 0 件なら timeout(exit 3)、1 件以上なら成功（enl 準拠）。
fn finish_on_timeout(received: u32) -> ExitCode {
    if received == 0 {
        MatError::new(ErrorKind::Timeout, "no events received within --timeout-ms").emit_exit()
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_request_json_omits_absent_filters() {
        assert_eq!(
            listen_request_json(None, None, &None, &None, &None),
            json!({"op":"listen"})
        );
        assert_eq!(
            listen_request_json(
                Some(21),
                Some(1),
                &Some("occupancysensing".into()),
                &Some("occupancy".into()),
                &None,
            ),
            json!({
                "op":"listen","node_id":21,"endpoint":1,
                "cluster":"occupancysensing","attribute":"occupancy"
            })
        );
    }

    /// `event` は指定時のみ載る（旧 matd 互換で省略時はキー自体を送らない）。
    /// bare `--event` は clap の `default_missing_value` で `"*"` に落ちた
    /// 状態で渡ってくる想定。
    #[test]
    fn listen_request_json_includes_event_only_when_set() {
        assert_eq!(
            listen_request_json(None, None, &None, &None, &Some("*".into())),
            json!({"op":"listen","event":"*"})
        );
        assert_eq!(
            listen_request_json(
                None,
                None,
                &Some("switch".into()),
                &None,
                &Some("initial-press".into()),
            ),
            json!({"op":"listen","cluster":"switch","event":"initial-press"})
        );
    }
}
