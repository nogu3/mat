//! optional な alias 解決（aliases.json）。
//!
//! store 配下の `aliases.json` が**あれば** node / group / endpoint の名前→数値
//! 解決を行い、無ければ完全に従来動作（数値のみ）。ワイヤ・chip-tool / matd に
//! 渡る値は常に数値で、解決は CLI 層の前処理に閉じる。
//!
//! alias 名は純数字・空文字を禁止（数値指定とのシャドーイングを構造的に排除）。
//! `endpoints` はノード配下定義（外側キーはノード alias または node_id の数字
//! 文字列）。endpoint 番号はノードごとに意味が違うため、グローバル辞書にしない。

use std::str::FromStr;

/// `-n/--node` / `--nodes` が受ける「数値 or alias」。clap が [`FromStr`] で受け、
/// resolve 層が `AliasBook` で `Id` へ確定する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRef {
    Id(u64),
    Alias(String),
}

/// `-g/--group` が受ける「数値 or alias」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRef {
    Id(u16),
    Alias(String),
}

/// `-e/--endpoint` が受ける「数値 or alias」（ノードを取るコマンドのみ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointRef {
    Id(u16),
    Alias(String),
}

macro_rules! impl_ref {
    ($ty:ident, $num:ty, $what:literal) => {
        impl FromStr for $ty {
            type Err = std::convert::Infallible;
            /// 数値として parse できれば `Id`、できなければ `Alias`（最優先で従来互換）。
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(s.parse::<$num>()
                    .map($ty::Id)
                    .unwrap_or_else(|_| $ty::Alias(s.to_string())))
            }
        }
        impl $ty {
            /// 解決済み（`Id`）前提で数値を返す。resolve 層通過後にのみ呼ぶ。
            pub fn id(&self) -> $num {
                match self {
                    $ty::Id(n) => *n,
                    $ty::Alias(a) => {
                        unreachable!(
                            "unresolved {} alias '{a}': resolve_command must run first",
                            $what
                        )
                    }
                }
            }
        }
    };
}
impl_ref!(NodeRef, u64, "node");
impl_ref!(GroupRef, u16, "group");
impl_ref!(EndpointRef, u16, "endpoint");

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, MatError};

/// store 配下の alias 定義ファイル名。
pub const ALIASES_FILE: &str = "aliases.json";

/// aliases.json のスキーマ。全セクション optional（無い = 定義なし）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AliasFile {
    #[serde(default = "alias_version")]
    version: u32,
    #[serde(default)]
    nodes: BTreeMap<String, u64>,
    #[serde(default)]
    groups: BTreeMap<String, u16>,
    /// 外側キー = ノード alias または node_id の数字文字列、内側 = alias → endpoint。
    #[serde(default)]
    endpoints: BTreeMap<String, BTreeMap<String, u16>>,
}

impl Default for AliasFile {
    /// serde の `default = "alias_version"` は deserialize 時のみ効くため、
    /// 手動 impl で version 既定値を揃える（derive だと 0 になる）。
    fn default() -> Self {
        AliasFile {
            version: alias_version(),
            nodes: BTreeMap::new(),
            groups: BTreeMap::new(),
            endpoints: BTreeMap::new(),
        }
    }
}

fn alias_version() -> u32 {
    1
}

/// alias 名の妥当性: 空でなく、純数字でない（数値指定とのシャドーイング禁止）。
fn is_valid_alias_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().all(|c| c.is_ascii_digit())
}

/// 読み込み済み alias 定義。ファイルが無ければ空（present = false）。
#[derive(Debug)]
pub struct AliasBook {
    file: AliasFile,
    /// aliases.json が実在したか（エラーメッセージの出し分け用）。
    present: bool,
}

impl AliasBook {
    /// aliases.json を読む。無ければ空の book（正常）。壊れていれば `store_parse`。
    pub fn load(store_root: &Path) -> Result<Self, MatError> {
        let path = store_root.join(ALIASES_FILE);
        if !path.exists() {
            return Ok(AliasBook {
                file: AliasFile::default(),
                present: false,
            });
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| MatError::store_parse(format!("cannot read {}: {e}", path.display())))?;
        let file: AliasFile = serde_json::from_str(&text)
            .map_err(|e| MatError::store_parse(format!("cannot parse {}: {e}", path.display())))?;
        Self::validate(&file, &path)?;
        Ok(AliasBook {
            file,
            present: true,
        })
    }

