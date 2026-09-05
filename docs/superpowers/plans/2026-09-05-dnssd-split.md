# mat-controller `dnssd/mod.rs` 分割（codec / resolve / browse）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `crates/mat-controller/src/dnssd/mod.rs`（2,362 行）を **挙動不変・機械的** に「DNS ワイヤコーデック」「one-shot 解決（operational / commissionable）」「browse」の 3 サブモジュールに分け、`mat_controller::dnssd::X` の公開パスを 1 つも変えない。

**Architecture:** 前回の可読性分割（`docs/superpowers/plans/2026-09-05-controller-readability-split.md`）と同じ作法。`dnssd/mod.rs` には定数・`DnssdError`・`ResolvedNode`・ソケット bind・共通ヘルパを残し、private サブモジュール `codec` / `resolve` / `browse` + `pub use <sub>::*;` の glob 再エクスポートで従来パスを維持する。サブモジュール間で共有する元 private 項目は `pub(super)`。テストは「テスト対象の関数を持つサブモジュール」へ移し、複数サブモジュール（cache.rs 含む）にまたがる合成応答ヘルパは `#[cfg(test)] mod test_util;` に置く。

**Tech Stack:** Rust 2021 workspace、tokio。検証は `cargo fmt` / `cargo clippy -D warnings` / `cargo test -p mat-controller`、テスト名の多重集合比較、`cargo semver-checks`。

**Spec:** ユーザー指示（2026-09-05 セッション、監査バックログ メモリ `mat-code-audit-2026-08-31` の残フォロー (5)）: 「mat-controller `dnssd/mod.rs`（2,362 行）をコーデック / one-shot 解決 / browse に分割（挙動不変、pub API 不変）。fragile part なので既存の unit + 実機テストは全部維持」。

## Global Constraints

- **挙動不変**: 関数本体・定数値・テスト本体は 1 文字も変えない（移動・可視性・`use` の調整・モジュール doc コメントのみ可）。ロジックの「ついで修正」は禁止。気づいた点はタスク報告に書くだけ。
- **公開パス不変**: 他クレート（mat-native / mat / matd / **mat-device / matv は並行セッションが触るので絶対に編集しない**）が使う `mat_controller::dnssd::{DnssdError, ResolvedNode, CommissionableInstance, OperationalCache, OPERATIONAL_RESOLVE_TIMEOUT, BROWSE_WINDOW, operational_instance, iface_index, resolve_operational, resolve_operational_many, resolve_commissionable, browse_commissionable, spawn_operational_cache}` はそのまま解決すること。サブモジュールは `mod x;`（private）+ `pub use x::*;`。`pub mod` にしない。
- **触ってよいファイル**: `crates/mat-controller/src/dnssd/` 配下だけ（`mod.rs` / `cache.rs` / 新規 3 + `test_util.rs`）。**`group_settings.rs` / `kvs.rs` / `group.rs` / `case.rs` と `crates/mat-device/` は並行セッションの領域なので絶対に編集しない**。
- **クレート内の可視性**: サブモジュール間で共有する元 private 項目は `pub(super)`。`pub(crate)` は使わない。`mod.rs` に残る private 項目は子モジュールから `super::name` で見えるので可視性を変えない。
- **テスト不変**: mat-controller lib のテストは移動前後で **同じ名前の多重集合（476 個、うち dnssd 47 個）**。テストを削除・改名・統合しない。テスト本体は 1 文字も変えない（`use` 行の追加は可）。
- **各タスク終了時**: `cargo fmt --all` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test -p mat-controller` → テスト名比較、が全部通ること。
- **コミット**: タスクごとに 1 つ。`git` は `/usr/bin/git` をフルパスで呼ぶ（rtk フックの書き換え回避）。メッセージ末尾に
  `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` と
  `Claude-Session: https://claude.ai/code/session_01YVMF7oEDFkVZcQQSRNgxLQ` を付ける。
- **ワークツリー**: `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-cleanup2`（ブランチ `worktree-controller-cleanup2`）。パスはすべてこの下の相対パス。元リポジトリ root には `cd` しない。

## File Structure

現 `dnssd/mod.rs` の行範囲（分割前、2,362 行）は次のとおり:

| 現行範囲 | 内容 | 行き先 |
|---|---|---|
| 1–31 | モジュール doc | `mod.rs`（残す。末尾にサブモジュール地図を追記） |
| 32–41 | `use` 群、`mod cache; pub use cache::*;` | `mod.rs` |
| 43–73 | 定数 `MDNS_GROUP` / `MDNS_PORT` / `TYPE_*` / `CLASS_IN` / `QU_CLASS_IN` / `MRP_*` / `QUERY_RESEND_INTERVAL` / `OPERATIONAL_RESOLVE_TIMEOUT` | `mod.rs`（残す） |
| 75–106 | `DnssdError` + Display + Error | `mod.rs` |
| 108–125 | `operational_instance` / `iface_index` | `mod.rs` |
| 127–174 | `bind_mdns_socket` / `is_link_local` | `mod.rs` |
| 176–223 | `ResolvedNode` + impl | `mod.rs` |
| 225–586 | `push_name` / `encode_query` / `KNOWN_ANSWER_PACKET_BUDGET` / `encode_ptr_query_with_known` / `read_name` / `RData` / `Record` / `MIN_RECORD_LEN` / `MAX_AAAA` / `record_capacity` / `push_aaaa` / `prune_aaaa` / `be16` / `be32` / `parse_message` / `txt_u32` | **`codec.rs`** |
| 1210–1225 | `txt_str`（browse 節にあるが TXT ヘルパ = `txt_u32` の兄弟） | **`codec.rs`** |
| 588–955 | `OperationalQuery` + impl / `resolve_operational_many` / `resolve_operational` / `long_discriminator_subtype` / `build_commissionable` / `commissionable_from_response` / `resolve_commissionable` | **`resolve.rs`** |
| 957–1262（`txt_str` を除く） | `// ── browse` 見出し以降: `BROWSE_WINDOW` / `MAX_INSTANCES` / `MAX_BROWSE_AAAA` / `MAX_QUESTIONS_PER_MSG` / `CommissionableInstance` / `FoldedInstance` / `InstanceFold` / `BrowseFold` + impl / `browse` / `browse_commissionable` / `split_vp` / `hostname_from_target` / `commissionable_from_fold` | **`browse.rs`** |
| 1264–2362 | `mod tests` | 下表のとおり 4 か所へ分配 |

テストの所属（`#[cfg(test)] mod tests` を各ファイルに置く。ヘルパは `test_util.rs`）:

| ファイル | 置くテスト / ヘルパ |
|---|---|
| `dnssd/test_util.rs`（`#[cfg(test)]`、全部 `pub(super)`） | `synth_response`（現 1322–1366）、`multicast_ifaces`（1368–1411）、`spawn_multicast_announcer`（1413–1431）、`spawn_unicast_responder`（1433–1463）、`synth_commissionable_response`（1642–1697）、`synth_aaaa_class`（2337–2350） |
| `dnssd/mod.rs` の `mod tests` | `instance_name_matches_avahi_form`、`mrp_config_uses_sii_and_clamps`、`mrp_config_uses_sai_for_active_interval`、`socket_addrs_prefers_non_link_local_and_scopes_link_local` |
| `dnssd/codec.rs` の `mod tests` | `encodes_srv_query`、`every_question_sets_qu_unicast_response_bit`、`parses_srv_txt_aaaa_with_compression`、`record_capacity_clamps_forged_counts`、`aaaa_fold_caps_growth_before_srv_is_known`、`aaaa_fold_dedupes`、`aaaa_fold_filters_on_srv_target_once_known`、`aaaa_prune_frees_flooded_slots_for_the_real_target`、`rejects_compression_pointer_loop`、`malformed_ptr_does_not_abort_datagram_parsing`、`known_answer_query_degenerates_without_known`、`known_answer_query_roundtrips_through_parser`、`known_answer_query_splits_and_sets_tc`、`parse_message_reads_cache_flush_bit` |
| `dnssd/resolve.rs` の `mod tests` | `resolve_commissionable_receives_multicast_only_response`、`resolve_operational_receives_multicast_only_response`、`resolve_operational_many_demuxes_unicast_only_responses`、`extracts_commissionable_from_ptr_srv_txt_aaaa`、`rejects_mismatched_discriminator` |
| `dnssd/browse.rs` の `mod tests` | ローカルヘルパ `synth_browse_response`（2018–2090、browse テストだけが使うのでここに private のまま）、`browse_receives_multicast_only_announcement`、`browse_fold_collects_two_instances_from_bundled_responses`、`browse_fold_is_order_independent_within_a_datagram`、`browse_fold_dedupes_instances_and_caps_growth`、`browse_fold_ignores_records_for_other_services`、`browse_finish_sorts_link_local_after_global_through_fold`、`browse_pending_questions_lists_missing_srv_txt_aaaa`、`commissionable_from_fold_parses_txt_hostname_and_sorts_addresses`、`commissionable_from_fold_accepts_vendor_only_vp_and_skips_empty`、`record_ttl_is_parsed`（`synth_browse_response` + `BrowseFold` を使うので browse 側） |
| `dnssd/cache.rs`（既存、tests の `use` 行のみ変更） | `use crate::dnssd::tests::{synth_aaaa_class, synth_commissionable_response, synth_response};` → `use crate::dnssd::test_util::{synth_aaaa_class, synth_commissionable_response, synth_response};`。`use crate::dnssd::{iface_index, push_name, CLASS_IN};` は `push_name` が `codec` へ移るので `use crate::dnssd::codec::push_name; use crate::dnssd::{iface_index, CLASS_IN};` に分ける |

### テスト名比較コマンド（全タスク共通）

