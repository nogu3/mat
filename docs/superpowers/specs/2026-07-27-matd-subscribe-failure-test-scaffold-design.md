# matd 購読の失敗経路テスト足場（安定性監査 Tier 2 ⑪）

- 日付: 2026-07-27
- 対象: `crates/mat-native/src/test_support.rs`, `crates/matd/src/subscription.rs`
- 出自: 2026-07-25 の安定性監査バックログ Tier 2 ⑪
  「再確立ラダーと無音 deadline のテスト足場が無い」

## 背景と問題

`matd` の常駐購読は「確立 → priming 配信 → pump → 死んだら backoff 再購読」の
ループで動く（`node_subscription_loop` / `run_subscription_once`）。この**失敗側**
の挙動が、統合テストで一度も通されていない。

現行の fake が失敗を表現できないためである:

- `FakeEstablisher::establish_subscription` は必ず `Ok` を返す。
- `FakeSubConn::subscribe_wildcard` も必ず `Ok`。
- `FakeSubConn::next_report` は `Ok(Some)` か、無音で `Ok(None)` のみ。`Err`
  （セッション死）を返す経路が無い。

`subscribe_wildcard` には別途の失敗注入ノブを足していない。`node_subscription_loop`
から見れば `subscribe_wildcard` のエラーは `establish_subscription` のエラーと
区別がつかない（どちらも確立前に起きる同じ失敗経路で、`failures` を進めて同じ
ラダーを登る）ため、`fail_subscription` が両方をまとめて代表できる。専用ノブが
無いのは見落としではなく、この理由による意図的な非対応。

結果、次が単体テスト（純関数）だけで担保され、ループを回した検証がゼロになって
いる:

| 経路 | 現状 |
|---|---|
| 指数 backoff ラダー（5s → 10s → 20s → … 上限 5min） | `next_backoff` の純関数テストのみ |
| `classify_failure` の実配線（First / StuckWarn / Quiet） | 純関数テストのみ |
| 確立成功時の `backoff` / `failures` / `warned` リセット | 未テスト |
| 無音 deadline 到達（`PumpEnd::BornDeadSilence` / `Silence`） | `pump_verdict` の純関数テストのみ |
| pump の `Err` 分岐（セッション死 → 再購読） | 未テスト |

既存の統合テスト 7 本のうち 5 本は「op 相関（`SubHealth` の pending）で殺す」
経路しか通っていない（残り 2 本 `manager_emits_priming_events_from_fake_subscription`
/ `manager_passes_clusters_to_subscribe` はそもそも何も殺さない）。実機で最も
頻繁に踏まれるのは弱リンクノードの**確立失敗ラダー**と**無音死**なので、そこが
素通しなのは監査で最も分の悪い抜けだった。

## スコープ

**テスト足場の追加のみ。`subscription.rs` のプロダクション挙動は 1 行も変えない。**

新しい欠陥の発見が目的ではなく、既存の正しい挙動を釘打ちして今後の回帰を防ぐの
が目的。バックログの他項目（#1 レポート破棄、#3 UDP ソケット共有、#4 台帳再読）
はこの spec の対象外。

## 設計

### ① fake に失敗を表現させる（`mat-native/src/test_support.rs`）

`FakeEstablisher` に 2 フィールドを足す。どちらも**残り失敗回数**のカウンタで、
既定 `0` = 常に成功。よって既存テストの挙動は不変。

```rust
pub struct FakeEstablisher {
    // ...既存フィールド...
    /// establish_subscription を残り回数だけ `fail_kind` で失敗させる（0 = 常に成功）。
    pub fail_subscription: Arc<AtomicUsize>,
    /// 確立後の next_report を残り回数だけ SessionFailed で失敗させる（0 = 常に成功）。
    /// 払い出す FakeSubConn と Arc で共有する。
    pub fail_next_report: Arc<AtomicUsize>,
}
```

設計上の判断:

- **bool ではなくカウンタ**。backoff ラダーは「N 回失敗してから成功」でしか回ら
  ない。ラダーの段数をテストが選べる必要がある。
- **`Arc` 共有**。`fail_next_report` は*確立後に*注入する必要がある（pump を狙って
  殺すため）。既存の `sub_live` / `sub_clusters` と同じパターンで、
  `establish_subscription` が払い出す `FakeSubConn` に `Arc::clone` を渡す。
- **失敗も `calls` に数える**。`calls` は「試行回数」を意味することにする。
  テストが `calls == 4` で「3 回失敗 + 1 回成功」を主張できる。
- **エラー種別**: 確立失敗は既存の `fail_kind`（既定 `Timeout`）を流用。pump の
  失敗は `ErrorKind::SessionFailed` 固定 — 実機でセッションが落ちる形に対応し、
  フィールドを増やさない。

減算は小さなヘルパ 1 本（`fetch_update` で 0 なら `None`）:

```rust
/// カウンタが正なら 1 減らして true（= この呼び出しは失敗させる）。
fn take_failure(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}
```

`FakeSubConn` には `fail_next_report: Arc<AtomicUsize>`（`Default` は 0）を持たせ、
`next_report` の先頭で `take_failure` を見る。

