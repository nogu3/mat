# op 単一ソース化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat`（one-shot 直経路）と `matd`（warm セッション）で二重実装されている op→TLV→成功 body を `mat-native` の 1 箇所に集約し、両バイナリは「セッションの取り方」（`NodeRunner`）だけを差し替える。

**Architecture:** 新モジュール `mat_native::op`（解決済み op 型 `NodeOp` / `GroupOp` / `ProvisionParams` + 名前解決・換算コンストラクタ + `run_node_op` / `run_group_op` / `run_group_bump`）と `mat_native::runner`（`NodeRunner` トレイト + `OneShotRunner` + `run_node` / `provision` / `grant`）。`mat` は `Command`→`DeviceOp` の match 1 本、`matd` は wire `Op`→`MatdOp` の match 1 本に縮む。wire プロトコル（`matd/protocol.rs`）と CLI（`cli.rs` / `resolve.rs`）は不変。

**Tech Stack:** Rust workspace（`mat-core` / `mat-controller` / `mat-native` / `mat` / `matd`）、tokio、async-trait、serde_json。テストは `mat_native::test_support::{FakeConn, FakeEstablisher}`。

**Spec:** `docs/superpowers/specs/2026-09-02-op-single-source-design.md`

## Global Constraints

- stdout は純粋 JSON のみ、診断は stderr の `tracing`（CLAUDE.md 設計ルール 2/3）。
- wire JSON（`matd_client::to_op` の出力）は **1 文字も変えない**。既存 golden テストの期待 JSON をそのまま使う。
- 成功 body の形・エラー kind・exit code は現行どおり（spec「エラー・exit code」表）。
- `matd/src/protocol.rs` の `Op` と helper、`cli.rs`、`resolve.rs`、`op_state_target` / `op_report_expectation` は変更しない。
- 各タスク終了時に `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` + 対象クレートの `cargo test` が通ること（最終は `task check`）。
- コミットは各タスクで 1 つ。メッセージ末尾に
  `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` と
  `Claude-Session: https://claude.ai/code/session_01KmkEgqmNW4jMcJRU2z64cM` を付ける。
- バージョンは上げない（次回 publish は `task semver` の判定に従う、Task 12 で記録のみ）。
- ワークツリー: `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/op-single-source`（ブランチ `worktree-op-single-source`）。パスはすべてこの下の相対パス。

## File Structure

| ファイル | 責務 |
|---|---|
| `crates/mat-core/src/body.rs`（変更） | 成功 body の単一ソース。直経路専用だった diag thread / open-window / group grant の body を追加 |
| `crates/mat-native/src/op.rs`（新規） | `NodeOp` / `NodeOpKind` / `GroupOp` / `GroupOpKind` / `ProvisionParams`、名前解決・換算コンストラクタ、`units`、`run_node_op` / `run_group_op` / `run_group_bump` |
| `crates/mat-native/src/runner.rs`（新規） | `NodeRunner` トレイト、`OneShotRunner`、`run_node` / `provision` / `grant` |
| `crates/mat-native/src/lib.rs`（変更） | `pub mod op; pub mod runner;` |
| `crates/matd/src/native.rs`（変更） | op 別メソッド削除、`impl NodeRunner`、`engine()` |
| `crates/matd/src/server.rs`（変更） | `MatdOp` + `to_device_op`、`run_op` 書き換え、旧分類関数削除 |
| `crates/mat/src/device_op.rs`（新規） | `DeviceOp` / `Dispatch` / `classify`（`Command` の match 1 本）、`resolve_discriminator` |
| `crates/mat/src/matd_client.rs`（変更） | `to_op(&DeviceOp)`、`attach_deadline(op, applies, ms)`、`dispatch*(…, &DeviceOp, …)`、`unsupported_exit` |
| `crates/mat/src/native_direct.rs`（変更） | `run(&DeviceOp, …)`、`NativeOp` / `classify*` / `op_*` 削除 |
| `crates/mat/src/main.rs`（変更） | `classify` を 1 回呼び `Dispatch` で経路分岐 |
| `crates/mat/src/units.rs`（削除） | `mat_native::op::units` へ移設 |
| `crates/mat/src/commands/{open_window,group}.rs`（削除）、`commands/diag.rs`（変更） | emit 関数を body へ移設 |
| `ARCHITECTURE.md` / `CLAUDE.md`（変更） | 記録 |

---

### Task 1: `mat_core::body` に直経路専用 body を追加

**Files:**
- Modify: `crates/mat-core/src/body.rs`

**Interfaces:**
- Produces:
  - `pub fn diag_thread_success(node_id: u64, endpoint: u16, thread: Map<String, Value>, unavailable: &[(String, ErrorKind)]) -> Value`
  - `pub fn open_window_success(node_id: u64, manual_code: &str, qr_payload: &str, timeout: u32) -> Value`
  - `pub fn group_grant_success(group_id: u16, node_ids: &[u64], updated: &[u64], unchanged: &[u64]) -> Value`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/body.rs` の `mod tests` 末尾に追加:

```rust
    #[test]
    fn diag_thread_success_omits_empty_unavailable() {
        let mut thread = serde_json::Map::new();
        thread.insert("channel".into(), json!(15));
        let body = diag_thread_success(5, 0, thread.clone(), &[]);
        assert_eq!(
            body,
            json!({ "node_id": 5, "endpoint": 0, "thread": { "channel": 15 } })
        );
        let body = diag_thread_success(
            5,
            0,
            thread,
            &[("rloc16".to_string(), crate::error::ErrorKind::Timeout)],
        );
        assert_eq!(
            body["unavailable"],
            json!([{ "attribute": "rloc16", "kind": "timeout" }])
        );
    }

    #[test]
    fn open_window_success_shape() {
        let body = open_window_success(5, "34970112332", "MT:ABC", 180);
        assert_eq!(body["node_id"], 5);
        assert_eq!(body["manual_code"], "34970112332");
        assert_eq!(body["qr_payload"], "MT:ABC");
        assert!(body["expires_at"].is_string());
    }

    #[test]
    fn group_grant_success_shape() {
        assert_eq!(
            group_grant_success(10, &[5, 6], &[5], &[6]),
            json!({
                "group_id": 10, "nodes": [5, 6], "updated": [5],
                "unchanged": [6], "status": "granted",
            })
        );
    }
```

（`"kind": "timeout"` の文字列は `ErrorKind` の serde 表現。既存テスト
`exit_codes_match_spec` 付近の `ErrorKind` の `Serialize` derive を確認し、
rename 規則が `snake_case` でなければ実際の表現に合わせる。）

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-core body::tests`
Expected: コンパイルエラー（`diag_thread_success` 未定義）。

- [ ] **Step 3: 実装**

`crates/mat-core/src/body.rs` の `group_provision_success` の後に追加:

```rust
/// `diag thread` の成功 body。`unavailable` は (chip-tool 属性名, kind)
/// — 空なら `unavailable` キー自体を出さない（既存形）。
pub fn diag_thread_success(
    node_id: u64,
    endpoint: u16,
    thread: serde_json::Map<String, Value>,
    unavailable: &[(String, crate::error::ErrorKind)],
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("node_id".to_string(), json!(node_id));
    body.insert("endpoint".to_string(), json!(endpoint));
    body.insert("thread".to_string(), Value::Object(thread));
    if !unavailable.is_empty() {
        let rows: Vec<Value> = unavailable
            .iter()
            .map(|(attr, kind)| {
                json!({
                    "attribute": attr,
                    "kind": serde_json::to_value(kind).unwrap_or(Value::Null),
                })
            })
            .collect();
        body.insert("unavailable".to_string(), Value::Array(rows));
    }
    Value::Object(body)
}

/// `open-window` の成功 body。`expires_at` は timeout 秒後の ISO 8601。
pub fn open_window_success(
    node_id: u64,
    manual_code: &str,
    qr_payload: &str,
    timeout: u32,
) -> Value {
    json!({
        "node_id": node_id,
        "manual_code": manual_code,
        "qr_payload": qr_payload,
        "expires_at": crate::output::expires_in(i64::from(timeout)),
    })
}

/// `group grant` の成功 body。
pub fn group_grant_success(
    group_id: u16,
    node_ids: &[u64],
    updated: &[u64],
    unchanged: &[u64],
) -> Value {
    json!({
        "group_id": group_id,
        "nodes": node_ids,
        "updated": updated,
        "unchanged": unchanged,
        "status": "granted",
    })
}
```

ファイル冒頭 doc の「対象は両経路に存在する op のみ。直経路専用 op … は
`mat` 側に残す」の段落を次に置き換える:

```rust
//! 直経路専用 op(open-window / diag thread / grant)の body もここに置く
//! (mat-native::op::run_node_op / runner::grant が経路によらず組むため)。
//! discover / commission / diag node / mesh は専用コマンド層(`mat/commands`)
//! が emit する。
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core body::tests`
Expected: PASS（3 本追加）。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-core/src/body.rs
git commit -m "feat(core): body に diag thread / open-window / group grant の成功 body を追加（監査④ Task1）"
```

---

### Task 2: `mat_native::op` — 解決済み op 型・コンストラクタ・換算

**Files:**
- Create: `crates/mat-native/src/op.rs`
- Modify: `crates/mat-native/src/lib.rs`（`pub mod op;` 追加、`pub mod ops;` の直後）

**Interfaces:**
- Consumes: `mat_core::ids::{resolve_cluster, resolve_attribute, classify_write, classify_invoke, WriteClass, InvokeClass, ScalarValue}`、`mat_core::color::ResolvedColor`、`crate::encode_command_fields`。
- Produces（後続タスクが使う正確な形）:

```rust
pub mod units {
    pub fn resolve_color_temp(kelvin: Option<u32>, mireds: Option<u16>) -> (u16, u32);
    pub fn resolve_level(percent: u8) -> u8;
}
#[derive(Debug, Clone, PartialEq)] pub struct NodeOp { pub node_id: u64, pub kind: NodeOpKind }
#[derive(Debug, Clone, PartialEq)] pub enum NodeOpKind {
    On { endpoint: u16 },
    Off { endpoint: u16 },
    Color { endpoint: u16, color: ResolvedColor, transition: u16 },
    ColorTemp { endpoint: u16, kelvin: u32, mireds: u16, transition: u16 },
    Level { endpoint: u16, percent: u8, level: u8, transition: u16 },
    Read { endpoint: u16, cluster_in: String, attribute_in: String, cluster: u32, attribute: u32 },
    Write { endpoint: u16, cluster_in: String, attribute_in: String, cluster: u32, attribute: u32,
            value_in: String, value: ScalarValue, timed: bool },
    Invoke { endpoint: u16, cluster_in: String, command_in: String, args_in: Vec<String>,
             cluster: u32, command: u32, fields_tlv: Option<Vec<u8>>, timed: bool },
    Describe,
    DiagThread { endpoint: u16 },
    OpenWindow { timeout: u32, iteration: u32, discriminator: u16 },
}
impl NodeOpKind {
    pub fn read(endpoint: u16, cluster_in: &str, attribute_in: &str) -> Result<Self, MatError>;
    pub fn write(endpoint: u16, cluster_in: &str, attribute_in: &str, value_in: &str) -> Result<Self, MatError>;
    pub fn invoke(endpoint: u16, cluster_in: &str, command_in: &str, args: &[String]) -> Result<Self, MatError>;
    pub fn color_temp(endpoint: u16, kelvin: Option<u32>, mireds: Option<u16>, transition: u16) -> Self;
    pub fn level(endpoint: u16, percent: u8, transition: u16) -> Self;
    pub fn budget_applies(&self) -> bool;
    pub fn name(&self) -> &'static str;
}
#[derive(Debug, Clone, PartialEq)] pub struct GroupOp { pub group_id: u16, pub endpoint: u16, pub kind: GroupOpKind }
#[derive(Debug, Clone, PartialEq)] pub enum GroupOpKind {
    Invoke { cluster_in: String, command_in: String, args_in: Vec<String>, cluster: u32, command: u32, fields_tlv: Option<Vec<u8>> },
    Color { color: ResolvedColor, transition: u16 },
    ColorTemp { kelvin: u32, mireds: u16, transition: u16 },
    Level { percent: u8, level: u8, transition: u16 },
}
impl GroupOpKind {
    pub fn invoke(cluster_in: &str, command_in: &str, args: &[String]) -> Result<Self, MatError>;
    pub fn color_temp(kelvin: Option<u32>, mireds: Option<u16>, transition: u16) -> Self;
    pub fn level(percent: u8, transition: u16) -> Self;
    pub fn name(&self) -> &'static str;
}
#[derive(Debug, Clone, PartialEq)] pub struct ProvisionParams { pub group_id: u16, pub node_ids: Vec<u64>,
    pub keyset_id: u16, pub name: String, pub endpoint: u16, pub epoch_key: Option<String>, pub rebind: bool }
```

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/op.rs` を作り、まずテストだけ置く（本体は Step 3）:

```rust
//! op → TLV → 成功 body の単一ソース（監査④）。
//!
//! `mat`（one-shot 直経路）と `matd`（warm セッション）の両方がここを通る。
//! 値はすべて解決済み（cluster/attribute/command は数値 ID、色・色温度・
//! level は raw 値、`*_in` は応答エコー用の入力文字列）。名前解決と換算の
//! 規則はこのモジュールのコンストラクタだけが持つ。

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::im::{CLUSTER_ON_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_TOGGLE};
    use mat_core::error::ErrorKind;

    #[test]
    fn kelvin_2700_converts_to_370_mireds() {
        assert_eq!(units::resolve_color_temp(Some(2700), None), (370, 2700));
    }

    #[test]
    fn kelvin_6500_rounds_to_154_mireds() {
        assert_eq!(units::resolve_color_temp(Some(6500), None), (154, 6500));
    }

    #[test]
    fn mireds_direct_computes_kelvin_echo() {
        assert_eq!(units::resolve_color_temp(None, Some(370)), (370, 2703));
    }

    #[test]
    fn resolve_level_rounds_percent_to_254_scale() {
        assert_eq!(units::resolve_level(0), 0);
        assert_eq!(units::resolve_level(1), 3);
        assert_eq!(units::resolve_level(50), 127);
        assert_eq!(units::resolve_level(100), 254);
    }

    #[test]
    fn read_resolves_names_and_numeric_ids() {
        let k = NodeOpKind::read(1, "levelcontrol", "current-level").unwrap();
        assert!(matches!(
            k,
            NodeOpKind::Read { endpoint: 1, cluster: 0x0008, attribute: 0x0000, .. }
        ));
        let k = NodeOpKind::read(1, "0x0008", "0").unwrap();
        assert!(matches!(k, NodeOpKind::Read { cluster: 0x0008, attribute: 0, .. }));
        let err = NodeOpKind::read(1, "nosuchcluster", "x").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("numeric IDs are accepted"), "{}", err.detail);
    }

    #[test]
    fn write_scalar_ok_list_rejected_unknown_unresolved() {
        let k = NodeOpKind::write(1, "levelcontrol", "on-level", "128").unwrap();
        assert!(matches!(
            k,
            NodeOpKind::Write { cluster: 0x0008, value: ScalarValue::UInt(128), timed: false, .. }
        ));
        let err = NodeOpKind::write(1, "accesscontrol", "acl", "[]").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("list"), "{}", err.detail);
        let err = NodeOpKind::write(1, "nosuch", "x", "1").unwrap_err();
        assert!(err.detail.contains("numeric IDs are accepted"), "{}", err.detail);
    }

    #[test]
    fn invoke_scalar_args_ok_struct_args_rejected() {
        let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
        let k = NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args).unwrap();
        assert!(matches!(k, NodeOpKind::Invoke { cluster: 0x0008, fields_tlv: Some(_), .. }));
        let k = NodeOpKind::invoke(1, "onoff", "on", &[]).unwrap();
        assert!(matches!(
            k,
            NodeOpKind::Invoke { cluster: CLUSTER_ON_OFF, command: CMD_ON_OFF_ON, fields_tlv: None, .. }
        ));
        let err = NodeOpKind::invoke(1, "groupkeymanagement", "key-set-write", &["{}".into()])
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn group_invoke_resolves_like_node_invoke() {
        let k = GroupOpKind::invoke("onoff", "toggle", &[]).unwrap();
        assert!(matches!(
            k,
            GroupOpKind::Invoke { cluster: CLUSTER_ON_OFF, command: CMD_ON_OFF_TOGGLE, fields_tlv: None, .. }
        ));
        let err = GroupOpKind::invoke("onoff", "on", &["1".into()]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        let err = GroupOpKind::invoke("onoff", "foo", &[]).unwrap_err();
        assert!(err.detail.contains("numeric IDs are accepted"), "{}", err.detail);
    }

    #[test]
    fn color_temp_and_level_constructors_convert_units() {
        assert_eq!(
            NodeOpKind::color_temp(1, Some(2700), None, 0),
            NodeOpKind::ColorTemp { endpoint: 1, kelvin: 2700, mireds: 370, transition: 0 }
        );
        assert_eq!(
            NodeOpKind::level(1, 50, 0),
            NodeOpKind::Level { endpoint: 1, percent: 50, level: 127, transition: 0 }
        );
        assert_eq!(
            GroupOpKind::color_temp(None, Some(370), 5),
            GroupOpKind::ColorTemp { kelvin: 2703, mireds: 370, transition: 5 }
        );
        assert_eq!(
            GroupOpKind::level(100, 0),
            GroupOpKind::Level { percent: 100, level: 254, transition: 0 }
        );
    }

    #[test]
    fn budget_applies_only_to_single_node_hotpath_ops() {
        assert!(NodeOpKind::On { endpoint: 1 }.budget_applies());
        assert!(NodeOpKind::Off { endpoint: 1 }.budget_applies());
        assert!(NodeOpKind::level(1, 1, 0).budget_applies());
        assert!(NodeOpKind::color_temp(1, Some(2700), None, 0).budget_applies());
        assert!(NodeOpKind::read(1, "onoff", "on-off").unwrap().budget_applies());
        assert!(NodeOpKind::write(1, "onoff", "on-off", "true").unwrap().budget_applies());
        assert!(NodeOpKind::invoke(1, "onoff", "on", &[]).unwrap().budget_applies());
        assert!(NodeOpKind::Describe.budget_applies());
        assert!(!NodeOpKind::DiagThread { endpoint: 0 }.budget_applies());
        assert!(!NodeOpKind::OpenWindow { timeout: 180, iteration: 1000, discriminator: 1 }
            .budget_applies());
    }

    #[test]
    fn names_are_snake_case_wire_tags() {
        assert_eq!(NodeOpKind::On { endpoint: 1 }.name(), "on");
        assert_eq!(NodeOpKind::color_temp(1, Some(2700), None, 0).name(), "color_temp");
        assert_eq!(NodeOpKind::OpenWindow { timeout: 1, iteration: 1, discriminator: 1 }.name(), "open_window");
        assert_eq!(GroupOpKind::level(1, 0).name(), "group_level");
    }
}
```

