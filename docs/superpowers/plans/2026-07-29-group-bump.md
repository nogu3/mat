# `mat group bump` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** group 送信 counter を「matd 再起動 1 回相当」前方ジャンプさせる応急コマンド `mat group bump` を、matd op + 直経路の両路で実装する（Issue #14 帰結、spec: `docs/superpowers/specs/2026-07-29-group-counter-bump-design.md`）。

**Architecture:** ジャンプの実体は `mat-controller::group::PersistedGroupCounter::jump()`（`load()` と同じ persist-ahead 算術）。`mat-native::group::bump(ctx)` が send() と同じ lazy 初期化を共有して matd / 直経路の両方から呼ばれる。matd socket プロトコルに `{"op":"group_bump"}` を追加し、成功 body は `mat-core::body::group_bump(from, to)` で両経路同形を構造的に保証する。

**Tech Stack:** Rust workspace（crates: mat-controller / mat-native / mat-core / matd / mat）、tokio、serde_json、clap。

## Global Constraints

- stdout は純粋な構造化 JSON のみ（成功 body に `timestamp` は含めない — 直経路は `output::emit`、matd は応答 envelope が付与する。`crates/mat-core/src/body.rs` 冒頭コメント参照）
- エラーは `{"error":{"kind","detail"}}`。counter 初期化不能・KVS 不備・persist 失敗は `Unavailable → store_parse`（exit 10）に写像（既存 `group::send` と同じ規律）
- `Op` の accessor（`name`/`node_id`/`group_id`/`endpoint`/`log_path`）は手書き網羅 match — variant 追加時にコンパイラが漏れを強制する設計。`_ =>` で潰さないこと
- 各タスク完了時に `task check`（fmt:check + clippy -D warnings + test）を通してから commit
- コミットメッセージは日本語・Conventional Commits、本文末尾に Co-Authored-By / Claude-Session トレーラ

---

### Task 1: `PersistedGroupCounter::jump()` + `GroupSender::bump_counter()`

**Files:**
- Modify: `crates/mat-controller/src/group.rs`（`impl PersistedGroupCounter` は 48–114 行、`impl GroupSender` は 193 行〜、`mod tests` は 261 行〜）

**Interfaces:**
- Consumes: 既存 `PersistedGroupCounter { next, ceiling, path, _lock }`、`persist(ceiling)`、`COUNTER_EPOCH = 4096`
- Produces: `PersistedGroupCounter::jump(&mut self) -> io::Result<(u32, u32)>`（返り値 = (ジャンプ前の次回送信値 from, ジャンプ後の次回送信値 to)）、`GroupSender::bump_counter(&mut self) -> std::io::Result<(u32, u32)>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/group.rs` の `mod tests` に追加（既存の `tmp_counter_path` ヘルパを使う）:

```rust
#[test]
fn counter_jump_advances_one_restart_window_and_persists_ahead() {
    let p = tmp_counter_path("jump");
    let _ = std::fs::remove_file(&p);
    let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
    let first = c.next().unwrap(); // = 4096 (EPOCH)。ceiling = first + EPOCH
    let (from, to) = c.jump().unwrap();
    // from = ジャンプしなかった場合の次回値、to = 旧 ceiling + EPOCH。
    assert_eq!(from, first + 1);
    assert_eq!(to, first + 2 * COUNTER_EPOCH);
    // ジャンプ後の払い出しは to から連番。
    assert_eq!(c.next().unwrap(), to);
    assert_eq!(c.next().unwrap(), to + 1);
    // persist-ahead 不変条件: drop → reload しても値が重ならない。
    drop(c);
    let mut c2 = PersistedGroupCounter::load(&p, 0).unwrap();
    assert!(c2.next().unwrap() > to + 1);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn counter_jump_twice_is_monotonic() {
    let p = tmp_counter_path("jump2");
    let _ = std::fs::remove_file(&p);
    let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
    let (_, to1) = c.jump().unwrap();
    let (from2, to2) = c.jump().unwrap();
    // 2 回目の from は 1 回目の to（間で next() していないので）。
    assert_eq!(from2, to1);
    assert!(to2 > to1);
    let _ = std::fs::remove_file(&p);
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller counter_jump`
Expected: FAIL（`jump` メソッド未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

`impl PersistedGroupCounter`（`next()` の直後）に追加:

```rust
/// counter 窓を「再起動 1 回相当」前方へジャンプする（Issue #14 応急処置）。
/// 次回払い出し値を `ceiling + EPOCH`（= `load()` と同じ算術）へ進め、
/// 新 ceiling を先に永続化してから返す。persist 失敗時は状態を変えない
/// （旧 ceiling のまま — 送信継続は安全）。
/// 返り値は (ジャンプ前の次回値 from, ジャンプ後の次回値 to)。
pub fn jump(&mut self) -> io::Result<(u32, u32)> {
    let from = self.next;
    let to = self.ceiling.wrapping_add(COUNTER_EPOCH);
    self.persist(to.wrapping_add(COUNTER_EPOCH))?;
    self.next = to;
    Ok((from, to))
}
```

`impl GroupSender`（`send_invoke` の後）に追加:

```rust
/// 内包 counter の窓ジャンプ（Issue #14 応急処置）。matd 常駐中は counter の
/// 実体（in-memory + flock）が GroupSender 内にあるため、ここを経由する。
pub fn bump_counter(&mut self) -> std::io::Result<(u32, u32)> {
    self.counter.jump()
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller group`
Expected: 新 2 件 PASS + 既存 group テスト全 PASS

- [ ] **Step 5: `task check` → commit**

```bash
task check
git add crates/mat-controller/src/group.rs
git commit -m "feat(controller): PersistedGroupCounter::jump — 再起動1回相当の counter 窓ジャンプ（Issue #14）"
```

---

### Task 2: `mat-native::group::bump()` — lazy 初期化を send() と共有

**Files:**
- Modify: `crates/mat-native/src/group.rs`（`send()` は 42–104 行、`mod tests` は 119 行〜）

