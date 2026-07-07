# group provision への ACL 書き込みステップ追加 — 実装計画

> **✅ 完了（2026-07-07）:** 全 Task 実装済み・main にコミット済み（b270b5e〜3f354a7、v0.13.0）。実機 6/6 グループキャスト動作確認済み。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat group provision`（直経路・matd 経由の両方）にデバイス ACL の read-merge-write ステップを追加し、修復コマンド `mat group grant` を新設する。

**Architecture:** 値の解釈・変換ロジックは新モジュール `crates/mat-core/src/acl.rs` に集約（TOO ログパーサ / ws 数値キー変換 / merge / compact JSON 生成）。`mat` 直経路は `commands/group.rs` の `ensure_group_acl`、matd は `server.rs` の `group_provision` step 4 からそれを呼ぶ。ACL write は**全置換**なので「read できたリスト + 追記」しか write せず、read 解釈不能なら `parse_error` で停止する（管理者エントリ喪失＝デバイス管理不能を防ぐ）。

**Tech Stack:** Rust（workspace: mat / matd / mat-core）、serde_json、assert_cmd + fake-chip-tool.sh（mat 統合テスト）、tokio-tungstenite fake-ws（matd 統合テスト）。

**Spec:** `docs/superpowers/specs/2026-07-07-group-provision-acl-design.md`

**Spec からの逸脱（1件）:** spec は「matd のバージョンを 0.10.0 に上げる」と書くが、バージョンは workspace 共有（ルート `Cargo.toml` の `version = "0.12.0"` を全 crate が継承）で既に 0.12.0。意図（挙動変更のバージョン反映)を保ち **workspace を 0.13.0 に上げる**（Task 7）。README の旧 matd 注記も「≤ 0.12」と書く。

## Global Constraints

- stdout は純粋な構造化 JSON のみ。`timestamp`（ISO 8601）必須。chip-tool 出力の素通し禁止。
- 診断・エラーは stderr に構造化ログ（`{"error":{"kind","detail"}}`）。
- ACL read が失敗・解釈不能なら**絶対に write しない**（write は全置換のため）。解釈不能は `ErrorKind::ParseError`（exit 1）。
- ACL write 失敗は既存 `classify_failure` で分類し fail-fast（部分結果を stdout に出さない）。
- privilege は 3 (Operate) 固定、authMode は 3 (Group) 固定、targets は null 固定（spec スコープ外: 削除・privilege 指定・targets 限定）。
- write JSON の fabricIndex は read で得た値をそのまま渡す（サーバ側で無視・置換される）。
- write JSON は**空白なし compact**（matd の ws コマンド行は空白が引数区切りのため必須）。
- `mat group grant` は**直経路のみ**。`--matd` 明示時は exit 2。matd の protocol.rs に op を追加しない。
- リポジトリは public。テスト・ドキュメントの値はダミーのみ（192.0.2.0/24、admin subject は chip-tool 既定の 112233）。
- 各コミット前に `task check`（fmt:check + clippy -D warnings + 全テスト）を通す。
- コミットは自分が編集したファイルのみ `git add`（セッション開始時から modified のファイルは含めない）。
- コミットメッセージ末尾: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`

## File Structure

- Create: `crates/mat-core/src/acl.rs` — ACL 値の解釈・変換（状態なし）。型 2 種 + 関数 5 種 + 単体テスト。
- Modify: `crates/mat-core/src/lib.rs` — `pub mod acl;` 追加。
- Modify: `crates/mat-core/src/parse.rs` — `strip_log_prefix` を `pub(crate)` に（acl.rs から再利用）。
- Modify: `crates/mat/src/commands/group.rs` — `ensure_group_acl`（共有ヘルパ）、`provision` step 4、`grant` 新設。
- Modify: `crates/mat/src/cli.rs` — `GroupCommand::Grant` 追加。
- Modify: `crates/mat/src/resolve.rs` — Grant の alias 解決 arm。
- Modify: `crates/mat/src/matd_client.rs` — Grant は matd 非対応（exit 2 / 直経路フォールバック）。
- Modify: `crates/mat/src/main.rs` — Grant のディスパッチ arm。
- Modify: `crates/mat/tests/fixtures/fake-chip-tool.sh` — `accesscontrol` ブランチ + 引数記録を追記式に。
- Modify: `crates/mat/tests/integration.rs` — provision の ACL ステップ列固定 + grant テスト。
- Modify: `crates/matd/src/server.rs` — `group_provision` に step 4。
- Modify: `crates/matd/tests/integration.rs` — fake-ws に ACL 応答 + 記録 fake + テスト 3 本。
- Modify: `Cargo.toml`（ルート） — version 0.12.0 → 0.13.0。
- Modify: `README.md` / `ARCHITECTURE.md` — ステップ説明・grant・トラブルシュート・旧 matd 注記。

---

### Task 1: mat-core `acl.rs` — 型・group エントリ生成・merge・write JSON

**Files:**
- Create: `crates/mat-core/src/acl.rs`
- Modify: `crates/mat-core/src/lib.rs`
- Test: `crates/mat-core/src/acl.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::error::MatError`（既存）
- Produces（後続 Task 2–6 が使う）:
  - `pub struct AclEntry { pub privilege: u8, pub auth_mode: u8, pub subjects: Vec<u64>, pub targets: Option<Vec<AclTarget>>, pub fabric_index: u8 }`（`Debug, Clone, PartialEq, Eq, Serialize` derive、serde rename_all = camelCase）
  - `pub struct AclTarget { pub cluster: Option<u32>, pub endpoint: Option<u16>, pub device_type: Option<u32> }`（同上）
  - `pub const PRIVILEGE_OPERATE: u8 = 3;` / `pub const AUTH_MODE_GROUP: u8 = 3;`
  - `pub fn group_acl_entry(group_id: u16, fabric_index: u8) -> AclEntry`
  - `pub fn merge_group_entry(entries: &[AclEntry], group_id: u16) -> Option<Vec<AclEntry>>`（`None` = 既に存在 = write 不要）
  - `pub fn to_chip_write_json(entries: &[AclEntry]) -> String`（空白なし compact、名前付きキー）

- [x] **Step 1: モジュール骨格 + 失敗するテストを書く**

`crates/mat-core/src/lib.rs` の `pub mod alias;` の直後に追加:

```rust
pub mod acl;
```

`crates/mat-core/src/acl.rs` を作成（まずテストと型だけ。関数本体は `todo!()` で置く）:

```rust
//! ACL（AccessControl クラスタ）値の解釈・変換。group provision / `mat group grant`
//! の read-merge-write ステップが使う。状態は持たない（設計ルール 4）。
//!
//! groupcast は authMode=Group で届くため、デバイス ACL に
//! `{privilege: Operate, authMode: Group, subjects: [<GroupId>]}` のエントリが無いと
//! デバイスが黙って捨てる（commissioning が作るのは CASE 管理者エントリだけ）。
//!
//! ACL の attribute write は**全置換**。write する値は必ず「read できたリスト + 追記」
//! だけから組み立てる。read が解釈できないときは `ErrorKind::ParseError` を返し、
//! 呼び出し側はそこで停止する（blind write は管理者エントリを失いデバイスが管理
//! 不能になるため、失敗側に倒す）。

use serde::Serialize;
use serde_json::Value;

use crate::error::MatError;
use crate::parse::strip_log_prefix;

/// Matter AccessControl の privilege。3 = Operate（Administer は authMode=Group と
/// 組み合わせ不可のため、group エントリは Operate 固定）。
pub const PRIVILEGE_OPERATE: u8 = 3;
/// Matter AccessControl の authMode。3 = Group。
pub const AUTH_MODE_GROUP: u8 = 3;

/// ACL エントリの target（クラスタ / エンドポイント / デバイス種別の限定）。
/// `mat` 自身は targets: null（全許可）しか生成しないが、既存エントリの保全のため
/// read 側は非 null も解釈できる必要がある。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AclTarget {
    pub cluster: Option<u32>,
    pub endpoint: Option<u16>,
    pub device_type: Option<u32>,
}

/// ACL エントリ。chip-tool の read 出力（TOO ログ / ws 値）から解釈し、write 用
/// JSON へ変換できる最小限の表現。Serialize は write JSON（名前付き camelCase キー、
/// chip-tool の accesscontrol write が受ける形）を直接生成する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AclEntry {
    pub privilege: u8,
    pub auth_mode: u8,
    pub subjects: Vec<u64>,
    pub targets: Option<Vec<AclTarget>>,
    /// read で得た値をそのまま write に渡す（サーバ側で無視・置換されるため
    /// ハードコード不要）。
    pub fabric_index: u8,
}

/// groupcast 許可用の ACL エントリを組み立てる。
pub fn group_acl_entry(group_id: u16, fabric_index: u8) -> AclEntry {
    todo!()
}

/// 既存 ACL に group エントリを追記した全リストを返す。既にあれば `None`（冪等、
/// write 不要）。fabricIndex は既存エントリの先頭から引き継ぐ（read 値をそのまま
/// 渡す方針。エントリ 0 件は起きない想定だが、その場合は 0 — サーバ側で置換される）。
pub fn merge_group_entry(entries: &[AclEntry], group_id: u16) -> Option<Vec<AclEntry>> {
    todo!()
}

/// `accesscontrol write acl` の引数用 compact JSON。matd の ws コマンド行は空白が
/// 引数区切りのため、空白なしであることが必須（serde_json の to_string は compact）。
pub fn to_chip_write_json(entries: &[AclEntry]) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// commissioning が作る CASE 管理者エントリ相当（subject 112233 は chip-tool の
    /// 既定 controller node id。ダミー値）。
    fn admin() -> AclEntry {
        AclEntry {
            privilege: 5,
            auth_mode: 2,
            subjects: vec![112233],
            targets: None,
            fabric_index: 4,
        }
    }

    #[test]
    fn group_acl_entry_is_operate_group() {
        let e = group_acl_entry(10, 4);
        assert_eq!(e.privilege, PRIVILEGE_OPERATE);
        assert_eq!(e.auth_mode, AUTH_MODE_GROUP);
        assert_eq!(e.subjects, vec![10]);
        assert_eq!(e.targets, None);
        assert_eq!(e.fabric_index, 4);
    }

    #[test]
    fn merge_appends_group_entry_preserving_existing() {
        let merged = merge_group_entry(&[admin()], 10).expect("should append");
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], admin());
        assert_eq!(merged[1], group_acl_entry(10, 4));
    }

    #[test]
    fn merge_is_none_when_group_entry_exists() {
        let entries = [admin(), group_acl_entry(10, 4)];
        assert!(merge_group_entry(&entries, 10).is_none());
    }

    #[test]
    fn merge_preserves_other_groups_entries() {
        // 同一デバイスへの複数グループ provision で先行グループを壊さない
        // （固定 2 エントリの blind write を不採用にした理由の回帰ガード）。
        let entries = [admin(), group_acl_entry(9, 4)];
        let merged = merge_group_entry(&entries, 10).expect("group 10 is new");
        assert_eq!(merged.len(), 3);
        assert!(merged.contains(&group_acl_entry(9, 4)));
        assert!(merged.contains(&group_acl_entry(10, 4)));
    }

    #[test]
    fn merge_ignores_case_entry_with_same_numeric_subject() {
        // subjects に同じ数値がいても authMode が Group でなければ「既存」とみなさない。
        let mut case_entry = admin();
        case_entry.subjects = vec![10];
        let merged = merge_group_entry(&[case_entry], 10).expect("must still append");
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn write_json_is_compact_named_keys() {
        let s = to_chip_write_json(&[admin(), group_acl_entry(10, 4)]);
        // ws コマンド行で 1 引数として渡すため空白なしが必須。
        assert!(!s.contains(' '), "must be compact: {s}");
        assert!(s.contains("\"privilege\":5"));
        assert!(s.contains("\"authMode\":2"));
        assert!(s.contains("\"authMode\":3"));
        assert!(s.contains("\"subjects\":[112233]"));
        assert!(s.contains("\"subjects\":[10]"));
        assert!(s.contains("\"targets\":null"));
        assert!(s.contains("\"fabricIndex\":4"));
    }

    #[test]
    fn write_json_round_trips_targets() {
        let entries = vec![AclEntry {
            privilege: 3,
            auth_mode: 3,
            subjects: vec![10],
            targets: Some(vec![AclTarget {
                cluster: Some(6),
                endpoint: None,
                device_type: None,
            }]),
            fabric_index: 1,
        }];
        let v: Value = serde_json::from_str(&to_chip_write_json(&entries)).unwrap();
        assert_eq!(v[0]["targets"][0]["cluster"], serde_json::json!(6));
        assert_eq!(v[0]["targets"][0]["endpoint"], Value::Null);
        assert_eq!(v[0]["targets"][0]["deviceType"], Value::Null);
    }
}
```

NOTE: この時点では `use crate::parse::strip_log_prefix;` と `use serde_json::Value;` が未使用（Task 2 で使う）。コンパイルを通すため Task 1 の間はこの 2 行を**入れない**こと（Task 2 で追加する）。

- [x] **Step 2: テストが失敗（todo! で panic）することを確認**

Run: `cargo test -p mat-core acl::`
Expected: FAIL（`not yet implemented` panic）

- [x] **Step 3: 最小実装**

`todo!()` を実装で置き換える:

```rust
pub fn group_acl_entry(group_id: u16, fabric_index: u8) -> AclEntry {
    AclEntry {
        privilege: PRIVILEGE_OPERATE,
        auth_mode: AUTH_MODE_GROUP,
        subjects: vec![u64::from(group_id)],
        targets: None,
        fabric_index,
    }
}

pub fn merge_group_entry(entries: &[AclEntry], group_id: u16) -> Option<Vec<AclEntry>> {
    let gid = u64::from(group_id);
    if entries
        .iter()
        .any(|e| e.auth_mode == AUTH_MODE_GROUP && e.subjects.contains(&gid))
    {
        return None;
    }
    let fabric_index = entries.first().map(|e| e.fabric_index).unwrap_or(0);
    let mut merged = entries.to_vec();
    merged.push(group_acl_entry(group_id, fabric_index));
    Some(merged)
}

pub fn to_chip_write_json(entries: &[AclEntry]) -> String {
    serde_json::to_string(entries).expect("AclEntry serialization cannot fail")
}
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core acl::`
Expected: PASS（7 tests）

- [x] **Step 5: task check + コミット**

Run: `task check`
Expected: fmt / clippy / 全テスト PASS

```bash
git add crates/mat-core/src/acl.rs crates/mat-core/src/lib.rs
git commit -m "feat(mat-core): ACL エントリ型と merge / write-JSON 生成（acl.rs）

group provision の ACL read-merge-write ステップの土台。write は全置換のため
「read できたリスト + 追記」のみを生成し、既存エントリ（先行グループ含む）を保全する。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: mat-core `parse_acl_from_chip_log` — TOO ログパーサ（直経路用）

**Files:**
- Modify: `crates/mat-core/src/acl.rs`
- Modify: `crates/mat-core/src/parse.rs:360`（`strip_log_prefix` の可視性のみ）
- Test: `crates/mat-core/src/acl.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `AclEntry` / `AclTarget`（Task 1）、`crate::parse::strip_log_prefix`（本 Task で `pub(crate)` 化）
- Produces: `pub fn parse_acl_from_chip_log(stdout: &str) -> Result<Vec<AclEntry>, MatError>` — 解釈不能は `ErrorKind::ParseError`。Task 4/5（mat 直経路）が使う。

- [x] **Step 1: `strip_log_prefix` を pub(crate) 化**

`crates/mat-core/src/parse.rs` の 360 行付近:

```rust
fn strip_log_prefix(line: &str) -> Option<&str> {
```

を次に変更（doc コメントはそのまま）:

```rust
pub(crate) fn strip_log_prefix(line: &str) -> Option<&str> {
```

- [x] **Step 2: 失敗するテストを書く**

`crates/mat-core/src/acl.rs` の `tests` モジュールに追加:

