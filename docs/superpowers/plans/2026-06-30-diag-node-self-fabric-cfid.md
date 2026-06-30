# diag node `advertised_self_fabric` 実機対応 (#3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat diag node --deep` の `advertised_self_fabric` が実機で `true`/`false` を返すよう、自 fabric CFID の取得を頑健化し、取得不能時を可観測にする。

**Architecture:** 自 fabric の compressed fabric id (CFID) を、既に走らせている operational read の stderr から拾う。第1候補を operational discovery が解決したインスタンス名 `<CFID>-<NodeId>`（token スキャン、NodeId が対象ノードに一致するものの CFID 部）、第2候補を現行の `Compressed FabricId 0x...` 行とするフォールバック連鎖にする。両方失敗したら `advertised_self_fabric` を黙って `None` にせず `unavailable` に `cfid_unavailable` を積む。fake-chip-tool に `[DIS]` インスタンス名行を足してテストし、最後に jarvis の実機で確定・E2E 検証する。

**Tech Stack:** Rust, cargo workspace（`mat-core` = 純ロジック、`mat` = CLI/統合テスト）、Task runner、assert_cmd + predicates、シェル fixture（fake-chip-tool / fake-avahi-browse / fake-ping6）。

## Global Constraints

- stdout は純 JSON のみ。`chip-tool` 出力をそのまま流さない（CLAUDE.md ルール2）。
- 診断は stderr / `tracing`（ルール3）。chip-tool stderr は分類・パースに使うのみ。
- 状態を持たない（ルール4）。CFID はその場の operational read 出力から都度抽出。
- TLV/CASE/暗号を `mat` 内で話さない（ルール1）。CFID の自前 HKDF 導出はしない。
- `task check`（fmt:check + clippy `-D warnings` + test）がコミット前に通ること。
- mat-core の純パーサは副作用なし。chip-tool には触れない。
- 検証対象ノードは node 5（実機 jarvis）。台帳 address・CFID のサンプルは
  `192.0.2.10` / `00AABB1122CC3344`（RFC 5737 ダミー）。

---

### Task 1: `parse_operational_instance_cfid` 純関数（mat-core）

operational discovery のインスタンス名 `<CFID>-<NodeId>` を stderr 全体から token スキャンで拾い、NodeId が対象ノードに一致するものの CFID（大文字正規化）を返す純関数。

**Files:**
- Modify: `crates/mat-core/src/diag.rs`（`parse_compressed_fabric_id` の直後、189 行付近に追加）
- Test: `crates/mat-core/src/diag.rs`（末尾の `#[cfg(test)] mod tests` に追加）

**Interfaces:**
- Consumes: なし（`std` のみ）。
- Produces: `pub fn parse_operational_instance_cfid(stderr: &str, node_id: u64) -> Option<String>`
  — Task 2 が第1候補として呼ぶ。返値は 16 桁大文字 hex の CFID。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/diag.rs` の `#[cfg(test)] mod tests` 内に追加:

```rust
#[test]
fn operational_instance_cfid_matches_node() {
    let stderr = "[DIS] OperationalSessionSetup[1:0000000000000005]: resolved instance \
                  00AABB1122CC3344-0000000000000005._matter._tcp.local.\n";
    assert_eq!(
        parse_operational_instance_cfid(stderr, 5),
        Some("00AABB1122CC3344".to_string())
    );
}

#[test]
fn operational_instance_cfid_lowercase_is_normalized() {
    let stderr = "00aabb1122cc3344-0000000000000005._matter._tcp\n";
    assert_eq!(
        parse_operational_instance_cfid(stderr, 5),
        Some("00AABB1122CC3344".to_string())
    );
}

#[test]
fn operational_instance_cfid_ignores_other_node() {
    let stderr = "[DIS] ... 00AABB1122CC3344-0000000000000009._matter._tcp ...\n";
    assert_eq!(parse_operational_instance_cfid(stderr, 5), None);
}

#[test]
fn operational_instance_cfid_absent_returns_none() {
    // fabricIndex:nodeid（コロン区切り、16桁hexでない左辺）は誤マッチしない。
    let stderr = "[DIS] OperationalSessionSetup[1:0000000000000005]: looking up\n";
    assert_eq!(parse_operational_instance_cfid(stderr, 5), None);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-core operational_instance_cfid`
Expected: コンパイルエラー（`parse_operational_instance_cfid` 未定義）。