移動前の名前一覧は `/tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/95f53270-fd14-4efe-b1ae-da25e2af3f4a/scratchpad/tests-before.txt`（476 行、`sort` 済み、関数名だけ）。各タスクの最後にこれを実行し **差分ゼロ** を確認する:

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-cleanup2
cargo test -p mat-controller --lib -- --list 2>/dev/null | /usr/bin/grep ': test$' | sed 's/: test$//' | awk -F'::' '{print $NF}' | sort > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/95f53270-fd14-4efe-b1ae-da25e2af3f4a/scratchpad/tests-after.txt
diff /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/95f53270-fd14-4efe-b1ae-da25e2af3f4a/scratchpad/tests-before.txt /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/95f53270-fd14-4efe-b1ae-da25e2af3f4a/scratchpad/tests-after.txt && echo SAME
```

Expected: `SAME`（差分行が 1 行でも出たらテストの取りこぼし・重複）。

### 移動の作法（全タスク共通）

1. 移す範囲は **doc コメント（`///` / `//` 見出し）ごと** 切り取る。切り取り元には何も残さない（空行の連続は `cargo fmt` が整える）。
2. 切り取った本体は **そのまま貼る**。可視性キーワードの追加（`fn` → `pub(super) fn`、`struct` → `pub(super) struct`、`const` → `pub(super) const`）と、必要な `use` の追加だけ行う。`pub(super) struct` にした構造体のフィールドは、親や兄弟が直接触る場合だけ `pub(super)` を付ける（`Record` の `name` / `rdata` / `ttl` / `cache_flush`、`RData` の variant は enum なので不要）。
3. 各サブモジュールの先頭は 2〜5 行のモジュール doc（`//!`）+ `use` 群。`use super::{...}` で親の項目、`use super::codec::{...}` で兄弟の項目を取る。**`use super::*;` はサブモジュール本体では使わない**（何を共有しているか読めるように）。`mod tests` 内は従来どおり `use super::*;` でよい。
4. 貼った後に `cargo build -p mat-controller --all-targets` を回し、unresolved になった名前だけ `pub(super)` に上げる／`use` を足す。「念のため」の可視性拡大はしない。
5. 実際の移動は `sed -n 'A,Bp' mod.rs >> new.rs` でブロックを取り出し、`sed -i 'A,Bd' mod.rs` で消す（範囲は **後ろから** 消して行番号ずれを防ぐ）。`awk`/エディタ手作業でも可。移動後に `/usr/bin/grep -c` で「元ファイルから消えた関数名が新ファイルに 1 回だけある」ことを確認する。

---

### Task 1: `dnssd/codec.rs` と `dnssd/test_util.rs` を切り出す

**Files:**
- Create: `crates/mat-controller/src/dnssd/codec.rs`
- Create: `crates/mat-controller/src/dnssd/test_util.rs`
- Modify: `crates/mat-controller/src/dnssd/mod.rs`（225–586 行、1210–1225 行、tests のコーデック系 14 本 + 共有ヘルパ 6 本を切り出す）
- Modify: `crates/mat-controller/src/dnssd/cache.rs:308-310`（tests の `use` 行）

**Interfaces:**
- Consumes: `mod.rs` に残る `MDNS_GROUP` / `MDNS_PORT` / `TYPE_PTR` / `TYPE_TXT` / `TYPE_AAAA` / `TYPE_SRV` / `CLASS_IN` / `QU_CLASS_IN` / `DnssdError` / `bind_mdns_socket` / `is_link_local` / `iface_index`（すべて子から `super::` で見える。可視性変更不要）。
- Produces: `codec` の `pub(super)` 項目 — `push_name(out: &mut Vec<u8>, name: &str)`、`encode_query(id: u16, questions: &[(&str, u16)]) -> Vec<u8>`、`encode_ptr_query_with_known(service: &str, known: &[(String, u32)]) -> Vec<Vec<u8>>`、`enum RData`、`struct Record { name, rdata, ttl, cache_flush }`、`MAX_AAAA`、`push_aaaa(...)`、`prune_aaaa(...)`、`parse_message(buf: &[u8]) -> Result<Vec<Record>, DnssdError>`、`txt_u32(strings: &[Vec<u8>], key: &str) -> Option<u32>`、`txt_str<'a>(strings: &'a [Vec<u8>], key: &str) -> Option<&'a str>`（`read_name` / `be16` / `be32` / `record_capacity` / `MIN_RECORD_LEN` / `KNOWN_ANSWER_PACKET_BUDGET` は codec 内 private のまま。`record_capacity` はテストが同モジュール内なので private でよい）。`test_util` の `pub(super)` 項目 — `synth_response` / `synth_commissionable_response` / `synth_aaaa_class` / `multicast_ifaces` / `spawn_multicast_announcer` / `spawn_unicast_responder`（シグネチャは現行どおり）。

