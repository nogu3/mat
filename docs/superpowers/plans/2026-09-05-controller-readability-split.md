# mat-controller 可読性分割（im / session / dnssd）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat-controller` の 3 巨大ファイル（`im.rs` 3,743 行 / `session.rs` 4,228 行 / `dnssd.rs` 3,059 行）を、**挙動不変・機械的**に責務ごとのサブモジュールへ分割し、`mat_controller::im::X` / `session::X` / `dnssd::X` の公開パスを 1 つも変えずに済ませる。

**Architecture:** 各ファイルを同名ディレクトリモジュール（`im/mod.rs` 等、リポジトリ既存の `mod.rs` 慣習）に変え、責務別の **private サブモジュール** + `pub use <sub>::*;` の glob 再エクスポートで従来パスを維持する。サブモジュール間で共有する private ヘルパは `pub(super)` に上げる（親から見えれば `pub use` には乗らないので公開 API は不変）。`SecureSession` は 1 型のまま、役割ごとの `impl SecureSession` ブロックを別ファイルに置く（子モジュールは親の private フィールドに触れる）。テストは「テスト対象の関数を持つサブモジュール」へ移し、複数サブモジュールにまたがるヘルパは `#[cfg(test)] mod test_util;` に置く。

**Tech Stack:** Rust 2021 workspace、tokio。検証は `cargo fmt` / `cargo clippy -D warnings` / `cargo test`、テスト名の多重集合比較、`cargo semver-checks --baseline-rev`。

**Spec:** ユーザー指示（2026-09-05 セッション）: 「可読性分割（挙動不変・機械的）: im.rs 3,700 行を read / write / invoke / subscribe / cmdfields / json へ、session.rs の 4 役（MRP / IM クライアント / レスポンダ / 購読）分離、dnssd.rs の cache を別モジュールへ。pub API の変更は最小にし、変えた場合は task semver で棚卸し」。監査バックログはメモリ `mat-code-audit-2026-08-31`。

## Global Constraints

- **挙動不変**: 関数本体・定数値・テスト本体は 1 文字も変えない（移動・可視性・`use` の調整・モジュール doc コメントのみ可）。ロジックの「ついで修正」は禁止。気づいた点はタスク報告に書くだけ。
- **公開パス不変**: 他クレート（mat-native / mat / matd / **mat-device / matv は並行セッションが触るので絶対に編集しない**）が使う `mat_controller::im::…` / `mat_controller::session::…` / `mat_controller::dnssd::…` はそのまま解決すること。サブモジュールは `mod x;`（private）+ `pub use x::*;`。`pub mod` にしない（新しい公開パスを増やさない）。
- **クレート内の可視性**: サブモジュール間で共有する元 private 項目は `pub(super)`。`pub(crate)` は使わない（`pub use` に乗らないのは同じだが、スコープを広げない）。
- **ファイル配置**: `crates/mat-controller/src/im/mod.rs` + `im/*.rs`、`session/mod.rs` + `session/*.rs`、`dnssd/mod.rs` + `dnssd/cache.rs`（リポジトリは `mod.rs` 慣習: `crates/mat/src/commands/mod.rs`、`crates/mat-device/src/net/mod.rs`）。`lib.rs` の `pub mod im; pub mod session; pub mod dnssd;` は変更不要。
- **テスト不変**: mat-controller lib のテストは移動前後で **同じ名前の多重集合**（475 個）。確認コマンドは各タスクの検証ステップに記載。テストを削除・改名・統合しない。
- **各タスク終了時**: `cargo fmt --all` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test -p mat-controller` → テスト名比較、が全部通ること。最終タスクで `task check`。
- **コミット**: タスクごとに 1 つ。`git` は `/usr/bin/git` をフルパスで呼ぶ（rtk フックが `git`→`rtk git` に書き換えると worktree 隔離チェックに弾かれる）。メッセージ末尾に
  `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` と
  `Claude-Session: https://claude.ai/code/session_01KDp3tH1KJGSyTLqjVmYt6Z` を付ける。
- **ワークツリー**: `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-split`（ブランチ `worktree-controller-split`）。パスはすべてこの下の相対パス。元リポジトリ root には `cd` しない。
- **git mv は使わない**: `mod.rs` へは内容の大半が残らないので、履歴追跡は諦めて通常の新規作成 + 旧ファイル削除でよい（`git log --follow` は期待しない）。ただし Task 1 では `git mv im.rs im/mod.rs` してから切り出すと差分が読みやすいので推奨。

## File Structure