- [ ] **Step 3: 本体を実装**

`crates/mat-core/src/diag.rs` の `parse_compressed_fabric_id`（189 行付近）の直後に追加:

```rust
/// chip-tool の operational discovery ログ（`[DIS]` 行など）から、対象 `node_id` 向けに
/// 解決されたインスタンス名 `<CFID>-<NodeId>` を探して自 fabric の compressed id を返す。
///
/// stderr 全体を走査し、空白 / `;` / `,` 区切りの各 token の先頭（`.` より前）が
/// `<16hex>-<16hex>` 形で、後半（NodeId）が `node_id` に一致するものの前半（CFID、
/// 大文字正規化）を返す。複数あれば最初の一致。無ければ `None`。
/// 第1候補として使う理由: operational read 自体が必ず通る解決経路のログで、
/// fabric init の `Compressed FabricId` 行より出やすい。
pub fn parse_operational_instance_cfid(stderr: &str, node_id: u64) -> Option<String> {
    for line in stderr.lines() {
        for tok in line.split(|c: char| c.is_whitespace() || c == ';' || c == ',') {
            let head = tok.split('.').next().unwrap_or(tok);
            if let Some((fab, node)) = head.split_once('-') {
                let fab_ok = fab.len() == 16 && fab.bytes().all(|b| b.is_ascii_hexdigit());
                let node_ok = node.len() == 16 && node.bytes().all(|b| b.is_ascii_hexdigit());
                if fab_ok && node_ok {
                    if let Ok(n) = u64::from_str_radix(node, 16) {
                        if n == node_id {
                            return Some(fab.to_ascii_uppercase());
                        }
                    }
                }
            }
        }
    }
    None
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core operational_instance_cfid`
Expected: 4 件 PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-core/src/diag.rs
git commit -m "feat(diag): operational instance 名から自 fabric CFID を拾う純関数を追加(#3)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: 自 fabric CFID 取得をフォールバック連鎖に差し替え（mat）

`node()` の CFID 取得を「第1候補=instance 名 → 第2候補=Compressed FabricId 行」に変更。

**Files:**
- Modify: `crates/mat/src/commands/diag.rs`（import 行 25-28、CFID 取得 156-159 行）

**Interfaces:**
- Consumes: `mat_core::diag::parse_operational_instance_cfid`（Task 1）、既存
  `parse_compressed_fabric_id`。
- Produces: `self_cfid: Option<String>`（既存変数）の取得元を変更。Task 3 が同変数を使う。

- [ ] **Step 1: import に新関数を追加**

`crates/mat/src/commands/diag.rs` の `use mat_core::diag::{ ... };`（25-28 行）に
`parse_operational_instance_cfid` を追加:

```rust
use mat_core::diag::{
    derive_verdict, parse_compressed_fabric_id, parse_operational_instance_cfid, parse_ping6,
    Checks, IpCheck, MdnsCheck, OperationalCheck, ThreadCheck,
};
```

- [ ] **Step 2: CFID 取得をフォールバック連鎖へ**

156-159 行の以下を:

```rust
    if let Some(cfid) = parse_compressed_fabric_id(&op_out.stderr) {
        self_cfid = Some(cfid);
    }
```

次に置き換える:

```rust
    // 自 fabric CFID: 第1候補 = operational discovery のインスタンス名 `<CFID>-<NodeId>`、
    // 第2候補 = `Compressed FabricId 0x...` 行。どちらも op read の stderr から拾う。
    self_cfid = parse_operational_instance_cfid(&op_out.stderr, node_id)
        .or_else(|| parse_compressed_fabric_id(&op_out.stderr));
```

- [ ] **Step 3: ビルドと既存統合テストで回帰確認**

Run: `cargo test -p mat --test integration diag_node`
Expected: 既存の `diag_node_*` が全て PASS（fake-chip-tool は `[FP] Compressed FabricId`
行を出すので第2候補で従来どおり `00AABB1122CC3344` を取得、挙動不変）。

Run: `cargo clippy -p mat -- -D warnings`
Expected: 警告ゼロ（未使用 import が無いこと）。

- [ ] **Step 4: コミット**