    /// alias 名の検証。純数字・空文字は `store_parse`（ファイル自体の不備）。
    /// `endpoints` の外側キーだけは node_id の数字文字列を許可（空は不可）。
    fn validate(file: &AliasFile, path: &Path) -> Result<(), MatError> {
        let alias_names = file
            .nodes
            .keys()
            .chain(file.groups.keys())
            .chain(file.endpoints.values().flat_map(|eps| eps.keys()));
        for name in alias_names {
            if !is_valid_alias_name(name) {
                return Err(MatError::store_parse(format!(
                    "invalid alias name '{name}' in {} (must be non-empty and not all digits)",
                    path.display()
                )));
            }
        }
        if file.endpoints.keys().any(|k| k.is_empty()) {
            return Err(MatError::store_parse(format!(
                "invalid empty node key in endpoints section of {}",
                path.display()
            )));
        }
        Ok(())
    }

    /// node 参照を数値へ確定する（`Id` はパススルー）。未知 alias は kind=Other
    /// （main が exit 2 に写す）。
    pub fn resolve_node(&self, r: &NodeRef) -> Result<u64, MatError> {
        match r {
            NodeRef::Id(n) => Ok(*n),
            NodeRef::Alias(name) => self.file.nodes.get(name).copied().ok_or_else(|| {
                MatError::new(
                    ErrorKind::Other,
                    self.unknown_alias("node", name, self.file.nodes.keys()),
                )
            }),
        }
    }

    /// group 参照を数値へ確定する。
    pub fn resolve_group(&self, r: &GroupRef) -> Result<u16, MatError> {
        match r {
            GroupRef::Id(n) => Ok(*n),
            GroupRef::Alias(name) => self.file.groups.get(name).copied().ok_or_else(|| {
                MatError::new(
                    ErrorKind::Other,
                    self.unknown_alias("group", name, self.file.groups.keys()),
                )
            }),
        }
    }

    /// endpoint 参照を数値へ確定する。alias は「解決後の node」の定義だけを見る:
    /// 外側キー（ノード alias / 数字文字列）を node_id に正規化して照合するので、
    /// `-n 5 -e main` でも `-n living-light -e main` でも同じ結果になる。
    pub fn resolve_endpoint(&self, node_id: u64, r: &EndpointRef) -> Result<u16, MatError> {
        let name = match r {
            EndpointRef::Id(n) => return Ok(*n),
            EndpointRef::Alias(name) => name,
        };
        let mut known: Vec<&str> = Vec::new();
        for (outer, eps) in &self.file.endpoints {
            let outer_id = outer
                .parse::<u64>()
                .ok()
                .or_else(|| self.file.nodes.get(outer).copied());
            if outer_id == Some(node_id) {
                if let Some(ep) = eps.get(name) {
                    return Ok(*ep);
                }
                known.extend(eps.keys().map(String::as_str));
            }
        }
        let detail = if known.is_empty() {
            format!(
                "unknown endpoint alias '{name}' for node {node_id} (no endpoint aliases defined for this node)"
            )
        } else {
            format!(
                "unknown endpoint alias '{name}' for node {node_id} (known: {})",
                known.join(", ")
            )
        };
        Err(MatError::new(ErrorKind::Other, detail))
    }

