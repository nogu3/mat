# matd ws keepalive + warm セッション温存リカバリ 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** アイドル 3 分超後の初回コマンド必失敗（issue #7）を解消し、ws 障害時に warm CASE セッション（chip-tool 子プロセス）を温存する。chip-tool busy-loop（issue #8）は reap 運用で有限化する。

**Architecture:** 変更は `crates/matd/src/backend.rs` と `crates/matd/src/server.rs` に閉じる。(1) `exchange` のエラーを送信/受信で型分離し、送信失敗のみ透過リトライ・受信失敗は ws だけ捨てて子を温存（連続 2 失敗で従来どおり子ごと teardown）。(2) `ensure_connected` に子の生死確認（`try_wait`）を足す。(3) keepalive タスク（45 秒周期で matd から ws Ping を送出 + 受信ドレイン）で chip-tool の「180 秒無トラフィック → PING → 20 秒で切断」をそもそも発火させない。mat の JSON スキーマ・サブコマンド・エラー kind は不変。

**Tech Stack:** Rust (edition 2021), tokio, tokio-tungstenite 0.24（`Message::Ping` は `Vec<u8>`、`Message::Text` は `String`）。テストは `crates/matd/tests/integration.rs` の fake ws サーバ方式（実 chip-tool 不使用）。

**Spec:** `docs/superpowers/specs/2026-07-10-matd-ws-keepalive-design.md`

## Global Constraints

- stdout 純 JSON / 診断は stderr `tracing`（CLAUDE.md ルール 2・3）。
- エラー kind 名は既存の安定集合のみ（`child_failed` / `timeout` / `parse_error` 等）。新 kind を作らない。
- keepalive は `last_used` を更新しない（reap を妨げない — #8 の焼き有限化の前提）。
- 受信失敗（timeout・切断・parse）はコマンド再送しない（toggle 二重実行防止）。再送するのは送信自体の失敗のみ、1 回だけ。
- 各タスク完了時に `task check`（fmt:check + clippy -D warnings + test）が通ること。
- コミットは各タスクで作成。メッセージ末尾: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`

---

### Task 1: exchange の送信/受信エラー型分離と送信失敗の透過リトライ

> **実行時訂正（2026-07-10）:** Step 1 の統合テスト `send_failure_is_retried_transparently` は
> 実測で成立しないことが判明した。サーバ側 close 後の送信は TCP 半クローズのため「成功」し、
> エラーは受信側（`next_text` の `closed before responding`）で出る — issue #7 の実機エラーが
> receive 側だったことと整合する。送信失敗の分類は `exchange`/`next_text` をトランスポート
> generic 化して `tokio::io::duplex`（相手側 drop）で決定論的にユニットテストし、統合側は
> 「サーバ close 後の失敗は最大 1 回・次は必ず成功」の性質テスト
> `server_close_costs_at_most_one_failure` に差し替えた。

**Files:**
- Modify: `crates/matd/src/backend.rs`（`exchange` / `run_cmdline`）
- Test: `crates/matd/tests/integration.rs`

**Interfaces:**
- Produces: `enum ExchangeError { Send(MatError), AfterSend(MatError) }`（backend.rs 内 private）、`run_cmdline` の新エラー経路（Task 2, 3 が拡張する）。
- Consumes: 既存 `Conn` / `ensure_connected` / `teardown` / `next_text`。

- [ ] **Step 1: 失敗テストを書く**

`crates/matd/tests/integration.rs` に追加。「応答を返した直後に接続を閉じる fake」を新設し、2 回目のコマンドが（死んだソケットへの送信失敗を内部リトライで乗り越えて）成功することを検証する。現行実装は 2 回目が `child_failed` で失敗するのでテストは落ちる。

```rust
/// 各コマンドへ応答した直後に接続を閉じる fake ws。matd 側から見ると「次の送信時には
/// ソケットが死んでいる」状況を毎回作る（issue #7 の決定論的再現形状）。
/// accept ループは生きているので、張り直せば次のコマンドは通る。
async fn spawn_fake_ws_close_after_reply() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                if let Some(Ok(Message::Text(line))) = ws.next().await {
                    let resp = json!({ "cmd": line, "results": [{ "value": true }], "logs": [] });
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                    let _ = ws.close(None).await;
                }
            });
        }
    });
    port
}

