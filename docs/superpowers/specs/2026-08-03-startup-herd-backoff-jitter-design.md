# 起動 herd の stagger と backoff / MRP の jitter（安定性監査 Tier 2 ⑧）

日付: 2026-08-03 / 対象バージョン: 1.18.0

## 背景 / 問題

同期バーストを生む機構が 3 つあり、いずれも「タイミングを散らす」一点で潰せる。

1. **起動 herd** — `matd/src/subscription.rs` の supervisor は初回台帳スキャンで
   全ノード（本番 13 台）の購読ループを同一ティックで一斉 spawn する。各ループは
   即座に mDNS resolve → CASE → wildcard Subscribe(priming) に入るため、デプロイ
   再起動のたびに Thread メッシュへ CASE バーストがかかり、BR 無線の CCA 失敗
   → no-ack が 1〜2 分続く。**op ログ 1 週間観測（2026-08-03）で唯一の再現性ある
   実 symptom**（全リリースのスモークで既知として毎回観測）。
2. **backoff の同期** — `next_backoff` は決定論的な 5→10→20→40→60s。メッシュ
   全域イベントで 13 ノードが同時に失敗すると、以後のリトライ波も全ノード同時刻に
   同期したまま繰り返される（2026-07-23 のメッシュ全域遅延で実証された
   「購読チャーン正帰還」の増幅要素）。
3. **MRP 再送 jitter の欠如** — 再送間隔は `interval × 1.6^n` の決定論。
   Matter spec 4.12.2.1 は各再送待ちに `× (1 + random(0,1) · MRP_BACKOFF_JITTER)`
   （JITTER = 0.25）を要求しており、現実装はこれを省略している（spec 逸脱）。

BACKOFF_MAX の 300s→60s 短縮（1.6.0）は本監査項目の前半で、jitter が未実施の
まま残っていた。

## 決定

3 点すべてに jitter / stagger を入れる（A 案）。設計変更は無く、既存ループの
「待ち時間の算出」だけを変える。

### 1. 乱数ヘルパー（mat-controller）

- `pub fn unit_random() -> f64`（[0,1) 一様）を mat-controller に新設。
  `getrandom` 8 バイト → u64 → スケール。**getrandom 失敗時は 0.5 に退避**
  （panic させない — jitter は品質であって正しさではない）。
- jitter の適用は乱数値を引数に取る**純関数**にし、`next_backoff` /
  `pump_verdict` と同じ規律でユニットテストする。matd は mat-controller 依存
  越しに import する（mat-controller は mat-core に依存しないため、共有の置き場
  は mat-controller 側）。

### 2. 起動 herd の stagger（matd）

- supervisor の同一ティックで**複数ノード**を spawn するとき、バッチ内
  index × **1s** の初期遅延を購読ループへ渡し、最初の確立試行前に sleep する。
  13 台なら開始が 0〜12s に均等分散。
- rescan で 1 台だけ増えた場合は遅延ゼロ（現行どおり即購読）。
- 乱数でなく決定論的 index stagger を選ぶ理由: herd は単一プロセス内の現象で、
  乱数より均等間隔のほうが厳密に非衝突。テストも単純になる。

### 3. 再購読 backoff の jitter（matd）

- `next_backoff` は決定論のまま（名目エンベロープ 5→10→…→60s、既存テスト不変）。
- sleep 直前に **cap 適用後の名目値 × [0.75, 1.25)** を掛ける。cap 後に掛ける
  ので、長期障害で全ノードが 60s に飽和しても実待ちは 45〜75s に散り続け、
  再同期しない（cap 前に掛けると飽和ノードが全員ちょうど 60s で再同期する）。
- ×[0.75, 1.25) は中央値を名目値に保つ — 観測済みの設計軌道（再確立 down_s
  中央値 7-9s）を変えない。full jitter（rand(0..backoff)）は中央値が半減する
  ため不採用。
- `mark_down` / `matd status` の backoff 表示は名目値のまま（表示は
  エンベロープの説明であって実 sleep の予告ではない、と定義する）。

### 4. MRP 再送 jitter（mat-controller）