```rust
    use crate::error::ErrorKind;

    /// 実機 chip-tool の `accesscontrol read acl` TOO ログ（admin 1 エントリ）。
    /// この形式は 2026-07-06 の実機デバッグ（jarvis）に基づく想定形。upstream の
    /// バージョン変化はこのテストで検知する（CLAUDE.md の fragile-parse ルール）。
    const ACL_ADMIN_ONLY: &str = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";

    const ACL_ADMIN_AND_GROUP: &str = "\
[1656][CHIP:TOO]   ACL: 2 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
[1656][CHIP:TOO]     [2]: {
[1656][CHIP:TOO]       Privilege: 3
[1656][CHIP:TOO]       AuthMode: 3
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 10
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";

    #[test]
    fn too_log_parses_admin_only() {
        let entries = parse_acl_from_chip_log(ACL_ADMIN_ONLY).unwrap();
        assert_eq!(entries, vec![admin()]);
    }

    #[test]
    fn too_log_parses_admin_and_group() {
        let entries = parse_acl_from_chip_log(ACL_ADMIN_AND_GROUP).unwrap();
        assert_eq!(entries, vec![admin(), group_acl_entry(10, 4)]);
    }

    #[test]
    fn too_log_parses_non_null_targets() {
        // 他 admin が書いた targets 限定エントリも保全のため解釈できること。
        let s = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 3
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: 1 entries
[1656][CHIP:TOO]         [1]: {
[1656][CHIP:TOO]           Cluster: 6
[1656][CHIP:TOO]           Endpoint: null
[1656][CHIP:TOO]           DeviceType: null
[1656][CHIP:TOO]          }
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";
        let entries = parse_acl_from_chip_log(s).unwrap();
        assert_eq!(
            entries[0].targets,
            Some(vec![AclTarget {
                cluster: Some(6),
                endpoint: None,
                device_type: None,
            }])
        );
    }

    #[test]
    fn too_log_zero_entries_is_ok_empty() {
        let entries =
            parse_acl_from_chip_log("[1656][CHIP:TOO]   ACL: 0 entries\n").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn too_log_realworld_prefix_format() {
        // 実機 v1.4.2.0 のログ接頭辞（小数点 ts + pid:tid + CHIP: 無しタグ）。
        let s = "\
[1780817887.948] [32231:32235] [TOO]   ACL: 1 entries
[1780817887.948] [32231:32235] [TOO]     [1]: {
[1780817887.948] [32231:32235] [TOO]       Privilege: 5
[1780817887.948] [32231:32235] [TOO]       AuthMode: 2
[1780817887.948] [32231:32235] [TOO]       Subjects: 1 entries
[1780817887.948] [32231:32235] [TOO]         [1]: 112233
[1780817887.948] [32231:32235] [TOO]       Targets: null
[1780817887.948] [32231:32235] [TOO]       FabricIndex: 4
[1780817887.948] [32231:32235] [TOO]      }
";
        assert_eq!(parse_acl_from_chip_log(s).unwrap(), vec![admin()]);
    }

    #[test]
    fn too_log_broken_output_is_parse_error() {
        // ヘッダ無し / エントリ数不一致（途中で切れた出力）はどちらも ParseError。
        // 解釈できないまま write すると管理者エントリを失いかねないため、失敗側に倒す。
        for s in [
            "no acl here",
            "[1656][CHIP:TOO] something unparseable",
            // ヘッダは 2 entries だが 1 つしか無い（truncated）。
            "[1656][CHIP:TOO]   ACL: 2 entries\n\
             [1656][CHIP:TOO]     [1]: {\n\
             [1656][CHIP:TOO]       Privilege: 5\n\
             [1656][CHIP:TOO]       AuthMode: 2\n\
             [1656][CHIP:TOO]       Targets: null\n\
             [1656][CHIP:TOO]       FabricIndex: 4\n\
             [1656][CHIP:TOO]      }\n",
            // 必須フィールド欠け（Privilege 無し）。
            "[1656][CHIP:TOO]   ACL: 1 entries\n\
             [1656][CHIP:TOO]     [1]: {\n\
             [1656][CHIP:TOO]       AuthMode: 2\n\
             [1656][CHIP:TOO]       Targets: null\n\
             [1656][CHIP:TOO]       FabricIndex: 4\n\
             [1656][CHIP:TOO]      }\n",
        ] {
            let err = parse_acl_from_chip_log(s).expect_err(&format!("must fail: {s}"));
            assert_eq!(err.kind, ErrorKind::ParseError, "input: {s}");
        }
    }
```

- [x] **Step 3: テストが失敗することを確認**

Run: `cargo test -p mat-core acl::`
Expected: FAIL（`parse_acl_from_chip_log` 未定義のコンパイルエラー）

- [x] **Step 4: パーサを実装**

`crates/mat-core/src/acl.rs` の import に追加:

```rust
use crate::parse::strip_log_prefix;
```

関数群を追加（`to_chip_write_json` の下）:

```rust
/// chip-tool 直経路の `accesscontrol read acl <node> 0` stdout（TOO ログ形式）を
/// 解釈する。
///
/// 想定形（NeighborTable 等と同じ DataModelLogger の list-of-struct 整形。ただし
/// Subjects / Targets の**ネストしたリスト**を含むため `parse_struct_list` では
/// 表現できず専用パーサとする）:
/// ```text
///   ACL: 2 entries
///     [1]: {
///       Privilege: 5
///       AuthMode: 2
///       Subjects: 1 entries
///         [1]: 112233
///       Targets: null
///       FabricIndex: 4
///      }
///     [2]: { ... }
/// ```
/// 安全弁（いずれも `ParseError`）: `ACL: n entries` ヘッダが無い / パースできた
/// エントリ数がヘッダと合わない（途中で切れた出力）/ 必須フィールド欠け /
/// 予期しない行。ACL write は全置換なので、部分的にしか読めていないリストで
/// write すると読み落としたエントリを消してしまう — 迷ったら失敗させる。
pub fn parse_acl_from_chip_log(stdout: &str) -> Result<Vec<AclEntry>, MatError> {
    let mut declared: Option<usize> = None;
    let mut entries: Vec<AclEntry> = Vec::new();
    let mut cur: Option<EntryBuilder> = None;
    let mut cur_target: Option<TargetBuilder> = None;
    let mut section = Section::Fields;

    for line in stdout.lines() {
        let Some(payload) = strip_log_prefix(line) else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }

        // ヘッダ `ACL: n entries`（エントリ外でのみ現れる）。
        if cur.is_none() {
            if let Some(rest) = payload.strip_prefix("ACL:") {
                let n = rest
                    .split_whitespace()
                    .next()
                    .and_then(|t| t.parse::<usize>().ok())
                    .ok_or_else(|| {
                        MatError::parse_error(format!("unparseable ACL header: {payload}"))
                    })?;
                declared = Some(n);
                continue;
            }
        }

        // インデックス行 `[i]: ...`（エントリ開始 / subject / target 開始）。
        if let Some(rest) = index_line(payload) {
            match (&mut cur, &section) {
                (None, _) if rest.starts_with('{') => {
                    cur = Some(EntryBuilder::default());
                    section = Section::Fields;
                }
                (Some(b), Section::Subjects) => {
                    let head = rest.split_whitespace().next().unwrap_or(rest);
                    let v = head.trim_end_matches(',').parse::<u64>().map_err(|_| {
                        MatError::parse_error(format!("unparseable ACL subject: {payload}"))
                    })?;
                    b.subjects.push(v);
                }
                (Some(_), Section::Targets) if rest.starts_with('{') => {
                    cur_target = Some(TargetBuilder::default());
                }
                _ => {
                    return Err(MatError::parse_error(format!(
                        "unexpected line in ACL output: {payload}"
                    )))
                }
            }
            continue;
        }

        // 閉じ括弧: target → entry の順で内側から閉じる。
        if payload.starts_with('}') {
            if let Some(t) = cur_target.take() {
                let Some(b) = cur.as_mut() else {
                    return Err(MatError::parse_error("ACL target outside an entry"));
                };
                b.targets.get_or_insert_with(Vec::new).push(AclTarget {
                    cluster: t.cluster,
                    endpoint: t.endpoint,
                    device_type: t.device_type,
                });
            } else if let Some(b) = cur.take() {
                entries.push(b.build()?);
                section = Section::Fields;
            }
            continue;
        }

        // フィールド行 `Key: Value`。エントリ外の無関係行は無視する。
        let Some(colon) = payload.find(':') else { continue };
        let key = payload[..colon].trim();
        let val = payload[colon + 1..].trim().trim_end_matches(',').trim();

        if let Some(t) = cur_target.as_mut() {
            match key {
                "Cluster" => t.cluster = field_opt_num(val, "target Cluster")?,
                "Endpoint" => t.endpoint = field_opt_num(val, "target Endpoint")?,
                "DeviceType" => t.device_type = field_opt_num(val, "target DeviceType")?,
                _ => {}
            }
            continue;
        }
        let Some(b) = cur.as_mut() else { continue };
        match key {
            "Privilege" => b.privilege = Some(field_num(val, "Privilege")?),
            "AuthMode" => b.auth_mode = Some(field_num(val, "AuthMode")?),
            "FabricIndex" => {
                b.fabric_index = Some(field_num(val, "FabricIndex")?);
                section = Section::Fields;
            }
            "Subjects" => {
                section = if val.starts_with("null") {
                    Section::Fields
                } else {
                    Section::Subjects
                };
            }
            "Targets" => {
                if val.starts_with("null") {
                    b.targets = None;
                    section = Section::Fields;
                } else {
                    b.targets = Some(Vec::new());
                    section = Section::Targets;
                }
            }
            _ => {}
        }
    }

    let declared = declared
        .ok_or_else(|| MatError::parse_error("no `ACL: n entries` header in chip-tool output"))?;
    if entries.len() != declared || cur.is_some() || cur_target.is_some() {
        return Err(MatError::parse_error(format!(
            "ACL parse mismatch: header declared {declared} entries, parsed {} (refusing to write a possibly truncated list)",
            entries.len()
        )));
    }
    Ok(entries)
}

/// パーサ内部: 構築途中のエントリ。
#[derive(Default)]
struct EntryBuilder {
    privilege: Option<u8>,
    auth_mode: Option<u8>,
    subjects: Vec<u64>,
    targets: Option<Vec<AclTarget>>,
    fabric_index: Option<u8>,
}

