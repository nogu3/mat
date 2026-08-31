# Small Cleanups (audit ①) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 2026-08-31 コード監査の「小粒クリーンアップ一式」— chip-tool 残骸削除・TLV ヘルパ重複統合・エラーヘルパ共通化・commands ラッパ層解体（約 -900 行、挙動変更なし）。

**Architecture:** 4 つの独立なリファクタタスク。すべて既存テストが挙動を固定しているので、TDD の「新規 failing test」は原則不要 — 各タスクは「削除/移動 → 既存テスト緑 → clippy 緑 → commit」のサイクル。テストごと移動するものはテストも一緒に動かす。

**Tech Stack:** Rust workspace（cargo, clippy, rustfmt）。検証は `task check`（fmt:check + clippy + test、CI 同等）。

**Spec:** この計画自体が仕様（出典: メモリ `mat-code-audit-2026-08-31.md` の「リファクタ候補 ①小粒一式」）。**挙動変更ゼロが受け入れ基準** — stdout JSON スキーマ・エラー文言・exit code は 1 バイトも変えない。

## Global Constraints

- stdout の JSON スキーマ、`{"error":{"kind","detail"}}` の文言・形は変更禁止（CLAUDE.md 出力規約）。
- エラー detail 文字列は既存と完全一致を保つ（テストが文言を pin している可能性があるため、変更したら即テストで発覚する — 発覚したら実装を直す。テスト側を緩めない）。
- 各タスク完了時に `cargo fmt` + 該当クレートの `cargo clippy -- -D warnings` + `cargo test` を通す。最終タスクでワークスペース全体 `task check`。
- コミットメッセージは既存リポジトリの流儀（`refactor(scope): 日本語要約`）に合わせる。
- 削除対象でも **`#[ignore]` 付き実機テスト（`live_*`）はビルドが通ること**を確認する（`cargo test --no-run` でコンパイルされる）。
- main 直コミット禁止。ブランチ `refactor/small-cleanups` 上で作業。

---

### Task 1: mat-core の chip-tool 残骸削除

**Files:**
- Modify: `crates/mat-core/src/acl.rs`
- Modify: `crates/mat-core/src/parse.rs`

**Interfaces:**
- Consumes: なし（先頭タスク）
- Produces: `mat_core::acl` の公開面は `AclEntry` / `AclTarget` / `PRIVILEGE_OPERATE` / `AUTH_MODE_GROUP` / `group_acl_entry` / `merge_group_entry` / `entries_from_im_json` のみになる。外部の唯一の利用者は `crates/mat-native/src/ops.rs:17`（`entries_from_im_json` / `merge_group_entry` / `AclEntry`）— シグネチャ不変。

背景: chip-tool は M8c-3 で撤去済み。acl.rs には chip-tool の TOO ログ/ws 出力を読むためのコードが残っているが、本番から到達不能。ただし **ws 数値キー変換（`ws_entry` / `ws_u8` / `ws_target` / `ws_opt_num` / `reject_unknown_keys`）は `entries_from_im_json` 経由で生きている** — IM 数値キー慣習が同じため委譲している（acl.rs:284-292 のコメント参照）。消すのはログパーサと write JSON 生成だけ。

- [ ] **Step 1: 死んでいることを再確認**

Run: `/usr/bin/grep -rn "parse_acl_from_chip_log\|to_chip_write_json\|acl_entries_from_ws_value\|strip_log_prefix" crates --include="*.rs" | /usr/bin/grep -v "crates/mat-core/src/acl.rs\|crates/mat-core/src/parse.rs"`
Expected: ヒット 0 件（ヒットしたらそのシンボルは削除対象から外し、報告する）。

- [ ] **Step 2: acl.rs から削除**

以下を削除:
- `to_chip_write_json`（94-96 行 + doc コメント 92-93）
- `parse_acl_from_chip_log`（98-268 行、doc コメント込み）
- パーサ内部型・ヘルパ: `EntryBuilder`（389-436）、`TargetBuilder`（438-444）、`Section`（446-451）、`index_line`（453-462）、`field_num`（464-471）、`field_opt_num`（473-479）
- `use crate::parse::strip_log_prefix;`（17 行）

