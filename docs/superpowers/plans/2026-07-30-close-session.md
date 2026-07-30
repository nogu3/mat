# CloseSession Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 全 teardown 経路でセッションを手放す直前に CloseSession（Secure Channel StatusReport）を best-effort 送信し、放置 CASE セッションが FP300 系デバイスの常駐購読を黙殺する事故（Issue #20）を根治する。

**Architecture:** `SecureSession` に暗号化1発・MRP再送なしの `send_close_session()` を追加（`send_standalone_ack` と同型）。`NodeConn`/`SubscribeConn` 両 trait に default no-op の `close()` を生やして実装型が委譲。呼び出しは mat 直経路 op 終了時・matd warm op 破棄時・matd 購読 pump 終了時の3系統。

**Tech Stack:** Rust (tokio, async-trait)。spec = `docs/superpowers/specs/2026-07-30-close-session-design.md`。

## Global Constraints

- CloseSession = StatusReport `{general=SUCCESS(0), protocol_id=0x0000, protocol_code=2}`。
- 送信は **needs_ack=false・1データグラム・エラー無視**。teardown 経路に待ちを追加しない（`pase.rs:363-380` の send_once 前例、`subscription.rs:9` の probe 撤去実測に整合）。
- 各タスク完了時に `cargo test -p <crate>` green、最後に `task check`（fmt:check + clippy -D warnings + 全 test）。
- コミットメッセージ末尾: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` と `Claude-Session: https://claude.ai/code/session_01GkZMfhCpteXdQMueRBBHn2`。
- コメントは既存流儀（日本語・「なぜ」だけ書く）。

---

### Task 1: mat-controller — `send_close_session` プリミティブ

**Files:**
- Modify: `crates/mat-controller/src/case.rs`（定数追加、`:34` の `STATUS_SUCCESS` 付近）
- Modify: `crates/mat-controller/src/session.rs`（メソッド追加、`send_standalone_ack` の直後 `:260` 付近 + tests mod）

**Interfaces:**
- Produces: `pub async fn SecureSession::send_close_session(&mut self)`（戻り値なし。失敗は内部で握って debug ログのみ）
- Produces: `pub(crate) const SC_PROTOCOL_CODE_CLOSE_SESSION: u16 = 2;`（case.rs）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/session.rs` の `mod tests` に追加（既存ヘルパ `bind_local` / `keys` / `open_from_controller` / 定数群をそのまま使う）:

```rust
/// CloseSession は needs_ack なしの 1 データグラムで、payload が
/// StatusReport(SUCCESS, secure channel, CloseSession=2) であること（Issue #20）。
#[tokio::test]
async fn close_session_sends_single_best_effort_status_report() {
    let device = bind_local().await;
    let peer = device.local_addr().unwrap();
    let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
    let mut s = SecureSession::new(
        Arc::clone(&transport),
        peer,
        LOCAL_SID,
        PEER_SID,
        keys(),
        OUR_NODE,
        DEV_NODE,
    );
    s.send_close_session().await;
    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, _) = device.recv_from(&mut buf).await.unwrap();
    let (_, proto, payload) = open_from_controller(&buf[..n]);
    assert_eq!(proto.protocol_id, PROTOCOL_ID_SECURE_CHANNEL);
    assert_eq!(proto.opcode, OPCODE_STATUS_REPORT);
    assert!(!proto.needs_ack, "CloseSession must be best-effort");
    let (general, proto_id, code) = crate::case::parse_status_report(&payload).unwrap();
    assert_eq!((general, proto_id, code), (0, 0, 2));
    // 再送しないこと（MRP に乗せない）。
    let again =
        tokio::time::timeout(Duration::from_millis(300), device.recv_from(&mut buf)).await;
    assert!(again.is_err(), "CloseSession must not be retransmitted");
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller close_session_sends_single -- --nocapture`
Expected: FAIL（`send_close_session` 未定義のコンパイルエラー）

- [ ] **Step 3: 最小実装**

`case.rs` の `STATUS_SUCCESS`（`:34` 付近）の近くに:

```rust
/// Secure Channel StatusReport の CloseSession protocol code（general=SUCCESS 側。
/// 同値 2 の `SC_PROTOCOL_CODE_INVALID_PARAMETER` は general=FAILURE 側で別物）。
pub(crate) const SC_PROTOCOL_CODE_CLOSE_SESSION: u16 = 2;
```

`session.rs` の `send_standalone_ack` 直後に:

```rust
/// CloseSession（StatusReport SUCCESS/secure channel/2）を best-effort で 1 発
/// 送る。放置セッションは FP300 系 FW の購読レポート付け替えで常駐購読を
/// 黙殺する（Issue #20）ため、セッションを手放す全経路がこれを呼ぶ。
/// MRP に乗せない（teardown を ~4.7s の再送予算でブロックしない —
/// pase.rs の abort StatusReport と同じ判断）。失敗は握りつぶす。
pub async fn send_close_session(&mut self) {
    let payload = crate::case::encode_status_report(
        0,
        u32::from(PROTOCOL_ID_SECURE_CHANNEL),
        crate::case::SC_PROTOCOL_CODE_CLOSE_SESSION,
    );
    let sealed = self.seal(
        Self::new_exchange_id(),
        true,
        PROTOCOL_ID_SECURE_CHANNEL,
        OPCODE_STATUS_REPORT,
        false,
        None,
        &payload,
    );
    if let Ok((datagram, _)) = sealed {
        let _ = self.transport.send_to(&datagram, self.peer).await;
        tracing::debug!(peer = %self.peer, "sent CloseSession");
    }
}
```

注意: `encode_status_report` / `parse_status_report` は `pub(crate)`（`case.rs:299/:314`）なので同一クレート内から見える。可視性変更は不要。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller close_session_sends_single`
Expected: PASS