impl EntryBuilder {
    fn build(self) -> Result<AclEntry, MatError> {
        Ok(AclEntry {
            privilege: self
                .privilege
                .ok_or_else(|| MatError::parse_error("ACL entry missing Privilege"))?,
            auth_mode: self
                .auth_mode
                .ok_or_else(|| MatError::parse_error("ACL entry missing AuthMode"))?,
            subjects: self.subjects,
            targets: self.targets,
            fabric_index: self
                .fabric_index
                .ok_or_else(|| MatError::parse_error("ACL entry missing FabricIndex"))?,
        })
    }
}

/// パーサ内部: 構築途中の target。
#[derive(Default)]
struct TargetBuilder {
    cluster: Option<u32>,
    endpoint: Option<u16>,
    device_type: Option<u32>,
}

/// 現エントリ内でインデックス行（`[i]: ...`）が属するリスト。
enum Section {
    Fields,
    Subjects,
    Targets,
}

/// `[i]: <rest>` 形（i は数値）の行なら `<rest>` を返す。
fn index_line(payload: &str) -> Option<&str> {
    let inner = payload.strip_prefix('[')?;
    let close = inner.find(']')?;
    inner[..close].trim().parse::<u64>().ok()?;
    inner[close + 1..]
        .trim_start()
        .strip_prefix(':')
        .map(str::trim)
}

/// フィールド値の数値解釈。実機の名前注釈付き（`5 (Administer)`）も先頭トークンで読む。
fn field_num<T: TryFrom<u64>>(val: &str, what: &str) -> Result<T, MatError> {
    let head = val.split_whitespace().next().unwrap_or(val);
    head.parse::<u64>()
        .ok()
        .and_then(|v| T::try_from(v).ok())
        .ok_or_else(|| MatError::parse_error(format!("unparseable ACL {what}: {val}")))
}

/// `null` 許容の数値フィールド。
fn field_opt_num<T: TryFrom<u64>>(val: &str, what: &str) -> Result<Option<T>, MatError> {
    if val.starts_with("null") {
        return Ok(None);
    }
    field_num(val, what).map(Some)
}
```

- [x] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat-core acl::`
Expected: PASS（Task 1 の 7 本 + 本 Task の 6 本）

- [x] **Step 6: task check + コミット**

Run: `task check`
Expected: PASS

```bash
git add crates/mat-core/src/acl.rs crates/mat-core/src/parse.rs
git commit -m "feat(mat-core): accesscontrol read acl の TOO ログパーサ

ACL write は全置換のため、ヘッダとエントリ数の不一致（truncated 出力）や必須
フィールド欠けは ParseError にして write を止める。形式は単体テストで固定し、
chip-tool のバージョン変化を検知する砦にする。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: mat-core `acl_entries_from_ws_value` — ws 数値キー変換（matd 用）

**Files:**
- Modify: `crates/mat-core/src/acl.rs`
- Test: `crates/mat-core/src/acl.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `AclEntry` / `AclTarget`（Task 1）
- Produces: `pub fn acl_entries_from_ws_value(value: &serde_json::Value) -> Result<Vec<AclEntry>, MatError>` — Task 6（matd）が `results[0].value` を渡す。

- [x] **Step 1: 失敗するテストを書く**

`tests` モジュールに追加:

```rust
    use serde_json::json;

    #[test]
    fn ws_value_numeric_keys_parse() {
        // 実機で確定済みの ws 応答形: 数値フィールド ID がキー
        // （"1"=privilege, "2"=authMode, "3"=subjects, "4"=targets, "254"=fabricIndex）。
        let v = json!([{"1":5,"2":2,"3":[112233],"4":null,"254":4}]);
        assert_eq!(acl_entries_from_ws_value(&v).unwrap(), vec![admin()]);
    }

    #[test]
    fn ws_value_parses_admin_and_group() {
        let v = json!([
            {"1":5,"2":2,"3":[112233],"4":null,"254":4},
            {"1":3,"2":3,"3":[10],"4":null,"254":4}
        ]);
        assert_eq!(
            acl_entries_from_ws_value(&v).unwrap(),
            vec![admin(), group_acl_entry(10, 4)]
        );
    }

    #[test]
    fn ws_value_targets_non_null() {
        // targets 内は "0"=cluster, "1"=endpoint, "2"=deviceType。
        let v = json!([{"1":3,"2":2,"3":[112233],"4":[{"0":6,"1":1,"2":null}],"254":4}]);
        let entries = acl_entries_from_ws_value(&v).unwrap();
        assert_eq!(
            entries[0].targets,
            Some(vec![AclTarget {
                cluster: Some(6),
                endpoint: Some(1),
                device_type: None,
            }])
        );
    }

    #[test]
    fn ws_value_bad_shape_is_parse_error() {
        for v in [
            json!(true),                 // 配列ですらない
            json!([42]),                 // 要素がオブジェクトでない
            json!([{"2":2,"254":1}]),    // privilege（"1"）欠け
            json!([{"1":5,"2":2,"3":"x","254":1}]), // subjects が配列でない
        ] {
            let err = acl_entries_from_ws_value(&v).expect_err(&format!("must fail: {v}"));
            assert_eq!(err.kind, ErrorKind::ParseError, "input: {v}");
        }
    }
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-core acl::`
Expected: FAIL（`acl_entries_from_ws_value` 未定義のコンパイルエラー）

- [x] **Step 3: 実装**

import に `use serde_json::Value;` を追加し、`parse_acl_from_chip_log` の下に:

```rust
/// matd（ws）経路の `accesscontrol read acl` 応答 `results[0].value` を解釈する。
///
/// ws 値は数値フィールド ID キーのオブジェクト配列（実機で確定済みの形）:
/// `[{"1":5,"2":2,"3":[112233],"4":null,"254":4}]`
/// （`"1"`=privilege, `"2"`=authMode, `"3"`=subjects, `"4"`=targets,
/// `"254"`=fabricIndex。targets 内は `"0"`=cluster, `"1"`=endpoint,
/// `"2"`=deviceType）。解釈不能は `ParseError`（write を止める）。
pub fn acl_entries_from_ws_value(value: &Value) -> Result<Vec<AclEntry>, MatError> {
    let arr = value.as_array().ok_or_else(|| {
        MatError::parse_error(format!("ACL ws value is not an array: {value}"))
    })?;
    arr.iter().map(ws_entry).collect()
}

fn ws_entry(v: &Value) -> Result<AclEntry, MatError> {
    let obj = v
        .as_object()
        .ok_or_else(|| MatError::parse_error(format!("ACL ws entry is not an object: {v}")))?;
    let subjects = match obj.get("3") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|s| {
                s.as_u64().ok_or_else(|| {
                    MatError::parse_error(format!("ACL ws subject is not an integer: {s}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => {
            return Err(MatError::parse_error(format!(
                "ACL ws subjects (field 3) is not an array: {other}"
            )))
        }
    };
    let targets = match obj.get("4") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(a.iter().map(ws_target).collect::<Result<Vec<_>, _>>()?),
        Some(other) => {
            return Err(MatError::parse_error(format!(
                "ACL ws targets (field 4) is not an array: {other}"
            )))
        }
    };
    Ok(AclEntry {
        privilege: ws_u8(obj, "1", "privilege")?,
        auth_mode: ws_u8(obj, "2", "authMode")?,
        subjects,
        targets,
        fabric_index: ws_u8(obj, "254", "fabricIndex")?,
    })
}

fn ws_u8(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<u8, MatError> {
    obj.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| {
            MatError::parse_error(format!("ACL ws entry missing/invalid {what} (field {key})"))
        })
}

fn ws_target(v: &Value) -> Result<AclTarget, MatError> {
    let obj = v
        .as_object()
        .ok_or_else(|| MatError::parse_error(format!("ACL ws target is not an object: {v}")))?;
    Ok(AclTarget {
        cluster: ws_opt_num(obj, "0")?,
        endpoint: ws_opt_num(obj, "1")?,
        device_type: ws_opt_num(obj, "2")?,
    })
}

fn ws_opt_num<T: TryFrom<u64>>(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<T>, MatError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| T::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                MatError::parse_error(format!("ACL ws target field {key} is invalid: {v}"))
            }),
    }
}
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core acl::`
Expected: PASS（累計 17 本）

- [x] **Step 5: task check + コミット**

Run: `task check`
Expected: PASS

