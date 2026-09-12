# Refactor lane native-core (mat-native + mat-core + mat/src/probe.rs) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 2026-09-12 監査バックログ（`mat-wt/tasks/audit.md`）のうち native-core レーン担当分（`tasks/native-core.md` 項目 1〜7）を、挙動変更 0 で消化する。

**Architecture:** mat-native は `mat` / `matd` が共有する native エンジン（`Engine` = 確立器 + group ctx）。mat-core は純ロジック（エラー種別・JSON body・mesh グラフ）。本計画は (a) 死コード削除、(b) 認証情報ロード / CASE 確立 / エラー前置きの一本化、(c) mesh `build_graph` のフェーズ分割、(d) lib.rs / op.rs の機械的分割、(e) テスト基盤の共通化 — の順で、各タスクが独立にテスト緑で終わるよう並べる。

**Tech Stack:** Rust 2021（MSRV 1.87）、tokio、serde_json、mat-controller（TLV/CASE/dnssd/kvs）。テストは `cargo test -p mat-core -p mat-native --all-features` と `cargo test -p mat`。

**Spec:** `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/_common.md`、`/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/native-core.md`、`/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/audit.md`（Tier 1 / 3 / 5 / 6 の mat-native / mat-core 項目）。

## Global Constraints

- **触ってよいファイルは `crates/mat-native/**`、`crates/mat-core/**`、`crates/mat/src/probe.rs` のみ。** mat の他ファイル・matd・mat-controller・mat-device は別レーンが並行編集中 — 参照は可、編集は不可。ビルドを通すために他クレートの変更が必要になった場合は、公開 API（関数名・シグネチャ・re-export）を維持して mat-native 側で吸収する。
- `mat-core/src/error.rs` に足す関数は **`ErrorKind::as_str`** と **`MatError::prefixed`** の 2 本だけ（cli-daemon レーンが同ファイルに `emit_exit` / `with_fabric_init_hint` / `from_wire` を足す。名前が被らないこと）。
- `kvs::ALPHA_INI_FILE` は ctrl-store レーンが mat-controller に追加する → マージ前は mat-native 側にローカル const `ALPHA_INI_FILE`（同じ文字列 `"chip_tool_config.alpha.ini"`）を置く。
- リファクタは挙動変更 0 が原則。既存テストが pin している文言・バイト列・順序は保つ。ErrorKind（exit code）は一切変えない（唯一の例外: probe.rs の NOC 自己発行失敗を `StoreMissing` → `StoreParse` に統一する — これは指示書の明示項目）。
- 各タスク後に `cargo test -p mat-core -p mat-native --all-features`（probe.rs を触るタスクは `cargo test -p mat` も）、最後に `task check`（fmt:check + clippy + doc:check + test）緑。
- コミットは `refactor/native-core` ブランチに小さく積む。**main へのマージ・push・リリース・デプロイはしない。**
- コミットメッセージは既存流儀（`refactor(<scope>): 日本語要約`）。末尾に以下を付ける:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01RNUgkVTjNBkq7J14Jd7JTT
  ```
- 並行 5 セッションが cargo を回すので遅い。待つ。`cargo` の同時実行ロック待ちはエラーではない。
- audit.md の行番号は main 411ba68 時点。着手前に必ず `grep -n` で現存確認する。

## 見送り（着手しない、DONE に理由を書く）

- `NodeConn::read_onoff` の廃止（matd テスト 26 箇所に波及、別件）。
- `to_op` ↔ `to_device_op` の Serialize 派生（ワイヤ変更）。
- `group_settings.rs::map_gs_err` と `rotate_ipk.rs::map_gs_err` の統合: 全アームで文言が異なり、`Corrupt` の kind も `Other`（provision）vs `StoreParse`（rotate）で違う。統合すると挙動変更か引数だらけの関数になるため見送り。
- `mat/matd` 側の `FakeConn::with_group_provision_fixture()` 利用、`mat_core::hex` の mat/matd/mat-device 側置換（別レーンのフォローアップ）。

---

### Task 1: Tier 1 死コード削除（mat-core diag / group、mat-native SubscribeConn 既定メソッド、ops.rs 定数）

**Files:**
- Modify: `crates/mat-core/src/diag.rs` （`parse_compressed_fabric_id` / `parse_operational_instance_cfid` とそのテスト 6 本を削除）
- Modify: `crates/mat-core/src/group.rs` （`KEY_SECURITY_POLICY` / `EPOCH_START_TIME` / `GROUP_NODE_ID_BASE` / `group_node_id` + テスト 1 本を削除、module doc 更新）
- Modify: `crates/mat-native/src/lib.rs` （`SubscribeConn::subscribe_wildcard` / `next_report` 既定メソッド削除、テスト 2 本を `subscribe` / `next_report_full` へ）
- Modify: `crates/mat-native/src/ops.rs` （`IPK_KEYSET_ID` / `CMD_REMOVE_GROUP` を mat-controller の定数に置換）
- Modify: `crates/mat-native/src/runner.rs:114` （`crate::ops::IPK_KEYSET_ID` → `mat_controller::group_settings::IPK_KEYSET_ID`）

**Interfaces:**
- Consumes: `mat_controller::group_settings::IPK_KEYSET_ID: u16`（= 0、pub）、`mat_controller::im::CMD_REMOVE_GROUP: u32`（= 0x03、pub）。`CMD_KEY_SET_REMOVE` は mat-controller に**無い**ので ops.rs に残す。
- Produces: なし（削除のみ）。

- [ ] **Step 1: 現存確認**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat-wt/native-core
grep -rn 'parse_compressed_fabric_id\|parse_operational_instance_cfid\|group_node_id\|KEY_SECURITY_POLICY\|EPOCH_START_TIME\|subscribe_wildcard\|\.next_report(\|ops::IPK_KEYSET_ID\|ops::CMD_REMOVE_GROUP' crates/ --include='*.rs' | grep -v '^crates/mat-device'
```
Expected: 呼び手は mat-core 内のテスト、mat-native/lib.rs のテスト 2 本（1508/1513/1523/1626/1629/1636 付近）、runner.rs:114 のみ。mat / matd に呼び手が出たらそれは触れないので、その項目だけ見送りにして DONE に書く。

- [ ] **Step 2: mat-core diag.rs から chip-tool パーサを削除**

`crates/mat-core/src/diag.rs` の `pub fn parse_compressed_fabric_id` と `pub fn parse_operational_instance_cfid`（doc コメント含め、122〜159 行付近）を削除。テストモジュール内の `cfid_extracted_from_chip_log`、`cfid_absent_is_none`、`operational_instance_cfid_matches_node`、`operational_instance_cfid_lowercase_is_normalized`、`operational_instance_cfid_ignores_other_node`、`operational_instance_cfid_absent_returns_none` の 6 本を削除。

- [ ] **Step 3: mat-core group.rs の chip-tool 残骸を削除し module doc を更新**

`crates/mat-core/src/group.rs` を以下に置き換える（`validate_epoch_key` / `generate_epoch_key` / `resolve_epoch_key` とそのテスト 3 本は不変）:

```rust
//! group（groupcast）の共有ロジック。`mat group`（one-shot）と `matd` の group op が
//! 同じ epoch 鍵の検証・生成を使うよう、一箇所で保守する。
//!
//! group state（鍵束・GroupKeyMap）自体は `mat`/`matd` 独自台帳を持たず、mat が
//! 所有する chip-tool INI 互換 KVS（`mat-controller::group_settings`）に置く
//! （設計ルール 4）。ここにあるのは値の検証・生成・整形だけ。

use crate::error::{ErrorKind, MatError};

/// `--epoch-key` の妥当性検証（16バイト = 32桁 hex）。小文字へ正規化して返す。
pub fn validate_epoch_key(key: &str) -> Result<String, MatError> {
    // …（既存のまま）
}
```

削除するもの: `KEY_SECURITY_POLICY`、`EPOCH_START_TIME`、`GROUP_NODE_ID_BASE`、`group_node_id`、テスト `group_node_id_packs_group_into_low_bits`。

- [ ] **Step 4: SubscribeConn の既定メソッドを削除し、テストを本体 API へ**

`crates/mat-native/src/lib.rs` の `trait SubscribeConn` から `subscribe_wildcard` と `next_report` の 2 メソッド（doc 含む）を削除。テスト側（`fake_establisher_serves_scripted_subscription` と `fake_sub_conn_next_report_fails_when_injected_after_establish`）を次の形に書き換える:

```rust
// 旧: let (info, priming) = conn.subscribe_wildcard(&[]).await.unwrap();
let (info, priming, _events) = conn.subscribe(&[], &[], None).await.unwrap();
// 旧: conn.next_report(Duration::from_millis(50)).await
//   → 属性側だけ見るなら `.map(|o| o.map(|r| r.data))` を付ける。
let r = conn
    .next_report_full(std::time::Duration::from_millis(50))
    .await
    .unwrap()
    .map(|r| r.data);
```
既存の assert が `ReportDataMessage` を期待している箇所は `.data` で取り出した値を渡す。assert の内容は変えない。

- [ ] **Step 5: ops.rs の定数を mat-controller のものへ**

`crates/mat-native/src/ops.rs`:
- `pub const IPK_KEYSET_ID: u16 = 0;`（doc 含め）を削除し、`use mat_controller::group_settings::IPK_KEYSET_ID;` を先頭 use 群に追加。
- `pub const CMD_REMOVE_GROUP: u32 = 0x03;` を削除し、`use mat_controller::im::{...}` の既存 import 群に `CMD_REMOVE_GROUP` を追加。
- 残る `CMD_KEY_SET_REMOVE` の doc を書き換える（偽コメントを消す）:
  ```rust
  /// GroupKeyManagement KeySetRemove（spec §11.2.8.3）。`mat_controller::im` に
  /// 無いのでここで局所定義する（`CMD_REMOVE_GROUP` は im 側にある）。
  pub const CMD_KEY_SET_REMOVE: u32 = 0x03;
  ```