`crates/mat-native/src/lib.rs` の `pub mod ops;` の直後に `pub mod op;` を追加。

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-native op::tests`
Expected: コンパイルエラー（型未定義）。

- [ ] **Step 3: 実装**

`crates/mat-native/src/op.rs` のテストの前に本体を書く:

```rust
use mat_core::color::ResolvedColor;
use mat_core::error::MatError;
use mat_core::ids::{self, InvokeClass, ScalarValue, WriteClass};

/// 経路非依存の入力換算（CLI 入力 → Matter 生値）。旧 `mat/src/units.rs`。
pub mod units {
    /// `--kelvin` / `--mireds`（排他・どちらか必須）を `(mireds, kelvin)` に
    /// 解決する。与えられなかった側は `round(1_000_000 / x)` で補完し、出力
    /// JSON へのエコーに使う。デバイス対応範囲の検証はしない。
    pub fn resolve_color_temp(kelvin: Option<u32>, mireds: Option<u16>) -> (u16, u32) {
        fn recip(v: u32) -> u32 {
            (1_000_000 + v / 2) / v
        }
        match (kelvin, mireds) {
            // CLI の値域制約（16..=1_000_000 K）により mireds は u16 に収まる。
            (Some(k), None) => (recip(k) as u16, k),
            (None, Some(m)) => (m, recip(u32::from(m))),
            _ => unreachable!("clap enforces exactly one of --kelvin / --mireds"),
        }
    }

    /// `--percent`（0–100）を LevelControl の 0–254 生値へ（255 は予約値）。
    pub fn resolve_level(percent: u8) -> u8 {
        ((u32::from(percent) * 254 + 50) / 100) as u8
    }
}

/// 単一ノード宛 op。
#[derive(Debug, Clone, PartialEq)]
pub struct NodeOp {
    pub node_id: u64,
    pub kind: NodeOpKind,
}

/// 単一ノード宛 op の種別。値は解決済み。
#[derive(Debug, Clone, PartialEq)]
pub enum NodeOpKind {
    On { endpoint: u16 },
    Off { endpoint: u16 },
    Color { endpoint: u16, color: ResolvedColor, transition: u16 },
    ColorTemp { endpoint: u16, kelvin: u32, mireds: u16, transition: u16 },
    Level { endpoint: u16, percent: u8, level: u8, transition: u16 },
    Read { endpoint: u16, cluster_in: String, attribute_in: String, cluster: u32, attribute: u32 },
    Write {
        endpoint: u16,
        cluster_in: String,
        attribute_in: String,
        cluster: u32,
        attribute: u32,
        value_in: String,
        value: ScalarValue,
        timed: bool,
    },
    Invoke {
        endpoint: u16,
        cluster_in: String,
        command_in: String,
        /// wire（matd）へ引数を名前のまま渡すためのエコー。
        args_in: Vec<String>,
        cluster: u32,
        command: u32,
        fields_tlv: Option<Vec<u8>>,
        timed: bool,
    },
    Describe,
    DiagThread { endpoint: u16 },
    OpenWindow { timeout: u32, iteration: u32, discriminator: u16 },
}

/// `classify_invoke` の結果を (cluster, command, fields_tlv, timed) に写す共通部。
fn resolve_invoke(
    cluster_in: &str,
    command_in: &str,
    args: &[String],
) -> Result<(u32, u32, Option<Vec<u8>>, bool), MatError> {
    match ids::classify_invoke(cluster_in, command_in, args) {
        InvokeClass::NotNative => Err(MatError::unresolved_op()),
        InvokeClass::Reject(msg) => Err(MatError::parse_error(msg)),
        InvokeClass::Native { cluster, command, fields, timed } => {
            let fields_tlv = if fields.is_empty() {
                None
            } else {
                Some(crate::encode_command_fields(&fields))
            };
            Ok((cluster, command, fields_tlv, timed))
        }
    }
}

impl NodeOpKind {
    /// 名前（または数値 ID）解決。未解決は `unresolved_op`（parse_error）。
    pub fn read(endpoint: u16, cluster_in: &str, attribute_in: &str) -> Result<Self, MatError> {
        let cluster = ids::resolve_cluster(cluster_in).ok_or_else(MatError::unresolved_op)?;
        let attr = ids::resolve_attribute(cluster, attribute_in).ok_or_else(MatError::unresolved_op)?;
        Ok(NodeOpKind::Read {
            endpoint,
            cluster_in: cluster_in.to_string(),
            attribute_in: attribute_in.to_string(),
            cluster,
            attribute: attr.id,
        })
    }

    /// 名前解決 + 値のスカラー化。`NotNative` = 未解決、`Reject` = 符号化不能。
    pub fn write(
        endpoint: u16,
        cluster_in: &str,
        attribute_in: &str,
        value_in: &str,
    ) -> Result<Self, MatError> {
        match ids::classify_write(cluster_in, attribute_in, value_in) {
            WriteClass::NotNative => Err(MatError::unresolved_op()),
            WriteClass::Reject(msg) => Err(MatError::parse_error(msg)),
            WriteClass::Native { cluster, attribute, value, timed } => Ok(NodeOpKind::Write {
                endpoint,
                cluster_in: cluster_in.to_string(),
                attribute_in: attribute_in.to_string(),
                cluster,
                attribute,
                value_in: value_in.to_string(),
                value,
                timed,
            }),
        }
    }

    /// 名前解決 + 引数のスカラー化 → CommandFields TLV。
    pub fn invoke(
        endpoint: u16,
        cluster_in: &str,
        command_in: &str,
        args: &[String],
    ) -> Result<Self, MatError> {
        let (cluster, command, fields_tlv, timed) = resolve_invoke(cluster_in, command_in, args)?;
        Ok(NodeOpKind::Invoke {
            endpoint,
            cluster_in: cluster_in.to_string(),
            command_in: command_in.to_string(),
            args_in: args.to_vec(),
            cluster,
            command,
            fields_tlv,
            timed,
        })
    }

    pub fn color_temp(endpoint: u16, kelvin: Option<u32>, mireds: Option<u16>, transition: u16) -> Self {
        let (mireds, kelvin) = units::resolve_color_temp(kelvin, mireds);
        NodeOpKind::ColorTemp { endpoint, kelvin, mireds, transition }
    }

    pub fn level(endpoint: u16, percent: u8, transition: u16) -> Self {
        NodeOpKind::Level { endpoint, percent, level: units::resolve_level(percent), transition }
    }

    /// `--op-timeout-ms` / matd `deadline_ms` の対象か（単一ノードの
    /// read/write/invoke/on/off/color 系/level/describe のみ）。
    pub fn budget_applies(&self) -> bool {
        !matches!(self, NodeOpKind::DiagThread { .. } | NodeOpKind::OpenWindow { .. })
    }

    /// ログ用の op 名（wire の snake_case タグと同じ）。
    pub fn name(&self) -> &'static str {
        match self {
            NodeOpKind::On { .. } => "on",
            NodeOpKind::Off { .. } => "off",
            NodeOpKind::Color { .. } => "color",
            NodeOpKind::ColorTemp { .. } => "color_temp",
            NodeOpKind::Level { .. } => "level",
            NodeOpKind::Read { .. } => "read",
            NodeOpKind::Write { .. } => "write",
            NodeOpKind::Invoke { .. } => "invoke",
            NodeOpKind::Describe => "describe",
            NodeOpKind::DiagThread { .. } => "diag_thread",
            NodeOpKind::OpenWindow { .. } => "open_window",
        }
    }
}

/// groupcast op（unacknowledged、"sent" のみ報告）。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupOp {
    pub group_id: u16,
    pub endpoint: u16,
    pub kind: GroupOpKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GroupOpKind {
    Invoke {
        cluster_in: String,
        command_in: String,
        args_in: Vec<String>,
        cluster: u32,
        command: u32,
        fields_tlv: Option<Vec<u8>>,
    },
    Color { color: ResolvedColor, transition: u16 },
    ColorTemp { kelvin: u32, mireds: u16, transition: u16 },
    Level { percent: u8, level: u8, transition: u16 },
}

impl GroupOpKind {
    /// 単体 invoke と同じ解決規則（timed は groupcast に無いので捨てる）。
    pub fn invoke(cluster_in: &str, command_in: &str, args: &[String]) -> Result<Self, MatError> {
        let (cluster, command, fields_tlv, _timed) = resolve_invoke(cluster_in, command_in, args)?;
        Ok(GroupOpKind::Invoke {
            cluster_in: cluster_in.to_string(),
            command_in: command_in.to_string(),
            args_in: args.to_vec(),
            cluster,
            command,
            fields_tlv,
        })
    }

    pub fn color_temp(kelvin: Option<u32>, mireds: Option<u16>, transition: u16) -> Self {
        let (mireds, kelvin) = units::resolve_color_temp(kelvin, mireds);
        GroupOpKind::ColorTemp { kelvin, mireds, transition }
    }

    pub fn level(percent: u8, transition: u16) -> Self {
        GroupOpKind::Level { percent, level: units::resolve_level(percent), transition }
    }

    pub fn name(&self) -> &'static str {
        match self {
            GroupOpKind::Invoke { .. } => "group_invoke",
            GroupOpKind::Color { .. } => "group_color",
            GroupOpKind::ColorTemp { .. } => "group_color_temp",
            GroupOpKind::Level { .. } => "group_level",
        }
    }
}

/// `group provision` の入力（直経路・matd 共通）。`epoch_key` は 32 桁 hex
/// または None（ランダム生成）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProvisionParams {
    pub group_id: u16,
    pub node_ids: Vec<u64>,
    pub keyset_id: u16,
    pub name: String,
    pub endpoint: u16,
    pub epoch_key: Option<String>,
    pub rebind: bool,
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native op::tests && cargo clippy -p mat-native --all-targets -- -D warnings`
Expected: PASS（11 本）。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-native/src/op.rs crates/mat-native/src/lib.rs
git commit -m "feat(native): mat_native::op — 解決済み op 型と名前解決・換算コンストラクタ（監査④ Task2）"
```

---

### Task 3: `run_node_op` — op → TLV → body の単一ソース（単一ノード）

**Files:**
- Modify: `crates/mat-native/src/op.rs`

**Interfaces:**
- Consumes: `crate::NodeConn`、`crate::ops::{describe, diag_thread}`、`crate::scalar_to_tlv`、`mat_core::body::*`（Task 1 の 3 本含む）、`mat_controller::im::{encode_move_to_hue_and_saturation_fields, encode_move_to_color_temperature_fields, encode_move_to_level_fields, CLUSTER_ON_OFF, ATTR_ON_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_OFF, CLUSTER_COLOR_CONTROL, CMD_MOVE_TO_HUE_AND_SATURATION, CMD_MOVE_TO_COLOR_TEMPERATURE, CLUSTER_LEVEL_CONTROL, CMD_MOVE_TO_LEVEL}`。
- Produces: `pub async fn run_node_op(conn: &mut dyn NodeConn, op: &NodeOp) -> Result<Value, MatError>`

- [ ] **Step 1: 失敗するテストを書く**

`op.rs` の `mod tests` に追加（`use crate::test_support::FakeConn; use serde_json::json; use mat_controller::im;` を先頭 use に足す）:

```rust
    fn node(kind: NodeOpKind) -> NodeOp {
        NodeOp { node_id: 5, kind }
    }

    #[tokio::test]
    async fn on_off_invoke_onoff_and_build_invoke_body() {
        let mut conn = FakeConn::default();
        let body = run_node_op(&mut conn, &node(NodeOpKind::On { endpoint: 1 })).await.unwrap();
        assert_eq!(body, mat_core::body::invoke_success(5, 1, "onoff", "on"));
        let body = run_node_op(&mut conn, &node(NodeOpKind::Off { endpoint: 1 })).await.unwrap();
        assert_eq!(body, mat_core::body::invoke_success(5, 1, "onoff", "off"));
        assert_eq!(
            conn.calls(),
            &[
                format!("invoke(1,{:#06X},{:#06X})", im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON),
                format!("invoke(1,{:#06X},{:#06X})", im::CLUSTER_ON_OFF, im::CMD_ON_OFF_OFF),
            ]
        );
    }

    #[tokio::test]
    async fn color_color_temp_level_send_expected_commands() {
        let mut conn = FakeConn::default();
        let color = ResolvedColor { hue_raw: 233, sat_raw: 203, hue: 330, sat: 80, name: None, rgb: None };
        let body = run_node_op(
            &mut conn,
            &node(NodeOpKind::Color { endpoint: 1, color: color.clone(), transition: 30 }),
        )
        .await
        .unwrap();
        assert_eq!(body, mat_core::body::color_success(5, 1, &color, 30));
        let body = run_node_op(&mut conn, &node(NodeOpKind::color_temp(1, Some(2700), None, 0)))
            .await
            .unwrap();
        assert_eq!(body, mat_core::body::color_temp_success(5, 1, 2700, 370, 0));
        let body = run_node_op(&mut conn, &node(NodeOpKind::level(1, 50, 0))).await.unwrap();
        assert_eq!(
            body,
            mat_core::body::level_success(5, 1, mat_core::body::LevelEcho { percent: 50, level: 127 }, 0)
        );
        assert_eq!(
            conn.calls(),
            &[
                format!("invoke(1,{:#06X},{:#06X})", im::CLUSTER_COLOR_CONTROL, im::CMD_MOVE_TO_HUE_AND_SATURATION),
                format!("invoke(1,{:#06X},{:#06X})", im::CLUSTER_COLOR_CONTROL, im::CMD_MOVE_TO_COLOR_TEMPERATURE),
                format!("invoke(1,{:#06X},{:#06X})", im::CLUSTER_LEVEL_CONTROL, im::CMD_MOVE_TO_LEVEL),
            ]
        );
    }

    #[tokio::test]
    async fn read_onoff_uses_bool_fast_path_and_generic_read_uses_json() {
        // FakeConn::read_onoff は常に true、read_json は登録値（未登録は 1）。
        let mut conn = FakeConn::scripted().with_read(1, 0x0008, 0x0000, json!(200));
        let body = run_node_op(&mut conn, &node(NodeOpKind::read(1, "onoff", "on-off").unwrap()))
            .await
            .unwrap();
        assert_eq!(body, mat_core::body::read_success(5, 1, "onoff", "on-off", json!(true)));
        let body = run_node_op(
            &mut conn,
            &node(NodeOpKind::read(1, "levelcontrol", "current-level").unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(body["value"], json!(200));
        assert_eq!(body["cluster"], "levelcontrol");
        assert_eq!(body["attribute"], "current-level");
    }

    #[tokio::test]
    async fn write_encodes_scalar_tlv_and_echoes_normalized_value() {
        let mut conn = FakeConn::default();
        let op = node(NodeOpKind::write(1, "levelcontrol", "on-level", "128").unwrap());
        let body = run_node_op(&mut conn, &op).await.unwrap();
        assert_eq!(body, mat_core::body::write_success(5, 1, "levelcontrol", "on-level", "128"));
        let (ep, cluster, attr, tlv) = &conn.written_tlv()[0];
        assert_eq!((*ep, *cluster, *attr), (1, 0x0008, 0x0011));
        assert_eq!(tlv, &crate::scalar_to_tlv(&ScalarValue::UInt(128)));
    }

    #[tokio::test]
    async fn invoke_generic_forwards_ids_and_builds_body() {
        let mut conn = FakeConn::default();
        let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
        let op = node(NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args).unwrap());
        let body = run_node_op(&mut conn, &op).await.unwrap();
        assert_eq!(body, mat_core::body::invoke_success(5, 1, "levelcontrol", "move-to-level"));
        assert_eq!(
            conn.calls(),
            &[format!("invoke(1,{:#06X},{:#06X})", im::CLUSTER_LEVEL_CONTROL, im::CMD_MOVE_TO_LEVEL)]
        );
    }

    #[tokio::test]
    async fn describe_diag_thread_and_open_window_build_bodies() {
        let mut conn = FakeConn::scripted().with_cluster(
            0,
            0x0035,
            vec![(0x0007, json!([{"5": 200}, {"5": 100}]))],
        );
        let body = run_node_op(&mut conn, &node(NodeOpKind::Describe)).await.unwrap();
        assert_eq!(body["node_id"], 5);
        assert!(body["endpoints"].is_array());

        let body = run_node_op(&mut conn, &node(NodeOpKind::DiagThread { endpoint: 0 }))
            .await
            .unwrap();
        assert_eq!(body["endpoint"], 0);
        assert!(body["thread"].is_object());

        let body = run_node_op(
            &mut conn,
            &node(NodeOpKind::OpenWindow { timeout: 180, iteration: 1000, discriminator: 3840 }),
        )
        .await
        .unwrap();
        assert_eq!(body["manual_code"], "34970112332");
        assert!(body["qr_payload"].as_str().unwrap().starts_with("MT:"));
        assert!(body["expires_at"].is_string());
    }

    #[tokio::test]
    async fn conn_error_propagates_unchanged() {
        let mut conn = FakeConn { fail_first_send: true, fail_kind: ErrorKind::Timeout, ..Default::default() };
        let err = run_node_op(&mut conn, &node(NodeOpKind::read(1, "onoff", "on-off").unwrap()))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Timeout);
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-native op::tests`
Expected: コンパイルエラー（`run_node_op` 未定義）。

