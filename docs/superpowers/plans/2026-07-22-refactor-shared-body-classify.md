# リファクタ: 成功 JSON 共有 builder 化 + classify 解決済み ID 一貫化 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 成功 JSON の組み立てを `mat_core::body` に一本化し、classify が解決済み cluster ID を返すようにして、出力・挙動を一切変えずに sibling 重複と panic-on-drift を排除する。

**Architecture:** (1) timestamp 抜きの成功 body を返す純関数群 `mat_core::body` を新設し、mat 直経路の `emit_*` と matd の `*_body` が同じ関数を呼ぶ。(2) `mat_core::ids::classify_write` / `classify_invoke` の `Native` variant に解決済み `cluster: u32` を含め、呼び手の `resolve_cluster(...).expect(...)` 再解決を全廃。(3) `native_direct::run_op`(~530行)を 1 op = 1 async fn に分割。

**Tech Stack:** Rust workspace(crates: mat-core / mat / matd / mat-native / mat-controller)、serde_json、Taskfile(`task check`)。

**Spec:** `docs/superpowers/specs/2026-07-22-refactor-shared-body-classify-design.md`

## Global Constraints

- **出力・挙動は完全不変。** JSON スキーマ(キー・値・optional キーの出現条件)、エラー kind、exit code、tracing ログを一切変えない。
- 既存テストは**期待値を変更しない**(コンパイルを通すためのパターンマッチ追記のみ可 — Task 4 の `Native { cluster, .. }` 追加)。
- 各 Task の最後に `task check`(fmt:check + clippy -D warnings + test)が通ること。
- コミットは当該 Task で編集したファイルのみ `git add` する。
- コミットメッセージ末尾に以下を付ける:
  ```
  Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01D1nMZo3ak5uZh4ibnmTrpR
  ```
- コード内コメントは既存流儀(日本語、「なぜ」を書く)に合わせる。
- stdout 純 JSON / stderr tracing の設計規則(CLAUDE.md)はそのまま。

---

### Task 1: `mat_core::body` モジュール新設(builder + 形状固定テスト)

**Files:**
- Create: `crates/mat-core/src/body.rs`
- Modify: `crates/mat-core/src/lib.rs`(`pub mod body;` を追加)

**Interfaces:**
- Consumes: `crate::color::ResolvedColor`(pub fields: `hue_raw: u8, sat_raw: u8, hue: u16, sat: u8, name: Option<String>, rgb: Option<String>`)、`crate::parse::normalize_value(&str) -> serde_json::Value`
- Produces(後続 Task 2/3 が呼ぶ。全て `pub fn ... -> serde_json::Value`、timestamp 抜き):
  - `read_success(node_id: u64, endpoint: u16, cluster: &str, attribute: &str, value: Value)`
  - `write_success(node_id: u64, endpoint: u16, cluster: &str, attribute: &str, value_in: &str)`(normalize_value を内包)
  - `invoke_success(node_id: u64, endpoint: u16, cluster: &str, command: &str)`
  - `color_temp_success(node_id: u64, endpoint: u16, kelvin: u32, mireds: u16, transition: u16)`
  - `level_success(node_id: u64, endpoint: u16, percent: u8, level: u8, transition: u16)`
  - `color_success(node_id: u64, endpoint: u16, color: &ResolvedColor, transition: u16)`
  - `describe_success(node_id: u64, endpoints: &[(u16, Vec<u64>)])`
  - `group_invoke_sent(group_id: u16, cluster: &str, command: &str, endpoint: u16)`
  - `group_color_temp_sent(group_id: u16, kelvin: u32, mireds: u16, transition: u16, endpoint: u16)`
  - `group_level_sent(group_id: u16, percent: u8, level: u8, transition: u16, endpoint: u16)`
  - `group_color_sent(group_id: u16, color: &ResolvedColor, transition: u16, endpoint: u16)`
  - `group_provision_success(group_id: u16, keyset_id: u16, name: &str, endpoint: u16, nodes: &[u64], note: Option<&str>)`

- [ ] **Step 1: 形状固定テストを先に書く**