- `crates/mat-native/src/runner.rs:114` の `crate::ops::IPK_KEYSET_ID` → `mat_controller::group_settings::IPK_KEYSET_ID`。
- ops.rs テストは `use super::*;` なので `CMD_REMOVE_GROUP` は import 経由で見える（変更不要）。

- [ ] **Step 6: テスト**

```bash
cargo test -p mat-core -p mat-native --all-features 2>&1 | tail -20
```
Expected: 全 PASS（削除したテスト分だけ件数が減る）。

- [ ] **Step 7: コミット**

```bash
git add crates/mat-core/src/diag.rs crates/mat-core/src/group.rs crates/mat-native/src/lib.rs crates/mat-native/src/ops.rs crates/mat-native/src/runner.rs
git commit -m "refactor(native-core): Tier1 死コード削除 — chip-tool stderr パーサ・group_node_id・SubscribeConn 既定メソッド・ops.rs の重複定数"
```

---

### Task 2: mat-core に `ErrorKind::as_str` / `MatError::prefixed` / `hex` モジュールを追加し、黙殺・手組み箇所を置換

**Files:**
- Modify: `crates/mat-core/src/error.rs`（`as_str` を `impl ErrorKind` の末尾、`prefixed` を `impl MatError` の `parse_error` の直後に追加 — 他レーンとの衝突を最小化する位置）
- Create: `crates/mat-core/src/hex.rs`
- Modify: `crates/mat-core/src/lib.rs`（`pub mod hex;` を `pub mod group;` の後に追加）
- Modify: `crates/mat-core/src/body.rs:321`（`serde_json::to_value(kind).unwrap_or(Value::Null)` → `kind.as_str()`）
- Modify: `crates/mat-core/src/group.rs:47`（`generate_epoch_key` の hex 手組み → `crate::hex::encode_lower`）
- Modify: `crates/mat-native/src/rotate_ipk.rs`（`kind_str` 削除 → `as_str`、`step_err` → `prefixed`）
- Modify: `crates/mat-native/src/runner.rs`（`node {node_id}: ` 前置き ×3 → `prefixed`）
- Modify: `crates/mat-native/src/ops.rs`（`provision_step_err` / `write_ipk_keyset` / `remove_step_err` → `prefixed`）
- Modify: `crates/mat/src/probe.rs:147`（`cfid_hex` → `mat_core::hex::encode_upper`）

**Interfaces:**
- Produces:
  - `impl ErrorKind { pub fn as_str(self) -> &'static str }` — serde の `snake_case` 表現と完全一致。
  - `impl MatError { pub fn prefixed(self, prefix: impl std::fmt::Display) -> Self }` — `detail` を `"{prefix}: {detail}"` に、kind 不変。
  - `mat_core::hex::encode_lower(bytes: &[u8]) -> String`、`encode_upper(bytes: &[u8]) -> String`、`decode(s: &str) -> Option<Vec<u8>>`（奇数長・非 hex は `None`、`0x` 接頭辞は扱わない）。

- [ ] **Step 1: error.rs のテストを先に書く**

`crates/mat-core/src/error.rs` の `mod tests` に追加:

```rust
    /// `as_str` は serde の snake_case 表現と 1 変種も違わない（黙殺していた
    /// `to_value(..).unwrap_or(Null)` の置き換え先なので、ここでズレると
    /// stdout の `kind` が変わる）。
    #[test]
    fn as_str_matches_serde_snake_case_for_every_variant() {
        const ALL: [ErrorKind; 13] = [
            ErrorKind::StoreMissing,
            ErrorKind::StoreParse,
            ErrorKind::NodeNotCommissioned,
            ErrorKind::ChildNotFound,
            ErrorKind::ChildFailed,
            ErrorKind::CommissionFailed,
            ErrorKind::Timeout,
            ErrorKind::Unreachable,
            ErrorKind::SessionFailed,
            ErrorKind::DeviceRejected,
            ErrorKind::ParseError,
            ErrorKind::MatdUnavailable,
            ErrorKind::Other,
        ];
        for k in ALL {
            let via_serde = serde_json::to_value(k).unwrap();
            assert_eq!(via_serde.as_str().unwrap(), k.as_str(), "{k:?}");
        }
    }

    #[test]
    fn prefixed_keeps_kind_and_prepends_colon_separated() {
        let e = MatError::new(ErrorKind::Timeout, "fake send failure").prefixed("node 5");
        assert_eq!(e.kind, ErrorKind::Timeout);
        assert_eq!(e.detail, "node 5: fake send failure");
        let e = MatError::parse_error("x").prefixed(format!("provision step '{}' failed", "acl read"));
        assert_eq!(e.detail, "provision step 'acl read' failed: x");
    }
```

- [ ] **Step 2: 失敗確認**

```bash
cargo test -p mat-core error:: 2>&1 | tail -5
```
Expected: コンパイルエラー（`as_str` / `prefixed` 未定義）。

- [ ] **Step 3: error.rs に 2 本を実装**

`impl ErrorKind` の `exit_code` の直後に追加:

```rust
    /// serde の `snake_case` 表現（stdout の `kind` 文字列）。`to_value(..)
    /// .unwrap_or(Null)` で黙殺していた箇所の置き換え先 — テストが serde と
    /// 全変種一致を釘打ちする。
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::StoreMissing => "store_missing",
            ErrorKind::StoreParse => "store_parse",
            ErrorKind::NodeNotCommissioned => "node_not_commissioned",
            ErrorKind::ChildNotFound => "child_not_found",
            ErrorKind::ChildFailed => "child_failed",
            ErrorKind::CommissionFailed => "commission_failed",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Unreachable => "unreachable",
            ErrorKind::SessionFailed => "session_failed",
            ErrorKind::DeviceRejected => "device_rejected",
            ErrorKind::ParseError => "parse_error",
            ErrorKind::MatdUnavailable => "matd_unavailable",
            ErrorKind::Other => "other",
        }
    }
```

`impl MatError` の `parse_error` の直後に追加:

```rust
    /// `detail` に文脈を前置する（`"{prefix}: {detail}"`、kind は不変）。
    /// `node 5: …` / `provision step 'acl read' failed: …` のような手組みの
    /// 一本化先。
    pub fn prefixed(self, prefix: impl std::fmt::Display) -> Self {
        MatError::new(self.kind, format!("{prefix}: {}", self.detail))
    }
```

- [ ] **Step 4: hex.rs を作る（テスト込み）**

`crates/mat-core/src/hex.rs`:

```rust
//! hex 文字列 ⇄ バイト列。`hex` crate を足すほどではない小さな往復を、
//! 各クレートで手書きしていたのを一本化する（監査 2026-09-12 Tier 6）。