    /// 未知 alias の detail 文。AI が自己修復できるよう既知 alias を列挙する。
    fn unknown_alias<'a>(
        &self,
        section: &str,
        name: &str,
        known: impl Iterator<Item = &'a String>,
    ) -> String {
        if !self.present {
            return format!("unknown {section} alias '{name}' (no aliases.json in store)");
        }
        let known: Vec<&str> = known.map(String::as_str).collect();
        if known.is_empty() {
            format!(
                "unknown {section} alias '{name}' (no {section} aliases defined in aliases.json)"
            )
        } else {
            format!(
                "unknown {section} alias '{name}' (known: {})",
                known.join(", ")
            )
        }
    }

    /// commission --alias の事前検証: 形式 NG / 使用済みはエラー（kind=Other、
    /// main が exit 2 に写す）。commission 開始前に呼び、成功後に alias 書き込み
    /// だけ失敗する中途半端な状態を作らない。
    pub fn validate_new_node_alias(&self, name: &str) -> Result<(), MatError> {
        if !is_valid_alias_name(name) {
            return Err(MatError::new(
                ErrorKind::Other,
                format!("invalid alias name '{name}' (must be non-empty and not all digits)"),
            ));
        }
        if self.file.nodes.contains_key(name) {
            return Err(MatError::new(
                ErrorKind::Other,
                format!("node alias '{name}' already exists in aliases.json (edit the file to reassign)"),
            ));
        }
        Ok(())
    }

    /// node alias を追加して aliases.json へ保存する（無ければ作成）。
    pub fn insert_node_alias(
        &mut self,
        name: &str,
        node_id: u64,
        store_root: &Path,
    ) -> Result<(), MatError> {
        self.validate_new_node_alias(name)?;
        self.file.nodes.insert(name.to_string(), node_id);
        let path = store_root.join(ALIASES_FILE);
        let text = serde_json::to_string_pretty(&self.file).map_err(|e| {
            MatError::new(ErrorKind::Other, format!("cannot serialize aliases: {e}"))
        })?;
        std::fs::write(&path, text).map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("cannot write {}: {e}", path.display()),
            )
        })?;
        self.present = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use std::path::Path;

    fn write_aliases(dir: &Path, json: &str) {
        std::fs::write(dir.join(ALIASES_FILE), json).unwrap();
    }

    const SAMPLE: &str = r#"{
        "version": 1,
        "nodes":  { "living-light": 5, "hall-sensor": 12 },
        "groups": { "all-lights": 258 },
        "endpoints": { "living-light": { "main": 1, "night": 2 }, "12": { "pir": 3 } }
    }"#;

    #[test]
    fn numeric_parses_to_id() {
        assert_eq!("5".parse::<NodeRef>().unwrap(), NodeRef::Id(5));
        assert_eq!("1".parse::<EndpointRef>().unwrap(), EndpointRef::Id(1));
        assert_eq!("258".parse::<GroupRef>().unwrap(), GroupRef::Id(258));
    }

    #[test]
    fn non_numeric_parses_to_alias() {
        assert_eq!(
            "living-light".parse::<NodeRef>().unwrap(),
            NodeRef::Alias("living-light".into())
        );
        // 数字始まりでも数値として parse できなければ alias。
        assert_eq!(
            "2f-light".parse::<NodeRef>().unwrap(),
            NodeRef::Alias("2f-light".into())
        );
    }

    #[test]
    fn out_of_range_number_falls_to_alias() {
        // u16 を溢れる数字列は GroupRef では alias 扱いになり、解決で
        // unknown alias（exit 2）に落ちる（従来の clap 範囲エラーも exit 2）。
        assert_eq!(
            "70000".parse::<GroupRef>().unwrap(),
            GroupRef::Alias("70000".into())
        );
    }

    #[test]
    fn id_returns_inner_value() {
        assert_eq!(NodeRef::Id(7).id(), 7);
        assert_eq!(GroupRef::Id(258).id(), 258);
        assert_eq!(EndpointRef::Id(2).id(), 2);
    }

    #[test]
    fn missing_file_yields_empty_book_and_numeric_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let book = AliasBook::load(dir.path()).unwrap();
        assert_eq!(book.resolve_node(&NodeRef::Id(5)).unwrap(), 5);
        let err = book.resolve_node(&NodeRef::Alias("x".into())).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("no aliases.json"), "{}", err.detail);
    }

    #[test]
    fn resolves_node_group_and_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), SAMPLE);
        let book = AliasBook::load(dir.path()).unwrap();
        assert_eq!(
            book.resolve_node(&NodeRef::Alias("living-light".into()))
                .unwrap(),
            5
        );
        assert_eq!(
            book.resolve_group(&GroupRef::Alias("all-lights".into()))
                .unwrap(),
            258
        );
        // 外側キーがノード alias。
        assert_eq!(
            book.resolve_endpoint(5, &EndpointRef::Alias("night".into()))
                .unwrap(),
            2
        );
        // 外側キーが node_id の数字文字列。
        assert_eq!(
            book.resolve_endpoint(12, &EndpointRef::Alias("pir".into()))
                .unwrap(),
            3
        );
        // 数値パススルー。
        assert_eq!(book.resolve_endpoint(5, &EndpointRef::Id(9)).unwrap(), 9);
    }

    #[test]
    fn unknown_alias_lists_known_names() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), SAMPLE);
        let book = AliasBook::load(dir.path()).unwrap();
        let err = book
            .resolve_node(&NodeRef::Alias("bogus".into()))
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("hall-sensor"), "{}", err.detail);
        assert!(err.detail.contains("living-light"), "{}", err.detail);
    }

    #[test]
    fn endpoint_alias_of_other_node_is_not_visible() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), SAMPLE);
        let book = AliasBook::load(dir.path()).unwrap();
        // "pir" は node 12 の定義。node 5 からは見えない。
        let err = book
            .resolve_endpoint(5, &EndpointRef::Alias("pir".into()))
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("node 5"), "{}", err.detail);
    }

    #[test]
    fn corrupt_json_yields_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), "{ not json");
        let err = AliasBook::load(dir.path()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreParse);
    }

    #[test]
    fn all_digit_or_empty_alias_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), r#"{ "nodes": { "42": 5 } }"#);
        assert_eq!(
            AliasBook::load(dir.path()).unwrap_err().kind,
            ErrorKind::StoreParse
        );
        write_aliases(dir.path(), r#"{ "groups": { "": 1 } }"#);
        assert_eq!(
            AliasBook::load(dir.path()).unwrap_err().kind,
            ErrorKind::StoreParse
        );
        // endpoints の内側キーも alias 名なので純数字は拒否。
        write_aliases(dir.path(), r#"{ "endpoints": { "living": { "1": 2 } } }"#);
        assert_eq!(
            AliasBook::load(dir.path()).unwrap_err().kind,
            ErrorKind::StoreParse
        );
        // endpoints の外側キーは node_id の数字文字列を許可。
        write_aliases(dir.path(), r#"{ "endpoints": { "5": { "main": 1 } } }"#);
        assert!(AliasBook::load(dir.path()).is_ok());
    }

    #[test]
    fn insert_node_alias_creates_file_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut book = AliasBook::load(dir.path()).unwrap(); // ファイル無し
        book.insert_node_alias("new-light", 9, dir.path()).unwrap();
        // 新規作成されたファイルの version はスキーマ既定の 1。
        let text = std::fs::read_to_string(dir.path().join(ALIASES_FILE)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["version"], 1);
        // 再ロードで永続を確認。
        let book = AliasBook::load(dir.path()).unwrap();
        assert_eq!(
            book.resolve_node(&NodeRef::Alias("new-light".into()))
                .unwrap(),
            9
        );
    }

    #[test]
    fn insert_preserves_existing_sections() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), SAMPLE);
        let mut book = AliasBook::load(dir.path()).unwrap();
        book.insert_node_alias("new-light", 9, dir.path()).unwrap();
        let book = AliasBook::load(dir.path()).unwrap();
        assert_eq!(
            book.resolve_group(&GroupRef::Alias("all-lights".into()))
                .unwrap(),
            258
        );
        assert_eq!(
            book.resolve_node(&NodeRef::Alias("living-light".into()))
                .unwrap(),
            5
        );
    }

    #[test]
    fn validate_new_node_alias_rejects_dup_and_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), SAMPLE);
        let book = AliasBook::load(dir.path()).unwrap();
        // 使用済み。
        let err = book.validate_new_node_alias("living-light").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("already"), "{}", err.detail);
        // 純数字 / 空。
        assert!(book.validate_new_node_alias("42").is_err());
        assert!(book.validate_new_node_alias("").is_err());
        // 未使用の妥当な名前。
        assert!(book.validate_new_node_alias("new-light").is_ok());
    }
}