`crates/mat-core/src/body.rs` を新規作成し、まずファイル末尾に置くテストを書く(実装はまだ空でよいがコンパイルのため関数スタブごと書くなら Step 3 と併合してよい。TDD の意図は「形状の期待値を先に確定させる」こと)。テストは現行出力からの転記であり、**期待 JSON は下記の値を一字も変えない**:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn color_fixture() -> ResolvedColor {
        ResolvedColor {
            hue_raw: 169,
            sat_raw: 254,
            hue: 240,
            sat: 100,
            name: Some("blue".into()),
            rgb: None,
        }
    }

    #[test]
    fn read_success_shape() {
        assert_eq!(
            read_success(5, 1, "onoff", "on-off", json!(true)),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "onoff",
                "attribute": "on-off", "value": true,
            })
        );
    }

    #[test]
    fn write_success_normalizes_value_like_read() {
        // "100" → 100(read と型を揃える normalize_value 内包)。
        assert_eq!(
            write_success(5, 1, "levelcontrol", "on-level", "100"),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "levelcontrol",
                "attribute": "on-level", "value": 100, "status": "success",
            })
        );
    }

    #[test]
    fn invoke_success_shape() {
        assert_eq!(
            invoke_success(5, 1, "onoff", "on"),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "onoff",
                "command": "on", "status": "success",
            })
        );
    }

    #[test]
    fn color_temp_success_shape() {
        assert_eq!(
            color_temp_success(5, 1, 2700, 370, 0),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "colorcontrol",
                "command": "move-to-color-temperature",
                "kelvin": 2700, "mireds": 370, "transition": 0,
                "status": "success",
            })
        );
    }

    #[test]
    fn level_success_shape() {
        assert_eq!(
            level_success(5, 1, 50, 127, 0),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "levelcontrol",
                "command": "move-to-level",
                "percent": 50, "level": 127, "transition": 0,
                "status": "success",
            })
        );
    }

    #[test]
    fn color_success_includes_optional_name_and_omits_absent_rgb() {
        assert_eq!(
            color_success(5, 1, &color_fixture(), 0),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 240, "saturation": 100,
                "hue_raw": 169, "saturation_raw": 254,
                "transition": 0, "status": "success",
                "name": "blue",
            })
        );
    }

    #[test]
    fn describe_success_shape() {
        assert_eq!(
            describe_success(5, &[(1, vec![6, 8])]),
            json!({
                "node_id": 5,
                "endpoints": [{ "endpoint": 1, "clusters": [6, 8] }],
            })
        );
    }

    #[test]
    fn group_invoke_sent_shape() {
        assert_eq!(
            group_invoke_sent(10, "onoff", "on", 1),
            json!({
                "group_id": 10, "cluster": "onoff", "command": "on",
                "endpoint": 1, "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_color_temp_sent_shape() {
        assert_eq!(
            group_color_temp_sent(10, 2700, 370, 0, 1),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-color-temperature",
                "kelvin": 2700, "mireds": 370, "transition": 0,
                "endpoint": 1, "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_level_sent_shape() {
        assert_eq!(
            group_level_sent(10, 50, 127, 0, 1),
            json!({
                "group_id": 10, "cluster": "levelcontrol",
                "command": "move-to-level",
                "percent": 50, "level": 127, "transition": 0,
                "endpoint": 1, "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            })
        );
    }

    #[test]
    fn group_color_sent_shape() {
        assert_eq!(
            group_color_sent(10, &color_fixture(), 0, 1),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 240, "saturation": 100,
                "hue_raw": 169, "saturation_raw": 254,
                "transition": 0, "endpoint": 1, "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
                "name": "blue",
            })
        );
    }

    #[test]
    fn group_provision_success_with_and_without_note() {
        assert_eq!(
            group_provision_success(10, 60, "living", 1, &[5, 6], None),
            json!({
                "group_id": 10, "keyset_id": 60, "name": "living",
                "endpoint": 1, "nodes": [5, 6], "status": "provisioned",
            })
        );
        let with_note = group_provision_success(10, 60, "living", 1, &[5], Some("x"));
        assert_eq!(with_note["note"], json!("x"));
    }
}
```

- [ ] **Step 2: テストが失敗する(コンパイルエラー)ことを確認**

Run: `cargo test -p mat-core body`
Expected: FAIL(`body` モジュール未定義 / 関数未定義のコンパイルエラー)

- [ ] **Step 3: builder を実装**

`crates/mat-core/src/body.rs` の実装部(テストの上に置く):

```rust
//! 成功 JSON body の単一ソース。
//!
//! `mat` 直経路(`commands/*` の `emit_*`)と `matd`(`server.rs`)の両方が
//! ここを呼ぶことで、同一 op の成功出力が経路によらず同形であることを
//! **構造的に**保証する(0.23.1 で踏んだ「sibling 関数への修正適用漏れ」
//! クラスの再発防止)。timestamp は含めない — 直経路は `output::emit`、
//! matd は応答 envelope が付与する。
//!
//! 対象は両経路に存在する op のみ。直経路専用 op(open-window / diag /
//! grant / discover / commission 等)の emit は重複が無いため `mat` 側に残す。

use serde_json::{json, Value};

use crate::color::ResolvedColor;
use crate::parse::normalize_value;

/// groupcast は unacknowledged — "sent" 系 body 共通の注記。
const GROUPCAST_NOTE: &str = "unacknowledged groupcast; per-device delivery not confirmed";

/// `read` の成功 body。
pub fn read_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value: Value,
) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "attribute": attribute,
        "value": value,
    })
}

