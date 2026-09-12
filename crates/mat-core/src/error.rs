//! `mat` 自身のエラー型と exit code マッピング。
//!
//! mat は native backend から構造化エラー（`ErrorKind` と詳細情報）を受け取る。
//! 0.22.0 以降、chip-tool パーサは廃止された（フルスクラッチ Rust コントローラ
//! `mat-controller` が backend を担当）。

use serde::{Deserialize, Serialize};

/// `mat` の機械可読エラー種別。stderr に `{"error":{"kind","detail"}}` で出す。
/// `Deserialize` は matd 応答の `error.kind` を exit code へ逆引きするのに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// 認証情報ストアが存在しない。
    StoreMissing,
    /// 認証情報ストアのパースに失敗。
    StoreParse,
    /// 指定 node_id がストアに無い（未 commission）。
    NodeNotCommissioned,
    /// `chip-tool` バイナリが見つからない / 実行不可。
    /// トップレベルの CLI エラーとしては 0.22.0 以降 emit されない（chip-tool
    /// 撤去、exit code 12 は歴史的欠番）。`mat` は `diag node --deep` の内部で
    /// ping6 バイナリ不在を `unavailable` 配列の `tool_missing` エントリへ
    /// 吸収するためだけに構築する（exit 12 にはならない）。exit code 12 の
    /// マッピングと wire 互換のため variant は残置 — README で撤去を告知。
    ChildNotFound,
    /// `chip-tool` が失敗終了（分類不能）。
    /// 0.22.0 以降どちらのバイナリからも emit されない — `mat`（M8c-3 Task9）に続き
    /// `matd`（M8c-3 Task10）も chip-tool 経路を完全撤去し、これが最後の emitter
    /// だった。exit code 1 のマッピングと wire 互換のため variant 自体は残置する
    /// （旧 `mat`/`matd` が返した過去の応答を deserialize できるように）。
    ChildFailed,
    /// commissioning に失敗。
    CommissionFailed,
    /// 応答待ちタイムアウト。
    Timeout,
    /// ノードに到達できない / ネットワーク不達。
    Unreachable,
    /// IP は届くが CASE（運用セキュアセッション）確立に失敗。
    /// Sigma 交換段階で落ちる間欠失敗など。`unreachable`（IP 不達）とも
    /// `device_rejected`（デバイス拒否）とも異なる、リトライ可能なセッション失敗。
    SessionFailed,
    /// デバイスが要求を拒否。
    DeviceRejected,
    /// `chip-tool` 出力をパースできない。
    ParseError,
    /// matd が利用できない。`mat listen`（常駐リスナ必須・バインド失敗含む）に
    /// 加え、1.0.0 から matd 経路の途中失敗（強制 matd の接続失敗・送受信の
    /// I/O 断・応答なし切断）にも使う。
    /// exit code 12 は歴史的欠番（chip-tool 撤去）のため、13 を割当。
    MatdUnavailable,
    /// その他。
    Other,
}

impl ErrorKind {
    /// プロセス終了コード。CLAUDE.md の表に従う。
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorKind::StoreMissing | ErrorKind::StoreParse => 10,
            ErrorKind::NodeNotCommissioned => 11,
            ErrorKind::ChildNotFound => 12,
            ErrorKind::MatdUnavailable => 13,
            ErrorKind::Timeout => 3,
            ErrorKind::DeviceRejected => 4,
            ErrorKind::Unreachable => 5,
            ErrorKind::SessionFailed => 6,
            ErrorKind::ChildFailed
            | ErrorKind::CommissionFailed
            | ErrorKind::ParseError
            | ErrorKind::Other => 1,
        }
    }

    /// matd 応答 / admin 応答の `error.kind` を `ErrorKind` へ逆引きする。
    /// 未知の kind（新しい matd / 壊れた応答）は warn を 1 行出して `Other`
    /// （exit 1）に倒す。`mat` の `matd_client::emit_response` と `matd` の
    /// `admin_response_to_result` が同じ規律を共有する（逐語コピーの一本化）。
    pub fn from_wire(kind: Option<&serde_json::Value>) -> ErrorKind {
        match kind.and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok()) {
            Some(k) => k,
            None => {
                let raw_kind = kind.cloned().unwrap_or(serde_json::Value::Null);
                tracing::warn!(
                    kind = %raw_kind,
                    "unknown error kind from matd; mapping to `other` for the exit code"
                );
                ErrorKind::Other
            }
        }
    }
}