| ファイル | 責務 |
|---|---|
| `crates/mat-controller/src/im/mod.rs` | モジュール doc、opcode / cluster / attribute / command / status 定数（現 im.rs 1–230 行）、共通型 `ImValue` / `ReportData` / `InvokeOutcome` / `InvokeResponseData` / `ImError`（231–323 行）、共有 private ヘルパ `expect_struct_start` / `skip_container` / `value_to_im` / `encode_im_value`（`pub(super)`）、`mod` 宣言と `pub use` |
| `crates/mat-controller/src/im/json.rs` | `tlv_to_json` / `tlv_element_to_json` / `tlv_element_to_json_impl` / `hex_lower`（現 562–646 行） |
| `crates/mat-controller/src/im/read.rs` | ReadRequest / ReportData の encode / decode 一式: `encode_read_request` / `encode_read_request_cluster`、`decode_attribute_*_ib(_full)` / `decode_attribute_path_ib` / `decode_report_data` / `decode_report_data_message`、`AttributeReport` / `ReportDataMessage` / `AttrReportOut` / `ReportEntryOut` / `encode_report_data_entries` / `encode_report_data`、`AttrPathIn` / `ReadRequestIn` / `decode_attribute_requests` / `decode_read_request(_message)`、`merge_reports`（現 345–1110 行と 1278–1310 行、json 関数を除く） |
| `crates/mat-controller/src/im/subscribe.rs` | `encode_subscribe_request` / `SubscribeRequestIn` / `decode_subscribe_request` / `SubscribeResponse` / `decode_subscribe_response` / `encode_subscribe_response`（現 1111–1277 行） |
| `crates/mat-controller/src/im/cmdfields.rs` | 個別コマンドの CommandFields エンコーダ: `encode_move_to_hue_and_saturation_fields` / `encode_move_to_color_temperature_fields` / `encode_move_to_level_fields` / `encode_key_set_write_fields` / `encode_group_key_map_tlv` / `encode_add_group_fields`（現 1311–1424 行） |
| `crates/mat-controller/src/im/invoke.rs` | InvokeRequest / InvokeResponse / TimedRequest / StatusResponse: `encode_invoke_request(_inner/_timed)` / `encode_timed_request` / `encode_group_invoke_request`、`InvokeRequestIn` / `decode_request_command_data_ib` / `decode_invoke_request`、`decode_status_ib` / `decode_command_status_ib` / `decode_invoke_response(_ib)` / `decode_command_data_ib` / `decode_invoke_response_ib_data` / `decode_invoke_response_data`、`encode_invoke_response_status` / `encode_invoke_response_data`、`encode_status_response` / `decode_status_response`（現 1425–2049 行） |
| `crates/mat-controller/src/im/write.rs` | WriteRequest / WriteResponse: `encode_write_request(_inner/_tlv/_tlv_timed)`、`WriteAttrIn` / `WriteRequestIn` / `decode_write_attribute_data_ib` / `decode_write_requests_array` / `decode_write_request`、`decode_write_response` / `encode_write_response`（現 2069–2342 行） |
| `crates/mat-controller/src/session/mod.rs` | モジュール doc、`IM_RECV_TIMEOUT` / `worst_case_send_budget` / 定数、`SessionKeys` / `SessionError`（+ From 実装）、`SecureSession` 構造体本体、コンストラクタ・アクセサ（`new` / `new_device_role` / `peer_node_id` / `with_peer_cats` / `peer_cats` / `attestation_challenge` / `new_exchange_id`）、`mod` 宣言、`#[cfg(test)] mod test_util;` |
| `crates/mat-controller/src/session/mrp.rs` | MRP 層: `ScreenFilter` / `payload_head_hex` / `MAX_PEER_INITIATED_BUFFER`、`impl SecureSession { seal, send_standalone_ack, send_close_session, screen, screen_with, send_reliable, recv }` |
| `crates/mat-controller/src/session/client.rs` | IM クライアント（コントローラ役）: `MAX_REPORT_CHUNKS`、`impl SecureSession { read_attribute, invoke, invoke_for_data, send_timed_request, collect_reports, read_attribute_json, read_cluster_json, write_attribute_tlv }` |
| `crates/mat-controller/src/session/responder.rs` | レスポンダ（デバイス役 + StatusResponse 送信）: `impl SecureSession { respond_status, deliver_request, recv_request, take_buffered_request, requeue_buffered_request, reply_reliable }` |
| `crates/mat-controller/src/session/subscribe.rs` | 購読: `impl SecureSession { subscribe_wildcard, next_subscription_report }` |
| `crates/mat-controller/src/session/test_util.rs` | `#[cfg(test)]` 専用。現 tests mod 先頭の定数（`I2R` / `R2I` / `OUR_NODE` / `DEV_NODE` / `LOCAL_SID` / `PEER_SID`）と複数テストが共有するヘルパ（`keys` / `fast_cfg` / `bind_local` / `device_datagram` / `open_from_controller` / `report_data_payload` / `invoke_response_*_payload` / `write_response_payload` / `report_data_message_attr*` / `reliable_session_pair` / `subscription_report_payload` / `keepalive_payload` / `subscribe_response_payload` / `invoke_response_status_ok`）を `pub(super)` で置く |
| `crates/mat-controller/src/dnssd/mod.rs` | 現 dnssd.rs から cache 部分を除いた全部（定数、`DnssdError`、`ResolvedNode`、クエリ/応答コーデック、`resolve_operational(_many)`、`resolve_commissionable`、browse 一式）。cache が使う private 項目を `pub(super)` に上げる |
| `crates/mat-controller/src/dnssd/cache.rs` | `MAX_CACHE` / `CacheEntry` / `CacheInner` / `OperationalCache`、`OPERATIONAL_SUFFIX` / `MAX_FOLD_ENTRIES` / `MAX_ADDRS_PER_HOST` / `CACHE_FLUSH_GRACE` / `OperationalFold` / `fold_operational_into_cache` / `run_operational_cache` / `spawn_operational_cache`（現 1262–1546 行）と、その tests |
| `ARCHITECTURE.md`（変更、Task 5） | `im.rs` / `dnssd.rs` の 2 箇所のファイル名言及をディレクトリ形に |

### テスト名比較コマンド（全タスク共通）

移動前の名前一覧は `/tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/367a993e-b64c-4ff3-8664-10373bcfb87b/scratchpad/tests-before.txt`（475 行、`sort` 済み、関数名だけ）。各タスクの最後にこれを実行し **差分ゼロ** を確認する:

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-split
cargo test -p mat-controller --lib -- --list 2>/dev/null | /usr/bin/grep ': test$' | sed 's/: test$//' | awk -F'::' '{print $NF}' | sort > /tmp/tests-after.txt
diff /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/367a993e-b64c-4ff3-8664-10373bcfb87b/scratchpad/tests-before.txt /tmp/tests-after.txt && echo SAME
```

Expected: `SAME`（差分行が 1 行でも出たらテストの取りこぼし・重複）。

### 移動の作法（全タスク共通）

1. 移動元の行範囲を **そのままコピー**（インデント・コメント・空行も）。エディタで手打ちしない。`sed -n 'A,Bp' src > dst` で切り出し、その後 `use` 行を先頭に足す。
2. 新ファイルの先頭は `//!` の 1〜3 行のモジュール doc（責務を書く。元ファイルの doc をコピペしない）。
3. サブモジュールから親の項目を使うときは `use super::{...};` を明示（glob `use super::*;` は本番コードでは使わない。テストの `mod tests { use super::*; }` は従来どおり可）。
4. 元ファイルで private (`fn` / `const` / `struct` に修飾なし) だった項目を他のサブモジュールも使うなら `pub(super)` に上げる。それ以外の可視性は変えない。
5. 移動後、`mod.rs` から移動元の行を削除し、`mod x;` と `pub use x::*;` を追加。`pub use` は「元々 `pub` だった項目」を外へ出すためのもので、`pub(super)` 項目は glob に乗らない（rustc の仕様）。
6. `rustc` の `unused import` は `-D warnings` で落ちるので、切り出し後の `use` は最小に整える。

---

### Task 1: im の分割 (前半) — `im/mod.rs` + `json` / `read` / `subscribe`

**Files:**
- Create: `crates/mat-controller/src/im/mod.rs`（`git mv crates/mat-controller/src/im.rs crates/mat-controller/src/im/mod.rs` から開始）
- Create: `crates/mat-controller/src/im/json.rs`
- Create: `crates/mat-controller/src/im/read.rs`
- Create: `crates/mat-controller/src/im/subscribe.rs`
- Delete: `crates/mat-controller/src/im.rs`（`git mv` で消える）