/// 小文字 hex（`00ff…`）。
pub fn encode_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 大文字 hex（`00FF…`）。CFID や ExtAddress の正準形が使う。
pub fn encode_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// hex 文字列をバイト列へ。奇数長・非 hex 文字は `None`。`0x` 接頭辞は
/// 剥がさない（呼び手の責務）。大文字小文字は不問。
pub fn decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_lower_and_upper() {
        assert_eq!(encode_lower(&[0x00, 0xAB, 0xff]), "00abff");
        assert_eq!(encode_upper(&[0x00, 0xAB, 0xff]), "00ABFF");
        assert_eq!(encode_lower(&[]), "");
    }

    #[test]
    fn decode_roundtrips_and_rejects_bad_input() {
        assert_eq!(decode("00abFF"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(decode(""), Some(vec![]));
        assert_eq!(decode("abc"), None, "odd length");
        assert_eq!(decode("zz"), None, "non-hex");
        assert_eq!(decode("0x00"), None, "prefix is not stripped here");
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode_lower(&bytes)).unwrap(), bytes);
        assert_eq!(decode(&encode_upper(&bytes)).unwrap(), bytes);
    }
}
```

`crates/mat-core/src/lib.rs` に `pub mod hex;` を追加（`pub mod group;` の次行）。

- [ ] **Step 5: mat-core 内の呼び手を置換**

- `crates/mat-core/src/body.rs` `diag_thread_success` 内: `"kind": serde_json::to_value(kind).unwrap_or(Value::Null),` → `"kind": kind.as_str(),`。
- `crates/mat-core/src/group.rs` `generate_epoch_key`: `bytes.iter().map(|b| format!("{b:02x}")).collect()` → `crate::hex::encode_lower(&bytes)`。

```bash
cargo test -p mat-core 2>&1 | tail -5
```
Expected: 全 PASS。

- [ ] **Step 6: mat-native の黙殺・手組み箇所を置換**

`crates/mat-native/src/rotate_ipk.rs`:
- `fn kind_str(kind: ErrorKind) -> String { … }`（doc 含む）を削除。`partial_error` 内の `kind_str(e.kind)` → `e.kind.as_str()`。
- `fn step_err` を次に置換（文言逐語維持）:
  ```rust
  fn step_err(node_id: u64, step: &str, e: MatError) -> MatError {
      if step.is_empty() {
          e.prefixed(format!("node {node_id}"))
      } else {
          e.prefixed(format!("node {node_id}: {step}"))
      }
  }
  ```

`crates/mat-native/src/runner.rs` の 3 箇所（provision / grant / remove_group）:
```rust
.map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
```
→
```rust
.map_err(|e| e.prefixed(format!("node {node_id}")))?;
```

`crates/mat-native/src/ops.rs`:
```rust
fn provision_step_err(e: MatError, step: &str) -> MatError {
    e.prefixed(format!("provision step '{step}' failed"))
}
fn remove_step_err(e: MatError, step: &str) -> MatError {
    e.prefixed(format!("remove step '{step}' failed"))
}
```
`write_ipk_keyset` の `.map_err(|e| MatError::new(e.kind, format!("key-set-write (ipk): {}", e.detail)))` → `.map_err(|e| e.prefixed("key-set-write (ipk)"))`。

`crates/mat/src/probe.rs`: `fn cfid_hex` の本体を `mat_core::hex::encode_upper(cfid)` に（テスト `cfid_hex_formats_16_uppercase_hex` は残す）。

- [ ] **Step 7: テスト**

```bash
cargo test -p mat-core -p mat-native --all-features 2>&1 | tail -5
cargo test -p mat probe 2>&1 | tail -5
```
Expected: 全 PASS（`provision step '…' failed: …` / `node N: …` / `remove step` の文言を pin するテストが緑のまま）。

- [ ] **Step 8: コミット**

```bash
git add crates/mat-core/src/error.rs crates/mat-core/src/hex.rs crates/mat-core/src/lib.rs crates/mat-core/src/body.rs crates/mat-core/src/group.rs crates/mat-native/src/rotate_ipk.rs crates/mat-native/src/runner.rs crates/mat-native/src/ops.rs crates/mat/src/probe.rs
git commit -m "refactor(mat-core): ErrorKind::as_str / MatError::prefixed / hex モジュールを追加し、黙殺・手組みの呼び手を置換"
```

---

### Task 3: 認証情報ロードの一本化（`load_self_issue_materials` / `self_issue_credentials` / `op_scope_id`）と `Engine::build` の iface 二重解決解消

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`load_fabric_credentials` 周辺と `case_establisher` / `build_with_resolver`）
- Modify: `crates/mat-native/src/commission.rs:536-575`（`commission()` の資材構築）
- Modify: `crates/mat-native/src/rotate_ipk.rs:196-205`（`case_establisher` の新シグネチャ）
- Modify: `crates/mat/src/probe.rs:60-88`（NativeConfig を組んで mat-native の関数を呼ぶ）

**Interfaces:**
- Produces（すべて `crates/mat-native/src/lib.rs`、pub）:
  ```rust
  /// chip-tool 互換 KVS の alpha INI 名。ctrl-store レーンが `mat_controller::kvs::ALPHA_INI_FILE`
  /// を足したらそちらへ差し替える（マージ後のフォローアップ）。
  pub const ALPHA_INI_FILE: &str = "chip_tool_config.alpha.ini";
  pub fn load_self_issue_materials(cfg: &NativeConfig) -> Result<mat_controller::kvs::SelfIssueMaterials, MatError>;
  pub fn self_issue_credentials(materials: mat_controller::kvs::SelfIssueMaterials) -> Result<FabricCredentials, MatError>;
  pub fn load_fabric_credentials(cfg: &NativeConfig) -> Result<FabricCredentials, MatError>; // = 上 2 本の合成、既存名維持
  pub fn op_scope_id(cfg: &NativeConfig) -> Result<u32, MatError>;
  pub fn case_establisher(cfg: &NativeConfig, creds: FabricCredentials, resolver: Arc<dyn Resolver>, scope_id: u32) -> Box<dyn Establisher>; // Result ではなくなる
  ```
- 文言（3 経路すべて同一に統一する。テストは kind と `mat fabric init` の含有だけを pin している）:
  - 読み取り失敗: kind `StoreMissing`、`"native: read KVS credentials: {e} — run `mat fabric init`"`
  - NOC 自己発行失敗: kind `StoreParse`、`"native: self-issue NOC: {e} — run `mat fabric init`"`
  - iface 解決失敗: kind `Other`、`"native: resolve iface {:?} index: {e}"`

- [ ] **Step 1: 現存確認**

```bash
grep -rn 'chip_tool_config.alpha.ini\|iface_index(&cfg.iface)\|iface_index(p.iface)\|read_self_issue_materials\|from_self_issued' crates/mat-native/src crates/mat/src/probe.rs
```
Expected: lib.rs（419 / 449 / 480 付近）、commission.rs（538 / 544-571）、probe.rs（62-84）。

- [ ] **Step 2: lib.rs のテストを先に足す**

`crates/mat-native/src/lib.rs` の `mod tests`（`load_fabric_credentials_maps_missing_store_to_store_missing` の直後）に追加:

```rust
    /// 3 経路（Engine::build / commission / probe）が同じ 1 本を通るので、
    /// 文言の `mat fabric init` ヒントと kind をここで釘打ちする。
    #[test]
    fn load_self_issue_materials_hints_fabric_init_on_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = NativeConfig {
            store: dir.path().to_path_buf(),
            iface: "lo".into(),
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        };
        let err = load_self_issue_materials(&cfg).unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreMissing);
        assert!(err.detail.starts_with("native: read KVS credentials: "), "{}", err.detail);
        assert!(err.detail.ends_with(" — run `mat fabric init`"), "{}", err.detail);
    }

    #[test]
    fn op_scope_id_maps_unknown_iface_to_other() {
        let cfg = NativeConfig {
            store: std::path::PathBuf::from("/nonexistent"),
            iface: "no-such-iface-at-all".into(),
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        };
        let err = op_scope_id(&cfg).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.starts_with("native: resolve iface \"no-such-iface-at-all\" index: "), "{}", err.detail);
    }
```

- [ ] **Step 3: 失敗確認**

```bash
cargo test -p mat-native --all-features load_self_issue 2>&1 | tail -5
```
Expected: コンパイルエラー。

- [ ] **Step 4: lib.rs を実装**

`load_fabric_credentials` を次に置き換え、`case_establisher` / `build_with_resolver` を修正:

```rust
/// chip-tool 互換 KVS の alpha INI 名。ctrl-store レーンが
/// `mat_controller::kvs::ALPHA_INI_FILE` を足したらそちらへ差し替える。
pub const ALPHA_INI_FILE: &str = "chip_tool_config.alpha.ini";

/// KVS から自己発行資材（root CA 鍵・fabric id・node id）を読む。`Engine::build`
/// / `commission` / `mat` の probe が同じ 1 本を通る。読めない = fabric 未
/// bootstrap → `store_missing`（`mat fabric init` 誘導付き）。
pub fn load_self_issue_materials(
    cfg: &NativeConfig,
) -> Result<mat_controller::kvs::SelfIssueMaterials, MatError> {
    let alpha_ini = cfg.store.join(ALPHA_INI_FILE);
    let main_ini = cfg.store.join(mat_controller::kvs::MAIN_INI_FILE);
    mat_controller::kvs::read_self_issue_materials(
        &alpha_ini,
        &main_ini,
        cfg.fabric_index,
        cfg.issuer_index,
    )
    .map_err(|e| {
        MatError::new(
            ErrorKind::StoreMissing,
            format!("native: read KVS credentials: {e} — run `mat fabric init`"),
        )
    })
}

/// 資材から NOC を自己発行して `FabricCredentials` を組む。資材はあるが
/// NOC を組めない = 壊れた / 不整合な store → `store_parse`。
pub fn self_issue_credentials(
    materials: mat_controller::kvs::SelfIssueMaterials,
) -> Result<FabricCredentials, MatError> {
    FabricCredentials::from_self_issued(materials).map_err(|e| {
        MatError::new(
            ErrorKind::StoreParse,
            format!("native: self-issue NOC: {e} — run `mat fabric init`"),
        )
    })
}

/// KVS から fabric 資格情報を組み立てる（`Engine::build` の前半）。`fabric
/// rotate-ipk` も同じ経路で読む（epoch を差し替えた別 IPK の確立器を作るため）。
pub fn load_fabric_credentials(cfg: &NativeConfig) -> Result<FabricCredentials, MatError> {
    self_issue_credentials(load_self_issue_materials(cfg)?)
}

/// 運用 iface（`cfg.iface`）の scope_id（ifindex）。解決失敗は `other`。
pub fn op_scope_id(cfg: &NativeConfig) -> Result<u32, MatError> {
    mat_controller::dnssd::iface_index(&cfg.iface).map_err(|e| {
        MatError::new(
            ErrorKind::Other,
            format!("native: resolve iface {:?} index: {e}", cfg.iface),
        )
    })
}

/// 資格情報から実確立器（mDNS 解決 → CASE）を作る（`Engine::build` の後半）。
/// `creds.ipk_operational` を差し替えて渡せば別 epoch の IPK で CASE を張る
/// 確立器になる（rotate-ipk の受理実証）。`scope_id` は `op_scope_id(cfg)`。
pub fn case_establisher(
    cfg: &NativeConfig,
    creds: FabricCredentials,
    resolver: Arc<dyn Resolver>,
    scope_id: u32,
) -> Box<dyn Establisher> {
    Box::new(CaseEstablisher {
        creds: std::sync::RwLock::new(Arc::new(creds)),
        scope_id,
        resolver,
        cfg: cfg.clone(),
    })
}
```

`build_with_resolver`:
- `let scope_id = mat_controller::dnssd::iface_index(&cfg.iface).map_err(…)?;` → `let scope_id = op_scope_id(cfg)?;`
- `let establisher = case_establisher(cfg, creds, resolver)?;` → `let establisher = case_establisher(cfg, creds, resolver, scope_id);`

- [ ] **Step 5: rotate_ipk.rs の呼び手**

`crates/mat-native/src/rotate_ipk.rs` `run()` 内の `make` クロージャ:
```rust
    let make = move |epoch: &[u8; 16]| {
        let mut c = creds.clone();
        c.ipk_operational = fabric::derive_ipk_operational(epoch, &cfid);
        let scope_id = crate::op_scope_id(&cfg)?;
        Ok(crate::case_establisher(&cfg, c, Arc::clone(&resolver), scope_id))
    };