/// `mat` のエラー。`kind` で分岐、`detail` は AI がリカバリ判断できる粒度の説明。
#[derive(Debug, Clone)]
pub struct MatError {
    pub kind: ErrorKind,
    pub detail: String,
}

impl MatError {
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        MatError {
            kind,
            detail: detail.into(),
        }
    }

    pub fn store_missing(detail: impl Into<String>) -> Self {
        MatError::new(ErrorKind::StoreMissing, detail)
    }

    pub fn store_parse(detail: impl Into<String>) -> Self {
        MatError::new(ErrorKind::StoreParse, detail)
    }

    /// read/write/invoke が未 commission node 参照時に使う。
    pub fn node_not_commissioned(node_id: u64) -> Self {
        MatError::new(
            ErrorKind::NodeNotCommissioned,
            format!("Node {node_id} is not commissioned (absent from store)"),
        )
    }

    pub fn child_not_found(detail: impl Into<String>) -> Self {
        MatError::new(ErrorKind::ChildNotFound, detail)
    }

    pub fn parse_error(detail: impl Into<String>) -> Self {
        MatError::new(ErrorKind::ParseError, detail)
    }

    /// 名前解決できない op（未知の cluster/attribute/command 名、または非スカラー型）。
    /// M8c-3 の chip-tool 撤去でフォールバック先が無くなったため数値 ID 以外は拒否
    /// する。mat 直経路と matd が同一文言を共有する（逐語コピーの一本化）。
    pub fn unresolved_op() -> Self {
        MatError::parse_error(
            "unknown cluster/attribute/command name (or unsupported non-scalar type); \
             numeric IDs are accepted",
        )
    }

    /// group 送信不能（未 provision・KVS 不備等）。理由文字列に
    /// `mat group provision` 誘導を含む（`mat_native::group` 由来）。
    pub fn group_unavailable(reason: &str) -> Self {
        MatError::store_parse(format!("native group send unavailable: {reason}"))
    }

    /// group ctx / group_settings ctx 未構成（本番 `Engine::build` では常に `Some`
    /// なので実質到達しない — テスト注入時のみ）。
    pub fn group_ctx_unconfigured() -> Self {
        MatError::new(
            ErrorKind::Other,
            "native group context not configured (internal)",
        )
    }

    /// `{"error":{"kind","detail"}}` ボディ。stderr emit と matd 応答が共有する。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "error": { "kind": self.kind, "detail": self.detail } })
    }

    /// stderr に構造化 JSON で1行出す。
    pub fn emit(&self) {
        eprintln!("{}", self.to_json());
    }

    /// `emit()` して、この kind の exit code を `ExitCode` で返す。CLI の
    /// `Err(e) => { e.emit(); ExitCode::from(e.kind.exit_code()) }` の定型を畳む。
    pub fn emit_exit(&self) -> std::process::ExitCode {
        self.emit();
        std::process::ExitCode::from(self.kind.exit_code())
    }

    /// エンジン構築失敗の写像: `store_missing` に「`mat fabric init` で資材を
    /// 作れ」の誘導を足す（二重付与はしない）。他 kind はそのまま。`mat` の
    /// 直経路と `matd` 起動時の両方が使う。
    pub fn with_fabric_init_hint(mut self) -> Self {
        if self.kind == ErrorKind::StoreMissing && !self.detail.contains("mat fabric init") {
            self.detail = format!(
                "{} — run `mat fabric init` to bootstrap the credential store",
                self.detail
            );
        }
        self
    }
}

