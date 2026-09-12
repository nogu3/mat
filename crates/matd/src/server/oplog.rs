//! op ログ 1 行の level 方針（warn = 経路の問題、info = 要求側の問題 / 遅い成功、debug = 通常成功）。

use serde_json::Value;

use mat_core::error::{ErrorKind, MatError};

use crate::protocol::Op;

/// 成功 op のうち、この時間以上かかったものは info で残す（劣化の前兆）。
/// warm セッションの実測は 71-149ms なので、300ms を超えた成功はもう
/// 「普段と違う」= 弱リンク化 / メッシュ劣化の兆候である。
const SLOW_OP_MS: u64 = 300;

/// op ログの level 方針。ここに集約してテストで釘を打ち、tracing のマクロ
/// 呼び出し側は薄く保つ。
#[derive(Debug, PartialEq, Eq)]
enum OpLogClass {
    /// 経路そのものの問題（warn）。`journalctl -p warning` で劣化だけ抽出できる。
    Failed,
    /// 要求側・意味の問題（info）。warn を汚さない。
    Rejected,
    /// 成功だが遅い（info）。
    Slow,
    /// 通常の成功（debug）。既定 level（info）では出ない。
    Ok,
}

/// `ErrorKind` を網羅 match する — 将来 variant が増えたら level の判断を
/// コンパイラが強制する。
fn classify_op_log(result: &Result<Value, MatError>, elapsed_ms: u64) -> OpLogClass {
    match result {
        Ok(_) if elapsed_ms >= SLOW_OP_MS => OpLogClass::Slow,
        Ok(_) => OpLogClass::Ok,
        Err(e) => match e.kind {
            ErrorKind::Timeout
            | ErrorKind::Unreachable
            | ErrorKind::SessionFailed
            | ErrorKind::Other
            // commission / child 系は matd 経路では発生しないが、網羅 match の
            // ために分類は決めておく（発生したら経路の問題として扱う）。
            | ErrorKind::CommissionFailed
            | ErrorKind::MatdUnavailable
            | ErrorKind::ChildNotFound
            | ErrorKind::ChildFailed => OpLogClass::Failed,
            ErrorKind::StoreMissing
            | ErrorKind::StoreParse
            | ErrorKind::NodeNotCommissioned
            | ErrorKind::DeviceRejected
            | ErrorKind::ParseError => OpLogClass::Rejected,
        },
    }
}