- [ ] **Step 3: 実装**

`op.rs` の `ProvisionParams` の後に追加（先頭 use に `use serde_json::Value; use mat_controller::im; use mat_core::body; use crate::NodeConn;` を足す）:

```rust
/// 単一ノード op を 1 回実行し、成功 body（timestamp 抜き）を返す。
/// op → NodeConn 呼び出し（TLV 符号化）→ body 組立はここだけ。セッションの
/// 取得・後始末は呼び出し側（`runner`）の責務。
pub async fn run_node_op(conn: &mut dyn NodeConn, op: &NodeOp) -> Result<Value, MatError> {
    let node_id = op.node_id;
    let body = match &op.kind {
        NodeOpKind::On { endpoint } => {
            conn.invoke(*endpoint, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None, false)
                .await?;
            body::invoke_success(node_id, *endpoint, "onoff", "on")
        }
        NodeOpKind::Off { endpoint } => {
            conn.invoke(*endpoint, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_OFF, None, false)
                .await?;
            body::invoke_success(node_id, *endpoint, "onoff", "off")
        }
        NodeOpKind::Color { endpoint, color, transition } => {
            let fields = im::encode_move_to_hue_and_saturation_fields(
                color.hue_raw,
                color.sat_raw,
                *transition,
            );
            conn.invoke(
                *endpoint,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(fields),
                false,
            )
            .await?;
            body::color_success(node_id, *endpoint, color, *transition)
        }
        NodeOpKind::ColorTemp { endpoint, kelvin, mireds, transition } => {
            let fields = im::encode_move_to_color_temperature_fields(*mireds, *transition);
            conn.invoke(
                *endpoint,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(fields),
                false,
            )
            .await?;
            body::color_temp_success(node_id, *endpoint, *kelvin, *mireds, *transition)
        }
        NodeOpKind::Level { endpoint, percent, level, transition } => {
            let fields = im::encode_move_to_level_fields(*level, *transition);
            conn.invoke(
                *endpoint,
                im::CLUSTER_LEVEL_CONTROL,
                im::CMD_MOVE_TO_LEVEL,
                Some(fields),
                false,
            )
            .await?;
            body::level_success(
                node_id,
                *endpoint,
                body::LevelEcho { percent: *percent, level: *level },
                *transition,
            )
        }
        NodeOpKind::Read { endpoint, cluster_in, attribute_in, cluster, attribute } => {
            // onoff/on-off は bool 専用 read（両経路の従来挙動）。数値 ID 指定
            // （6/0）も同じ腕に落ちるが JSON は Bool で同形。
            let v = if *cluster == im::CLUSTER_ON_OFF && *attribute == im::ATTR_ON_OFF {
                Value::Bool(conn.read_onoff(*endpoint).await?)
            } else {
                conn.read_json(*endpoint, *cluster, *attribute).await?
            };
            body::read_success(node_id, *endpoint, cluster_in, attribute_in, v)
        }
        NodeOpKind::Write {
            endpoint, cluster_in, attribute_in, cluster, attribute, value_in, value, timed,
        } => {
            conn.write_tlv(*endpoint, *cluster, *attribute, crate::scalar_to_tlv(value), *timed)
                .await?;
            body::write_success(node_id, *endpoint, cluster_in, attribute_in, value_in)
        }
        NodeOpKind::Invoke {
            endpoint, cluster_in, command_in, cluster, command, fields_tlv, timed, ..
        } => {
            conn.invoke(*endpoint, *cluster, *command, fields_tlv.clone(), *timed)
                .await?;
            body::invoke_success(node_id, *endpoint, cluster_in, command_in)
        }
        NodeOpKind::Describe => {
            let endpoints = crate::ops::describe(conn).await?;
            body::describe_success(node_id, &endpoints)
        }
        NodeOpKind::DiagThread { endpoint } => {
            let snap = crate::ops::diag_thread(conn, *endpoint).await?;
            body::diag_thread_success(node_id, *endpoint, snap.fields, &snap.unavailable)
        }
        NodeOpKind::OpenWindow { timeout, iteration, discriminator } => {
            // CLI の timeout は u32、window API は u16（spec 上 16-bit）。飽和。
            let timeout_u16 = u16::try_from(*timeout).unwrap_or(u16::MAX);
            let (manual_code, qr_payload) = conn
                .open_window(timeout_u16, *discriminator, *iteration)
                .await?;
            body::open_window_success(node_id, &manual_code, &qr_payload, *timeout)
        }
    };
    tracing::debug!(node_id, op = op.kind.name(), "node op executed");
    Ok(body)
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native op::tests && cargo clippy -p mat-native --all-targets -- -D warnings`
Expected: PASS（7 本追加）。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-native/src/op.rs
git commit -m "feat(native): run_node_op — 単一ノード op の TLV/body 単一ソース（監査④ Task3）"
```

---

### Task 4: `run_group_op` / `run_group_bump`

**Files:**
- Modify: `crates/mat-native/src/op.rs`

**Interfaces:**
- Consumes: `crate::Engine`（`engine.group: Option<GroupCtx>`）、`crate::group::{send, bump, GroupOutcome, BumpOutcome}`、`mat_core::body::{group_invoke_sent, group_color_sent, group_color_temp_sent, group_level_sent, group_bump, LevelEcho}`、`MatError::{group_ctx_unconfigured, group_unavailable}`。
- Produces:
  - `impl GroupOpKind { pub fn wire(&self) -> (u32, u32, Option<Vec<u8>>) }`（cluster, command, fields_tlv）
  - `impl GroupOp { pub fn sent_body(&self, egress: &[String]) -> Value }`
  - `pub async fn run_group_op(engine: &Engine, op: &GroupOp) -> Result<Value, MatError>`
  - `pub async fn run_group_bump(engine: &Engine) -> Result<Value, MatError>`

- [ ] **Step 1: 失敗するテストを書く**

`op.rs` の `mod tests` に追加:

```rust
    #[test]
    fn group_wire_and_sent_body_for_shortcuts() {
        let ct = GroupOp { group_id: 10, endpoint: 1, kind: GroupOpKind::color_temp(Some(2702), None, 0) };
        let (cluster, command, fields) = ct.kind.wire();
        assert_eq!((cluster, command), (im::CLUSTER_COLOR_CONTROL, im::CMD_MOVE_TO_COLOR_TEMPERATURE));
        assert_eq!(fields.unwrap(), im::encode_move_to_color_temperature_fields(370, 0));
        assert_eq!(
            ct.sent_body(&["eth0".into()]),
            mat_core::body::group_color_temp_sent(10, 2702, 370, 0, 1, &["eth0".to_string()])
        );

        let lv = GroupOp { group_id: 10, endpoint: 1, kind: GroupOpKind::level(100, 0) };
        let (cluster, command, fields) = lv.kind.wire();
        assert_eq!((cluster, command), (im::CLUSTER_LEVEL_CONTROL, im::CMD_MOVE_TO_LEVEL));
        assert_eq!(fields.unwrap(), im::encode_move_to_level_fields(254, 0));

        let color = ResolvedColor { hue_raw: 180, sat_raw: 200, hue: 254, sat: 78, name: None, rgb: None };
        let c = GroupOp { group_id: 10, endpoint: 1, kind: GroupOpKind::Color { color: color.clone(), transition: 0 } };
        let (cluster, command, fields) = c.kind.wire();
        assert_eq!((cluster, command), (im::CLUSTER_COLOR_CONTROL, im::CMD_MOVE_TO_HUE_AND_SATURATION));
        assert_eq!(fields.unwrap(), im::encode_move_to_hue_and_saturation_fields(180, 200, 0));
        assert_eq!(c.sent_body(&[]), mat_core::body::group_color_sent(10, &color, 0, 1, &[]));

        let inv = GroupOp { group_id: 10, endpoint: 1, kind: GroupOpKind::invoke("onoff", "on", &[]).unwrap() };
        assert_eq!(inv.kind.wire(), (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None));
        assert_eq!(inv.sent_body(&[]), mat_core::body::group_invoke_sent(10, "onoff", "on", 1, &[]));
    }

    #[tokio::test]
    async fn group_op_hard_errors_when_engine_group_ctx_unconfigured() {
        use crate::test_support::FakeEstablisher;
        let engine = crate::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        let op = GroupOp { group_id: 10, endpoint: 1, kind: GroupOpKind::invoke("onoff", "toggle", &[]).unwrap() };
        let err = run_group_op(&engine, &op).await.expect_err("group ctx unconfigured must hard-error");
        assert_eq!(err.kind, ErrorKind::Other);
        let err = run_group_bump(&engine).await.expect_err("bump without ctx must hard-error");
        assert_eq!(err.kind, ErrorKind::Other);
    }

    #[tokio::test]
    async fn group_bump_advances_counter_via_engine() {
        // 旧 native_direct::tests::group_bump_advances_counter_via_engine の移植。
        use crate::group::GroupCtx;
        use crate::test_support::{write_group_fixture_ini, FakeEstablisher};
        use mat_controller::transport::UdpTransport;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        write_group_fixture_ini(&ini);
        let counter_path = dir.path().join("native_group_counter");
        let transport = Arc::new(UdpTransport::bind().await.unwrap());
        let group_ctx = GroupCtx {
            main_ini: ini,
            counter_path: counter_path.clone(),
            fabric_index: 2,
            fabric_id: 1,
            node_id: 0x0001_0001,
            egress: vec![mat_controller::group::GroupEgress { iface: "lo".into(), transport, scope_id: 1 }],
            dest_port: 5540,
            op_iface: "lo".into(),
            thread_retry: false,
            sender: Mutex::new(None),
        };
        let engine = crate::Engine::with_parts(Box::new(FakeEstablisher::default()), Some(group_ctx));
        assert!(!counter_path.exists());
        let body = run_group_bump(&engine).await.expect("bump must succeed when ctx is configured");
        assert!(body["group_counter"]["from"].is_number());
        assert!(body["group_counter"]["to"].is_number());
        assert!(counter_path.exists(), "counter file must be created/advanced by bump");
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-native op::tests`
Expected: コンパイルエラー（`wire` / `run_group_op` 未定義）。

- [ ] **Step 3: 実装**

`op.rs` の `run_node_op` の後に追加（use に `use crate::group::{self, BumpOutcome, GroupOutcome}; use crate::Engine;` を足す）:

```rust
impl GroupOpKind {
    /// 送出する (cluster, command, CommandFields TLV)。
    pub fn wire(&self) -> (u32, u32, Option<Vec<u8>>) {
        match self {
            GroupOpKind::Invoke { cluster, command, fields_tlv, .. } => {
                (*cluster, *command, fields_tlv.clone())
            }
            GroupOpKind::Color { color, transition } => (
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(im::encode_move_to_hue_and_saturation_fields(
                    color.hue_raw,
                    color.sat_raw,
                    *transition,
                )),
            ),
            GroupOpKind::ColorTemp { mireds, transition, .. } => (
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(im::encode_move_to_color_temperature_fields(*mireds, *transition)),
            ),
            GroupOpKind::Level { level, transition, .. } => (
                im::CLUSTER_LEVEL_CONTROL,
                im::CMD_MOVE_TO_LEVEL,
                Some(im::encode_move_to_level_fields(*level, *transition)),
            ),
        }
    }
}

impl GroupOp {
    /// 送出後の "sent" body。`egress` は実送出した iface 名。
    pub fn sent_body(&self, egress: &[String]) -> Value {
        match &self.kind {
            GroupOpKind::Invoke { cluster_in, command_in, .. } => {
                body::group_invoke_sent(self.group_id, cluster_in, command_in, self.endpoint, egress)
            }
            GroupOpKind::Color { color, transition } => {
                body::group_color_sent(self.group_id, color, *transition, self.endpoint, egress)
            }
            GroupOpKind::ColorTemp { kelvin, mireds, transition } => body::group_color_temp_sent(
                self.group_id, *kelvin, *mireds, *transition, self.endpoint, egress,
            ),
            GroupOpKind::Level { percent, level, transition } => body::group_level_sent(
                self.group_id,
                body::LevelEcho { percent: *percent, level: *level },
                *transition,
                self.endpoint,
                egress,
            ),
        }
    }
}

/// groupcast を 1 発送り "sent" body を返す。`engine.group` 未構成（テスト
/// 注入時のみ）は Other、未 provision / KVS 不備は `store_parse`。
pub async fn run_group_op(engine: &Engine, op: &GroupOp) -> Result<Value, MatError> {
    let Some(ctx) = &engine.group else {
        return Err(MatError::group_ctx_unconfigured());
    };
    let (cluster, command, fields) = op.kind.wire();
    match group::send(ctx, op.group_id, cluster, command, fields).await? {
        GroupOutcome::Sent { egress } => {
            tracing::debug!(group_id = op.group_id, op = op.kind.name(), "group op sent");
            Ok(op.sent_body(&egress))
        }
        GroupOutcome::Unavailable(reason) => Err(MatError::group_unavailable(&reason)),
    }
}

/// group 送信 counter の窓ジャンプ（Issue #14）。
pub async fn run_group_bump(engine: &Engine) -> Result<Value, MatError> {
    let Some(ctx) = &engine.group else {
        return Err(MatError::group_ctx_unconfigured());
    };
    match group::bump(ctx).await {
        BumpOutcome::Bumped { from, to } => Ok(body::group_bump(from, to)),
        BumpOutcome::Unavailable(reason) => Err(MatError::group_unavailable(&reason)),
    }
}
```

`tempfile` は mat-native の dev-dependency に既にある。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native op::tests && cargo clippy -p mat-native --all-targets -- -D warnings`
Expected: PASS（3 本追加）。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-native/src/op.rs
git commit -m "feat(native): run_group_op / run_group_bump — groupcast op の単一ソース（監査④ Task4）"
```

---
### Task 5: `mat_native::runner` — `NodeRunner` / `OneShotRunner` / `run_node` / `provision` / `grant`

**Files:**
- Create: `crates/mat-native/src/runner.rs`
- Modify: `crates/mat-native/src/lib.rs`（`pub mod runner;` を `pub mod op;` の直後に追加）

**Interfaces:**
- Consumes: Task 2–4 の `op::{NodeOp, ProvisionParams, run_node_op}`、`crate::ops::{provision_node, ensure_group_acl, epoch_key_from_hex, ProvisionNodeParams}`、`crate::group_settings::write_group_provision`、`mat_core::group::resolve_epoch_key`、`mat_core::body::{group_provision_success, group_grant_success}`、`crate::{Engine, NodeConn}`。
- Produces:

```rust
#[async_trait]
pub trait NodeRunner: Sync {
    async fn with_node<T, F>(&self, node_id: u64, deadline: Option<Instant>, f: F) -> Result<T, MatError>
    where
        T: Send,
        F: for<'a> Fn(&'a mut Box<dyn NodeConn>) -> Pin<Box<dyn Future<Output = Result<T, MatError>> + Send + 'a>> + Send + Sync;
}
pub struct OneShotRunner<'e, H: Fn(u64) + Send + Sync> { /* engine, after_close */ }
impl<'e, H: Fn(u64) + Send + Sync> OneShotRunner<'e, H> { pub fn new(engine: &'e Engine, after_close: H) -> Self }
pub async fn run_node(r: &impl NodeRunner, op: &NodeOp, deadline: Option<Instant>) -> Result<Value, MatError>;
pub async fn provision(r: &impl NodeRunner, engine: &Engine, p: &ProvisionParams, note: Option<&str>) -> Result<Value, MatError>;
pub async fn grant(r: &impl NodeRunner, group_id: u16, node_ids: &[u64]) -> Result<Value, MatError>;
```

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/runner.rs` を作り、テストを置く:

```rust
//! セッション取得戦略の差し替え点（監査④）。
//!
//! `mat`（確立 → 1 op → close、設計ルール 4）と `matd`（per-node warm slot、
//! Timeout で 1 回だけ再確立）の違いは `NodeRunner::with_node` の実装だけ。
//! その上の `run_node` / `provision` / `grant` は両経路共通。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::NodeOpKind;
    use crate::test_support::{FakeConn, FakeEstablisher};
    use mat_core::error::ErrorKind;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn read_onoff_op() -> NodeOp {
        NodeOp { node_id: 5, kind: NodeOpKind::read(1, "onoff", "on-off").unwrap() }
    }

    #[tokio::test]
    async fn one_shot_closes_and_calls_after_close_on_success() {
        let est = FakeEstablisher::default();
        let close_calls = Arc::clone(&est.conn_close_calls);
        let engine = Engine::with_parts(Box::new(est), None);
        let hinted = AtomicU64::new(0);
        let runner = OneShotRunner::new(&engine, |id| hinted.store(id, Ordering::SeqCst));
        let body = run_node(&runner, &NodeOp { node_id: 5, kind: NodeOpKind::On { endpoint: 1 } }, None)
            .await
            .unwrap();
        assert_eq!(body["status"], "success");
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hinted.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn one_shot_closes_on_failure_and_does_not_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let close_calls = Arc::clone(&est.conn_close_calls);
        let engine = Engine::with_parts(Box::new(est), None);
        let runner = OneShotRunner::new(&engine, |_| {});
        let err = run_node(&runner, &read_onoff_op(), None).await.expect_err("timeout must surface");
        assert_eq!(err.kind, ErrorKind::Timeout);
        // one-shot は再確立しない（確立直後の session が stale なことはない）。
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    }

    /// `ops::provision_node` / `ensure_group_acl` が読む group-key-map / acl に
    /// 妥当な JSON（空リスト／管理者エントリのみ）を返す establisher。
    struct ScriptedEstablisher;
    #[async_trait::async_trait]
    impl crate::Establisher for ScriptedEstablisher {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            Ok(Box::new(
                FakeConn::scripted()
                    .with_read(0, 0x003F, 0x0000, serde_json::json!([]))
                    .with_read(
                        0,
                        0x001F,
                        0x0000,
                        serde_json::json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]),
                    ),
            ))
        }
    }

    fn params(epoch_key: Option<String>) -> ProvisionParams {
        ProvisionParams {
            group_id: 99,
            node_ids: vec![5],
            keyset_id: 99,
            name: "e2e".into(),
            endpoint: 1,
            epoch_key,
            rebind: false,
        }
    }

    #[tokio::test]
    async fn provision_writes_controller_state_and_builds_body_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let mut engine = Engine::with_parts(Box::new(ScriptedEstablisher), None);
        engine.group_settings = Some(crate::group_settings::GroupSettingsCtx {
            main_ini: ini.clone(),
            fabric_index: 2,
            cfid: [7u8; 8],
        });
        let runner = OneShotRunner::new(&engine, |_| {});
        let body = provision(&runner, &engine, &params(Some("42".repeat(16))), Some("restart matd"))
            .await
            .unwrap();
        assert!(mat_controller::kvs::read_group_credentials(&ini, 2, 99).is_ok());
        assert_eq!(body["status"], "provisioned");
        assert_eq!(body["nodes"], serde_json::json!([5]));
        assert_eq!(body["note"], "restart matd");
        let body = provision(&runner, &engine, &params(None), None).await.unwrap();
        assert!(body.get("note").is_none());
    }

    #[tokio::test]
    async fn provision_hard_errors_when_group_settings_ctx_missing() {
        let engine = Engine::with_parts(Box::new(ScriptedEstablisher), None);
        let runner = OneShotRunner::new(&engine, |_| {});
        let err = provision(&runner, &engine, &params(None), None)
            .await
            .expect_err("missing group_settings ctx must hard-error");
        assert_eq!(err.kind, ErrorKind::Other);
    }

    #[tokio::test]
    async fn provision_prefixes_node_errors_with_node_id() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let est = FakeEstablisher {
            fail_first_send: true,
            fail_kind: ErrorKind::DeviceRejected,
            ..Default::default()
        };
        let mut engine = Engine::with_parts(Box::new(est), None);
        engine.group_settings = Some(crate::group_settings::GroupSettingsCtx {
            main_ini: ini,
            fabric_index: 2,
            cfid: [7u8; 8],
        });
        let runner = OneShotRunner::new(&engine, |_| {});
        let err = provision(&runner, &engine, &params(None), None).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        assert!(err.detail.starts_with("node 5: "), "{}", err.detail);
    }

    #[tokio::test]
    async fn grant_reports_updated_nodes() {
        let engine = Engine::with_parts(Box::new(ScriptedEstablisher), None);
        let runner = OneShotRunner::new(&engine, |_| {});
        let body = grant(&runner, 10, &[5]).await.unwrap();
        assert_eq!(body["status"], "granted");
        assert_eq!(body["updated"], serde_json::json!([5]));
        assert_eq!(body["unchanged"], serde_json::json!([]));
    }
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-native runner::tests`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装**

`runner.rs` のテストの前に:

```rust
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use mat_core::body;
use mat_core::error::MatError;

use crate::op::{run_node_op, NodeOp, ProvisionParams};
use crate::{Engine, NodeConn};

/// 「node_id のセッションを取り、`f` を呼ぶ」だけを抽象化する。`Fn` なのは
/// matd が Timeout 後に再確立して `f` を 2 回目に呼ぶため。closure 環境の
/// 借用は返す Future に持ち込めない（`'a` = conn の借用のみ）— 値は
/// `async move` ブロックへ move する。
#[async_trait]
pub trait NodeRunner: Sync {
    async fn with_node<T, F>(
        &self,
        node_id: u64,
        deadline: Option<Instant>,
        f: F,
    ) -> Result<T, MatError>
    where
        T: Send,
        F: for<'a> Fn(
                &'a mut Box<dyn NodeConn>,
            ) -> Pin<Box<dyn Future<Output = Result<T, MatError>> + Send + 'a>>
            + Send
            + Sync;
}

/// one-shot 直経路: 確立 → `f` → close → `after_close(node_id)`（`mat` は
/// ここで matd への node_touched ヒントを撃つ、Issue #20）。`deadline` は
/// 無視 — 直経路の予算は呼び出し側が future 全体に timeout を掛ける
/// （Issue #22 の「超過時も hint だけ撃つ」を保つ）。Timeout でも再確立しない。
pub struct OneShotRunner<'e, H: Fn(u64) + Send + Sync> {
    engine: &'e Engine,
    after_close: H,
}