/// `write` の成功 body。`value_in` は CLI/プロトコル入力の生文字列 —— read と
/// 型を揃えるため normalize_value で型推定してから載せる(両経路共通の規則)。
pub fn write_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value_in: &str,
) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "attribute": attribute,
        "value": normalize_value(value_in),
        "status": "success",
    })
}

/// `invoke` / `on` / `off` の成功 body。
pub fn invoke_success(node_id: u64, endpoint: u16, cluster: &str, command: &str) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "command": command,
        "status": "success",
    })
}

/// `color-temp` の成功 body。入力 kelvin と換算後 mireds を両方エコー
/// (`color-temperature-mireds` の読み返しと突合しやすくする)。
pub fn color_temp_success(
    node_id: u64,
    endpoint: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "colorcontrol",
        "command": "move-to-color-temperature",
        "kelvin": kelvin,
        "mireds": mireds,
        "transition": transition,
        "status": "success",
    })
}

/// `level` の成功 body。入力 percent と換算後 level を両方エコー。
pub fn level_success(
    node_id: u64,
    endpoint: u16,
    percent: u8,
    level: u8,
    transition: u16,
) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": percent,
        "level": level,
        "transition": transition,
        "status": "success",
    })
}

/// `color` の成功 body。入力(name / rgb / 度・%)と換算後 0–254 生値を両方
/// エコー。name / rgb は指定時のみキーが現れる(省略時キー無し — 既存形)。
pub fn color_success(
    node_id: u64,
    endpoint: u16,
    color: &ResolvedColor,
    transition: u16,
) -> Value {
    let mut body = json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "colorcontrol",
        "command": "move-to-hue-and-saturation",
        "hue": color.hue,
        "saturation": color.sat,
        "hue_raw": color.hue_raw,
        "saturation_raw": color.sat_raw,
        "transition": transition,
        "status": "success",
    });
    if let Some(name) = &color.name {
        body["name"] = json!(name);
    }
    if let Some(rgb) = &color.rgb {
        body["rgb"] = json!(rgb);
    }
    body
}

/// `describe` の成功 body。クラスタは数値 ID の配列(名前解決は `mat` の責務外)。
pub fn describe_success(node_id: u64, endpoints: &[(u16, Vec<u64>)]) -> Value {
    let out_endpoints: Vec<Value> = endpoints
        .iter()
        .map(|(ep, clusters)| json!({ "endpoint": ep, "clusters": clusters }))
        .collect();
    json!({
        "node_id": node_id,
        "endpoints": out_endpoints,
    })
}

/// `group invoke` の sent body。
pub fn group_invoke_sent(group_id: u16, cluster: &str, command: &str, endpoint: u16) -> Value {
    json!({
        "group_id": group_id,
        "cluster": cluster,
        "command": command,
        "endpoint": endpoint,
        "status": "sent",
        "note": GROUPCAST_NOTE,
    })
}

/// `group color-temp` の sent body。
pub fn group_color_temp_sent(
    group_id: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
    endpoint: u16,
) -> Value {
    json!({
        "group_id": group_id,
        "cluster": "colorcontrol",
        "command": "move-to-color-temperature",
        "kelvin": kelvin,
        "mireds": mireds,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": GROUPCAST_NOTE,
    })
}

/// `group level` の sent body。
pub fn group_level_sent(
    group_id: u16,
    percent: u8,
    level: u8,
    transition: u16,
    endpoint: u16,
) -> Value {
    json!({
        "group_id": group_id,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": percent,
        "level": level,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": GROUPCAST_NOTE,
    })
}

/// `group color` の sent body。name / rgb は指定時のみキーが現れる。
pub fn group_color_sent(
    group_id: u16,
    color: &ResolvedColor,
    transition: u16,
    endpoint: u16,
) -> Value {
    let mut body = json!({
        "group_id": group_id,
        "cluster": "colorcontrol",
        "command": "move-to-hue-and-saturation",
        "hue": color.hue,
        "saturation": color.sat,
        "hue_raw": color.hue_raw,
        "saturation_raw": color.sat_raw,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": GROUPCAST_NOTE,
    });
    if let Some(name) = &color.name {
        body["name"] = json!(name);
    }
    if let Some(rgb) = &color.rgb {
        body["rgb"] = json!(rgb);
    }
    body
}

/// `group provision` の成功 body。`note` は経路差のある案内文(直経路 native は
/// KVS 直書き+matd 再起動案内、matd 経路は無し)— 文言の決定は呼び出し側の責務。
pub fn group_provision_success(
    group_id: u16,
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    nodes: &[u64],
    note: Option<&str>,
) -> Value {
    let mut body = json!({
        "group_id": group_id,
        "keyset_id": keyset_id,
        "name": name,
        "endpoint": endpoint,
        "nodes": nodes,
        "status": "provisioned",
    });
    if let Some(note) = note {
        body["note"] = json!(note);
    }
    body
}
```

`crates/mat-core/src/lib.rs` の既存 `pub mod` 群に `pub mod body;` を追加(アルファベット順の位置)。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core body`
Expected: PASS(12 テスト)