**Interfaces:**
- Consumes: Task 1 の `GroupSender::bump_counter()`、既存 `GroupCtx`（`sender: Mutex<Option<GroupSender>>`）、`kvs::read_group_data_counter`、`PersistedGroupCounter::load`
- Produces:
  - `pub enum BumpOutcome { Bumped { from: u32, to: u32 }, Unavailable(String) }`
  - `pub async fn bump(ctx: &GroupCtx) -> BumpOutcome`（Err なし — socket 送出が無いため。counter/KVS 系の失敗はすべて `Unavailable`）
  - リファクタ: `fn init_sender(ctx: &GroupCtx, slot: &mut Option<GroupSender>) -> Result<(), String>`（send/bump 共有の lazy 初期化。Err = Unavailable 理由文字列）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/group.rs` の `mod tests` に追加（既存 `write_group_fixture_ini` を使う。multicast 送信をしないので iface 探索は不要）:

```rust
#[tokio::test]
async fn group_bump_initializes_lazily_and_jumps_forward() {
    let dir = std::env::temp_dir().join(format!("mat-native-bump-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ini = dir.join("chip_tool_config.ini");
    write_group_fixture_ini(&ini);
    let counter_path = dir.join("native_group_counter");
    let _ = std::fs::remove_file(&counter_path);
    let transport = Arc::new(UdpTransport::bind().await.unwrap());
    let ctx = GroupCtx {
        main_ini: ini.clone(),
        counter_path,
        fabric_index: 2,
        fabric_id: 1,
        node_id: 0x0001_0001,
        scope_id: 1, // lo — 送信しないので join 可否は無関係
        dest_port: 5540,
        transport,
        sender: Mutex::new(None),
    };
    let r = bump(&ctx).await;
    let BumpOutcome::Bumped { from, to } = r else {
        panic!("expected Bumped, got Unavailable");
    };
    // lazy init 直後: next = start, ceiling = start + EPOCH →
    // from = start, to = start + 2*EPOCH。
    assert_eq!(to, from + 2 * mat_controller::group::COUNTER_EPOCH);
    // 2 回目は単調に先へ。
    let BumpOutcome::Bumped { from: f2, to: t2 } = bump(&ctx).await else {
        panic!("second bump");
    };
    assert_eq!(f2, to);
    assert!(t2 > to);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn group_bump_without_gdc_is_unavailable() {
    // g/gdc 無し ini → send() と同じ Unavailable 理由で拒否。
    let dir = std::env::temp_dir().join(format!("mat-native-bump-nogdc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ini = dir.join("chip_tool_config.ini");
    std::fs::write(&ini, "[Default]\n").unwrap();
    let transport = Arc::new(UdpTransport::bind().await.unwrap());
    let ctx = GroupCtx {
        main_ini: ini,
        counter_path: dir.join("native_group_counter"),
        fabric_index: 2,
        fabric_id: 1,
        node_id: 0x0001_0001,
        scope_id: 1,
        dest_port: 5540,
        transport,
        sender: Mutex::new(None),
    };
    assert!(matches!(bump(&ctx).await, BumpOutcome::Unavailable(_)));
    let _ = std::fs::remove_dir_all(&dir);
}
```

注: `COUNTER_EPOCH` が非公開なら `mat_controller::group::COUNTER_EPOCH` は既に `pub const`（group.rs:21）なのでそのまま使える。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-native group_bump`
Expected: FAIL（`bump` / `BumpOutcome` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装（init_sender 抽出 + bump）**

`send()` の `if slot.is_none() { ... }` ブロック（59–91 行）を関数へ抽出し、send と bump で共有する:

```rust
/// send / bump 共有の lazy 初期化。`Err(reason)` = native では実行できない
/// （未 provision・KVS 不備・counter 初期化不能）→ 呼び出し側が
/// `Unavailable` に写像する。
fn init_sender(ctx: &GroupCtx, slot: &mut Option<GroupSender>) -> Result<(), String> {
    if slot.is_some() {
        return Ok(());
    }
    let gdc = match kvs::read_group_data_counter(&ctx.main_ini) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Err("chip-tool g/gdc missing; refusing to start the group counter low".into())
        }
        Err(e) => return Err(format!("read g/gdc: {e}")),
    };
    let counter = PersistedGroupCounter::load(&ctx.counter_path, gdc)
        .map_err(|e| format!("group counter store: {e}"))?;
    let sender = GroupSender::new(
        Arc::clone(&ctx.transport),
        ctx.scope_id,
        ctx.dest_port,
        ctx.fabric_id,
        ctx.node_id,
        counter,
    )
    .map_err(|e| format!("multicast socket setup: {e}"))?;
    *slot = Some(sender);
    Ok(())
}
```

`send()` の該当ブロックは `if let Err(reason) = init_sender(ctx, &mut slot) { return Ok(GroupOutcome::Unavailable(reason)); }` に置換（出す `Unavailable` 理由文字列は現行と同一に保つ — matd integration テストが detail 文字列に依存）。

`bump` 本体と outcome:

```rust
/// group 送信 counter の窓ジャンプ結果。`Unavailable` は send() と同じ
/// 「native では実行できない」合図（消費側で store_parse ハードエラー化）。
pub enum BumpOutcome {
    Bumped { from: u32, to: u32 },
    Unavailable(String),
}

/// 送信 counter を matd 再起動 1 回相当ジャンプする（Issue #14 応急処置）。
/// 送出を伴わないため Err は無く、counter/KVS 系の失敗はすべて
/// `Unavailable`（persist 失敗含む — counter ストアの書込不能は store 問題）。
pub async fn bump(ctx: &GroupCtx) -> BumpOutcome {
    let mut slot = ctx.sender.lock().await;
    if let Err(reason) = init_sender(ctx, &mut slot) {
        return BumpOutcome::Unavailable(reason);
    }
    match slot.as_mut().expect("built above").bump_counter() {
        Ok((from, to)) => {
            tracing::info!(from, to, "group counter bumped (native)");
            BumpOutcome::Bumped { from, to }
        }
        Err(e) => BumpOutcome::Unavailable(format!("group counter store: {e}")),
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native group`
Expected: 新 2 件 PASS + 既存（`group_invoke_sends_multicast_and_reports_sent` 等）全 PASS

- [ ] **Step 5: `task check` → commit**

```bash
task check
git add crates/mat-native/src/group.rs
git commit -m "feat(mat-native): group::bump — send と lazy 初期化を共有する counter 窓ジャンプ"
```

---

### Task 3: matd — protocol `group_bump` op + NativeBackend + server dispatch + body

**Files:**
- Modify: `crates/mat-core/src/body.rs`（group 系 body の並び、147 行〜付近）
- Modify: `crates/matd/src/protocol.rs`（`enum Op` 23 行〜、accessor 4 つ、`mod tests`）
- Modify: `crates/matd/src/native.rs`（`group_invoke` 341 行〜の隣）
- Modify: `crates/matd/src/server.rs`（dispatch の `match op`、526 行付近 `Op::GroupProvision` の隣）
- Modify: `crates/matd/tests/integration.rs`（`group_invoke_without_group_ctx_returns_store_parse` 273 行〜の隣）

**Interfaces:**
- Consumes: Task 2 の `mat_native::group::{bump, BumpOutcome}`、既存 `group_unavailable_error(&str)`（server.rs）、`Store::open`
- Produces:
  - `mat_core::body::group_bump(from: u32, to: u32) -> Value` = `{"group_counter":{"from":…,"to":…}}`
  - `matd::protocol::Op::GroupBump`（wire: `{"op":"group_bump"}`、フィールドなし）
  - `matd::native::NativeBackend::group_bump(&self) -> mat_native::group::BumpOutcome`

- [ ] **Step 1: 失敗するテストを書く（protocol / body / integration）**

`crates/matd/src/protocol.rs` の `mod tests`:

```rust
#[test]
fn group_bump_parses_with_no_fields() {
    let r = parse(r#"{"op":"group_bump"}"#);
    assert!(matches!(r.op, Op::GroupBump));
    // fabric 全体で counter は 1 本 — node / group / endpoint を持たない。
    assert_eq!(r.op.node_id(), None);
    assert_eq!(r.op.group_id(), None);
    assert_eq!(r.op.endpoint(), None);
    assert_eq!(r.op.log_path(), None);
    assert_eq!(r.op.name(), "group_bump");
}
```

`crates/matd/tests/integration.rs`（既存 `group_invoke_without_group_ctx_returns_store_parse` を雛形に、同じ start ヘルパで）:

```rust
/// group ctx 未構成の backend への group_bump は send と同じ store_parse。
#[tokio::test]
async fn group_bump_without_group_ctx_returns_store_parse() {
    let (store_dir, store_path) = make_store();
    let (socket, _task) = start_matd_with_fake(store_path).await;
    let resps = roundtrip(&socket, &[json!({"op":"group_bump"})]).await;
    assert_eq!(resps[0]["error"]["kind"], json!("store_parse"));
    drop(store_dir);
}
```

（`make_store` / `start_matd_with_fake` / `roundtrip` の正確なシグネチャは同ファイル既存テストに合わせること。）

`crates/matd/src/server.rs` の `mod tests` に成功系（`write_group_fixture_ini` + GroupCtx を組む既存 1591 行付近のテストを雛形に、dispatch を通して）:

```rust
#[tokio::test]
async fn group_bump_dispatch_reports_from_and_to() {
    // 雛形: group provision / group invoke 系の既存 server テストと同じ
    // fixture（write_group_fixture_ini + GroupCtx 付き NativeBackend）を組み、
    // Op::GroupBump を dispatch して body を検証する。
    // 期待: body["group_counter"]["to"] = body["group_counter"]["from"] + 8192
    //（fresh counter file の lazy init 直後は to - from = 2 * COUNTER_EPOCH）。
}
```

（組み立ての詳細は既存 server テストのヘルパ流用。検証は `from`/`to` の存在と差 8192。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd group_bump`
Expected: FAIL（`Op::GroupBump` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

1. `mat-core/src/body.rs`（`group_invoke_sent` の後）:

```rust
/// `group bump` の成功 body。counter 窓ジャンプ（Issue #14 応急コマンド —
/// 受信側リプレイ窓が送信系列より先行した状態を matd 再起動なしで回復する）。
pub fn group_bump(from: u32, to: u32) -> Value {
    json!({ "group_counter": { "from": from, "to": to } })
}
```

2. `matd/src/protocol.rs`: `enum Op` の `Listen` の前に variant 追加 + accessor 4 つの match へ網羅追加:

```rust
/// group 送信 counter の窓ジャンプ（`mat group bump` 相当、Issue #14 応急
/// コマンド）。counter は fabric 全体で 1 本 — 対象 group は取らない。
GroupBump,
```

`node_id()` / `group_id()` / `endpoint()` / `log_path()` の None 側 arm に `Op::GroupBump` を追加、`name()` に `Op::GroupBump => "group_bump"`。

3. `matd/src/native.rs`（`group_invoke` の直後）:

```rust
/// group 送信 counter の窓ジャンプ（Issue #14）。ctx 未構成は send と同じ
/// Unavailable（消費側で store_parse 化）。
pub async fn group_bump(&self) -> mat_native::group::BumpOutcome {
    let Some(ctx) = &self.engine.group else {
        return mat_native::group::BumpOutcome::Unavailable(
            "native group context not configured".into(),
        );
    };
    mat_native::group::bump(ctx).await
}
```

4. `matd/src/server.rs` dispatch の `match op`（`Op::GroupProvision` arm の隣）:

```rust
Op::GroupBump => {
    // 前提チェックは group invoke と同じ（store が開けること）。
    let _store = Store::open(store_path)?;
    match native.group_bump().await {
        mat_native::group::BumpOutcome::Bumped { from, to } => {
            Ok(mat_core::body::group_bump(from, to))
        }
        mat_native::group::BumpOutcome::Unavailable(reason) => {
            Err(group_unavailable_error(&reason))
        }
    }
}
```

（dispatch までの到達経路で `require_node` / hotpath 分岐に触れないこと — `node_id()` が None なので既存フローで素通りする。server.rs 内の op 分類（OpLogClass 等）に網羅 match があれば コンパイラの指示どおり追加する。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd && cargo test -p mat-core`
Expected: 新規 3 件 PASS + 既存全 PASS

- [ ] **Step 5: `task check` → commit**

```bash
task check
git add crates/mat-core/src/body.rs crates/matd/src/protocol.rs crates/matd/src/native.rs crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): group_bump op — counter 窓ジャンプを socket プロトコルへ追加"
```

---

### Task 4: mat CLI — `group bump` サブコマンド（matd 経路 + 直経路）

**Files:**
- Modify: `crates/mat/src/cli.rs`（`enum GroupCommand` 413 行〜、`Grant` variant の後）
- Modify: `crates/mat/src/matd_client.rs`（`Command::Group` の match 287 行〜、`mod tests` の group マッピングテスト群 771 行〜の隣）
- Modify: `crates/mat/src/native_direct.rs`（`enum NativeOp` 35 行〜、classify の `Command::Group` 群 256 行〜、exec の group 群 1249 行〜、`op_group_invoke_generic` 987 行〜の隣）
- Modify: `crates/mat/src/commands/group.rs`（`emit_invoke_sent` 49 行〜の隣）

**Interfaces:**
- Consumes: Task 2 の `mat_native::group::{bump, BumpOutcome}`、Task 3 の `mat_core::body::group_bump`、既存 `group_ctx_unconfigured_error()` / `group_unavailable_error()`（native_direct.rs）、`output::emit`
- Produces: CLI `mat group bump`（引数なし）、`NativeOp::GroupBump`、`commands::group::emit_bump(from: u32, to: u32)`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/src/matd_client.rs` の既存 group マッピングテスト群（771 行〜）の隣:

```rust
#[test]
fn group_bump_maps_to_group_bump_op() {
    let cmd = Command::Group {
        action: GroupCommand::Bump,
    };
    // 既存 group テストと同じ request-build ヘルパで op JSON を検証。
    let op = build_op_for_test(&cmd).unwrap();
    assert_eq!(op["op"], json!("group_bump"));
}
```

（`build_op_for_test` は仮名 — 既存テスト（`let cmd = Command::Group {…}` で始まる 771/883/894 行）が実際に使っている関数名に合わせること。）

`crates/mat/src/native_direct.rs` の classify テスト（1668 行付近の `GroupOnOff` classify テストの隣）:

```rust
#[test]
fn classify_group_bump() {
    let cmd = Command::Group {
        action: GroupCommand::Bump,
    };
    // 既存 classify テストの呼び形に合わせる。
    assert!(matches!(classify_for_test(&cmd), Some(Ok(NativeOp::GroupBump))));
}
```

exec 側テスト（1777 行付近の `GroupOnOff` exec テストが `Engine::with_parts` + fixture で組んでいる形を雛形に）: `NativeOp::GroupBump` を実行 → stdout emit はテスト不能でも `Ok(())` と counter ファイル前進を検証。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat group_bump`
Expected: FAIL（`GroupCommand::Bump` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

1. `cli.rs` の `enum GroupCommand` に追加（`Grant` の後）:

```rust
/// group 送信 counter を「matd 再起動 1 回相当」前方ジャンプする応急
/// コマンド（Issue #14）。受信側のリプレイ窓が送信系列より先行して
/// groupcast が黙って捨てられる状態を、matd 再起動なし・常駐購読を
/// 落とさずに回復する。counter は fabric 全体で 1 本 — group 指定は無い。
Bump,
```

2. `matd_client.rs` の `Command::Group { action } => match action` に追加（`Grant` の `unsupported` より前）:

```rust
GroupCommand::Bump => json!({ "op": "group_bump" }),
```

3. `native_direct.rs`:
   - `enum NativeOp` に `GroupBump,` を追加（doc コメント: 直経路の counter 窓ジャンプ。matd 稼働中は flock WouldBlock → store_parse になる旨）。
   - classify の `Command::Group` 群に `Command::Group { action: GroupCommand::Bump } => NativeOp::GroupBump,` を追加。
   - `NativeOp` を網羅する他の match（682 行付近の node_id 対応表など）へコンパイラの指示どおり `GroupBump` を追加（node 文脈なし側）。
   - exec の group 群（1249 行付近）に arm 追加 + 実行関数（`op_group_invoke_generic` の隣）:

```rust
async fn op_group_bump(engine: &Engine) -> Result<(), MatError> {
    let Some(ctx) = &engine.group else {
        return Err(group_ctx_unconfigured_error());
    };
    match mat_native::group::bump(ctx).await {
        mat_native::group::BumpOutcome::Bumped { from, to } => {
            crate::commands::group::emit_bump(from, to);
            Ok(())
        }
        mat_native::group::BumpOutcome::Unavailable(reason) => {
            Err(group_unavailable_error(&reason))
        }
    }
}
```

4. `commands/group.rs`（`emit_invoke_sent` の隣）:

```rust
/// `group bump` の出力部（body は `mat_core::body` 共有 — matd 経路と同形）。
pub(crate) fn emit_bump(from: u32, to: u32) {
    output::emit(body::group_bump(from, to));
}
```

- [ ] **Step 4: テストが通ることを確認 + 手元スモーク**

Run: `cargo test -p mat`
Expected: 新規 PASS + 既存全 PASS

Run: `cargo run -p mat -- group bump --help`
Expected: help が出て exit 0（引数なしサブコマンドとして認識）

- [ ] **Step 5: `task check` → commit**

```bash
task check
git add crates/mat/src/cli.rs crates/mat/src/matd_client.rs crates/mat/src/native_direct.rs crates/mat/src/commands/group.rs
git commit -m "feat(mat): group bump サブコマンド — matd 経路 + 直経路"
```

---

### Task 5: ドキュメント + バージョン 1.9.0

**Files:**
- Modify: `Cargo.toml`（workspace `version = "1.8.0"` → `"1.9.0"`）+ `cargo update --workspace`（Cargo.lock 追随。実際のコマンドはリポジトリ慣行に合わせ `cargo check` で lock 更新でも可）
- Modify: `README.md`（group コマンド節 657 行付近に bump の説明 + 使用例、matd 対応 op への言及箇所）
- Modify: `ARCHITECTURE.md`（更新履歴の節があれば 1.9.0 行を追加 — 既存の直近リリース記録の形式に合わせる）

**Interfaces:**
- Consumes: Task 1–4 の完成物（記載内容の正）
- Produces: リリース可能な 1.9.0

- [ ] **Step 1: README 更新**

`group invoke` の説明（665 行付近）の後に追加する内容（英語 — README の既存文体に合わせる）:

```markdown
### `mat group bump` — jump the group counter window (first aid)

If some devices silently drop groupcast (unicast fine, group settings
identical to a working peer), their replay window may have run ahead of the
controller's send counter. `mat group bump` jumps the counter forward by one
matd-restart-equivalent window — the same remedy a matd restart applied,
without dropping warm sessions or resident subscriptions.

    mat group bump
    {"timestamp":"...","group_counter":{"from":176561405,"to":176569504}}

The counter is fabric-global (one series for all groups), so there is no
`--group` argument. Routed like other group ops: via matd when one is
running, else directly (a direct run while matd holds the counter lock
fails with `store_parse` — use the matd route).
```

- [ ] **Step 2: バージョン 1.9.0 + ARCHITECTURE 記録**

`Cargo.toml` の workspace version を `1.9.0` へ。ARCHITECTURE.md に直近リリースの記録節があれば同形式で 1 行（「1.9.0: `mat group bump` — Issue #14 帰結の counter 窓ジャンプ応急コマンド」相当）を追加。無ければ変更不要。

- [ ] **Step 3: `task check` → commit**

```bash
task check
git add Cargo.toml Cargo.lock README.md ARCHITECTURE.md
git commit -m "chore: 1.9.0（mat group bump — Issue #14 帰結の応急コマンド）+ README/ARCHITECTURE"
```

---

### Task 6: 実機 E2E（マージ前・隔離）+ デプロイ後スモーク手順

**Files:** なし（実機検証。手順は jarvis-matd-deploy の隔離方式に従う）

**Interfaces:**
- Consumes: `task dist:arm64` の `dist/arm64/{mat,matd}`、jarvis の隔離 matd 方式（別 socket + store コピー + 台帳 1 ノード）

**マージ前（隔離 — 本番 counter・電波に触れない）:**

- [ ] **Step 1: `task dist:arm64` → `mat.new` / `matd.new` を jarvis へ scp**
- [ ] **Step 2: 隔離 store（`cp -r ~/.config/mat /tmp/bump-e2e/store` + nodes.json を 1 ノードへ）で `matd.new --store … --socket /tmp/bump-e2e/t.sock` を起動**
- [ ] **Step 3: `mat.new --matd /tmp/bump-e2e/t.sock group bump` → `{"group_counter":{"from","to"}}` を確認、`to - from` が 4096〜8192 域であること、コピー側 `native_group_counter` ファイル値が `to + 4096` になっていること**
- [ ] **Step 4: もう一度 bump → from が前回 to と一致（matd in-memory とファイルの一貫性）**
- [ ] **Step 5: 隔離 matd 停止・`/tmp/bump-e2e` 削除（★group 送信はコピー store から絶対に行わない — Issue #14 フォレンジックの教訓）**
- [ ] **Step 6: 直経路も 1 確認: 隔離 store を指して（matd 停止状態で）`MAT_FABRIC_INDEX=2 mat.new group bump` 直実行 → 同形 JSON**

**マージ・デプロイ後（本番スモーク — これが本命の配達検証）:**

- [ ] **Step 7: despliegue 手順で本番へ 1.9.0 配備・matd 再起動**
- [ ] **Step 8: `mat group bump`（matd 経路）→ journal で次の groupcast counter がジャンプ済みであること**
- [ ] **Step 9: `mat group invoke -g desk_room_lights -c onoff --command on/off` → node 17 / node 6 両方の物理反応を確認（bump 後も配達正常 = 前方ジャンプは受信側に無害の再確認）**

---

### Task 7: Issue #14 更新（フォレンジック追記 → 実装リンク → クローズ）

**Files:** なし（gh CLI）

- [ ] **Step 1: Issue #14 へフォレンジックコメント追記**（spec の「背景」節の内容: 3 仮説の否定根拠・診断シグネチャ訂正・7/9 初回は経路混在で正＝0.21.0 で解消済み・残存仮説は受信側・次回採証手順（tcpdump / UpTime=0x33/2 / RebootCount=0x33/1 の即時取得））
- [ ] **Step 2: 対応 = `mat group bump`（spec / 実装 PR / バージョン 1.9.0）をコメントで紐付け**
- [ ] **Step 3: デプロイ・スモーク（Task 6 Step 7–9）完了後にクローズ**

---

## Self-Review 済み事項

- spec 全節にタスク対応あり（jump 算術=Task 1、lazy 共有=Task 2、matd op=Task 3、CLI/ルーティング=Task 4、README=Task 5、E2E=Task 6、issue=Task 7。スコープ外の「案1 見送り」「受信側解明の保留」はコードタスク無しが正）
- 型整合: `jump()/bump_counter() -> io::Result<(u32,u32)>`、`bump(ctx) -> BumpOutcome`（Err なし）、`group_bump(from,to) -> Value` を全タスクで統一
- バージョンスキュー: 新 mat → 旧 matd は op parse エラー（serde unknown variant → 既存の parse_error 応答）。mat/matd は常に同時デプロイ（despliegue 手順）なので恒常状態では発生しない。旧 mat → 新 matd は影響なし