`acl_entries_from_ws_value`（277-282）は削除し、その本体を `entries_from_im_json` に移す（現状は 1 行委譲の向きを逆にするだけ）:

```rust
/// native（IM）直経路の `AccessControlEntryStruct` 列 —— `tlv_to_json` の数値
/// キー規約（`{1: privilege, 2: authMode, 3: subjects, 4: targets, 254:
/// fabricIndex}`）から `AclEntry` 列へ。targets 内は `"0"`=cluster,
/// `"1"`=endpoint, `"2"`=deviceType。解釈不能なら `ParseError`（read できなければ
/// write しない既存方針、モジュール冒頭のコメント参照）。
pub fn entries_from_im_json(v: &Value) -> Result<Vec<AclEntry>, MatError> {
    let arr = v
        .as_array()
        .ok_or_else(|| MatError::parse_error(format!("ACL ws value is not an array: {v}")))?;
    arr.iter().map(ws_entry).collect()
}
```

（エラー文言 `"ACL ws ..."` はそのまま残す — 文言は挙動の一部。）

- [ ] **Step 3: Serialize derive の要否判定**

Run: `/usr/bin/grep -rn "to_string(&\|to_value(\|json!(" crates --include="*.rs" | /usr/bin/grep -i "aclentry\|acl_entry"` および `/usr/bin/grep -rn "serde_json::to_" crates/mat-native/src/ops.rs`
`AclEntry` をシリアライズする本番コードが残っていなければ、`AclEntry` / `AclTarget` から `Serialize` derive と `#[serde(rename_all = "camelCase")]` を外し、両構造体の doc コメントから chip-tool write JSON への言及を削る（`AclEntry` doc 36-38 行、`AclTarget` doc 25-27 行の「chip-tool の read 出力」等を IM read 前提の記述に書き換え）。残っていれば derive は残し、その旨を報告する。

- [ ] **Step 4: テストの整理**

`crates/mat-core/src/acl.rs` の `mod tests`:
- 削除: `write_json_is_compact_named_keys`、`write_json_round_trips_targets`、`too_log_*` 全 12 本（716-913 行付近）
- 呼び先付け替え: `ws_value_numeric_keys_parse` / `ws_value_parses_admin_and_group` / `ws_value_targets_non_null` / `ws_value_bad_shape_is_parse_error` / `ws_value_unknown_entry_field_is_parse_error` / `ws_value_unknown_target_field_is_parse_error` の `acl_entries_from_ws_value(` を `entries_from_im_json(` へ（この 6 本は生きている変換ロジックのテストなので**削除しない**）
- `entries_from_im_json_maps_numeric_keys` はそのまま

- [ ] **Step 5: parse.rs から strip_log_prefix を削除**

`strip_log_prefix`（63-108 行、doc 込み）を削除。モジュール doc の 7 行目
`//! ACL 読み出しのテキストパーサ（parse_acl_from_chip_log）は acl.rs に残っている。`
を削除。`normalize_value` は現役か `/usr/bin/grep -rn "normalize_value" crates --include="*.rs"` で確認 — 本番呼び出しゼロなら削除はせず報告のみ（このタスクのスコープ外）。

- [ ] **Step 6: 検証**

Run: `cargo test -p mat-core && cargo test -p mat-native --no-run && cargo clippy -p mat-core -p mat-native -- -D warnings && cargo fmt`
Expected: 全部緑。

- [ ] **Step 7: Commit**

```bash
git add crates/mat-core/src/acl.rs crates/mat-core/src/parse.rs
git commit -m "refactor(mat-core): chip-tool 撤去後に到達不能な ACL ログ/ws パーサ残骸を削除"
```

---

### Task 2: TLV skip_container / copy_value の重複統合

**Files:**
- Modify: `crates/mat-controller/src/tlv.rs`
- Modify: `crates/mat-controller/src/{case.rs, pase.rs, case_responder.rs, im.rs, commissioning.rs}`
- Modify: `crates/mat-device/src/core/access_control.rs`

**Interfaces:**
- Consumes: なし（Task 1 と独立）
- Produces: `mat_controller::tlv` に新規 `pub fn skip_container(r: &mut Reader<'_>) -> Result<(), TlvError>`。既存 `copy_value` を `pub(crate)` → `pub` に、`copy_container` を private → `pub` に昇格。

**スコープ境界（重要）:** 統合するのは **depth-1 開始**（`*Start` 要素を読み終えた直後から対応する `ContainerEnd` まで）の 6 コピーのみ。`group_settings.rs:96` と `kvs.rs:268` の 2 つは **depth-0 開始**（「今開いているコンテナの残り」を読み飛ばす）で意味論が違い、かつ kvs 版は要素ごとに fabric_index 付きエラー文脈を持つ。**この 2 つは触らない。**

- [ ] **Step 1: tlv.rs に正本を追加**

`crates/mat-controller/src/tlv.rs` の `copy_value` の近くに追加（case.rs:163-177 の実装とdocを移設）:

```rust
/// Skips a container (struct/array/list) whose `StructStart`/`ArrayStart`/
/// `ListStart` element has already been consumed, up to and including its
/// matching `ContainerEnd`.
pub fn skip_container(r: &mut Reader<'_>) -> Result<(), TlvError> {
    let mut depth = 1usize;
    while depth > 0 {
        let el = r.next()?.ok_or(TlvError::Truncated)?;
        match el.value {
            Value::StructStart | Value::ArrayStart | Value::ListStart => depth += 1,
            Value::ContainerEnd => depth -= 1,
            _ => {}
        }
    }
    Ok(())
}
```

同時に `copy_value` を `pub` へ、`copy_container` を `pub` へ昇格（doc コメントに mat-device の responder 側 decoder も使う旨を 1 行追記）。

- [ ] **Step 2: TlvError 系 3 コピーを置換**

`case.rs:166` / `pase.rs:128` / `case_responder.rs:414` のローカル `fn skip_container` を削除し、各ファイルに `use crate::tlv::skip_container;`（既存の `use crate::tlv::{...}` に追加）。pase.rs / case_responder.rs の「Local copy of ...」doc コメントも削除。呼び出し側はシグネチャ同一なので無変更。

- [ ] **Step 3: エラー変換ありの 2 コピーを thin wrapper 化**

`im.rs:331` — ローカル関数を委譲に置換（エラー文言を保つため wrapper は残す）:

```rust
/// Consumes the rest of a container whose `*Start` element has already been
/// read (depth 1), including its matching `ContainerEnd`. Delegates to
/// `tlv::skip_container`, restoring this module's error wording.
fn skip_container(r: &mut Reader) -> Result<(), ImError> {
    crate::tlv::skip_container(r).map_err(|e| match e {
        TlvError::Truncated => ImError::Malformed("truncated container"),
        other => ImError::from(other),
    })
}
```

（`ImError: From<TlvError>` が無ければ既存の `?` がどう変換しているか確認し、同じ変換を使う。）

`commissioning.rs:264` — 同様に委譲 wrapper 化。ただし `step` 文脈を失わないこと: 既存実装は `next_el(r, step)?` でエラーに step を載せている。`next_el` が生成するエラー型を確認し、次の形にする:

```rust
fn skip_container(r: &mut Reader, step: &'static str) -> Result<(), CommissionError> {
    crate::tlv::skip_container(r).map_err(|_| /* next_el が同状況で返すのと同じ CommissionError（step 込み） */)
}
```

`next_el` の実装（commissioning.rs 内）を読んで map_err の中身を合わせる。TlvError の細分（Truncated か InvalidType か）が既存エラーで区別されているなら wrapper 化を諦めてこのコピーは現状維持し、報告する。

- [ ] **Step 4: mat-device 側を委譲**

`crates/mat-device/src/core/access_control.rs`:
- `fn skip_container`（403-415）を削除し、Option チェーンで使えるよう置換。呼び出し箇所（同ファイル内を grep）を `skip_container(r)?` → `mat_controller::tlv::skip_container(r).ok()?` に。呼び出しが 2 箇所以上なら 1 行 wrapper を残してよい:

```rust
fn skip_container(r: &mut Reader) -> Option<()> {
    mat_controller::tlv::skip_container(r).ok()
}
```

- `fn copy_element`（421-447）と `fn copy_container`（448-458）を削除し、呼び出し側を `mat_controller::tlv::copy_value(w, r, tag, value).ok()?` に置換（copy_element の呼び出し箇所を grep して全部）。「mat-device 内版」doc コメントも削除。

- [ ] **Step 5: 検証**

Run: `cargo test -p mat-controller -p mat-device && cargo clippy -p mat-controller -p mat-device -- -D warnings && cargo fmt`
Expected: 全部緑（`live_*` の `#[ignore]` テストもコンパイルは通ること）。

- [ ] **Step 6: Commit**

```bash
git add crates/mat-controller/src crates/mat-device/src
git commit -m "refactor(tlv): skip_container 6 コピーと copy_value の mat-device 複製を tlv.rs 正本へ統合"
```

---

### Task 3: mat/matd 間エラーヘルパの mat-core 共通化

**Files:**
- Modify: `crates/mat-core/src/error.rs`
- Modify: `crates/mat/src/native_direct.rs`
- Modify: `crates/mat/src/matd_client.rs`
- Modify: `crates/matd/src/server.rs`

**Interfaces:**
- Consumes: なし（Task 1/2 と独立）
- Produces: `MatError` に 4 つの関連関数を追加 —
  `pub fn unresolved_op() -> Self` / `pub fn group_unavailable(reason: &str) -> Self` / `pub fn group_ctx_unconfigured() -> Self` / `pub fn to_json(&self) -> serde_json::Value`

- [ ] **Step 1: mat-core に共通ヘルパを追加**

`crates/mat-core/src/error.rs` の `impl MatError` に追加:

```rust
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
```

`emit()` の本体を `eprintln!("{}", self.to_json());` に置換。

- [ ] **Step 2: 文言一致を pin する failing しないテストを追加**

`error.rs` の `mod tests` に（wire 互換の回帰防止 — 文言変更はここで落ちる）:

```rust
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
```

Run: `cargo test -p mat-core shared_helper` — PASS（既存文言と一致しているはず。落ちたら実装をコピー元と突き合わせる）。

- [ ] **Step 3: mat 側を委譲に置換**

`crates/mat/src/native_direct.rs`:
- `unresolved_op_error(spec)`（色 spec 検証つき、~640-664 行）は**関数ごと残す**が、最後の `MatError::parse_error("unknown cluster/...")` を `MatError::unresolved_op()` に置換。
- `group_ctx_unconfigured_error()`（682-687）と `group_unavailable_error(reason)`（689-693）を削除し、呼び出し箇所（grep）を `MatError::group_ctx_unconfigured()` / `MatError::group_unavailable(reason)` へ。

`crates/mat/src/matd_client.rs`:
- `fn emit_error(kind, detail)`（577-581）を削除し、呼び出し箇所を `MatError::new(kind, detail).emit()` へ（`use mat_core::error::MatError;` が無ければ追加）。

- [ ] **Step 4: matd 側を委譲に置換**

`crates/matd/src/server.rs`:
- `unresolved_op_error()`（1236-1241）/ `group_unavailable_error()`（1246-1248）/ `group_ctx_unconfigured_error()`（1253-1258）を削除し、呼び出し箇所を `MatError::unresolved_op()` 等へ。
- `error_response`（1296-1305）を `to_json` ベースへ:

```rust
/// エラー応答 `{"error":{"kind","detail"}, "id"?, "timestamp"}`。
fn error_response(id: Option<Value>, e: &MatError) -> Value {
    let mut body = e.to_json();
    if let Value::Object(map) = &mut body {
        map.insert("timestamp".into(), json!(now_iso8601()));
        if let Some(id) = id {
            map.insert("id".into(), id);
        }
    }
    body
}
```

- [ ] **Step 5: 検証**

Run: `cargo test -p mat-core -p mat -p matd && cargo clippy -p mat-core -p mat -p matd -- -D warnings && cargo fmt`
Expected: 全部緑。matd の応答形（id/timestamp の有無）をテストが pin しているはずなので、そこが緑なら wire 互換は保たれている。

- [ ] **Step 6: Commit**

```bash
git add crates/mat-core/src/error.rs crates/mat/src crates/matd/src/server.rs
git commit -m "refactor(error): mat/matd 逐語コピーのエラーヘルパ3種+JSONボディ組立を mat-core へ一本化"
```

---

### Task 4: commands/ 空洞ラッパ層の解体

**Files:**
- Create: `crates/mat/src/units.rs`
- Delete: `crates/mat/src/commands/{read.rs, write.rs, describe.rs, invoke.rs}`
- Modify: `crates/mat/src/commands/mod.rs`, `crates/mat/src/commands/group.rs`, `crates/mat/src/native_direct.rs`, `crates/mat/src/matd_client.rs`, `crates/mat/src/main.rs`（mod 宣言がある場所を確認）

**Interfaces:**
- Consumes: なし（Task 1-3 と独立。ただし native_direct.rs は Task 3 も触るので、実行順は Task 3 → Task 4 を推奨 — コンフリクト回避のみが理由）
- Produces: `crate::units::resolve_color_temp(kelvin: Option<u32>, mireds: Option<u16>) -> (u16, u32)` と `crate::units::resolve_level(percent: u8) -> u8`（`pub(crate)`）。emit 系は `mat_core::{body, output}` 直呼びになる。

背景: `mat_core::body` が JSON 形の単一ソースになった結果、`commands/read.rs`（20行）/ `write.rs`（21行）/ `describe.rs`（13行）は `output::emit(body::xxx(...))` 1 本だけの転送層。`invoke.rs` は転送 4 本 + 実ロジック 2 本（`resolve_color_temp` / `resolve_level` — `native_direct` と `matd_client` の両方が使う経路非依存の入力換算）。転送は呼び出し元でインライン化し、実ロジックは `units.rs` へ移す。**`group.rs`（provision note の決定ロジックあり）と `open_window.rs` / `commission.rs` / `diag.rs` / `discover.rs` / `fabric.rs` は触らない。**

- [ ] **Step 1: units.rs を作る**

`crates/mat/src/commands/invoke.rs` から `resolve_color_temp`（55-67 行）と `resolve_level`（73-75 行）を doc コメントごと `crates/mat/src/units.rs` へ移動。`resolve_level` は `pub(crate)`、`resolve_color_temp` も `pub(crate)` に統一。モジュール doc:

```rust
//! 経路非依存の入力換算（CLI 入力 → Matter 生値）。native 直経路
//! （`native_direct`）と matd 経路（`matd_client::to_op`）の両方が使う。
```

`mod tests`（kelvin_2700_converts_to_370_mireds / kelvin_6500_rounds_to_154_mireds / mireds_direct_computes_kelvin_echo / resolve_level_rounds_percent_to_254_scale の 4 本）もそのまま移動。`main.rs`（または `lib.rs` — mod 宣言の場所を grep で確認）に `mod units;` を追加。

- [ ] **Step 2: 転送 emit をインライン化**

`crates/mat/src/native_direct.rs` で以下を置換（`use mat_core::{body, output};` を追加）:
- `crate::commands::invoke::emit_invoke_success(a, b, c, d)` → `output::emit(body::invoke_success(a, b, c, d))`（838, 864, 1323 行）
- `crate::commands::invoke::emit_color_success(n, e, color, t)` → `output::emit(body::color_success(n, e, color, t))`（923 行）
- `crate::commands::invoke::emit_color_temp_success(n, e, k, m, t)` → `output::emit(body::color_temp_success(n, e, k, m, t))`（957 行）
- `crate::commands::invoke::emit_level_success(n, e, percent, level, t)` → `output::emit(body::level_success(n, e, body::LevelEcho { percent, level }, t))`（993 行）
- `crate::commands::read::emit_read_success(...)` → `output::emit(body::read_success(...))`（883, 1253 行 — `body::read_success` のシグネチャは `crates/mat-core/src/body.rs` で確認して合わせる）
- `crate::commands::write::emit_write_success(...)` → `output::emit(body::write_success(...))`（1291 行）
- `crate::commands::describe::emit_describe_success(...)` → `output::emit(body::describe_success(...))`（1337 行）
- `crate::commands::invoke::resolve_color_temp` → `crate::units::resolve_color_temp`（230, 316 行）、`resolve_level` 同様（245, 334 行）
- 2513 行付近のコメント `emit_read_success 等が println! 直書き…` を `output::emit（println! 直書き）…` 等、実体に合わせて書き換え。

`crates/mat/src/matd_client.rs` で `crate::commands::invoke::resolve_color_temp` / `resolve_level` → `crate::units::`（264, 278, 357, 371 行）。

- [ ] **Step 3: ファイル削除と mod 整理**

```bash
git rm crates/mat/src/commands/read.rs crates/mat/src/commands/write.rs crates/mat/src/commands/describe.rs crates/mat/src/commands/invoke.rs
```

`crates/mat/src/commands/mod.rs`: `pub mod describe; pub mod invoke; pub mod read; pub mod write;` の 4 行を削除し、モジュール doc の 1 行目を chip-tool 言及なしに書き換え:

```rust
//! サブコマンド実装。各 `run` は副作用（ストア更新・stdout 出力）を行い、
//! 成功なら `Ok(())`、失敗なら [`mat_core::error::MatError`] を返す。
```

`crates/mat/src/commands/group.rs`: `emit_color_temp_sent`（67 行）と `emit_level_sent`（82 行）の `#[allow(clippy::too_many_arguments)]` は引数 6 個で発火しない死んだ allow — 削除。

- [ ] **Step 4: 検証**

Run: `cargo test -p mat && cargo clippy -p mat -- -D warnings && cargo fmt`
Expected: 全部緑。特に native_direct のスナップショット/出力系テストが緑なら stdout スキーマ不変。

- [ ] **Step 5: Commit**

```bash
git add -A crates/mat/src
git commit -m "refactor(mat): 空洞化した commands 転送層を解体、入力換算を units.rs へ"
```

---

### Task 5: 仕上げ検証

**Files:** なし（検証のみ。修正が出たら該当タスクのファイル）

**Interfaces:**
- Consumes: Task 1-4 の全変更
- Produces: `task check` 緑のブランチ

- [ ] **Step 1: ワークスペース全体の CI 同等チェック**

Run: `task check`
Expected: fmt:check + clippy + test 全部緑。

- [ ] **Step 2: 削除漏れ・参照漏れの最終 grep**

Run: `/usr/bin/grep -rn "parse_acl_from_chip_log\|to_chip_write_json\|acl_entries_from_ws_value\|strip_log_prefix\|emit_read_success\|emit_write_success\|emit_describe_success\|emit_invoke_success\|commands::invoke::resolve" crates docs/commands.md docs/backend.md README.md 2>/dev/null`
Expected: 0 件（docs にヒットしたら該当ドキュメントの記述を現状に合わせて修正しコミット）。

- [ ] **Step 3: 行数削減の実測を報告**

Run: `git diff --stat main...HEAD | tail -1`
削減行数を最終報告に含める（目安 -900 行。大きく外れても失敗ではない — 実測を報告する）。

- [ ] **Step 4: マージ前の実機スモーク（ユーザールール）**

ユーザーの運用ルール「main マージ前に jarvis 実機 E2E 必須」に従い、マージはこの計画のスコープ外。ブランチを push せず、実機スモーク（`mat read` 1 発程度）の実施可否をユーザーに確認するところで止める。

## Self-Review 済み

- スコープ外を明記: group_settings/kvs の depth-0 skip、group.rs/open_window.rs の emit、normalize_value の生死判定は報告のみ。
- 型整合: `resolve_color_temp` の `(u16, u32)` 戻り、`skip_container` の `Result<(), TlvError>`、`MatError` ヘルパ 4 種のシグネチャは Task 間で一致。
- 挙動不変の担保: エラー文言 pin テスト（Task 3 Step 2）、既存テスト網、`task check`。