```
（iface 解決は従来どおり確立器生成時に遅延 — エラー順序を変えない。）

- [ ] **Step 6: commission.rs の呼び手**

`crates/mat-native/src/commission.rs` `commission()` 先頭の資材構築（`let scope_id = dnssd::iface_index(…)` から `let creds = match … from_self_issued …` まで）を次に置換:

```rust
    // 資材構築（ローカル — M8c-3 で失敗は種別ごとのハードエラー）。
    let scope_id = crate::op_scope_id(cfg)?;
    let main_ini = cfg.store.join(kvs::MAIN_INI_FILE);
    let materials = crate::load_self_issue_materials(cfg)?;
    // epoch IPK 解決（M8c-3）には fabric の root 公開鍵が要るため一度
    // `FabricCredentials` を組む——`kvs::SelfIssueMaterials` は既に
    // `#[derive(Clone)]` 済み（秘密鍵を持つ型に Clone をここで新規に足す
    // わけではない）ので、安価な INI 再読みではなく `materials.clone()`
    // で賄う。
    let creds = crate::self_issue_credentials(materials.clone())?;
```
`main_ini` は後段の `resolve_ipk_epoch(&main_ini, …)` が使うので残す。`"chip_tool_config.ini"` リテラルは `kvs::MAIN_INI_FILE` へ。不要になった `dnssd` / `fabric` の import は clippy（unused）に従って整理。

- [ ] **Step 7: probe.rs の呼び手**

`crates/mat/src/probe.rs` `resolve_ledger_nodes` の `let scope_id = …` から `let cfid = …` の直前までを次に置換:

```rust
    // 資材・iface 解決は mat-native の 1 本（Engine::build / commission と
    // 同じ写像: iface 解決失敗 = other、KVS 読み取り失敗 = store_missing、
    // NOC 自己発行失敗 = store_parse、いずれも `mat fabric init` 誘導付き）。
    let cfg = mat_native::NativeConfig {
        store: p.store_root.to_path_buf(),
        iface: p.iface.to_string(),
        thread_iface: None,
        fabric_index: p.fabric_index,
        issuer_index: p.issuer_index,
    };
    let scope_id = mat_native::op_scope_id(&cfg)?;
    let creds = mat_native::load_fabric_credentials(&cfg)?;
```
`use mat_controller::{dnssd, fabric, kvs};` → `use mat_controller::{dnssd, fabric};`（`kvs` 不要。`fabric::compressed_fabric_id` は残る）。module doc の「失敗源ごとに ErrorKind を作り分ける … NOC 自己発行失敗は `StoreMissing`」の記述を `StoreParse` に直す。既存テスト `resolve_ledger_nodes_maps_missing_kvs_materials_to_store_missing` はそのまま緑（読み取り失敗 = StoreMissing + `mat fabric init`）。

- [ ] **Step 8: テスト**

```bash
cargo test -p mat-native --all-features 2>&1 | tail -5
cargo test -p mat 2>&1 | tail -5
```
Expected: 全 PASS。`build_fails_cleanly_without_kvs`、`case_establisher_reload_*`、probe の 2 本が緑。

- [ ] **Step 9: コミット**

```bash
git add crates/mat-native/src/lib.rs crates/mat-native/src/commission.rs crates/mat-native/src/rotate_ipk.rs crates/mat/src/probe.rs
git commit -m "refactor(mat-native): 認証情報ロードと iface 解決を load_self_issue_materials / self_issue_credentials / op_scope_id に一本化（Engine::build の iface 二重解決も解消）"
```

---

### Task 4: `establish` / `establish_subscription` → `establish_raw(node_id, role)`、`SessionConn` / `SubscriptionSession` 統合

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`impl Establisher for CaseEstablisher`、`struct SubscriptionSession` とその `impl SubscribeConn`）

**Interfaces:**
- Consumes: `EstablishRole { Op, Subscription }`（既存 private enum）。
- Produces: `SessionConn` が `NodeConn` と `SubscribeConn` の両方を実装する。`SubscriptionSession` は消える。`CaseEstablisher::establish_raw(&self, node_id: u64, role: EstablishRole) -> Result<SessionConn, MatError>`（private）。

- [ ] **Step 1: 現存確認**

```bash
grep -n 'fn establish\|SubscriptionSession\|struct SessionConn\|transport bound' crates/mat-native/src/lib.rs
grep -rn 'transport bound' crates/ --include='*.rs' | grep -v mat-native/src/lib.rs
```
Expected: ログ文言 `"op transport bound (dedicated socket + CASE)"` / `"subscription transport bound (dedicated socket + CASE)"` を他ファイルは pin していない（2 つ目の grep が空）。

- [ ] **Step 2: `EstablishRole` にログ用ラベルを足し、`establish_raw` を書く**

```rust
/// `map_establish_err` の detail 前置き分岐（op / 購読でログ・detail の
/// 文言を従来どおり出し分ける）。
#[derive(Clone, Copy)]
enum EstablishRole {
    Op,
    Subscription,
}

impl EstablishRole {
    /// "op transport bound …" / "subscription transport bound …" のログ用。
    fn log_label(self) -> &'static str {
        match self {
            EstablishRole::Op => "op",
            EstablishRole::Subscription => "subscription",
        }
    }
}

impl CaseEstablisher {
    // …既存 creds / swap_credentials の後に:

    /// mDNS 解決 → 専用 UdpTransport + CASE（`case::establish_any` の
    /// staggered race）。op / 購読の違いはエラー detail とログの前置きだけ。
    async fn establish_raw(
        &self,
        node_id: u64,
        role: EstablishRole,
    ) -> Result<SessionConn, MatError> {
        // 専用ソケット: 共有ソケットでは並行 op が他ノード宛の応答を recv して
        // screen で捨てる（監査#3）。購読も op 用 transport と recv を奪い合わ
        // ないようノードごとに専用（spec 構造判断）。試行ごとの bind と候補
        // アドレスの staggered race（Happy Eyeballs）は `case::establish_any`
        // が一括して行う。
        let creds = self.creds();
        let cfid = compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| map_resolve_err(node_id, e))?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let est = case::establish_any(&peers, &creds, node_id, &mrp, case::RACE_STAGGER)
            .await
            .map_err(|e| map_establish_err(node_id, role, e))?;
        // local port は実機切り分け（ss -uanp / tcpdump 突合）の鍵なので
        // 確立ごとに可視化する（op / 購読で同形）。
        tracing::info!(
            node_id,
            local = %est.local.map(|a| a.to_string()).unwrap_or_default(),
            peer = %est.peer,
            "{} transport bound (dedicated socket + CASE)",
            role.log_label()
        );
        Ok(SessionConn {
            session: est.session,
            mrp,
        })
    }
}

#[async_trait]
impl Establisher for CaseEstablisher {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        Ok(Box::new(self.establish_raw(node_id, EstablishRole::Op).await?))
    }

    async fn establish_subscription(
        &self,
        node_id: u64,
    ) -> Result<Box<dyn SubscribeConn>, MatError> {
        Ok(Box::new(
            self.establish_raw(node_id, EstablishRole::Subscription)
                .await?,
        ))
    }

    fn reload_credentials(&self) -> Result<bool, MatError> {
        // …既存のまま
    }
}
```

- [ ] **Step 3: `SubscriptionSession` を消し、`impl SubscribeConn for SessionConn` に付け替える**

`struct SubscriptionSession { … }` と doc を削除。`impl SubscribeConn for SubscriptionSession` → `impl SubscribeConn for SessionConn`（本体不変）。`struct SessionConn` の doc を「実セッション: SecureSession + そのノードの MRP 設定。op（`NodeConn`）と購読（`SubscribeConn`）は同じ型で、確立時の役割（専用ソケット + 専用 CASE）が違うだけ。」にする。`map_session_err` のコメント内 `SubscriptionSession::next_report` → `SessionConn::next_report_full`。

- [ ] **Step 4: テスト**

```bash
cargo test -p mat-native --all-features 2>&1 | tail -5
cargo clippy -p mat-native --all-targets --all-features -- -D warnings 2>&1 | tail -5
```
Expected: 全 PASS、clippy 警告 0（`dedicated_op_socket_tests` の 2 本がループバック CASE 応答器で通る）。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-native/src/lib.rs
git commit -m "refactor(mat-native): establish / establish_subscription を establish_raw(node_id, role) に統合、SessionConn が NodeConn と SubscribeConn の両方を実装"
```

---

### Task 5: epoch key の bytes API、`{0: u16}` encoder 統合、shortcut wire 三つ組、`open_egress`

**Files:**
- Modify: `crates/mat-core/src/group.rs`（`generate_epoch_key_bytes` / `resolve_epoch_key_bytes` 追加 — string 版は mat が使うので残す）
- Modify: `crates/mat-native/src/runner.rs:120-121`（`resolve_epoch_key_bytes` へ）
- Modify: `crates/mat-native/src/ops.rs`（`epoch_key_from_hex` 削除、`encode_remove_group_fields` / `encode_key_set_remove_fields` → `encode_ctx0_u16`）
- Modify: `crates/mat-native/src/rotate_ipk.rs`（`fresh_epoch` → `generate_epoch_key_bytes`、`decode_generated_epoch_hex` 削除、テストの hex 手組み → `mat_core::hex::encode_lower`）
- Modify: `crates/mat-native/src/op.rs`（Color / ColorTemp / Level の (cluster, command, fields) 三つ組を `shortcut` 関数 3 本に）
- Modify: `crates/mat-native/src/group.rs`（`open_egress` を pub(crate) に定義し `acquire_late_thread_egress` で使う）
- Modify: `crates/mat-native/src/lib.rs`（`build_with_resolver` の thread egress bind を `group::open_egress` へ）