```bash
git add crates/mat-core/src/acl.rs
git commit -m "feat(mat-core): matd ws 応答（数値キー形式）の ACL 変換

ws の accesscontrol read acl は数値フィールド ID キーで返る（実機確定）。
matd の group_provision step 4 がこの変換を使う。解釈不能は ParseError。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: mat 直経路 — provision に step 4（ACL read-merge-write）

**Files:**
- Modify: `crates/mat/tests/fixtures/fake-chip-tool.sh`
- Modify: `crates/mat/src/commands/group.rs`
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: `mat_core::acl::{merge_group_entry, parse_acl_from_chip_log, to_chip_write_json}`（Task 1–2）、既存 `ChipTool::run` / `classify_failure` / `run_node_step`
- Produces: `fn ensure_group_acl(chip: &ChipTool, node_id: u64, group_id: u16) -> Result<bool, MatError>`（group.rs 内 private。戻り値 true = write した / false = 既存で skip。Task 5 の `grant` も使う）。fake-chip-tool の `accesscontrol` ブランチと制御 env `FAKE_ACL_HAS_GROUP` / `FAKE_ACL_BROKEN`、および `FAKE_CHIP_ARGS_FILE` の**追記式**記録（Task 5–6 のテストも前提にする）。

- [x] **Step 1: fake-chip-tool を更新（記録を追記式に + accesscontrol ブランチ）**

`crates/mat/tests/fixtures/fake-chip-tool.sh` の記録部分（13–15 行目）:

```sh
# テスト検証用: 受け取った全引数を記録（PAA フラグ受け渡し等の確認に使う）。
if [ -n "$FAKE_CHIP_ARGS_FILE" ]; then
  echo "$*" > "$FAKE_CHIP_ARGS_FILE"
fi
```

を追記式に変更（provision のステップ**列**を 1 ファイルで検証できるようにする。既存テストの検証は全て `contains` なので互換）:

```sh
# テスト検証用: 受け取った全引数を記録（1 呼び出し = 1 行、追記式。mat 1 回の
# 実行内の複数 chip-tool 呼び出しのステップ列を検証できる）。
if [ -n "$FAKE_CHIP_ARGS_FILE" ]; then
  echo "$*" >> "$FAKE_CHIP_ARGS_FILE"
fi
```

`groups)` ケースの直後（`descriptor)` の前）に `accesscontrol` ブランチを追加:

```sh
  accesscontrol)
    # provision step 4 / grant の ACL read-merge-write。read は TOO ログ形式で
    # エントリ列を吐く（実機 v1.4.x 相当）。
    #   FAKE_ACL_HAS_GROUP=1 → group 1 のエントリ入り（write スキップのテスト用）
    #   FAKE_ACL_BROKEN=1    → 解釈不能出力（parse_error で write しないテスト用）
    op="$2"
    emit_failure
    if [ "$op" = "read" ]; then
      if [ -n "$FAKE_ACL_BROKEN" ]; then
        echo "[1656][CHIP:TOO] something unparseable"
        exit 0
      fi
      cat <<'EOF'
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 1
[1656][CHIP:TOO]      }
EOF
      if [ -n "$FAKE_ACL_HAS_GROUP" ]; then
        # 2 エントリ目（group 1）。ヘッダの件数も合わせる必要があるため、
        # HAS_GROUP のときは上の 1 エントリ出力を使わず全体を出し直す…わけには
        # いかないので、ここは cat を分岐の中に置く（下の Step 1 補足参照）。
        :
      fi
      exit 0
    fi
    # write: attribute write の成功形（既存 write と同じ status 行）。
    echo "[1656][CHIP:DMG] AttributeStatusIB ="
    echo "[1656][CHIP:DMG]   status = 0x00 (SUCCESS),"
    exit 0
    ;;
```

**Step 1 補足（上のままでは HAS_GROUP でヘッダ件数が合わない）**: read 部分は最終的に次の形にする（こちらが完成形。上の断片ではなくこれを書く）:

```sh
  accesscontrol)
    # provision step 4 / grant の ACL read-merge-write。read は TOO ログ形式で
    # エントリ列を吐く（実機 v1.4.x 相当）。
    #   FAKE_ACL_HAS_GROUP=1 → group 1 のエントリ入り（write スキップのテスト用）
    #   FAKE_ACL_BROKEN=1    → 解釈不能出力（parse_error で write しないテスト用）
    op="$2"
    emit_failure
    if [ "$op" = "read" ]; then
      if [ -n "$FAKE_ACL_BROKEN" ]; then
        echo "[1656][CHIP:TOO] something unparseable"
        exit 0
      fi
      if [ -n "$FAKE_ACL_HAS_GROUP" ]; then
        cat <<'EOF'
[1656][CHIP:TOO]   ACL: 2 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 1
[1656][CHIP:TOO]      }
[1656][CHIP:TOO]     [2]: {
[1656][CHIP:TOO]       Privilege: 3
[1656][CHIP:TOO]       AuthMode: 3
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 1
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 1
[1656][CHIP:TOO]      }
EOF
      else
        cat <<'EOF'
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 1
[1656][CHIP:TOO]      }
EOF
      fi
      exit 0
    fi
    # write: attribute write の成功形（既存 write と同じ status 行）。
    echo "[1656][CHIP:DMG] AttributeStatusIB ="
    echo "[1656][CHIP:DMG]   status = 0x00 (SUCCESS),"
    exit 0
    ;;
