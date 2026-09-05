# mat-device `serve_secured_message` マイルストーン抽出 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (2 tasks — メインループで直接実行、subagent 不要). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `crates/mat-device/src/net/runtime.rs` の 260 行 `serve_secured_message()`（`run()` 分割後、mat-device で最長の非テスト関数）から、返信送信後の 4 つの「コミッショニング・マイルストーン反応」ブロック（AddNOC の operational advert / ECM window 再整合 / CommissioningComplete の window close / RemoveFabric の advert 撤去 + セッション破棄判定）を **挙動不変・機械的** に自由関数へ抽出し、ループ本体を「dispatch → reply → 4 マイルストーン呼び出し → 次メッセージ」の骨格だけにする。

**Architecture:** 各ブロックは `comm_server` / `mdns` / `window` / `config` と、そのブロック固有の入力（`fabrics_before` / `resp_opcode` + `req_cluster_command` + `resp_payload` / セッションの `fabric_index`）しか触らない。それぞれを private `async fn` に切り出し、ループ内は 4 行の呼び出しにする。`RemoveFabric` ブロックだけが `return ServeOutcome::DropSession` を持つので、その関数は `ServeOutcome` を返し、呼び出し側で `DropSession` なら `return`。`ServeState` / `serve_secured` / `serve_secured_message` のシグネチャ・テストは不変。

**Spec:** ユーザー指示（2026-09-05）「余力があれば mat-device 内の同型の長関数を洗って同じ扱い」— `run()` 分割（`2026-09-05-matv-runtime-run-split.md`）の続き。

## Global Constraints

- **挙動不変**: 4 ブロックの処理順序・条件・ログ文言（`tracing::*` のメッセージ/フィールド）を変えない。ブロック本体（コメント含む）はそのまま移し、`**window` → `*window`、`window`（`&mut &mut`）→ `window`（`&mut`）のような **借用形の付け替えだけ** を行う。
- **編集範囲**: `crates/mat-device/src/net/runtime.rs` のみ（+ 本計画ファイル）。他クレートは絶対に編集しない。
- **不変**: `serve_secured_message` / `serve_secured` / `drain_buffered_requests` / `ServeState` / `ServeOutcome` / `admin_window_action` / `apply_window_request` / `advert_params_for_window` / `remove_fabric_drops_session` / `operational_advert` のシグネチャ。新設関数はすべて private。
- **テスト不変**: mat-device + matv のテスト名多重集合 375 個。新規テスト不要（既存の `serve_secured_*` テスト + E2E m1 が AddNOC / CommissioningComplete / RemoveFabric の 3 経路を実走する。ECM 再整合は `admin_window_action` の単体テストが決定表を、本抽出は副作用の移動のみ）。
- **検証**: `cargo fmt --all` → `cargo clippy -p mat-device -p matv --all-targets -- -D warnings` → `cargo test -p mat-device -p matv` → テスト名比較 `SAME` → `tracing::` 行数の増減ゼロ → `task check` → `task e2e:device:m1` / `m3`。
- **git**: `/usr/bin/git` フルパス。コミット末尾に `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` と `Claude-Session: https://claude.ai/code/session_01DbRmGXELPVLTCHBym3RZaS`。

## File Structure

`runtime.rs` 内、`serve_secured_message` の直後（`ServeState` 定義の直前）に 4 関数を置く:

| 関数 | 元ブロック |
|---|---|
| `async fn advertise_added_fabric(comm_server: &CommissioningServer, mdns: Option<&MdnsCtx>, fabrics_before: usize)` | `// AddNOC success: ...` |
| `async fn reconcile_admin_window(comm_server: &CommissioningServer, mdns: Option<&MdnsCtx>, window: &mut CommissioningWindow, config: &DeviceConfig)` | `// ECM window reconciliation (Task 4) ...` |
| `async fn close_window_on_commissioning_complete(resp_opcode: u8, req_cluster_command: Option<(u32, u32)>, resp_payload: &[u8], comm_server: &CommissioningServer, mdns: Option<&MdnsCtx>, window: &mut CommissioningWindow)` | `// CommissioningComplete success: ...` |
| `async fn retire_removed_fabric(comm_server: &CommissioningServer, mdns: Option<&MdnsCtx>, session_fabric_index: u8) -> ServeOutcome` | `// RemoveFabric (Task 6): ...` |

---

### Task 1: 4 マイルストーン関数の抽出

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`（`serve_secured_message` のループ後半 = "AddNOC success" から "RemoveFabric" ブロックの終わりまで）

- [ ] **Step 1:** 4 ブロックをそのまま新関数へ移す。ループ内は次の 4 行 + `DropSession` 判定に置き換える:

```rust
        advertise_added_fabric(comm_server, mdns, fabrics_before).await;
        reconcile_admin_window(comm_server, mdns, window, config).await;
        close_window_on_commissioning_complete(
            resp_opcode,
            req_cluster_command,
            &resp_payload,
            comm_server,
            mdns,
            window,
        )
        .await;
        if retire_removed_fabric(comm_server, mdns, fabric_index).await == ServeOutcome::DropSession {
            return ServeOutcome::DropSession;
        }
```

各ブロック先頭の `//` コメントは、新関数の `///` doc コメントへ昇格させる（文面は据え置き。「above」「this block」等の相対語だけ実態に合わせる）。`window` は `serve_secured_message` 内では `&mut &mut CommissioningWindow`（destructure の結果）なので、呼び出しでは `window` をそのまま渡す（`&mut &mut T` → `&mut T` の再借用は自動）。関数内では `**window =` を `*window =` に、`admin_window_action(.., window)` / `advert_params_for_window(window, ..)` は `&*window` 相当（自動 deref）でそのまま通る。

- [ ] **Step 2:** 検証（fmt / clippy / test / `SAME` / tracing 増減ゼロ）。
- [ ] **Step 3:** Commit: `refactor(mat-device): serve_secured_message のコミッショニング・マイルストーン 4 ブロックを自由関数へ抽出（挙動不変）`

### Task 2: ゲート（`task check` + E2E m1 / m3）と実測記録

- [ ] **Step 1:** `task check`。
- [ ] **Step 2:** `task e2e:device:m1` → `task e2e:device:m3`（順に。並列不可）。
- [ ] **Step 3:** 本計画末尾に実測記録（`serve_secured_message` 行数 before/after、4 関数の行数、E2E 結果）を追記し、コミット: `docs(mat-device): serve_secured_message 抽出の実測記録`

## 実測記録（2026-09-05）

- `serve_secured_message`: 260 行 → 158 行。抽出 4 関数 = `advertise_added_fabric` 18 / `reconcile_admin_window` 37 / `close_window_on_commissioning_complete` 40 / `retire_removed_fabric` 22 行
- ログ文言不変（`tracing::` 行 2/2 移動のみ、フィールド名 `fabric_index` は `session_fabric_index` の値をそのまま出す）。テスト名多重集合 375 個不変。clippy `-D warnings` 0（`needless_borrow` 1 件は `&[u8]` 引数化に伴う `&resp_payload` → `resp_payload`）
- `task check` 合格。E2E m1 合格（commission success → unpair で `RemoveFabric removed the invoking session's own fabric — dropping session` ログ = `retire_removed_fabric` 経路の実走、read → exit 11）、E2E m3 合格（group provision / remove / re-provision / groupcast on-off=true / matd 常駐 Subscribe → `mat listen` イベント受信）
- 残り（対象外・次回候補）: `serve_subscribe_request` 160 行、`Device::new` 241 行（同期セットアップの直列、`device.rs`）、`core::commissioning::handle_add_noc` 138 行