- [ ] **Step 1: 分割前の状態を確認**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-cleanup2
wc -l crates/mat-controller/src/dnssd/mod.rs      # 2362
/usr/bin/grep -n -E "^fn push_name|^fn txt_u32|^fn txt_str|^struct OperationalQuery|^/// \[\`resolve_operational_many\`\] の per-node" crates/mat-controller/src/dnssd/mod.rs
```

Expected: `push_name` 225、`txt_u32` 571、`txt_str` 1211、`OperationalQuery` の doc 586–587 / struct 588。行番号が違えば以降の範囲を読み替える（内容で切る）。

- [ ] **Step 2: `codec.rs` を作る**

先頭に置く doc と `use`:

```rust
//! DNS ワイヤコーデック（RFC 1035 / RFC 6762）: 質問の符号化（QU ビット、
//! Known-Answer 分割）と応答の復号（名前圧縮、SRV/TXT/AAAA/PTR の `Record`）、
//! AAAA 候補プールの上限付き fold、TXT の `key=value` 取り出し。
//! ソケットは持たない純粋関数群 — `resolve` / `browse` / `cache` が共有する。

use std::net::Ipv6Addr;

use super::{DnssdError, QU_CLASS_IN, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT};
```

続けて現 `mod.rs` 223 行目（`impl ResolvedNode` の閉じ `}` の次の空行）以降、`/// Appends \`name\` in DNS label form` の doc から `txt_u32` の閉じ `}`（586 行）までを **そのまま** 移す。次に browse 節の `txt_str`（`/// TXT から文字列値` doc 込み、1210–1225）を `txt_u32` の直後に移す。可視性を付ける: `push_name` / `encode_query` / `encode_ptr_query_with_known` / `RData` / `Record`（+ 4 フィールド） / `MAX_AAAA` / `push_aaaa` / `prune_aaaa` / `parse_message` / `txt_u32` / `txt_str` を `pub(super)`。`Record` のフィールドは:

```rust
pub(super) struct Record {
    pub(super) name: String,
    pub(super) rdata: RData,
    pub(super) ttl: u32,
    pub(super) cache_flush: bool,
}
```

（現行のフィールド名・順序・型はそのまま。`#[derive]` も変えない。）実際の `use` は `cargo build` の unresolved で確定する（`CLASS_IN` / `MDNS_*` を codec 本体は使わないはずなので入れない。tests 側は `use super::*;` + 必要なら `use super::super::{CLASS_IN, ...}`）。

- [ ] **Step 3: `test_util.rs` を作る**

```rust
//! dnssd テスト共有ヘルパ（合成 mDNS 応答、実 iface 上の multicast / unicast
//! 応答器）。`codec` / `resolve` / `browse` / `cache` の tests から使う。
#![cfg(test)]

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

use tokio::net::UdpSocket;

use super::codec::push_name;
use super::{bind_mdns_socket, MDNS_GROUP, MDNS_PORT, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT};
```

現 tests から `synth_response`（doc 込み 1318–1366）、`multicast_ifaces`（1368–1411）、`spawn_multicast_announcer`（1413–1431）、`spawn_unicast_responder`（1433–1463）、`synth_commissionable_response`（doc 込み 1638–1697）、`synth_aaaa_class`（doc 込み 2336–2350）を **インデントを 4 つ外して** 移し、`fn` を全部 `pub(super) fn` にする（`synth_*` は既に `pub(super)`）。`multicast_ifaces` 内のローカル `const IFF_UP` 等はそのまま。`use` は build で確定（`spawn_*` が `tokio::task::JoinHandle` 等を返すならその型も）。`mod.rs` には `#[cfg(test)] mod test_util;` を `mod cache;` の隣に足す（`pub use` しない）。

- [ ] **Step 4: `mod.rs` の tests をコーデック分だけ `codec.rs` へ**

`codec.rs` 末尾に:

```rust
#[cfg(test)]
mod tests {
    use super::super::test_util::{synth_aaaa_class, synth_response};
    use super::super::CLASS_IN;
    use super::*;
```

を置き、File Structure の表にある codec 所属 14 本（`encodes_srv_query` 〜 `parse_message_reads_cache_flush_bit`）を `#[test]` 属性・doc コメントごと移す。`malformed_ptr_does_not_abort_datagram_parsing` は `txt_str` / `TYPE_PTR` / `push_name` を使う（すべて codec か親にある）。必要な `use`（`Ipv6Addr` 等）は build で足す。

- [ ] **Step 5: `mod.rs` を整える**

`mod cache;` の周りを次の形に:

```rust
mod browse;   // Task 3 で作る。Task 1 時点ではこの行はまだ書かない
mod cache;
mod codec;
mod resolve;  // Task 2 で作る。Task 1 時点ではこの行はまだ書かない
#[cfg(test)]
mod test_util;
pub use browse::*;   // Task 3
pub use cache::*;
pub use resolve::*;  // Task 2
```