- [ ] **Step 5: クレート全体の回帰確認 + コミット**

Run: `cargo test -p mat-controller`
Expected: all PASS

```bash
git add crates/mat-controller/src/case.rs crates/mat-controller/src/session.rs
git commit -m "feat(mat-controller): SecureSession::send_close_session — best-effort CloseSession (Issue #20)"
```

---

### Task 2: mat-native — `close()` を両 trait に追加し実装型が委譲

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`NodeConn` trait `:43`、`SubscribeConn` trait `:164`、`SessionConn` impl `:553` 付近、`SubscriptionSession` impl `:511` 付近）
- Modify: `crates/mat-native/src/test_support.rs`（`FakeConn` `:121` / `FakeSubConn` `:21` に close 記録を追加）

**Interfaces:**
- Consumes: Task 1 の `SecureSession::send_close_session()`
- Produces: `NodeConn::close(&mut self)` / `SubscribeConn::close(&mut self)`（default no-op、`#[async_trait]` の default method）
- Produces: `FakeConn::close_calls() -> usize`（`calls` 記録に `"close()"` を push する方式）、`FakeSubConn` に `pub close_calls: Arc<AtomicUsize>`（購読テストは conn を establisher に渡してしまうため共有カウンタで観測する）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/test_support.rs` の tests（無ければ `lib.rs` の tests mod）に:

```rust
/// close() の default は no-op（fake は上書きで記録する）。
#[tokio::test]
async fn fake_conn_records_close() {
    let mut c = FakeConn::default();
    c.close().await;
    assert_eq!(c.calls(), ["close()"]);
}

#[tokio::test]
async fn fake_sub_conn_records_close() {
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut c = FakeSubConn::default();
    c.close_calls = std::sync::Arc::clone(&counter);
    c.close().await;
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
}
```

（`FakeSubConn` のフィールド初期化流儀は既存 `Default`/コンストラクタに合わせて調整。）

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-native fake_conn_records_close`
Expected: FAIL（`close` 未定義）

- [ ] **Step 3: 実装**

`NodeConn` trait（`lib.rs:43` の末尾、`open_window` の後）:

```rust
/// セッションを手放す直前の後始末。CloseSession を best-effort 送信する
/// （Issue #20: 放置セッションが FP300 系の常駐購読を黙殺する）。fake は
/// 既定 no-op で足りるよう default 実装を持つ。
async fn close(&mut self) {}
```

`SubscribeConn` trait（`:164`）にも同文で `async fn close(&mut self) {}`。

`SessionConn` の `impl NodeConn` に:

```rust
async fn close(&mut self) {
    self.session.send_close_session().await;
}
```

`SubscriptionSession` の `impl SubscribeConn` にも同じ委譲を追加。

`FakeConn` の `impl NodeConn` に（記録のみ）:

```rust
async fn close(&mut self) {
    self.calls.push("close()".to_string());
}
```