impl<'e, H: Fn(u64) + Send + Sync> OneShotRunner<'e, H> {
    pub fn new(engine: &'e Engine, after_close: H) -> Self {
        Self { engine, after_close }
    }
}

#[async_trait]
impl<H: Fn(u64) + Send + Sync> NodeRunner for OneShotRunner<'_, H> {
    async fn with_node<T, F>(
        &self,
        node_id: u64,
        _deadline: Option<Instant>,
        f: F,
    ) -> Result<T, MatError>
    where
        T: Send,
        F: for<'a> Fn(
                &'a mut Box<dyn NodeConn>,
            ) -> Pin<Box<dyn Future<Output = Result<T, MatError>> + Send + 'a>>
            + Send
            + Sync,
    {
        let mut conn = self.engine.establisher.establish(node_id).await?;
        // 成否によらず close してから返す（Issue #20: 放置セッションは FP300 系の
        // 常駐購読を黙殺する）。
        let result = f(&mut conn).await;
        conn.close().await;
        (self.after_close)(node_id);
        result
    }
}

/// 単一ノード op を runner のセッション戦略で実行し、成功 body を返す。
pub async fn run_node(
    r: &impl NodeRunner,
    op: &NodeOp,
    deadline: Option<Instant>,
) -> Result<Value, MatError> {
    let op = op.clone();
    r.with_node(op.node_id, deadline, move |c| {
        let op = op.clone();
        Box::pin(async move { run_node_op(c.as_mut(), &op).await })
    })
    .await
}

/// `group provision`: コントローラ側 group state（KVS）→ 各ノードへデバイス側
/// 4 ステップ（unicast, acknowledged）。最初の失敗で停止する。`note` は経路
/// 依存の案内文（直経路 = KVS 直書き + matd 再起動案内、matd = None）。
/// provision は deadline 対象外（常に無制限）。
pub async fn provision(
    r: &impl NodeRunner,
    engine: &Engine,
    p: &ProvisionParams,
    note: Option<&str>,
) -> Result<Value, MatError> {
    let Some(gs) = &engine.group_settings else {
        return Err(MatError::group_ctx_unconfigured());
    };
    let epoch_key_hex = mat_core::group::resolve_epoch_key(p.epoch_key.as_deref())?;
    let epoch_key = crate::ops::epoch_key_from_hex(&epoch_key_hex)?;
    crate::group_settings::write_group_provision(
        gs, p.group_id, p.keyset_id, &p.name, &epoch_key, p.rebind,
    )?;
    for &node_id in &p.node_ids {
        let np = crate::ops::ProvisionNodeParams {
            group_id: p.group_id,
            keyset_id: p.keyset_id,
            name: p.name.clone(),
            endpoint: p.endpoint,
            epoch_key,
        };
        r.with_node(node_id, None, move |c| {
            let np = np.clone();
            Box::pin(async move { crate::ops::provision_node(c.as_mut(), &np).await })
        })
        .await
        .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
    }
    tracing::info!(group_id = p.group_id, keyset_id = p.keyset_id, "group provision executed");
    Ok(body::group_provision_success(
        p.group_id, p.keyset_id, &p.name, p.endpoint, &p.node_ids, note,
    ))
}