- [ ] **Step 5: `task check` → コミット**

Run: `task check`
Expected: PASS

```bash
git add crates/mat-core/src/body.rs crates/mat-core/src/lib.rs
git commit -m "refactor(mat-core): 成功JSON bodyの単一ソース mat_core::body を新設"
```

---

### Task 2: mat 直経路の `emit_*` を `body::*` 委譲に置換

**Files:**
- Modify: `crates/mat/src/commands/read.rs`
- Modify: `crates/mat/src/commands/write.rs`
- Modify: `crates/mat/src/commands/invoke.rs`(emit 4 本のみ。`resolve_color_temp` / `resolve_level` とそのテストは触らない)
- Modify: `crates/mat/src/commands/describe.rs`
- Modify: `crates/mat/src/commands/group.rs`(`emit_grant_success` は直経路専用なので触らない)

**Interfaces:**
- Consumes: Task 1 の `mat_core::body::*`(シグネチャは Task 1 の Produces を参照)
- Produces: 各 `emit_*` の**シグネチャ・呼び出し面は不変**(呼び出し側 `native_direct.rs` の変更なし)

- [ ] **Step 1: 各 emit_* の中身を委譲に置換**

パターン(全ファイル共通): `json!({...})` の組み立てを `mat_core::body::…` 呼び出しに置き換え、`output::emit` に渡す。doc コメントの「単一ソース」記述は「単一ソースは `mat_core::body`」へ更新する。例(`read.rs` 全体):

```rust
//! `mat read` — 属性を読む。
//!
//! バックエンド実行は native 直経路(`native_direct`)が担う(M8c-3 で chip-tool
//! 経路は撤去)。成功 JSON の形は `mat_core::body`(直経路・matd 共有の単一
//! ソース)、このモジュールは stdout への emit のみを持つ。

use mat_core::{body, output};

/// `read` の成功 JSON を stdout へ emit する(body は `mat_core::body` 共有)。
pub(crate) fn emit_read_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value: serde_json::Value,
) {
    output::emit(body::read_success(node_id, endpoint, cluster, attribute, value));
}
```

各ファイルの置換対応(引数はそのまま横流し):

| emit 関数 | 委譲先 |
|---|---|
| `read.rs::emit_read_success` | `body::read_success` |
| `write.rs::emit_write_success` | `body::write_success`(normalize_value は body 内包になるので `use mat_core::parse::normalize_value` を削除) |
| `invoke.rs::emit_invoke_success` | `body::invoke_success` |
| `invoke.rs::emit_color_temp_success` | `body::color_temp_success` |
| `invoke.rs::emit_level_success` | `body::level_success` |
| `invoke.rs::emit_color_success` | `body::color_success` |
| `describe.rs::emit_describe_success` | `body::describe_success` |
| `group.rs::emit_invoke_sent` | `body::group_invoke_sent` |
| `group.rs::emit_color_temp_sent` | `body::group_color_temp_sent` |
| `group.rs::emit_level_sent` | `body::group_level_sent` |
| `group.rs::emit_color_sent` | `body::group_color_sent` |
| `group.rs::emit_provision_success` | `body::group_provision_success`(下記) |

`emit_provision_success` は note の分岐ロジックを残して文字列を `Option<&str>` に落とす:

```rust
pub(crate) fn emit_provision_success(
    group_id: u16,
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    node_ids: &[u64],
    rebind: bool,
    native_kvs: bool,
) {
    // note は経路依存(matd 経路の provision は note 無し)なのでここで決める。
    let note = if native_kvs {
        // native は rebind の有無によらず KVS を直接書くので常にこの note。
        Some(
            "controller group state written natively to kvs; if matd is running, restart it to reload group state",
        )
    } else if rebind {
        // 直経路の rebind は matd の warm セッションが旧 group 状態をメモリに
        // 持ったままになるため、稼働中なら再起動が要る(storage は更新済み)。
        Some("rebound keyset binding; if matd is running, restart it to reload group state")
    } else {
        None
    };
    output::emit(body::group_provision_success(
        group_id, keyset_id, name, endpoint, node_ids, note,
    ));
}
```

- [ ] **Step 2: `task check` で出力不変を確認**

Run: `task check`
Expected: PASS(mat のバイナリ統合テストが既存期待値のまま通る = 直経路出力不変の証明)

- [ ] **Step 3: コミット**

```bash
git add crates/mat/src/commands/read.rs crates/mat/src/commands/write.rs \
  crates/mat/src/commands/invoke.rs crates/mat/src/commands/describe.rs \
  crates/mat/src/commands/group.rs
git commit -m "refactor(mat): emit_* を mat_core::body 委譲に置換(出力不変)"
```

---

### Task 3: matd の `*_body` を `body::*` 委譲に置換し `let … else unreachable!` を排除