**Interfaces:**
- Produces:
  - `mat_core::group::generate_epoch_key_bytes() -> [u8; 16]`、`mat_core::group::resolve_epoch_key_bytes(epoch_key: Option<&str>) -> Result<[u8; 16], MatError>`（`Some` は `validate_epoch_key` 経由なので kind `Other` の検証エラーは不変）。
  - `mat_native::op::shortcut::{color(color: &ResolvedColor, transition: u16), color_temp(mireds: u16, transition: u16), level(level: u8, transition: u16)} -> (u32, u32, Option<Vec<u8>>)`（pub(crate) mod）。
  - `mat_native::group::open_egress(name: &str, scope_id: u32) -> Result<GroupEgress, std::io::Error>`（pub(crate)、`UdpTransport::bind` + 構築）。

- [ ] **Step 1: mat-core group.rs のテストを先に更新**

`crates/mat-core/src/group.rs` の `mod tests` に追加:

```rust
    #[test]
    fn generated_epoch_key_bytes_are_random_and_hex_form_matches() {
        let a = generate_epoch_key_bytes();
        let b = generate_epoch_key_bytes();
        assert_ne!(a, b);
        // string 版は bytes 版の小文字 hex（両 API の一致を釘打ち）。
        let s = generate_epoch_key();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn resolve_epoch_key_bytes_decodes_explicit_key_and_normalizes_case() {
        let k = resolve_epoch_key_bytes(Some("0x00112233445566778899AABBCCDDEEFF")).unwrap();
        assert_eq!(
            k,
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert_eq!(resolve_epoch_key_bytes(Some("dead")).unwrap_err().kind, ErrorKind::Other);
        // None = 生成（2 回で異なる）。
        assert_ne!(resolve_epoch_key_bytes(None).unwrap(), resolve_epoch_key_bytes(None).unwrap());
    }
```

- [ ] **Step 2: 失敗確認**

```bash
cargo test -p mat-core group:: 2>&1 | tail -5
```
Expected: コンパイルエラー。

- [ ] **Step 3: mat-core group.rs を実装**

```rust
/// ランダムな 16 バイトの epoch key。
pub fn generate_epoch_key_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("getrandom failed to fill epoch key");
    bytes
}

/// ランダムな 16 バイトの epoch key を生成し 32桁 hex で返す（CLI 表示・
/// ワイヤ用。バイト列が要る呼び手は [`generate_epoch_key_bytes`]）。
pub fn generate_epoch_key() -> String {
    crate::hex::encode_lower(&generate_epoch_key_bytes())
}

/// epoch key を決める: 明示指定があれば検証して採用、無ければランダム生成。
pub fn resolve_epoch_key(epoch_key: Option<&str>) -> Result<String, MatError> {
    match epoch_key {
        Some(k) => validate_epoch_key(k),
        None => Ok(generate_epoch_key()),
    }
}

/// [`resolve_epoch_key`] のバイト列版。hex → bytes の往復を呼び手（provision /
/// rotate-ipk）が各自やっていたのを一本化する。
pub fn resolve_epoch_key_bytes(epoch_key: Option<&str>) -> Result<[u8; 16], MatError> {
    match epoch_key {
        Some(k) => {
            let hex = validate_epoch_key(k)?;
            let bytes = crate::hex::decode(&hex).expect("validated as 32 hex chars");
            Ok(<[u8; 16]>::try_from(bytes).expect("32 hex chars = 16 bytes"))
        }
        None => Ok(generate_epoch_key_bytes()),
    }
}
```

```bash
cargo test -p mat-core 2>&1 | tail -3
```
Expected: PASS。

- [ ] **Step 4: mat-native の呼び手を置換**

`crates/mat-native/src/runner.rs` `provision()`:
```rust
    let epoch_key = mat_core::group::resolve_epoch_key_bytes(p.epoch_key.as_deref())?;
```
（`epoch_key_hex` と `crate::ops::epoch_key_from_hex` の 2 行を 1 行に。）

`crates/mat-native/src/ops.rs`: `pub fn epoch_key_from_hex` を doc ごと削除（呼び手は runner.rs だけだった — `grep -rn epoch_key_from_hex crates/` で確認）。

`crates/mat-native/src/rotate_ipk.rs`:
```rust
/// CSPRNG の新 epoch（現行と一致したら引き直す）。鍵素材は format! に渡さない。
fn fresh_epoch(cur: &[u8; 16]) -> [u8; 16] {
    loop {
        let e = mat_core::group::generate_epoch_key_bytes();
        if e != *cur {
            return e;
        }
    }
}
```
`decode_generated_epoch_hex` を削除。`fresh_epoch(...)?` の呼び手（`rotate()` 内）から `?` を外す。テスト `rotate_two_nodes_…`（724 行付近）の `next.iter().map(|b| format!("{b:02x}")).collect()` → `mat_core::hex::encode_lower(&next)`。「鍵バイトが body に漏れない」assert は維持。

- [ ] **Step 5: `{0: u16}` encoder を 1 本に**

`crates/mat-native/src/ops.rs`:
```rust
/// `{0: <u16>}` 形の CommandFields（RemoveGroup `{0: groupID}` / KeySetRemove
/// `{0: groupKeySetID}` は同形）。
fn encode_ctx0_u16(value: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(value));
    w.end_container();
    w.finish()
}
```
`encode_remove_group_fields(p.group_id)` → `encode_ctx0_u16(p.group_id)`、`encode_key_set_remove_fields(ks)` → `encode_ctx0_u16(ks)`。旧 2 関数を削除。

- [ ] **Step 6: shortcut wire 三つ組**

`crates/mat-native/src/op.rs` に `run_node_op` の前へ追加:

```rust
/// ショートカット op（color / color-temp / level）の (cluster, command,
/// CommandFields) 三つ組。単一ノード（`run_node_op`）と groupcast
/// （`GroupOpKind::wire`）が同じワイヤを出すことをここで保証する。
pub(crate) mod shortcut {
    use mat_controller::im;
    use mat_core::color::ResolvedColor;

    pub fn color(color: &ResolvedColor, transition: u16) -> (u32, u32, Option<Vec<u8>>) {
        (
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(im::encode_move_to_hue_and_saturation_fields(
                color.hue_raw,
                color.sat_raw,
                transition,
            )),
        )
    }

    pub fn color_temp(mireds: u16, transition: u16) -> (u32, u32, Option<Vec<u8>>) {
        (
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_COLOR_TEMPERATURE,
            Some(im::encode_move_to_color_temperature_fields(mireds, transition)),
        )
    }

    pub fn level(level: u8, transition: u16) -> (u32, u32, Option<Vec<u8>>) {
        (
            im::CLUSTER_LEVEL_CONTROL,
            im::CMD_MOVE_TO_LEVEL,
            Some(im::encode_move_to_level_fields(level, transition)),
        )
    }
}
```

`run_node_op` の 3 アーム:
```rust
        NodeOpKind::Color { endpoint, color, transition } => {
            let (cluster, command, fields) = shortcut::color(color, *transition);
            conn.invoke(*endpoint, cluster, command, fields, false).await?;
            body::color_success(node_id, *endpoint, color, *transition)
        }
        NodeOpKind::ColorTemp { endpoint, kelvin, mireds, transition } => {
            let (cluster, command, fields) = shortcut::color_temp(*mireds, *transition);
            conn.invoke(*endpoint, cluster, command, fields, false).await?;
            body::color_temp_success(node_id, *endpoint, *kelvin, *mireds, *transition)
        }
        NodeOpKind::Level { endpoint, percent, level, transition } => {
            let (cluster, command, fields) = shortcut::level(*level, *transition);
            conn.invoke(*endpoint, cluster, command, fields, false).await?;
            body::level_success(node_id, *endpoint, body::LevelEcho { percent: *percent, level: *level }, *transition)
        }
```
`GroupOpKind::wire`:
```rust
            GroupOpKind::Color { color, transition } => shortcut::color(color, *transition),
            GroupOpKind::ColorTemp { mireds, transition, .. } => shortcut::color_temp(*mireds, *transition),
            GroupOpKind::Level { level, transition, .. } => shortcut::level(*level, *transition),
```
op.rs のテストに追加（`mod tests` 内）:
```rust
    /// 単一ノードと groupcast のショートカットは同じワイヤ（監査 Tier 3）。
    #[tokio::test]
    async fn node_and_group_shortcuts_share_wire() {
        let color = mat_core::color::from_hue_sat(0, 100);
        let mut conn = FakeConn::default();
        run_node_op(&mut conn, &node(NodeOpKind::Color { endpoint: 1, color: color.clone(), transition: 3 })).await.unwrap();
        run_node_op(&mut conn, &node(NodeOpKind::color_temp(1, Some(2700), None, 3))).await.unwrap();
        run_node_op(&mut conn, &node(NodeOpKind::level(1, 50, 3))).await.unwrap();
        let group = [
            GroupOpKind::Color { color, transition: 3 }.wire(),
            GroupOpKind::color_temp(Some(2700), None, 3).wire(),
            GroupOpKind::level(50, 3).wire(),
        ];
        for (i, (cluster, command, fields)) in group.into_iter().enumerate() {
            let (ep, c, cmd, f) = &conn.invoked_fields[i];
            assert_eq!((*ep, *c, *cmd), (1, cluster, command));
            assert_eq!(f, &fields.unwrap_or_default());
        }
    }
```
（`ResolvedColor` は `Clone` 済み — `crates/mat-core/src/color.rs:70`。）

- [ ] **Step 7: `open_egress`**