impl std::fmt::Display for MatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for MatError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_spec() {
        assert_eq!(ErrorKind::StoreMissing.exit_code(), 10);
        assert_eq!(ErrorKind::StoreParse.exit_code(), 10);
        assert_eq!(ErrorKind::NodeNotCommissioned.exit_code(), 11);
        assert_eq!(ErrorKind::ChildNotFound.exit_code(), 12);
        assert_eq!(ErrorKind::MatdUnavailable.exit_code(), 13);
        assert_eq!(ErrorKind::Timeout.exit_code(), 3);
        assert_eq!(ErrorKind::DeviceRejected.exit_code(), 4);
        assert_eq!(ErrorKind::Unreachable.exit_code(), 5);
        assert_eq!(ErrorKind::SessionFailed.exit_code(), 6);
        assert_eq!(ErrorKind::ChildFailed.exit_code(), 1);
        assert_eq!(ErrorKind::CommissionFailed.exit_code(), 1);
        assert_eq!(ErrorKind::ParseError.exit_code(), 1);
        assert_eq!(ErrorKind::Other.exit_code(), 1);
    }

    #[test]
    fn kind_serializes_snake_case() {
        let s = serde_json::to_string(&ErrorKind::NodeNotCommissioned).unwrap();
        assert_eq!(s, "\"node_not_commissioned\"");
    }

    #[test]
    fn matd_unavailable_is_exit_13_snake_case() {
        assert_eq!(ErrorKind::MatdUnavailable.exit_code(), 13);
        assert_eq!(
            serde_json::to_string(&ErrorKind::MatdUnavailable).unwrap(),
            "\"matd_unavailable\""
        );
    }

    #[test]
    fn emit_exit_maps_kind_to_exit_code() {
        let e = MatError::new(ErrorKind::NodeNotCommissioned, "x");
        assert_eq!(e.emit_exit(), std::process::ExitCode::from(11));
        assert_eq!(
            MatError::new(ErrorKind::Timeout, "x").emit_exit(),
            std::process::ExitCode::from(3)
        );
    }

    #[test]
    fn fabric_init_hint_is_added_once_and_only_for_store_missing() {
        let e = MatError::store_missing("no KVS").with_fabric_init_hint();
        assert_eq!(
            e.detail,
            "no KVS — run `mat fabric init` to bootstrap the credential store"
        );
        // idempotent: a detail that already carries the hint is left alone.
        let again = e.clone().with_fabric_init_hint();
        assert_eq!(again.detail, e.detail);
        // other kinds are untouched.
        let other = MatError::new(ErrorKind::Unreachable, "node 5").with_fabric_init_hint();
        assert_eq!(other.detail, "node 5");
    }

    #[test]
    fn from_wire_decodes_known_kind_and_falls_back_to_other() {
        assert_eq!(
            ErrorKind::from_wire(Some(&serde_json::json!("store_missing"))),
            ErrorKind::StoreMissing
        );
        assert_eq!(
            ErrorKind::from_wire(Some(&serde_json::json!("not_a_kind_we_know"))),
            ErrorKind::Other
        );
        assert_eq!(ErrorKind::from_wire(None), ErrorKind::Other);
    }

    #[test]
    fn shared_helper_wordings_are_stable() {
        assert_eq!(
            MatError::unresolved_op().detail,
            "unknown cluster/attribute/command name (or unsupported non-scalar type); numeric IDs are accepted"
        );
        assert_eq!(
            MatError::group_unavailable("no keyset").detail,
            "native group send unavailable: no keyset"
        );
        assert_eq!(
            MatError::group_ctx_unconfigured().detail,
            "native group context not configured (internal)"
        );
        assert_eq!(
            MatError::unresolved_op().to_json().to_string(),
            r#"{"error":{"detail":"unknown cluster/attribute/command name (or unsupported non-scalar type); numeric IDs are accepted","kind":"parse_error"}}"#
        );
    }
}