**Interfaces:**
- Consumes: 現 `im.rs` の全項目（行番号は Task 1 開始時点の `im.rs`: 定数 9–230、型 231–323、`expect_struct_start` 325、`skip_container` 337、`encode_read_request` 345、`value_to_im` 366、read 系 384–531、`AttributeReport`/`ReportDataMessage` 532–561、json 562–646、`_full` 系 647–893、`AttrReportOut`〜`encode_read_request_cluster` 894–1017、`AttrPathIn`〜`decode_read_request` 1018–1110、subscribe 1111–1277、`merge_reports` 1278–1310、tests 2343–3743）。
- Produces: `im/mod.rs` に `pub(super) fn expect_struct_start(r: &mut Reader) -> Result<(), ImError>`、`pub(super) fn skip_container(r: &mut Reader) -> Result<(), ImError>`、`pub(super) fn value_to_im(v: Value) -> Result<ImValue, ImError>`、`pub(super) fn encode_im_value(value: &ImValue) -> Vec<u8>`（後者は Task 2 の write が使う。Task 1 では元位置 2050 行から `mod.rs` の型定義直後へ移動し `pub(super)` を付ける）。`im/read.rs` に `pub(super) fn decode_attribute_path_ib(r: &mut Reader) -> Result<AttributePathFields, ImError>` と `pub(super) struct AttributePathFields`（write.rs の `decode_write_attribute_data_ib` が使うか確認: 使っていれば `pub(super)`、使っていなければ private のまま）。外部公開パスは全て `mat_controller::im::<name>` のまま。

- [ ] **Step 1: 移動前の基準を確保する**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-split
cargo test -p mat-controller --lib 2>&1 | /usr/bin/grep 'test result'
wc -l crates/mat-controller/src/im.rs
```

Expected: `test result: ok. 475 passed`、`3743 crates/mat-controller/src/im.rs`。

- [ ] **Step 2: ディレクトリモジュール化**

```bash
mkdir -p crates/mat-controller/src/im
/usr/bin/git mv crates/mat-controller/src/im.rs crates/mat-controller/src/im/mod.rs
cargo build -p mat-controller 2>&1 | tail -1
```

Expected: `Finished`（ファイル名を変えただけでビルドは通る）。

- [ ] **Step 3: `json.rs` を切り出す**

`im/mod.rs` の `fn tlv_element_to_json` 〜 `fn hex_lower` の終わり（現 562–646 行。境界は「`fn hex_lower` の閉じ `}` の直後の空行」まで）を `im/json.rs` に移し、先頭に以下を付ける:

```rust
//! IM の TLV 値 → JSON 変換（read / listen の出力形）。struct のキーは
//! context tag の 10 進文字列、bytes は小文字 hex 文字列。

use crate::tlv::{Element, Reader, Value};

use super::ImError;
```

`mod.rs` の該当行を削除し、`mod.rs` の `impl From<TlvError> for ImError` の直後に

```rust
mod json;
pub use json::*;
```

を置く（`mod` 宣言と `pub use` は `mod.rs` の 1 箇所にまとめ、Task 1 / 2 で行を足していく）。
`json.rs` 内で `skip_container` / `expect_struct_start` を使っていれば `use super::{expect_struct_start, skip_container};` を足し、`mod.rs` 側のそれらを `pub(super) fn` にする。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
```

Expected: 出力なし（エラーゼロ）。`unused import` が出たらその `use` を消す。

- [ ] **Step 4: `subscribe.rs` を切り出す**

`mod.rs` の `pub fn encode_subscribe_request` の doc コメント開頭（現 1111 行付近、直前の空行から）〜 `pub fn encode_subscribe_response` の閉じ `}`（現 1277 行）を `im/subscribe.rs` に移す。先頭:

```rust
//! SubscribeRequest / SubscribeResponse（spec §8.5–8.6）。priming ReportData
//! 自体は read と同じ `decode_report_data_message` で読む。

use crate::tlv::{Reader, Tag, Value, Writer};

use super::{expect_struct_start, skip_container, ImError, IM_REVISION};
```

実際に参照している名前だけ残す（例: `AttrPathIn` / `decode_attribute_requests` を使うなら `use super::{AttrPathIn, decode_attribute_requests};` — `decode_attribute_requests` は read.rs へ移すので、その場合 read.rs 側で `pub(super) fn` にする）。`mod.rs` に `mod subscribe; pub use subscribe::*;` を追加。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
```

Expected: 出力なし。

- [ ] **Step 5: `read.rs` を切り出す**

以下を `im/read.rs` へ、**元の順序のまま**移す: `encode_read_request`（345）、`decode_attribute_status_ib` / `decode_attribute_data_ib` / `decode_attribute_report_ib` / `decode_report_data`（384–531）、`AttributeReport` / `ReportDataMessage`（532–561）、`decode_attribute_path_ib` 〜 `decode_report_data_message`（647–893）、`AttrReportOut` / `ReportEntryOut` / `encode_report_data_entries` / `encode_report_data` / `encode_read_request_cluster`（894–1017）、`AttrPathIn` / `decode_attribute_requests` / `ReadRequestIn` / `decode_read_request_message` / `decode_read_request`（1018–1110）、`merge_reports`（1278–1310）。`value_to_im`（366）は `mod.rs` に残して `pub(super)`（invoke / write も使う）。先頭:

```rust
//! ReadRequest / ReportData の encode / decode（単一属性・cluster wildcard・
//! chunk 結合 `merge_reports`）。コントローラ側の read と、device 側の
//! ReadRequest 受理 / ReportData 送出の両方向。

use crate::tlv::{Reader, Tag, TlvError, Value, Writer};

use super::{expect_struct_start, skip_container, value_to_im, ImError, ImValue, ReportData, IM_REVISION};
```

（`TlvError` 等、未使用なら消す。）`mod.rs` に `mod read; pub use read::*;` を追加。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -60
```

Expected: 出力なし。`private type in public interface` が出たら、その型（例: `AttributePathFields`）は元々 `pub` だったか確認し、元どおりの可視性にする。

- [ ] **Step 6: `encode_im_value` を `mod.rs` の共有ヘルパ位置へ**

`mod.rs` の `fn encode_im_value`（元 2050–2068 行、write 節の先頭にある）を切り取り、`value_to_im` の直後に貼って `pub(super) fn encode_im_value` にする（Task 2 の write.rs が `use super::encode_im_value;` で使う）。同時に `expect_struct_start` / `skip_container` / `value_to_im` も `pub(super)` になっていることを確認。

- [ ] **Step 7: モジュール doc を現状に合わせる**

`mod.rs` 先頭の `//!` を次に置き換える（旧 doc の「Only what M2's onoff read/invoke path needs … No subscriptions」は陳腐化）:

```rust
//! Interaction Model payloads (Matter Core Spec 1.4, Chapter 8).
//!
//! Layout: this file holds the opcode / cluster / attribute / command /
//! status constants, the shared value & error types (`ImValue`, `ImError`,
//! `ReportData`, `InvokeOutcome`, `InvokeResponseData`) and the private TLV
//! helpers every codec uses. The codecs live in one submodule per
//! interaction, re-exported flat so callers keep writing `im::<name>`:
//! `read` (ReadRequest / ReportData), `subscribe`, `invoke` (Invoke /
//! Timed / StatusResponse), `write`, `cmdfields` (per-command CommandFields
//! encoders) and `json` (TLV → JSON).
```