`crates/mat-native/src/group.rs` に追加（`acquire_late_thread_egress` の直前）:
```rust
/// 名前解決済み iface へ groupcast egress を 1 本張る（専用 UdpTransport の
/// bind + `GroupEgress` 構築）。build 時（`Engine::build_with_resolver`）と
/// 送信時後付け（`acquire_late_thread_egress`）が共有する。
pub(crate) async fn open_egress(name: &str, scope_id: u32) -> Result<GroupEgress, std::io::Error> {
    let transport = UdpTransport::bind().await?;
    Ok(GroupEgress {
        iface: name.to_string(),
        transport: Arc::new(transport),
        scope_id,
    })
}
```
`acquire_late_thread_egress` の `let transport = match UdpTransport::bind().await { … }` と `sender.add_egress(GroupEgress { … })` を:
```rust
    let egress = match open_egress(&name, scope_id).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(iface = %name, error = %e,
                "late thread egress socket bind failed; groupcast stays LAN-only");
            return;
        }
    };
    match sender.add_egress(egress) {
```
`crates/mat-native/src/lib.rs` `build_with_resolver` の `Ok(Some((name, tsid))) => { match UdpTransport::bind().await { Ok(t) => { info!; egress.push(GroupEgress {…}) } Err(e) => … } }` を:
```rust
            Ok(Some((name, tsid))) => {
                // Thread egress は専用 socket（LAN 側の IPV6_MULTICAST_IF と独立）。
                match group::open_egress(&name, tsid).await {
                    Ok(e) => {
                        tracing::info!(iface = %name, "groupcast thread egress enabled");
                        egress.push(e);
                    }
                    Err(e) => match &cfg.thread_iface {
                        // …既存のまま（Explicit はハードエラー、他は warn）
                    },
                }
            }
```
`UdpTransport::bind` の import が lib.rs / group.rs で未使用になったら削る（lib.rs の LAN 側 `UdpTransport::bind()` は残る）。

- [ ] **Step 8: テスト**

```bash
cargo test -p mat-core -p mat-native --all-features 2>&1 | tail -5
```
Expected: 全 PASS（remove_group の `invoke_for_data(2,0x0004,0x0003)` 記録、rotate_ipk の KeySetWrite leaf テスト、provision の keyset 0 拒否が緑）。

- [ ] **Step 9: コミット**

```bash
git add crates/mat-core/src/group.rs crates/mat-native/src/runner.rs crates/mat-native/src/ops.rs crates/mat-native/src/rotate_ipk.rs crates/mat-native/src/op.rs crates/mat-native/src/group.rs crates/mat-native/src/lib.rs
git commit -m "refactor(native-core): epoch key の bytes API、{0:u16} encoder・ショートカット wire・open_egress の重複を一本化"
```

---

### Task 6: `mat-core::mesh::build_graph`（411 行）を 6 フェーズ関数に分割

**Files:**
- Modify: `crates/mat-core/src/mesh.rs:283-694`

**Interfaces:**
- Consumes: 既存 private ヘルパ（`table_rows` / `row_ext_hex` / `row_rloc16` / `is_routeless_row` / `retain_unique_values` / `link_metrics` / `ordered` / `fabric_vertex_id` / `canon_ml_prefix` / `derive_rloc16` / `role_from_routing_role` / `router_id_of` / `rloc16_str`、`SELF_ROW_MAX_AGE`、`struct Part`）。
- Produces（すべて private、`build_graph` の公開シグネチャ不変）:
  ```rust
  struct NetworkScan { name: Option<String>, channel: Option<u64>, leader_router_id: Option<u64>, ml_prefix: Option<String>, partition_ids: Vec<u64> }
  fn scan_network(inputs: &[NodeInput]) -> NetworkScan;                                   // 旧フェーズ 1
  fn identify_fabric_nodes(inputs: &[NodeInput], ml_prefix: Option<&str>) -> (BTreeMap<u64, String>, BTreeMap<u64, u16>); // 旧フェーズ 2（retain_unique_values 2 回まで含む）
  fn rescue_identities(inputs: &[NodeInput], partition_ids: &[u64], self_ext: &mut BTreeMap<u64, String>, self_rloc: &mut BTreeMap<u64, u16>) -> BTreeMap<u64, &'static str>; // 旧フェーズ 2b（末尾の self_rloc.retain + retain_unique_values まで含む）
  fn collect_participants(inputs: &[NodeInput]) -> BTreeMap<String, Part>;                // 旧フェーズ 3
  struct EdgeAcc { a_sees_b: Option<LinkMetrics>, b_sees_a: Option<LinkMetrics>, route_a: Option<RouteMetrics>, route_b: Option<RouteMetrics> } // 関数内 struct をモジュール階層へ
  fn collect_edges(inputs: &[NodeInput], self_ext: &BTreeMap<u64, String>) -> BTreeMap<(String, String), EdgeAcc>; // 旧フェーズ 4
  fn emit_nodes(inputs: &[NodeInput], self_ext: &BTreeMap<u64, String>, self_rloc: &BTreeMap<u64, u16>, parts: &BTreeMap<String, Part>, ident_by: &BTreeMap<u64, &'static str>, thread_labels: &BTreeMap<String, String>, leader_router_id: Option<u64>) -> Vec<MeshNode>; // 旧フェーズ 5
  fn emit_edges(edges: BTreeMap<(String, String), EdgeAcc>) -> Vec<MeshEdge>;              // 旧フェーズ 6
  ```

- [ ] **Step 1: 現存確認とベースライン**

```bash
grep -n 'pub fn build_graph\|// 1\. network\|// 2\. fabric\|// 2b\.\|// 3\. 参加者\|// 4\. エッジ集約\|// 5\. ノード出力\|// 6\. エッジ出力' crates/mat-core/src/mesh.rs
cargo test -p mat-core mesh:: 2>&1 | grep 'test result'
```
Expected: 7 箇所のフェーズコメントが見つかり、mesh のテスト（22 本以上）が全 PASS。件数を控える。

- [ ] **Step 2: 純粋な切り出し（コード本文は 1 文字も変えない）**

`build_graph` の各フェーズのブロックをそのまま上記シグネチャの関数へ移し、`build_graph` を次にする:

```rust
pub fn build_graph(inputs: &[NodeInput], thread_labels: &BTreeMap<String, String>) -> MeshGraph {
    // 1. network サマリ（最初に読めた値を採用）+ mesh-local-prefix。
    let net = scan_network(inputs);
    // 2. fabric ノードの自己同定（node_id → 正準 ext hex / rloc16）。
    let (mut self_ext, mut self_rloc) = identify_fabric_nodes(inputs, net.ml_prefix.as_deref());
    // 2b. 自己同定できなかった probed ノードの救済（issue #13）。
    let ident_by = rescue_identities(inputs, &net.partition_ids, &mut self_ext, &mut self_rloc);
    // 3. 参加者台帳（ext hex → 証拠）。
    let parts = collect_participants(inputs);
    // 4. エッジ集約（無向、キーは辞書順ペア）。
    let edges = collect_edges(inputs, &self_ext);
    // 5. ノード出力: fabric ノード（入力順）→ 未知参加者（ext 昇順）。
    let nodes = emit_nodes(
        inputs,
        &self_ext,
        &self_rloc,
        &parts,
        &ident_by,
        thread_labels,
        net.leader_router_id,
    );
    // 6. エッジ出力（キー昇順、route は a 視点優先）。
    let edges = emit_edges(edges);

    MeshGraph {
        network: NetworkSummary {
            name: net.name,
            channel: net.channel,
            partition_ids: net.partition_ids,
            leader_router_id: net.leader_router_id,
        },
        nodes,
        edges,
    }
}
```

移設時の注意:
- フェーズ 2 の `ml_prefix.clone()` フォールバックは `ml_prefix.map(str::to_string)` に（引数が `Option<&str>` になるため）。
- フェーズ 2b は元コードの `let mut ident_by = BTreeMap::new(); { … }` ブロックの中身 + 直後の `self_rloc.retain(|n, _| self_ext.contains_key(n)); retain_unique_values(&mut self_rloc);` までを 1 関数にし、`ident_by` を返す。
- フェーズ 4 の `#[derive(Default)] struct EdgeAcc` は関数の外（`struct Part` の直後）へ移す。
- 各フェーズの説明コメント（NL68 FW バグ、issue #13、2026-07-23 実機 E2E 等）は移設先の関数 doc / 本文に**そのまま**残す。
- 各関数に 1 行 doc（「`build_graph` フェーズ N: …」）を付ける。

- [ ] **Step 3: テスト（出力 pin が全部緑）**

```bash
cargo test -p mat-core mesh:: 2>&1 | grep 'test result'
cargo clippy -p mat-core --all-targets -- -D warnings 2>&1 | tail -3
```
Expected: Step 1 と同じ件数で全 PASS、clippy 0（`too_many_arguments` が出たら `emit_nodes` に `#[allow(clippy::too_many_arguments)]` を付けるのではなく、`self_ext`/`self_rloc`/`ident_by` を `struct Identities { ext, rloc, by }` にまとめる）。

- [ ] **Step 4: コミット**

```bash
git add crates/mat-core/src/mesh.rs
git commit -m "refactor(mat-core): mesh::build_graph を 6 フェーズ関数へ純粋分割（出力不変、22 テスト pin）"
```

---

### Task 7: Tier 5 機械的分割 — op.rs / lib.rs のテスト別ファイル化、`put_value` 系 → op.rs、Resolver → resolver.rs、`map_*_err` → errmap.rs

**Files:**
- Create: `crates/mat-native/src/op/tests.rs`（op.rs の `mod tests` 本体）
- Create: `crates/mat-native/src/tests/mod.rs`（lib.rs の `mod tests` 本体）
- Create: `crates/mat-native/src/tests/dedicated_op_socket.rs`（lib.rs の `mod dedicated_op_socket_tests` 本体）
- Create: `crates/mat-native/src/resolver.rs`
- Create: `crates/mat-native/src/errmap.rs`
- Modify: `crates/mat-native/src/op.rs`（`put_value` / `arg_value_to_tlv` / `encode_command_fields` を受け入れ、`crate::` 呼びを局所へ）
- Modify: `crates/mat-native/src/lib.rs`（≈400 行に）

