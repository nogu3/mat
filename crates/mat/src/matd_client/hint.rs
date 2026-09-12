//! matd への片方向ヒント: `reload`（IPK 再読込、rotate-ipk が送る）と
//! `hint node-touched`（直経路で触ったノードの購読を促す）。matd 不在は黙って
//! 無視する（直経路の成否に影響させない）。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use super::{connect_candidates, sockets_from_env_or_default};

/// `mat fabric rotate-ipk` が commit 後に matd へ送る `reload` ヒントの結果
/// （body の `matd_reload`）。`node_touched` と同じ best-effort 送信路だが、
/// こちらは応答を読んで状態語にする — 操作者が「稼働中の matd が新 IPK を
/// 取り込んだか」を rotate の出力だけで判断できるようにするため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatdReload {
    /// matd が `{"reloaded":true}` を返した。
    Reloaded,
    /// 候補 socket のどれにも接続できない（matd 不在）。
    NotRunning,
    /// 接続はできたが ack が無い: エラー応答（旧 matd の `parse_error` を含む）、
    /// timeout、非 JSON。
    Failed,
}

impl MatdReload {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MatdReload::Reloaded => "reloaded",
            MatdReload::NotRunning => "not_running",
            MatdReload::Failed => "failed",
        }
    }
}

/// reload ヒントの応答 1 行を読む上限。`node_touched`（読み捨て 300 ms）と違い
/// この応答は `matd_reload` として報告するので待つ価値がある。matd 側の reload は
/// KVS の読み直し + NOC 自己発行（P-256 署名）を伴うため 300 ms では足りないことが
/// ある。
const RELOAD_ACK_TIMEOUT: Duration = Duration::from_millis(1_500);

/// 稼働中の matd に資格情報（IPK）の読み直しを頼む（`mat fabric rotate-ipk` の
/// commit 後）。socket 候補・接続失敗の扱いは [`hint_node_touched`] と同じ。
/// read 上限だけは 1500 ms（[`RELOAD_ACK_TIMEOUT`]）。結果は呼び出し側の
/// exit code に影響しない。
pub(crate) fn hint_reload() -> MatdReload {
    let sockets = sockets_from_env_or_default(std::env::var_os("MAT_MATD_SOCKET"));
    hint_reload_at(&sockets)
}

/// [`hint_reload`] の socket 候補注入版（テスト用に env 非依存の核）。
fn hint_reload_at(sockets: &[PathBuf]) -> MatdReload {
    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            tracing::debug!(error = %detail, "reload hint: matd unreachable");
            return MatdReload::NotRunning;
        }
    };
    match send_reload_line(stream) {
        Ok(ReloadAck::Reloaded) => MatdReload::Reloaded,
        Ok(ReloadAck::ReloadedIpkUnchanged) => {
            // commit 済みローテーションの直後なのに IPK が動いていない = その
            // matd は別の store / 別の fabric index を見ている疑いが濃い（同じ
            // ものを見ていれば必ず changed になる）。語彙は 3 値のままなので
            // body は `reloaded`、注意喚起は stderr のログで出す。
            tracing::warn!(
                socket = %socket.display(),
                "matd reloaded but reports the IPK unchanged — is it serving the same store and fabric index as this rotation?"
            );
            MatdReload::Reloaded
        }
        Ok(ReloadAck::NoAck) => {
            tracing::warn!(socket = %socket.display(), "reload hint: matd did not acknowledge (old matd, or reload failed — run `matd reload`)");
            MatdReload::Failed
        }
        Err(e) => {
            tracing::warn!(socket = %socket.display(), error = %e, "reload hint: send/recv failed");
            MatdReload::Failed
        }
    }
}

/// matd の reload 応答 1 行の解釈。`MatdReload` の 3 値（wire 語彙）とは別で、
/// 「ack はしたが IPK が変わっていない」を warn 用に切り分けるための内部型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadAck {
    /// `reloaded: true`（`ipk` は `changed`、または欄が無い旧応答）。
    Reloaded,
    /// `reloaded: true` かつ `ipk: "unchanged"`。
    ReloadedIpkUnchanged,
    /// ack 無し: エラー応答（旧 matd の `parse_error` を含む）、`reloaded` が
    /// 無い/false、非 JSON、timeout。
    NoAck,
}