```bash
git add crates/mat/src/commands/diag.rs
git commit -m "feat(diag): 自 fabric CFID 取得をインスタンス名優先のフォールバック連鎖に(#3)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: CFID 取得不能時を `unavailable` に可観測化（mat）

`self_cfid` が `None` のとき、`advertised_self_fabric` を黙って省略するだけでなく
`unavailable` に理由を積む。

**Files:**
- Modify: `crates/mat/src/commands/diag.rs`（`deep_probes` の mdns Ok ブロック、266-282 行付近）

**Interfaces:**
- Consumes: `self_cfid: Option<String>`（Task 2 で設定）。`deep_probes` は既に
  引数で受け取っている（`diag.rs:235`）。
- Produces: `unavailable` に `{"check":"mdns_self_fabric","kind":"cfid_unavailable","detail":...}`
  を追加。検証は Task 4 の統合テストで行う。

- [ ] **Step 1: 取得不能時の push を追加**

`crates/mat/src/commands/diag.rs` の `deep_probes` 内、`advertised_self_fabric` を
計算している `Ok(instances) => { ... }` ブロック（266-276 行付近）で、`checks.mdns = ...`
の**直前**に以下を挿入:

```rust
            if self_cfid.is_none() {
                unavailable.push(json!({
                    "check": "mdns_self_fabric",
                    "kind": "cfid_unavailable",
                    "detail": "could not obtain self compressed-fabric-id from chip-tool operational logs"
                }));
            }
```

挿入後のブロックは概略:

```rust
        Ok(instances) => {
            let addr = address.as_deref();
            let advertised_any_fabric = match addr { /* 既存のまま */ };
            let advertised_self_fabric =
                match (self_cfid.as_ref(), addr) { /* 既存のまま */ };
            if self_cfid.is_none() {
                unavailable.push(json!({
                    "check": "mdns_self_fabric",
                    "kind": "cfid_unavailable",
                    "detail": "could not obtain self compressed-fabric-id from chip-tool operational logs"
                }));
            }
            checks.mdns = Some(MdnsCheck {
                advertised_self_fabric,
                advertised_any_fabric,
            });
        }
```

- [ ] **Step 2: ビルドと既存テストで回帰確認**

Run: `cargo test -p mat --test integration diag_node`
Expected: 既存 `diag_node_*` PASS（既存テストは全て `self_cfid` が Some なので
新 push は発火せず挙動不変）。

- [ ] **Step 3: コミット**

```bash
git add crates/mat/src/commands/diag.rs
git commit -m "feat(diag): 自 fabric CFID 取得不能を unavailable に可観測化(#3)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: fake-chip-tool に `[DIS]` インスタンス名行を追加し統合テスト

第1候補（instance 名）経路と `cfid_unavailable` 経路を fake で再現してテストする。

**Files:**
- Modify: `crates/mat/tests/fixtures/fake-chip-tool.sh`（18 行付近の CFID ダミー出力）
- Test: `crates/mat/tests/integration.rs`（diag node 節、712 行付近に追加）

**Interfaces:**
- Consumes: fake-chip-tool の stderr、`MAT_AVAHI_BROWSE_BIN`=fake-avahi、`FAKE_AVAHI_*`。
- Produces: 環境変数 `FAKE_CHIP_NO_DIS_CFID` / `FAKE_CHIP_NO_CFID` で CFID 行の出力を制御。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs` の `diag_node_deep_missing_probe_binary`（712 行）の
直後に追加:

```rust
#[test]
fn diag_node_deep_self_fabric_via_instance_name() {
    // fake-chip-tool は [DIS] インスタンス名 00AABB1122CC3344-0000000000000005 を出す。
    // avahi も同 CFID・192.0.2.10 で広告 → advertised_self_fabric=true。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .env("MAT_PING6_BIN", fake_ping6())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .env("FAKE_AVAHI_ADDR", "192.0.2.10")
        .env("FAKE_AVAHI_FABRIC", "00AABB1122CC3344")
        .args(["diag", "node", "--node", "5", "--deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"advertised_self_fabric\":true"));
}

