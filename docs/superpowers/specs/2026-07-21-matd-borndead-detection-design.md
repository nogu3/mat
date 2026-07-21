# matd born-dead 購読の高速検知（op 相関 + 受動 deadline 短縮）設計

- 日付: 2026-07-21
- 対象: `matd`（subscription manager / server op 経路）、`mat-controller::session`、`mat-native`
- 前提 spec: `2026-07-20-matd-subscribe-listen-design.md`、`2026-07-21-matd-subscribe-priming-weight-fix-design.md`

## 背景と実測エビデンス（2026-07-21 夜）

0.26.0/0.27.0 の夜間 listen E2E 全滅（exit 3）の調査で、以下を実機確定した。

1. **450s 無音検知は正常動作している**。node6 で established 18:47:42 →
   ちょうど 450 秒後の 18:55:12 に pump timeout 発火 → 7 秒で再確立
   （attempts=1）。前夜の「2h 放置でも再確立ログゼロ」は journald
   volatile 消失による観測アーティファクト。
2. **born-dead の実態はワイヤ完全無音**。born-dead 中の購読ソケット
   （node6, port 39810）を tcpdump した 5.5 分（`mat on` トグルと
   keep-alive 期待時刻を含む）で双方向 0 パケット。priming までは届き、
   その後デバイス発（keep-alive 含む）が一切無い。
3. **故障モデル**: 弱 Thread リンク（node6 = RF 20〜47% loss、夜間悪化）で
   デバイスの最初のレポート/keep-alive 送信の MRP 再送が全滅 →
   デバイスは購読を黙って破棄（既知の silent discard と同族）。
   priming 成立直後は「リンクが通っていた瞬間」なので、確立は成功し続ける
   （attempts=1）のに直後に死ぬ、を夜間は延々引き直す。
4. **問題の本質は検知不能ではなく盲目窓**: 1 ロールあたり最長 450s 盲目で、
   夜間はこれが連鎖して listen が実質死ぬ。コマンド経路（op）は同時刻も正常。
5. 付随バグ: 無音タイムアウトの pump 終了ログが
   `no acknowledgement within MRP retry budget`（MRP 送信失敗の文言）と出る。
   `SessionError::Timeout` の Display が送信失敗と共用のため
   （`session.rs` の Display impl）。切り分けを混乱させた。

## ゴール

- 照明系（matd 経由 op がある）: born-dead を **op 後数秒〜グレース(10s)** で
  検知し即再購読。再購読 priming が変更後状態を運ぶため、失われたイベントも
  `priming: true` として遅延配信され、`mat listen` 既定 60s timeout に収まる
  （op → ≤10s 検知 → ~7s 再確立 → priming 配信）。
- センサー系（デバイス発のみ、相関 op 無し）: 受動の無音 deadline を
  450s → **max_interval + 30s（=330s）** に短縮。
- 観測性: 無音死・born-dead 死・MRP 送信失敗死をログで区別可能にする。

## 非ゴール

- ワイヤ上の購読交渉パラメータ（MinIntervalFloor=0 /
  MaxIntervalCeiling=300s / KeepSubscriptions=false）は変更しない。
  ceiling 短縮（keep-alive 高頻度化）は本設計のセンサー検知 330s で
  不足と実測されてからの後続候補（電池・メッシュ負荷コストがあるため）。
- mat 側 CLI・JSON スキーマ・`subscriptions.toml` 形式は変更しない。
- デバイス側の silent discard 自体は直せない（デバイス実装の挙動）。
- group op・直経路 op からの検知（下記トレードオフ参照）。

## 設計

### §1 op 相関検知（born-dead の数秒検知）

matd 内に共有の購読ヘルス表 `SubHealth` を新設する（`subscription.rs`）:

```rust
// Arc で server と全 pump に共有。ephemeral なランタイム状態
// （設計ルール4の「状態」= 永続状態には該当しない）。
struct SubHealth {
    // 購読対象クラスタ集合（subscriptions.toml 由来。空 = full wildcard = 全許可）
    clusters: Arc<[u32]>,
    inner: Mutex<HashMap<u64 /* node_id */, NodeSubHealth>>,
}
struct NodeSubHealth {
    last_device_msg: Option<Instant>, // keep-alive 含むデバイス発の最終受信
    pending_op: Option<Instant>,      // 未消化の状態変更 op の時刻
}
```

- **server 側**（`server.rs` の native op 経路）: 状態変更 op
  （on/off/level/color-temp/color/write/invoke）が **success** で返り、
  対象クラスタが購読対象なら `note_op(node_id)` で `pending_op = now` を記録。
  read/describe は対象外。
