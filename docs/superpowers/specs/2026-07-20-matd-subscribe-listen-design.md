# matd 常駐 Subscribe + mat listen（センサー対応）

日付: 2026-07-20 / 対象: `mat-controller`(im/session) + `matd`(購読管理・socket) + `mat`(listen)

## 目的

人感(OccupancySensing)・開閉(BooleanState)・温湿度などの計測系センサーを
mat 系だけで扱えるようにし、HA 依存を減らす（脱HAの一段）。Matter の
Subscribe/Report 受信を実装し、デバイス発の状態変化をイベントとして
外部（casa）へ渡す。

- オートメーション（「人感→ライトON」のルール実行）は **casa の責務**。
  mat/matd はイベント配信まで（設計ルール「scenes, automation はスコープ外」不変）。
- CLI の UX は `enl listen` を踏襲する（`--count`/`--timeout-ms`/フィルタ、
  0件 timeout = exit 3、外側ループはコンシューマが回す）。

## 決定（ユーザー承認 2026-07-20）

- **購読の主体は matd**（常駐購読）。`mat listen` は matd に接続して
  イベントを受けるだけの薄い口。listen は初の **matd 専用 op**（direct
  fallback なし — 常駐なしに購読は成立しない）。
- **v1 は commissioned 全ノードへ自動 wildcard 購読**。将来
  `<store>/subscriptions.toml` で対象絞り込み等を可能にする
  （無ければ全ノード = v1 既定。aliases.toml と同じ「無ければ既定動作」規律）。
- **v1 スコープ = attribute report のみ**。EventReport 受信（Generic Switch
  等のボタン）、DataVersionFilter、LIT ICD の check-in 登録はスコープ外
  （対象は常時給電 + SIT sleepy まで）。

## ① mat-controller 層

### im.rs

- SubscribeRequest(opcode 0x03) / SubscribeResponse(0x04) の TLV
  encode/decode を追加。
- AttributePathIB の endpoint/cluster/attribute を Option 化し、全省略 =
  wildcard を表現（既存の単一パス read と共用）。EventRequests は常に空。
- ReportData デコードを拡張: 複数 AttributeReportIB、`SubscriptionId`、
  `MoreChunkedMessages`（priming report の分割）、空 report(keep-alive)。

### session.rs — 購読ポンプ

長寿命ループとして追加:

1. SubscribeRequest 送信 → priming ReportData（分割対応、各チャンクに
   StatusResponse 応答）→ SubscribeResponse 受信で購読成立
   （SubscriptionId とデバイス選択の MaxInterval を得る）。
2. 以後 recv ループ: デバイス発の新 exchange で届く ReportData を
   MRP ack + StatusResponse で受け、デコード済み report を channel へ流す。
   空 report は keep-alive として消費。
3. MaxInterval の 1.5 倍を超えて無音なら購読死亡としてループを抜け、
   上位（matd）が再購読する。

### 構造判断: 購読は専用ソケット + 専用 CASE セッション

現行 `SecureSession` は request-response 前提で、同一 UDP ソケットを op と
ポンプが同時に recv すると相手のメッセージを吸って壊れる。demux（単一 recv
ループ + session/exchange id ルーティング）への全面改修は **やらない**。
ノードごとに購読専用の `UdpTransport` + CASE を別に確立し、ポンプが独占する。
既存 op 経路（warm session）は不変。コストは購読ノードあたり CASE 1本。

### 購読パラメータ

- MinIntervalFloor = 0（人感の即応性優先）
- MaxIntervalCeiling = 3600s（sleepy の電池優先。実間隔はデバイスが選ぶ）
- KeepSubscriptions = false（matd 再購読時に古い購読を掃除）

## ② matd 層

### SubscriptionManager

- 起動時に KVS から commissioned ノード一覧を読み、ノードごとに購読タスクを
  1本張る: resolve（常駐 mDNS キャッシュ）→ 専用 CASE → wildcard Subscribe
  → ポンプ。
- 失敗・死亡時は指数 backoff（5s 開始、上限 5min）で再購読。リトライは
  debug ログ、確立/喪失の状態遷移のみ info（弱リンクノードを常駐ノイズに
  しない）。

### イベント配信