- [ ] **Step 8: tests を移す**

`mod.rs` 末尾の `#[cfg(test)] mod tests { … }` を走査し、各 `#[test]`（および付随する `fn` ヘルパ）を **その関数が主に呼ぶコードのサブモジュール**へ移す: `tlv_to_json` 系 → `json.rs`、read/report/merge 系 → `read.rs`、subscribe 系 → `subscribe.rs`。invoke / write / cmdfields のテストは Task 2 で移すので **今は `mod.rs` の tests に残す**。移した各ファイル末尾に

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};
    // 必要に応じて use super::super::{…}; で親（mod.rs）の定数・型を取る
}
```

を置き、テスト本体はそのまま貼る。テスト内で親の定数（`CLUSTER_ON_OFF` 等）を使う場合、`use super::*;` はサブモジュール直下の項目しか取らないので `use crate::im::*;`（親の再エクスポート経由）を足す — これが最も機械的で、名前衝突も起きない。

```bash
cargo test -p mat-controller --lib 2>&1 | /usr/bin/grep -E 'test result|FAILED|^error' | head
```

Expected: `test result: ok. 475 passed`。

- [ ] **Step 9: fmt / clippy / テスト名比較**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
cargo test -p mat-controller --lib -- --list 2>/dev/null | /usr/bin/grep ': test$' | sed 's/: test$//' | awk -F'::' '{print $NF}' | sort > /tmp/tests-after.txt
diff /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/367a993e-b64c-4ff3-8664-10373bcfb87b/scratchpad/tests-before.txt /tmp/tests-after.txt && echo SAME
cargo build --workspace --all-targets 2>&1 | tail -1
wc -l crates/mat-controller/src/im/*.rs
```

Expected: clippy 出力なし、`SAME`、`Finished`、行数合計 ≈ 3,743 + 各ファイルの `use`/doc 分（+60 行程度。大幅に減っていたら取りこぼし）。

- [ ] **Step 10: Commit**

```bash
/usr/bin/git add -A crates/mat-controller/src/im crates/mat-controller/src/im.rs
/usr/bin/git commit -F - <<'EOF'
refactor(mat-controller): im.rs をディレクトリモジュール化し json / read / subscribe を切り出す（挙動不変）

im/mod.rs = 定数・共有型・共有 TLV ヘルパ（pub(super)）+ 平坦な pub use。
im/json.rs / read.rs / subscribe.rs は元コードの行移動のみ。公開パス
mat_controller::im::<name> は不変（他クレートの変更なし）。テスト 475 個は
名前の多重集合が一致。invoke / write / cmdfields は次コミット。

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KDp3tH1KJGSyTLqjVmYt6Z
EOF
```

---

### Task 2: im の分割 (後半) — `cmdfields` / `invoke` / `write`

**Files:**
- Modify: `crates/mat-controller/src/im/mod.rs`
- Create: `crates/mat-controller/src/im/cmdfields.rs`
- Create: `crates/mat-controller/src/im/invoke.rs`
- Create: `crates/mat-controller/src/im/write.rs`

**Interfaces:**
- Consumes: Task 1 の `mod.rs` にある `pub(super) fn expect_struct_start / skip_container / value_to_im / encode_im_value`、および `pub` 型 `ImError` / `ImValue` / `InvokeOutcome` / `InvokeResponseData`、定数 `IM_REVISION` / `OPCODE_*`。`read.rs` の `pub(super) fn decode_attribute_path_ib`（write が使う場合）。
- Produces: 公開パス `mat_controller::im::<name>` は全て不変。`invoke.rs` の `decode_command_data_ib` はクレート内の他ファイル（`im::decode_command_data_ib` を参照している箇所が 1 つ: `grep -rn 'im::decode_command_data_ib' crates/mat-controller/src`）が使うので、元の可視性を確認して同じにする（元が `pub` なら `pub`、そうでなければ呼び手が同クレートなので `pub(crate)` … ただし Global Constraints の `pub(crate)` 禁止はサブモジュール間共有の話であり、**元々の可視性を保つ**のが優先。元の宣言をそのまま写す）。

- [ ] **Step 1: `cmdfields.rs` を切り出す**

`mod.rs` の `pub fn encode_move_to_hue_and_saturation_fields` の doc 開頭 〜 `pub fn encode_add_group_fields` の閉じ `}` を `im/cmdfields.rs` へ。先頭:

```rust
//! 個別コマンドの CommandFields エンコーダ（ColorControl / LevelControl /
//! GroupKeyManagement / Groups）。汎用 invoke は mat-native が
//! `mat_core::ids` の型表から TLV を組むので、ここにあるのは shortcut 系と
//! group provision 専用の固定形だけ。

use crate::tlv::{Tag, Writer};
```

`mod.rs` に `mod cmdfields; pub use cmdfields::*;` を追加。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
```

Expected: 出力なし。

- [ ] **Step 2: `invoke.rs` を切り出す**

`mod.rs` の `fn encode_invoke_request_inner` の doc 開頭 〜 `pub fn decode_status_response` の閉じ `}` を `im/invoke.rs` へ（`encode_invoke_request(_timed)`、`encode_timed_request`、`encode_group_invoke_request`、`InvokeRequestIn`、`decode_request_command_data_ib`、`decode_invoke_request`、`decode_status_ib`、`decode_command_status_ib`、`decode_invoke_response(_ib)`、`decode_command_data_ib`、`decode_invoke_response_ib_data`、`decode_invoke_response_data`、`encode_invoke_response_status`、`encode_invoke_response_data`、`encode_status_response`、`decode_status_response`）。先頭:

```rust
//! InvokeRequest / InvokeResponse / TimedRequest / StatusResponse の
//! encode / decode。コントローラ側の invoke と、device 側の InvokeRequest
//! 受理 / InvokeResponse 送出の両方向。

use crate::tlv::{Reader, Tag, Value, Writer};

use super::{expect_struct_start, skip_container, value_to_im, ImError, ImValue, InvokeOutcome, InvokeResponseData, IM_REVISION};
```

未使用の名前は消す。`mod.rs` に `mod invoke; pub use invoke::*;` を追加。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
```

Expected: 出力なし。

- [ ] **Step 3: `write.rs` を切り出す**

`mod.rs` の `fn encode_write_request_inner` の doc 開頭 〜 `pub fn encode_write_response` の閉じ `}`（tests の直前）を `im/write.rs` へ。先頭:

```rust
//! WriteRequest / WriteResponse の encode / decode。コントローラ側の write
//! と、device 側の WriteRequest 受理 / WriteResponse 送出の両方向。

use crate::tlv::{Reader, Tag, Value, Writer};

use super::{encode_im_value, expect_struct_start, skip_container, value_to_im, ImError, ImValue, IM_REVISION};
```

`decode_write_attribute_data_ib` が `decode_attribute_path_ib` / `AttributePathFields` を使うなら `use super::read::{decode_attribute_path_ib, AttributePathFields};` を足し、read.rs 側をそれぞれ `pub(super)` にする。`mod.rs` に `mod write; pub use write::*;` を追加。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
```

Expected: 出力なし。

- [ ] **Step 4: 残りの tests を移す**

`mod.rs` に残った `mod tests` の各 `#[test]` を、invoke 系 → `invoke.rs`、write 系 → `write.rs`、cmdfields 系（`encode_move_to_*` / `encode_key_set_write_fields` / `encode_group_key_map_tlv` / `encode_add_group_fields` を叩くもの）→ `cmdfields.rs` へ、Task 1 Step 8 と同じ形（`mod tests { use super::*; use crate::im::*; use crate::tlv::{…}; }`）で移す。`mod.rs` の `mod tests` が空になったらブロック自体を削除する。複数サブモジュールにまたがるテスト（例: invoke と read を両方叩く）は **`mod.rs` に `#[cfg(test)] mod tests` を残して**そこに置く（公開項目だけを使うので `use super::*;` で足りる）。

```bash
cargo test -p mat-controller --lib 2>&1 | /usr/bin/grep -E 'test result|FAILED|^error' | head
```

Expected: `test result: ok. 475 passed`。

- [ ] **Step 5: `mod.rs` の最終形を確認**

```bash
/usr/bin/grep -n '^mod \|^pub use \|^pub(super) fn \|^fn \|^pub fn \|^#\[cfg(test)\]' crates/mat-controller/src/im/mod.rs
wc -l crates/mat-controller/src/im/*.rs
```

Expected: `mod.rs` に `pub fn` は 1 つも残らない（全コーデックがサブモジュールへ移った）。`mod` 6 個 + `pub use` 6 個。`mod.rs` は概ね 350 行以下（定数 230 行 + 型 90 行 + ヘルパ）。

- [ ] **Step 6: fmt / clippy / テスト名比較 / 全ワークスペースビルド**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
cargo test -p mat-controller --lib -- --list 2>/dev/null | /usr/bin/grep ': test$' | sed 's/: test$//' | awk -F'::' '{print $NF}' | sort > /tmp/tests-after.txt
diff /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/367a993e-b64c-4ff3-8664-10373bcfb87b/scratchpad/tests-before.txt /tmp/tests-after.txt && echo SAME
cargo test --workspace 2>&1 | /usr/bin/grep -E 'test result: FAILED|^error' | head
```

Expected: clippy 出力なし、`SAME`、ワークスペース全体のテストに FAILED なし（mat-device / matv も `im::*` 経由で通る）。

- [ ] **Step 7: Commit**

```bash
/usr/bin/git add -A crates/mat-controller/src/im
/usr/bin/git commit -F - <<'EOF'
refactor(mat-controller): im の cmdfields / invoke / write を切り出し、mod.rs を定数・共有型・ヘルパのみに（挙動不変）

im.rs 3,743 行 → mod.rs（定数・ImValue/ImError 等・pub(super) ヘルパ）+
json / read / subscribe / cmdfields / invoke / write。元コードの行移動のみ、
公開パス mat_controller::im::<name> は不変、テスト 475 個の名前多重集合一致。

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KDp3tH1KJGSyTLqjVmYt6Z
EOF
```

---

### Task 3: session の 4 役分離 — `mrp` / `client` / `responder` / `subscribe` + `test_util`

**Files:**
- Create: `crates/mat-controller/src/session/mod.rs`（`git mv crates/mat-controller/src/session.rs crates/mat-controller/src/session/mod.rs` から開始）
- Create: `crates/mat-controller/src/session/mrp.rs`
- Create: `crates/mat-controller/src/session/client.rs`
- Create: `crates/mat-controller/src/session/responder.rs`
- Create: `crates/mat-controller/src/session/subscribe.rs`
- Create: `crates/mat-controller/src/session/test_util.rs`
- Delete: `crates/mat-controller/src/session.rs`

**Interfaces:**
- Consumes: 現 `session.rs`（Task 3 開始時点の行番号: `IM_RECV_TIMEOUT` 24 / `worst_case_send_budget` 28 / `MAX_REPORT_CHUNKS` 35 / `payload_head_hex` 39 / `ScreenFilter` 49 / `MAX_PEER_INITIATED_BUFFER` 59 / `SessionKeys` 62 / `SessionError` 69–121 / `SecureSession` 123–167 / `impl SecureSession` 168–1547 / tests 1548–4228）。
- Produces: `SecureSession` の全 `pub` メソッドは同名・同シグネチャで `mat_controller::session::SecureSession` から呼べる（メソッドは型に付くのでモジュール分割の影響を受けない）。`mod.rs` の private 項目のうちサブモジュールが使うもの（`payload_head_hex` / `ScreenFilter` / 各 `MAX_*` / `IM_RECV_TIMEOUT`）は **定義をそのサブモジュールに移す**か `pub(super)` にする。子モジュールは親の private フィールド（`self.transport` 等）に直接触れるので、フィールドの可視性は変えない。

- [ ] **Step 1: 基準確保とディレクトリ化**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-split
wc -l crates/mat-controller/src/session.rs
mkdir -p crates/mat-controller/src/session
/usr/bin/git mv crates/mat-controller/src/session.rs crates/mat-controller/src/session/mod.rs
cargo build -p mat-controller 2>&1 | tail -1
```

Expected: `4228`、`Finished`。

- [ ] **Step 2: `impl SecureSession` の役割境界を確定する**

```bash
/usr/bin/grep -n '^    pub fn \|^    pub async fn \|^    async fn \|^    fn \|^impl SecureSession\|^}$' crates/mat-controller/src/session/mod.rs | sed -n 1,60p
```

期待する並び（メソッド名 → 行き先）:
- `mod.rs` に残す: `new`, `new_device_role`, `peer_node_id`, `with_peer_cats`, `peer_cats`, `attestation_challenge`, `new_exchange_id`
- `mrp.rs`: `seal`, `send_standalone_ack`, `send_close_session`, `screen`, `screen_with`, `send_reliable`, `recv`
- `client.rs`: `read_attribute`, `invoke`, `invoke_for_data`, `send_timed_request`, `collect_reports`, `read_attribute_json`, `read_cluster_json`, `write_attribute_tlv`
- `responder.rs`: `respond_status`, `deliver_request`, `recv_request`, `take_buffered_request`, `requeue_buffered_request`, `reply_reliable`
- `subscribe.rs`: `subscribe_wildcard`, `next_subscription_report`