/// `group grant`: 各ノードへ ACL read-merge-write のみ。
pub async fn grant(
    r: &impl NodeRunner,
    group_id: u16,
    node_ids: &[u64],
) -> Result<Value, MatError> {
    let mut updated: Vec<u64> = Vec::new();
    let mut unchanged: Vec<u64> = Vec::new();
    for &node_id in node_ids {
        let changed = r
            .with_node(node_id, None, move |c| {
                Box::pin(async move { crate::ops::ensure_group_acl(c.as_mut(), group_id).await })
            })
            .await
            .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
        if changed {
            updated.push(node_id);
        } else {
            unchanged.push(node_id);
        }
    }
    tracing::info!(group_id, "group grant executed");
    Ok(body::group_grant_success(group_id, node_ids, &updated, &unchanged))
}
```

`c.as_mut()` が `&mut (dyn NodeConn + 'static)` にならず借用エラーになる場合は
`&mut **c` を使う。`async_trait` がジェネリック `T` / `F` に `'async_trait`
境界を要求するエラーが出たら、trait と impl の where 節に `T: 'async_trait, F: 'async_trait`
を足す（マクロ展開で導入されるライフタイム名）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native && cargo clippy -p mat-native --all-targets -- -D warnings`
Expected: PASS（runner 6 本追加、既存全緑）。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-native/src/runner.rs crates/mat-native/src/lib.rs
git commit -m "feat(native): NodeRunner / OneShotRunner と run_node・provision・grant の共有ループ（監査④ Task5）"
```

---

### Task 6: matd `NativeBackend` を `NodeRunner` に載せ替え、op 別メソッドを削除

**Files:**
- Modify: `crates/matd/src/native.rs`

**Interfaces:**
- Consumes: `mat_native::runner::NodeRunner`、`mat_native::Engine`。
- Produces: `impl NodeRunner for NativeBackend`（`with_node` = 既存 `with_session`）、`pub fn engine(&self) -> &mat_native::Engine`。削除: `read_onoff` / `on` / `off` / `color` / `color_temp` / `level` / `read_json` / `write_tlv` / `invoke_generic` / `describe` / `provision_node` / `group_invoke` / `group_bump`。

このタスク終了時点で `server.rs` はまだ旧メソッドを呼んでいるためコンパイルが
通らない。**Task 6 と Task 7 は同一コミットにまとめる**（Task 6 の Step 5 は
コミットせず Task 7 へ続く）。

- [ ] **Step 1: `native.rs` の op 別メソッドを削除し、trait impl を足す**

`impl NativeBackend` 内の `pub async fn read_onoff` から `pub async fn group_bump`
までのうち、`establish_subscription` **以外**を削除する（`drop_session` /
`with_session` / `with_session_inner` / `group_settings_ctx` / `set_on_new_session` /
`with_*` コンストラクタ / `establish_subscription` は残す）。不要になった
`use mat_controller::im::{...}` を消し、`use mat_native::runner::NodeRunner;`
と `use async_trait::async_trait;` を足す。

`impl NativeBackend` に追加:

```rust
    /// group 送信 / group_settings / 確立器を持つ共有エンジン（`mat_native::op`
    /// の group 系関数が受ける）。
    pub fn engine(&self) -> &mat_native::Engine {
        &self.engine
    }
```

`impl NativeBackend` の後に追加:

```rust
/// warm セッション戦略の差し替え点: `with_session`（per-node slot、Timeout で
/// 1 回だけ再確立、deadline 予算、`on_new_session` 発火）をそのまま使う。
#[async_trait]
impl NodeRunner for NativeBackend {
    async fn with_node<T, F>(
        &self,
        node_id: u64,
        deadline: Option<Instant>,
        f: F,
    ) -> Result<T, MatError>
    where
        T: Send,
        F: for<'a> Fn(
                &'a mut Box<dyn NodeConn>,
            ) -> Pin<Box<dyn std::future::Future<Output = Result<T, MatError>> + Send + 'a>>
            + Send
            + Sync,
    {
        self.with_session(node_id, deadline, f).await
    }
}
```

`with_session` の doc は残す（本体不変）。ファイル冒頭 doc の「read/write/
invoke/on/off/色/色温度/describe/group を in-process で処理する」を
「op の実体は `mat_native::op` / `runner`（mat one-shot と共有）。ここに残るのは
warm session を per-node に保持する責務（`NodeRunner` 実装）のみ」に改める。

- [ ] **Step 2: `native.rs` のテストを helper 経由に置き換える**

`mod tests` 冒頭に helper を追加し、`backend.read_onoff(A, B, C)` の全呼び出し
（19 テスト）を `read_onoff(&backend, A, B, C)` に機械置換する:

```rust
    /// 旧 `NativeBackend::read_onoff` 相当（with_session の挙動テスト用）。
    async fn read_onoff(
        b: &NativeBackend,
        node_id: u64,
        endpoint: u16,
        deadline: Option<Instant>,
    ) -> Result<bool, MatError> {
        b.with_node(node_id, deadline, move |c| c.read_onoff(endpoint))
            .await
    }
```

置換例: `backend.read_onoff(0x1234, 1, deadline)` → `read_onoff(&backend, 0x1234, 1, deadline)`。

- [ ] **Step 3: `cargo check -p matd --lib` で native.rs 単体の妥当性を見る**

Run: `cargo check -p matd --lib 2>&1 | grep -c "native.rs"`
Expected: `0`（残るエラーは server.rs 側のみ）。Task 7 へ続く（コミットしない）。

---

### Task 7: matd `server.rs` — `to_device_op` 1 本に集約

**Files:**
- Modify: `crates/matd/src/server.rs`

**Interfaces:**
- Consumes: `mat_native::op::{NodeOp, NodeOpKind, GroupOp, GroupOpKind, ProvisionParams, run_group_op, run_group_bump}`、`mat_native::runner::{run_node, provision}`、`NativeBackend::engine()`、`protocol::Op`（不変）。
- Produces（crate 内）:

```rust
pub(crate) enum MatdOp { Node(NodeOp), Group(GroupOp), Provision(ProvisionParams), Bump }
pub(crate) fn to_device_op(op: &Op) -> Result<MatdOp, MatError>;
```

削除: `is_native_hotpath` / `native_op` / `native_group_params` / `group_provision` / `SentBodyBuilder` / `GroupSendParams`。`run_op` のシグネチャは不変。

- [ ] **Step 1: 失敗するテストを書く（既存テストの移植）**

`server.rs` の `mod tests` で次を行う:

1. `hotpath_routing_selects_native_ops` / `hotpath_routing_rejects_unresolved_names` /
   `native_group_params_maps_onoff_and_shortcuts` / `generic_ops_join_the_native_hotpath` /
   `native_op_invariant_violations_are_typed_errors_not_panics` /
   `group_provision_rejects_non_group_provision_op_without_panic` を削除し、代わりに:

```rust
    use mat_native::op::{GroupOpKind, NodeOpKind};

    #[test]
    fn to_device_op_maps_node_ops_with_resolved_ids() {
        let m = to_device_op(&Op::On { node_id: 1, endpoint: 1 }).unwrap();
        assert!(matches!(m, MatdOp::Node(ref n) if n.node_id == 1 && n.kind == NodeOpKind::On { endpoint: 1 }));
        let m = to_device_op(&Op::ColorTemp { node_id: 1, endpoint: 1, mireds: 370, kelvin: 2700, transition: 0 })
            .unwrap();
        assert!(matches!(
            m,
            MatdOp::Node(ref n) if n.kind == NodeOpKind::ColorTemp { endpoint: 1, kelvin: 2700, mireds: 370, transition: 0 }
        ));
        let m = to_device_op(&Op::Level { node_id: 1, endpoint: 1, level: 127, percent: 50, transition: 0 }).unwrap();
        assert!(matches!(m, MatdOp::Node(ref n) if n.kind == NodeOpKind::Level { endpoint: 1, percent: 50, level: 127, transition: 0 }));
        let m = to_device_op(&Op::Color {
            node_id: 1, endpoint: 1, hue_raw: 0, saturation_raw: 254, hue: 0, saturation: 100,
            name: Some("red".into()), rgb: Some("#ff0000".into()), transition: 0,
        })
        .unwrap();
        match m {
            MatdOp::Node(n) => match n.kind {
                NodeOpKind::Color { color, .. } => {
                    assert_eq!((color.hue_raw, color.sat_raw, color.hue, color.sat), (0, 254, 0, 100));
                    assert_eq!(color.name.as_deref(), Some("red"));
                    assert_eq!(color.rgb.as_deref(), Some("#ff0000"));
                }
                other => panic!("expected Color, got {other:?}"),
            },
            other => panic!("expected Node, got {other:?}"),
        }
        let m = to_device_op(&Op::Read { node_id: 5, endpoint: 1, cluster: "levelcontrol".into(), attribute: "current-level".into() })
            .unwrap();
        assert!(matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Read { cluster: 0x0008, attribute: 0, .. })));
        let m = to_device_op(&Op::Write { node_id: 5, endpoint: 1, cluster: "levelcontrol".into(), attribute: "on-level".into(), value: "128".into() })
            .unwrap();
        assert!(matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Write { .. })));
        let m = to_device_op(&Op::Invoke {
            node_id: 5, endpoint: 1, cluster: "levelcontrol".into(), command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
        })
        .unwrap();
        assert!(matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Invoke { fields_tlv: Some(_), .. })));
        assert!(matches!(to_device_op(&Op::Describe { node_id: 5 }).unwrap(), MatdOp::Node(ref n) if n.kind == NodeOpKind::Describe));
    }

    #[test]
    fn to_device_op_rejects_unresolved_names_and_unencodable_values() {
        // 未知名 → unresolved_op（parse_error、数値 ID 案内付き）。
        let err = to_device_op(&Op::Read { node_id: 1, endpoint: 1, cluster: "nosuchcluster".into(), attribute: "x".into() })
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("numeric IDs are accepted"), "{}", err.detail);
        let err = to_device_op(&Op::Invoke { node_id: 1, endpoint: 1, cluster: "nosuchcluster".into(), command: "x".into(), args: vec![] })
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        // 名前は解決できるが list 型 → parse_error（classify の msg）。
        let err = to_device_op(&Op::Write { node_id: 1, endpoint: 1, cluster: "accesscontrol".into(), attribute: "acl".into(), value: "[]".into() })
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("list"), "{}", err.detail);
    }

    #[test]
    fn to_device_op_maps_group_ops_and_shortcuts() {
        let m = to_device_op(&group_on_op()).unwrap();
        match m {
            MatdOp::Group(g) => {
                assert_eq!((g.group_id, g.endpoint), (10, 1));
                assert_eq!(g.kind.wire(), (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None));
            }
            other => panic!("expected Group, got {other:?}"),
        }
        // 引数過多（onoff on は 0 引数）は即 parse_error。
        let err = to_device_op(&Op::GroupInvoke { group_id: 10, cluster: "onoff".into(), command: "on".into(), args: vec!["1".into()], endpoint: 1 })
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        // 未知コマンド名は unresolved_op。
        let err = to_device_op(&Op::GroupInvoke { group_id: 10, cluster: "onoff".into(), command: "foo".into(), args: vec![], endpoint: 1 })
            .unwrap_err();
        assert!(err.detail.contains("numeric IDs are accepted"), "{}", err.detail);

        let m = to_device_op(&Op::GroupColorTemp { group_id: 10, mireds: 370, kelvin: 2702, transition: 0, endpoint: 1 }).unwrap();
        assert!(matches!(m, MatdOp::Group(ref g) if g.kind == GroupOpKind::ColorTemp { kelvin: 2702, mireds: 370, transition: 0 }));
        let m = to_device_op(&Op::GroupLevel { group_id: 10, level: 254, percent: 100, transition: 0, endpoint: 1 }).unwrap();
        assert!(matches!(m, MatdOp::Group(ref g) if g.kind == GroupOpKind::Level { percent: 100, level: 254, transition: 0 }));
        let m = to_device_op(&Op::GroupColor {
            group_id: 10, hue_raw: 180, saturation_raw: 200, hue: 254, saturation: 78,
            name: None, rgb: None, transition: 0, endpoint: 1,
        })
        .unwrap();
        assert!(matches!(m, MatdOp::Group(ref g) if matches!(g.kind, GroupOpKind::Color { .. })));
        assert!(matches!(to_device_op(&Op::GroupBump).unwrap(), MatdOp::Bump));
        let m = to_device_op(&Op::GroupProvision {
            group_id: 7, node_ids: vec![1, 2], keyset_id: 42, name: "grp7".into(),
            endpoint: 1, epoch_key: None, rebind: true,
        })
        .unwrap();
        assert!(matches!(m, MatdOp::Provision(ref p) if p.group_id == 7 && p.node_ids == vec![1, 2] && p.rebind));
    }

    /// dispatch 不変条件が破れても panic しない（v1 Task6 規律）。
    #[test]
    fn to_device_op_rejects_non_device_ops_without_panic() {
        for op in [Op::Ping, Op::Status, Op::Shutdown, Op::NodeTouched { node_id: 1 },
                   Op::Listen { node_id: None, endpoint: None, cluster: None, attribute: None }] {
            let err = to_device_op(&op).unwrap_err();
            assert_eq!(err.kind, ErrorKind::ParseError);
            assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);
        }
    }
```

2. `native_op(&X, &native, PATH, None)` を直接呼ぶ 3 テスト
   （`native_generic_read_body_matches_expected_schema` /
   `native_write_rejects_list_type_with_parse_error` /
   `native_generic_invoke_and_describe_bodies_match_expected_schema`）と
   `group_provision(&op, &native, &store_path)` を呼ぶ 2 テスト
   （`group_provision_writes_controller_and_device_state_natively` /
   `group_provision_without_group_settings_ctx_is_internal_error`）は、呼び出しを
   `run_op` 経由に書き換える:

```rust
        // 旧: native_op(&op, &native, dir.path(), None).await
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let body = run_op(&op, &state, dir.path(), &health, None).await.unwrap();
```

（`native` を `Box` に move するので、同一テスト内で複数回呼ぶ場合は
最初に `state` を作ってから使い回す。）

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p matd --lib server::tests::to_device_op 2>&1 | head`
Expected: コンパイルエラー（`to_device_op` / `MatdOp` 未定義）。

- [ ] **Step 3: 実装**

`server.rs` で `is_native_hotpath` / `native_op` / `native_group_params` /
`group_provision` / `SentBodyBuilder` / `GroupSendParams` を削除し、
`run_op` の native 取得以降を次に置き換える:

```rust
    let native = match native {
        NativeState::Ready(n) => n,
        NativeState::Unavailable(e) => return Err(e.clone()),
    };

    match to_device_op(op)? {
        MatdOp::Node(node_op) => {
            // commission 済みか毎回 KVS で確認する。
            require_node(store_path, node_op.node_id)?;
            let body = mat_native::runner::run_node(native.as_ref(), &node_op, deadline).await?;
            // 前提: デバイスは invoke 応答を先に、購読 report を後に送る。
            // report が note_op より先に pump へ届く逆順だと pending が残り
            // 健全購読を 1 回余分に再購読するが、それが最悪ケース。
            note_op_expectation(op, health);
            Ok(body)
        }
        MatdOp::Group(group_op) => {
            // chip-tool 撤去前と同じ前提チェック（store が開けること）。
            let _store = Store::open(store_path)?;
            mat_native::op::run_group_op(native.engine(), &group_op).await
        }
        MatdOp::Provision(p) => {
            let store = Store::open(store_path)?;
            // 全ノードが commission 済みか先に確認（1つでも未登録なら停止）。
            for &node_id in &p.node_ids {
                store.require_node(node_id)?;
            }
            // matd 経路の provision は note 無し（KVS は matd 自身が書くため
            // 再起動案内は不要）。
            mat_native::runner::provision(native.as_ref(), native.engine(), &p, None).await
        }
        MatdOp::Bump => {
            let _store = Store::open(store_path)?;
            mat_native::op::run_group_bump(native.engine()).await
        }
    }
}

/// wire `Op` → 解決済み op。名前解決・引数符号化の規則は `mat_native::op` の
/// コンストラクタ（mat 直経路と同一）。Ping / Shutdown / Listen / Status /
/// NodeTouched は `run_op` 冒頭 / `dispatch` / `handle_conn` が先取りするため
/// ここへは来ない（不変条件が破れても panic せず typed error）。
#[derive(Debug)]
pub(crate) enum MatdOp {
    Node(mat_native::op::NodeOp),
    Group(mat_native::op::GroupOp),
    Provision(mat_native::op::ProvisionParams),
    Bump,
}

pub(crate) fn to_device_op(op: &Op) -> Result<MatdOp, MatError> {
    use mat_native::op::{GroupOp, GroupOpKind, NodeOp, NodeOpKind, ProvisionParams};
    let node = |node_id: u64, kind: NodeOpKind| MatdOp::Node(NodeOp { node_id, kind });
    Ok(match op {
        Op::Read { node_id, endpoint, cluster, attribute } => {
            node(*node_id, NodeOpKind::read(*endpoint, cluster, attribute)?)
        }
        Op::Write { node_id, endpoint, cluster, attribute, value } => {
            node(*node_id, NodeOpKind::write(*endpoint, cluster, attribute, value)?)
        }
        Op::Invoke { node_id, endpoint, cluster, command, args } => {
            node(*node_id, NodeOpKind::invoke(*endpoint, cluster, command, args)?)
        }
        Op::On { node_id, endpoint } => node(*node_id, NodeOpKind::On { endpoint: *endpoint }),
        Op::Off { node_id, endpoint } => node(*node_id, NodeOpKind::Off { endpoint: *endpoint }),
        // 換算済み値が wire で届く（protocol.rs の約束）— struct リテラルで組む。
        Op::ColorTemp { node_id, endpoint, mireds, kelvin, transition } => node(
            *node_id,
            NodeOpKind::ColorTemp { endpoint: *endpoint, kelvin: *kelvin, mireds: *mireds, transition: *transition },
        ),
        Op::Level { node_id, endpoint, level, percent, transition } => node(
            *node_id,
            NodeOpKind::Level { endpoint: *endpoint, percent: *percent, level: *level, transition: *transition },
        ),
        Op::Color { node_id, endpoint, hue_raw, saturation_raw, hue, saturation, name, rgb, transition } => node(
            *node_id,
            NodeOpKind::Color {
                endpoint: *endpoint,
                color: mat_core::color::ResolvedColor {
                    hue_raw: *hue_raw,
                    sat_raw: *saturation_raw,
                    hue: *hue,
                    sat: *saturation,
                    name: name.clone(),
                    rgb: rgb.clone(),
                },
                transition: *transition,
            },
        ),
        Op::Describe { node_id } => node(*node_id, NodeOpKind::Describe),
        Op::GroupProvision { group_id, node_ids, keyset_id, name, endpoint, epoch_key, rebind } => {
            MatdOp::Provision(ProvisionParams {
                group_id: *group_id,
                node_ids: node_ids.clone(),
                keyset_id: *keyset_id,
                name: name.clone(),
                endpoint: *endpoint,
                epoch_key: epoch_key.clone(),
                rebind: *rebind,
            })
        }
        Op::GroupInvoke { group_id, cluster, command, args, endpoint } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::invoke(cluster, command, args)?,
        }),
        Op::GroupColorTemp { group_id, mireds, kelvin, transition, endpoint } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::ColorTemp { kelvin: *kelvin, mireds: *mireds, transition: *transition },
        }),
        Op::GroupLevel { group_id, level, percent, transition, endpoint } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::Level { percent: *percent, level: *level, transition: *transition },
        }),
        Op::GroupColor { group_id, hue_raw, saturation_raw, hue, saturation, name, rgb, transition, endpoint } => {
            MatdOp::Group(GroupOp {
                group_id: *group_id,
                endpoint: *endpoint,
                kind: GroupOpKind::Color {
                    color: mat_core::color::ResolvedColor {
                        hue_raw: *hue_raw,
                        sat_raw: *saturation_raw,
                        hue: *hue,
                        sat: *saturation,
                        name: name.clone(),
                        rgb: rgb.clone(),
                    },
                    transition: *transition,
                },
            })
        }
        Op::GroupBump => MatdOp::Bump,
        Op::Listen { .. } | Op::Ping | Op::Status | Op::Shutdown | Op::NodeTouched { .. } => {
            return Err(MatError::parse_error(
                "internal: non-device op reached to_device_op (dispatch invariant violated)",
            ))
        }
    })
}
```

`run_op` の doc コメント（「M8c-3: native が唯一の経路 …」）の末尾に
「名前解決・値符号化は `to_device_op` → `mat_native::op` に集約（監査④）。
未解決名は `require_node` より先に `parse_error` になる（mat 直経路と同順）」を足す。
不要になった `use mat_controller::im` の項目（`CLUSTER_*` / `CMD_*` /
`encode_*`）は消す（`op_report_expectation` が使う `im::CLUSTER_ON_OFF` 等は残る）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd && cargo clippy -p matd --all-targets -- -D warnings`
Expected: PASS（`tests/integration.rs` 含む全緑）。

- [ ] **Step 5: Commit（Task 6 と合わせて）**

```bash
git add crates/matd/src/native.rs crates/matd/src/server.rs
git commit -m "refactor(matd): op 実行を mat_native::op / runner へ委譲 — NativeBackend は NodeRunner 実装のみ（監査④ Task6-7）"
```

---
### Task 8: `mat::device_op` — `Command` → `DeviceOp` の match 1 本

**Files:**
- Create: `crates/mat/src/device_op.rs`
- Modify: `crates/mat/src/main.rs`（`mod device_op;` を `mod cli;` の直後に追加。まだ呼ばない）

**Interfaces:**
- Consumes: `crate::cli::{Command, DiagCommand, GroupCommand, ColorSpecArgs}`、`mat_core::alias::{NodeRef, GroupRef, EndpointRef}`（`id()`）、`mat_core::color::resolve_spec`、`mat_native::op::{NodeOp, NodeOpKind, GroupOp, GroupOpKind, ProvisionParams}`。
- Produces:

```rust
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeviceOp {
    Node(NodeOp),
    Group(GroupOp),
    GroupProvision(ProvisionParams),
    GroupGrant { group_id: u16, node_ids: Vec<u64> },
    GroupBump,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Dispatch { Device(DeviceOp), Dedicated(&'static str) }
pub(crate) fn classify(command: &Command) -> Result<Dispatch, MatError>;
pub(crate) fn resolve_discriminator(node_id: u64, discriminator: Option<u16>) -> u16;
impl DeviceOp { pub(crate) fn name(&self) -> &'static str }  // ログ用
```

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/src/device_op.rs`:

```rust
//! clap `Command` → 解決済み device op（監査④）。
//!
//! `resolve::resolve_command` の後段。alias は数値確定済みで届く（未解決は
//! 内部バグとして typed error）。名前解決・単位換算・color spec 解決はここで
//! 1 回だけ行い、以降の経路（matd wire / 直経路）には `DeviceOp` だけが流れる。
//! 専用コマンド層を持つ op（discover / commission / fabric / listen / diag node /
//! diag mesh）は `Dispatch::Dedicated(name)`（name は `--matd` 強制時の
//! unsupported 文言用）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ColorSpecArgs, DiagCommand, GroupCommand};
    use mat_core::alias::{EndpointRef, GroupRef, NodeRef};
    use mat_core::error::ErrorKind;
    use mat_native::op::{GroupOpKind, NodeOpKind};

    fn node_op(c: &Command) -> NodeOp {
        match classify(c).unwrap() {
            Dispatch::Device(DeviceOp::Node(n)) => n,
            other => panic!("expected Node, got {other:?}"),
        }
    }

    fn group_op(c: &Command) -> GroupOp {
        match classify(c).unwrap() {
            Dispatch::Device(DeviceOp::Group(g)) => g,
            other => panic!("expected Group, got {other:?}"),
        }
    }

    #[test]
    fn on_off_read_shapes() {
        let on = Command::On { node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1) };
        assert_eq!(node_op(&on), NodeOp { node_id: 5, kind: NodeOpKind::On { endpoint: 1 } });
        let off = Command::Off { node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1) };
        assert_eq!(node_op(&off).kind, NodeOpKind::Off { endpoint: 1 });
        let read = Command::Read {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(), attribute: "current-level".into(),
        };
        assert!(matches!(node_op(&read).kind, NodeOpKind::Read { cluster: 0x0008, attribute: 0, .. }));
        let byid = Command::Read {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            cluster: "0x0008".into(), attribute: "0".into(),
        };
        assert!(matches!(node_op(&byid).kind, NodeOpKind::Read { .. }));
        // 未知名は parse_error（旧 classify None → unresolved_op_error と同じ kind）。
        let unknown = Command::Read {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            cluster: "nosuchcluster".into(), attribute: "x".into(),
        };
        let err = classify(&unknown).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("numeric IDs are accepted"), "{}", err.detail);
    }

    #[test]
    fn unresolved_alias_is_internal_error() {
        let on = Command::On { node_id: NodeRef::Alias("kitchen".into()), endpoint: EndpointRef::Id(1) };
        let err = classify(&on).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("kitchen"), "{}", err.detail);
    }

    #[test]
    fn color_temp_level_color_convert_units() {
        let ct = Command::ColorTemp {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            kelvin: Some(2700), mireds: None, transition: 0,
        };
        assert_eq!(node_op(&ct).kind, NodeOpKind::ColorTemp { endpoint: 1, kelvin: 2700, mireds: 370, transition: 0 });
        let lv = Command::Level { node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1), percent: 50, transition: 3 };
        assert_eq!(node_op(&lv).kind, NodeOpKind::Level { endpoint: 1, percent: 50, level: 127, transition: 3 });
        // classify は resolve_command 後に呼ばれるため name は rgb 解決済みで届く。
        let c = Command::Color {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            spec: ColorSpecArgs { name: Some("red".into()), rgb: Some("#ff0000".into()), hue: None, sat: None },
            transition: 0,
        };
        match node_op(&c).kind {
            NodeOpKind::Color { color, .. } => {
                assert_eq!((color.hue_raw, color.sat_raw, color.hue, color.sat), (0, 254, 0, 100));
                assert_eq!(color.name.as_deref(), Some("red"));
            }
            other => panic!("expected Color, got {other:?}"),
        }
        // 不正 rgb は resolve_spec のエラーがそのまま出る。
        let bad = Command::Color {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            spec: ColorSpecArgs { name: None, rgb: Some("zzz".into()), hue: None, sat: None },
            transition: 0,
        };
        assert!(classify(&bad).is_err());
    }

    #[test]
    fn write_and_invoke_reject_unencodable_values() {
        let w = Command::Write {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(), attribute: "on-level".into(), value: "128".into(),
        };
        assert!(matches!(node_op(&w).kind, NodeOpKind::Write { .. }));
        let acl = Command::Write {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            cluster: "accesscontrol".into(), attribute: "acl".into(), value: "[]".into(),
        };
        let err = classify(&acl).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("list"), "{}", err.detail);
        let inv = Command::Invoke {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(), command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
        };
        assert!(matches!(node_op(&inv).kind, NodeOpKind::Invoke { fields_tlv: Some(_), .. }));
        let ks = Command::Invoke {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            cluster: "groupkeymanagement".into(), command: "key-set-write".into(), args: vec!["{}".into()],
        };
        assert_eq!(classify(&ks).unwrap_err().kind, ErrorKind::ParseError);
    }

    #[test]
    fn describe_diag_thread_open_window_and_dedicated() {
        let d = Command::Describe { node_id: NodeRef::Id(5) };
        assert_eq!(node_op(&d), NodeOp { node_id: 5, kind: NodeOpKind::Describe });
        let t = Command::Diag { action: DiagCommand::Thread { node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(0) } };
        assert_eq!(node_op(&t).kind, NodeOpKind::DiagThread { endpoint: 0 });
        let ow = Command::OpenWindow { node_id: NodeRef::Id(5), timeout: 180, iteration: 1000, discriminator: Some(3840) };
        assert_eq!(node_op(&ow).kind, NodeOpKind::OpenWindow { timeout: 180, iteration: 1000, discriminator: 3840 });
        // discriminator 未指定は node_id % 4096 で決定的に補完。
        let ow = Command::OpenWindow { node_id: NodeRef::Id(4101), timeout: 180, iteration: 1000, discriminator: None };
        assert!(matches!(node_op(&ow).kind, NodeOpKind::OpenWindow { discriminator: 5, .. }));

        assert_eq!(classify(&Command::Discover { probe: false }).unwrap(), Dispatch::Dedicated("discover"));
        let dn = Command::Diag { action: DiagCommand::Node { node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(0), deep: false } };
        assert_eq!(classify(&dn).unwrap(), Dispatch::Dedicated("diag"));
        let dm = Command::Diag { action: DiagCommand::Mesh { nodes: vec![] } };
        assert_eq!(classify(&dm).unwrap(), Dispatch::Dedicated("diag"));
    }

    #[test]
    fn group_ops_provision_grant_bump() {
        let toggle = Command::Group { action: GroupCommand::Invoke {
            group_id: GroupRef::Id(10), cluster: "onoff".into(), command: "toggle".into(), args: vec![], endpoint: 1,
        } };
        let g = group_op(&toggle);
        assert_eq!((g.group_id, g.endpoint), (10, 1));
        assert_eq!(g.kind.wire(), (mat_controller::im::CLUSTER_ON_OFF, mat_controller::im::CMD_ON_OFF_TOGGLE, None));
        let generic = Command::Group { action: GroupCommand::Invoke {
            group_id: GroupRef::Id(10), cluster: "levelcontrol".into(), command: "move-to-level".into(),
            args: vec!["128".into()], endpoint: 1,
        } };
        assert!(matches!(group_op(&generic).kind, GroupOpKind::Invoke { fields_tlv: Some(_), .. }));
        let ct = Command::Group { action: GroupCommand::ColorTemp {
            group_id: GroupRef::Id(10), kelvin: Some(2700), mireds: None, transition: 0, endpoint: 1,
        } };
        assert_eq!(group_op(&ct).kind, GroupOpKind::ColorTemp { kelvin: 2700, mireds: 370, transition: 0 });
        let lv = Command::Group { action: GroupCommand::Level { group_id: GroupRef::Id(10), percent: 50, transition: 0, endpoint: 1 } };
        assert_eq!(group_op(&lv).kind, GroupOpKind::Level { percent: 50, level: 127, transition: 0 });
        let color = Command::Group { action: GroupCommand::Color {
            group_id: GroupRef::Id(10),
            spec: ColorSpecArgs { name: None, rgb: Some("#ff0000".into()), hue: None, sat: None },
            transition: 0, endpoint: 1,
        } };
        assert!(matches!(group_op(&color).kind, GroupOpKind::Color { .. }));

        let grant = Command::Group { action: GroupCommand::Grant { group_id: GroupRef::Id(10), node_ids: vec![NodeRef::Id(5), NodeRef::Id(6)] } };
        assert_eq!(
            classify(&grant).unwrap(),
            Dispatch::Device(DeviceOp::GroupGrant { group_id: 10, node_ids: vec![5, 6] })
        );
        let provision = Command::Group { action: GroupCommand::Provision {
            group_id: GroupRef::Id(10), node_ids: vec![NodeRef::Id(5)], keyset_id: 60,
            name: None, endpoint: 1, epoch_key: None, rebind: false,
        } };
        assert_eq!(
            classify(&provision).unwrap(),
            Dispatch::Device(DeviceOp::GroupProvision(ProvisionParams {
                group_id: 10, node_ids: vec![5], keyset_id: 60, name: "grp10".into(),
                endpoint: 1, epoch_key: None, rebind: false,
            }))
        );
        assert_eq!(
            classify(&Command::Group { action: GroupCommand::Bump }).unwrap(),
            Dispatch::Device(DeviceOp::GroupBump)
        );
    }
}
```

`main.rs` に `mod device_op;` を追加。

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat --bin mat device_op::tests 2>&1 | head`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装**

`device_op.rs` のテストの前に:

```rust
use crate::cli::{Command, DiagCommand, GroupCommand};
use mat_core::alias::NodeRef;
use mat_core::error::MatError;
use mat_native::op::{GroupOp, GroupOpKind, NodeOp, NodeOpKind, ProvisionParams};

/// 解決済み device op（直経路 / matd wire の共通入力）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeviceOp {
    Node(NodeOp),
    Group(GroupOp),
    GroupProvision(ProvisionParams),
    /// 直経路専用（matd プロトコルに op を足さない — 稀な修復操作で warm
    /// session の恩恵が小さく、mat/matd のバージョンスキューにも安全）。
    GroupGrant { group_id: u16, node_ids: Vec<u64> },
    GroupBump,
}

impl DeviceOp {
    /// ログ用の op 名。
    pub(crate) fn name(&self) -> &'static str {
        match self {
            DeviceOp::Node(n) => n.kind.name(),
            DeviceOp::Group(g) => g.kind.name(),
            DeviceOp::GroupProvision(_) => "group_provision",
            DeviceOp::GroupGrant { .. } => "group_grant",
            DeviceOp::GroupBump => "group_bump",
        }
    }
}

/// `classify` の結果。`Dedicated(name)` = 専用コマンド層を持つ op。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Dispatch {
    Device(DeviceOp),
    Dedicated(&'static str),
}

/// `mat open-window` の discriminator 未指定時の決定的補完（12-bit に収める）。
pub(crate) fn resolve_discriminator(node_id: u64, discriminator: Option<u16>) -> u16 {
    discriminator.unwrap_or((node_id % 4096) as u16)
}

/// `Command` の網羅 match（`_` 無し）: 新しいサブコマンドを足すとここが
/// コンパイルエラーになり、経路割当の考慮漏れを防ぐ。`Err` = 未解決 alias
/// （内部バグ）/ 名前未解決 / 値符号化不能 / 不正 color spec。
pub(crate) fn classify(command: &Command) -> Result<Dispatch, MatError> {
    fn node(node_id: &NodeRef, kind: NodeOpKind) -> Result<Dispatch, MatError> {
        Ok(Dispatch::Device(DeviceOp::Node(NodeOp { node_id: node_id.id()?, kind })))
    }
    fn color(spec: &crate::cli::ColorSpecArgs) -> Result<mat_core::color::ResolvedColor, MatError> {
        mat_core::color::resolve_spec(spec.name.as_deref(), spec.rgb.as_deref(), spec.hue, spec.sat)
    }
    fn nodes(ids: &[NodeRef]) -> Result<Vec<u64>, MatError> {
        ids.iter().map(NodeRef::id).collect()
    }

    match command {
        Command::Discover { .. } => Ok(Dispatch::Dedicated("discover")),
        Command::Commission { .. } => Ok(Dispatch::Dedicated("commission")),
        Command::Fabric { .. } => Ok(Dispatch::Dedicated("fabric")),
        Command::Listen { .. } => Ok(Dispatch::Dedicated("listen")),
        Command::Diag { action: DiagCommand::Node { .. } | DiagCommand::Mesh { .. } } => {
            Ok(Dispatch::Dedicated("diag"))
        }
        Command::Read { node_id, endpoint, cluster, attribute } => {
            node(node_id, NodeOpKind::read(endpoint.id()?, cluster, attribute)?)
        }
        Command::Write { node_id, endpoint, cluster, attribute, value } => {
            node(node_id, NodeOpKind::write(endpoint.id()?, cluster, attribute, value)?)
        }
        Command::Invoke { node_id, endpoint, cluster, command, args } => {
            node(node_id, NodeOpKind::invoke(endpoint.id()?, cluster, command, args)?)
        }
        Command::Describe { node_id } => node(node_id, NodeOpKind::Describe),
        Command::On { node_id, endpoint } => node(node_id, NodeOpKind::On { endpoint: endpoint.id()? }),
        Command::Off { node_id, endpoint } => node(node_id, NodeOpKind::Off { endpoint: endpoint.id()? }),
        Command::ColorTemp { node_id, endpoint, kelvin, mireds, transition } => node(
            node_id,
            NodeOpKind::color_temp(endpoint.id()?, *kelvin, *mireds, *transition),
        ),
        Command::Level { node_id, endpoint, percent, transition } => {
            node(node_id, NodeOpKind::level(endpoint.id()?, *percent, *transition))
        }
        Command::Color { node_id, endpoint, spec, transition } => node(
            node_id,
            NodeOpKind::Color { endpoint: endpoint.id()?, color: color(spec)?, transition: *transition },
        ),
        Command::OpenWindow { node_id, timeout, iteration, discriminator } => {
            let nid = node_id.id()?;
            node(
                node_id,
                NodeOpKind::OpenWindow {
                    timeout: *timeout,
                    iteration: *iteration,
                    discriminator: resolve_discriminator(nid, *discriminator),
                },
            )
        }
        Command::Diag { action: DiagCommand::Thread { node_id, endpoint } } => {
            node(node_id, NodeOpKind::DiagThread { endpoint: endpoint.id()? })
        }
        Command::Group { action } => Ok(Dispatch::Device(match action {
            GroupCommand::Provision { group_id, node_ids, keyset_id, name, endpoint, epoch_key, rebind } => {
                let gid = group_id.id()?;
                DeviceOp::GroupProvision(ProvisionParams {
                    group_id: gid,
                    node_ids: nodes(node_ids)?,
                    keyset_id: *keyset_id,
                    // name 未指定は group_id から決定的に補完（両経路共通の規則）。
                    name: name.clone().unwrap_or_else(|| format!("grp{gid}")),
                    endpoint: *endpoint,
                    epoch_key: epoch_key.clone(),
                    rebind: *rebind,
                })
            }
            GroupCommand::Invoke { group_id, cluster, command, args, endpoint } => DeviceOp::Group(GroupOp {
                group_id: group_id.id()?,
                endpoint: *endpoint,
                kind: GroupOpKind::invoke(cluster, command, args)?,
            }),
            GroupCommand::ColorTemp { group_id, kelvin, mireds, transition, endpoint } => DeviceOp::Group(GroupOp {
                group_id: group_id.id()?,
                endpoint: *endpoint,
                kind: GroupOpKind::color_temp(*kelvin, *mireds, *transition),
            }),
            GroupCommand::Level { group_id, percent, transition, endpoint } => DeviceOp::Group(GroupOp {
                group_id: group_id.id()?,
                endpoint: *endpoint,
                kind: GroupOpKind::level(*percent, *transition),
            }),
            GroupCommand::Color { group_id, spec, transition, endpoint } => DeviceOp::Group(GroupOp {
                group_id: group_id.id()?,
                endpoint: *endpoint,
                kind: GroupOpKind::Color { color: color(spec)?, transition: *transition },
            }),
            GroupCommand::Grant { group_id, node_ids } => DeviceOp::GroupGrant {
                group_id: group_id.id()?,
                node_ids: nodes(node_ids)?,
            },
            GroupCommand::Bump => DeviceOp::GroupBump,
        })),
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat --bin mat device_op::tests && cargo clippy -p mat --all-targets -- -D warnings`
Expected: PASS（6 本）。dead_code 警告が出る場合は Task 9–10 で解消されるので、
このタスクでは `#[allow(dead_code)]` を `pub(crate) fn classify` と
`DeviceOp::name` に一時付与し、Task 10 で外す。

- [ ] **Step 5: Commit**

```bash
git add crates/mat/src/device_op.rs crates/mat/src/main.rs
git commit -m "feat(mat): device_op::classify — Command→DeviceOp の match 1 本（監査④ Task8）"
```

---

### Task 9: `matd_client` を `DeviceOp` 入力にし、`main.rs` の経路分岐を `Dispatch` に

**Files:**
- Modify: `crates/mat/src/matd_client.rs`
- Modify: `crates/mat/src/main.rs`