Task 1 では `mod cache; mod codec; #[cfg(test)] mod test_util; pub use cache::*;` まで。`codec` は pub 項目を持たないので `pub use codec::*;` は書かない。`mod.rs` に残った `resolve` / `browse` 本体からは `codec` の項目を `use codec::{encode_query, encode_ptr_query_with_known, parse_message, prune_aaaa, push_aaaa, txt_str, txt_u32, RData, Record};` で取る（Task 2/3 でそれぞれのファイルへ持っていく）。`mod.rs` に残る tests の `use super::*;` は codec の `pub(super)` 項目を glob では拾えないことがあるので、残るテスト（`mrp_config_*` 等）が使う名前だけ `use super::codec::*;` を tests 内に足す。

`cache.rs` の tests の `use` を File Structure の表どおりに書き換える。

- [ ] **Step 6: ビルド・fmt・clippy・テスト・名前比較**

```bash
cargo build -p mat-controller --all-targets 2>&1 | /usr/bin/grep -E '^(error|warning)' | head -30
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
cargo test -p mat-controller 2>&1 | /usr/bin/grep -E 'test result|FAILED|panicked' | head
```

続けて「テスト名比較コマンド」を実行。Expected: clippy 警告 0、全 `test result: ok`、`SAME`。

未使用 import 警告が出たら該当 `use` を消す（clippy `-D warnings` で落ちる）。`dead_code` 警告が出た項目は「どこからも使われなくなった」のではなく可視性の付け忘れ（`pub(super)` を付けると消える）か、テスト専用ヘルパの `#[cfg(test)]` 漏れ。

- [ ] **Step 7: 移動漏れ確認とコミット**

```bash
for f in push_name encode_query encode_ptr_query_with_known read_name parse_message txt_u32 txt_str push_aaaa prune_aaaa record_capacity; do printf '%s mod=%s codec=%s\n' $f "$(/usr/bin/grep -c "fn $f\b" crates/mat-controller/src/dnssd/mod.rs)" "$(/usr/bin/grep -c "fn $f\b" crates/mat-controller/src/dnssd/codec.rs)"; done
wc -l crates/mat-controller/src/dnssd/*.rs
/usr/bin/git add crates/mat-controller/src/dnssd
/usr/bin/git commit -m "refactor(mat-controller): dnssd のワイヤコーデックを dnssd/codec.rs へ、テスト共有ヘルパを test_util.rs へ（挙動不変）

push_name / encode_query / encode_ptr_query_with_known / read_name / parse_message /
Record / RData / push_aaaa / prune_aaaa / txt_u32 / txt_str と、そのテスト 14 本を
codec.rs へ。synth_* / multicast_ifaces / spawn_* の 6 ヘルパは cache.rs の
テストとも共有するため #[cfg(test)] mod test_util へ。公開パスは不変。

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YVMF7oEDFkVZcQQSRNgxLQ"
```

Expected: 各関数が mod=0 / codec=1。

---

### Task 2: `dnssd/resolve.rs` を切り出す

**Files:**
- Create: `crates/mat-controller/src/dnssd/resolve.rs`
- Modify: `crates/mat-controller/src/dnssd/mod.rs`（`OperationalQuery` 〜 `resolve_commissionable` と、resolve 系テスト 5 本）

**Interfaces:**
- Consumes: 親の `DnssdError` / `ResolvedNode` / `operational_instance` / `bind_mdns_socket` / `is_link_local` / `MDNS_GROUP` / `MDNS_PORT` / `TYPE_*` / `QUERY_RESEND_INTERVAL`（`super::`）、Task 1 の `codec::{encode_query, parse_message, prune_aaaa, push_aaaa, txt_u32, RData}`、`test_util::{synth_response, synth_commissionable_response, multicast_ifaces, spawn_multicast_announcer, spawn_unicast_responder}`。
- Produces: `pub async fn resolve_operational_many(scope_id: u32, compressed_fabric_id: &[u8; 8], node_ids: &[u64], timeout: Duration) -> Result<Vec<(u64, Result<ResolvedNode, DnssdError>)>, DnssdError>`、`pub async fn resolve_operational(scope_id, compressed_fabric_id, node_id: u64, timeout) -> Result<ResolvedNode, DnssdError>`、`pub async fn resolve_commissionable(scope_id: u32, long_discriminator: u16, timeout: Duration) -> Result<ResolvedNode, DnssdError>`（すべて現行シグネチャのまま、`pub use resolve::*;` で `mat_controller::dnssd::` 直下に見える）。`long_discriminator_subtype` / `build_commissionable` / `commissionable_from_response` / `OperationalQuery` は resolve 内 private。

- [ ] **Step 1: `resolve.rs` を作る**