**Files:**
- Modify: `crates/matd/src/server.rs`

**Interfaces:**
- Consumes: Task 1 の `mat_core::body::*`、`mat_core::color::ResolvedColor`
- Produces: `native_group_params` の戻り値型が `(u16, u32, u32, Option<Vec<u8>>)` から **成功 body 同梱の 5 要素** `(u16, u32, u32, Option<Vec<u8>>, Value)` に変わる(同ファイル内でのみ使用)。`write_success_body` / `invoke_success_body` / `describe_success_body` / `hotpath_success_body` / `group_sent_body` は**削除**される。

- [ ] **Step 1: 現行テストが通ることを確認(ベースライン)**

Run: `cargo test -p matd`
Expected: PASS

- [ ] **Step 2: `native_op` の各アームで body を直接組む**

`native_op`(`server.rs:546` 付近)の各アームは変種が確定しているので、`hotpath_success_body(op, ...)` / `write_success_body(op)` / `invoke_success_body(op)` 呼び出しをアーム内での `mat_core::body::*` 直接呼び出しに置き換える:

- `Op::On` アーム: `Ok(mat_core::body::invoke_success(*node_id, *endpoint, "onoff", "on"))`
- `Op::Off` アーム: `Ok(mat_core::body::invoke_success(*node_id, *endpoint, "onoff", "off"))`
- `Op::ColorTemp` アーム: `Ok(mat_core::body::color_temp_success(*node_id, *endpoint, *kelvin, *mireds, *transition))`
- `Op::Level` アーム: `Ok(mat_core::body::level_success(*node_id, *endpoint, *percent, *level, *transition))`
- `Op::Color` アーム: アームで `ResolvedColor` を組んで渡す:
  ```rust
  let color = mat_core::color::ResolvedColor {
      hue_raw: *hue_raw,
      sat_raw: *saturation_raw,
      hue: *hue,
      sat: *saturation,
      name: name.clone(),
      rgb: rgb.clone(),
  };
  Ok(mat_core::body::color_success(*node_id, *endpoint, &color, *transition))
  ```
- `Op::Read`(onoff 専用アーム): `Ok(mat_core::body::read_success(*node_id, *endpoint, cluster, attribute, Value::Bool(v)))`
- `Op::Read`(汎用アーム): `Ok(mat_core::body::read_success(*node_id, *endpoint, cluster, attribute, v))`
- `Op::Write` アーム: `Ok(mat_core::body::write_success(*node_id, *endpoint, cluster, attribute, value))`
- `Op::Invoke` アーム: `Ok(mat_core::body::invoke_success(*node_id, *endpoint, cluster, command))`
- `Op::Describe` アーム: `Ok(mat_core::body::describe_success(*node_id, &endpoints))`

注: アームのフィールド束縛は既存のまま使う(必要なら束縛を追加)。`native_op` 末尾の `_ => unreachable!("native_op called with non-hotpath op")` は `is_native_hotpath` ガード下の網羅性キャッチオールなので**残す**(スコープ外)。

- [ ] **Step 3: `native_group_params` に成功 body を同梱させる**

型 alias(`server.rs:464`)を変更:

```rust
/// `native_group_params` の Ok 内訳: (group_id, cluster_id, command_id, fields_tlv,
/// 成功時 sent body)。body は op 変種が確定しているここで組む(旧 `group_sent_body`
/// の `let … else unreachable!` を型で排除)。
type GroupSendParams = (u16, u32, u32, Option<Vec<u8>>, Value);
```

各アームの `Some(Ok((...)))` に body を追加:

- `Op::GroupInvoke` アーム: `mat_core::body::group_invoke_sent(*group_id, cluster, command, *endpoint)`
- `Op::GroupColorTemp` アーム: `mat_core::body::group_color_temp_sent(*group_id, *kelvin, *mireds, *transition, *endpoint)`(`kelvin` / `endpoint` の束縛を `..` から明示に追加)
- `Op::GroupLevel` アーム: `mat_core::body::group_level_sent(*group_id, *percent, *level, *transition, *endpoint)`(同上)
- `Op::GroupColor` アーム: `ResolvedColor` を組んで `mat_core::body::group_color_sent(*group_id, &color, *transition, *endpoint)`(Step 2 の `Op::Color` と同じ組み方)

呼び出し側 `run_op`(`server.rs:369-388`)を 5 要素で受ける:

```rust
if let Some(result) = native_group_params(op) {
    return match result {
        Ok((group_id, cluster, command, fields, sent_body)) => {
            // chip-tool 撤去前と同じ前提チェック(store が開けること)。
            let _store = Store::open(store_path)?;
            match native
                .group_invoke(group_id, cluster, command, fields)
                .await?
            {
                crate::native::GroupOutcome::Sent => Ok(sent_body),
                crate::native::GroupOutcome::Unavailable(reason) => {
                    Err(group_unavailable_error(&reason))
                }
            }
        }
        // 名前は解決できたが引数が符号化不能 → 即座に拒否(mat 側
        // classify_strict と同じ規則)。
        Err(e) => Err(e),
    };
}
```