- ポンプ → `tokio::sync::broadcast` → listen 接続ごとの購読者。
- 遅い listener の lag は黙って欠落させず、その listener にだけ
  `{"error":{"kind":"other","detail":"event stream lagged"}}` を送って切断。

### イベント形式（NDJSON、mat スキーマ）

```json
{"timestamp":"2026-07-20T21:00:00+09:00","node_id":21,"endpoint":1,"cluster":"occupancysensing","attribute":"occupancy","value":1,"priming":false}
```

- cluster/attribute は read と同じ chip-tool 記法（`mat-core::ids` に
  無いものは数値のまま）。
- **scalar 値のみイベント化**。list/struct（ACL、server-list 等 wildcard
  priming に混ざるもの）は debug ログのみで捨てる（generic read と同じ
  既知の制限）。
- `priming`: 購読(再)確立直後の初回全量 report 由来は `true`。casa が
  matd 再起動直後の残留状態（例: occupancy=1）をトリガと誤認しないため。

### socket 新 op `listen`

- リクエスト: `{"op":"listen","node_id"?,"endpoint"?,"cluster"?,"attribute"?}`
  （全省略 = 全イベント）。
- matd は 1 行 ack `{"timestamp":...,"listening":true}` を返し、以後フィルタ
  一致イベントを同接続へ流し続ける。切断はクライアント側。
- この op のみ「1行=1往復」の例外（ack 行以降ストリーム）。既存 op 不変。

### 状態は持たない

イベントのリングバッファ/リプレイはやらない（enl と同じ「聞いている間だけ
届く」契約）。必要になったら priming を使う状態スナップショット op を将来
検討。

## ③ mat CLI — listen

```
mat listen [--node <id|alias>] [--endpoint <n>] [--cluster <name>] [--attribute <name>]
           [--count <N>] [--timeout-ms <T>]
```

- matd socket へ接続 → listen リクエスト → ack → イベント行をそのまま
  stdout へ（1行1 JSON）。
- count/timeout は mat 側制御（enl 同様）: `--count N` 到達で exit 0、
  `--timeout-ms T` 経過で打ち切り（`0` = 無期限）。既定 count=1 /
  timeout 60s。0件で timeout → exit 3、1件以上 → exit 0。
- alias 解決は既存どおり CLI 層（node のみ。cluster/attribute は chip-tool
  記法で素通し）。
- 利用形（casa）:

```bash
while ev=$(mat listen --node 21 --cluster occupancysensing --count 1 --timeout-ms 0); do
  # ev を見て mat on / mat off 等
done
```

## ④ エラー

- matd 不在/socket 応答なし → 新 kind **`matd_unavailable`**、**exit 13**
  （README の kind 一覧・exit code 表に追記。12 は chip-tool 退役の歴史的
  欠番のため飛ばす）。
- ストリーム途中の matd 落ち → 出力済みイベントはそのまま、stderr に
  `matd_unavailable` を出して exit 13（count 未達でも timeout(3) にしない）。
- lag 切断は matd がエラー行で明示（上記）。

## ⑤ テスト

実デバイス不要の既存パターンに乗せる:

1. `ReliableChannel` ペア unit テスト: 購読ハンドシェイク（priming 分割・
   StatusResponse・SubscribeResponse）、ポンプ（report 受信・keep-alive・
   MaxInterval 超過死亡検知）。fragile part としてここを釘打ちする。
2. matd server テスト: listen op の ack → ストリーム → フィルタ → lag 切断。
3. バイナリ統合テスト: fake matd socket に対する `mat listen` の
   count/timeout/exit code。
4. 実機 E2E（実装後・デプロイ後の別セッション）: センサー未着でも
   **Nanoleaf で検証可能** — wildcard 購読中に `mat on` を撃ち、on-off
   変化イベントが `mat listen` に流れることを確認。人感実機は入手後に追加。

## スコープ外（将来）

- EventReport 受信（ボタン/Generic Switch）— im.rs の EventRequests /
  EventReportIB デコード追加で載る設計余地は確保済み。
- `subscriptions.toml`（対象絞り込み・パス絞り込み・購読間隔調整）。
- LIT ICD 対応（ICDManagement register-client + check-in 受信）。
- 状態スナップショット op / イベントリプレイ。