### ② テスト 4 本（`matd/src/subscription.rs` の `mod tests`）

全て `#[tokio::test(start_paused = true)]`。`FakeSubConn.max_interval_s = 60` なの
で無音 deadline は 90s。仮想時計なので実時間の待ちは発生しない。

**1. `establish_failures_climb_backoff_then_recover`**
`fail_subscription = 3`。確立が 3 回失敗したあと 4 回目で成功する。
- priming 到達までの経過が `>= 35s`（5+10+20 のラダーを実際に登った証明）
- かつ `< 40s`（5+10+20 = 35s のラダーを登り終わり、40s 段目の backoff には入らない）
- `calls == 4`

**2. `pump_session_error_resubscribes_without_waiting_deadline`**
確立後に `fail_next_report = 1` を注入し、pump をセッションエラーで殺す。
- 再 priming までの経過が `< 20s`（無音 deadline 90s を待っていない）
- `run_subscription_once` の `Err` 分岐が `Ok(())` を返し、ループが「購読喪失」
  として backoff 5s で再購読する釘打ち

**3. `silent_subscription_dies_at_deadline_and_resubscribes`**
確立後、live report を一切入れない（完全無音）。
- 再 priming までの経過が `>= 90s`（deadline より早く殺さない）
- かつ `< 120s`（deadline + backoff 5s + スライス誤差の範囲で再購読する）
- 完全無音の購読が 90s deadline で殺されて再購読される（バリアント選択は純関数テスト `pump_verdict_prioritizes_op_grace_then_silence` で固定）

**4. `backoff_resets_after_successful_establishment`**
`fail_subscription = 3` でラダーを 20s まで育ててから確立させ、その後
`fail_next_report = 1` で pump を殺す。
- 殺してから再 priming までの経過が `< 15s`（リセットされていれば 5s、されて
  いなければ 40s なので明確に区別できる）
- `node_subscription_loop` の `backoff = Duration::ZERO` リセットが実配線されて
  いる釘打ち（同時にリセットされる `failures = 0` / `warned = false` はログ
  レベルにしか影響せず、この統合テストでは観測できないため対象外）

### ③ テストヘルパでコピペを畳む

既存の統合テスト 7 本は全て次の 20 行を丸ごとコピペしている:

```rust
let dir = tempfile::tempdir().unwrap();
let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
store.upsert_node(mat_core::store::NodeRecord { node_id: 5, ... }).unwrap();
let native = crate::native::NativeBackend::with_establisher(Box::new(est));
let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
let (tx, mut rx) = tokio::sync::broadcast::channel(64);
let health = std::sync::Arc::new(SubHealth::new(None));
let _handles = spawn_subscription_manager(state, dir.path().to_path_buf(), tx, None, health);
```

新規 4 本でこれが更に 80 行増えるので、`mod tests` 内にローカルヘルパを 1 本切り、
**既存 7 本もそれに寄せる**（触っている場所を整えるのが妥当な範囲。同じ形が 2 種類
並ぶのを避ける）。

```rust
/// node 5 だけの台帳 + fake establisher で購読マネージャを起動する。
/// 戻り値の `TempDir` はテストが生かし続ける必要がある（drop で store が消える）。
fn spawn_manager(
    est: FakeEstablisher,
    clusters: Option<Vec<u32>>,
) -> (
    broadcast::Receiver<Event>,
    Arc<SubHealth>,
    tempfile::TempDir,
    Vec<tokio::task::JoinHandle<()>>,
)
```

- `clusters` を引数に残すのは `manager_passes_clusters_to_subscribe` が使うため。
- `SubHealth` を返すのは op 相関テスト群が `note_op` を打つため。
- `JoinHandle` を返すのは既存テストが `_handles` で寿命を握っているのと同じ理由
  （drop するとタスクが即終了するわけではないが、意図を明示する既存の書き方を保つ）。
- `FakeEstablisher` を値で受け取り、テスト側は事前に `Arc::clone` で
  `sub_live` / `sub_clusters` / `fail_*` を握ってから渡す（既存テストと同じ順序）。

移行は機械的な置換で、各テストの**主張部分は一切変えない**。

## テスト方針

新規 4 本は上記のとおり。加えて既存テスト全数が移行後も通ることを、`task check`
（fmt:check + clippy -D warnings + test）で確認する。実機 E2E は不要
（プロダクション挙動が変わらないため）。

失敗時の診断性のため、各 assert のメッセージには実測経過（`{elapsed:?}`）を必ず
載せる — 既存の op 相関テストと同じ規律。

## 非目標

- 新しい欠陥の修正（Tier 2 の他項目 ⑤〜⑩、Tier 1 の #1/#3/#4）
- `matd status` op の新設（別途）
- 実 CASE / 実 mDNS を使ったテスト（fake の範囲で完結させる）
- `STUCK_WARN_AFTER`（600s）到達の統合テスト。1.5.0 で同ファイルにトレース
  キャプチャの足場（`Buf` + `MakeWriter`、
  `classify_promotion_emits_info_log_with_old_and_new_values`）が入ったので
  技術的な障壁は無くなったが、`classify_failure` の純関数テストで論理は担保
  済みであり今回のスコープには含めない。将来足すなら安い。