（メソッドがこの一覧に無い名前で存在したら、doc コメントの役割で最も近い行き先に入れ、タスク報告に書く。）

- [ ] **Step 3: `mrp.rs` を作る**

新規 `session/mrp.rs`:

```rust
//! MRP 層（spec §4.12）: seal / open、RxWindow dedup、standalone ack、
//! piggyback ack、再送付きの `send_reliable` / `recv`。IM の意味は知らない。

use std::time::Duration;

use tokio::time::Instant;

use crate::crypto::{open_message, seal_message, OpenError};
use crate::exchange::{IncomingMessage, MrpConfig};
use crate::message::{
    Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, OPCODE_STATUS_REPORT,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use crate::transport::MAX_DATAGRAM;

use super::{SecureSession, SessionError};

// ← ここに mod.rs から `payload_head_hex`（39 行〜）、`ScreenFilter`（49 行〜）、
//    `MAX_PEER_INITIATED_BUFFER`（59 行）を定義ごと移す（可視性はそのまま private。
//    他サブモジュールが使う場合のみ pub(super)）。

impl SecureSession {
    // ← mod.rs の impl から seal / send_standalone_ack / send_close_session /
    //    screen / screen_with / send_reliable / recv を doc コメント込みで
    //    そのまま移す（各メソッドは「直前の空行〜閉じ `}`」単位で sed -n 切り出し）。
}
```

`use` は実際に参照する名前だけに整える。`mod.rs` に `mod mrp;` を追加（`pub use` は不要 — メソッドは型に付く。ただし `mrp.rs` に `pub` な自由関数・型を置いた場合のみ `pub use mrp::*;`）。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
```

Expected: 出力なし。`private method` エラーが出たら、そのメソッド（例: `seal`）を他サブモジュールも呼んでいるので `pub(super) fn`（または `pub(super) async fn`）に上げる。**`pub` には上げない。**

- [ ] **Step 4: `client.rs` を作る**

```rust
//! IM クライアント（コントローラ役）: read / invoke / timed invoke / write を
//! 1 exchange で完結させる。chunk 結合と StatusResponse の返送も含む。

use crate::exchange::MrpConfig;
use crate::im;

use super::{SecureSession, SessionError, IM_RECV_TIMEOUT};

// ← `MAX_REPORT_CHUNKS`（mod.rs 35 行）を移す。subscribe.rs も使うなら pub(super)。

impl SecureSession {
    // ← read_attribute / invoke / invoke_for_data / send_timed_request /
    //    collect_reports / read_attribute_json / read_cluster_json /
    //    write_attribute_tlv
}
```

`mod.rs` に `mod client;` 追加。`IM_RECV_TIMEOUT` は `pub const` で `mod.rs` に残す（外部が使う可能性 — 元の可視性のまま）。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
```

Expected: 出力なし。

- [ ] **Step 5: `responder.rs` を作る**

```rust
//! レスポンダ側（デバイス役 CASE/PASE セッション + 購読の StatusResponse）:
//! ピア発リクエストの受理・待避・返送と、reliable な応答送信。

use crate::exchange::{IncomingMessage, MrpConfig};

use super::{SecureSession, SessionError};

impl SecureSession {
    // ← respond_status / deliver_request / recv_request / take_buffered_request /
    //    requeue_buffered_request / reply_reliable
}
```

`mod.rs` に `mod responder;` 追加。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
```

Expected: 出力なし。

- [ ] **Step 6: `subscribe.rs` を作る**

```rust
//! 購読（matd の常駐 Subscribe）: `subscribe_wildcard` のハンドシェイクと
//! priming、`next_subscription_report` のデバイス発 ReportData / keepalive の
//! pump。

use std::time::Duration;

use crate::exchange::MrpConfig;
use crate::im;

use super::{SecureSession, SessionError};

impl SecureSession {
    // ← subscribe_wildcard / next_subscription_report
}
```

`mod.rs` に `mod subscribe;` 追加。この時点で `mod.rs` の `impl SecureSession` にはコンストラクタ・アクセサ・`new_exchange_id` だけが残る。

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
/usr/bin/grep -c '^    pub async fn \|^    async fn ' crates/mat-controller/src/session/mod.rs
```

Expected: 出力なし、`0`（async メソッドは mod.rs に 1 つも残らない）。

- [ ] **Step 7: `test_util.rs` にテスト共有物を移す**

`mod.rs` の `mod tests` 冒頭（定数 `I2R` … `PEER_SID`、`fn keys` / `fast_cfg` / `bind_local` / `device_datagram` / `open_from_controller`、および 2 つ以上のテストが使う payload ビルダ `report_data_payload` / `invoke_response_success_payload` / `invoke_response_error_payload` / `invoke_response_with_fields_payload` / `write_response_payload` / `report_data_message_attr` / `report_data_message_attr_list_append_2` / `reliable_session_pair` / `subscription_report_payload` / `keepalive_payload` / `subscribe_response_payload` / `invoke_response_status_ok`）を新規 `session/test_util.rs` へ移し、それぞれに `pub(super)` を付ける（`const` も `pub(super) const`）。先頭:

```rust
//! session サブモジュールのテストが共有する定数・鍵・datagram ビルダ。
//! `#[cfg(test)]` 専用（mod.rs 側で `#[cfg(test)] mod test_util;`）。

use std::time::Duration;

use crate::crypto::seal_message;
use crate::exchange::MrpConfig;
use crate::message::{Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL};
use crate::transport::{ReliableChannel, Transport, UdpTransport, MAX_DATAGRAM, RELIABLE_PEER};

use super::{SecureSession, SessionKeys};
```

`mod.rs` に `#[cfg(test)] mod test_util;` を追加。

- [ ] **Step 8: tests を役割ごとに移す**

`mod.rs` の残りの `#[test]` / `#[tokio::test]` を、テスト対象メソッドの行き先に合わせて移す:
- `mrp.rs`: `respond_status_retransmits_fast_after_recent_peer_rx`（respond_status は responder だが MRP 再送間隔の検証 — **responder.rs** へ。判断に迷ったら「テスト名の先頭の動詞・対象メソッド」で決める）、`send_reliable_encrypts_and_completes_on_sealed_ack`、`recv_decrypts_dedups_and_acks`、`ignores_wrong_key_wrong_session_and_wrong_exchange`、`acks_foreign_exchange_needs_ack_message_with_its_own_exchange_id`、`budget_components_are_pinned`（`worst_case_send_budget` は mod.rs 側なので **mod.rs の tests に残す**）、`close_session_sends_single_best_effort_status_report`
- `client.rs`: `read_attribute_*`、`invoke_*`、`write_attribute_*`、`read_cluster_json_*`
- `responder.rs`: `respond_status_*`、`device_role_session_serves_invoke`、`reply_reliable_*`
- `subscribe.rs`: `subscribe_wildcard_*`、`next_subscription_report_*`、`status_response_piggybacks_ack_of_report`、`udp_device_initiated_report_is_acked_and_delivered`、`report_chunk_arriving_during_status_ack_wait_is_not_lost`