**Interfaces:**
- Consumes: Task 8 の `device_op::{classify, Dispatch, DeviceOp}`、`mat_native::op::{NodeOpKind, GroupOpKind}`。
- Produces:
  - `fn to_op(op: &DeviceOp) -> Result<Value, String>`（`Err` = 非対応 op の detail 文言）
  - `fn attach_deadline(op: &mut Value, applies: bool, op_timeout_ms: u64) -> Option<Duration>`
  - `pub fn dispatch(sockets: &[PathBuf], op: &DeviceOp, op_timeout_ms: u64) -> ExitCode`
  - `pub fn dispatch_auto(sockets: &[PathBuf], op: &DeviceOp, op_timeout_ms: u64) -> Option<ExitCode>`
  - `pub fn unsupported_exit(name: &str) -> ExitCode`（`--matd` 強制 × 非対応: kind=other、exit 2）
- `native_direct::run(&Command, …)` はこのタスクでは不変（Task 10 で置換）。

- [ ] **Step 1: 失敗するテストを書く（golden JSON は不変）**

`matd_client.rs` の `mod tests` で:

1. 先頭の `use` を `use crate::device_op::DeviceOp; use mat_native::op::{GroupOp, GroupOpKind, NodeOp, NodeOpKind, ProvisionParams};` に差し替え（`Command` / `NodeRef` 系は `hint_*` テスト等が使わなければ消す）。helper を足す:

```rust
    fn node(node_id: u64, kind: NodeOpKind) -> DeviceOp {
        DeviceOp::Node(NodeOp { node_id, kind })
    }
    fn group(group_id: u16, endpoint: u16, kind: GroupOpKind) -> DeviceOp {
        DeviceOp::Group(GroupOp { group_id, endpoint, kind })
    }
```

2. to_op の golden テストの**入力だけ**を置き換える（期待 `json!` は 1 文字も変えない）:

| テスト | 新しい入力 |
|---|---|
| `read_maps_to_read_op` | `node(1, NodeOpKind::read(2, "onoff", "on-off").unwrap())` |
| `on_maps_to_on_op_with_endpoint` | `node(3, NodeOpKind::On { endpoint: 1 })` |
| `color_temp_kelvin_maps_to_color_temp_op_with_converted_mireds` | `node(6, NodeOpKind::color_temp(1, Some(2700), None, 30))` |
| `color_temp_mireds_maps_with_computed_kelvin_echo` | `node(6, NodeOpKind::color_temp(1, None, Some(370), 0))` |
| `color_maps_to_color_op_with_converted_values` | `node(6, NodeOpKind::Color { endpoint: 1, color: mat_core::color::resolve_spec(None, None, Some(330), Some(80)).unwrap(), transition: 30 })` |
| `color_name_op_includes_name_and_rgb_echo` | `node(6, NodeOpKind::Color { endpoint: 1, color: mat_core::color::resolve_spec(Some("red"), Some("#ff0000"), None, None).unwrap(), transition: 0 })` |
| `group_provision_fills_default_name_and_keeps_null_epoch` | `DeviceOp::GroupProvision(ProvisionParams { group_id: 7, node_ids: vec![1, 2], keyset_id: 42, name: "grp7".into(), endpoint: 1, epoch_key: None, rebind: false })`（name 補完は Task 8 の classify のテストで担保） |
| `group_bump_maps_to_group_bump_op` | `DeviceOp::GroupBump` |
| `group_grant_is_unsupported_via_matd` | `DeviceOp::GroupGrant { group_id: 1, node_ids: vec![5] }` → `to_op(..).unwrap_err()` が `"group grant"` を含む |
| `group_color_temp_maps_to_group_color_temp_op` | `group(1, 1, GroupOpKind::color_temp(Some(2700), None, 0))` |
| `group_level_maps_to_group_level_op` | `group(1, 1, GroupOpKind::level(50, 0))` |
| `group_color_maps_to_group_color_op_with_echo` | `group(1, 1, GroupOpKind::Color { color: mat_core::color::resolve_spec(Some("blue"), Some("#0000ff"), None, None).unwrap(), transition: 0 })` |

追加で invoke / write / group invoke の golden を足す（旧テストに無かった腕）:

```rust
    #[test]
    fn write_invoke_and_group_invoke_keep_names_and_args_on_the_wire() {
        let w = node(1, NodeOpKind::write(1, "levelcontrol", "on-level", "128").unwrap());
        assert_eq!(
            to_op(&w).unwrap(),
            json!({"op":"write","node_id":1,"endpoint":1,"cluster":"levelcontrol","attribute":"on-level","value":"128"})
        );
        let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
        let i = node(1, NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args).unwrap());
        assert_eq!(
            to_op(&i).unwrap(),
            json!({"op":"invoke","node_id":1,"endpoint":1,"cluster":"levelcontrol","command":"move-to-level","args":["128","0","0","0"]})
        );
        let g = group(10, 1, GroupOpKind::invoke("onoff", "on", &[]).unwrap());
        assert_eq!(
            to_op(&g).unwrap(),
            json!({"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","args":[],"endpoint":1})
        );
        assert_eq!(
            to_op(&node(1, NodeOpKind::Describe)).unwrap(),
            json!({"op":"describe","node_id":1})
        );
    }
```

3. `discover_and_commission_are_unsupported` と `to_op_separates_unsupported_from_real_errors` を削除し、代わりに:

```rust
    /// 直経路専用 op は matd へ送らない（文言はサブコマンド名入り）。
    #[test]
    fn direct_only_ops_are_unsupported_via_matd() {
        let dt = node(1, NodeOpKind::DiagThread { endpoint: 0 });
        assert!(to_op(&dt).unwrap_err().contains("diag"));
        let ow = node(1, NodeOpKind::OpenWindow { timeout: 180, iteration: 1000, discriminator: 1 });
        assert!(to_op(&ow).unwrap_err().contains("open-window"));
    }
```

4. `attach_deadline_only_for_single_node_ops` を新シグネチャに:

```rust
    #[test]
    fn attach_deadline_only_when_budget_applies() {
        let mut op = json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"});
        let rt = attach_deadline(&mut op, true, 15_000);
        assert_eq!(op["deadline_ms"], json!(15_000));
        assert_eq!(rt, Some(std::time::Duration::from_millis(15_000) + CLIENT_SLACK));
        // 0 = 明示無制限: フィールドは付く（matd の既定 60s を止める）が read timeout なし。
        let mut op = json!({"op":"on","node_id":3,"endpoint":1});
        assert_eq!(attach_deadline(&mut op, true, 0), None);
        assert_eq!(op["deadline_ms"], json!(0));
        // 対象外（group 系・bump）: 無変更・read timeout なし。
        let mut op = json!({"op":"group_bump"});
        assert_eq!(attach_deadline(&mut op, false, 15_000), None);
        assert!(op.get("deadline_ms").is_none());
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat --bin mat matd_client::tests 2>&1 | head`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装**

`matd_client.rs`:

```rust
use crate::device_op::DeviceOp;
use mat_native::op::{GroupOpKind, NodeOpKind};

/// 予算対象 op（`NodeOpKind::budget_applies`）へ deadline_ms を付与し、
/// 適用時の read timeout を返す。非対象は無変更・read timeout なし。
/// 0 = 明示無制限（matd 既定 60s の適用を止める）— read timeout も掛けない。
fn attach_deadline(op: &mut Value, applies: bool, op_timeout_ms: u64) -> Option<Duration> {
    if !applies {
        return None;
    }
    if let Value::Object(map) = op {
        map.insert("deadline_ms".into(), json!(op_timeout_ms));
    }
    (op_timeout_ms > 0).then(|| Duration::from_millis(op_timeout_ms) + CLIENT_SLACK)
}

fn budget_applies(op: &DeviceOp) -> bool {
    matches!(op, DeviceOp::Node(n) if n.kind.budget_applies())
}

/// `--matd` 強制時の非対応 op。kind=other だが exit 2 を返すのは「2 = CLI
/// 引数エラー」の documented シグナルを保つ意図的な例外（spec B 節）。
pub fn unsupported_exit(name: &str) -> ExitCode {
    MatError::new(ErrorKind::Other, unsupported_detail(name)).emit();
    ExitCode::from(2)
}

fn unsupported_detail(name: &str) -> String {
    format!(
        "`mat --matd` does not support the `{name}` subcommand; run it without --matd (direct native path)"
    )
}

/// `DeviceOp` を matd の op JSON に変換する。wire は名前のまま（`*_in`）—
/// 契約不変。直経路専用 op は `Err(detail)`。
fn to_op(op: &DeviceOp) -> Result<Value, String> {
    Ok(match op {
        DeviceOp::Node(n) => {
            let node_id = n.node_id;
            match &n.kind {
                NodeOpKind::Read { endpoint, cluster_in, attribute_in, .. } => json!({
                    "op": "read", "node_id": node_id, "endpoint": endpoint,
                    "cluster": cluster_in, "attribute": attribute_in,
                }),
                NodeOpKind::Write { endpoint, cluster_in, attribute_in, value_in, .. } => json!({
                    "op": "write", "node_id": node_id, "endpoint": endpoint,
                    "cluster": cluster_in, "attribute": attribute_in, "value": value_in,
                }),
                NodeOpKind::Invoke { endpoint, cluster_in, command_in, args_in, .. } => json!({
                    "op": "invoke", "node_id": node_id, "endpoint": endpoint,
                    "cluster": cluster_in, "command": command_in, "args": args_in,
                }),
                NodeOpKind::Describe => json!({ "op": "describe", "node_id": node_id }),
                NodeOpKind::On { endpoint } => {
                    json!({ "op": "on", "node_id": node_id, "endpoint": endpoint })
                }
                NodeOpKind::Off { endpoint } => {
                    json!({ "op": "off", "node_id": node_id, "endpoint": endpoint })
                }
                // 換算済み値を渡し、kelvin / percent / 度 / % / name / rgb は
                // 応答エコー用（matd 側で逆算すると丸めで入力とずれる）。
                NodeOpKind::ColorTemp { endpoint, kelvin, mireds, transition } => json!({
                    "op": "color_temp", "node_id": node_id, "endpoint": endpoint,
                    "mireds": mireds, "kelvin": kelvin, "transition": transition,
                }),
                NodeOpKind::Level { endpoint, percent, level, transition } => json!({
                    "op": "level", "node_id": node_id, "endpoint": endpoint,
                    "level": level, "percent": percent, "transition": transition,
                }),
                NodeOpKind::Color { endpoint, color, transition } => {
                    let mut op = json!({
                        "op": "color", "node_id": node_id, "endpoint": endpoint,
                        "hue_raw": color.hue_raw, "saturation_raw": color.sat_raw,
                        "hue": color.hue, "saturation": color.sat, "transition": transition,
                    });
                    if let Some(name) = &color.name {
                        op["name"] = json!(name);
                    }
                    if let Some(rgb) = &color.rgb {
                        op["rgb"] = json!(rgb);
                    }
                    op
                }
                // matd は warm CASE セッション層。これらは直経路でしか実行できない。
                NodeOpKind::DiagThread { .. } => return Err(unsupported_detail("diag")),
                NodeOpKind::OpenWindow { .. } => return Err(unsupported_detail("open-window")),
            }
        }
        DeviceOp::Group(g) => {
            let (group_id, endpoint) = (g.group_id, g.endpoint);
            match &g.kind {
                GroupOpKind::Invoke { cluster_in, command_in, args_in, .. } => json!({
                    "op": "group_invoke", "group_id": group_id, "cluster": cluster_in,
                    "command": command_in, "args": args_in, "endpoint": endpoint,
                }),
                GroupOpKind::ColorTemp { kelvin, mireds, transition } => json!({
                    "op": "group_color_temp", "group_id": group_id,
                    "mireds": mireds, "kelvin": kelvin,
                    "transition": transition, "endpoint": endpoint,
                }),
                GroupOpKind::Level { percent, level, transition } => json!({
                    "op": "group_level", "group_id": group_id,
                    "level": level, "percent": percent,
                    "transition": transition, "endpoint": endpoint,
                }),
                GroupOpKind::Color { color, transition } => {
                    let mut op = json!({
                        "op": "group_color", "group_id": group_id,
                        "hue_raw": color.hue_raw, "saturation_raw": color.sat_raw,
                        "hue": color.hue, "saturation": color.sat,
                        "transition": transition, "endpoint": endpoint,
                    });
                    if let Some(name) = &color.name {
                        op["name"] = json!(name);
                    }
                    if let Some(rgb) = &color.rgb {
                        op["rgb"] = json!(rgb);
                    }
                    op
                }
            }
        }
        DeviceOp::GroupProvision(p) => json!({
            "op": "group_provision", "group_id": p.group_id, "node_ids": p.node_ids,
            "keyset_id": p.keyset_id, "name": p.name, "endpoint": p.endpoint,
            "epoch_key": p.epoch_key, "rebind": p.rebind,
        }),
        DeviceOp::GroupBump => json!({ "op": "group_bump" }),
        // grant は稀な修復操作で warm session の恩恵が小さく、mat/matd の
        // バージョンスキューにも安全なため直経路のみ。
        DeviceOp::GroupGrant { .. } => return Err(unsupported_detail("group grant")),
    })
}
```

`dispatch` / `dispatch_auto` は引数を `op: &DeviceOp` に変え、冒頭を:

```rust
    // dispatch:
    let mut op_json = match to_op(op) {
        Ok(v) => v,
        Err(detail) => {
            MatError::new(ErrorKind::Other, &detail).emit();
            return ExitCode::from(2);
        }
    };
    // …connect_candidates は不変…
    let read_timeout = attach_deadline(&mut op_json, budget_applies(op), op_timeout_ms);

    // dispatch_auto:
    // matd 非対応 op（open-window / diag thread / grant）は probe せず直経路。
    let mut op_json = match to_op(op) {
        Ok(v) => v,
        Err(_) => return None,
    };
    // …
    let read_timeout = attach_deadline(&mut op_json, budget_applies(op), op_timeout_ms);
```

`ToOpError` と `unsupported` 関数、`use crate::cli::{Command, GroupCommand}` は削除。

`main.rs`: Listen の早期 return の後、`resolve_route` の前に

```rust
    // Command → DeviceOp（名前解決・換算・color spec は経路によらずここで 1 回。
    // 未知名・符号化不能・不正 color spec は matd に触れる前に固有 kind で失敗）。
    let dispatch = match device_op::classify(&command) {
        Ok(d) => d,
        Err(e) => {
            e.emit();
            return ExitCode::from(e.kind.exit_code());
        }
    };
    let device_op = match &dispatch {
        device_op::Dispatch::Device(op) => Some(op),
        device_op::Dispatch::Dedicated(_) => None,
    };
```

を置き、経路解決の match を:

```rust
    match matd_client::resolve_route(
        &args.matd,
        std::env::var_os("MAT_MATD_SOCKET"),
        std::env::var_os("MAT_MATD"),
    ) {
        matd_client::Route::Forced(sockets) => {
            return match &dispatch {
                device_op::Dispatch::Device(op) => {
                    matd_client::dispatch(&sockets, op, args.op_timeout_ms)
                }
                // 専用コマンド層の op は matd プロトコルに無い（従来どおり exit 2）。
                device_op::Dispatch::Dedicated(name) => matd_client::unsupported_exit(name),
            };
        }
        matd_client::Route::Auto(sockets) => {
            if let Some(op) = device_op {
                if let Some(code) = matd_client::dispatch_auto(&sockets, op, args.op_timeout_ms) {
                    return code;
                }
            }
        }
        matd_client::Route::Direct => {}
    }
```