`FakeSubConn` に `pub close_calls: Arc<AtomicUsize>` フィールドを追加（Default で fresh）、`impl SubscribeConn` に:

```rust
async fn close(&mut self) {
    self.close_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}
```

- [ ] **Step 4: 通ることを確認 + クレート回帰**

Run: `cargo test -p mat-native`
Expected: all PASS（`ops.rs:649` の `FailingConn` 等、他の fake は default no-op で無変更コンパイル）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-native/src/lib.rs crates/mat-native/src/test_support.rs
git commit -m "feat(mat-native): NodeConn/SubscribeConn に close() — CloseSession 委譲 (Issue #20)"
```

---

### Task 3: mat 直経路 — 全 op が終了前に close する

**Files:**
- Modify: `crates/mat/src/native_direct.rs`（`establisher.establish(` の全16サイト: `:786, :800, :820, :847, :875, :903, :1108, :1136, :1160, :1180, :1217, :1226, :1234, :1261, :1513, :1594` 付近）
- Test: 同ファイル内の既存 tests mod

**Interfaces:**
- Consumes: Task 2 の `NodeConn::close()`
- Produces: なし（挙動変更のみ: 各 op fn が **成否によらず** conn を手放す前に close）

- [ ] **Step 1: 失敗するテストを書く**

`native_direct.rs` の既存テスト流儀（FakeEstablisher/FakeConn で op fn を直接呼ぶ）に合わせ、代表 2 本:

```rust
/// op 成功時に close が呼ばれる（Issue #20）。
#[tokio::test]
async fn op_on_closes_session_on_success() {
    // 既存テストと同じ手順で FakeEstablisher から engine を組む。
    // 実行後、fake conn の calls() 末尾が "close()" であること:
    // assert_eq!(calls.last().map(String::as_str), Some("close()"));
}

/// op 失敗時（invoke がエラー）でも close が呼ばれる。
#[tokio::test]
async fn op_closes_session_on_failure() {
    // FakeConn { fail_first_send: true, fail_kind: ErrorKind::Timeout, .. } を
    // 使い read_onoff 系 op を実行。Err が返り、かつ "close()" が記録されること。
}
```

（fake conn への参照の取り出し方は既存テストのパターンに従う。既存テストが conn を
establisher に move する構造で参照が残せない場合は、FakeConn にも
`close_calls: Arc<AtomicUsize>` を足して観測する — FakeSubConn と同じ手法。）

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat op_on_closes_session`
Expected: FAIL（close 未呼び出し）

- [ ] **Step 3: 実装 — establish 直後から返り値までを async ブロックで包む**

全 16 サイトを機械的に同じ形へ（例 = `op_on` `:785`）:

```rust
async fn op_on(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    // 成否によらず close してから返す（Issue #20: 放置セッションは FP300 系の
    // 常駐購読を黙殺する）。
    let result = async {
        conn.invoke(endpoint, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None, false)
            .await?;
        tracing::info!(node_id, cluster = "onoff", command = "on", "invoke executed (native direct)");
        crate::commands::invoke::emit_invoke_success(node_id, endpoint, "onoff", "on");
        Ok(())
    }
    .await;
    conn.close().await;
    result
}
```

establish が `match` される特殊サイト（`:1513` diag/probe 経路）は、conn が取れた腕
にだけ同じ「async ブロック + close + result」を適用する。establish 自体の失敗は
セッションが無いので close 不要。

- [ ] **Step 4: 通ることを確認 + クレート回帰**

Run: `cargo test -p mat`
Expected: all PASS

- [ ] **Step 5: コミット**

```bash
git add crates/mat/src/native_direct.rs
git commit -m "feat(mat): 直経路 op の全経路で終了前に CloseSession (Issue #20)"
```

---

### Task 4: matd warm op — slot を手放す全4箇所で close

**Files:**
- Modify: `crates/matd/src/native.rs`（`drop_session` `:159`、`with_session` 内 `*guard = None` 3箇所 `:223/:260/:278`）
- Test: 同ファイル `mod tests`（`:514`）

**Interfaces:**
- Consumes: Task 2 の `NodeConn::close()`
- Produces: `async fn close_and_clear(guard: &mut Option<Box<dyn NodeConn>>)`（native.rs 内 private ヘルパ）

- [ ] **Step 1: 失敗するテストを書く**

`native.rs` の既存 tests 流儀（FakeEstablisher で NativeBackend を組む）で:

```rust
/// Timeout で slot を捨てる前に close が呼ばれる（Issue #20）。
#[tokio::test]
async fn with_session_closes_before_dropping_on_timeout() {
    // fail_first_send: true / fail_kind: Timeout の FakeConn を仕込み、
    // read_onoff を1回実行（re-establish 側は成功する fake）。
    // 旧 conn の close_calls == 1 を確認。
}

/// drop_session でも close が呼ばれる。
#[tokio::test]
async fn drop_session_closes_warm_conn() {
    // warm 状態を作ってから drop_session(node)。close_calls == 1。
}
```

（観測は Task 3 と同じく `Arc<AtomicUsize>` 共有カウンタ。FakeConn に
`close_calls` を足していない場合はここで足す。）

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p matd with_session_closes_before_dropping`
Expected: FAIL

- [ ] **Step 3: 実装**

`native.rs` にヘルパを追加し、`*guard = None` の全サイトを置換:

```rust
/// slot の session を手放す前に best-effort CloseSession（Issue #20: 放置
/// セッションは FP300 系の常駐購読を黙殺する）。Timeout 腕（相手死亡疑い）でも
/// 送る — 待ちゼロなのでコスト無し。
async fn close_and_clear(guard: &mut Option<Box<dyn NodeConn>>) {
    if let Some(conn) = guard.as_mut() {
        conn.close().await;
    }
    *guard = None;
}
```

- `drop_session` `:163`: `*guard = None;` → `close_and_clear(&mut guard).await;`
- `with_session` `:223` / `:260` / `:278`: 同様に置換。

- [ ] **Step 4: 通ることを確認 + クレート回帰**

Run: `cargo test -p matd`
Expected: all PASS

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/native.rs crates/mat-native/src/test_support.rs
git commit -m "feat(matd): warm op session を手放す全経路で CloseSession (Issue #20)"
```

---

### Task 5: matd 購読 pump — 終了全経路で close

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`run_subscription_once` `:633-:740`）
- Test: 同ファイル `mod tests`（`:1498` 付近の無音 deadline テスト等が土台）

**Interfaces:**
- Consumes: Task 2 の `SubscribeConn::close()` と `FakeSubConn.close_calls`
- Produces: なし（挙動変更のみ: pump 終了・subscribe 失敗の全経路で close）

- [ ] **Step 1: 失敗するテストを書く**

既存の無音 deadline テスト（`:1498` 「max_interval 60s + slack 30s = 90s」）と同じ
組み立てで、FakeSubConn の `close_calls` を事前に clone して保持し:

```rust
/// pump が無音 deadline で終わるとき close が呼ばれる（Issue #20）。
#[tokio::test(start_paused = true)]
async fn pump_silence_end_closes_subscription_session() {
    // 既存 silence テストのセットアップを流用。pump 終了後:
    // assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

/// subscribe_wildcard が失敗したとき（CASE 成立後の失敗）も close される。
#[tokio::test]
async fn subscribe_failure_closes_session() {
    // FakeSubConn を subscribe_wildcard がエラーを返す設定にして
    // run_subscription_once が Err を返し、close_calls == 1 を確認。
}
```

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p matd pump_silence_end_closes`
Expected: FAIL

- [ ] **Step 3: 実装 — `return` を `break` に変えて関数末尾で close**

`run_subscription_once` の変更点:

1. `subscribe_wildcard` 失敗時に close してから Err:

```rust
let (info, priming) = match conn.subscribe_wildcard(clusters).await {
    Ok(v) => v,
    Err(e) => {
        // CASE は成立済み — 放置すると Issue #20 の黙殺経路になる。
        conn.close().await;
        return Err(e);
    }
};
```

2. pump loop の 4 つの `return Ok(<reason>)`（`:686/:697/:708/:736`）を
   `break <reason>` にし、loop を `let reason = loop { ... };` で受けて末尾で:

```rust
let reason = loop {
    /* 既存の判定・受信処理。return Ok(x) は break x に置換 */
};
conn.close().await;
Ok(reason)
```

（tracing の各終了ログは既存のまま残す。）

- [ ] **Step 4: 通ることを確認 + クレート回帰**

Run: `cargo test -p matd`
Expected: all PASS（既存 pump テスト群は挙動不変 — close は fake の default/記録のみ）

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): 購読 pump 終了の全経路で CloseSession (Issue #20)"
```

---

### Task 6: バージョン 1.14.0 + ドキュメント + task check

**Files:**
- Modify: `Cargo.toml`（workspace version 1.13.0 → 1.14.0）+ `Cargo.lock`
- Modify: `ARCHITECTURE.md`（Phase 5 の記録欄に 1.14.0 の1段落: CloseSession 導入の理由 = Issue #20、FP300 の最新セッション付け替え挙動）
- Modify: `README.md`（セッション後始末に言及がある場合のみ整合。無ければ触らない）

**Interfaces:**
- Consumes: Tasks 1-5 完了済みの main ツリー
- Produces: リリース可能な 1.14.0 ツリー

- [ ] **Step 1: バージョン更新**

`Cargo.toml` の workspace version を `1.14.0` へ。`cargo build` で lock 追従。

- [ ] **Step 2: ARCHITECTURE.md に記録を1段落追加**

既存の直近バージョン記録（1.13.0 の段落）の直後に、同じ文体で: Issue #20 の要約
（FP300 が購読レポートを同一ファブリック最新セッションへ付け替える → 放置セッションが
ブラックホール化 → 購読 silent 死）、CloseSession best-effort 送信を3系統
（直経路 op / matd warm op / 購読 pump）に入れた旨、send_reliable を使わない理由。

- [ ] **Step 3: CI 相当の全確認**

Run: `task check`
Expected: fmt:check / clippy (-D warnings) / 全テスト PASS

- [ ] **Step 4: コミット**

```bash
git add Cargo.toml Cargo.lock ARCHITECTURE.md README.md
git commit -m "chore: 1.14.0（CloseSession — 放置セッションの後始末、Issue #20）"
```

---

### Task 7: 実機 E2E（マージ前必須 — main セッションが実施）

**Files:** なし（jarvis 実機での検証。memory: e2e-before-merge）

**Interfaces:**
- Consumes: Task 6 までの feat/close-session ツリー

- [ ] **Step 1: aarch64 ビルドと配置**

```bash
task dist:arm64
scp dist/arm64/mat jarvis:~/mat.new-closesession
```

（本番 mat/matd は置き換えない — 検証は `mat.new-closesession` の直経路で行う。
修正の核心 = 「新 mat の直経路 diag がセッションを閉じる」なので、本番 matd の
購読が生き残るかをそのまま合否にできる。）

- [ ] **Step 2: 修正前の再現条件を確認**

```bash
ssh jarvis 'matd status'   # node16 が established であること・for_s を記録
```

- [ ] **Step 3: 再現手順の再実行（新バイナリ）**

```bash
ssh jarvis 'MAT_FABRIC_INDEX=2 ~/mat.new-closesession diag thread --node 16 >/dev/null; date +%T'
```

- [ ] **Step 4: 合否判定（6分監視）**

修正前は同手順で「最終レポート+330s」に ±1 秒で `subscription lost` が出た。
6 分後に:

```bash
ssh jarvis 'journalctl --user -u matd --since "-8 min" --no-pager | grep -E "node_id=16" | grep -c "subscription lost" ; matd status'
```

Expected: `subscription lost` が **0 件**、`matd status` の node16 `for_s` が
diag 実行を跨いで継続増加（購読生存）。

- [ ] **Step 5: 一般スモーク**

```bash
ssh jarvis 'MAT_FABRIC_INDEX=2 ~/mat.new-closesession read --node 16 --endpoint 0 --cluster basicinformation --attribute product-name'
ssh jarvis 'MAT_FABRIC_INDEX=2 ~/mat.new-closesession on --node desk_tape_light && sleep 2 && MAT_FABRIC_INDEX=2 ~/mat.new-closesession off --node desk_tape_light'
```

Expected: 正常 JSON 出力・照明が反応（op 後の close が op 自体を壊していないこと）。
op 直後にも node16 購読が死んでいないことを `matd status` で再確認。

- [ ] **Step 6: 合格したらマージ・push・デプロイ**

```bash
git checkout main && git merge --no-ff feat/close-session && git push origin main
```

その後、通常手順（memory: jarvis-matd-deploy）で mat/matd を 1.14.0 へ更新し、
再起動後の `matd status` スモークまで確認。Issue #20 をクローズ。