各ファイル末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_util::*;
    // 元 tests の use（crate::message::… 等）のうち、このファイルのテストが使うものだけ
}
```

`mod.rs` の `mod tests` は、残るテスト（`budget_components_are_pinned` 等）が無ければ削除、あれば `use super::*; use crate::session::test_util::*;` の形に整える。

```bash
cargo test -p mat-controller --lib session 2>&1 | /usr/bin/grep -E 'test result|FAILED|^error' | head
```

Expected: `test result: ok.`（session のテスト数は移動前と同じ。全体の名前比較は次ステップ）。

- [ ] **Step 9: fmt / clippy / テスト名比較 / 全ワークスペース**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
cargo test -p mat-controller --lib -- --list 2>/dev/null | /usr/bin/grep ': test$' | sed 's/: test$//' | awk -F'::' '{print $NF}' | sort > /tmp/tests-after.txt
diff /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/367a993e-b64c-4ff3-8664-10373bcfb87b/scratchpad/tests-before.txt /tmp/tests-after.txt && echo SAME
cargo test --workspace 2>&1 | /usr/bin/grep -E 'test result: FAILED|^error' | head
wc -l crates/mat-controller/src/session/*.rs
```

Expected: clippy 出力なし、`SAME`、FAILED なし、合計 ≈ 4,228 + `use`/doc 分。

- [ ] **Step 10: Commit**

```bash
/usr/bin/git add -A crates/mat-controller/src/session crates/mat-controller/src/session.rs
/usr/bin/git commit -F - <<'EOF'
refactor(mat-controller): session.rs を MRP / IM クライアント / レスポンダ / 購読の 4 ファイルに分離（挙動不変）

SecureSession は 1 型のまま、役割ごとの impl ブロックを session/{mrp,client,
responder,subscribe}.rs へ行移動。mod.rs = 型・エラー・コンストラクタ・
アクセサ。テスト共有ヘルパは session/test_util.rs。公開 API・テスト 475 個の
名前多重集合は不変。

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KDp3tH1KJGSyTLqjVmYt6Z
EOF
```

---

### Task 4: dnssd の cache を `dnssd/cache.rs` へ

**Files:**
- Create: `crates/mat-controller/src/dnssd/mod.rs`（`git mv crates/mat-controller/src/dnssd.rs crates/mat-controller/src/dnssd/mod.rs` から開始）
- Create: `crates/mat-controller/src/dnssd/cache.rs`
- Delete: `crates/mat-controller/src/dnssd.rs`

**Interfaces:**
- Consumes: 現 `dnssd.rs` 1262–1546 行（`MAX_CACHE` / `CacheEntry` / `CacheInner` / `OperationalCache` + impl / `OPERATIONAL_SUFFIX` / `MAX_FOLD_ENTRIES` / `MAX_ADDRS_PER_HOST` / `CACHE_FLUSH_GRACE` / `OperationalFold` / `fold_operational_into_cache` / `run_operational_cache` / `spawn_operational_cache`）と、その部分が使う親の private 項目: `bind_mdns_socket` / `parse_message` / `Record` / `RData` / `txt_u32` / `MDNS_GROUP` / `MDNS_PORT` / `TYPE_*` / `QU_CLASS_IN` / `encode_query` / `is_link_local` 等（実際に `cargo build` が unresolved と言うものだけ `pub(super)` に上げる）。
- Produces: `mat_controller::dnssd::{OperationalCache, spawn_operational_cache}` は不変（`mod cache; pub use cache::*;`）。`OperationalCache::{new, get, request, insert}` は型に付くメソッドなので不変。

- [ ] **Step 1: 基準確保とディレクトリ化**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-split
wc -l crates/mat-controller/src/dnssd.rs
mkdir -p crates/mat-controller/src/dnssd
/usr/bin/git mv crates/mat-controller/src/dnssd.rs crates/mat-controller/src/dnssd/mod.rs
cargo build -p mat-controller 2>&1 | tail -1
```

Expected: `3059`、`Finished`。

- [ ] **Step 2: `cache.rs` を切り出す**

`mod.rs` の `const MAX_CACHE` の直前のコメント/空行から `pub fn spawn_operational_cache` の閉じ `}`（tests の直前）までを `dnssd/cache.rs` へ。先頭:

```rust
//! matd 常駐用の operational mDNS キャッシュ: `_matter._tcp` の SRV/TXT/AAAA
//! を受信し続けて `ResolvedNode` を鮮度順に保持する（`OperationalCache`）。
//! one-shot 解決（親モジュール）とは別経路 — `mat` 単発実行は使わない
//! （設計ルール 4: 状態を持たない）。

use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{bind_mdns_socket, parse_message, RData, Record, ResolvedNode, /* 必要なものだけ */};
```

`cargo build` の unresolved / private エラーに従って `mod.rs` 側の該当項目を `pub(super)` にし、`use super::{…}` を確定する。`mod.rs` の `mod` 宣言位置は `use` 群の直後:

```rust
mod cache;
pub use cache::*;
```

```bash
cargo build -p mat-controller 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -60
```

Expected: 出力なし。

- [ ] **Step 3: モジュール doc を現状に合わせる**

`mod.rs` 先頭 `//!` の「No advertising, no cache」の段落末尾（`[`bind_mdns_socket`] and [`QU_CLASS_IN`]). …` の文の後）に 1 文を追加する（他は不変）:

```rust
//! The resident cache `matd` uses (`OperationalCache`) lives in the `cache`
//! submodule; the one-shot resolvers here never touch it.
```

- [ ] **Step 4: cache の tests を移す**

`mod.rs` の `mod tests` から、cache 系テストとそのヘルパを `cache.rs` 末尾の `#[cfg(test)] mod tests` へ移す: `fold_operational_populates_cache_from_one_message`、`fold_operational_ignores_non_matter_and_incomplete`、`spawn_operational_cache_binds_on_loopback`、`sample_node`、`opcache_*`（4 つ）、`synth_srv_txt_only` / `synth_aaaa_class` / `synth_aaaa_only`（cache テスト専用なら移す。親 tests でも使うなら **親に残して `pub(super)`** にし、cache 側から `use super::super::tests::{…}` で参照 — 迷ったら親に残す方を選ぶ）、`parse_message_reads_cache_flush_bit`（`parse_message` は親なので **親に残す**）、`fold_cache_flush_*` / `fold_cross_datagram_*` / `fold_no_global_aaaa_starvation` / `fold_freshness_orders_latest_first` / `fold_full_pool_evicts_oldest_not_newest` / `fold_departed_instance_expires_and_is_not_refreshed`。`synth_response` は親の resolve テストも使うので親に残し `pub(super)`。