```rust
//! One-shot 解決: operational（`<CFID>-<NodeId>._matter._tcp`、複数ノードを
//! 1 ソケットで demux する `resolve_operational_many`）と commissionable
//! （`_L<disc>._sub._matterc._udp` の PTR → SRV/TXT/AAAA）。SRV + target
//! 一致 AAAA が揃った時点で早期 return する（browse と違い全員は集めない）。

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::time::Instant;

use super::codec::{encode_query, parse_message, prune_aaaa, push_aaaa, txt_u32, RData};
use super::{
    bind_mdns_socket, is_link_local, operational_instance, DnssdError, ResolvedNode, MDNS_GROUP,
    MDNS_PORT, QUERY_RESEND_INTERVAL, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT,
};
```

現 `mod.rs` の `/// [\`resolve_operational_many\`] の per-node fold 状態。` doc から `resolve_commissionable` の閉じ `}`（`// ── browse` 見出しの直前）までを **そのまま** 移す。可視性は変えない（`pub async fn` 3 本は既に `pub`、残りは private のまま）。

- [ ] **Step 2: tests を移す**

`resolve.rs` 末尾に:

```rust
#[cfg(test)]
mod tests {
    use super::super::test_util::{
        multicast_ifaces, spawn_multicast_announcer, spawn_unicast_responder,
        synth_commissionable_response, synth_response,
    };
    use super::*;
```

File Structure の resolve 所属 5 本（`resolve_commissionable_receives_multicast_only_response` / `resolve_operational_receives_multicast_only_response` / `resolve_operational_many_demuxes_unicast_only_responses` / `extracts_commissionable_from_ptr_srv_txt_aaaa` / `rejects_mismatched_discriminator`）を属性・doc ごと移す。`#[tokio::test]` の 3 本は実 iface を要求する（`multicast_ifaces()` が空なら早期 return する作り）— 本体は変えない。

- [ ] **Step 3: `mod.rs` に `mod resolve; pub use resolve::*;` を足し、`mod.rs` 内で不要になった `use`（`Instant` / `codec::...` 等）を消す**

- [ ] **Step 4: ビルド・fmt・clippy・テスト・名前比較**

Task 1 Step 6 と同じコマンド。Expected: 警告 0、全 ok、`SAME`。加えて外部利用者が壊れていないことを:

```bash
cargo build --workspace --all-targets 2>&1 | /usr/bin/grep -E '^error' | head
```

Expected: 出力なし。

- [ ] **Step 5: コミット**

```bash
for f in resolve_operational_many resolve_operational resolve_commissionable commissionable_from_response build_commissionable long_discriminator_subtype; do printf '%s mod=%s resolve=%s\n' $f "$(/usr/bin/grep -c "fn $f\b" crates/mat-controller/src/dnssd/mod.rs)" "$(/usr/bin/grep -c "fn $f\b" crates/mat-controller/src/dnssd/resolve.rs)"; done
/usr/bin/git add crates/mat-controller/src/dnssd
/usr/bin/git commit -m "refactor(mat-controller): dnssd の one-shot 解決（operational / commissionable）を dnssd/resolve.rs へ（挙動不変）

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YVMF7oEDFkVZcQQSRNgxLQ"
```

Expected: 各関数が mod=0 / resolve=1。

---

### Task 3: `dnssd/browse.rs` を切り出し、`mod.rs` の doc にサブモジュール地図を書く

**Files:**
- Create: `crates/mat-controller/src/dnssd/browse.rs`
- Modify: `crates/mat-controller/src/dnssd/mod.rs`（browse 節と browse 系テスト 10 本 + `synth_browse_response`、モジュール doc 末尾）

**Interfaces:**
- Consumes: 親の `DnssdError` / `bind_mdns_socket` / `is_link_local` / `MDNS_GROUP` / `MDNS_PORT` / `TYPE_*` / `QUERY_RESEND_INTERVAL`、`codec::{encode_query, encode_ptr_query_with_known, parse_message, txt_str, txt_u32, RData, Record}`、`test_util::{synth_commissionable_response, multicast_ifaces, spawn_multicast_announcer}`。
- Produces: `pub const BROWSE_WINDOW: Duration`、`pub struct CommissionableInstance { hostname, port, addresses, discriminator, vendor_id, product_id }`、`pub async fn browse_commissionable(scope_id: u32, window: Duration) -> Result<Vec<CommissionableInstance>, DnssdError>`（現行シグネチャのまま — 実際の引数は現行コードを正とする）。`FoldedInstance` / `InstanceFold` / `BrowseFold` / `browse` / `split_vp` / `hostname_from_target` / `commissionable_from_fold` / `MAX_INSTANCES` / `MAX_BROWSE_AAAA` / `MAX_QUESTIONS_PER_MSG` は browse 内 private。

- [ ] **Step 1: `browse.rs` を作る**