- **pump 側**（`run_subscription_once`）: `next_report` の待ち時間を
  **5 秒スライス**に刻む（`next_report(min(5s, remaining))`）。
  `tokio::select!` を使わないのは next_report が recv → screen →
  StatusResponse 応答の多段 await で cancel-safe でないため（スライス方式なら
  report 1 通の取り扱いは現行コードのまま）。各スライス後に判定:
  1. デバイス発メッセージ受信（keep-alive 含む）→ `last_device_msg = now`。
     受信が `pending_op` より後なら `pending_op` を解除
     （keep-alive でも解除 = 経路生存の証明とみなし再購読しない）。
  2. `pending_op` から **グレース 10s** 経過してもデバイス発ゼロ →
     pump を抜けて再購読へ（born-dead 検知。backoff は従来の喪失時と
     同じくリセットされ、初期値 5s 待って再購読 — 検知から再確立まで
     実質 10〜20 秒）。
  3. 従来の無音 deadline（§2）超過 → 従来どおり再購読。

**トレードオフ（許容と根拠）**:
- 状態が変わらない op（既に on のライトへ on）はレポートが出ず、健全な購読を
  1 回無駄に再購読する。実 op はほぼ状態変更であり、再購読は軽い
  （priming 4 チャンク・~7s、0.26.0 の軽量化済み）ため許容。
- group op はノード特定不能、直経路 op は matd から見えないため対象外
  （README/ARCHITECTURE には書かない — matd 内部挙動であり契約ではない）。
- 購読タスクが無いノードへの `note_op` は no-op。

### §2 受動検知の高速化（センサー系）

`DEATH_FACTOR = 1.5`（=450s）を廃止し、
**deadline = デバイス選択 max_interval + `SILENCE_SLACK`(30s)**（=330s）へ。
仕様上デバイスは max_interval までに必ず report か keep-alive を送る義務が
あり、30s は MRP 再送とジッタの余裕。下限 `max(…, 5s)` は維持。

### §3 観測性修正

- `mat-controller::session` に **`SessionError::Silence` variant を分離**。
  `next_subscription_report` の期限切れはこちらを返す（Display:
  `no device-initiated message within the subscription deadline`）。
  既存 `SessionError::Timeout`（MRP 送信 ack 切れ）の文言・用途は不変。
  MatError への写像は従来どおり kind=Timeout（exit code 契約に変更なし）。
- pump 終了ログで死因を区別:
  - establishment 以降デバイス発ゼロの無音死 → `report pump ended (born-dead)`
  - op 相関検知 → 専用理由（op からの経過秒を添える）
  - 通常無音（生存実績ありの無音死）/ MRP 送信失敗 → それぞれ現行 detail で区別可
- `subscription established` の `down_s`/`attempts` は現状維持。

### §4 テスト

- ユニット（`matd::subscription` + FakeEstablisher 拡張）:
  - 「priming 後に沈黙する conn」で op 通知 → グレース内に pump が抜けて
    再確立される（established 2 回目を観測）。
  - keep-alive 受信が `pending_op` を解除し再購読しないこと。
  - 330s deadline 計算（max_interval + 30s、下限 5s）。
  - `SessionError::Silence` の文言と、無音死ログの born-dead 判定。
  - 判定ロジックは純関数に切り出してテスト（`classify_failure` と同じ規律）。
- 既存テストは無改変で通ること（`task check`）。
- 実機 E2E（デプロイ後の受け入れ基準）:
  1. born-dead 時間帯（夜）に node6 で `mat off` → `mat listen` が
     priming 経由で 60s 内に受信・exit 0。ログに born-dead 検知 →
     再確立の連鎖が出る。
  2. 落ち着いた時間帯の通常 listen E2E 再確認（born-alive 経路の無回帰）。
  3. 健全ノード（node8 等）で従来どおり ~数百 ms のイベント配信。

## 変更ファイル見込み

- `crates/matd/src/subscription.rs`: SubHealth、pump スライス化、判定純関数、
  `DEATH_FACTOR` → `SILENCE_SLACK`（deadline 計算はここにある）
- `crates/matd/src/server.rs`: op success 時の `note_op` 配線
- `crates/mat-controller/src/session.rs`: `SessionError::Silence` 分離
- `crates/mat-native/src/lib.rs`: SubscribeConn 実装の error 写像（Silence →
  MatError kind=Timeout の detail 維持）
- `crates/mat-native/src/test_support.rs`: FakeEstablisher の沈黙モード
