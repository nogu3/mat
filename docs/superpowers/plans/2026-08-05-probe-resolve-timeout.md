# probe resolve 窓の CASE 経路統一（監査⑩）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat discover --probe` / `diag node --deep` の resolve 窓（独自 3s）を CASE establish 経路と同一の共有定数（8s）に統一し、健全ノードの `reachable:false` 誤報を根絶する。

**Architecture:** `mat-controller::dnssd` に `OPERATIONAL_RESOLVE_TIMEOUT`（8s）を新設し、`mat-native`（establish）と `crates/mat`（probe）の両方がそれを参照する。値の乖離をコンパイル時に不可能にする構造的ピン。ロジック変更なし。

**Tech Stack:** Rust workspace（crates: mat-controller / mat-native / mat）、Task ランナー（`task check`）、jarvis 実機 E2E（aarch64 クロスビルド）。

**Spec:** `docs/superpowers/specs/2026-08-05-probe-resolve-timeout-design.md`

## Global Constraints

- 対象バージョン: **1.21.0**（`Cargo.toml` workspace.package.version、現在 1.20.0）。
- `resolve_operational` 本体（`QUERY_RESEND_INTERVAL = 1s` 含む）は無変更。
- matd の `CachingResolver` / `CACHE_MISS_TIMEOUT` は無変更（窓分離は既存設計の意図）。
- `commissioning.rs:1054` の resolve（5s×12 リトライ）は別規律でスコープ外・無変更。
- エラー分類・出力スキーマ（`reachable` true/false/null）・exit code は無変更。
- 新規ユニットテストは追加しない（値 assert は tautology — ピンは共有定数参照そのもの。spec で決定済み）。
- コミット前に `task check`（fmt:check + clippy + test）必須。
- コミット対象はこのセッションで編集したファイルのみ（セッション開始時から modified の `CLAUDE.md` は含めない）。
- main マージは jarvis 実機 E2E 合格後のみ（Task 3 → Task 4 の順序厳守）。

---

### Task 1: 共有定数の新設と参照置換

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`（`QUERY_RESEND_INTERVAL` 定義（現 63 行目付近）の直後に定数追加）
- Modify: `crates/mat-native/src/lib.rs:224-225`（`RESOLVE_TIMEOUT` の定義を参照に置換）
- Modify: `crates/mat/src/probe.rs`（`PROBE_RESOLVE_TIMEOUT` 削除、共有定数使用、doc 追記）

**Interfaces:**
- Produces: `pub const OPERATIONAL_RESOLVE_TIMEOUT: Duration = Duration::from_secs(8)`（`mat_controller::dnssd`）— Task 3 の E2E が挙動（probe 窓 8s）として検証する。

- [ ] **Step 1: 作業ブランチを切る**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat
git checkout -b fix/tier2-probe-resolve-timeout main
```

- [ ] **Step 2: `dnssd.rs` に共有定数を追加**

`crates/mat-controller/src/dnssd.rs` の `const QUERY_RESEND_INTERVAL: Duration = Duration::from_secs(1);`（63 行目付近）の直後に追加:

```rust
/// CASE 前 targeted resolve（[`resolve_operational`]）の共通窓。establish
/// （mat-native）と probe（mat の `discover --probe` / `diag node --deep`）が
/// 共有し、「CASE が届く範囲 = probe が reachable と言う範囲」を定義上
/// 一致させる。監査⑩: probe 独自の 3s 窓が establish の 8s と乖離し、
/// Thread メッシュ + advertising proxy 経由で resolve に 3〜8 秒かかる
/// 健全ノードを `reachable:false` と誤報していた。
pub const OPERATIONAL_RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);
```

- [ ] **Step 3: `mat-native/src/lib.rs` のローカル定数を参照に置換**

`crates/mat-native/src/lib.rs:224-225` の

```rust
/// mDNS 解決 timeout。SII が来ない場合でも過度に待たない上限。
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);
```