/// issue #7: ソケットが死んでいても、送信失敗は ws を張り直して透過リトライされ、
/// 呼び出し側にはエラーが見えない。
#[tokio::test]
async fn send_failure_is_retried_transparently() {
    let port = spawn_fake_ws_close_after_reply().await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_secs(300))
        .await
        .unwrap();

    // 1 回目は普通に成功し、直後に fake が接続を閉じる。
    let v1 = backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert_eq!(v1["results"][0]["value"], json!(true));

    // fake の close がワイヤに乗るまで少し待つ（send をエラーにさせるため）。
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2 回目: 死んだソケットへの送信 → 内部で張り直して透過リトライ → 成功。
    let v2 = backend.run_cmdline("onoff off 5 1").await.unwrap();
    assert_eq!(v2["results"][0]["value"], json!(true));
    // 応答の混線がないこと（fake は cmd をエコーする）。
    assert!(v2["cmd"].as_str().unwrap().contains("onoff off"));
}
```

注意: `ChipToolBackend::connect` 直後の 1 回目のコマンドの前にも fake は接続を 1 本受けている（`new` の早期接続）。fake は接続ごとに 1 コマンドだけ処理して閉じる作りなので問題ない。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd --test integration send_failure_is_retried_transparently`
Expected: FAIL（2 回目の `run_cmdline` が `ws send failed` または `chip-tool ws closed before responding` の `child_failed` を返す）

※ 送信バッファリングにより 2 回目の失敗が send でなく receive 側で出る場合、このテストは Task 2 の受信失敗経路（リトライなし）とぶつかる。その場合も「テストが今は落ちる」ことに変わりはなく、Task 2 完了後は sleep(100ms) が close をワイヤに乗せるため send 失敗として安定する。実装後もこのテストが flaky なら sleep を 300ms へ伸ばす。

- [ ] **Step 3: 実装**

`crates/matd/src/backend.rs` の `exchange` を型分離し、`run_cmdline` にリトライ経路を入れる。既存の `exchange` / `run_cmdline` を以下で置き換える:

```rust
/// exchange の失敗を送信/受信で区別する。送信失敗はコマンドが chip-tool に届いて
/// いないことが確定しているので安全に再試行できる。送信後の失敗（timeout・切断・
/// parse）はコマンドが実行された可能性を排除できない（toggle 等は再送で二重実行に
/// なる）。
enum ExchangeError {
    /// 送信自体が失敗した（chip-tool には届いていない）。
    Send(MatError),
    /// 送信後に失敗した（実行された可能性がある）。
    AfterSend(MatError),
}

impl ExchangeError {
    fn into_mat(self) -> MatError {
        match self {
            ExchangeError::Send(e) | ExchangeError::AfterSend(e) => e,
        }
    }
}

/// 確立済みの ws で 1 往復する。
async fn exchange(ws: &mut Ws, line: &str) -> Result<Value, ExchangeError> {
    ws.send(Message::Text(line.to_string())).await.map_err(|e| {
        ExchangeError::Send(MatError::new(
            ErrorKind::ChildFailed,
            format!("ws send failed: {e}"),
        ))
    })?;

    let text = match tokio::time::timeout(COMMAND_TIMEOUT, next_text(ws)).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Err(ExchangeError::AfterSend(e)),
        Err(_) => {
            return Err(ExchangeError::AfterSend(MatError::new(
                ErrorKind::Timeout,
                format!("no response from chip-tool within {COMMAND_TIMEOUT:?} for: {line}"),
            )))
        }
    };

    // 生 ws 応答（results / 失敗 error の実形状）を debug に残す。診断のみ stderr
    // （CLAUDE.md ルール 3）。失敗時 `results[i].error` の形状はこのログで実機確定済み:
    // `{"results":[{"error":"FAILURE"}],"logs":[...]}` ― `error` は status 名の
    // **文字列**（数値ではない）。[`super::server::ensure_ok`] がこれを分類する。
    tracing::debug!(%text, "chip-tool ws raw response");

    serde_json::from_str(&text).map_err(|e| {
        ExchangeError::AfterSend(MatError::parse_error(format!(
            "chip-tool ws response was not JSON: {e}; body={text}"
        )))
    })
}
```

`run_cmdline`（Task 2 でさらに受信失敗経路を仕上げる。ここでは送信リトライまで）:

```rust
    /// コマンド行を送り、最初に返る Text メッセージ（= 実行結果 JSON）を返す。
    ///
    /// chip-tool ws はコマンド完了時に結果メッセージを 1 つ返す。送信自体の失敗は
    /// 「ソケットが送信前から死んでいた」ことを意味する（chip-tool には届いていない）
    /// ので、ws だけ張り直して 1 回だけ透過リトライする — 子プロセスは温存し warm CASE
    /// セッションを守る（issue #7）。
    pub async fn run_cmdline(&self, line: &str) -> Result<Value, MatError> {
        let mut conn = self.conn.lock().await;
        self.ensure_connected(&mut conn).await?;

        let mut result = exchange(conn.ws.as_mut().expect("ensured above"), line).await;

        if let Err(ExchangeError::Send(e)) = &result {
            tracing::info!(error = %e.detail, "ws send failed; reconnecting and retrying once");
            conn.ws = None;
            self.ensure_connected(&mut conn).await?;
            result = exchange(conn.ws.as_mut().expect("ensured above"), line).await;
        }

        match result {
            Ok(mut value) => {
                conn.last_used = Instant::now();
                drop_logs(&mut value);
                Ok(value)
            }
            Err(e) => {
                // 接続が壊れた可能性。畳んで次回フル再確立に委ねる。
                // （Task 2 で受信失敗の温存経路に置き換える）
                teardown(&mut conn).await;
                Err(e.into_mat())
            }
        }
    }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --test integration send_failure_is_retried_transparently`