- spec 4.12.2.1 準拠: 各再送の待ち interval に `× (1 + 0.25 · r)`、r は
  **再送のたびに引き直す**。決定論的な `× 1.6^n` の進行はそのまま、sleep に
  使う値だけ jitter を掛ける。
- 適用は再送ループ**3 箇所すべて**: `exchange.rs` `send_reliable`（unsecured）、
  `session.rs` の secure 送信ループ 2 箇所（:464 / :995 相当）。
  sibling 関数の全数適用（mDNS QU ビットバグで「片方だけ直す」を踏んだ教訓）。
- `total_budget` は各項 × 1.25 の**最悪値**へ更新 — Issue #16 の op 予算・
  BTP の recv 予算が実待ちより短くなって早切れしないようにする（予算が緩む
  方向なので安全側）。`session.rs` の重複 local `total_budget` は
  `exchange::total_budget` の呼び出しに統合する。
  `worst_case_send_budget` のコメントの概算値（≈14.74s）と matd `native.rs`
  の参照コメントも追随して更新する。

### 却下した代替案

- **semaphore で同時確立数を制限** — 共有状態が増え rescan 経路との絡みも出る。
  「設計変更不要・待ち時間の算出だけ変える」という本監査項目の枠を超える。
- **full jitter（rand(0..backoff)）** — 分散は最大だが中央値が半減し、観測済みの
  設計軌道が変わる。±25% で 13 台の分散には十分。
- **起動 stagger を乱数化** — 単一プロセス内では index 均等が厳密に非衝突で優る。

## 実装

### crates/mat-controller

1. `unit_random()` 新設（getrandom、失敗時 0.5）。
2. jitter 純関数（例: `jittered(interval, r)` = `interval × (1 + 0.25·r)`）を
   `exchange.rs` に置き、再送ループ 3 箇所の sleep 値に適用。
3. `total_budget` を × 1.25 込みに変更、`session.rs` の重複を統合、
   コメントの概算値を更新。

### crates/matd

4. 購読ループに `initial_delay: Duration` を追加し、supervisor がバッチ内
   index × 1s を渡す（バッチサイズ 1 は ZERO）。ループ先頭で sleep。
5. backoff sleep 箇所で `nominal × (0.75 + 0.5·r)` を計算して sleep に使う
   （純関数 + `unit_random()`）。`tokio::select!` の touch_notify 短絡起床は
   現行のまま。

## テスト

- unit（純関数）: backoff jitter の範囲（0.75x ≤ 実待ち < 1.25x、cap 後適用、
  r=0 / r→1 の端点）、MRP jitter の範囲、stagger 遅延の算出（バッチ 1 = ZERO、
  バッチ n = index×1s）、`total_budget` の 1.25 係数。
- 既存回帰: `task check`（`next_backoff` / SubHealth / pump の既存テストは不変の
  はず — 名目エンベロープに触らないため)。
- **実機 E2E（マージ前必須）**: jarvis へ `*.new` デプロイ → 再起動直後の
  journal で (1) CASE no-ack バーストの長さと attempts 分布を現行（毎回 1-2 分）
  と比較、(2) `matd status` で全ノード established 到達を確認（最終ノードの
  到達は stagger 分（≤12s）遅くなるが、バースト輻輳の解消で総所要は悪化しない
  こと)、(3) 定常運転で silence 死からの再確立が設計軌道（attempts=1 /
  down_s 中央値 7-9s 近傍）を保つこと。

## リスク / 非目標

- 起動時の「全ノード established」到達は最後のノードで最大 +12s（13 台時)。
  現状はバースト輻輳で 1〜2 分乱れるため正味では改善見込み — 実機 E2E で確認。
- mat CLI 直経路にも MRP jitter が乗る（spec 準拠化)。op の最悪待ちは再送毎に
  +0〜25% 伸びるが、`total_budget` の更新で予算との整合は保たれる。
- 非目標: ⑤ mDNS AAAA 鮮度、⑨ IM デコード誤分類、⑩ probe 3 秒窓（それぞれ
  別項目）。commission / PASE 経路は unsecured exchange の `send_reliable` を
  通るため自動的に jitter が乗る — 個別対応はしない。