を次に置換（値 8s のまま = establish の挙動不変）:

```rust
/// mDNS 解決 timeout（`dnssd::OPERATIONAL_RESOLVE_TIMEOUT` の別名 — probe と
/// 共有、監査⑩）。SII が来ない場合でも過度に待たない上限。
const RESOLVE_TIMEOUT: Duration = dnssd::OPERATIONAL_RESOLVE_TIMEOUT;
```

- [ ] **Step 4: `probe.rs` の独自 3s 窓を共有定数に置換**

`crates/mat/src/probe.rs` で 3 箇所:

(a) 33-35 行目の定数定義

```rust
/// 1 ノードあたりの resolve タイムアウト。全ノード並行実行のため、
/// 台帳が何ノードあっても総所要時間はおよそこの値に収まる。
const PROBE_RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
```

を削除し、97 行目付近の呼び出しを

```rust
                let res =
                    dnssd::resolve_operational(scope_id, &cfid, node_id, PROBE_RESOLVE_TIMEOUT)
                        .await;
```

から

```rust
                let res = dnssd::resolve_operational(
                    scope_id,
                    &cfid,
                    node_id,
                    dnssd::OPERATIONAL_RESOLVE_TIMEOUT,
                )
                .await;
```

に置換。

(b) 定数削除で `use std::time::Duration;`（17 行目）が未使用になるので削除（clippy/rustc の unused import で検出される）。

(c) モジュール doc（ファイル先頭 `//!` ブロック）の末尾に追記:

```rust
//!
//! resolve 窓は establish（mat-native）と同じ
//! `dnssd::OPERATIONAL_RESOLVE_TIMEOUT`（8s）を共有する（監査⑩、1.21.0）。
//! かつて probe 独自の 3s 窓を持っており、resolve に 3〜8 秒かかる健全
//! ノードを「CASE なら届くのに reachable:false」と誤報していた。全ノード
//! 並行実行のため、台帳が何ノードあっても総所要時間はおよそ窓 1 つ分。
```

- [ ] **Step 5: `task check` で全テスト・lint 通過を確認**

```bash
task check
```

Expected: fmt:check / clippy / test すべて成功（既存テストは 3s に依存していないため無修正で通る）。

- [ ] **Step 6: コミット**

```bash
git add crates/mat-controller/src/dnssd.rs crates/mat-native/src/lib.rs crates/mat/src/probe.rs
git commit -m "fix(mat): probe の resolve 窓を CASE 経路と共有定数 8s に統一（監査⑩）"
```

---

### Task 2: バージョン 1.21.0

**Files:**
- Modify: `Cargo.toml`（workspace.package.version）
- Modify: `Cargo.lock`（ビルドで自動更新）

**Interfaces:**
- Produces: `mat --version` = 1.21.0（Task 3 の E2E がバイナリ同定に使う）。

- [ ] **Step 1: バージョンを上げる**

`Cargo.toml` の

```toml
version = "1.20.0"
```

を

```toml
version = "1.21.0"
```

に変更。

- [ ] **Step 2: Cargo.lock を追随させ、ビルド確認**

```bash
cargo check --workspace
```

Expected: 成功（`Cargo.lock` の mat 系 crate バージョンが 1.21.0 に更新される）。

