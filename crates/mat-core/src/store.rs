//! 認証情報ストア（KVS）。
//!
//! `mat` が持つ唯一の永続状態。Root CA / controller 鍵・証明書・commission 済み
//! ノードの台帳・`chip-tool` の永続ストレージをこのディレクトリ配下に置く。
//! 認証情報はリポジトリで管理しない（`.gitignore` で除外）。
//!
//! 配置の優先順位: `--store` > `MAT_STORE` > `$XDG_CONFIG_HOME/mat` > `~/.config/mat`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, MatError};

/// commission 済みノード1件の台帳エントリ。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeRecord {
    pub node_id: u64,
    /// commission したアドレス（IP / DNS-SD ホスト）。診断用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// commission 完了時刻（ISO 8601）。
    pub commissioned_at: String,
}

/// nodes.json のスキーマ。`chip-tool` 自身の鍵束とは別に `mat` が持つメタ台帳。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Ledger {
    #[serde(default = "ledger_version")]
    version: u32,
    #[serde(default)]
    nodes: BTreeMap<u64, NodeRecord>,
}

fn ledger_version() -> u32 {
    1
}

/// 認証情報ストアのハンドル。
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
    ledger: Ledger,
}

impl Store {
    /// ストアのルートディレクトリを優先順位に従って決定する。
    pub fn locate(cli_store: Option<PathBuf>) -> PathBuf {
        if let Some(p) = cli_store {
            return p;
        }
        if let Some(p) = std::env::var_os("MAT_STORE") {
            return PathBuf::from(p);
        }
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(x).join("mat");
        }
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(".config").join("mat");
        }
        // 最終フォールバック（HOME 無し環境）: カレント配下。
        PathBuf::from(".config/mat")
    }

    /// 既存ストアを開く。存在しなければ [`ErrorKind::StoreMissing`]（exit 10）。
    ///
    /// 認証情報必須の経路（read/write/invoke/describe 等）が使う。bootstrap して
    /// よい discover/commission は [`Store::open_or_init`] を使う。
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, MatError> {
        let root = root.into();
        if !root.is_dir() {
            return Err(MatError::store_missing(format!(
                "credential store not found at {} (run `mat fabric init` to bootstrap, or pass --store)",
                root.display()
            )));
        }
        let ledger = Self::load_ledger(&root)?;
        Ok(Store { root, ledger })
    }

    /// 既存ストアを開く。無ければ bootstrap（ディレクトリ + 空台帳）して開く。
    /// `mat commission` の初回など、ストアを作ってよい経路で使う。
    pub fn open_or_init(root: impl Into<PathBuf>) -> Result<Self, MatError> {
        let root = root.into();
        if !root.is_dir() {
            std::fs::create_dir_all(&root).map_err(|e| {
                MatError::new(
                    ErrorKind::Other,
                    format!("failed to create store dir {}: {e}", root.display()),
                )
            })?;
            tracing::debug!(path = %root.display(), "bootstrapped credential store");
        }
        let ledger = Self::load_ledger(&root)?;
        Ok(Store { root, ledger })
    }

    fn ledger_path(root: &Path) -> PathBuf {
        root.join("nodes.json")
    }

    fn load_ledger(root: &Path) -> Result<Ledger, MatError> {
        let path = Self::ledger_path(root);
        if !path.exists() {
            return Ok(Ledger {
                version: ledger_version(),
                nodes: BTreeMap::new(),
            });
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| MatError::store_parse(format!("cannot read {}: {e}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|e| MatError::store_parse(format!("cannot parse {}: {e}", path.display())))
    }

    fn save_ledger(&self) -> Result<(), MatError> {
        let path = Self::ledger_path(&self.root);
        let text = serde_json::to_string_pretty(&self.ledger).map_err(|e| {
            MatError::new(ErrorKind::Other, format!("cannot serialize ledger: {e}"))
        })?;
        std::fs::write(&path, text).map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("cannot write {}: {e}", path.display()),
            )
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// commission 済みノード一覧（node_id 昇順）。
    pub fn nodes(&self) -> impl Iterator<Item = &NodeRecord> {
        self.ledger.nodes.values()
    }

    #[allow(dead_code)]
    pub fn contains(&self, node_id: u64) -> bool {
        self.ledger.nodes.contains_key(&node_id)
    }

    /// ノードを取得。未 commission なら [`ErrorKind::NodeNotCommissioned`]（exit 11）。
    ///
    /// read/write/invoke/describe が node_id 解決に使う。
    pub fn require_node(&self, node_id: u64) -> Result<&NodeRecord, MatError> {
        self.ledger
            .nodes
            .get(&node_id)
            .ok_or_else(|| MatError::node_not_commissioned(node_id))
    }

    /// ノードを台帳に追加し、ディスクへ永続化する。
    pub fn upsert_node(&mut self, record: NodeRecord) -> Result<(), MatError> {
        self.ledger.nodes.insert(record.node_id, record);
        self.save_ledger()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_prefers_cli_over_env() {
        let p = Store::locate(Some(PathBuf::from("/tmp/explicit")));
        assert_eq!(p, PathBuf::from("/tmp/explicit"));
    }

    #[test]
    fn open_missing_yields_store_missing() {
        let err = Store::open("/nonexistent/path/for/mat/test").unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreMissing);
    }

    #[test]
    fn require_node_absent_yields_not_commissioned() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_or_init(dir.path()).unwrap();
        let err = store.require_node(42).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NodeNotCommissioned);
    }

    #[test]
    fn upsert_then_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = Store::open_or_init(dir.path()).unwrap();
            store
                .upsert_node(NodeRecord {
                    node_id: 7,
                    address: Some("192.0.2.10".into()),
                    commissioned_at: "2026-06-06T00:00:00+09:00".into(),
                })
                .unwrap();
        }
        // 再オープンして永続を確認。
        let store = Store::open(dir.path()).unwrap();
        assert!(store.contains(7));
        assert_eq!(
            store.require_node(7).unwrap().address.as_deref(),
            Some("192.0.2.10")
        );
    }

    #[test]
    fn corrupt_ledger_yields_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nodes.json"), "{ not json").unwrap();
        let err = Store::open(dir.path()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreParse);
    }
}