**Interfaces:**
- 公開 API は **re-export で不変**（mat / matd が `mat_native::{encode_command_fields, arg_value_to_tlv, put_value, Resolver, OneShotResolver, CachingResolver, CACHE_MISS_TIMEOUT}` を使う）:
  ```rust
  // lib.rs
  pub mod errmap;   // pub(crate) 関数のみ — 外からは見えない
  pub mod resolver;
  pub use op::{arg_value_to_tlv, encode_command_fields, put_value};
  pub use resolver::{CachingResolver, OneShotResolver, Resolver, CACHE_MISS_TIMEOUT};
  use errmap::{map_commission_err, map_establish_err, map_resolve_err, map_session_err, EstablishRole};
  ```
- `errmap.rs`: `pub(crate) fn map_resolve_err / map_session_err / map_commission_err / map_establish_err`、`pub(crate) enum EstablishRole`（Task 4 の `log_label` 含む）。
- `resolver.rs`: `Resolver` trait、`OneShotResolver`、`CachingResolver`、`CACHE_MISS_TIMEOUT`、`CACHE_POLL`。

- [ ] **Step 1: ベースライン**

```bash
cargo test -p mat-native --all-features 2>&1 | grep 'test result'
wc -l crates/mat-native/src/lib.rs crates/mat-native/src/op.rs
```
件数を控える（分割後に同数であること）。

- [ ] **Step 2: op.rs のテストを `op/tests.rs` へ**

op.rs の `#[cfg(test)] mod tests { … }` の中身を `crates/mat-native/src/op/tests.rs` に移し（先頭の `use super::*;` はそのまま — `super` は引き続き `op`）、op.rs 末尾を:
```rust
#[cfg(test)]
mod tests;
```
にする。`op.rs` はファイルのまま（`op/mod.rs` にしない — 2018 edition 以降は `op.rs` + `op/tests.rs` で動く）。

- [ ] **Step 3: `put_value` / `arg_value_to_tlv` / `encode_command_fields` を op.rs へ**

lib.rs の 3 関数（doc 含む）を op.rs の `units` モジュールの直前へ移す。op.rs 内の `crate::encode_command_fields(&fields)` → `encode_command_fields(&fields)`、`crate::arg_value_to_tlv(value)` → `arg_value_to_tlv(value)`（op/tests.rs の `crate::arg_value_to_tlv` も同様）。lib.rs に `pub use op::{arg_value_to_tlv, encode_command_fields, put_value};` を追加。lib.rs のテストのうち `arg_value_conversions`、`put_value_encodes_list_of_struct_as_tlv_array_and_roundtrips_to_read_json`、`generic_acl_encoding_matches_dedicated_encoder`、`generic_group_key_map_encoding_matches_dedicated_encoder`、`generic_key_set_write_encoding_matches_dedicated_encoder`、`encode_command_fields_uses_positional_context_tags` の 6 本を `op/tests.rs` へ移す（必要な `use` は `super::*` で足りる — 足りなければ `use mat_controller::tlv::…` を足す）。

- [ ] **Step 4: Resolver 系を `resolver.rs` へ**

lib.rs の `trait Resolver` / `OneShotResolver` / `CachingResolver` / `CACHE_MISS_TIMEOUT` / `CACHE_POLL`（doc 含む）を `crates/mat-native/src/resolver.rs` に移す:
```rust
//! establish の mDNS 解決を差し替え可能にする抽象。`mat`（一発）は
//! [`OneShotResolver`]（キャッシュ無し＝設計ルール4）、`matd` は
//! [`CachingResolver`]（常駐キャッシュ）を注入する。

use std::time::Duration;

use async_trait::async_trait;
use mat_controller::dnssd;
// …本体は lib.rs から逐語移設
```
lib.rs に `pub mod resolver;` と `pub use resolver::{CachingResolver, OneShotResolver, Resolver, CACHE_MISS_TIMEOUT};`。テスト `cache_miss_timeout_is_pinned`、`oneshot_resolver_times_out_without_responder`（+ helper `multicast_capable_iface_index`）、`caching_resolver_returns_cached_hit_immediately`、`caching_resolver_awaits_listener_fill_then_returns`、`caching_resolver_times_out_when_never_filled` を `resolver.rs` の `#[cfg(test)] mod tests` へ移す。

- [ ] **Step 5: `map_*_err` を `errmap.rs` へ**

lib.rs の `map_resolve_err` / `map_session_err` / `map_commission_err` / `map_establish_err` / `enum EstablishRole`（+ `impl EstablishRole`）を `crates/mat-native/src/errmap.rs` へ（可視性は `pub(crate)`）:
```rust
//! mat-controller のエラー（dnssd / session / commissioning / CASE 確立）を
//! mat の `ErrorKind` へ写像する表。経路（mat 直経路 / matd）によらず分類を
//! 揃えるため 1 箇所に置く。

use mat_controller::{case, dnssd};
use mat_core::error::{ErrorKind, MatError};
// …本体は逐語移設
```
lib.rs に `mod errmap;` と `use errmap::{map_commission_err, map_establish_err, map_resolve_err, map_session_err, EstablishRole};`。テスト `resolve_timeout_maps_to_timeout_kind`、`map_session_err_maps_malformed_message_to_parse_error`、`map_session_err_splits_im_decode_failure_from_device_rejection`、`invalid_argument_maps_to_parse_error` を `errmap.rs` の `#[cfg(test)] mod tests` へ。

- [ ] **Step 6: lib.rs の残りテストを `tests/` へ**

lib.rs に残った `mod tests` の中身（thread_egress_* / generic_read_write_via_fake / build_fails_cleanly_without_kvs / load_fabric_credentials_* / load_self_issue_materials_* / op_scope_id_* / default_establisher_rejects_subscription / fake_* / establisher_reload_* / check_identity_* / case_establisher_* / bootstrapped_store / establisher_over_store）を `crates/mat-native/src/tests/mod.rs` に移す。先頭は `use super::*;`（`super` = crate root、private 項目は子孫から見える）。`mod dedicated_op_socket_tests` の中身を `crates/mat-native/src/tests/dedicated_op_socket.rs` に移し、先頭を `use crate::*;` にする。lib.rs 末尾:
```rust
#[cfg(test)]
mod tests;
```
`tests/mod.rs` の先頭に `mod dedicated_op_socket;` を置く。

- [ ] **Step 7: テスト件数一致・行数・clippy**

```bash
cargo test -p mat-native --all-features 2>&1 | grep 'test result'
wc -l crates/mat-native/src/lib.rs
cargo clippy -p mat-native --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo doc -p mat-native --no-deps 2>&1 | grep -i warn
```
Expected: 件数が Step 1 と同じ（Task 3/5 で足したテスト分を含めて）、lib.rs ≈ 400 行、clippy / rustdoc 警告 0（intra-doc link `[`OneShotResolver`]` 等は re-export 先で解決する — 壊れたら `[`resolver::OneShotResolver`]` に直す）。

- [ ] **Step 8: コミット**

```bash
git add crates/mat-native/src/op.rs crates/mat-native/src/op/tests.rs crates/mat-native/src/lib.rs crates/mat-native/src/tests crates/mat-native/src/resolver.rs crates/mat-native/src/errmap.rs
git commit -m "refactor(mat-native): op.rs / lib.rs のテストを別ファイルへ、put_value 系を op.rs、Resolver を resolver.rs、map_*_err を errmap.rs に分割（公開 API は re-export で不変）"
```

---

### Task 8: Tier 6 テスト基盤 — `iface_select::scan()` 公開 + `IfaceInfo.index`、`FakeConn::gate_send`、`with_group_provision_fixture()`

**Files:**
- Modify: `crates/mat-native/src/iface_select.rs`（`scan` / `eligible` を pub、`IfaceInfo.index: u32` 追加）
- Modify: `crates/mat-native/src/resolver.rs`（テスト helper `multicast_capable_iface_index` を `scan()` から導出）
- Modify: `crates/mat-native/src/test_support.rs`（`multicast_capable_interfaces` を `scan()` から導出、`gate_send`、`invoke_for_data` の gate、`with_group_provision_fixture`）
- Modify: `crates/mat-native/src/ops.rs`（`provision_node_runs_steps_in_order` 等のフィクスチャ 4 箇所を `with_group_provision_fixture()` へ — `"254": 2` の管理者エントリのみのものだけ。Group エントリ付きの 1 箇所は残す）
- Modify: `crates/mat-native/src/runner.rs`（テストのフィクスチャ 2 箇所）

**Interfaces:**
- Produces:
  ```rust
  // iface_select.rs
  pub struct IfaceInfo { pub name: String, pub flags: u32, pub operstate_up: bool, pub has_ipv6_ll: bool, pub index: u32 } // index: /sys/class/net/<name>/ifindex、読めなければ 0
  pub fn eligible(i: &IfaceInfo) -> bool;
  pub fn scan() -> std::io::Result<Vec<IfaceInfo>>;
  // test_support.rs
  impl FakeConn {
      async fn gate_send(&mut self) -> Result<(), MatError>;               // private: delay → sent++ → fail_first_send/fail_at 判定
      pub fn with_group_provision_fixture() -> Self;                      // scripted + group-key-map [] + ACL 管理者のみ
  }
  ```
- `invoke_for_data` も `gate_send` を通る（= `sent` を進め、`fail_at` が効く）。既存の `fail_at` 利用は runner.rs:420（provision: invoke(0) 成功 → write_tlv(1) 失敗）だけで、provision は `invoke_for_data` を呼ばないので番号はずれない。