Expected: PASS

- [ ] **Step 5: 全チェック**

Run: `task check`
Expected: fmt / clippy / 全テスト PASS（既存テストの回帰なし）

- [ ] **Step 6: コミット**

```bash
git add crates/matd/src/backend.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): ws送信失敗を型分離し1回だけ透過リトライ（#7）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: 受信失敗はリトライせず ws のみ捨てる（子温存 + 連続失敗 2 回で teardown）

**Files:**
- Modify: `crates/matd/src/backend.rs`（`Conn` / `run_cmdline`）
- Test: `crates/matd/tests/integration.rs`

**Interfaces:**
- Consumes: Task 1 の `ExchangeError`。
- Produces: `Conn` に `failures: u8` フィールド追加。`run_cmdline` の受信失敗経路（ws 捨て・子温存・連続 2 失敗で `teardown`）。Task 3 がこの挙動を Spawn モードで検証する。

- [ ] **Step 1: 失敗テストを書く**

「コマンドを受け取って記録し、応答せずに接続を閉じる」fake で、(a) エラーが 1 回返ること、(b) fake へのコマンド着信が正確に 1 回であること（= 二重実行しない）、(c) 次のコマンドは遅延再接続で成功すること、を検証する。

```rust
/// 受け取ったコマンド行を記録し、`fail_first` 回までは応答せずに接続を閉じる fake ws。
/// それ以降の接続では普通に応答する。受信失敗（実行されたかもしれない）の再現用。
async fn spawn_fake_ws_no_reply_then_ok(
    fail_first: usize,
) -> (u16, Arc<tokio::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lines_log: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&lines_log);
    tokio::spawn(async move {
        let mut conn_no = 0usize;
        while let Ok((stream, _)) = listener.accept().await {
            conn_no += 1;
            // 1 本目は new() の早期接続だが、最初のコマンドはその接続上で走るので
            // 「接続 n 本目まで失敗」= 「コマンド n 回目まで失敗」に一致する。
            let fail = conn_no <= fail_first;
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(line) = msg {
                        log.lock().await.push(line.clone());
                        if fail {
                            let _ = ws.close(None).await;
                            return;
                        }
                        let resp =
                            json!({ "cmd": line, "results": [{ "value": true }], "logs": [] });
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    (port, lines_log)
}

/// 受信失敗（応答なしで切断）はコマンドを再送しない（toggle 二重実行防止）。
/// エラーは 1 回返り、次のコマンドは遅延再接続で成功する。
#[tokio::test]
async fn recv_failure_is_not_retried_and_recovers_lazily() {
    let (port, log) = spawn_fake_ws_no_reply_then_ok(1).await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_secs(300))
        .await
        .unwrap();

    // 1 回目: fake は受信を記録して応答せず切断 → エラーが返る。
    let err = backend.run_cmdline("onoff toggle 5 1").await.unwrap_err();
    assert_eq!(err.kind, mat_core::error::ErrorKind::ChildFailed);
    // 着信は正確に 1 回 = 再送していない。
    assert_eq!(log.lock().await.len(), 1);

    // 2 回目: 遅延再接続で成功する。
    let v = backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert_eq!(v["results"][0]["value"], json!(true));
    assert_eq!(log.lock().await.len(), 2);
}
```

補足: 1 本目の ws 接続は `connect()` の早期接続が張るが、fake はコマンドが来るまで読み待ちなので、1 回目の `run_cmdline` はその接続上で走る。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd --test integration recv_failure_is_not_retried_and_recovers_lazily`
Expected: PASS してしまう可能性がある（現行も受信失敗は再送しない）。落ちない場合、このテストは回帰防止として残し、Step 3 の実装（子温存・failures カウンタ）は Task 3 のテストで担保されることを注記して先へ進む。

- [ ] **Step 3: 実装**

`Conn` に連続失敗カウンタを足す:

```rust
/// 現在の接続状態。`ws` が `None` なら未確立（遅延確立される）。
struct Conn {
    ws: Option<Ws>,
    child: Option<Child>,
    last_used: Instant,
    /// 連続コマンド失敗数。成功で 0 に戻る。2 に達したら子ごと畳む
    /// （wedge した chip-tool を温存し続けて永久に timeout し続けるのを防ぐ保険）。
    failures: u8,
}
```

`new` の初期化に `failures: 0,` を追加。`run_cmdline` の `match result` を差し替える:

```rust
        match result {
            Ok(mut value) => {
                conn.last_used = Instant::now();
                conn.failures = 0;
                drop_logs(&mut value);
                Ok(value)
            }
            Err(ExchangeError::Send(e)) => {
                // リトライも送信で失敗。子ごと畳んで次回フル再確立に委ねる。
                teardown(&mut conn).await;
                Err(e)
            }
            Err(ExchangeError::AfterSend(e)) => {
                // 実行された可能性がありリトライしない（二重実行防止）。古い ws は捨てて
                // 遅延応答の混線を断ち、子は温存して warm CASE を守る。ただし連続 2 回
                // 失敗したら従来どおり子ごと畳む（wedge からの回復経路）。
                conn.failures = conn.failures.saturating_add(1);
                if conn.failures >= 2 {
                    tracing::warn!(
                        failures = conn.failures,
                        "consecutive command failures; tearing down chip-tool session"
                    );
                    teardown(&mut conn).await;
                } else {
                    conn.ws = None;
                }
                Err(e)
            }
        }
```

`teardown` の直後に `conn.failures = 0;` を入れる（`teardown` 関数側に足すなら `conn.failures = 0;` を関数末尾へ。呼び出し箇所が複数あるので **`teardown` 関数側に足す**こと）:

```rust
/// セッションを畳む。ws を閉じ、子プロセスがあれば落として待つ。
async fn teardown(conn: &mut Conn) {
    conn.ws = None; // Drop で close フレーム送出。
    if let Some(mut child) = conn.child.take() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    conn.failures = 0;
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --test integration recv_failure_is_not_retried_and_recovers_lazily`
Expected: PASS

- [ ] **Step 5: 全チェック**

Run: `task check`
Expected: PASS

- [ ] **Step 6: コミット**

```bash
git add crates/matd/src/backend.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): 受信失敗はws破棄のみで子を温存、連続2失敗でteardown（#7）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: 子プロセスの生死確認と Spawn モードの温存検証

**Files:**
- Modify: `crates/matd/src/backend.rs`（`ensure_connected` / テスト用アクセサ）
- Test: `crates/matd/tests/integration.rs`

**Interfaces:**
- Consumes: Task 2 の受信失敗経路（子温存）。
- Produces: `pub async fn child_pid(&self) -> Option<u32>`、`pub async fn ws_connected(&self) -> bool`（Task 4 のテストも使う）。`ensure_connected` の respawn 挙動。

- [ ] **Step 1: テスト用アクセサを実装**

`ChipToolBackend` の impl に追加（テスト専用だが integration テスト（別クレート扱い）から見える必要があるため pub。運用上も PID はログ・診断に有用）:

```rust
    /// 現在保持している子プロセスの PID。子が居なければ None（Connect モードは常に None）。
    /// 統合テストが「子が温存されたか / respawn されたか」を PID で検証するのに使う。
    pub async fn child_pid(&self) -> Option<u32> {
        self.conn.lock().await.child.as_ref().and_then(|c| c.id())
    }

    /// ws が確立されているか。統合テストが keepalive の切断検知を検証するのに使う。
    pub async fn ws_connected(&self) -> bool {
        self.conn.lock().await.ws.is_some()
    }
```

- [ ] **Step 2: 失敗テストを書く**

Spawn モードで「受信失敗しても子 PID が変わらない」「子が死んだら respawn される」を検証する。fake の ws ポートと子プロセスは独立（子は寝ているだけのスクリプト、ws は fake が同ポートで待つ）なので、実 chip-tool なしで Spawn モードの子管理だけを検証できる。

```rust
/// Spawn モード検証用の fake chip-tool（寝るだけのシェルスクリプト）を用意し、
/// MAT_CHIP_TOOL_BIN に設定する。ws は fake サーバが別途待ち受けるので、子は引数を
/// 無視して寝ていればよい。
///
/// env はプロセスグローバルなので、パスは**固定**（temp_dir 直下）・内容は全テスト共通・
/// 削除しない。これで Spawn モードの複数テストが並行しても互いに無害（同じ値を
/// set_var し合うだけ）。
fn setup_fake_child_bin() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join("matd-test-fake-chip-tool.sh");
    std::fs::write(&path, "#!/bin/sh\nsleep 300\n").unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    std::env::set_var("MAT_CHIP_TOOL_BIN", &path);
    path
}

/// 受信失敗 1 回では子プロセスを殺さない（warm CASE 温存）。連続 2 回で子ごと畳み、
/// 子が死んでいたら次の ensure で respawn する。
///
/// MAT_CHIP_TOOL_BIN はプロセスグローバルなので、Spawn モードを使うテストはこの 1 本に
/// まとめる（並行テストとの env 競合を避ける）。
#[tokio::test]
async fn spawn_mode_preserves_child_and_respawns_dead_child() {
    let tmp = tempfile::tempdir().unwrap();
    setup_fake_child_bin();

    // 1 回だけ応答なし切断 → 以降正常、の fake（Task 2 のものを流用）。
    let (port, _log) = spawn_fake_ws_no_reply_then_ok(1).await;
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend = matd::backend::ChipToolBackend::spawn(&store, port, Duration::from_secs(300))
        .await
        .unwrap();

    let pid1 = backend.child_pid().await.expect("child spawned");

    // 受信失敗 1 回: エラーは返るが子は温存される。
    let _ = backend.run_cmdline("onoff toggle 5 1").await.unwrap_err();
    assert_eq!(
        backend.child_pid().await,
        Some(pid1),
        "receive failure must not kill the healthy child"
    );
    assert!(!backend.ws_connected().await, "broken ws must be dropped");

    // 次のコマンドは遅延再接続で成功し、失敗カウンタが 0 に戻る。
    backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert_eq!(backend.child_pid().await, Some(pid1));

    // 子を外から殺す → 次の ensure（ws を落としてから）で respawn される。
    let kill = std::process::Command::new("kill")
        .arg(pid1.to_string())
        .status()
        .unwrap();
    assert!(kill.success());
    tokio::time::sleep(Duration::from_millis(100)).await; // 子の exit を待つ
    backend.shutdown_ws_for_test().await; // 下記 Step 3 参照: ws だけ落とすヘルパ
    backend.run_cmdline("onoff on 5 1").await.unwrap();
    let pid2 = backend.child_pid().await.expect("respawned");
    assert_ne!(pid2, pid1, "dead child must be respawned");
}

/// 保険の検証: 受信失敗が 2 連続したら従来どおり子ごと畳む（wedge した chip-tool を
/// 温存し続けて永久に timeout し続けるのを防ぐ）。
#[tokio::test]
async fn two_consecutive_recv_failures_tear_down_child() {
    let tmp = tempfile::tempdir().unwrap();
    setup_fake_child_bin();

    let (port, _log) = spawn_fake_ws_no_reply_then_ok(2).await;
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend = matd::backend::ChipToolBackend::spawn(&store, port, Duration::from_secs(300))
        .await
        .unwrap();
    let pid1 = backend.child_pid().await.expect("child spawned");

    // 1 回目の受信失敗: 子は温存。
    let _ = backend.run_cmdline("onoff on 5 1").await.unwrap_err();
    assert_eq!(backend.child_pid().await, Some(pid1));

    // 2 回目の受信失敗: 連続 2 回で子ごと teardown。
    let _ = backend.run_cmdline("onoff on 5 1").await.unwrap_err();
    assert_eq!(backend.child_pid().await, None, "2nd consecutive failure tears down");

    // 3 回目: フル再確立（respawn）で成功し、カウンタも 0 に戻っている。
    backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert!(backend.child_pid().await.is_some());
}
```

注意: `tempfile` が `crates/matd/Cargo.toml` の `[dev-dependencies]` に無ければ追加する（`tempfile = "3"`）。既にあるか `grep tempfile crates/matd/Cargo.toml` で確認。

- [ ] **Step 3: 実装**

`ensure_connected` の `Mode::Spawn` 分岐に生死確認を足す:

```rust
            Mode::Spawn { store, port } => {
                // 子が既に死んでいれば respawn（死んだ子の ws ポートへ STARTUP_TIMEOUT
                // いっぱい粘って無駄に待つのを防ぐ）。
                if let Some(child) = conn.child.as_mut() {
                    if let Ok(Some(status)) = child.try_wait() {
                        tracing::warn!(%status, "chip-tool child exited; will respawn");
                        conn.child = None;
                    }
                }
                if conn.child.is_none() {
                    conn.child = Some(spawn_child(store, *port)?);
                }
                *port
            }
```

テスト用に ws だけ落とすヘルパを追加（respawn 検証は「ws 未確立 + 子死亡」の組で ensure を通す必要がある）:

```rust
    /// テスト用: ws だけ捨てる（子は触らない）。次のコマンドで遅延再接続される。
    pub async fn shutdown_ws_for_test(&self) {
        self.conn.lock().await.ws = None;
    }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --test integration spawn_mode_preserves_child_and_respawns_dead_child`
Expected: PASS

- [ ] **Step 5: 全チェック**

Run: `task check`
Expected: PASS

- [ ] **Step 6: コミット**

```bash
git add crates/matd/src/backend.rs crates/matd/tests/integration.rs crates/matd/Cargo.toml
git commit -m "feat(matd): ensure_connectedで死んだ子をrespawn、Spawn温存をテストで担保（#7）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

（`Cargo.toml` は tempfile 追加時のみ。`Cargo.lock` が変わったら含める。）

---

### Task 4: keepalive_tick（Ping 送出 + 受信ドレイン + 切断の先回り検知）

**Files:**
- Modify: `crates/matd/src/backend.rs`
- Test: `crates/matd/tests/integration.rs`

**Interfaces:**
- Consumes: Task 3 の `ws_connected()`。
- Produces: `pub async fn keepalive_tick(&self)`、`pub(crate) const KEEPALIVE_INTERVAL: Duration`（Task 5 が serve に配線）。

- [ ] **Step 1: 失敗テストを書く**

```rust
/// 受信した ws 制御フレーム（Ping）を記録する fake ws。コマンドには通常応答する。
async fn spawn_fake_ws_recording_pings() -> (u16, Arc<tokio::sync::Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let pings: Arc<tokio::sync::Mutex<usize>> = Arc::new(tokio::sync::Mutex::new(0));
    let count = Arc::clone(&pings);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let count = Arc::clone(&count);
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    match msg {
                        Message::Ping(_) => *count.lock().await += 1,
                        Message::Text(line) => {
                            let resp =
                                json!({ "cmd": line, "results": [{ "value": true }], "logs": [] });
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    (port, pings)
}

/// keepalive_tick は matd 側から Ping を送って生存トラフィックを作る
/// （chip-tool の 180 秒無トラフィック PING をそもそも発火させない）。
#[tokio::test]
async fn keepalive_tick_sends_ping() {
    let (port, pings) = spawn_fake_ws_recording_pings().await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_secs(300))
        .await
        .unwrap();

    backend.keepalive_tick().await;
    backend.keepalive_tick().await;

    // fake 側の受信は非同期なので少しだけ待って集計する。
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(*pings.lock().await >= 2, "each tick must emit a ws ping");
    assert!(backend.ws_connected().await);
}

/// keepalive_tick は切断を先回りで検知し、ws を捨てる（子は Task 3 で温存を担保済み）。
/// 次のコマンドは遅延再接続で成功する。
#[tokio::test]
async fn keepalive_tick_detects_close_and_drops_ws() {
    // 応答直後に閉じる fake（Task 1 のものを流用）。
    let port = spawn_fake_ws_close_after_reply().await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_secs(300))
        .await
        .unwrap();
    backend.run_cmdline("onoff on 5 1").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await; // close がワイヤに乗るのを待つ

    backend.keepalive_tick().await;
    assert!(
        !backend.ws_connected().await,
        "keepalive must detect the close and drop the ws"
    );

    // 遅延再接続で次のコマンドは成功。
    let v = backend.run_cmdline("onoff off 5 1").await.unwrap();
    assert!(v["cmd"].as_str().unwrap().contains("onoff off"));
}

/// アイドル中に届いた想定外の遅延応答は keepalive_tick がドレインして捨て、
/// 次のコマンドの応答と混線しない。
#[tokio::test]
async fn keepalive_tick_drains_stale_messages() {
    // 接続直後に応答を勝手に 1 個送る fake。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                let stale = json!({ "cmd": "STALE", "results": [{ "value": false }], "logs": [] });
                ws.send(Message::Text(stale.to_string())).await.unwrap();
                while let Some(Ok(Message::Text(line))) = ws.next().await {
                    let resp = json!({ "cmd": line, "results": [{ "value": true }], "logs": [] });
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            });
        }
    });

    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_secs(300))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await; // stale がバッファに届くのを待つ
    backend.keepalive_tick().await;

    // ドレイン済みなので、次の応答は自分のコマンドのエコーになる。
    let v = backend.run_cmdline("onoff on 5 1").await.unwrap();
    assert!(v["cmd"].as_str().unwrap().contains("onoff on"), "stale response must not leak");
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd --test integration keepalive_tick`
Expected: FAIL（`keepalive_tick` が未定義でコンパイルエラー）

- [ ] **Step 3: 実装**

`backend.rs` に追加:

```rust
/// keepalive の周期。chip-tool（libwebsockets）は最終トラフィックの 180 秒後に
/// 生存確認 PING を送り、20 秒で PONG が無いと切断する（issue #7）。45 秒ごとに
/// こちらから送信トラフィックを作れば、その 180 秒タイマー自体が発火しない。
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(45);

/// keepalive_tick の受信ドレイン待ち時間。静かなら即抜ける正常系。
const KEEPALIVE_DRAIN: Duration = Duration::from_millis(100);
```

impl に追加:

```rust
    /// アイドル中の ws 生存維持。matd から Ping を送って生存トラフィックを作り、
    /// 受信をドレインして切断を先回りで検知する。コマンド実行中（lock 保持中）は
    /// 何もしない — その間 ws は poll されており tungstenite が PING に自動応答する。
    ///
    /// `last_used` は更新しない: keepalive で reap を延命すると chip-tool の
    /// busy-loop（issue #8）が焼き続けてしまう。切断検知時は ws だけ捨てる
    /// （子は温存、次コマンドで遅延再接続）。
    pub async fn keepalive_tick(&self) {
        let Ok(mut conn) = self.conn.try_lock() else {
            return; // コマンド実行中。ws は poll されている。
        };
        let Some(ws) = conn.ws.as_mut() else {
            return;
        };
        if !ping_and_drain(ws).await {
            tracing::info!("ws died while idle; dropping ws for lazy reconnect (child kept)");
            conn.ws = None;
        }
    }
```

自由関数として追加:

```rust
/// Ping を送り、短時間受信をドレインする。ws がまだ生きていれば true。
/// ドレインは (a) サーバ側 Ping への Pong 自動返送を tungstenite に行わせる、
/// (b) アイドル中に届いた想定外の遅延応答を捨てて次コマンドとの混線を断つ、
/// (c) Close/EOF を先回りで検知する、の 3 役。
async fn ping_and_drain(ws: &mut Ws) -> bool {
    if ws.send(Message::Ping(Vec::new())).await.is_err() {
        return false;
    }
    loop {
        match tokio::time::timeout(KEEPALIVE_DRAIN, ws.next()).await {
            Err(_) => return true, // 静か = 正常
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))) => continue,
            Ok(Some(Ok(Message::Text(t)))) => {
                tracing::debug!(%t, "dropped unexpected ws message while idle");
            }
            Ok(Some(Ok(Message::Binary(b)))) => {
                tracing::debug!(len = b.len(), "dropped unexpected ws binary while idle");
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => return false,
        }
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --test integration keepalive_tick`
Expected: PASS（3 テストとも）

- [ ] **Step 5: 全チェック**

Run: `task check`
Expected: PASS

- [ ] **Step 6: コミット**

```bash
git add crates/matd/src/backend.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): keepalive_tick（Ping送出+ドレイン+切断先回り検知）（#7）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: serve への keepalive 配線と「reap を妨げない」担保

**Files:**
- Modify: `crates/matd/src/server.rs`（`serve`）
- Test: `crates/matd/tests/integration.rs`

**Interfaces:**
- Consumes: Task 4 の `keepalive_tick` / `KEEPALIVE_INTERVAL`、既存 `reap_if_idle`。

- [ ] **Step 1: 失敗テストを書く**

keepalive が `last_used` を触らないこと（= reap の焼き有限化を壊さないこと）を検証する。

```rust
/// keepalive は last_used を更新しない: tick を繰り返してもアイドル判定は進み、
/// reap は予定どおりセッションを畳む（#8 の CPU 焼き有限化の前提）。
#[tokio::test]
async fn keepalive_does_not_extend_idle_reap() {
    let port = spawn_fake_ws().await;
    let backend = matd::backend::ChipToolBackend::connect(port, Duration::from_millis(200))
        .await
        .unwrap();
    backend.run_cmdline("onoff on 5 1").await.unwrap();

    // アイドル期間中 keepalive を回し続ける。
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        backend.keepalive_tick().await;
    }

    backend.reap_if_idle().await;
    assert!(
        !backend.ws_connected().await,
        "keepalive must not extend the idle window"
    );
}
```

- [ ] **Step 2: テストを実行**

Run: `cargo test -p matd --test integration keepalive_does_not_extend_idle_reap`
Expected: PASS（Task 4 の実装が正しければ通る。落ちたら `keepalive_tick` が `last_used` を触っている — 実装のバグなので直す）

- [ ] **Step 3: serve に keepalive ループを配線**

`crates/matd/src/server.rs` の reaper ブロックの直後に追加:

```rust
    // アイドル中も ws を生かす keepalive。chip-tool interactive server は 180 秒
    // 無トラフィックで ws PING を送り、20 秒で PONG が無いと切断する（issue #7）。
    // matd はコマンド実行中しか ws を poll しないため、アイドル中はこちらから定期的に
    // 生存トラフィックを作る。reap とは独立（last_used は更新しない）。
    let keepalive = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(crate::backend::KEEPALIVE_INTERVAL).await;
                backend.keepalive_tick().await;
            }
        })
    };
