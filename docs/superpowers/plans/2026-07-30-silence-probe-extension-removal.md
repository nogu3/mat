# 無音 probe 延長撤去 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd の無音 deadline 到達時の probe 延長（キャップ 2）を撤去し、即 teardown → 再購読に戻す（Issue #15 帰結、spec: `docs/superpowers/specs/2026-07-30-silence-probe-extension-removal-design.md`）。

**Architecture:** probe は 1.6.0 で「偽陽性の計測」として導入され、実測（jarvis journal 3 日分）で「延長後にデバイス発が再開する率 4%/0%、再購読コスト中央値 9s」= 純損失と判明した。matd の pump から probe 分岐を消し、mat-native から `SubscribeConn::probe()` を純減で撤去する。`SILENCE_SLACK`（30s）・born-dead / op 相関・backoff（5s→60s）は無変更。

**Tech Stack:** Rust (tokio, tracing)。テストは `FakeEstablisher`/`FakeSubConn`（`mat-native::test_support`）+ `start_paused` の仮想時計。

## Global Constraints

- コミット前に `task check`（fmt:check + clippy -D warnings + test）を通す（CLAUDE.md）。
- stdout 純 JSON / stderr tracing の規律に触れる変更はない（ログ文言の変更のみ許可範囲）。
- バージョンは workspace `Cargo.toml` の `version = "1.11.0"`（Task 3 で bump）。
- ブランチ: `fix/issue15-silence-probe-removal`（Task 1 冒頭で main から作成）。
- テストの deadline は fake の max_interval=60s + SILENCE_SLACK 30s = **90s**、backoff 初段 5s。

---

### Task 1: matd — pump から probe 分岐を撤去（TDD）

**Files:**
- Modify: `crates/matd/src/subscription.rs`

**Interfaces:**
- Consumes: `FakeEstablisher`（`mat_native::test_support`）— `sub_live`（live レポート注入キュー）、`spawn_manager` テストヘルパ（同ファイル内既存）。
- Produces: pump は `PumpEnd::Silence` で即 teardown し `report pump ended (silence past deadline)` を info ログ。`should_probe` / `SILENCE_PROBE_MAX` は削除され、以後どのタスクからも参照されない。**この時点では `SubscribeConn::probe()` は mat-native に残る（未使用）— 削除は Task 2。**

- [ ] **Step 1: ブランチ作成**

```bash
git checkout -b fix/issue15-silence-probe-removal main
```

- [ ] **Step 2: 失敗するテストを書く**

`crates/matd/src/subscription.rs` のテスト mod にある以下 3 テストを**削除**する（probe 延長という撤去対象の挙動を釘打ちしているため）:

- `silence_probe_extends_twice_then_dies`（1429 行付近）
- `silence_probe_failure_tears_down_at_deadline`（1486 行付近）
- `device_message_resets_probe_budget`（1529 行付近）

同じ場所に、新しい期待を釘打ちするテストを 1 本追加する:

```rust
    /// 生存実績ありの無音は deadline (90s) で即 teardown → backoff 5s で
    /// 再購読する（probe 延長は Issue #15 の実測で「救済 0/18・再購読
    /// 中央値 9s」= 純損失と判明し撤去 — spec 2026-07-30）。
    #[tokio::test(start_paused = true)]
    async fn proven_silence_tears_down_at_deadline() {
        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        // 生存実績を作る（proven=true — born-dead ではなく Silence 経路に
        // 乗せる）。値は priming デフォルト（on-off=true）と揃える: 変える
        // と再確立時の priming が差分回復（`classify_against_cache`）に
        // 昇格して `recovered: true` になり、「本物の再確立」検証を汚す。
        live.lock().unwrap().push_back(onoff_report(1, true));
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();
        assert!(!ev.priming);
        let t0 = tokio::time::Instant::now();

        // 以後完全無音 → deadline (90s) + backoff 5s の範囲で再購読の
        // priming が届く（probe 延長で 270s まで引き延ばさないこと）。
        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("silence teardown 後の再購読 priming")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(90),
            "deadline より早く購読を殺さないこと: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(120),
            "deadline + backoff 5s の範囲で再購読すること: {elapsed:?}"
        );
    }
```