cache.rs の tests 先頭:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dnssd::tests::{synth_response, /* 使うものだけ */};
    use std::net::Ipv6Addr;
    use std::time::Duration;
}
```

親 `mod.rs` の `mod tests` を `pub(super) mod tests`（`#[cfg(test)]` 付きのまま）にし、共有ヘルパを `pub(super) fn` にする。

```bash
cargo test -p mat-controller --lib dnssd 2>&1 | /usr/bin/grep -E 'test result|FAILED|^error' | head
```

Expected: `test result: ok.`。

- [ ] **Step 5: fmt / clippy / テスト名比較 / 全ワークスペース**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | /usr/bin/grep -E '^(error|warning)' -A5 | head -40
cargo test -p mat-controller --lib -- --list 2>/dev/null | /usr/bin/grep ': test$' | sed 's/: test$//' | awk -F'::' '{print $NF}' | sort > /tmp/tests-after.txt
diff /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/367a993e-b64c-4ff3-8664-10373bcfb87b/scratchpad/tests-before.txt /tmp/tests-after.txt && echo SAME
cargo test --workspace 2>&1 | /usr/bin/grep -E 'test result: FAILED|^error' | head
wc -l crates/mat-controller/src/dnssd/*.rs
```

Expected: clippy 出力なし、`SAME`、FAILED なし、合計 ≈ 3,059 + `use`/doc 分。

- [ ] **Step 6: Commit**

```bash
/usr/bin/git add -A crates/mat-controller/src/dnssd crates/mat-controller/src/dnssd.rs
/usr/bin/git commit -F - <<'EOF'
refactor(mat-controller): dnssd の operational cache を dnssd/cache.rs へ（挙動不変）

OperationalCache / OperationalFold / fold_operational_into_cache /
run_operational_cache / spawn_operational_cache とその tests を行移動。
one-shot 解決と browse は dnssd/mod.rs に残る。公開パスは不変。

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KDp3tH1KJGSyTLqjVmYt6Z
EOF
```

---

### Task 5: 仕上げ — docs のファイル名追従・`task check`・pub API 不変の機械確認

**Files:**
- Modify: `ARCHITECTURE.md:692`（`dnssd.rs` → `dnssd/`）, `ARCHITECTURE.md:1143`（`im.rs` → `im/`）
- Modify: `docs/superpowers/plans/2026-09-05-controller-readability-split.md`（末尾に実測記録を追記）

**Interfaces:**
- Consumes: Task 1〜4 のコミット。分割前の最終コミットは `6ba0aab`（`decode_response_status` 修正）。
- Produces: なし（記録のみ）。

- [ ] **Step 1: ARCHITECTURE.md のファイル名言及を更新**

```bash
/usr/bin/grep -n 'im\.rs\|session\.rs\|dnssd\.rs' ARCHITECTURE.md README.md CLAUDE.md docs/*.md
```

出た行（`ARCHITECTURE.md:692` の「既存 `dnssd.rs`（operational 解決）」、`ARCHITECTURE.md:1143` の「im.rs の EventRequests/EventReportIB デコード追加」）を、それぞれ「既存 `dnssd/`（operational 解決）」「`im/read.rs` の EventRequests/EventReportIB デコード追加」に書き換える。`docs/superpowers/plans/` 配下の過去 plan は履歴なので触らない。

- [ ] **Step 2: `task check`**

```bash
task check > /tmp/check-split.log 2>&1; echo "exit=$?"; /usr/bin/grep -E -n 'FAILED|^error|^warning|task: Failed' /tmp/check-split.log | head
```

Expected: `exit=0`、grep 出力なし。

- [ ] **Step 3: 分割前後で mat-controller の公開 API が不変であることを機械確認**

```bash
cargo semver-checks check-release -p mat-controller --baseline-rev 6ba0aab --default-features 2>&1 | tail -15
```

Expected: `Summary no semver update required`（または「no changes」相当）。**breaking / minor が 1 件でも報告されたら**、それは分割のミス（`pub` 項目の取りこぼし、`pub(super)` へ落としすぎ、新しい `pub mod`）なので該当 Task の該当ファイルを直して再実行する。参考として v1.31.0 publish 基準（Task 1 より前のコミット `ead4741` の `ArgValue` 改名は mat-core の break で、ここには出ない）も 1 回回して棚卸しを記録する:

```bash
task semver 2>&1 | /usr/bin/grep -E 'Summary|--- failure|Checking' | head -40
```

- [ ] **Step 4: 実測を plan 末尾に追記**

本ファイル末尾に以下の節を追加し、実測値を埋める:

```markdown
## 実測記録（2026-09-05）

- im: `im.rs` 3,743 行 → `mod.rs` N / `json.rs` N / `read.rs` N / `subscribe.rs` N / `cmdfields.rs` N / `invoke.rs` N / `write.rs` N
- session: `session.rs` 4,228 行 → `mod.rs` N / `mrp.rs` N / `client.rs` N / `responder.rs` N / `subscribe.rs` N / `test_util.rs` N
- dnssd: `dnssd.rs` 3,059 行 → `mod.rs` N / `cache.rs` N
- テスト名多重集合: 475 = 475（SAME）
- `cargo semver-checks --baseline-rev 6ba0aab`（mat-controller）: <結果>
- `task semver`（crates.io 1.31.0 基準）: <クレート別件数>
```

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add ARCHITECTURE.md docs/superpowers/plans/2026-09-05-controller-readability-split.md
/usr/bin/git commit -F - <<'EOF'
docs: 可読性分割に伴うファイル名言及の追従と実測記録

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KDp3tH1KJGSyTLqjVmYt6Z
EOF
```

---

## Self-Review

- **Spec coverage**: im → read / write / invoke / subscribe / cmdfields / json（Task 1・2）、session 4 役（Task 3）、dnssd cache（Task 4）、pub API 最小変更 + `task semver` 棚卸し（Task 5）。実機スモーク（hogar-matd）は plan 外で親セッションが行う。
- **Placeholder scan**: 各ステップは切り出し範囲（関数名・行番号）・先頭 `use`・検証コマンド・期待値を持つ。行番号は「タスク開始時点」のものであり、切り出し後にずれるので **関数名を正**とする旨を各所に明記。
- **Type consistency**: `pub(super)` に上げる名前（`expect_struct_start` / `skip_container` / `value_to_im` / `encode_im_value` / `decode_attribute_path_ib` / `AttributePathFields` / `decode_attribute_requests`）は Task 1 の Produces と Task 2 の Consumes で一致。session の行き先メソッド一覧は Step 2 と各ファイルのコメントで一致。