```rust
//! One-shot browse（M8b: `discover` の native 化）: `_matterc._udp` の PTR を
//! 列挙し、instance ごとに SRV/TXT/AAAA を固定窓（[`BROWSE_WINDOW`]）まで
//! 畳み込む。early return しない。Known-Answer 抑制で TC 切り捨て応答を回避
//! （実機 2026-07 の 29+ instance 観測）。operational の到達性判定は browse
//! ではなく `resolve_operational` の targeted resolve（mod.rs の doc 参照）。

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::time::Instant;

use super::codec::{encode_query, encode_ptr_query_with_known, parse_message, txt_str, txt_u32, RData, Record};
use super::{
    bind_mdns_socket, is_link_local, DnssdError, MDNS_GROUP, MDNS_PORT, QUERY_RESEND_INTERVAL,
    TYPE_AAAA, TYPE_SRV, TYPE_TXT,
};
```

現 `mod.rs` の `// ── browse（M8b: discover native 化）` 見出しから `commissionable_from_fold` の閉じ `}`（tests の直前）までを **そのまま** 移す（`txt_str` は Task 1 で既に codec へ移っている）。見出し行 `// ── browse …` は browse.rs では不要なので削除してよい（doc `//!` に置き換わる）。

- [ ] **Step 2: tests を移す**

`browse.rs` 末尾に:

```rust
#[cfg(test)]
mod tests {
    use super::super::codec::push_name;
    use super::super::test_util::{
        multicast_ifaces, spawn_multicast_announcer, synth_commissionable_response,
    };
    use super::super::{CLASS_IN, TYPE_PTR};
    use super::*;
```

`synth_browse_response`（doc + `#[allow(clippy::too_many_arguments)]` 込み）と File Structure の browse 所属 10 本（`browse_receives_multicast_only_announcement`、`browse_fold_*` 5 本、`browse_pending_questions_lists_missing_srv_txt_aaaa`、`commissionable_from_fold_*` 2 本、`record_ttl_is_parsed`）を移す。`browse_receives_multicast_only_announcement` は `resolve_operational` / `resolve_commissionable` を名前で参照する場合があるが（doc 内の言及）、コード参照なら `use super::super::{resolve_commissionable, resolve_operational};` を足す。

- [ ] **Step 3: `mod.rs` を最終形に**

`mod.rs` に残るのは: doc、`use`、定数、`DnssdError`、`operational_instance`、`iface_index`、`bind_mdns_socket`、`is_link_local`、`ResolvedNode` + impl、`mod` 宣言 + `pub use`、`mod tests`（4 本: `instance_name_matches_avahi_form` / `mrp_config_uses_sii_and_clamps` / `mrp_config_uses_sai_for_active_interval` / `socket_addrs_prefers_non_link_local_and_scopes_link_local`）。`mod` 節は:

```rust
mod browse;
mod cache;
mod codec;
mod resolve;
#[cfg(test)]
mod test_util;
pub use browse::*;
pub use cache::*;
pub use resolve::*;
```

モジュール doc（1–31 行）の末尾に、サブモジュール地図を追記する（既存文は変えない）:

```rust
//!
//! Layout: `codec` (wire encode/decode, no sockets) · `resolve` (one-shot
//! operational / commissionable resolve with early return) · `browse`
//! (fixed-window `_matterc._udp` enumeration) · `cache` (`matd`'s resident
//! `OperationalCache`). This file keeps the shared constants, `DnssdError`,
//! `ResolvedNode`, and the 5353-bound query socket (`bind_mdns_socket`).
```

`mod.rs` の `use` から不要になったもの（`Instant` / `SocketAddr` 等）を消す。`MDNS_GROUP` / `MDNS_PORT` / `TYPE_*` / `CLASS_IN` / `QU_CLASS_IN` / `QUERY_RESEND_INTERVAL` は子が `super::` で使うので private のままでよい（`dead_code` 警告が出た定数だけ本当に未使用なので、その場合は **消さずに** 報告する — 挙動不変の範囲で `#[allow(dead_code)]` も付けない。Task 1〜3 で全部使われているはず）。

- [ ] **Step 4: ビルド・fmt・clippy・テスト・名前比較・行数**

Task 1 Step 6 と同じコマンド + `cargo build --workspace --all-targets`。続けて:

```bash
wc -l crates/mat-controller/src/dnssd/*.rs
cargo test -p mat-controller --lib -- --list 2>/dev/null | /usr/bin/grep ': test$' | /usr/bin/grep -c dnssd
```

Expected: 警告 0、全 ok、`SAME`、dnssd テスト 47、`mod.rs` は 400 行前後（doc 31 + 定数 + error + ResolvedNode + tests 4 本）、`codec.rs` / `resolve.rs` / `browse.rs` はいずれも 1,000 行未満。

- [ ] **Step 5: コミット**

```bash
/usr/bin/git add crates/mat-controller/src/dnssd
/usr/bin/git commit -m "refactor(mat-controller): dnssd の browse を dnssd/browse.rs へ、mod.rs にサブモジュール地図（挙動不変）

dnssd/mod.rs 2,362 行 → mod（定数・DnssdError・ResolvedNode・bind_mdns_socket）
+ codec / resolve / browse / cache / test_util。公開パス不変。

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YVMF7oEDFkVZcQQSRNgxLQ"
```