- [ ] **Step 3: テストが失敗することを確認**

Run: `cargo test -p matd proven_silence_tears_down_at_deadline`
Expected: **FAIL** — 現行実装は 90s の deadline で probe が成功して延長するため、120s 以内に再購読 priming が届かず `expect("silence teardown 後の再購読 priming")` で panic する。

- [ ] **Step 4: probe 分岐を撤去する**

`crates/matd/src/subscription.rs` に対して以下を全部行う:

(a) モジュール doc（9 行目）の probe 言及を撤去理由に差し替え:

```rust
// 旧:
//! op 相関 + 無音 deadline = max_interval+30s の死活判定（spec 2026-07-21-matd-borndead-detection。無音は teardown 前に probe で最大 2 回延長 — spec 2026-07-27 無音 probe）。
// 新:
//! op 相関 + 無音 deadline = max_interval+30s の死活判定（spec 2026-07-21-matd-borndead-detection。teardown 前の probe 延長は実測で純損失と判明し撤去 — spec 2026-07-30）。
```

(b) `SILENCE_PROBE_MAX`（doc コメント込み、62-66 行）と `should_probe()`（doc コメント込み、68-73 行）を削除。

(c) pump ループ内（`run_subscription` 相当、530 行付近〜）:

```rust
// 削除: probes_used の宣言とコメント
    // 無音 probe による連続延長回数（デバイス発メッセージ受信でリセット）。
    let mut probes_used: u32 = 0;

// 削除: pump_verdict が Some を返した直後の probe 分岐まるごと
            if should_probe(&end, probes_used) {
                match conn.probe().await {
                    Ok(()) => { /* ... re-arm ... */ }
                    Err(e) => { /* ... probe failed teardown ... */ }
                }
            }

// 変更: 直後のコメントから probe への言及を落とす
// 旧:
            // 再購読直後に同じ pending で即再発火しないよう先に消す
            // （probe 継続時は消さない — op 相関シグナルを保つ）。
// 新:
            // 再購読直後に同じ pending で即再発火しないよう先に消す。

// 変更: Silence アームのログ（probes フィールドを落とし 0.28.0 の文言に戻す）
// 旧:
                PumpEnd::Silence => tracing::info!(
                    node_id,
                    silent_s = last_msg.elapsed().as_secs(),
                    probes = probes_used,
                    "report pump ended (silence past deadline; probe extensions exhausted)"
                ),
// 新:
                PumpEnd::Silence => tracing::info!(
                    node_id,
                    silent_s = last_msg.elapsed().as_secs(),
                    "report pump ended (silence past deadline)"
                ),

// 削除: next_report Ok(Some(msg)) アーム内のリセット行
                probes_used = 0;
```

(d) ユニットテスト `should_probe_only_for_proven_silence_under_cap`（809 行付近）を削除。

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: **PASS**（新テスト含む全テスト。特に既存の born-dead テスト・`backoff_resets_after_successful_establishment` が無風であること）

