//! `mat` 自身のエラー型と exit code マッピング。
//!
//! `chip-tool` は失敗時の exit code が粗い（おおむね `1`）ため、`mat` は
//! stdout/stderr をパースして失敗種別を分類し、自身の `ErrorKind` にマップする。

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
    ChildNotFound,
    /// `chip-tool` が失敗終了（分類不能）。
    ChildFailed,
    /// commissioning に失敗。
    CommissionFailed,
    /// 応答待ちタイムアウト。
    Timeout,
    /// ノードに到達できない / ネットワーク不達。
    Unreachable,
    /// デバイスが要求を拒否。
    DeviceRejected,
    /// `chip-tool` 出力をパースできない。
    ParseError,
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
            ErrorKind::Timeout => 3,
            ErrorKind::DeviceRejected => 4,
            ErrorKind::Unreachable => 5,
            ErrorKind::ChildFailed
            | ErrorKind::CommissionFailed
            | ErrorKind::ParseError
            | ErrorKind::Other => 1,
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

    /// stderr に構造化 JSON で1行出す。
    pub fn emit(&self) {
        let body = serde_json::json!({
            "error": { "kind": self.kind, "detail": self.detail }
        });
        eprintln!("{body}");
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
        assert_eq!(ErrorKind::Timeout.exit_code(), 3);
        assert_eq!(ErrorKind::DeviceRejected.exit_code(), 4);
        assert_eq!(ErrorKind::Unreachable.exit_code(), 5);
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
}