に書き換える。`native_direct::run(&command, …)` の呼び出しはこのタスクでは
そのまま（Task 10 で `device_op` を渡す形に変える）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat && cargo clippy -p mat --all-targets -- -D warnings`
Expected: PASS（`tests/integration.rs` の `--matd discover` 系の exit 2 / 文言テスト含む）。

- [ ] **Step 5: Commit**

```bash
git add crates/mat/src/matd_client.rs crates/mat/src/main.rs
git commit -m "refactor(mat): matd_client を DeviceOp 入力に — wire JSON は不変、経路分岐は Dispatch で（監査④ Task9）"
```

---
### Task 10: `native_direct` を `DeviceOp` + `OneShotRunner` に置き換え、旧実装・emit 群・units を削除

**Files:**
- Modify: `crates/mat/src/native_direct.rs`（大幅縮小）
- Modify: `crates/mat/src/main.rs`
- Delete: `crates/mat/src/units.rs`、`crates/mat/src/commands/open_window.rs`、`crates/mat/src/commands/group.rs`
- Modify: `crates/mat/src/commands/mod.rs`（`pub mod open_window; pub mod group;` を削除）、`crates/mat/src/commands/diag.rs`（`emit_diag_thread_success` を削除）

**Interfaces:**
- Consumes: `device_op::{DeviceOp, Dispatch}`、`mat_native::runner::{OneShotRunner, run_node, provision, grant}`、`mat_native::op::{run_group_op, run_group_bump}`、`matd_client::hint_node_touched`。
- Produces: `pub(crate) fn run(op: &DeviceOp, store_path: &Path, cfg: &Config, op_timeout_ms: u64) -> Result<(), MatError>`（`Option` を外す — 専用コマンド層の判定は `classify` が済ませている）。`Config` / `diag_im_probe` / `DiagImProbe` / `mesh_probe_one` 等の diag 補助は不変。

- [ ] **Step 1: 失敗するテストを書く（既存テストの整理）**

`native_direct.rs` の `mod tests` で:

1. 次のテストは Task 2–5 で mat-native へ移植済みなので**削除**:
   `on_off_read_onoff_and_color_shapes_are_native` / `color_temp_shape_is_native` /
   `color_shape_is_native` / `group_onoff_generic_provision_and_grant_are_all_native` /
   `classify_group_bump` / `group_color_and_color_temp_shapes_are_always_native` /
   `group_onoff_hard_errors_when_engine_group_ctx_unconfigured` /
   `group_bump_advances_counter_via_engine` / `one_shot_does_not_retry_on_timeout` /
   `one_shot_invoke_succeeds_via_engine` / `op_on_closes_session_on_success` /
   `op_closes_session_on_failure` / `generic_read_is_native_when_names_resolve` /
   `write_scalar_native_and_list_rejected` / `generic_invoke_scalar_args_native_and_bad_args_rejected` /
   `describe_diag_thread_open_window_shapes_are_native` / `diag_mesh_is_excluded_from_native_direct_run` /
   `run_op_group_provision_writes_controller_state_to_kvs_natively` /
   `run_op_group_provision_hard_errors_when_ctx_missing` / `open_window_runs_via_fake_and_emits_codes` /
   `run_op_read_attr_completes_via_native` / `run_op_write_attr_completes_via_native` /
   `run_op_invoke_generic_completes_via_native` / `run_op_describe_completes_via_native` /
   `budget_applies_only_to_single_node_hotpath_ops`、および `ScriptedEstablisher` /
   `scripted_establisher`。
2. 残す: `diag_im_with_engine_*` 2 本、`ScriptedImEstablisher` / `FailingImEstablisher` /
   `failing_establisher`、`op_deadline_fires_node_touched_hint_on_timeout` /
   `op_deadline_passes_through_completion_without_hint`（後者の `std::future::ready(Ok(()))`
   は `Ok(serde_json::json!({}))` に変える — `run_op_with_deadline` が `T` ジェネリックになるため）。
3. 追加（engine レベルの完走確認、旧 `run_op_*_completes_via_native` の後継）:

```rust
    /// `classify` が出す DeviceOp を engine ごと直経路の実行関数に通し、
    /// FakeConn 応答で最後まで body が返ることを保証する。
    #[tokio::test]
    async fn run_with_engine_completes_for_node_group_bump_and_grant() {
        use crate::cli::GroupCommand;
        use crate::device_op::{classify, Dispatch};
        use mat_core::alias::{EndpointRef, GroupRef, NodeRef};
        use mat_native::test_support::FakeEstablisher;
        let engine = Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        let read = Command::Read {
            node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(), attribute: "current-level".into(),
        };
        let Dispatch::Device(op) = classify(&read).unwrap() else { panic!("device op") };
        let body = run_with_engine(&engine, &op).await.unwrap();
        assert_eq!(body["cluster"], "levelcontrol");
        let ow = Command::OpenWindow { node_id: NodeRef::Id(5), timeout: 180, iteration: 1000, discriminator: Some(3840) };
        let Dispatch::Device(op) = classify(&ow).unwrap() else { panic!("device op") };
        assert!(run_with_engine(&engine, &op).await.unwrap()["qr_payload"].is_string());
        // group ctx 未構成はハードエラー（Other）。
        let toggle = Command::Group { action: GroupCommand::Invoke {
            group_id: GroupRef::Id(10), cluster: "onoff".into(), command: "toggle".into(), args: vec![], endpoint: 1,
        } };
        let Dispatch::Device(op) = classify(&toggle).unwrap() else { panic!("device op") };
        assert_eq!(run_with_engine(&engine, &op).await.unwrap_err().kind, mat_core::error::ErrorKind::Other);
        let Dispatch::Device(op) = classify(&Command::Group { action: GroupCommand::Bump }).unwrap() else { panic!() };
        assert_eq!(run_with_engine(&engine, &op).await.unwrap_err().kind, mat_core::error::ErrorKind::Other);
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat --bin mat native_direct::tests 2>&1 | head`
Expected: コンパイルエラー（`run_with_engine` 未定義）。

- [ ] **Step 3: 実装**

`native_direct.rs` から `NativeOp` / `budget_applies` / `resolve_discriminator` /
`classify` / `classify_inner` / `classify_strict` / `classify_strict_inner` /
`unresolved_op_error` / `execute` / `finish_conn` / `op_*` 全部 / `run_op` を削除し、
`run` を次に置き換える（`map_engine_build_error` / `run_op_with_deadline` /
diag 系は残す）:

```rust
use crate::device_op::DeviceOp;
use mat_native::runner::OneShotRunner;

/// 直経路 provision の note（KVS を直接書いたので matd の warm 状態は古い）。
const PROVISION_NOTE: &str =
    "controller group state written natively to kvs; if matd is running, restart it to reload group state";

/// 直経路 native の入口。store / commission チェック → engine 構築 →
/// `OneShotRunner`（確立 → 1 op → close → matd へ node_touched ヒント）で実行し、
/// 成功 body を stdout へ emit する。
pub(crate) fn run(
    op: &DeviceOp,
    store_path: &Path,
    cfg: &Config,
    op_timeout_ms: u64,
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    // group 送信 / bump は特定ノード宛ではないため require_node をしない。
    // provision / grant は「1 つでも未 commission なら exit 11」。
    let node_id = match op {
        DeviceOp::Node(n) => Some(n.node_id),
        DeviceOp::GroupProvision(p) => {
            for &id in &p.node_ids {
                store.require_node(id)?;
            }
            None
        }
        DeviceOp::GroupGrant { node_ids, .. } => {
            for &id in node_ids {
                store.require_node(id)?;
            }
            None
        }
        DeviceOp::Group(_) | DeviceOp::GroupBump => None,
    };
    if let Some(id) = node_id {
        store.require_node(id)?;
    }
    // CLI 指定 epoch key はバックエンド接触前に検証する（不正入力に fail-fast）。
    if let DeviceOp::GroupProvision(p) = op {
        if let Some(k) = &p.epoch_key {
            mat_core::group::resolve_epoch_key(Some(k))?;
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MatError::new(mat_core::error::ErrorKind::Other, format!("tokio runtime: {e}")))?;
    let body = rt.block_on(async {
        let native_cfg = NativeConfig {
            store: store.root().to_path_buf(),
            iface: cfg.iface.to_string(),
            thread_iface: cfg.thread_iface.clone(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = Engine::build(&native_cfg).await.map_err(map_engine_build_error)?;
        let budget = matches!(op, DeviceOp::Node(n) if n.kind.budget_applies());
        if budget && op_timeout_ms > 0 {
            // 直経路にも matd 経路と同じ予算セマンティクス（exit 3）。
            run_op_with_deadline(
                run_with_engine(&engine, op),
                op_timeout_ms,
                node_id,
                crate::matd_client::hint_node_touched,
            )
            .await
        } else {
            run_with_engine(&engine, op).await
        }
    })?;
    tracing::info!(op = op.name(), "op executed (native direct)");
    output::emit(body);
    Ok(())
}

/// engine 上で 1 op を実行し成功 body を返す（emit しない — テスト可能な単位）。
async fn run_with_engine(engine: &Engine, op: &DeviceOp) -> Result<serde_json::Value, MatError> {
    let runner = OneShotRunner::new(engine, crate::matd_client::hint_node_touched);
    match op {
        DeviceOp::Node(n) => mat_native::runner::run_node(&runner, n, None).await,
        DeviceOp::Group(g) => mat_native::op::run_group_op(engine, g).await,
        DeviceOp::GroupProvision(p) => {
            mat_native::runner::provision(&runner, engine, p, Some(PROVISION_NOTE)).await
        }
        DeviceOp::GroupGrant { group_id, node_ids } => {
            mat_native::runner::grant(&runner, *group_id, node_ids).await
        }
        DeviceOp::GroupBump => mat_native::op::run_group_bump(engine).await,
    }
}
```

`run_op_with_deadline` を `T` ジェネリックに:

```rust
async fn run_op_with_deadline<T, F>(
    fut: F,
    op_timeout_ms: u64,
    node_id: Option<u64>,
    hint: impl FnOnce(u64),
) -> Result<T, MatError>
where
    F: std::future::Future<Output = Result<T, MatError>>,
{
    // 本体は不変（Ok(r) => r / Err(_) => hint + Timeout）。
}
```

不要になった `use`（`im`, `ResolvedColor`, `ScalarValue`, `GroupOutcome`,
`NodeRef`, `Command`/`DiagCommand`/`GroupCommand` のうち diag 系が使わないもの）を
消す。ファイル冒頭 doc を「直経路 = `OneShotRunner`。op の実体は
`mat_native::op` / `runner`（matd と共有）。ここに残るのは store チェック・
engine 構築・予算・emit と diag 補助」に改める。

`main.rs`: `native_direct::run(&command, &store_path, cfg, args.op_timeout_ms)` の
`if let Some(result) = …` ブロックを

```rust
    if let Some(cfg) = &native_cfg {
        if let Some(op) = device_op {
            return match native_direct::run(op, &store_path, cfg, args.op_timeout_ms) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::debug!(kind = ?e.kind, detail = %e.detail, "native direct failed");
                    e.emit();
                    ExitCode::from(e.kind.exit_code())
                }
            };
        }
    }
```

に置き換え、その下の専用コマンド層 match の `_ =>` 腕コメントを
「`Dispatch::Dedicated` の残り（fabric / listen は早期 return 済み）」に合わせる。
`mod units;` を削除し `units.rs` を `git rm`。`commands/open_window.rs` と
`commands/group.rs` を `git rm` し `commands/mod.rs` から外す。`commands/diag.rs`
の `emit_diag_thread_success` と、それだけが使う `use` を削除。Task 8 で付けた
`#[allow(dead_code)]` を外す。`open_window.rs` 冒頭の QR/ブリッジ非対象の doc
は `device_op.rs` の `OpenWindow` 腕コメントに 2 行で移す（「QR 画像の
レンダリングは `mat` の責務ではない。複数機器の一括共有は Matter 仕様上不可」）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat && cargo clippy -p mat --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS。`tests/integration.rs`（バイナリ経由の exit code / エラー文言）無退行。

- [ ] **Step 5: Commit**

```bash
git add -A crates/mat/src
git commit -m "refactor(mat): native_direct を DeviceOp + OneShotRunner に — NativeOp/classify/op_* を撤去、units・emit 群を mat-native/mat-core へ（監査④ Task10）"
```

---

### Task 11: ドキュメント記録 + semver 判定 + 行数計測

**Files:**
- Modify: `ARCHITECTURE.md`（`## Things we never do` の直前に節を追加）
- Modify: `CLAUDE.md`（「Backend (native)」節に 1 項目追加）
- Modify: `crates/mat-core/src/body.rs`（Task 1 で doc 更新済みなら不要）

- [ ] **Step 1: ARCHITECTURE.md に記録を追加**

`## Things we never do` の直前に:

```markdown
### Phase 5 保守 — op 単一ソース化（監査④、2026-09-02）

1 op 追加で `mat` / `matd` の 6 箇所以上の網羅 match と 2 系統の op 実行本体
（TLV 符号化 + 成功 body 組立）を書いていた構造を解消した。

- **`mat-native::op`**: 解決済み op 型 `NodeOp` / `GroupOp` / `ProvisionParams` と
  名前解決・換算コンストラクタ（`mat-core::ids` / `color` の規則をここで 1 回だけ
  適用）、`run_node_op` / `run_group_op` / `run_group_bump` = op → TLV → body の
  唯一の場所。`budget_applies`（`--op-timeout-ms` / matd `deadline_ms` の対象）もここ。
- **`mat-native::runner`**: `NodeRunner::with_node` がセッション取得戦略の差し替え点。
  `mat` = `OneShotRunner`（確立 → 1 op → close → matd へ node_touched ヒント、
  設計ルール 4）、`matd` = `NativeBackend`（per-node warm slot、Timeout で 1 回だけ
  再確立、Issue #16 の予算）。`run_node` / `provision` / `grant` は両経路共通。
- **`mat`**: `device_op::classify` が `Command` → `DeviceOp` の match 1 本
  （旧 `classify` / `classify_strict` の 2 段は chip-tool fallback の遺物として撤去）。
  `matd_client::to_op` は `DeviceOp` から wire JSON を組む（wire は名前のまま、契約不変）。
  matd 経路でも名前解決を mat 側で先に行うため、未知名は matd に送る前に
  `parse_error` になる（kind / exit / JSON 形は同一）。
- **`matd`**: `server::to_device_op` が wire `Op` → `MatdOp` の match 1 本
  （旧 `is_native_hotpath` / `native_op` / `native_group_params` / `group_provision` を撤去）。
  `protocol.rs` の `Op` と helper、born-dead 判定（`op_state_target`）は不変。
- 1 op 追加で触る場所: cli / resolve / `device_op::classify` / `to_op` / `protocol::Op` /
  `to_device_op` / `NodeOpKind` + `run_node_op` の 7 箇所（旧 ~15）。
- spec: `docs/superpowers/specs/2026-09-02-op-single-source-design.md`。
```

- [ ] **Step 2: CLAUDE.md の「Backend (native)」節に追記**

「`mat` couples to the backend only through …」の項目の直前に:

```markdown
- **op → TLV → body lives once**, in `mat-native::op` (`run_node_op` /
  `run_group_op`). `mat` and `matd` differ only in session strategy
  (`mat-native::runner::NodeRunner`: one-shot vs warm). Adding an op means
  one arm in `NodeOpKind` + `run_node_op`, plus the CLI (`cli.rs` / `resolve.rs`
  / `device_op::classify` / `matd_client::to_op`) and wire (`protocol::Op` /
  `server::to_device_op`) mappings — never a second copy of the encoding.
```

- [ ] **Step 3: semver 判定と行数計測を実行し、結果をコミットメッセージに記録**

Run:
```bash
task check
task semver 2>&1 | tail -30
git diff --stat main -- crates/ | tail -1
```

`task semver` は cargo-semver-checks が必要（未インストールなら
`cargo install cargo-semver-checks --locked` を実行してよい）。結果（major 判定の
有無と対象クレート）と `git diff --stat` の合計行数をコミットメッセージ本文に書く。
バージョンは上げない。

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md CLAUDE.md
git commit -m "docs: op 単一ソース化の記録（監査④ Task11）

task semver: <結果を 1 行で>
git diff --stat main -- crates/: <合計行数>"
```

---

### Task 12: 最終検証（オーケストレータ実施）

subagent ではなくメインセッションが行う。

- [ ] **Step 1: `task check` 緑を確認**

Run: `task check 2>&1 | tail -5`
Expected: exit 0。

- [ ] **Step 2: matv 回帰 E2E**

Run: `task e2e:device:m3 2>&1 | tail -20`（要 実 NIC、既定 eth1 / `MAT_E2E_IFACE`）
Expected: PASS（group provision → matd 常駐 Subscribe → `mat listen` イベント受信）。

- [ ] **Step 3: 実機 E2E（hogar-matd コンテナ内、`*.new` で本番未置換）**

x86_64 musl 静的ビルド（`cargo build --release --target x86_64-unknown-linux-musl -p mat -p matd`）
を hogar-matd コンテナへ `docker cp` し、隔離 matd 方式で:

| 経路 | コマンド | 期待 |
|---|---|---|
| 直経路 | `MAT_MATD=0 MAT_FABRIC_INDEX=2 mat.new read --node 23 --cluster onoff --attribute on-off` | exit 0、value bool |
| matd 経由 | `MAT_MATD_SOCKET=<隔離 sock> mat.new read --node 24 --cluster onoff --attribute on-off` | exit 0 |
| matd 経由 | `MAT_MATD_SOCKET=<隔離 sock> mat.new describe --node 24` | exit 0、endpoints 配列 |
| 直経路 | `MAT_MATD=0 mat.new invoke --node 24 --cluster 8 --command 0 <現在 level> 0 0 0`（無変化パターン） | exit 0 |
| エラー | `mat.new read --node 99 --cluster onoff --attribute on-off` | exit 11 |
| エラー | `mat.new read --node 24 --cluster nosuch --attribute x`（matd 経由） | exit 1、parse_error |

後始末（隔離 matd 停止、`*.new` 削除）まで行う。結果は最終報告に表で載せる。

- [ ] **Step 4: マージ判断へ**

superpowers:finishing-a-development-branch を起動する。