---

### Task 4: 公開 API 不変の機械確認と実測記録

**Files:**
- Modify: `docs/superpowers/plans/2026-09-05-dnssd-split.md`（本ファイル末尾に実測記録）

**Interfaces:**
- Consumes: Task 1〜3 のコミット。
- Produces: `cargo semver-checks -p mat-controller --baseline-rev 324fb10` = 破壊なし、の記録。

- [ ] **Step 1: semver-checks（分割直前の main = `324fb10` 基準）**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/controller-cleanup2
cargo semver-checks -p mat-controller --baseline-rev 324fb10 --default-features 2>&1 | tail -5
```

Expected: `Summary no semver update required`（または同義）。`major` 判定が出たら **修正せず報告**（分割で pub パスが変わったということなので Task 1〜3 のどれかで `pub use` 漏れ）。`cargo-semver-checks` が無ければ `cargo install cargo-semver-checks --locked` してから。

- [ ] **Step 2: 公開項目の名前一覧を分割前後で比較**

```bash
/usr/bin/git show 324fb10:crates/mat-controller/src/dnssd/mod.rs | /usr/bin/grep -oE '^pub (async fn|fn|struct|enum|const) [A-Za-z_]+' | sort > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/95f53270-fd14-4efe-b1ae-da25e2af3f4a/scratchpad/pub-before.txt
cat crates/mat-controller/src/dnssd/{mod,codec,resolve,browse}.rs | /usr/bin/grep -oE '^pub (async fn|fn|struct|enum|const) [A-Za-z_]+' | sort > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/95f53270-fd14-4efe-b1ae-da25e2af3f4a/scratchpad/pub-after.txt
diff /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/95f53270-fd14-4efe-b1ae-da25e2af3f4a/scratchpad/pub-before.txt /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/95f53270-fd14-4efe-b1ae-da25e2af3f4a/scratchpad/pub-after.txt && echo SAME
```

Expected: `SAME`（`pub(super)` は `^pub ` の後に `(` が来るので一致しない = 数えない）。

- [ ] **Step 3: `task check`**

```bash
task check 2>&1 | tail -15
```

Expected: fmt:check / clippy / test すべて緑。

- [ ] **Step 4: 実測を本ファイル末尾に追記してコミット**

```markdown
## 実測記録（2026-09-05）

- `dnssd/mod.rs` 2,362 行 → `mod.rs` N / `codec.rs` N / `resolve.rs` N / `browse.rs` N / `test_util.rs` N（`cache.rs` 727 は不変、合計 N）
- mat-controller lib テスト 476（dnssd 47）不変、`cargo semver-checks -p mat-controller --baseline-rev 324fb10` = 破壊なし
- `task check` 緑
```

```bash
/usr/bin/git add docs/superpowers/plans/2026-09-05-dnssd-split.md
/usr/bin/git commit -m "docs: dnssd 分割の実測記録

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YVMF7oEDFkVZcQQSRNgxLQ"
```

---

## Self-Review

- **Spec coverage**: コーデック（Task 1）/ one-shot 解決（Task 2）/ browse（Task 3）の 3 分割、挙動不変（Global Constraints + 各タスクの「そのまま移す」）、pub API 不変（`pub use` glob + Task 4 の semver-checks と名前一覧比較）、unit テスト全維持（テスト名多重集合 476 / dnssd 47 を各タスクで比較）。実機テスト（hogar-matd コンテナ内の直経路 read = mDNS resolve 経路、matd 経由 read = cache 経路）は plan 外で親セッションが行う。
- **Placeholder scan**: 行番号は分割前の実測。各タスクは「内容で切る」と明記してあるので、前タスクで行番号がずれても成立する。
- **Type consistency**: `codec` の `pub(super)` 項目名は Task 1 の Produces と Task 2/3 の `use super::codec::{...}` で一致。`test_util` の 6 ヘルパ名は Task 1 Produces と Task 2/3/cache.rs の `use` で一致。

## 実測記録（2026-09-05）

- `dnssd/mod.rs` 2,362 行 → `mod.rs` 332 / `codec.rs` 721 / `resolve.rs` 569 / `browse.rs` 609 / `test_util.rs` 220（`cache.rs` は 727→730、import 差し替えのみで内容は不変、合計 3,181 行）
- mat-controller lib テスト 476（うち dnssd 47）不変
- `cargo semver-checks -p mat-controller --baseline-rev 324fb10 --default-features`:
  ```
  Checked [   0.018s] 223 checks: 223 pass, 31 skip
  Summary no semver update required
  Finished [  15.123s] mat-controller
  ```
- 公開項目名一覧（`^pub (async fn|fn|struct|enum|const) `）: 分割前後で `diff` = 差分なし（`SAME`、11 件）
- `task check`（fmt:check + clippy -D warnings + test）: 緑（476 lib tests 含め全 green、doc-tests 含む）
