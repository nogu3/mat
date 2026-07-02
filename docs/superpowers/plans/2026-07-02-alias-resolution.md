# node / group / endpoint alias 解決（optional）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** store 配下の `aliases.json` が**あれば** `-n/--nodes/-g/-e` の名前→数値解決を行い、無ければ完全に従来動作にする optional な alias 層を追加する。

**Architecture:** clap は `-n` 等を「数値 or alias」の enum 型（`NodeRef` 等）で受け、`main.rs` が **clap parse 直後・matd 経路解決より前**に `resolve.rs` で一括解決して全 ref を数値（`Id`）へ確定する。以降（matd `to_op` / 直経路の `commands::*`）はすべて従来どおり数値。alias の読み書きは `mat-core::alias::AliasBook` に集約。

**Tech Stack:** Rust / clap(derive) / serde_json。新規依存なし。

**Spec:** `docs/superpowers/specs/2026-07-02-alias-resolution-design.md`

## Global Constraints

- stdout は純粋な構造化 JSON のみ（スキーマ不変。alias のエコーバックはしない）。
- exit code: 未知 alias・不正 alias 名 = **2**（CLI 引数エラー）、壊れた aliases.json = **10**（`store_parse`）。exit code 表への追加は無し。
- alias 名は**純数字・空文字を禁止**（`endpoints` の外側キーだけは node_id の数字文字列を許可）。
- 解決は mat CLI 層に閉じる。matd プロトコル・`commands/*.rs` のシグネチャ・chip-tool へ渡る値は数値のまま（matd 側変更ゼロ）。
- `group provision` / `group invoke` の `-e` は数値のみ（endpoint alias はノード文脈が必要で、group 系にはノード文脈が無い）。
- 各タスク完了時に `cargo test --workspace` が全て通ること。最後に `task check`。
- コミットメッセージは既存スタイル（日本語 + `feat:`/`test:`/`docs:` プレフィックス、`Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` を末尾に付ける）。

---

### Task 1: mat-core `alias.rs` — 参照型（NodeRef / GroupRef / EndpointRef）

**Files:**
- Create: `crates/mat-core/src/alias.rs`
- Modify: `crates/mat-core/src/lib.rs`（`pub mod alias;` を追加）

**Interfaces:**
- Produces: `NodeRef { Id(u64), Alias(String) }` / `GroupRef { Id(u16), Alias(String) }` / `EndpointRef { Id(u16), Alias(String) }`。各型は `FromStr`（`Err = Infallible`、数値 parse 成功なら `Id`、失敗なら `Alias`）と `id()`（`Id` 前提で数値を返す。`Alias` なら `unreachable!`）を持つ。Task 4 の cli.rs / resolve.rs / matd_client.rs が使う。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/alias.rs` を作成し、まず型とテストだけ書く（この Step ではテスト部分に集中し、Step 3 の実装まで含めて一気に書いてよい。TDD の意図は「テストが実装を規定する」こと）:

```rust
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
                        unreachable!("unresolved {} alias '{a}': resolve_command must run first", $what)
                    }
                }
            }
        }
    };
}
impl_ref!(NodeRef, u64, "node");
impl_ref!(GroupRef, u16, "group");
impl_ref!(EndpointRef, u16, "endpoint");