```

- [x] **Step 2: 失敗する統合テストを書く**

`crates/mat/tests/integration.rs` の既存テスト `group_provision_last_chip_call_is_add_group`（727–753 行）を次で**置き換え**、さらに 2 本追加:

```rust
#[test]
fn group_provision_runs_acl_read_merge_write_after_add_group() {
    // provision の 4 ステップ目: add-group の後に acl read → （エントリが無いので）
    // 全リスト + group エントリの write が走る。ステップ列を固定する。
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args([
            "group",
            "provision",
            "--group",
            "7",
            "--nodes",
            "5",
            "--name",
            "kitchen",
            "--endpoint",
            "2",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"provisioned\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    let add = recorded
        .find("groups add-group 7 kitchen 5 2")
        .expect("add-group call missing");
    let read = recorded
        .find("accesscontrol read acl 5 0")
        .expect("acl read call missing");
    let write = recorded
        .find("accesscontrol write acl ")
        .expect("acl write call missing");
    assert!(
        add < read && read < write,
        "acl steps out of order: {recorded}"
    );
    // write は「read できたリスト + 追記」の全置換: admin エントリ保全 + group 7。
    let write_line = recorded
        .lines()
        .find(|l| l.contains("accesscontrol write acl"))
        .unwrap();
    assert!(write_line.contains("\"subjects\":[112233]"), "{write_line}");
    assert!(write_line.contains("\"authMode\":3"), "{write_line}");
    assert!(write_line.contains("\"subjects\":[7]"), "{write_line}");
    // JSON は空白なし 1 引数（`acl ` と ` 5 0` の間に空白が無い）。
    let json_part = write_line
        .split("accesscontrol write acl ")
        .nth(1)
        .unwrap()
        .split(" 5 0")
        .next()
        .unwrap();
    assert!(!json_part.contains(' '), "write JSON must be compact: {json_part}");
}

#[test]
fn group_provision_skips_acl_write_when_entry_exists() {
    // 既に group 1 のエントリがある → 冪等: write は飛ばない。
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .env("FAKE_ACL_HAS_GROUP", "1")
        .args(["group", "provision", "--group", "1", "--nodes", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"provisioned\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(recorded.contains("accesscontrol read acl 5 0"));
    assert!(
        !recorded.contains("accesscontrol write acl"),
        "must not write when the group entry already exists: {recorded}"
    );
}

#[test]
fn group_provision_broken_acl_read_is_parse_error_without_write() {
    // ACL read が解釈不能 → parse_error（exit 1）で停止し、絶対に write しない
    // （write は全置換。解釈できないまま書くと管理者エントリを失う）。
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .env("FAKE_ACL_BROKEN", "1")
        .args(["group", "provision", "--group", "1", "--nodes", "5"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("parse_error"));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        !recorded.contains("accesscontrol write acl"),
        "must never write after an unparseable read: {recorded}"
    );
}
```

- [x] **Step 3: テストが失敗することを確認**

Run: `cargo test -p mat --test integration group_provision`
Expected: 新 3 本が FAIL（acl read の呼び出しが無い / exit 0 のまま）。既存 `group_provision_succeeds` 等は PASS のまま。

- [x] **Step 4: `ensure_group_acl` を実装し provision に組み込む**

`crates/mat/src/commands/group.rs` の import を更新:

```rust
use mat_core::acl::{merge_group_entry, parse_acl_from_chip_log, to_chip_write_json};
use mat_core::error::{ErrorKind, MatError};
```

`provision()` のノードループ内、`groups add-group` の `run_node_step(...)?;`（146 行目付近）の直後に追加:

```rust
        // ACL: groupcast は authMode=Group で届くため、Group エントリが無いと
        // デバイスが黙って捨てる（commissioning が作るのは CASE 管理者エントリだけ）。
        ensure_group_acl(&chip, node_id, group_id)?;
```

ファイル末尾（`run_node_step` の下）にヘルパを追加:

```rust
/// ACL の read-merge-write（provision の step 4 / `mat group grant` の本体）。
/// 戻り値: write した = true / 既に Group エントリがあり skip = false（冪等）。
///
/// ACL の attribute write は**全置換**なので、write は必ず「read できたリスト +
/// 追記」のみ。read が失敗・解釈不能なら絶対に write しない（管理者エントリを
/// 失うとデバイスが管理不能になり工場リセット行きのため）。
fn ensure_group_acl(chip: &ChipTool, node_id: u64, group_id: u16) -> Result<bool, MatError> {
    // read。属性 read は成功時に status 行を出さない（operation_succeeded が偽に
    // なる）ため run_node_step は使わず、分類 + パースで成否を判定する。
    let out = chip.run(vec![
        "accesscontrol".to_string(),
        "read".into(),
        "acl".into(),
        node_id.to_string(),
        "0".into(),
    ])?;
    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("provision step 'acl read' failed on node {node_id}"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!("provision step 'acl read' on node {node_id} did not succeed"),
        ));
    }
    let entries = parse_acl_from_chip_log(&out.stdout)
        .map_err(|e| MatError::new(e.kind, format!("acl read on node {node_id}: {}", e.detail)))?;

    let Some(merged) = merge_group_entry(&entries, group_id) else {
        return Ok(false); // 既に Group エントリがある。write 不要（冪等）。
    };
    run_node_step(
        chip,
        vec![
            "accesscontrol".to_string(),
            "write".into(),
            "acl".into(),
            to_chip_write_json(&merged),
            node_id.to_string(),
            "0".into(),
        ],
        node_id,
        "acl write",
    )?;
    Ok(true)
}
```

- [x] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat --test integration`
Expected: 全 PASS（既存 fixture 変更の影響も含めて確認。`>>` 化で落ちるテストは無い想定 — 落ちたら該当 assert を確認）

- [x] **Step 6: task check + コミット**

Run: `task check`
Expected: PASS

```bash
git add crates/mat/src/commands/group.rs crates/mat/tests/fixtures/fake-chip-tool.sh crates/mat/tests/integration.rs
git commit -m "feat(mat): group provision に ACL read-merge-write ステップを追加

groupcast は authMode=Group で届くため、デバイス ACL に Group エントリが無いと
全デバイスが黙殺する（2026-07-06 実機で確定した設計ギャップ）。各ノード処理の
4 ステップ目として read-merge-write を追加。read 解釈不能時は write せず
parse_error で停止する。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: `mat group grant` — ACL 修復サブコマンド（直経路のみ）

**Files:**
- Modify: `crates/mat/src/cli.rs`（GroupCommand に Grant 追加）
- Modify: `crates/mat/src/resolve.rs`（alias 解決 arm）
- Modify: `crates/mat/src/matd_client.rs`（matd 非対応扱い + 単体テスト）
- Modify: `crates/mat/src/main.rs`（ディスパッチ arm）
- Modify: `crates/mat/src/commands/group.rs`（`grant` 実装）
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: `ensure_group_acl`（Task 4）、既存 `Store` / `output::emit` / alias 解決層
- Produces:
  - CLI: `mat group grant --group <ID|ALIAS> --nodes <N|ALIAS>...`
  - `pub fn grant(store_path: &Path, group_id: u16, node_ids: &[u64]) -> Result<(), MatError>`（commands::group）
  - stdout: `{"timestamp": ..., "group_id": 10, "nodes": [5,7,8], "updated": [5,7], "unchanged": [8], "status": "granted"}`

- [x] **Step 1: 失敗する統合テストを書く**

`crates/mat/tests/integration.rs` の groupcast 節に追加:

```rust
#[test]
fn group_grant_appends_acl_entry_and_reports_updated() {
    // provision 済みで ACL だけ欠けたグループの修復（grant の主目的。実機 jarvis の
    // group 10 相当: fake の既定 ACL は admin エントリのみ = ACL 欠落状態）。
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["group", "grant", "--group", "10", "--nodes", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"group_id\":10"))
        .stdout(predicate::str::contains("\"nodes\":[5]"))
        .stdout(predicate::str::contains("\"updated\":[5]"))
        .stdout(predicate::str::contains("\"unchanged\":[]"))
        .stdout(predicate::str::contains("\"status\":\"granted\""))
        .stdout(predicate::str::contains("\"timestamp\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(recorded.contains("accesscontrol read acl 5 0"));
    let write_line = recorded
        .lines()
        .find(|l| l.contains("accesscontrol write acl"))
        .expect("acl write call missing");
    // 既存 admin エントリを保全した全置換 + group 10 の Operate/Group エントリ。
    assert!(write_line.contains("\"subjects\":[112233]"), "{write_line}");
    assert!(write_line.contains("\"subjects\":[10]"), "{write_line}");
    assert!(write_line.contains("\"privilege\":3"), "{write_line}");
    assert!(write_line.contains("\"authMode\":3"), "{write_line}");
}

#[test]
fn group_grant_reports_unchanged_when_entry_exists() {
    // 既にエントリがある → 冪等: write せず unchanged に載せる。
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .env("FAKE_ACL_HAS_GROUP", "1")
        .args(["group", "grant", "--group", "1", "--nodes", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"updated\":[]"))
        .stdout(predicate::str::contains("\"unchanged\":[5]"))
        .stdout(predicate::str::contains("\"status\":\"granted\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        !recorded.contains("accesscontrol write acl"),
        "must not write when the entry already exists: {recorded}"
    );
}

#[test]
fn group_grant_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["group", "grant", "--group", "1", "--nodes", "99"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn group_grant_broken_acl_read_is_parse_error_without_write() {
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .env("FAKE_ACL_BROKEN", "1")
        .args(["group", "grant", "--group", "1", "--nodes", "5"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("parse_error"));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(!recorded.contains("accesscontrol write acl"), "{recorded}");
}

#[test]
fn group_grant_with_forced_matd_exits_2() {
    // grant は直経路のみ（matd プロトコルに op を追加しない）。--matd 明示は
    // discover / commission と同じ「非対応サブコマンド」扱いで exit 2。
    let store = store_with_node5();
    mat(store.path())
        .args([
            "--matd",
            "/nonexistent/matd.sock",
            "group",
            "grant",
            "--group",
            "1",
            "--nodes",
            "5",
        ])
        .assert()
        .code(2);
}
```

`crates/mat/src/matd_client.rs` の `tests` モジュールにも単体テストを追加:

```rust
    #[test]
    fn group_grant_is_unsupported_via_matd() {
        // grant は稀な修復操作で warm session の恩恵が小さく、mat/matd バージョン
        // スキューにも安全なため直経路のみ（matd プロトコルに op を足さない）。
        let cmd = Command::Group {
            action: GroupCommand::Grant {
                group_id: GroupRef::Id(1),
                node_ids: vec![NodeRef::Id(5)],
            },
        };
        assert!(to_op(&cmd).is_err());
    }
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat`
Expected: FAIL（`GroupCommand::Grant` 未定義のコンパイルエラー）

- [x] **Step 3: CLI・解決・ディスパッチ・実装を追加**

(a) `crates/mat/src/cli.rs` — `GroupCommand` の `Invoke` バリアントの後に追加:

```rust
    /// provision 済みグループの ACL 修復: 各ノードの ACL に Group エントリ
    /// （privilege=Operate, authMode=Group, subjects=[GroupId]）を read-merge-write
    /// で追記する。既にあれば何もしない（冪等）。provision の 4 ステップ目と同じ
    /// 処理を単独実行する（controller 側 groupsettings が非冪等で provision を
    /// 再実行できない既存グループの救済用）。常に直経路（--matd 明示時は exit 2）。
    Grant {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        /// 対象の commission 済み node_id または node alias（1つ以上）。
        #[arg(long = "nodes", required = true, num_args = 1..)]
        node_ids: Vec<NodeRef>,
    },
```

(b) `crates/mat/src/resolve.rs` — `GroupCommand::Invoke` arm の後に追加（match は網羅なのでこれが無いとコンパイルエラー）:

```rust
                GroupCommand::Grant { group_id, node_ids } => GroupCommand::Grant {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    node_ids: node_ids
                        .iter()
                        .map(|n| book.resolve_node(n).map(NodeRef::Id))
                        .collect::<Result<Vec<_>, _>>()?,
                },
```

(c) `crates/mat/src/matd_client.rs` — `to_op` の `Command::Group { action }` match 内、`GroupCommand::Invoke` arm の後に追加:

```rust
            // grant は稀な修復操作で warm session の恩恵が小さく、mat/matd の
            // バージョンスキューにも安全なため直経路のみ（matd に op を足さない）。
            GroupCommand::Grant { .. } => return Err(unsupported("group grant")),
        },
```

(d) `crates/mat/src/main.rs` — `GroupCommand::Invoke` arm の後に追加:

```rust
            GroupCommand::Grant { group_id, node_ids } => {
                let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();
                commands::group::grant(&store_path, group_id.id(), &ids)
            }
```

(e) `crates/mat/src/commands/group.rs` — `invoke` の下に追加:

```rust
/// `mat group grant` — provision 済みグループの ACL 欠落を修復する。各ノードへ
/// ACL の read-merge-write（provision の step 4 と同じ処理）だけを実行する。
/// ノードごとに fail-fast（provision と同じ方針。部分結果は stdout に出さない）。
pub fn grant(store_path: &Path, group_id: u16, node_ids: &[u64]) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    // 全ノードが commission 済みであることを先に確認（1つでも未登録なら exit 11）。
    for &node_id in node_ids {
        store.require_node(node_id)?;
    }
    let chip = ChipTool::new(store.root());

    let mut updated: Vec<u64> = Vec::new();
    let mut unchanged: Vec<u64> = Vec::new();
    for &node_id in node_ids {
        if ensure_group_acl(&chip, node_id, group_id)? {
            updated.push(node_id);
        } else {
            unchanged.push(node_id);
        }
    }

    output::emit(json!({
        "group_id": group_id,
        "nodes": node_ids,
        "updated": updated,
        "unchanged": unchanged,
        "status": "granted",
    }));
    Ok(())
}
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat`
Expected: 全 PASS（grant 統合 5 本 + matd_client 単体 1 本を含む）

- [x] **Step 5: task check + コミット**

Run: `task check`
Expected: PASS

```bash
git add crates/mat/src/cli.rs crates/mat/src/resolve.rs crates/mat/src/matd_client.rs crates/mat/src/main.rs crates/mat/src/commands/group.rs crates/mat/tests/integration.rs
git commit -m "feat(mat): group grant サブコマンド（ACL 修復、直経路のみ）

controller 側 groupsettings が非冪等で provision を再実行できないため、
provision 済みグループの ACL 欠落は grant で修復する。updated / unchanged を
返し冪等。matd プロトコルには op を追加しない（--matd 明示時は exit 2）。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: matd — `group_provision` に ACL ステップ

**Files:**
- Modify: `crates/matd/src/server.rs`
- Test: `crates/matd/tests/integration.rs`

**Interfaces:**
- Consumes: `mat_core::acl::{acl_entries_from_ws_value, merge_group_entry, to_chip_write_json}`（Task 1, 3）、既存 `backend.run_cmdline` / `ensure_ok` / `read_value` / `group_step`
- Produces: matd の `group_provision` op が各ノードで `accesscontrol read acl {node} 0` →（必要時）`accesscontrol write acl {compact_json} {node} 0` を実行する。protocol.rs は変更なし。

- [x] **Step 1: fake-ws を更新し、失敗するテストを書く**

(a) `crates/matd/tests/integration.rs` の `spawn_fake_ws`（25–53 行）の value 分岐に ACL 応答を追加（既存テスト `group_provision_reports_provisioned` が新ステップ込みで通るようにする）:

```rust
                        let value = if line.contains("descriptor read parts-list") {
                            json!([1])
                        } else if line.contains("descriptor read server-list") {
                            json!([6, 8])
                        } else if line.contains("accesscontrol read acl") {
                            // 実機の数値キー形式（admin エントリのみ = ACL 未設定）。
                            json!([{"1":5,"2":2,"3":[112233],"4":null,"254":1}])
                        } else {
                            json!(true)
                        };
```

(b) 同ファイルに記録付き fake を追加（`spawn_fake_ws_discovery_timeout` の下）:

```rust
/// コマンド行を記録する fake ws。`accesscontrol read acl` には `acl_value` を返し、
/// それ以外は `true`。group_provision の ACL ステップ（read → 条件付き write）の
/// コマンド列を検証する。
async fn spawn_fake_ws_recording(
    acl_value: Value,
) -> (u16, Arc<tokio::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lines_log: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&lines_log);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            let acl_value = acl_value.clone();
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(line) = msg {
                        log.lock().await.push(line.clone());
                        let value = if line.contains("accesscontrol read acl") {
                            acl_value.clone()
                        } else {
                            json!(true)
                        };
                        let resp = json!({ "results": [{ "value": value }], "logs": [] });
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    (port, lines_log)
}
```

(c) テスト 3 本を追加（`group_provision_rejects_uncommissioned_node` の下）:

```rust
/// group provision の step 4: ACL read → 既存リスト + group エントリの全置換 write。
#[tokio::test]
async fn group_provision_appends_group_acl_entry() {
    let (port, log) =
        spawn_fake_ws_recording(json!([{"1":5,"2":2,"3":[112233],"4":null,"254":1}])).await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision",
            "group_id":1,
            "node_ids":[1],
            "keyset_id":42,
            "name":"living",
            "endpoint":1,
            "epoch_key":"00112233445566778899aabbccddeeff"
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned", "{}", resps[0]);

    let lines = log.lock().await;
    assert!(
        lines.iter().any(|l| l == "accesscontrol read acl 1 0"),
        "acl read missing: {lines:?}"
    );
    let write = lines
        .iter()
        .find(|l| l.starts_with("accesscontrol write acl "))
        .expect("acl write missing");
    // compact JSON 1 引数 + 宛先。admin エントリ保全 + group 1 の Operate/Group。
    assert!(write.ends_with(" 1 0"), "{write}");
    assert!(write.contains("\"subjects\":[112233]"), "{write}");
    assert!(write.contains("\"authMode\":3"), "{write}");
    assert!(write.contains("\"subjects\":[1]"), "{write}");

    handle.abort();
}

/// 既に Group エントリがある → 冪等: write は送らない。
#[tokio::test]
async fn group_provision_skips_acl_write_when_entry_exists() {
    let (port, log) = spawn_fake_ws_recording(json!([
        {"1":5,"2":2,"3":[112233],"4":null,"254":1},
        {"1":3,"2":3,"3":[1],"4":null,"254":1}
    ]))
    .await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision",
            "group_id":1,
            "node_ids":[1],
            "keyset_id":42,
            "name":"living",
            "endpoint":1,
            "epoch_key":"00112233445566778899aabbccddeeff"
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned", "{}", resps[0]);

    let lines = log.lock().await;
    assert!(lines.iter().any(|l| l == "accesscontrol read acl 1 0"));
    assert!(
        !lines.iter().any(|l| l.contains("accesscontrol write acl")),
        "must not write when the entry already exists: {lines:?}"
    );

    handle.abort();
}

/// ACL read の値が解釈不能 → parse_error で停止し、絶対に write しない。
#[tokio::test]
async fn group_provision_unparseable_acl_stops_with_parse_error() {
    let (port, log) = spawn_fake_ws_recording(json!(true)).await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision",
            "group_id":1,
            "node_ids":[1],
            "keyset_id":42,
            "name":"living",
            "endpoint":1,
            "epoch_key":"00112233445566778899aabbccddeeff"
        })],
    )
    .await;
    assert_eq!(resps[0]["error"]["kind"], "parse_error", "{}", resps[0]);

    let lines = log.lock().await;
    assert!(
        !lines.iter().any(|l| l.contains("accesscontrol write acl")),
        "must never write after an unparseable read: {lines:?}"
    );

    handle.abort();
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd --test integration`
Expected: 新 3 本が FAIL（acl read が呼ばれない）。既存テストは PASS。

- [x] **Step 3: server.rs に step 4 を実装**

`crates/matd/src/server.rs` の import に追加:

```rust
use mat_core::acl::{acl_entries_from_ws_value, merge_group_entry, to_chip_write_json};
```

`group_provision`（server.rs:328 付近）のノードループ内、`groups add-group` の `group_step(...).await?;`（401–405 行）の直後に追加:

```rust
        // 4) ACL: groupcast は authMode=Group で届くため、Group エントリが無いと
        //    デバイスが黙って捨てる。read-merge-write（write は全置換なので
        //    「read できたリスト + 追記」のみ。read 解釈不能なら write しない）。
        let result = backend
            .run_cmdline(&format!("accesscontrol read acl {node_id} 0"))
            .await?;
        ensure_ok(&result)?;
        let value = read_value(&result).ok_or_else(|| {
            MatError::parse_error(format!(
                "no value in chip-tool ws result for acl read on node {node_id}"
            ))
        })?;
        let entries = acl_entries_from_ws_value(&value).map_err(|e| {
            MatError::new(e.kind, format!("acl read on node {node_id}: {}", e.detail))
        })?;
        if let Some(merged) = merge_group_entry(&entries, *group_id) {
            group_step(
                backend,
                &format!(
                    "accesscontrol write acl {} {node_id} 0",
                    to_chip_write_json(&merged)
                ),
            )
            .await?;
        }
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: 全 PASS（既存 `group_provision_reports_provisioned` も fake-ws の ACL 応答追加で通る）

- [x] **Step 5: task check + コミット**

Run: `task check`
Expected: PASS

```bash
git add crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): group_provision に ACL read-merge-write ステップを追加

mat 直経路と同じ step 4 を ws コマンドで実行する（matd 経由の provision だけ
ACL が入らない事故の再発防止）。ws の数値キー ACL 値は mat-core::acl で解釈し、
解釈不能なら write せず parse_error で停止する。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: ドキュメント + バージョン 0.13.0 + 最終確認

**Files:**
- Modify: `Cargo.toml`（ルート、6 行目 `version = "0.12.0"`）
- Modify: `README.md`（Groupcast 節・Groupcast E2E 節）
- Modify: `ARCHITECTURE.md`（Phase 3 / Phase 4 の group 記述）

**Interfaces:**
- Consumes: Task 4–6 で確定した CLI・出力スキーマ
- Produces: なし（ドキュメントのみ）

- [x] **Step 1: workspace バージョンを 0.13.0 に**

ルート `Cargo.toml` の 6 行目:

```toml
version = "0.13.0"
```

（挙動変更: matd の group_provision に ACL ステップが加わったため。spec の「matd 0.10.0」はバージョンが workspace 共有である現状に合わせて 0.13.0 に読み替え。）

- [x] **Step 2: README の Groupcast 節を更新**

(a) bash ブロック（README.md 359 行付近）の provision コメントと直後に grant を追加。既存:

```bash
# Provision: burn the key set + mapping into every node, and set up the
# controller-side group state. --group is the GroupId, --nodes one or more
# commissioned node_ids.
```

を次に変更し、`mat group invoke` の例の後に grant の例を追加:

```bash
# Provision: burn the key set + mapping + ACL group entry into every node, and
# set up the controller-side group state. --group is the GroupId, --nodes one
# or more commissioned node_ids.
# provision --group <ID> --nodes <N>... [--keyset-id N] [--name NAME]
#                                       [--endpoint EP] [--epoch-key HEX]
mat group provision --group 1 --nodes 5 6 7 --name living

# Invoke: one multicast send to the group (unacknowledged).
# invoke --group <ID> --cluster <NAME> --command <NAME> [args...] [--endpoint EP]
mat group invoke --group 1 --cluster onoff --command on

# Grant (repair): run just the ACL step on already-provisioned nodes. Use it for
# groups provisioned before the ACL step existed (or through an old matd).
# Idempotent: nodes that already have the entry are reported as "unchanged".
# grant --group <ID> --nodes <N>...
mat group grant --group 1 --nodes 5 6 7
```

(b) Outputs の json ブロックに grant の行を追加:

```json
// grant — per-node repair result (ACL updated vs already had the entry)
{ "timestamp": "...", "group_id": 1, "nodes": [5, 6, 7], "updated": [5, 7], "unchanged": [6], "status": "granted" }
```

(c) 箇条書き（`- **Groupcast is unacknowledged.**` 等が並ぶ箇所）に 2 項目追加:

```markdown
- **Provision also writes the device ACL (its 4th per-node step).** Group
  commands arrive with authMode=Group, so each device needs an ACL entry
  `{privilege: Operate, authMode: Group, subjects: [GroupId]}` — commissioning
  only creates the CASE admin entry, and without the group entry every device
  **silently drops** the groupcast (it is unacknowledged, so nothing fails
  visibly). The step is a read-merge-write: `mat` reads the current ACL, appends
  the entry only when missing (idempotent, existing entries — including other
  groups' — are preserved), and writes the full list back. If the ACL read
  cannot be parsed, `mat` stops with `parse_error` and **never writes** (an ACL
  write replaces the whole list; a blind write could drop the admin entry and
  make the device unmanageable).
- **`mat group grant` repairs older groups.** Groups provisioned before this
  step existed — including any provision routed through a `matd` ≤ 0.12, which
  does not run the ACL step — lack the entry and their groupcast is silently
  ignored. The controller-side `groupsettings` state is not idempotent, so
  provision cannot simply be re-run; `grant` runs just the ACL step instead.
  It is always direct chip-tool (`--matd` exits 2).
```

- [x] **Step 3: README の Groupcast E2E 節にトラブルシュートを追記**

`### Groupcast E2E (real devices)` の blockquote（`> Groupcast is **unacknowledged** ...`）に続けて追加:

```markdown
> If **no** device reacts although provision reported success, suspect the
> device ACL first: provisions made before the ACL step (or through an old
> `matd` ≤ 0.12) never granted the group permission, and devices silently drop
> unauthorized groupcast. `mat group grant --group 1 --nodes 5 6 7` adds the
> missing entries idempotently.
```

- [x] **Step 4: ARCHITECTURE.md を更新**

(a) Phase 3 節（305 行付近）:

```markdown
- `mat group provision` (KeySetWrite / GroupKeyMap / AddGroup / ACL
  read-merge-write on every node).
- `mat group grant` (repair: just the ACL step, for groups provisioned before
  the ACL step existed; direct chip-tool only).
```

（既存の `- mat group provision (KeySetWrite / GroupKeyMap / AddGroup on every node).` 行を置き換え、grant の行を追加。）

(b) Phase 4 節の group ops 記述（360 行付近）。既存:

```markdown
- **`group`** ops: `group_provision` (controller groupsettings + per-node
  KeySetWrite / GroupKeyMap / AddGroup) and `group_invoke` (multicast, reports
  `sent`). The shared epoch-key / group-node-id logic lives in `mat-core::group`
  so `mat` and `matd` use one copy.
```

を次に変更:

```markdown
- **`group`** ops: `group_provision` (controller groupsettings + per-node
  KeySetWrite / GroupKeyMap / AddGroup / ACL read-merge-write) and
  `group_invoke` (multicast, reports `sent`). The shared epoch-key /
  group-node-id logic lives in `mat-core::group`, and the ACL
  interpretation/merge logic in `mat-core::acl`, so `mat` and `matd` use one
  copy. `group grant` (ACL repair) is deliberately **not** a matd op: it is a
  rare repair operation with little warm-session benefit, and keeping it
  direct-only avoids mat/matd version-skew hazards.
```

(c) Phase 3 の「Heavy pre-provisioning」注記（317 行付近）の `KeySetWrite / GroupKeyMap / AddGroup on every node.` も `KeySetWrite / GroupKeyMap / AddGroup / ACL write on every node.` に更新。

- [x] **Step 5: 受け入れ基準の最終確認**

Run: `task check`
Expected: PASS（fmt:check + clippy -D warnings + 全テスト）

受け入れ基準の突合:
1. `task check` が通る → 本 Step で確認。
2. provision の 4 ステップ目（acl read → 条件付き write）がテストで固定 → `group_provision_runs_acl_read_merge_write_after_add_group`（mat）/ `group_provision_appends_group_acl_entry`（matd）。
3. grant が ACL 欠落を修復（jarvis group 10 相当の fake 再現） → `group_grant_appends_acl_entry_and_reports_updated`。
4. ACL read 解釈不能時に write せず parse_error 停止 → `group_provision_broken_acl_read_is_parse_error_without_write`（mat）/ `group_grant_broken_acl_read_is_parse_error_without_write`（mat）/ `group_provision_unparseable_acl_stops_with_parse_error`（matd）。

- [x] **Step 6: コミット**

```bash
git add Cargo.toml Cargo.lock README.md ARCHITECTURE.md
git commit -m "docs: group provision の ACL ステップと grant を反映、0.13.0

README に provision step 4（ACL）・grant コマンド・「groupcast が届かない時は
ACL を疑う」トラブルシュート・旧 matd（≤0.12）との挙動差の注記を追加。
matd の挙動変更（ACL ステップ追加）に伴い workspace を 0.13.0 に。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Self-Review（実施済み）

- **Spec coverage:** provision 直経路（Task 4）/ matd 経由（Task 6）/ grant 新設・直経路のみ・`--matd` exit 2（Task 5）/ acl.rs のコンポーネント配置と 5 関数（Task 1–3）/ read-merge-write 案 A・read 失敗時 write 禁止（各所）/ fabricIndex は read 値パススルー（merge 実装）/ エラー処理（parse_error / classify_failure / fail-fast）/ 互換性注記と grant 出力スキーマ（Task 5, 7）/ テストマトリクス（TOO パーサ 5 分類・ws 変換・merge・compact round-trip・fake-chip-tool 統合・fake-ws 統合）/ ドキュメント（Task 7）— 全て対応するタスクあり。
- **逸脱:** バージョンは spec の「matd 0.10.0」ではなく workspace 0.13.0（冒頭に明記）。
- **Type consistency:** `ensure_group_acl(chip: &ChipTool, node_id: u64, group_id: u16) -> Result<bool, MatError>` を Task 4 で定義し Task 5 が同シグネチャで使用。`AclEntry` のフィールド名・`to_chip_write_json` の camelCase キー（`authMode`/`fabricIndex`）はテストの assert 文字列と一致。fake の `FabricIndex: 1` に対し統合テストは fabricIndex の値を assert しない（fixture は 1、mat-core 単体は 4 を使用 — 衝突なし）。
- **Placeholder scan:** TBD / 「適切に処理」類なし。全コード步にコードブロックあり。