- [ ] **Step 4: `group_provision` の body を委譲に置換**

`group_provision`(`server.rs:884` 付近)の末尾 `Ok(json!({...}))` を:

```rust
Ok(mat_core::body::group_provision_success(
    *group_id, *keyset_id, name, *endpoint, node_ids, None,
))
```

(matd 経路の provision は note 無し — 既存出力どおり。)

- [ ] **Step 5: 旧 body 関数 5 本を削除**

`write_success_body` / `invoke_success_body` / `describe_success_body` / `hotpath_success_body` / `group_sent_body` を削除(`let … else unreachable!` もろとも)。未使用 import(`json!` 等)が出たら整理。

- [ ] **Step 6: `task check` で出力不変を確認**

Run: `task check`
Expected: PASS。特に `server.rs` 内の既存スキーマ期待値テスト
(`native_generic_read_body_matches_expected_schema` /
`native_generic_invoke_and_describe_bodies_match_expected_schema` 等)が
**無変更で**通ること = matd 経路出力不変の証明。

- [ ] **Step 7: コミット**

```bash
git add crates/matd/src/server.rs
git commit -m "refactor(matd): 成功bodyを mat_core::body 共有化、let-else unreachable! を排除"
```

---

### Task 4: `classify_write` / `classify_invoke` が解決済み cluster ID を返す

**Files:**
- Modify: `crates/mat-core/src/ids.rs`
- Modify: `crates/mat/src/native_direct.rs`(`classify_strict` の 3 箇所)
- Modify: `crates/matd/src/server.rs`(`native_group_params` の GroupInvoke アーム、`native_op` の Write / Invoke アーム)

**Interfaces:**
- Produces: `WriteClass::Native` / `InvokeClass::Native` に `cluster: u32` フィールドが追加される。呼び手は `resolve_cluster(...).expect(...)` の代わりにこのフィールドを使う。

- [ ] **Step 1: variant にフィールドを足し、既存テストの構築箇所を更新(先に赤にする)**

`crates/mat-core/src/ids.rs`:

```rust
pub enum WriteClass {
    /// native で実行可能。`cluster` / `attribute` は数値 ID、`value` は型に沿って
    /// 符号化済みのスカラー。cluster ID を含めるのは、呼び手に resolve_cluster の
    /// 再解決(classifier との drift で panic し得る)をさせないため。
    Native {
        cluster: u32,
        attribute: u32,
        value: ScalarValue,
        timed: bool,
    },
    // Reject / NotNative は不変
}
```

`classify_write` の `Ok(v) =>` 分岐を `WriteClass::Native { cluster: cluster_id, attribute: attr.id, value: v, timed }` に。`InvokeClass::Native` も同様に `cluster: u32` を追加し、`classify_invoke` の 2 箇所(def あり / 数値直指定)で `cluster: cluster_id` を入れる。

`ids.rs` 内の既存テストで `WriteClass::Native { .. }` / `InvokeClass::Native { .. }` を構築・パターンマッチしている箇所に `cluster: 0x…`(期待値)または `cluster: _` を追記する。**期待値アサーションの他フィールドは変えない。**

Run: `cargo build --workspace`
Expected: FAIL(呼び手 3 ファイルのパターン不足エラー — 変更漏れ検出として意図どおり)

- [ ] **Step 2: 呼び手の `resolve_cluster(...).expect(...)` を全廃**

パターン(全 5 箇所共通): `Native { command: cmd_id, fields, timed }` → `Native { cluster: cluster_id, command: cmd_id, fields, timed }` と束縛し、直後の

```rust
let cluster_id = mat_core::ids::resolve_cluster(cluster)
    .expect("classify_* already resolved this cluster name");
```

を削除する。対象:

1. `crates/mat/src/native_direct.rs` `classify_strict` の `Command::Write` アーム(旧 :438)
2. 同 `Command::Invoke` アーム(旧 :470)
3. 同 `Command::Group`(GroupInvoke)アーム(旧 :514)
4. `crates/matd/src/server.rs` `native_group_params` の `Op::GroupInvoke` アーム(旧 :490)
5. 同 `native_op` の `Op::Write` / `Op::Invoke` アーム(旧 :608 / :664 付近 — `expect("classify_* already resolved ...")` で grep して漏れなく)

- [ ] **Step 3: `task check`**

Run: `task check`
Expected: PASS(`git grep 'already resolved this cluster name'` が 0 件になっていること)

- [ ] **Step 4: コミット**

```bash
git add crates/mat-core/src/ids.rs crates/mat/src/native_direct.rs crates/matd/src/server.rs
git commit -m "refactor(ids): classify_write/invoke が解決済み cluster ID を返す — 再解決 expect を全廃"
```