#[test]
fn diag_node_deep_cfid_unavailable_when_no_cfid_logs() {
    // CFID 行を両方とも抑止 → self_cfid 取得不能。advertised_any_fabric は出るが
    // advertised_self_fabric は省略、unavailable に cfid_unavailable が出る。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .env("FAKE_CHIP_NO_CFID", "1")
        .env("MAT_PING6_BIN", fake_ping6())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .env("FAKE_AVAHI_ADDR", "192.0.2.10")
        .env("FAKE_AVAHI_FABRIC", "0011223344556677")
        .args(["diag", "node", "--node", "5", "--deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cfid_unavailable\""))
        .stdout(predicate::str::contains("\"advertised_any_fabric\":true"))
        .stdout(predicate::str::contains("\"advertised_self_fabric\"").not());
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat --test integration diag_node_deep_self_fabric_via_instance_name diag_node_deep_cfid_unavailable_when_no_cfid_logs`
Expected: 両方 FAIL（fake が `[DIS]` 行を出さず、`FAKE_CHIP_NO_CFID` も未対応）。

- [ ] **Step 3: fake-chip-tool に `[DIS]` 行と抑止スイッチを実装**

`crates/mat/tests/fixtures/fake-chip-tool.sh` の 17-18 行:

```sh
# parse_compressed_fabric_id が自 fabric CFID を拾えるようにダミー行を出す。
echo "[FP] Compressed FabricId 0x00AABB1122CC3344, FabricId 0x1" >&2
```

を次に置き換える:

```sh
# 自 fabric CFID のダミー出力。第1候補 = operational discovery のインスタンス名
# `<CFID>-<NodeId>`、第2候補 = `Compressed FabricId 0x...` 行。テストで個別に抑止可能。
#   FAKE_CHIP_NO_DIS_CFID=1 → インスタンス名行のみ抑止（第2候補の回帰テスト用）
#   FAKE_CHIP_NO_CFID=1     → 両方抑止（cfid_unavailable のテスト用）
if [ -z "$FAKE_CHIP_NO_CFID" ]; then
  if [ -z "$FAKE_CHIP_NO_DIS_CFID" ]; then
    echo "[DIS] OperationalSessionSetup[1:0000000000000005]: resolved instance 00AABB1122CC3344-0000000000000005._matter._tcp.local." >&2
  fi
  echo "[FP] Compressed FabricId 0x00AABB1122CC3344, FabricId 0x1" >&2
fi
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat --test integration diag_node`
Expected: 新規 2 件を含む `diag_node_*` が全て PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/mat/tests/fixtures/fake-chip-tool.sh crates/mat/tests/integration.rs
git commit -m "test(diag): instance 名経由の self_fabric と cfid_unavailable を統合テスト(#3)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: 実機 jarvis での経験的確定と E2E 検証

実 chip-tool が既定 verbosity の stderr にどの CFID シグナルを出すかを実測し、パーサ
（特に fake の `[DIS]` 行の形）が実機と一致することを確認。最後に `mat diag node --deep`
で `advertised_self_fabric` が `true`/`false` で返ることを E2E 確認する。

**Files:**
- 採取ログに応じて Modify（必要時のみ）: `crates/mat-core/src/diag.rs`（token 抽出条件）、
  `crates/mat/tests/fixtures/fake-chip-tool.sh`（`[DIS]` 行の形を実機に合わせる）。

**Interfaces:**
- Consumes: jarvis 実機（node 5）、`MAT_CHIP_TOOL_BIN`、ssh アクセス。
- Produces: 採用したシグナルと（必要なら）verbosity 手段の確定記録。

- [ ] **Step 1: 実機で operational read の生 stderr を採取**

jarvis 上で one-shot 実行（warm セッションを避ける）。`<bin>` は実機の chip-tool パス:

```bash
ssh jarvis 'MAT_CHIP_TOOL_BIN=<bin> mat diag node --node 5 --deep 2>/tmp/diag5.err; \
            grep -nE "Compressed FabricId|-0{15}5\.|_matter\._tcp|[0-9A-Fa-f]{16}-[0-9A-Fa-f]{16}" /tmp/diag5.err'
```

Expected: CFID を載せた行を特定する。判断:
- インスタンス名 `<CFID>-0000000000000005`（`_matter._tcp` 近傍）が出る → 第1候補で取れる。
- `Compressed FabricId 0x...` が出る → 第2候補で取れる。
- **どちらも出ない** → Step 3 の verbosity 対応へ。

- [ ] **Step 2: fake の `[DIS]` 行を実機の形に合わせる（必要時）**

Step 1 で採取した実際のインスタンス名行の前後形式が Task 4 の fake 行
（`OperationalSessionSetup[...]: resolved instance <CFID>-<NodeId>._matter._tcp.local.`）と
著しく異なる場合のみ、`fake-chip-tool.sh` の `[DIS]` 行をその実形へ書き換え、
`cargo test -p mat --test integration diag_node` が PASS することを確認。

パーサ自体は token スキャン（`<16hex>-<16hex>`）なので前後テキストには依存しない。
token が出てさえいれば修正不要。

- [ ] **Step 3: 既定で CFID が一切出ない場合のみ verbosity 対応（条件付き）**

Step 1 で CFID シグナルが既定 verbosity に皆無だった場合のみ実施。実機で出力を増やす
手段（環境変数 / `--trace-to json:log` 等）を1つ確定し、`node()` の operational read
実行時に適用する。出力増加手段が確定するまでこのステップを完了扱いにしない。
皆無でなければ本ステップは「不要」と記録してスキップ（YAGNI）。

- [ ] **Step 4: E2E 検証**

```bash
ssh jarvis 'MAT_CHIP_TOOL_BIN=<bin> mat diag node --node 5 --deep' | \
  grep -E '"advertised_self_fabric":(true|false)'
```

Expected: `advertised_self_fabric` が `true` または `false` で出る（`None` 省略でない）。
health なノードかつ自 fabric 広告ありなら `true`。

- [ ] **Step 5: コミット（Step 2/3 で変更があった場合のみ）**

```bash
git add -A
git commit -m "fix(diag): self_fabric CFID シグナルを実機形に整合(#3)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

変更が無ければ（パーサが実機でそのまま機能）コミット不要。E2E 結果を Task 6 の
コミット/Issue クローズコメントに残す。

---

### Task 6: README 追記・CI チェック・Issue クローズ

**Files:**
- Modify: `README.md`（`diag node` の説明節）

**Interfaces:**
- Consumes: なし。
- Produces: ドキュメントと最終チェック。

- [ ] **Step 1: README に `advertised_self_fabric` / `cfid_unavailable` を説明**

`README.md` の `diag node` を説明している節に、`mdns.advertised_self_fabric` が自 fabric
向け広告の有無、取得元が operational read のログ（インスタンス名優先）であること、
取得不能時は `unavailable` に `cfid_unavailable` が出ることを 2-3 文で追記する。
既存の `diag node` 説明の文体・JSON 例の体裁に合わせる。

- [ ] **Step 2: CI 相当チェック**

Run: `task check`
Expected: fmt:check + clippy(`-D warnings`) + test 全て PASS。

- [ ] **Step 3: コミット**

```bash
git add README.md
git commit -m "docs(readme): diag node の advertised_self_fabric と cfid_unavailable を説明(#3)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

- [ ] **Step 4: Issue #3 をクローズ**

実機 E2E（Task 5 Step 4）で `advertised_self_fabric` が `true`/`false` を返したことと、
`cfid_unavailable` フォールバックがテスト済みであることを添えてクローズ:

```bash
gh issue close 3 --comment "対応済み。operational read のログから自 fabric CFID を
インスタンス名優先のフォールバック連鎖で取得するよう変更。取得不能時は unavailable に
cfid_unavailable を積み可観測化（テスト済み）。実機 jarvis/node 5 の diag node --deep で
advertised_self_fabric が <true/false> を返すことを確認。"
```

---

## Self-Review

**Spec coverage:**
- 設計「1. 複数シグナルのフォールバック連鎖」→ Task 1（新パーサ）+ Task 2（連鎖配線）。
- 設計「2. 取得不能時の可観測化」→ Task 3 + Task 4（テスト）。
- 設計「3. 実機での経験的確定」→ Task 5。
- テスト（fake に `[DIS]` 行追加 / 単体 / 両欠落→cfid_unavailable / 実機 E2E）→ Task 1・4・5 で網羅。
- 受け入れ条件「実機で true/false」→ Task 5 Step 4。「取得不能フォールバックがテスト担保」→ Task 4 Step 1 の 2 本目。
- スコープ外（HKDF 自前導出 / verdict 変更 / matd 経路）→ どのタスクでも触れていない。OK。

**Placeholder scan:** TBD/TODO なし。各コード step に実コードあり。Task 5 の `<bin>` は
実機固有パスのため記号のまま（実行者が埋める実値、プレースホルダではない）。

**Type consistency:** `parse_operational_instance_cfid(stderr: &str, node_id: u64) -> Option<String>`
を Task 1 で定義し Task 2 で同シグネチャ呼び出し。`self_cfid: Option<String>` は既存変数で
Task 2/3 が一貫使用。`unavailable` の key（`check`/`kind`/`detail`）は Task 3 と Task 4 の
アサート（`cfid_unavailable`）で一致。