- [ ] **Step 1: iface_select のテストを先に更新**

`crates/mat-native/src/iface_select.rs` の `ifi()` helper に `index: 0` を足し、テストを追加:
```rust
    fn ifi(name: &str, flags: u32, up: bool, ll: bool) -> IfaceInfo {
        IfaceInfo {
            name: name.into(),
            flags,
            operstate_up: up,
            has_ipv6_ll: ll,
            index: 0,
        }
    }

    /// `scan()` は実環境依存だが、少なくとも `lo` は index 付きで列挙される
    /// （テスト基盤が multicast 可能 iface を index で選ぶための前提）。
    #[test]
    fn scan_lists_loopback_with_index() {
        let infos = scan().expect("linux sysfs");
        let lo = infos.iter().find(|i| i.name == "lo").expect("lo exists");
        assert!(lo.index >= 1, "ifindex is 1-based");
        assert!(!eligible(lo), "loopback is never eligible");
    }
```

- [ ] **Step 2: 失敗確認**

```bash
cargo test -p mat-native --all-features iface_select 2>&1 | tail -5
```
Expected: コンパイルエラー（`index` フィールド無し / `scan` private ではなく「フィールド未定義」）。

- [ ] **Step 3: iface_select.rs を実装**

- `IfaceInfo` に `/// `/sys/class/net/<name>/ifindex`（読めなければ 0 — Linux の ifindex は 1 始まり）。` `pub index: u32,` を追加。
- `fn eligible` → `pub fn eligible`（doc: 「autodetect の適格条件（up・MULTICAST・非 loopback・非 POINTOPOINT・IPv6 link-local）。テスト基盤も同じ条件で multicast 可能 iface を選ぶ。」）。
- `fn scan` → `pub fn scan`（doc に「本番 `autodetect` とテスト基盤（`test_support::multicast_capable_interfaces`、resolver テスト）の共有スキャナ。」を追記）。`infos.push(IfaceInfo { … })` の前に:
  ```rust
        let index = std::fs::read_to_string(base.join("ifindex"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
  ```
  を足して `index` を入れる。

- [ ] **Step 4: 2 本の手書きスキャンを `scan()` から導出**

`crates/mat-native/src/resolver.rs` のテスト helper:
```rust
    /// resolve が実際に multicast 送受信できる iface の index を1つ探す。
    /// `crate::iface_select`（M8c-3 iface 自動検出）と同じ適格条件を使うが、
    /// こちらは複数候補でも先頭を採用する（本番の autodetect は曖昧なら
    /// ハードエラーだが、このテストは delegation の検証に使える iface が
    /// 1つあれば十分）。`flags`/`lo` だけで判定すると sandbox の `docker0` /
    /// `loopback0` / `tailscale0` を拾って send が `ENETUNREACH` で即死し、
    /// 意図した Timeout 経路を検証できなくなる。
    fn multicast_capable_iface_index() -> Option<u32> {
        crate::iface_select::scan()
            .ok()?
            .into_iter()
            .find(|i| crate::iface_select::eligible(i) && i.index != 0)
            .map(|i| i.index)
    }
```

`crates/mat-native/src/test_support.rs` `multicast_capable_interfaces`（判定条件・順序は従来どおり: `lo` 除外、IFF_UP かつ IFF_MULTICAST、operstate up を先に、各群 index 昇順）:
```rust
pub fn multicast_capable_interfaces() -> Vec<McastCandidate> {
    const IFF_UP: u32 = 0x1;
    const IFF_MULTICAST: u32 = 0x1000;
    let infos = crate::iface_select::scan().unwrap_or_default();
    let mut up_first = Vec::new();
    let mut rest = Vec::new();
    for i in infos {
        if i.name == "lo" || i.flags & IFF_UP == 0 || i.flags & IFF_MULTICAST == 0 || i.index == 0 {
            continue;
        }
        let candidate = McastCandidate { name: i.name, index: i.index };
        if i.operstate_up {
            up_first.push(candidate);
        } else {
            rest.push(candidate);
        }
    }
    up_first.sort_by_key(|c| c.index);
    rest.sort_by_key(|c| c.index);
    up_first.extend(rest);
    up_first
}
```
doc の「`mat_controller::group` … private there, so duplicated here」の記述は「`crate::iface_select::scan()` から導出（mat-controller 側の同種 2 本は別レーンで統合）」に直す。

- [ ] **Step 5: `gate_send` と `invoke_for_data` の gate**

`crates/mat-native/src/test_support.rs` `impl FakeConn` に追加:
```rust
    /// 送信系メソッド共通の前置き: `delay` → `sent` を進める → `fail_first_send`
    /// / `fail_at` の番号に当たれば `fail_kind` で失敗。read_onoff / invoke /
    /// invoke_for_data / write_tlv が通る（read_json / read_cluster は送信系
    /// ではないので数えない）。
    async fn gate_send(&mut self) -> Result<(), MatError> {
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        let n = self.sent;
        self.sent += 1;
        if (self.fail_first_send && n == 0) || self.fail_at == Some(n) {
            return Err(MatError::new(self.fail_kind, "fake send failure"));
        }
        Ok(())
    }
```
`read_onoff` / `invoke` / `write_tlv` の冒頭 8 行（delay + sent + fail 判定）を `self.gate_send().await?;` に。`invoke_for_data` の冒頭 `if let Some(d) = self.delay { … }` を `self.gate_send().await?;` に（`fail_at` が効くようになる）。`fail_at` の doc「read_onoff/invoke/write_tlv 共通」を「read_onoff/invoke/invoke_for_data/write_tlv 共通」に。

テストを追加（`delay_tests` の隣、`#[cfg(test)] mod gate_tests`）:
```rust
    #[tokio::test]
    async fn fail_at_applies_to_invoke_for_data() {
        let mut conn = FakeConn {
            fail_at: Some(1),
            fail_kind: ErrorKind::DeviceRejected,
            ..Default::default()
        };
        conn.invoke(0, 1, 1, None, false).await.unwrap(); // sent 0
        let err = conn.invoke_for_data(0, 1, 2, None, false).await.unwrap_err(); // sent 1
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        conn.invoke_for_data(0, 1, 2, None, false).await.unwrap(); // sent 2
        assert_eq!(conn.sent, 3);
    }
```

- [ ] **Step 6: `with_group_provision_fixture`**

`impl FakeConn` に追加:
```rust
    /// `ops::provision_node` / `runner::provision` が読む 2 属性に妥当な
    /// JSON を返す fake: group-key-map（0x003F/0x0000）= 空リスト、ACL
    /// （0x001F/0x0000）= CASE 管理者エントリ 1 本のみ（fabricIndex 2）。
    /// mat-native / mat / matd のテストが同じ形を手書きしていた共通
    /// フィクスチャ。
    pub fn with_group_provision_fixture() -> Self {
        Self::scripted()
            .with_read(0, 0x003F, 0x0000, json!([]))
            .with_read(
                0,
                0x001F,
                0x0000,
                json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]),
            )
    }
```
mat-native 内の呼び手を置換: `ops.rs` の `provision_node_runs_steps_in_order`（977-983）と管理者のみフィクスチャ 2 箇所（1039 / 1079 付近 — 読み側の属性 ID が同じで、他に `with_read` を連ねていればその後ろに続ける）、`runner.rs` の 302-307 / 413-418。Group エントリ付き（1012-1013 付近）は別形なので残す。置換後に `grep -n '"254": 2' crates/mat-native/src` で残数を確認する（Group エントリ付きの 1 箇所と fixture 定義のみになる）。

- [ ] **Step 7: テスト + 全体チェック**

```bash
cargo test -p mat-core -p mat-native --all-features 2>&1 | grep 'test result'
cargo test -p mat 2>&1 | grep 'test result'
cargo test -p matd 2>&1 | grep 'test result'   # 触っていないが FakeConn / McastCandidate の利用者なので回帰確認
task check 2>&1 | tail -15
```
Expected: 全 PASS、`task check` 緑（fmt:check / clippy / doc:check / test）。

- [ ] **Step 8: コミット**

```bash
git add crates/mat-native/src/iface_select.rs crates/mat-native/src/resolver.rs crates/mat-native/src/test_support.rs crates/mat-native/src/ops.rs crates/mat-native/src/runner.rs
git commit -m "refactor(mat-native): iface_select::scan を公開して multicast iface スキャン 2 本を導出、FakeConn::gate_send / with_group_provision_fixture"
```

---

### Task 9: 仕上げ — `task check` 緑確認と DONE ファイル

**Files:**
- Create: `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/native-core.DONE.md`（リポジトリ外、コミットしない）

- [ ] **Step 1: 全体検証**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat-wt/native-core
task check 2>&1 | tail -20
git status --short
git log --oneline main..HEAD
```
Expected: `task check` 緑、作業ツリー clean、8 コミット。

- [ ] **Step 2: DONE を書く**

内容: やった項目（Task 1〜8 を audit.md の項目番号に対応付けて）/ 見送った項目と理由（本計画冒頭の「見送り」節 + 実装中に増えたもの）/ `task check` 結果（テスト件数・警告 0）/ 実機 E2E: **必要**（認証情報ロード・establish 統合・open_egress は本番経路 — マージ時に親がスモーク）/ 「`kvs::ALPHA_INI_FILE` へ差し替え待ち（`mat_native::ALPHA_INI_FILE`）」/ 他レーンへのフォローアップ（`FakeConn::with_group_provision_fixture()` の mat/matd 利用、`mat_core::hex` の mat/matd/mat-device 置換、`mat/commands/diag.rs:280` の `as_str` 置換は cli-daemon レーン）。