```

graceful shutdown 部（`reaper.abort();` の行）に `keepalive.abort();` を追加:

```rust
    // graceful shutdown: reaper/keepalive を止め、chip-tool セッションを畳み、socket を消す。
    reaper.abort();
    keepalive.abort();
    backend.shutdown().await;
```

- [ ] **Step 4: 全チェック**

Run: `task check`
Expected: PASS（serve 経由の既存統合テストが keepalive 追加後も全部通る = 配線が無害であることの回帰確認）

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): serveにkeepaliveループを配線、reap非延命をテストで固定（#7/#8）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: README 注記・バージョン 0.16.0

**Files:**
- Modify: `README.md`（`### Routing through \`matd\`` セクション）
- Modify: `Cargo.toml`（workspace version）+ `Cargo.lock`

- [ ] **Step 1: README に idle-timeout 運用注記を追加**

`README.md` の「Only one `matd` runs per socket: …」段落（`### Routing through \`matd\`` セクション内）の直後に追加:

```markdown
`matd`'s child `chip-tool interactive server` burns ~100% of one CPU core the
whole time it is alive — a busy-loop in its websocket service loop (upstream
[project-chip/connectedhomeip#29971](https://github.com/project-chip/connectedhomeip/issues/29971),
open since 2023). `matd` keeps the websocket alive with a periodic keepalive
ping and preserves the warm child across transient socket errors, but the only
way to stop the CPU burn is to let the idle reaper kill the child. Keep
`--idle-timeout` moderate (default 300 s; 600–900 s is a reasonable ceiling) —
do **not** raise it to "practically forever" unless you are happy trading a
permanently hot core for never paying a cold start.
```

