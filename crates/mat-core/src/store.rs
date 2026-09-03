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
///
/// address は保存しない（Issue #18 で撤去）: 実行時は常に mDNS 解決であり、
/// 保存した IP は Thread prefix 再構成で原理的に stale になる。旧形式ファイル
/// （address 付き）は serde が未知フィールドを無視して読め、次の書き込みで消える。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeRecord {
    pub node_id: u64,
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
    /// 払い出し済み node_id の high-water mark（次に配る id）。`unpair` で
    /// 台帳から消えた id を再利用しないための単調増加カウンタ。0 = 未設定
    /// （このフィールドを持たない旧形式台帳）で、その場合は `nodes` の最大値
    /// から復元する。
    #[serde(default)]
    next_node_id: u64,
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
                next_node_id: 0,
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
        crate::fsatomic::write_atomic(&path, text.as_bytes()).map_err(|e| {
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

    /// ノードを取得。未 commission なら [`ErrorKind::NodeNotCommissioned`]（exit 11）。
    ///
    /// read/write/invoke/describe が node_id 解決に使う。
    pub fn require_node(&self, node_id: u64) -> Result<&NodeRecord, MatError> {
        self.ledger
            .nodes
            .get(&node_id)
            .ok_or_else(|| MatError::node_not_commissioned(node_id))
    }

    /// 次に払い出す node_id。台帳の high-water mark と現在の最大 id の大きい方
    /// （どちらも無ければ 1）。`unpair` で消えた id は再利用しない — stale な
    /// SRP レコードが残ったまま同じ id で再 commission すると CASE が必ず失敗
    /// するため（docs/commands.md の unpair 節）。
    pub fn next_node_id(&self) -> u64 {
        let from_nodes = self.ledger.nodes.keys().max().map_or(1, |m| m + 1);
        self.ledger.next_node_id.max(from_nodes).max(1)
    }

    /// ノードを台帳に追加し、ディスクへ永続化する。high-water mark も前進させる
    /// （同じ save で永続化されるので `remove_node` 側は触らなくてよい）。
    pub fn upsert_node(&mut self, record: NodeRecord) -> Result<(), MatError> {
        self.ledger.next_node_id = self.ledger.next_node_id.max(record.node_id + 1);
        self.ledger.nodes.insert(record.node_id, record);
        self.save_ledger()
    }

    /// ノードを台帳から削除して永続化する。無ければ `Ok(false)`（ファイルは
    /// 触らない）。`mat unpair` が使う唯一の削除経路。
    pub fn remove_node(&mut self, node_id: u64) -> Result<bool, MatError> {
        if self.ledger.nodes.remove(&node_id).is_none() {
            return Ok(false);
        }
        self.save_ledger()?;
        Ok(true)
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
                    commissioned_at: "2026-06-06T00:00:00+09:00".into(),
                })
                .unwrap();
        }
        // 再オープンして永続を確認。
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.require_node(7).unwrap().node_id, 7);
        // atomic write の tmp が残らないこと。
        assert!(!dir.path().join("nodes.tmp").exists());
    }

    #[test]
    fn corrupt_ledger_yields_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nodes.json"), "{ not json").unwrap();
        let err = Store::open(dir.path()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreParse);
    }

    #[test]
    fn old_format_ledger_with_address_parses_and_sheds_it_on_save() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("nodes.json"),
            r#"{"version":1,"nodes":{"5":{"node_id":5,"address":"192.0.2.10","commissioned_at":"2026-01-01T00:00:00+09:00"}}}"#,
        )
        .unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.require_node(5).unwrap().node_id, 5);
        // 次の台帳書き込みで旧フィールドは自然に消える。
        store
            .upsert_node(NodeRecord {
                node_id: 6,
                commissioned_at: "2026-01-02T00:00:00+09:00".into(),
            })
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join("nodes.json")).unwrap();
        assert!(!text.contains("address"));
    }

    #[test]
    fn next_node_id_never_recycles_removed_ids() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = Store::open_or_init(dir.path()).unwrap();
            assert_eq!(store.next_node_id(), 1, "空台帳は 1 から");
            for id in [5u64, 6u64] {
                store
                    .upsert_node(NodeRecord {
                        node_id: id,
                        commissioned_at: "2026-01-01T00:00:00+09:00".into(),
                    })
                    .unwrap();
            }
            assert_eq!(store.next_node_id(), 7);
            assert!(store.remove_node(6).unwrap());
            assert_eq!(
                store.next_node_id(),
                7,
                "削除しても払い出し済み id は戻らない"
            );
        }
        // high-water mark はディスクに残る。
        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(reopened.next_node_id(), 7);
    }

    #[test]
    fn next_node_id_tolerates_old_ledger_without_the_field() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("nodes.json"),
            r#"{"version":1,"nodes":{"9":{"node_id":9,"commissioned_at":"2026-01-01T00:00:00+09:00"}}}"#,
        )
        .unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.next_node_id(), 10);
    }

    #[test]
    fn remove_node_deletes_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_or_init(dir.path()).unwrap();
        for id in [5u64, 6u64] {
            store
                .upsert_node(NodeRecord {
                    node_id: id,
                    commissioned_at: "2026-01-01T00:00:00+09:00".into(),
                })
                .unwrap();
        }
        assert!(store.remove_node(5).unwrap());
        assert!(!store.remove_node(5).unwrap(), "2 回目は無いので false");
        let reopened = Store::open(dir.path()).unwrap();
        let ids: Vec<u64> = reopened.nodes().map(|n| n.node_id).collect();
        assert_eq!(ids, vec![6]);
        assert_eq!(
            reopened.require_node(5).unwrap_err().kind,
            ErrorKind::NodeNotCommissioned
        );
    }
}
