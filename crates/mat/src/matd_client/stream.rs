//! unix socket の接続（候補ソケットの順次試行）、1 行リクエスト / 1 行レスポンスの
//! 往復、レスポンス（mat スキーマ）の stdout / stderr への出力。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;

use mat_core::error::{ErrorKind, MatError};

/// 候補 socket へ順に connect し、最初に成功した stream と使用パスを返す。
/// 全滅は Err（試行した全パスと各エラーを列挙 — Forced 経路のエラー detail 用）。
pub(super) fn connect_candidates(sockets: &[PathBuf]) -> Result<(UnixStream, &Path), String> {
    let mut attempts = Vec::new();
    for socket in sockets {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok((stream, socket)),
            Err(e) => attempts.push(format!("{} ({e})", socket.display())),
        }
    }
    Err(format!(
        "could not connect to matd at {}",
        attempts.join(", ")
    ))
}

/// 接続済み stream で 1 行送り 1 行受け取る（自動検出は probe した接続を使い回す）。
///
/// v1 品質修正 3: 途中失敗を typed error 化。送受信の I/O 断・応答なし切断は
/// 「matd がいなくなった」= `matd_unavailable`（送信後はリクエストが実行済みの
/// 可能性があるので detail で明示）。応答が JSON でないのは `parse_error`。
pub(super) fn exchange_on_stream(
    mut stream: UnixStream,
    op: &Value,
    read_timeout: Option<Duration>,
) -> Result<Value, MatError> {
    let mut line = serde_json::to_vec(op)
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to encode request: {e}")))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(|e| {
        MatError::new(
            ErrorKind::MatdUnavailable,
            format!("failed to send request to matd: {e}"),
        )
    })?;

    if let Some(t) = read_timeout {
        stream.set_read_timeout(Some(t)).map_err(|e| {
            MatError::new(ErrorKind::Other, format!("failed to set read timeout: {e}"))
        })?;
    }

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    let n = reader.read_line(&mut resp).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            MatError::new(
                ErrorKind::Timeout,
                format!("no response from matd within the op budget: {e}; the request may have been executed"),
            )
        } else {
            MatError::new(
                ErrorKind::MatdUnavailable,
                format!("failed to read response from matd: {e}; the request may have been executed"),
            )
        }
    })?;
    if n == 0 {
        return Err(MatError::new(
            ErrorKind::MatdUnavailable,
            "matd closed the connection without responding; the request may have been executed",
        ));
    }
    serde_json::from_str(&resp)
        .map_err(|e| MatError::parse_error(format!("matd response was not JSON: {e}; body={resp}")))
}

/// matd 応答を mat の規約どおり出力する: 成功は stdout、エラーは stderr。exit code は
/// error.kind から逆引きする（matd と mat で ErrorKind 表が共通）。
pub(super) fn emit_response(resp: Value) -> ExitCode {
    if let Some(err) = resp.get("error") {
        eprintln!("{resp}");
        let kind = match err
            .get("kind")
            .and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok())
        {
            Some(k) => k,
            None => {
                let raw_kind = err.get("kind").cloned().unwrap_or(Value::Null);
                tracing::warn!(
                    kind = %raw_kind,
                    "unknown error kind from matd; mapping to `other` for the exit code"
                );
                ErrorKind::Other
            }
        };
        ExitCode::from(kind.exit_code())
    } else {
        println!("{resp}");
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn connect_candidates_falls_through_to_second_socket() {
        // 候補 1 = 存在しないパス、候補 2 = 生きた listener → 候補 2 で繋がる。
        let dir = tempfile::tempdir().unwrap();
        let dead = dir.path().join("matd").join("matd.sock"); // 不在（dir ごと無い）
        let alive = dir.path().join("matd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&alive).unwrap();

        // 戻り値の &Path は候補スライスを借用するため、候補は変数に束縛してから渡す。
        let candidates = [dead, alive.clone()];
        let (_stream, used) = connect_candidates(&candidates).expect("second candidate connects");
        assert_eq!(used, alive.as_path());
    }

    #[test]
    fn connect_candidates_skips_stale_socket_file() {
        // 候補 1 = stale socket ファイル（listener 死亡済み）→ connect 失敗で候補 2 へ。
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap()); // ファイルは残る
        assert!(stale.exists());
        let alive = dir.path().join("alive.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&alive).unwrap();

        let candidates = [stale, alive.clone()];
        let (_stream, used) = connect_candidates(&candidates).expect("stale is skipped");
        assert_eq!(used, alive.as_path());
    }

    #[test]
    fn connect_candidates_error_lists_all_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.sock");
        let b = dir.path().join("b.sock");
        let err = connect_candidates(&[a.clone(), b.clone()]).unwrap_err();
        assert!(err.contains(&a.display().to_string()), "got: {err}");
        assert!(err.contains(&b.display().to_string()), "got: {err}");
    }

    /// v1 品質修正 3: matd 経路の途中失敗が一律 `other` だったのを分離。
    /// 応答なし切断（EOF）= matd 側が死んだ → `matd_unavailable`(exit 13)。
    #[test]
    fn exchange_on_stream_maps_eof_to_matd_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            // リクエスト行を消費してから切断する（先にドロップすると client の
            // write_all がリクエスト到達前に broken pipe で失敗し得るため、EOF-on-read
            // を確実に踏ませるにはここで 1 行読んでおく必要がある）。
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap();
            drop(conn); // 1 行も返さず切断 → クライアント側は EOF
        });
        let stream = UnixStream::connect(&path).unwrap();
        let err = exchange_on_stream(stream, &json!({ "op": "on" }), None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::MatdUnavailable);
        assert!(
            err.detail.contains("may have been executed"),
            "detail should warn about possible partial execution: {}",
            err.detail
        );
        server.join().unwrap();
    }

    /// 応答は来たが JSON でない → `parse_error`（native 経路の出力不能時と同じ分類）。
    #[test]
    fn exchange_on_stream_maps_non_json_response_to_parse_error() {
        use std::io::{BufRead as _, BufReader, Write as _};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap(); // リクエスト 1 行を消費
            let mut conn = conn;
            conn.write_all(b"garbage\n").unwrap();
        });
        let stream = UnixStream::connect(&path).unwrap();
        let err = exchange_on_stream(stream, &json!({ "op": "on" }), None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        server.join().unwrap();
    }

    #[test]
    fn exchange_read_timeout_maps_to_timeout_kind() {
        // 応答しないサーバ相手に read timeout → ErrorKind::Timeout（exit 3）。
        let (client, _server) = UnixStream::pair().unwrap();
        let err = exchange_on_stream(
            client,
            &json!({"op":"ping"}),
            Some(std::time::Duration::from_millis(100)),
        )
        .expect_err("must time out");
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(
            err.detail.contains("may have been executed"),
            "detail: {}",
            err.detail
        );
    }
}