- [ ] **Step 2: バージョンを 0.16.0 に**

`Cargo.toml`（ワークスペースルート）の `version = "0.15.0"` を `version = "0.16.0"` に変更し、`cargo check` で `Cargo.lock` を更新する。

Run: `cargo check`
Expected: OK（`Cargo.lock` の mat / mat-core / matd のバージョン行が更新される）

- [ ] **Step 3: 全チェック**

Run: `task check`
Expected: PASS

- [ ] **Step 4: コミット**

```bash
git add README.md Cargo.toml Cargo.lock
git commit -m "docs: matd idle-timeout運用注記（上流#29971）、0.16.0

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: 実機デプロイ + E2E + issue 後始末【メインセッションで実施】

**このタスクは subagent に出さない**（実機 jarvis・ネットワーク・GitHub 投稿の判断が要る）。
メインセッションが memory の手順（`jarvis-matd-deploy` / `aarch64-musl rust-lldクロスビルド`）で行う:

- [ ] **Step 1: aarch64-musl クロスビルド → jarvis へ mat/matd 0.16.0 を配布**（`file` で aarch64 を確認してから scp）
- [ ] **Step 2: jarvis の matd 起動引数を `--idle-timeout 900` に変更して再起動**（`matd stop` → 起動。port 9100 孤児に注意: memory `matd-port9100-orphan`）
- [ ] **Step 3: 受け入れ基準の実機確認**（spec の 4 項目）:
  1. 操作 → **4 分放置** → 次の 1 コマンドが一発成功（従来は必ず 1 回失敗）
  2. 放置中の journal に `LWS_CALLBACK_CLOSED` が出ない
  3. 15 分超の放置後に chip-tool プロセスが消えている（`ps`）→ 次コマンドは cold start で成功
  4. `mat group invoke`（matd 経由）で living_lights 7/7 配達
- [ ] **Step 4: issue 後始末**: mat #7 に修正内容とバージョンを追記して close。#8 に「matd 側の緩和（keepalive + reap 運用）済み、根本は上流待ち」を追記して open のまま。上流 connectedhomeip#29971 へのコメント文面（journal の 180.0s/20.0s validity timing、`lws_service(-1)` spin、ps 実測値）をドラフトし、**ユーザー確認後に投稿**
- [ ] **Step 5: memory 更新**（`jarvis-matd-deploy` のバージョン・idle-timeout 記述）

---

## Self-Review（済）

- Spec 全項目とタスクの対応: keepalive（Task 4/5）、温存リカバリ + 透過リトライ + 連続失敗保険（Task 1/2/3）、try_wait respawn（Task 3）、reap 方針 = 運用 + README（Task 6/7）、上流還元・issue 後始末（Task 7）、テスト戦略 6 項目（Task 1〜5 + `task check`）、受け入れ基準（Task 7）。
- 型整合: `ExchangeError`（Task 1 定義、Task 2 消費）、`child_pid`/`ws_connected`（Task 3 定義、Task 3/4/5 消費)、`KEEPALIVE_INTERVAL`（Task 4 定義、Task 5 消費）。
- プレースホルダなし。全ステップに実コード・実コマンド・期待結果を記載。