---

### Task 5: `native_direct::run_op` を 1 op = 1 関数に分割

**Files:**
- Modify: `crates/mat/src/native_direct.rs`

**Interfaces:**
- Produces: 下記 19 個の `async fn`(いずれも `pub` にしない・同ファイル内)。`run_op` は破棄せず、各アームが対応関数へ委譲する薄い match になる。

- [ ] **Step 1: per-op 関数へ機械的に抽出**

`run_op`(旧 :706、~530 行)の各 match アームの本体を、**一字も変えずに**(tracing・エラー写像・emit 呼び出し含む)以下の関数へ移す。シグネチャはアームの束縛をそのまま引数化したもの:

```rust
async fn op_on(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError>
async fn op_off(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError>
async fn op_read_onoff(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError>
async fn op_color(engine: &Engine, node_id: u64, endpoint: u16, color: &ResolvedColor, transition: u16) -> Result<(), MatError>
async fn op_color_temp(engine: &Engine, node_id: u64, endpoint: u16, kelvin: u32, mireds: u16, transition: u16) -> Result<(), MatError>
async fn op_level(engine: &Engine, node_id: u64, endpoint: u16, percent: u8, level: u8, transition: u16) -> Result<(), MatError>
async fn op_group_onoff(engine: &Engine, group_id: u16, command_id: u32, command: &str, endpoint: u16) -> Result<(), MatError>
async fn op_group_color(engine: &Engine, group_id: u16, color: &ResolvedColor, transition: u16, endpoint: u16) -> Result<(), MatError>
async fn op_group_color_temp(engine: &Engine, group_id: u16, kelvin: u32, mireds: u16, transition: u16, endpoint: u16) -> Result<(), MatError>
async fn op_group_level(engine: &Engine, group_id: u16, percent: u8, level: u8, transition: u16, endpoint: u16) -> Result<(), MatError>
async fn op_group_invoke_generic(engine: &Engine, group_id: u16, cluster_in: &str, command_in: &str, cluster: u32, command: u32, fields_tlv: &Option<Vec<u8>>, endpoint: u16) -> Result<(), MatError>
async fn op_group_provision(engine: &Engine, group_id: u16, node_ids: &[u64], keyset_id: u16, name: &str, endpoint: u16, epoch_key: Option<&str>, rebind: bool) -> Result<(), MatError>
async fn op_group_grant(engine: &Engine, group_id: u16, node_ids: &[u64]) -> Result<(), MatError>
async fn op_read_attr(engine: &Engine, node_id: u64, endpoint: u16, cluster_in: &str, attribute_in: &str, cluster: u32, attribute: u32) -> Result<(), MatError>
async fn op_write_attr(engine: &Engine, node_id: u64, endpoint: u16, cluster_in: &str, attribute_in: &str, cluster: u32, attribute: u32, value_in: &str, value: &ScalarValue, timed: bool) -> Result<(), MatError>
async fn op_invoke_generic(engine: &Engine, node_id: u64, endpoint: u16, cluster_in: &str, command_in: &str, cluster: u32, command: u32, fields_tlv: &Option<Vec<u8>>, timed: bool) -> Result<(), MatError>
async fn op_describe(engine: &Engine, node_id: u64) -> Result<(), MatError>
async fn op_diag_thread(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError>
async fn op_open_window(engine: &Engine, node_id: u64, timeout: u32, iteration: u32, discriminator: u16) -> Result<(), MatError>
```

注:
- 引数型はアーム内の使用箇所に合わせて deref(`*node_id` → `node_id: u64`)し、`clone()` を増やさない(参照 `&Option<Vec<u8>>` のまま渡し、関数内で既存どおり `fields_tlv.clone()` する)。
- `use mat_controller::im;` は関数ごとに必要なら関数内 use にするか、モジュールレベル use に昇格させる(どちらでも可、clippy が通ること)。
- `op_open_window` のシグネチャは `NativeOp::OpenWindow` の実フィールド型(`timeout: u32, iteration: u32, discriminator: u16` — `native_direct.rs:154-159`)と一致済み。
- 各関数の doc コメントには元アームの説明コメントを移す(新規説明は書き足さない)。

新しい `run_op`:

```rust
/// 確立 → 1 op → 破棄。値を返す op(read)は emit まで行う。ディスパッチのみ —
/// 各 op の実体は op_*(1 op = 1 関数、matd の native.rs と同じ粒度)。
///
/// M8c-3(chip-tool 撤去): 従来 `Fallback` を返していた分岐はハードエラー化。
/// `engine.group` / `engine.group_settings` 未設定は本番 `Engine::build` では
/// 常に `Some`(`with_parts` テスト注入時のみ `None`)なので Other、
/// `GroupOutcome::Unavailable`(未 provision・KVS 不備)は store_parse で返す。
async fn run_op(engine: &Engine, op: &NativeOp) -> Result<(), MatError> {
    match op {
        NativeOp::On { node_id, endpoint } => op_on(engine, *node_id, *endpoint).await,
        NativeOp::Off { node_id, endpoint } => op_off(engine, *node_id, *endpoint).await,
        // …全 19 variant を同形で委譲(フィールドを deref して渡すだけ)…
    }
}
```