/// 応答 1 行を [`ReloadAck`] に写す（純関数 — テストはここを突く）。
fn classify_reload_ack(resp: &str) -> ReloadAck {
    let Ok(v) = serde_json::from_str::<Value>(resp) else {
        return ReloadAck::NoAck;
    };
    if v.get("reloaded").and_then(Value::as_bool) != Some(true) {
        return ReloadAck::NoAck;
    }
    // `unchanged` と明示されたときだけ切り分ける（欄が無い応答を推測しない）。
    if v.get("ipk").and_then(Value::as_str) == Some("unchanged") {
        ReloadAck::ReloadedIpkUnchanged
    } else {
        ReloadAck::Reloaded
    }
}

/// `{"op":"reload"}` を 1 行送り、応答 1 行を [`classify_reload_ack`] で分類する。
/// timeout は `NoAck`（送受信自体は成立した）、I/O エラーは Err。
fn send_reload_line(stream: UnixStream) -> std::io::Result<ReloadAck> {
    Ok(
        match request_line(stream, &json!({ "op": "reload" }), RELOAD_ACK_TIMEOUT)? {
            Some(resp) => classify_reload_ack(&resp),
            None => ReloadAck::NoAck,
        },
    )
}

/// 直経路 op（native_direct）完了後、matd がいれば `node_touched` ヒントを送る
/// fire-and-forget 通知（Issue #20）。常駐購読が古いセッションを掴んだままに
/// なるのを防ぐための best-effort で、matd 不在・旧 matd（`parse_error` 応答）・
/// タイムアウトなど全ての失敗は呼び出し側（native_direct の op 結果 / exit code）
/// に一切影響させない（`tracing::debug!` のみ）。`attach_deadline` /
/// `emit_response` は使わない専用送信路。
pub(crate) fn hint_node_touched(node_id: u64) {
    let sockets = sockets_from_env_or_default(std::env::var_os("MAT_MATD_SOCKET"));
    hint_node_touched_at(&sockets, node_id);
}

/// [`hint_node_touched`] の socket 候補注入版（テスト用に env 非依存の核）。
fn hint_node_touched_at(sockets: &[PathBuf], node_id: u64) {
    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            tracing::debug!(node_id, error = %detail, "node_touched hint: matd unreachable");
            return;
        }
    };
    if let Err(e) = send_hint_line(stream, node_id) {
        tracing::debug!(
            node_id,
            socket = %socket.display(),
            error = %e,
            "node_touched hint: send/recv failed"
        );
    }
}

/// `{"op":"node_touched","node_id":N}` を 1 行送り、応答を 1 行読み捨てる。
///
/// ブロッキング I/O（std `UnixStream` の read timeout）を使うのは、呼び出し元が
/// one-shot CLI の終了間際で他に走っている非同期タスクが無いため（async 化す
/// る価値が無い）。応答本体には関心が無い（ack `{"resubscribing":true}` でも
/// 旧 matd の `parse_error` でも同じ扱い）ので、300ms 上限で読み捨てるだけで
/// 十分（matd 応答が来ない＝matd 停止/ハング相当、これ以上待つ理由が無い）。
fn send_hint_line(stream: UnixStream, node_id: u64) -> std::io::Result<()> {
    request_line(
        stream,
        &json!({ "op": "node_touched", "node_id": node_id }),
        Duration::from_millis(300),
    )
    .map(|_| ())
}