- [ ] **Step 6: Commit**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): 無音deadlineのprobe延長を撤去 — 実測で救済0/18・純損失（Issue #15）"
```

---

### Task 2: mat-native — `SubscribeConn::probe()` を純減で撤去

**依存: Task 1 完了後**（matd 側の呼び手が消えてから。逆順だと matd がコンパイル不能）。

**Files:**
- Modify: `crates/mat-native/src/lib.rs`
- Modify: `crates/mat-native/src/test_support.rs`

**Interfaces:**
- Consumes: Task 1 の結果（workspace 内に `probe()` / `fail_probe` / `probe_calls` の参照が mat-native 自身以外に存在しないこと）。
- Produces: `SubscribeConn` トレイトは `subscribe_wildcard` / `next_report` のみ。`FakeSubConn` / `FakeEstablisher` から `fail_probe` / `probe_calls` フィールドが消える。

- [ ] **Step 1: 参照が残っていないことを確認（RED 相当の前提確認）**

Run: `grep -rn "\.probe()\|fail_probe\|probe_calls" crates/ --include="*.rs" | grep -v mat-native`
Expected: 0 件（1 件でも出たら Task 1 が未完 — 中断して報告）。

- [ ] **Step 2: mat-native から probe を削除**

(a) `crates/mat-native/src/lib.rs`: `SubscribeConn` トレイトの `probe` 宣言（doc コメント「購読セッションの生存確認。…」込み、178-184 行付近）を削除。

(b) `crates/mat-native/src/lib.rs`: 実装側の `async fn probe(&mut self)`（`ATTR_DATA_MODEL_REVISION` を read する本体、557-570 行付近）を削除。

(c) `crates/mat-native/src/lib.rs`: fake 契約テスト `fake_sub_conn_probe_succeeds_counts_and_fails_when_injected`（doc コメント込み、1079-1092 行付近）を削除。

(d) `crates/mat-native/src/test_support.rs`: 以下を削除 —
- `FakeSubConn` の `fail_probe` / `probe_calls` フィールド（doc コメント込み、33-36 行付近）と `Default` 初期化（74-75 行付近）
- `FakeSubConn` の `async fn probe` 実装（119-126 行付近）
- `FakeEstablisher` の `fail_probe` / `probe_calls` フィールド（doc コメント込み、331-334 行付近）、`Default` 初期化（351-352 行付近）、`establish_subscription` での `Arc::clone` 引き渡し（387-388 行付近）

- [ ] **Step 3: workspace 全体がコンパイル・テスト通過することを確認**

Run: `cargo test --workspace`
Expected: **PASS**（probe 関連の未使用警告・参照エラーゼロ）

- [ ] **Step 4: Commit**

```bash
git add crates/mat-native/src/lib.rs crates/mat-native/src/test_support.rs
git commit -m "refactor(native): SubscribeConn::probe を撤去 — 呼び手消滅の純減（Issue #15）"
```

---

### Task 3: ドキュメント + 1.11.0 bump

**依存: Task 2 完了後。**

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: `Cargo.toml`（workspace version）、`Cargo.lock`（cargo に更新させる）

**Interfaces:**
- Consumes: なし（ドキュメントのみ）。
- Produces: 1.11.0 リリース状態のブランチ。

- [ ] **Step 1: ARCHITECTURE.md に M レコードを追加**

265 行付近のリリース bullet 列（`- **Op budget (deadline), Issue #16.** …` の直後）に追加:

```markdown
- **Silence-probe extension removed (Issue #15 follow-through, 1.11.0).** The
  1.6.0 silence probe doubled as a measurement: over 3 days of production
  journal, ~90% of probes passed (the CASE session was alive at the silence
  deadline) but device messages resumed after an extension in only 4/108
  (later window: 0/18) — once a node is silent past max_interval + 30s it
  stays silent (device-side silent discard, or keep-alive-less firmware),
  while re-subscribing after teardown costs a median 9s (p90 34s, n=104). The
  probe extension therefore only delayed dead-subscription detection 330s →
  up to 990s and rescued nothing: `SubscribeConn::probe()` and the extension
  cap are removed, silence deadline tears down immediately again, and raising
  `SILENCE_SLACK` (the issue's original hypothesis) is rejected by the same
  data. Design: `docs/superpowers/specs/2026-07-30-silence-probe-extension-removal-design.md`.
```

- [ ] **Step 2: ARCHITECTURE.md の購読節の stale 記述を現実装へ是正**

988-992 行付近（「購読パラメータ」bullet の末尾）:

```markdown
（旧）
  掃除）。失敗・死亡（MaxInterval の 1.5 倍を超える無音）時は指数 backoff
  （5s 開始、上限 5min）で再購読。リトライは debug ログ、確立/喪失の状態遷移
  のみ info。
（新）
  掃除）。失敗・死亡（max_interval + 30s の無音 deadline — 0.28.0 で
  ×1.5 を置換）時は指数 backoff（5s 開始、上限 60s — 1.6.0 で 5min から
  短縮、issue #15）で再購読。リトライは debug ログ、確立/喪失の状態遷移
  のみ info。
```

- [ ] **Step 3: バージョン bump**

