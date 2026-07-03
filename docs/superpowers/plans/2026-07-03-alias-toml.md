# alias ファイルを TOML 化（aliases.json → aliases.toml）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** alias 定義ファイルを `<store>/aliases.json` から `<store>/aliases.toml` に全面置き換えする（手編集前提のファイルなので TOML の方が編集しやすい）。

**Architecture:** 変わるのは**ファイル名とパーサだけ**。`AliasBook` の検証ルール（純数字 alias 禁止等）・エラー分類（壊れたファイル = `store_parse` / exit 10、未知 alias = exit 2）・解決ロジック・CLI・exit code・stdout スキーマはすべて不変。aliases.json のフォールバックは**無し**（未リリース機能なので移行考慮不要）。

**Tech Stack:** `toml` crate（serde 対応）を workspace 依存に追加。

## Global Constraints

- 挙動不変: ファイル形式以外の observable な違いを作らない。既存テストはフィクスチャの書き換えのみで全て通ること。
- エラーメッセージ・CLI ヘルプ・コメント内の `aliases.json` 文言は全て `aliases.toml` に更新（`grep -rn "aliases.json" crates/` が 0 件になること）。
- TOML 形式:
  ```toml
  version = 1

  [nodes]
  living-light = 5
  hall-sensor = 12

  [groups]
  all-lights = 258

  [endpoints.living-light]
  main = 1
  night = 2

  [endpoints.12]
  pir = 3
  ```
  （`endpoints` の外側キーは bare key。数字文字列 `12` も bare key として有効）
- 各タスク完了時に `cargo test --workspace` 全パス。最後に `task check`。
- コミットメッセージは既存スタイル（日本語 + prefix、`Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` トレーラー）。

---

### Task 1: コード切替 — mat-core の TOML 化 + 全テストフィクスチャ更新

mat-core のパーサ変更と mat 側テストのフィクスチャは同一コミットでないとテストが落ちるため、1 コミットで行う。

**Files:**
- Modify: `Cargo.toml`（workspace dependencies に `toml` 追加）
- Modify: `crates/mat-core/Cargo.toml`（`toml.workspace = true`）
- Modify: `crates/mat-core/src/alias.rs`（load/save/メッセージ/テスト）
- Modify: `crates/mat/src/resolve.rs`（テストフィクスチャ）
- Modify: `crates/mat/src/cli.rs`（ヘルプ文の aliases.json 表記）
- Modify: `crates/mat/tests/integration.rs`（フィクスチャ + stderr アサーション）
- Modify: そのほか `grep -rn "aliases.json" crates/` に出る全箇所（コメント含む）

**Interfaces:**
- Consumes: 既存の `AliasBook` API（シグネチャ不変）。
- Produces: `ALIASES_FILE = "aliases.toml"`。API 変更なし。

- [ ] **Step 1: 依存追加**

ルート `Cargo.toml` の `[workspace.dependencies]` に追加（バージョンは `cargo add toml -p mat-core --dry-run` 等で最新安定を確認してよいが、`0.9` 系で問題なければそれを使う）:

```toml
toml = "0.9"
```

`crates/mat-core/Cargo.toml` の `[dependencies]` に:

```toml
toml.workspace = true
```

- [ ] **Step 2: alias.rs の切替（TDD: 既存テストのフィクスチャを先に TOML 化して RED を確認）**

`crates/mat-core/src/alias.rs`:

1. 定数:
   ```rust
   /// store 配下の alias 定義ファイル名。
   pub const ALIASES_FILE: &str = "aliases.toml";
   ```
2. `AliasBook::load` のパースを `serde_json::from_str` → `toml::from_str::<AliasFile>` に変更（エラーは従来どおり `store_parse`）。
3. `insert_node_alias` の直列化を `serde_json::to_string_pretty` → `toml::to_string_pretty` に変更（エラー文言の "serialize aliases" はそのまま）。
4. メッセージ文言 3 箇所: `no aliases.json in store` / `defined in aliases.json` / `already exists in aliases.json` の `aliases.json` を `aliases.toml` に。
5. モジュール冒頭 doc コメントの `aliases.json` 表記も更新。
6. テスト: `SAMPLE` を上記 Global Constraints の TOML 形式に書き換え。壊れファイルは `"{ not json"` → `"not = = toml"`。純数字 alias フィクスチャは
   ```toml
   [nodes]
   42 = 5
   ```
   （groups の空文字キーは `"" = 1`、endpoints 内側は `[endpoints.living]` 配下に `1 = 2`）。version 検証は
   ```rust
   let text = std::fs::read_to_string(dir.path().join(ALIASES_FILE)).unwrap();
   let value: toml::Value = text.parse().unwrap();
   assert_eq!(value["version"].as_integer(), Some(1));
   ```
   に変更。`"no aliases.json"` を含む文字列アサーションは `"no aliases.toml"` に。

- [ ] **Step 3: mat 側のフィクスチャ・文言更新**

- `crates/mat/src/resolve.rs`: テストの `store_with` が書くファイル名を `aliases.toml` に、`SAMPLE` を TOML に。モジュールコメントの表記更新。
- `crates/mat/src/cli.rs`: ヘルプ文の「aliases.json の node alias」等を `aliases.toml` に。
- `crates/mat/src/main.rs`: 解決ステップのコメント内表記を更新。
- `crates/mat/tests/integration.rs`: `store_with_node5_and_aliases` / 各テストの `std::fs::write(... "aliases.json" ...)` を `aliases.toml` + TOML 文字列に。壊れファイルは `"not = = toml"`。stderr アサーション `"no aliases.json"` → `"no aliases.toml"`（`"invalid alias name"` / `"store_parse"` / `"already exists"` は不変）。
- 仕上げに `grep -rn "aliases.json" crates/` が 0 件であることを確認。

- [ ] **Step 4: テスト実行**

Run: `cargo test --workspace` → 全 PASS（件数は現状 197 と同数）
Run: `cargo clippy --workspace --all-targets -- -D warnings` → clean
Run: `cargo fmt`

- [ ] **Step 5: コミット**

```bash
git add -A
git commit -m "feat(mat-core): alias 定義ファイルを TOML 化（aliases.json → aliases.toml）"
```

---

### Task 2: ドキュメント同期 + 最終チェック

**Files:**
- Modify: `README.md` / `CLAUDE.md` / `ARCHITECTURE.md` / `docs/superpowers/specs/2026-07-02-alias-resolution-design.md`

- [ ] **Step 1: 表記と JSON サンプルの置換**

- 4 ファイルの `aliases.json` 表記を `aliases.toml` に置換。
- README の Aliases 節の JSON サンプルを Global Constraints の TOML サンプルに差し替え（`[endpoints.12]` の node_id 直指定例を含めること — 最終レビュー follow-up (10) の解消を兼ねる）。README 見出し・アンカー `#aliases-aliasesjson-optional` → 見出し `## Aliases (\`aliases.toml\`, optional)` に伴い参照リンクも新スラッグへ更新。
- spec は「TOML 化（2026-07-03 改訂）」の一文を冒頭ステータス付近に追記した上でファイル名・サンプルを更新（履歴が追えるように）。
- `grep -rn "aliases.json" .`（.git / .superpowers 除く）が 0 件であることを確認。

- [ ] **Step 2: 最終チェック**

Run: `task check` → 全 PASS

- [ ] **Step 3: コミット**

```bash
git add README.md CLAUDE.md ARCHITECTURE.md docs/superpowers/specs/2026-07-02-alias-resolution-design.md
git commit -m "docs: aliases.toml 化を README/ARCHITECTURE/CLAUDE.md/spec に反映"
```
