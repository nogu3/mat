# op 予算（deadline）の伝播と執行 — Issue #16

- 日付: 2026-07-29
- ステータス: 設計確定（実装前）
- 対象 Issue: [#16](https://github.com/nogu3/mat/issues/16)

## 背景と動機

Issue #16 の実測（jarvis 45.7 時間）で、op 経路のタイムアウト予算が呼び出し側と
構造的に競合していることが確定した。

- matd の初回 op は最悪 **14.74s**（MRP 総和 4.74s + `IM_RECV_TIMEOUT` 10s）。
  呼び出し側 mando の予算 15.0s と実質同着で、正常系でも運次第で Timeout になる
- `with_session` の Timeout 腕「1 回だけ再確立して再送」はさらに最悪 45〜60s
  （`CACHE_MISS_TIMEOUT` 35s + CASE 10s + 再送 14.74s）を使う。**再送が成功しても
  呼び出し側は既に `mat` を kill しており、結果を返す相手がいない**（復旧ロジックが
  構造的に死んでいる）
- 諦めた後も matd は op を最後まで走らせ、per-node Mutex を握り続ける
  （head-of-line blocking）。クライアント切断（Broken pipe 305 件/45.7h）を
  検出する仕組みがない
- `mat` の op 経路（`matd_client.rs::exchange_on_stream`）は read timeout なしの
  無期限ブロック。呼び出し側の kill だけが打ち切り手段

「次の一手」のうち op ログ `elapsed_ms` は監査 Tier2⑪ で実装済み。本設計は残る
3 点（deadline 伝播 / 切断キャンセル / 最悪所要の定数化）を完結させる。

## スコープ

### やること

1. socket protocol に `deadline_ms`（相対予算）を追加し、mat → matd へ伝播
2. `with_session` を deadline-aware 化（予算執行 + 予算条件付き再確立 + slot 破棄）
3. matd server のクライアント切断検出と進行中 op のキャンセル
4. mat CLI にグローバル `--op-timeout-ms`（既定 60s）を追加、両経路で執行
5. 予算成分の pub 定数化と合計の釘打ちテスト

### やらないこと（YAGNI / スコープ外）

- MRP / IM の recv timeout 自体を残り予算に収縮させる（アプローチ B）。
  実績のある `exchange.rs` / `session.rs` へ横断変更が入る。本設計のログで
  「どこで予算が尽きるか」を実測してから必要なら別途
- group 系 / provision / bump への deadline 適用（provision はノード数×4 ステップの
  長時間 op で一律予算が正常系を切る。切断キャンセルは全 op 共通で効く）
- mando 側の変更（`--op-timeout-ms 13000` を渡す。別リポジトリのフォローアップ。
  kill 15s は保険として維持）
- listen の変更（既存の `--timeout-ms` セマンティクスは無関係・無変更）

## 設計

### 1. プロトコル（`crates/matd/src/protocol.rs`）

`Request` にトップレベル任意フィールドを追加する:

```rust
pub struct Request {
    pub id: Option<Value>,
    /// クライアントの op 予算（相対 ms）。単一ノード op のみ適用。
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    #[serde(flatten)]
    pub op: Op,
}
```

- **相対予算 ms**。絶対時刻は 2 プロセス間で時計合意を仮定するので使わない
- matd は**単一ノード op**（`Op::node_id()` が `Some` の op = read / write / invoke /
  on / off / color_temp / level / color / describe）のみに適用。group 系 /
  provision / bump / listen / ping / shutdown では無視
- 未指定（旧 mat クライアント）は matd 側既定 `DEFAULT_OP_BUDGET = 60s` を
  単一ノード op に適用
- 旧 matd は未知フィールドを無視する（internally-tagged + flatten は
  `deny_unknown_fields` なし）— 新 mat → 旧 matd の混在も安全。テストで釘打ち

### 2. mat CLI（`crates/mat/src/cli.rs` / `matd_client.rs` / `native_direct.rs`）

- グローバル `--op-timeout-ms`（env `MAT_OP_TIMEOUT_MS`、既定 `60000`、
  `0` = 無効 = 従来の無期限）。`mat listen` 既存の `--timeout-ms`
  （ストリーム受信予算、別セマンティクス）と名前衝突するため別名にする
- **matd 経路**: 単一ノード op のリクエストに `deadline_ms` = 予算を載せ、
  socket の read timeout を **予算 + 2s（`CLIENT_SLACK`）** に設定する。
  matd の構造化 timeout エラーが必ず先に届き、届かない場合（旧 matd・matd 停止）
  のみ mat 自身が `timeout`（exit 3）で返る — 無期限ハングが消える。
  read timeout 発火の detail は「リクエストは実行済みの可能性がある」旨を明示
  （既存 `matd_unavailable` の文言と同じ精神）
- **直経路**: 単一ノード op を同じ予算で外側 `tokio::time::timeout` し、
  超過は `timeout`（exit 3）。経路によってフラグの効き方が変わらない
- 非対象 op（group 系等）は両経路とも従来どおり（read timeout も掛けない）

### 3. matd server の切断キャンセル（`crates/matd/src/server.rs`）

`handle_conn` のディスパッチを `select!` 化する:

- in-flight の `dispatch` future と `lines.next_line()` を `select!`
- 接続 EOF / read エラー → op future を **drop**（per-node Mutex が即解放 =
  head-of-line blocking 解消）。該当ノードの slot を明示破棄
  （`NativeBackend::drop_session(node_id)` を新設）し、
  `op aborted (client disconnected)` を op 名 / node_id / elapsed_ms 付きで
  warn ログ。応答は書かない
- op 実行中に**別リクエスト行**が届いた場合はバッファし、現 op 完了後に処理
  （現行の逐次セマンティクスを維持。1 行だけ保持すれば足りる — 次の行は
  現 op 完了後の通常ループが読む）
- 切断キャンセルは deadline と違い**全 op 共通**（group 系含む）。ただし
  slot 破棄は単一ノード op のときだけ（`Op::node_id()` が `Some` のとき）

### 4. `with_session` の deadline 執行（`crates/matd/src/native.rs`）

シグネチャを `with_session(node_id, deadline: Option<Instant>, op)` に変更する。
server が `deadline_ms`（または既定 60s）から `Instant` を計算して渡す。

- 確立・送信の各段階を**残り予算**の `tokio::time::timeout` で包む
- 予算超過時は **slot を破棄**して `ErrorKind::Timeout` を返す。中途 exchange の
  session を持ち越さない（次 op が不定状態の session を掴む事故を防ぐ）。
  detail はフェーズと経過 ms を含める:
  `"op deadline exceeded after 13000ms in resend-establish"`
  （フェーズ: `establish` / `send` / `resend-establish` / `resend`）
- Timeout 腕の再確立+再送は**残り予算 ≥ `RETRY_MIN_BUDGET`（10s）** のときのみ。
  不足なら再確立を撃たずに元の Timeout を即返す（mando の 13s 予算では
  構造的に無駄だった再確立を最初から撃たない）。スキップ時は
  `"skipping re-establish; insufficient budget"` を info ログ
- `RETRY_MIN_BUDGET = 10s` の根拠: warm-cache の mDNS 解決 + CASE 往復
  （典型 ~1s）+ MRP 一巡（4.74s）+ 応答余裕。§5 の定数から導出した旨を
  コメントに残す
- `deadline = None`（`--op-timeout-ms 0`）は従来どおり無制限

既存の腕の整理（DeviceRejected / ParseError は slot 維持で即 Err、
その他は slot 破棄で Err）は変更しない。

### 5. 予算成分の定数化と釘打ち（`crates/mat-controller` / `mat-native`）

散在する予算成分を pub 化し、合計をテストで釘打ちする:

- `exchange.rs`: MRP 総和（`total_budget(default)` = 4.74s 相当）
- `session.rs`: `IM_RECV_TIMEOUT`（10s）→ pub 化
- `case.rs`: `RECV_TIMEOUT`（10s）→ pub 化
- `mat-native/lib.rs`: `CACHE_MISS_TIMEOUT`（35s）→ pub 化
- 導出定数として「単一 op 送信の最悪 = MRP 総和 + IM recv = 14.74s」を
  doc コメント + テストで固定。上流値の変更でテストが割れて気づける
- `RETRY_MIN_BUDGET` / `DEFAULT_OP_BUDGET` / `CLIENT_SLACK` もこの体系の
  一部として定義箇所に根拠コメントを置く

### 6. エラー・ログ形状

- 予算超過は既存の `timeout` kind（exit 3）に載せる。新しい error kind は増やさない
- matd の op ログ（`log_op`）は既存の failed 腕で `kind=Timeout` として出る。
  `elapsed_ms` が予算とほぼ一致することが deadline カットの目印
- 切断 abort は `log_op` に到達しない（応答が無い）ので §3 の専用 warn ログで残す

## テスト計画

- **unit（native.rs、FakeEstablisher 拡張: 遅延注入）**:
  - 予算内成功 → 従来どおり
  - 送信が予算超過 → Timeout + slot 破棄（次 op で再確立される）
  - 送信 Timeout 後、残り予算 < `RETRY_MIN_BUDGET` → 再確立せず即 Timeout
  - 残り予算 ≥ `RETRY_MIN_BUDGET` → 従来どおり再確立+再送
  - `deadline = None` → 無制限（従来テストが回帰チェック）
- **unit（protocol.rs)**: `deadline_ms` パース / 未指定 = None / 既存形式互換
- **integration（tests/integration.rs、fake establisher）**:
  - 極小 `deadline_ms` で構造化 timeout 応答（exit 3 相当）が返る
  - 遅い op 中にクライアント切断 → 同一ノードへの後続接続の op が
    Mutex 待ちせず即進む（キャンセル + slot 破棄の証明)
  - deadline_ms 無しの旧クライアント → 60s 既定が適用される（＝即失敗しない）
  - listen 経路が無変更
- **釘打ち（mat-controller）**: 予算成分の合計値テスト
- **実機 E2E（マージ前必須、jarvis 隔離 matd）**:
  - `mat read --op-timeout-ms 3000` が不達ノードで ~3s + slack で exit 3
  - 旧 mat（1.9.0）→ 新 matd の互換（通常 op 成功）
  - 新 mat → 本番相当 matd の通常 op 成功

## リリース

- バージョン 1.10.0
- README: `--op-timeout-ms` フラグ、Errors and exit codes 節の timeout 説明更新
- ARCHITECTURE: op 予算の設計（deadline 伝播・切断キャンセル）を matd 節へ追記
- Issue #16 はマージ + 実機 E2E 後にクローズ（mando 側フォローアップ issue を
  nogu3/mando に起票してリンク）