/// `op` を 1 行送り、応答 1 行を `read_timeout` まで待つ。write 失敗は Err、
/// read 失敗（timeout 等）は `Ok(None)`（送受信自体は成立 — 「応答なし」）。
/// `node_touched` と `reload` の 2 本のヒント送信路が共有する核。
fn request_line(
    mut stream: UnixStream,
    op: &Value,
    read_timeout: Duration,
) -> std::io::Result<Option<String>> {
    let mut line = serde_json::to_vec(op)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.set_read_timeout(Some(read_timeout))?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    Ok(reader.read_line(&mut resp).ok().map(|_| resp))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// listener を 1 本立て、受けた要求行と固定応答を返すテスト用 matd。
    fn one_shot_matd(
        reply: &'static [u8],
    ) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap();
            let mut conn = conn;
            conn.write_all(reply).unwrap();
            req
        });
        (dir, path, server)
    }

    /// Issue #20: 直経路 op 後の fire-and-forget ヒント。matd がいれば
    /// `{"op":"node_touched","node_id":N}` を 1 行送る（応答は読み捨て）。
    #[test]
    fn hint_node_touched_sends_op_line_to_matd() {
        let (_dir, path, server) = one_shot_matd(b"{\"resubscribing\":true}\n");
        hint_node_touched_at(&[path], 42);
        let v: Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(v, json!({"op":"node_touched","node_id":42}));
    }

    /// 旧 matd（node_touched 未対応）は `parse_error` を返してくるが、
    /// ヒント送信側はその応答も内容を見ずに読み捨てるだけで完走する。
    #[test]
    fn hint_node_touched_ignores_old_matd_parse_error_response() {
        let (_dir, path, server) =
            one_shot_matd(b"{\"error\":{\"kind\":\"parse_error\",\"detail\":\"unknown op\"}}\n");
        hint_node_touched_at(&[path], 7); // panic せず完走すること
        server.join().unwrap();
    }

    /// matd 不在（socket に誰もいない）でも panic せず、戻り値なしで完走する。
    #[test]
    fn hint_node_touched_is_silent_without_matd() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such.sock");
        hint_node_touched_at(&[missing], 1);
    }

    #[test]
    fn hint_reload_reports_reloaded_on_ack() {
        let (_dir, path, server) =
            one_shot_matd(b"{\"reloaded\":true,\"ipk\":\"changed\",\"reload_count\":1}\n");
        assert_eq!(hint_reload_at(&[path]), MatdReload::Reloaded);
        let req: Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(req, json!({"op": "reload"}));
    }

    /// 旧 matd（reload 未対応）の parse_error、その他のエラー応答は failed。
    #[test]
    fn hint_reload_reports_failed_on_error_response() {
        let (_dir, path, server) =
            one_shot_matd(b"{\"error\":{\"kind\":\"parse_error\",\"detail\":\"unknown op\"}}\n");
        assert_eq!(hint_reload_at(&[path]), MatdReload::Failed);
        server.join().unwrap();
    }

    #[test]
    fn hint_reload_reports_failed_on_non_json_response() {
        let (_dir, path, server) = one_shot_matd(b"garbage\n");
        assert_eq!(hint_reload_at(&[path]), MatdReload::Failed);
        server.join().unwrap();
    }

    /// 別 store / 別 fabric index を見ている matd は commit 済みローテーション
    /// の後でも `ipk: "unchanged"` を返す。語彙は 3 値のままなので結果は
    /// `Reloaded`（warn は stderr に出るだけ）。
    #[test]
    fn hint_reload_is_still_reloaded_when_matd_reports_ipk_unchanged() {
        let (_dir, path, server) =
            one_shot_matd(b"{\"reloaded\":true,\"ipk\":\"unchanged\",\"reload_count\":3}\n");
        assert_eq!(hint_reload_at(&[path]), MatdReload::Reloaded);
        server.join().unwrap();
    }

    /// 応答 1 行の分類（warn 判定の核）。
    #[test]
    fn classify_reload_ack_separates_the_unchanged_ipk_case() {
        assert_eq!(
            classify_reload_ack(r#"{"reloaded":true,"ipk":"changed","reload_count":1}"#),
            ReloadAck::Reloaded
        );
        assert_eq!(
            classify_reload_ack(r#"{"reloaded":true,"ipk":"unchanged"}"#),
            ReloadAck::ReloadedIpkUnchanged
        );
        // ipk 欄が無い応答は warn しない（unchanged と断定できない）。
        assert_eq!(
            classify_reload_ack(r#"{"reloaded":true}"#),
            ReloadAck::Reloaded
        );
        // ack 無し: エラー応答 / false / 非 JSON。
        assert_eq!(
            classify_reload_ack(r#"{"error":{"kind":"parse_error","detail":"unknown op"}}"#),
            ReloadAck::NoAck
        );
        assert_eq!(
            classify_reload_ack(r#"{"reloaded":false,"ipk":"unchanged"}"#),
            ReloadAck::NoAck
        );
        assert_eq!(classify_reload_ack("garbage"), ReloadAck::NoAck);
    }

    #[test]
    fn hint_reload_reports_not_running_without_matd() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such.sock");
        assert_eq!(hint_reload_at(&[missing]), MatdReload::NotRunning);
    }

    #[test]
    fn matd_reload_as_str_is_the_wire_vocabulary() {
        assert_eq!(MatdReload::Reloaded.as_str(), "reloaded");
        assert_eq!(MatdReload::NotRunning.as_str(), "not_running");
        assert_eq!(MatdReload::Failed.as_str(), "failed");
    }
}