- [ ] **Step 3: コミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.21.0（probe resolve 窓の CASE 経路統一 — 安定性監査 Tier 2 ⑩）"
```

---

### Task 3: jarvis 実機 E2E（マージ前必須）

**Files:** なし（検証のみ。リポジトリ変更はない）

**Interfaces:**
- Consumes: Task 1 の挙動（probe 窓 8s）、Task 2 のバージョン表示（1.21.0）。
- Produces: E2E 合否（合格が Task 4 マージの前提条件）。

前提知識（メモリ由来）:
- jarvis = aarch64 実機。ssh ホスト名 `jarvis`、バイナリは `~/.local/bin/mat`（本番 1.20.0）。
- 非対話 ssh の直経路は `MAT_FABRIC_INDEX=2` 前置き必須（無いと f/1 参照で store_missing）。
- `discover --probe` は読み取り専用（mDNS resolve のみ、CASE 確立なし・KVS 書込なし）なので本番 matd の購読を乱さない。隔離 matd 不要の低リスク E2E。
- 台帳は 15 ノード。node15 は慢性の mDNS 未解決（`reachable:false` が正しい）。他ノードの期待値は直前の `matd status` の established 集合と照合する。

- [ ] **Step 1: aarch64 バイナリをビルド**

```bash
task dist:arm64
```

Expected: `dist/arm64/mat`（aarch64-unknown-linux-gnu）生成。

- [ ] **Step 2: jarvis へ `.new` として転送（本番は未置換のまま）**

```bash
scp dist/arm64/mat jarvis:~/.local/bin/mat.new
ssh jarvis 'chmod +x ~/.local/bin/mat.new && ~/.local/bin/mat.new --version'
```

Expected: `1.21.0` が表示される。

- [ ] **Step 3: 現況ベースラインを取る**

```bash
ssh jarvis 'matd status' 2>/dev/null | head -50
ssh jarvis 'time MAT_FABRIC_INDEX=2 ~/.local/bin/mat discover --probe' > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/probe-old.json 2> /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/probe-old.time
```

Expected: 本番 1.20.0（3s 窓）の `reachable` 集合と所要時間を記録。`matd status` の established 集合を突き合わせ材料にする。

- [ ] **Step 4: 新バイナリで probe を実行**

```bash
ssh jarvis 'time MAT_FABRIC_INDEX=2 ~/.local/bin/mat.new discover --probe' > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/probe-new.json 2> /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/probe-new.time
```

- [ ] **Step 5: 合否判定**

判定基準（すべて満たすこと）:
1. 新バイナリの `reachable:true` 集合 ⊇ 旧バイナリの `reachable:true` 集合（窓拡大で減ることはない）。
2. `matd status` established のノードが新バイナリでほぼ全て `reachable:true`（node15 等の既知未解決は `false` で正しい）。
3. 全ノード健全なら所要は旧と同等、`false` ノードが残る場合は 8 秒強で頭打ち（3 秒台で切り上げていないこと = 新窓が効いている証拠）。
4. stderr に想定外の WARN/ERROR なし。
5. JSON スキーマ不変（`timestamp` / `reachable` / `address` フィールド構成が旧と同形）。

判定できない事態（例: SRP 障害で広告が広範に消えている）が起きたら中断してユーザーに報告する（[[otbr-srp-lockout-anycast-recovery]] の再発時は E2E 自体が無意味）。

- [ ] **Step 6: 後始末（`.new` は残置）**

`mat.new` は昇格待ちとしてそのまま残す（本番置換は別途デプロイ判断）。scratchpad の記録ファイルは残してよい。

---

### Task 4: main マージ + push（E2E 合格後のみ）

**Files:** なし（git 操作のみ）

**Interfaces:**
- Consumes: Task 3 の合格判定。

- [ ] **Step 1: main へ no-ff マージ**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat
git checkout main
git merge --no-ff fix/tier2-probe-resolve-timeout -m "Merge fix/tier2-probe-resolve-timeout: 1.21.0 — probe resolve 窓の CASE 経路統一（安定性監査 Tier 2 ⑩）"
```

- [ ] **Step 2: push（HTTPS 経由 — ssh 鍵は 1Password agent が拒否することがある）**

```bash
git push https://github.com/nogu3/mat.git main
git fetch https://github.com/nogu3/mat.git main && git update-ref refs/remotes/origin/main FETCH_HEAD
```

Expected: origin/main が新 merge commit と一致。

- [ ] **Step 3: ブランチ削除**

```bash
git branch -d fix/tier2-probe-resolve-timeout
```