`Cargo.toml` の `[workspace.package]` `version = "1.10.0"` → `"1.11.0"`。

Run: `cargo check -q`（Cargo.lock の追随を生成させる）

- [ ] **Step 4: CI 相当を通す**

Run: `task check`
Expected: fmt:check / clippy(-D warnings) / test 全部 PASS

- [ ] **Step 5: Commit**

```bash
git add ARCHITECTURE.md Cargo.toml Cargo.lock
git commit -m "chore: 1.11.0（無音 probe 延長撤去 — Issue #15 帰結）+ ARCHITECTURE"
```

---

### Task 4: 実機 E2E（マージ前・メインセッションが実施 — subagent には出さない）

隔離 matd 方式（[[jarvis-matd-deploy]] の 1.5.0/1.6.0 節の確立手順）。probe 撤去の検証は
1.6.0 E2E の裏返し: **「確立 → state-change op で proven 化 → 本番との追い出し合戦で
無音化」で `Silence` 経路を誘発**し、probe ログ無しで 330s teardown → 数秒再購読を見る。

- [ ] `task dist:arm64` でビルド、`scp dist/arm64/{mat,matd} jarvis:~/mat.new` 系で転送（`*.new`、本番未置換）
- [ ] jarvis 上で隔離環境を作る: `~/.config/mat` を `/tmp/mat-e2e-<date>/store` へコピーし、`nodes.json` を 1 ノード（node6 等ライト 1 台）に絞る
- [ ] 隔離 matd 起動（別 socket、ログはファイル直書き — journald 押し流し対策）: `setsid` + `--store <copy> --socket /tmp/mat-e2e-<date>/t.sock`、`MAT_MATD_IFACE`/`MAT_MATD_FABRIC_INDEX=2` は本番 unit と同値
- [ ] `subscription established` 確認 → 隔離 socket 経由で state-change op（`mat --matd <t.sock> on/off`）を 1 発 → live イベント受信 = proven 化
- [ ] 本番 matd との追い出し合戦で無音化 → **期待ログ: `report pump ended (silence past deadline) ... silent_s=330`（`probe` 文字列がログ全体に一切出ない）→ backoff 5s 内で再購読試行**
- [ ] 無回帰確認: born-dead 検知（`born-dead:` ログ）・op 相関（`op-correlated:`）・STUCK warn が従来どおり出る/出ないの確認（合戦環境では born-dead が自然に出る — 1.6.0 E2E 知見）
- [ ] 後片付け: 隔離 matd 停止（`pkill -f "matd[.]new"` ブラケット、平文でバイナリ名を echo しない）、`/tmp` の store コピー削除（認証情報）、本番購読が node6 で自己回復したことを journal で確認

- [ ] E2E 合格を記録（結果はマージコミット or メモリへ）

---

### Task 5: マージ → デプロイ → issue クローズ(メインセッションが実施)

- [ ] superpowers:finishing-a-development-branch で main へマージ(--no-ff)・push
- [ ] despliegue skill で jarvis 本番へ 1.11.0 デプロイ（`*.new` 昇格: backup `.bak-1.10.0` → `install -m755` → `systemctl --user restart matd`）
- [ ] デプロイ後スモーク: 再起動 ~3 分で購読確立（`ss -uanp` の matd UDP ソケット数 ≈ 購読数+warm op 数+2）、warm read 2-3 ノード exit0、`journalctl` に `probe` 文字列が出ないこと、`silence past deadline` teardown が発生したら silent_s≈330 + 直後 `subscription established` の down_s が数秒〜数十秒であること（自然発生 ~17 件/13h なので 1-2 時間以内に観測可能）
- [ ] Issue #15 へ実測サマリをコメントしてクローズ: probe pass 90% だが延長後再開 4%/0%、再購読中央値 9s、SLACK 引き上げ棄却の根拠、1.6.0（probe+backoff60s）+ 1.11.0（延長撤去）で blind 3.7h/日 → 実測値を添付
- [ ] メモリ更新: [[jarvis-matd-deploy]] に 1.11.0 節、[[mat-stability-audit-backlog]]/[[matd-subscribe-listen]] の関連記述追随