- [ ] **Step 2: `task check`**

Run: `task check`
Expected: PASS(mat のバイナリ統合テスト・native_direct のテストが無変更で通る)

- [ ] **Step 3: コミット**

```bash
git add crates/mat/src/native_direct.rs
git commit -m "refactor(mat): run_op を 1 op = 1 関数に分割(挙動不変)"
```

---

### Task 6: 小粒 — `op_report_expectation` の定数化 + dead code 削除

**Files:**
- Modify: `crates/matd/src/server.rs`(`op_report_expectation`)
- Modify: `crates/mat-core/src/store.rs`(`contains` 削除)

**Interfaces:**
- Consumes: `im::CLUSTER_ON_OFF` / `im::CLUSTER_LEVEL_CONTROL` / `im::CLUSTER_COLOR_CONTROL`(`server.rs` は既に `im::CLUSTER_COLOR_CONTROL` 等を使用中 — 同じ import を使う)
- Produces: なし(外部インターフェース不変)

- [ ] **Step 1: 生 hex を定数参照へ**

`op_report_expectation`(`server.rs:448` 付近):

```rust
fn op_report_expectation(op: &Op) -> Option<(u64, u32)> {
    match op {
        Op::On { node_id, .. } | Op::Off { node_id, .. } => {
            Some((*node_id, im::CLUSTER_ON_OFF))
        }
        Op::Level { node_id, .. } => Some((*node_id, im::CLUSTER_LEVEL_CONTROL)),
        Op::Color { node_id, .. } | Op::ColorTemp { node_id, .. } => {
            Some((*node_id, im::CLUSTER_COLOR_CONTROL))
        }
        Op::Write {
            node_id, cluster, ..
        }
        | Op::Invoke {
            node_id, cluster, ..
        } => mat_core::ids::resolve_cluster(cluster).map(|c| (*node_id, c)),
        _ => None,
    }
}
```

(`im::CLUSTER_ON_OFF` 等は `pub const CLUSTER_ON_OFF: u32 = 0x0006;`(`im.rs:21`)で戻り値型 `u32` と一致、`server.rs:21` で `use mat_controller::im;` 済み — キャスト・import 追加とも不要。)

- [ ] **Step 2: `store.rs` の dead code を削除**

`crates/mat-core/src/store.rs:139-142` の

```rust
#[allow(dead_code)]
pub fn contains(&self, node_id: u64) -> bool {
    self.ledger.nodes.contains_key(&node_id)
}
```

を削除する。(`git grep -n '\.contains('` で mat / matd からの呼び出しが無いことを確認してから。)

- [ ] **Step 3: `task check` → コミット**

Run: `task check`
Expected: PASS(born-dead 検知の既存テスト `sub_health_notes_and_clears_pending_respecting_clusters` 等が無変更で通る)

```bash
git add crates/matd/src/server.rs crates/mat-core/src/store.rs
git commit -m "refactor: op_report_expectation を im::CLUSTER_* 定数化、store の dead code 削除"
```

---

### Task 7: バージョン 0.28.1 へ bump

**Files:**
- Modify: `Cargo.toml`(workspace version — 実位置は `grep -n '0.28.0' Cargo.toml crates/*/Cargo.toml` で確認)
- Modify: `Cargo.lock`(ビルドで自動更新)

- [ ] **Step 1: version を 0.28.1 に変更しビルド**

workspace の version(0.28.0)を 0.28.1 へ。`cargo build --workspace` で `Cargo.lock` を更新。

- [ ] **Step 2: `task check`**

Run: `task check`
Expected: PASS

- [ ] **Step 3: コミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(release): 0.28.1 — 成功JSON共有builder化・classify解決済みID一貫化リファクタ"
```

---

## 完了条件(spec 対応表)

| spec 要件 | Task |
|---|---|
| `mat_core::body` 新設 + 形状固定テスト | 1 |
| mat 側 emit_* 委譲(呼び出し面不変) | 2 |
| matd 側 *_body 委譲 + `let…else unreachable!` 排除 | 3 |
| 既存 matd スキーマ期待値テスト無変更で通る | 3 Step 6 |
| classify の Native に cluster ID、`expect` 再解決全廃 | 4 |
| `run_op` 1 op = 1 関数分割 | 5 |
| `op_report_expectation` 定数化 | 6 |
| `store.rs` dead code 削除 | 6 |
| 0.28.1(patch) | 7 |
| `alias.rs` の `unreachable!` は対象外 | —(スコープ外) |