#[cfg(test)]
mod tests {
    use super::*;

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
}
```

- [ ] **Step 2: `lib.rs` にモジュール追加**

`crates/mat-core/src/lib.rs` の `pub mod diag;` の前に追加（アルファベット順）:

```rust
pub mod alias;
```

- [ ] **Step 3: テスト実行**

Run: `cargo test -p mat-core alias`
Expected: PASS（4 tests）

- [ ] **Step 4: コミット**

```bash
git add crates/mat-core/src/alias.rs crates/mat-core/src/lib.rs
git commit -m "feat(mat-core): alias 解決の参照型（NodeRef/GroupRef/EndpointRef）"
```

---

### Task 2: mat-core `alias.rs` — AliasBook（load / validate / resolve）

**Files:**
- Modify: `crates/mat-core/src/alias.rs`

**Interfaces:**
- Consumes: Task 1 の `NodeRef` / `GroupRef` / `EndpointRef`。
- Produces: `AliasBook::load(store_root: &Path) -> Result<AliasBook, MatError>`、`resolve_node(&NodeRef) -> Result<u64, MatError>`、`resolve_group(&GroupRef) -> Result<u16, MatError>`、`resolve_endpoint(node_id: u64, &EndpointRef) -> Result<u16, MatError>`、定数 `ALIASES_FILE = "aliases.json"`。未知 alias のエラー kind は `Other`（main が exit 2 に写す）、壊れたファイルは `StoreParse`。

- [ ] **Step 1: 失敗するテストを書く**

`alias.rs` の `tests` モジュールに追加:

```rust
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
        assert_eq!(book.resolve_node(&NodeRef::Alias("living-light".into())).unwrap(), 5);
        assert_eq!(book.resolve_group(&GroupRef::Alias("all-lights".into())).unwrap(), 258);
        // 外側キーがノード alias。
        assert_eq!(book.resolve_endpoint(5, &EndpointRef::Alias("night".into())).unwrap(), 2);
        // 外側キーが node_id の数字文字列。
        assert_eq!(book.resolve_endpoint(12, &EndpointRef::Alias("pir".into())).unwrap(), 3);
        // 数値パススルー。
        assert_eq!(book.resolve_endpoint(5, &EndpointRef::Id(9)).unwrap(), 9);
    }

    #[test]
    fn unknown_alias_lists_known_names() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), SAMPLE);
        let book = AliasBook::load(dir.path()).unwrap();
        let err = book.resolve_node(&NodeRef::Alias("bogus".into())).unwrap_err();
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
        let err = book.resolve_endpoint(5, &EndpointRef::Alias("pir".into())).unwrap_err();
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
        assert_eq!(AliasBook::load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
        write_aliases(dir.path(), r#"{ "groups": { "": 1 } }"#);
        assert_eq!(AliasBook::load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
        // endpoints の内側キーも alias 名なので純数字は拒否。
        write_aliases(dir.path(), r#"{ "endpoints": { "living": { "1": 2 } } }"#);
        assert_eq!(AliasBook::load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
        // endpoints の外側キーは node_id の数字文字列を許可。
        write_aliases(dir.path(), r#"{ "endpoints": { "5": { "main": 1 } } }"#);
        assert!(AliasBook::load(dir.path()).is_ok());
    }
```

テストモジュール先頭の `use super::*;` に加えて `use crate::error::ErrorKind;` が必要。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-core alias`
Expected: コンパイルエラー（`AliasBook` 未定義）

- [ ] **Step 3: 実装**

`alias.rs` の型定義の後（tests の前）に追加:

```rust
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, MatError};

/// store 配下の alias 定義ファイル名。
pub const ALIASES_FILE: &str = "aliases.json";

/// aliases.json のスキーマ。全セクション optional（無い = 定義なし）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
            format!("unknown {section} alias '{name}' (no {section} aliases defined in aliases.json)")
        } else {
            format!("unknown {section} alias '{name}' (known: {})", known.join(", "))
        }
    }
}
```

Task 1 の `use std::str::FromStr;` と重複しないよう import を整理する（`cargo fmt` に任せてよい）。

- [ ] **Step 4: テスト実行**

Run: `cargo test -p mat-core alias`
Expected: PASS（Task 1 の 4 + 新規 6 = 10 tests）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-core/src/alias.rs
git commit -m "feat(mat-core): AliasBook（aliases.json の load/検証/解決）"
```

---

### Task 3: mat-core `alias.rs` — commission --alias 用の書き込み経路

**Files:**
- Modify: `crates/mat-core/src/alias.rs`

**Interfaces:**
- Produces: `AliasBook::validate_new_node_alias(&self, name: &str) -> Result<(), MatError>`（形式 NG / 使用済み → kind=Other。resolve 層の事前検証が使い、main が exit 2 に写す）と `AliasBook::insert_node_alias(&mut self, name: &str, node_id: u64, store_root: &Path) -> Result<(), MatError>`（検証 + 追記 + 保存。ファイルが無ければ作成）。Task 6 の commission が使う。

- [ ] **Step 1: 失敗するテストを書く**

`tests` モジュールに追加:

```rust
    #[test]
    fn insert_node_alias_creates_file_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut book = AliasBook::load(dir.path()).unwrap(); // ファイル無し
        book.insert_node_alias("new-light", 9, dir.path()).unwrap();
        // 再ロードで永続を確認。
        let book = AliasBook::load(dir.path()).unwrap();
        assert_eq!(book.resolve_node(&NodeRef::Alias("new-light".into())).unwrap(), 9);
    }

    #[test]
    fn insert_preserves_existing_sections() {
        let dir = tempfile::tempdir().unwrap();
        write_aliases(dir.path(), SAMPLE);
        let mut book = AliasBook::load(dir.path()).unwrap();
        book.insert_node_alias("new-light", 9, dir.path()).unwrap();
        let book = AliasBook::load(dir.path()).unwrap();
        assert_eq!(book.resolve_group(&GroupRef::Alias("all-lights".into())).unwrap(), 258);
        assert_eq!(book.resolve_node(&NodeRef::Alias("living-light".into())).unwrap(), 5);
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
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-core alias`
Expected: コンパイルエラー（`insert_node_alias` 未定義）

- [ ] **Step 3: 実装**

`impl AliasBook` に追加:

```rust
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
```

- [ ] **Step 4: テスト実行**

Run: `cargo test -p mat-core alias`
Expected: PASS（計 13 tests）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-core/src/alias.rs
git commit -m "feat(mat-core): AliasBook に node alias の追記・保存（commission --alias 用）"
```

---

### Task 4: CLI 切替 — cli.rs 型変更 + resolve.rs + main.rs + matd_client.rs

このタスクはコンパイル単位として不可分（cli.rs の型変更が main.rs / matd_client.rs を同時に壊す）。1 コミットで行う。

**Files:**
- Modify: `crates/mat/src/cli.rs`（`-n`/`--nodes`/`-g`/ノード系 `-e` の型を Ref 型へ）
- Create: `crates/mat/src/resolve.rs`（一括解決 + 単体テスト）
- Modify: `crates/mat/src/main.rs`（parse 直後の解決ステップ、`.id()` 化）
- Modify: `crates/mat/src/matd_client.rs`（`to_op` の `.id()` 化、既存単体テストの型合わせ）

**Interfaces:**
- Consumes: Task 1–2 の `NodeRef` / `GroupRef` / `EndpointRef` / `AliasBook`。
- Produces: `resolve::resolve_command(command: Command, store_root: &Path) -> Result<Command, MatError>` — 返る `Command` は全 ref が `Id` 確定済み。`commands/*.rs` は一切変更しない。

- [ ] **Step 1: cli.rs の型を変更**

`crates/mat/src/cli.rs` に import を追加:

```rust
use mat_core::alias::{EndpointRef, GroupRef, NodeRef};
```

以下の置き換えを行う（doc コメントも更新。パターンは全箇所同じ）:

1. **node**: `Read` / `Write` / `Invoke` / `Describe` / `On` / `Off` / `ColorTemp` / `OpenWindow` / `DiagCommand::Thread` / `DiagCommand::Node` の
   ```rust
   /// commission 済みノードの node_id。
   #[arg(short = 'n', long = "node", value_name = "N")]
   node_id: u64,
   ```
   を
   ```rust
   /// commission 済みノードの node_id、または aliases.json の node alias。
   #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
   node_id: NodeRef,
   ```
   に（`Commission` の `node_id: Option<u64>` は**変更しない** — 新規採番なので alias 不可）。

2. **endpoint（ノードを取るコマンドのみ）**: `Read` / `Write` / `Invoke` / `On` / `Off` / `ColorTemp` / `DiagCommand::Thread` / `DiagCommand::Node` の
   ```rust
   #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
   endpoint: u16,
   ```
   を
   ```rust
   /// エンドポイント番号、または aliases.json の endpoint alias（既定 1）。
   #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
   endpoint: EndpointRef,
   ```
   に（diag 系は `default_value = "0"`）。`default_value_t` は `Display` が要るので文字列 `default_value` にする。
   **`GroupCommand::Provision` / `GroupCommand::Invoke` の `endpoint: u16` は変更しない**（ノード文脈が無いため alias 不可。doc コメントに「数値のみ」と追記）。

3. **group**: `GroupCommand::Provision` / `GroupCommand::Invoke` の
   ```rust
   #[arg(short = 'g', long = "group", value_name = "ID")]
   group_id: u16,
   ```
   を
   ```rust
   /// Matter GroupId、または aliases.json の group alias。
   #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
   group_id: GroupRef,
   ```
   に。

4. **nodes**: `GroupCommand::Provision` の
   ```rust
   #[arg(long = "nodes", required = true, num_args = 1..)]
   node_ids: Vec<u64>,
   ```
   を
   ```rust
   /// provision 対象の commission 済み node_id または node alias（1つ以上）。
   #[arg(long = "nodes", required = true, num_args = 1..)]
   node_ids: Vec<NodeRef>,
   ```
   に。

- [ ] **Step 2: resolve.rs を単体テストごと作成**

`crates/mat/src/resolve.rs`:

```rust
//! clap parse 直後の alias 一括解決。
//!
//! ここを通った後の `Command` は NodeRef / GroupRef / EndpointRef が全て `Id` に
//! 確定している（matd 経路・直経路の両方がこの後段）。exit code 規約: 壊れた
//! aliases.json は `store_parse`（10）、未知 alias / 不正 alias 名は CLI 引数
//! エラー（2）— main が `kind` で振り分ける。

use std::path::Path;

use crate::cli::{Command, DiagCommand, GroupCommand};
use mat_core::alias::{AliasBook, EndpointRef, GroupRef, NodeRef};
use mat_core::error::MatError;

/// command 内の alias を全て数値（`Id`）へ確定した `Command` を返す。
/// aliases.json が無ければ数値はパススルー（従来動作）。
///
/// match は網羅（`_` 無し）: 新しいサブコマンドを足すとここがコンパイルエラーに
/// なり、alias 解決の考慮漏れを防ぐ。
pub fn resolve_command(command: Command, store_root: &Path) -> Result<Command, MatError> {
    let book = AliasBook::load(store_root)?;
    Ok(match command {
        Command::Discover { probe } => Command::Discover { probe },
        Command::Commission {
            target,
            setup_code,
            node_id,
        } => Command::Commission {
            target,
            setup_code,
            node_id,
        },
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Read {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                cluster,
                attribute,
            }
        }
        Command::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Write {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                cluster,
                attribute,
                value,
            }
        }
        Command::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Invoke {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                cluster,
                command,
                args,
            }
        }
        Command::Describe { node_id } => Command::Describe {
            node_id: NodeRef::Id(book.resolve_node(&node_id)?),
        },
        Command::On { node_id, endpoint } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::On {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
            }
        }
        Command::Off { node_id, endpoint } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Off {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
            }
        }
        Command::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::ColorTemp {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                kelvin,
                mireds,
                transition,
            }
        }
        Command::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => Command::OpenWindow {
            node_id: NodeRef::Id(book.resolve_node(&node_id)?),
            timeout,
            iteration,
            discriminator,
        },
        Command::Group { action } => Command::Group {
            action: match action {
                GroupCommand::Provision {
                    group_id,
                    node_ids,
                    keyset_id,
                    name,
                    endpoint,
                    epoch_key,
                } => GroupCommand::Provision {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    node_ids: node_ids
                        .iter()
                        .map(|n| book.resolve_node(n).map(NodeRef::Id))
                        .collect::<Result<Vec<_>, _>>()?,
                    keyset_id,
                    name,
                    endpoint,
                    epoch_key,
                },
                GroupCommand::Invoke {
                    group_id,
                    cluster,
                    command,
                    args,
                    endpoint,
                } => GroupCommand::Invoke {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    cluster,
                    command,
                    args,
                    endpoint,
                },
            },
        },
        Command::Diag { action } => Command::Diag {
            action: match action {
                DiagCommand::Thread { node_id, endpoint } => {
                    let node = book.resolve_node(&node_id)?;
                    let ep = book.resolve_endpoint(node, &endpoint)?;
                    DiagCommand::Thread {
                        node_id: NodeRef::Id(node),
                        endpoint: EndpointRef::Id(ep),
                    }
                }
                DiagCommand::Node {
                    node_id,
                    endpoint,
                    deep,
                } => {
                    let node = book.resolve_node(&node_id)?;
                    let ep = book.resolve_endpoint(node, &endpoint)?;
                    DiagCommand::Node {
                        node_id: NodeRef::Id(node),
                        endpoint: EndpointRef::Id(ep),
                        deep,
                    }
                }
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_core::error::ErrorKind;

    fn store_with(json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("aliases.json"), json).unwrap();
        dir
    }

    const SAMPLE: &str = r#"{
        "nodes":  { "living-light": 5 },
        "groups": { "all-lights": 258 },
        "endpoints": { "living-light": { "night": 2 } }
    }"#;

    #[test]
    fn read_alias_resolves_node_then_endpoint() {
        let dir = store_with(SAMPLE);
        let cmd = Command::Read {
            node_id: NodeRef::Alias("living-light".into()),
            endpoint: EndpointRef::Alias("night".into()),
            cluster: "onoff".into(),
            attribute: "on-off".into(),
        };
        let resolved = resolve_command(cmd, dir.path()).unwrap();
        match resolved {
            Command::Read {
                node_id, endpoint, ..
            } => {
                assert_eq!(node_id, NodeRef::Id(5));
                assert_eq!(endpoint, EndpointRef::Id(2));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn numeric_node_still_resolves_endpoint_alias() {
        // -n 5 -e night: 外側キーが alias 表記でも解決後 node で照合される。
        let dir = store_with(SAMPLE);
        let cmd = Command::On {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Alias("night".into()),
        };
        match resolve_command(cmd, dir.path()).unwrap() {
            Command::On { endpoint, .. } => assert_eq!(endpoint, EndpointRef::Id(2)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn group_provision_resolves_group_and_each_node() {
        let dir = store_with(SAMPLE);
        let cmd = Command::Group {
            action: GroupCommand::Provision {
                group_id: GroupRef::Alias("all-lights".into()),
                node_ids: vec![NodeRef::Alias("living-light".into()), NodeRef::Id(7)],
                keyset_id: 42,
                name: None,
                endpoint: 1,
                epoch_key: None,
            },
        };
        match resolve_command(cmd, dir.path()).unwrap() {
            Command::Group {
                action:
                    GroupCommand::Provision {
                        group_id, node_ids, ..
                    },
            } => {
                assert_eq!(group_id, GroupRef::Id(258));
                assert_eq!(node_ids, vec![NodeRef::Id(5), NodeRef::Id(7)]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_alias_is_kind_other() {
        let dir = store_with(SAMPLE);
        let cmd = Command::Describe {
            node_id: NodeRef::Alias("bogus".into()),
        };
        let err = resolve_command(cmd, dir.path()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
    }

    #[test]
    fn no_aliases_file_passes_numerics_through() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = Command::Describe {
            node_id: NodeRef::Id(5),
        };
        match resolve_command(cmd, dir.path()).unwrap() {
            Command::Describe { node_id } => assert_eq!(node_id, NodeRef::Id(5)),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
```

- [ ] **Step 3: main.rs を更新**

`crates/mat/src/main.rs`:

1. `mod resolve;` を `mod probe;` の前に追加。
2. `use mat_core::error::ErrorKind;` を追加。
3. matd 経路解決の**前**に store locate + alias 解決を移動・挿入。既存の
   ```rust
   // 経路解決（matd_client::resolve_route）: ...
   match matd_client::resolve_route(
   ```
   の前に:
   ```rust
   let store_path = Store::locate(args.store);

   // alias 一括解決（aliases.json が無ければ数値パススルー）。matd 経路も数値しか
   // 受けないため、経路解決より前に行う。未知 alias / 不正 alias 名は CLI 引数
   // エラー（exit 2）、壊れた aliases.json は store_parse（exit 10）。
   let command = match resolve::resolve_command(args.command, &store_path) {
       Ok(c) => c,
       Err(e) => {
           e.emit();
           return match e.kind {
               ErrorKind::StoreParse => ExitCode::from(e.kind.exit_code()),
               _ => ExitCode::from(2),
           };
       }
   };
   ```
   既存の `let store_path = Store::locate(args.store);`（matd match の後にある行）は削除。
4. matd dispatch の引数を `&args.command` → `&command` に変更（`Forced` / `Auto` 両方）。
5. 直経路の `match &args.command` → `match &command` に変更し、数値の取り出しを `.id()` 化する。変更対象の全箇所:
   - `Read`: `commands::read::run(&store_path, node_id.id(), endpoint.id(), cluster, attribute)`
   - `Write`: `commands::write::run(&store_path, node_id.id(), endpoint.id(), cluster, attribute, value)`
   - `Invoke`: `commands::invoke::run(&store_path, node_id.id(), endpoint.id(), cluster, command, args)`
   - `Describe`: `commands::describe::run(&store_path, node_id.id())`
   - `On` / `Off`: `commands::invoke::run_onoff(&store_path, node_id.id(), endpoint.id(), true/false)`
   - `ColorTemp`: `commands::invoke::run_color_temp(&store_path, node_id.id(), endpoint.id(), kelvin, mireds, *transition)`
   - `OpenWindow`: `let disc = discriminator.unwrap_or_else(|| (node_id.id() % 4096) as u16);` と `commands::open_window::run(&store_path, node_id.id(), *timeout, *iteration, disc)`
   - `Group Provision`: `group_id.id()` と、`node_ids` は `let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();` を作って `&ids` を渡す（`commands::group::provision` のシグネチャは `&[u64]` 前提のまま）。name 既定値の行は `GroupRef` に `Display` が無いため `let gid = group_id.id();` を先に束縛して `format!("grp{gid}")` に変更し、以降の呼び出しも `gid` を渡す。import に `use mat_core::alias::NodeRef;` を追加。
   - `Group Invoke`: `group_id.id()`
   - `Diag Thread` / `Diag Node`: `node_id.id()`, `endpoint.id()`

   ※ シャドーイング注意: `Invoke` アームの束縛 `command` が上の解決済み `command` を隠すが、既存コードと同じ構図（`args` も同様）で問題ない。

- [ ] **Step 4: matd_client.rs を更新**

`to_op` 内の数値参照を `.id()` 化する:

- `Read` / `Write` / `Invoke` / `Describe` / `On` / `Off`: `"node_id": node_id.id()`、`"endpoint": endpoint.id()`
- `ColorTemp`: `"node_id": node_id.id(), "endpoint": endpoint.id()`
- `Group Provision`: `"group_id": group_id.id()` と
  ```rust
  let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();
  ```
  を作って `"node_ids": ids`（`name` 補完の `format!("grp{group_id}")` は `group_id.id()` を使うため `let gid = group_id.id();` を先に束縛して `format!("grp{gid}")` にする。json! 内も `gid`）。
- `Group Invoke`: `"group_id": group_id.id()`
- import に `use mat_core::alias::NodeRef;` を追加。

既存単体テストの Command 構築を型に合わせる（`use mat_core::alias::{EndpointRef, GroupRef, NodeRef};` を tests に追加）:

- `read_maps_to_read_op`: `node_id: NodeRef::Id(1), endpoint: EndpointRef::Id(2)`
- `on_maps_to_on_op_with_endpoint`: `node_id: NodeRef::Id(3), endpoint: EndpointRef::Id(1)`
- `color_temp_*`（2件）: `node_id: NodeRef::Id(6), endpoint: EndpointRef::Id(1)`
- `group_provision_fills_default_name_and_keeps_null_epoch`: `group_id: GroupRef::Id(7), node_ids: vec![NodeRef::Id(1), NodeRef::Id(2)]`

- [ ] **Step 5: 全テスト実行**

Run: `cargo test --workspace`
Expected: 既存テスト全 PASS（数値パススルーで挙動不変）+ resolve.rs の 5 tests PASS

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean

- [ ] **Step 6: コミット**

```bash
git add crates/mat/src/cli.rs crates/mat/src/resolve.rs crates/mat/src/main.rs crates/mat/src/matd_client.rs
git commit -m "feat(mat): -n/--nodes/-g/-e で alias を受理（parse 直後に一括解決、matd 経路もカバー）"
```

---

### Task 5: 統合テスト — alias 解決の E2E（fake-chip-tool）

**Files:**
- Modify: `crates/mat/tests/integration.rs`（末尾に追加）

**Interfaces:**
- Consumes: 既存ヘルパー `mat(store)` / `store_with_node5()`、fake-chip-tool の `FAKE_CHIP_ARGS_FILE`。

- [ ] **Step 1: 失敗するテストを書く**

`integration.rs` 末尾に追加:

```rust
// ---- alias 解決（aliases.json） ----

/// node 5 commission 済み + aliases.json を置いたストア。
fn store_with_node5_and_aliases() -> TempDir {
    let store = store_with_node5();
    std::fs::write(
        store.path().join("aliases.json"),
        r#"{
            "nodes":  { "living-light": 5 },
            "groups": { "all-lights": 1 },
            "endpoints": { "living-light": { "main": 1, "night": 2 } }
        }"#,
    )
    .unwrap();
    store
}

#[test]
fn read_resolves_node_alias_to_numeric_id() {
    let store = store_with_node5_and_aliases();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args([
            "read",
            "--node",
            "living-light",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ])
        .assert()
        .success()
        // stdout スキーマは数値のまま（alias エコーバック無し）。
        .stdout(predicate::str::contains("\"node_id\":5"))
        .stdout(predicate::str::contains("living-light").not());
    // chip-tool には数値 node_id が渡る。
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("onoff read on-off 5 1"),
        "alias was not resolved before chip-tool: {recorded}"
    );
}

#[test]
fn endpoint_alias_resolves_with_numeric_node() {
    // -n 5 -e night: endpoints の外側キーが alias 表記でも解決後 node で照合。
    let store = store_with_node5_and_aliases();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["on", "--node", "5", "--endpoint", "night"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"endpoint\":2"));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("onoff on 5 2"),
        "endpoint alias was not resolved: {recorded}"
    );
}

#[test]
fn group_invoke_resolves_group_alias() {
    let store = store_with_node5_and_aliases();
    mat(store.path())
        .args([
            "group",
            "invoke",
            "--group",
            "all-lights",
            "--cluster",
            "onoff",
            "--command",
            "on",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"group_id\":1"))
        .stdout(predicate::str::contains("\"status\":\"sent\""));
}

#[test]
fn unknown_alias_exits_2_and_lists_known() {
    let store = store_with_node5_and_aliases();
    mat(store.path())
        .args(["describe", "--node", "bogus"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown node alias 'bogus'"))
        .stderr(predicate::str::contains("living-light"));
}

#[test]
fn alias_without_aliases_file_exits_2() {
    let store = store_with_node5(); // aliases.json 無し
    mat(store.path())
        .args(["describe", "--node", "living-light"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no aliases.json"));
}

#[test]
fn corrupt_aliases_file_exits_10() {
    let store = store_with_node5();
    std::fs::write(store.path().join("aliases.json"), "{ not json").unwrap();
    mat(store.path())
        .args(["describe", "--node", "5"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("store_parse"));
}

#[test]
fn all_digit_alias_name_in_file_exits_10() {
    let store = store_with_node5();
    std::fs::write(
        store.path().join("aliases.json"),
        r#"{ "nodes": { "42": 5 } }"#,
    )
    .unwrap();
    mat(store.path())
        .args(["describe", "--node", "5"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("invalid alias name"));
}
```

**注意:** `read_resolves_node_alias_to_numeric_id` の `recorded.contains("onoff read on-off 5 1")` と `endpoint_alias_resolves_with_numeric_node` の `"onoff on 5 2"` は、既存テスト（`group_provision_last_chip_call_is_add_group` 等）の記録形式に倣った引数列。実行して形式が違ったら、まず `recorded` の実際の中身を確認して合わせる（chip-tool 引数順は `<cluster> <op> [<attr>] <node> <endpoint>`）。

- [ ] **Step 2: テスト実行**

Run: `cargo test -p mat --test integration alias -- --nocapture` および `cargo test -p mat --test integration resolves`
Expected: 全 PASS（Task 4 実装済みなので、ここで落ちたら Task 4 のバグ）

- [ ] **Step 3: コミット**

```bash
git add crates/mat/tests/integration.rs
git commit -m "test(mat): alias 解決の統合テスト（node/endpoint/group、exit 2/10）"
```

---

### Task 6: `mat commission --alias`

**Files:**
- Modify: `crates/mat/src/cli.rs`（`Commission` に `alias` フィールド追加）
- Modify: `crates/mat/src/resolve.rs`（Commission アームで事前検証）
- Modify: `crates/mat/src/main.rs`（alias を commission::run へ）
- Modify: `crates/mat/src/matd_client.rs`（Commission パターンの `..` 化は既存どおりで変更不要か確認）
- Modify: `crates/mat/src/commands/commission.rs`（成功後に aliases.json へ追記）
- Modify: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: Task 3 の `AliasBook::validate_new_node_alias` / `insert_node_alias`。
- Produces: `commands::commission::run(store_path, target, setup_code, node_id, alias: Option<&str>)`（シグネチャに第5引数追加）。

- [ ] **Step 1: 失敗する統合テストを書く**

`integration.rs` に追加:

```rust
#[test]
fn commission_with_alias_writes_aliases_json() {
    let store = TempDir::new().unwrap();
    mat(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
            "--node",
            "5",
            "--alias",
            "living-light",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"node_id\":5"));
    // aliases.json が作られ、以後 alias で参照できる。
    mat(store.path())
        .args(["describe", "--node", "living-light"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"node_id\":5"));
}

#[test]
fn commission_with_duplicate_alias_exits_2_before_running() {
    let store = store_with_node5_and_aliases(); // living-light 定義済み
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
            "--alias",
            "living-light",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));
    // 事前検証なので chip-tool は呼ばれていない。
    assert!(!args_file.exists(), "chip-tool was invoked despite invalid alias");
}

#[test]
fn commission_with_all_digit_alias_exits_2() {
    let store = TempDir::new().unwrap();
    mat(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
            "--alias",
            "42",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid alias name"));
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat --test integration commission_with`
Expected: FAIL（`--alias` は unexpected argument で exit 2 になり、1つ目のテストが落ちる）

- [ ] **Step 3: 実装**

1. `cli.rs` の `Commission` に追加:
   ```rust
   /// commission 成功時に aliases.json へ登録する node alias（任意）。
   /// 純数字・使用済みの名前は commission 開始前に exit 2。
   #[arg(long, value_name = "NAME")]
   alias: Option<String>,
   ```

2. `resolve.rs` の Commission アームを事前検証つきに変更:
   ```rust
   Command::Commission {
       target,
       setup_code,
       node_id,
       alias,
   } => {
       // 名前の妥当性・重複は commission 開始前に検証する（開始後に alias
       // 書き込みだけ失敗する中途半端な状態を作らない）。
       if let Some(name) = &alias {
           book.validate_new_node_alias(name)?;
       }
       Command::Commission {
           target,
           setup_code,
           node_id,
           alias,
       }
   }
   ```

3. `matd_client.rs` の `Command::Commission { .. }` パターンは `..` なので変更不要（コンパイルで確認）。`discover_and_commission_are_unsupported` テストの構築子に `alias: None,` を追加。

4. `main.rs` の Commission アーム:
   ```rust
   Command::Commission {
       target,
       setup_code,
       node_id,
       alias,
   } => commands::commission::run(&store_path, target, setup_code, *node_id, alias.as_deref()),
   ```

5. `commission.rs`:
   - import に `use mat_core::alias::AliasBook;` を追加。
   - シグネチャに `alias: Option<&str>` を追加:
     ```rust
     pub fn run(
         store_path: &Path,
         target: &str,
         setup_code: &str,
         node_id: Option<u64>,
         alias: Option<&str>,
     ) -> Result<(), MatError> {
     ```
   - 成功分岐の `store.upsert_node(...)?;` の直後・`output::emit` の前に:
     ```rust
     if let Some(name) = alias {
         // 名前の妥当性・重複は resolve 層で事前検証済み。ここで失敗するのは
         // 書き込みエラー等のみ（commission 自体は成功しているので detail に明記）。
         let mut book = AliasBook::load(store.root())?;
         book.insert_node_alias(name, node_id, store.root()).map_err(|e| {
             MatError::new(
                 e.kind,
                 format!(
                     "node {node_id} was commissioned, but writing alias '{name}' failed: {}",
                     e.detail
                 ),
             )
         })?;
     }
     ```

- [ ] **Step 4: テスト実行**

Run: `cargo test --workspace`
Expected: 全 PASS（新規 3 統合テスト含む）

- [ ] **Step 5: コミット**

```bash
git add crates/mat/src/cli.rs crates/mat/src/resolve.rs crates/mat/src/main.rs crates/mat/src/matd_client.rs crates/mat/src/commands/commission.rs crates/mat/tests/integration.rs
git commit -m "feat(mat): commission --alias（成功時に aliases.json へ登録、事前検証つき）"
```

---

### Task 7: ドキュメント同期 + 最終チェック

**Files:**
- Modify: `CLAUDE.md`
- Modify: `ARCHITECTURE.md`
- Modify: `README.md`

- [ ] **Step 1: CLAUDE.md のスコープ記述を改訂**

「Scope reminders」の
```
- Resolving human names to node_id / endpoint / cluster (out of scope; `mat`
  takes numeric values).
```
を
```
- Resolving human names on the wire or in the backend (chip-tool / matd always
  receive numeric values). The only exception: if `<store>/aliases.json`
  exists, the CLI layer resolves node / group / endpoint aliases to numbers
  right after arg parsing — optional, local, and absent-file = no behavior
  change. Cluster / attribute names stay chip-tool notation (no aliasing).
```
に置き換える。

- [ ] **Step 2: ARCHITECTURE.md の該当 2 箇所を改訂**

- 33 行目付近の `numeric node_id. Mapping human-facing names is out of scope.` を含む文を、「数値 node_id を取る。人間向けの名前解決は out of scope — ただし optional な `aliases.json`（store 配下）があるときだけ、CLI 層が parse 直後に node / group / endpoint alias を数値へ解決する（ワイヤ・chip-tool へは常に数値）」の趣旨に書き換える（前後の文体・言語に合わせる）。
- 394 行目付近の `- Hold human names or logical groups in mat (out of scope).` に「(exception: the optional aliases.json name→number map; see above)」の趣旨を追記。
- 実際の行番号・文面は現物を読んで整合させること。

- [ ] **Step 3: README.md に aliases.json 節を追加**

適切な位置（store の説明の近く、または「Usage」の後）に追加する内容:

- `aliases.json` の配置（store 配下）と完全なサンプル（spec の例をそのまま使う。ダミー値のみ）。
- 解決ルール: 数値優先・alias フォールバック / alias 名は純数字禁止 / `endpoints` はノード配下定義（外側キーは node alias か node_id 文字列）/ group 系の `-e` は数値のみ。
- `mat commission --alias <name>` の説明。
- エラー: 未知 alias = exit 2、壊れた aliases.json = exit 10（既存の Errors and exit codes 表の変更は不要と明記するか、表に注記を1行足す）。
- CLI ヘルプ例で `--node living-light` の使用例を1つ（`mat on -n living-light` 等）。

- [ ] **Step 4: 最終チェック**

Run: `task check`
Expected: fmt:check + clippy + test 全 PASS

- [ ] **Step 5: コミット**

```bash
git add CLAUDE.md ARCHITECTURE.md README.md
git commit -m "docs: aliases.json（optional な名前解決）を README/ARCHITECTURE/CLAUDE.md に反映"
```