/// op 1 件を 1 行で記録する。
///
/// `Option` のフィールドはそのまま渡す — tracing は `None` のフィールドを
/// 省略するので `node_id=Some(42)` にはならず、`grep node_id=42` が効く。
pub(super) fn log_op(op: &Op, result: &Result<Value, MatError>, elapsed_ms: u64) {
    let op_name = op.name();
    let node_id = op.node_id();
    let group_id = op.group_id();
    let endpoint = op.endpoint();
    let path = op.log_path();
    let path = path.as_deref();
    match result {
        Err(e) => match classify_op_log(result, elapsed_ms) {
            OpLogClass::Rejected => tracing::info!(
                op = op_name, node_id, group_id, endpoint, path, elapsed_ms,
                kind = ?e.kind, detail = %e.detail, "matd op rejected"
            ),
            _ => tracing::warn!(
                op = op_name, node_id, group_id, endpoint, path, elapsed_ms,
                kind = ?e.kind, detail = %e.detail, "matd op failed"
            ),
        },
        Ok(_) => match classify_op_log(result, elapsed_ms) {
            OpLogClass::Slow => tracing::info!(
                op = op_name,
                node_id,
                group_id,
                endpoint,
                path,
                elapsed_ms,
                "matd op slow"
            ),
            _ => tracing::debug!(
                op = op_name,
                node_id,
                group_id,
                endpoint,
                path,
                elapsed_ms,
                "matd op ok"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::protocol::Request;

    fn err(kind: ErrorKind) -> Result<Value, MatError> {
        Err(MatError::new(kind, "test"))
    }

    #[test]
    fn path_failures_are_warn_worthy() {
        for kind in [
            ErrorKind::Timeout,
            ErrorKind::Unreachable,
            ErrorKind::SessionFailed,
            ErrorKind::Other,
            ErrorKind::CommissionFailed,
            ErrorKind::MatdUnavailable,
            ErrorKind::ChildNotFound,
            ErrorKind::ChildFailed,
        ] {
            assert_eq!(
                classify_op_log(&err(kind), 10),
                OpLogClass::Failed,
                "kind: {kind:?}"
            );
        }
    }

    #[test]
    fn request_side_errors_do_not_pollute_warn() {
        for kind in [
            ErrorKind::StoreMissing,
            ErrorKind::StoreParse,
            ErrorKind::NodeNotCommissioned,
            ErrorKind::DeviceRejected,
            ErrorKind::ParseError,
        ] {
            assert_eq!(
                classify_op_log(&err(kind), 10),
                OpLogClass::Rejected,
                "kind: {kind:?}"
            );
        }
    }

    #[test]
    fn slow_threshold_is_inclusive_at_300ms() {
        let ok = Ok(json!({"value": true}));
        assert_eq!(classify_op_log(&ok, 0), OpLogClass::Ok);
        assert_eq!(classify_op_log(&ok, 299), OpLogClass::Ok);
        assert_eq!(classify_op_log(&ok, 300), OpLogClass::Slow);
        assert_eq!(classify_op_log(&ok, 8134), OpLogClass::Slow);
    }

    #[test]
    fn elapsed_time_does_not_change_error_classification() {
        // 失敗は所要時間に関わらず失敗として分類される（速い失敗を
        // 「速いから ok」に見せない）。
        assert_eq!(
            classify_op_log(&err(ErrorKind::Timeout), 0),
            OpLogClass::Failed
        );
        assert_eq!(
            classify_op_log(&err(ErrorKind::NodeNotCommissioned), 9999),
            OpLogClass::Rejected
        );
    }

    /// `log_op` の出力を直接検証するための writer。`Arc<Mutex<Vec<u8>>>` を
    /// 包むだけの薄いラッパで、新規依存は増やさない
    /// （`tracing-subscriber` は matd の dev-dependency なのでテストからは使える）。
    #[derive(Clone)]
    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// `log_op` を自前の subscriber の下で 1 回呼び、書き出された生テキストを
    /// 返す。ANSI の有無はここでは検証しない — 検証するのは自前で組み立てた
    /// この subscriber の出力であって、matd 本体が `main.rs` で設定する
    /// `with_ansi(false)` そのものではない。
    fn capture_log_op(op: &Op, result: &Result<Value, MatError>, elapsed_ms: u64) -> String {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = CapturingWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_op(op, result, elapsed_ms);
        });
        let captured = buf.lock().unwrap().clone();
        String::from_utf8(captured).unwrap()
    }

    /// node_id=6（実在ノードを書かない規律 — README のプレースホルダと同じ番号）
    /// の read op。
    fn read_op() -> Op {
        serde_json::from_str::<Request>(
            r#"{"op":"read","node_id":6,"endpoint":1,"cluster":"onoff","attribute":"on-off"}"#,
        )
        .unwrap()
        .op
    }

    #[test]
    fn log_op_omits_absent_fields_entirely() {
        // Op::Ping は node_id / endpoint / group_id / path を持たない op。
        // `node_id` が `?op.node_id()` のような Debug 整形に書き換えられて
        // いたら "Some(" やフィールド名自体が出てしまう — grep node_id=42 を
        // 壊す退行をここで釘で留める。
        let out = capture_log_op(&Op::Ping, &Ok(json!({"value": true})), 0);
        assert!(!out.contains("Some("), "output: {out}");
        assert!(!out.contains("node_id"), "output: {out}");
        assert!(!out.contains("path"), "output: {out}");
    }

    #[test]
    fn log_op_emits_grep_friendly_fields_on_ok() {
        let out = capture_log_op(&read_op(), &Ok(json!({"value": true})), 0);
        // 引用符も Some() も付かない生の "node_id=6" — これが
        // `grep node_id=42` の効く形。
        assert!(out.contains("node_id=6"), "output: {out}");
        assert!(out.contains(r#"path="onoff/on-off""#), "output: {out}");
        assert!(out.contains("matd op ok"), "output: {out}");
    }

    #[test]
    fn log_op_level_and_message_follow_classification() {
        // class（Slow/Ok/Rejected/Failed）→ level/message の対応が
        // 実際に効いていることを確かめる（classify_op_log 単体のテストは
        // 既にあるが、log_op がそれを使っているかは別の話）。
        let slow = capture_log_op(&read_op(), &Ok(json!({"value": true})), 300);
        assert!(slow.contains("matd op slow"), "output: {slow}");

        let failed = capture_log_op(
            &read_op(),
            &Err(MatError::new(ErrorKind::Timeout, "no ack")),
            0,
        );
        assert!(failed.contains("matd op failed"), "output: {failed}");
        assert!(failed.contains("kind=Timeout"), "output: {failed}");
        assert!(failed.contains("detail="), "output: {failed}");
    }
}
