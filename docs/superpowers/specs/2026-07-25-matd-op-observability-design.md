# matd の op 可観測性 — op ログ + ANSI 抑止（1.3.0）

日付: 2026-07-25 / 対象: `matd`（`server.rs` / `native.rs` / `protocol.rs` / `main.rs`）、
`mat`（`main.rs`）、`mat-core`（ログフィルタ選択の純関数）

## 問題

`matd` は **op 経路のログを 1 行も出していない**。`server.rs:306` の dispatch は
`run_op` の結果をそのまま応答ボディに詰めるだけで、成否・所要時間・失敗理由が
どこにも残らない。`server.rs` の tracing は 6 箇所（起動 / listen 開始 / 接続
ハンドラのエラー / Ctrl-C / shutdown op / listen lag）だけで、リクエスト処理
経路には 0 件である。一方 `mat` の直経路には op ごとの info ログが 19 箇所ある
（`native_direct.rs:1095` 他）ので、**同じ操作が経路によって記録されるかどうかが
非対称**になっている。spec / commit を追っても意図的に省いた記録はない。

結果として「22:31 に照明が反応しなかった」を matd 側と突き合わせられない。
どの node のどの op が何秒かけてどの kind で落ちたのかが残らないため、
弱リンク / メッシュ全域劣化 / matd の状態破損の切り分けができず、matd 再起動に
逃げることになる。所要時間も記録されないので「遅いが成功」と「失敗」も区別
できない。

さらに 2 つ、ログを実用にする前提が壊れている。どちらも実測で確認した:

1. **ログに ANSI エスケープが混入する。** `tracing_subscriber::fmt()` は tty
   判定をせず、`is_ansi = cfg!(feature = "ansi") && NO_COLOR が未設定/空` で
   決まる（tracing-subscriber 0.3.23 `fmt_layer.rs:743`。`ansi` は既定 feature
   で有効）。実出力は `^[[3mnode_id^[[0m^[[2m=^[[0m42` なので journald 上で
   **`grep 'node_id=42'` や `grep 'error='` が 0 件になる**。
2. **`MAT_LOG=""` でログが全 OFF になる。** 空文字は EnvFilter として有効
   （ディレクティブ 0 個）なので `try_from_env` が `Ok` を返し、既定 info に
   落ちない。実測で WARN すら出なくなる。`systemctl --user set-environment` で
   一時 debug を入れて戻すときに踏みやすい。

加えて 2 つの死角がある:

- **warm/cold が片側しか見えない。** `native.rs::with_session` は再確立
  （`:142` / `:158`）だけ info を出し、**初回確立にはログが無い**。「毎回 cold に
  なっている（= session churn）」が観測できない。
- **`mat listen` の接続/切断が完全に無記録。** `stream_events` はクライアント
  EOF で無言 `return Ok(())` する。casad が繋がっていたか、いつ切れたかが
  後から分からない。

## 決定（ユーザー承認 2026-07-25）

### 1. op ログを dispatch 1 箇所で出す

`server.rs::dispatch` で `run_op` を `Instant` で挟み、結果に応じて **1 行**出す。
実装位置を dispatch に置くのは、全 op を漏れなく捕まえられ、op 追加時にも
自動的に載るため。`run_op` の各分岐に散らす案（`mat` 直経路と同形）は 20 箇所に
分散し新 op で書き忘れが起きる。`#[instrument]` による span 案は既存 30 箇所の
ログの見え方を変えるので採らない（将来 `matd status` にメトリクスを入れる際に
再検討する）。

`elapsed_ms` は **`run_op` のみ**を測る（JSON パース・応答書き込みは含めない）。

出力の実物:

```
INFO  matd::server: matd op slow op=on node_id=7 endpoint=1 elapsed_ms=1243
WARN  matd::server: matd op failed op=read node_id=42 endpoint=1 target=occupancysensing/occupancy elapsed_ms=8134 kind=Timeout detail=no acknowledgement within MRP retry budget
INFO  matd::server: matd op rejected op=read node_id=99 endpoint=1 elapsed_ms=1 kind=NodeNotCommissioned detail=node 99 is not commissioned
DEBUG matd::server: matd op ok op=read node_id=6 endpoint=1 target=onoff/on-off elapsed_ms=94
```

`kind` は既存 5 箇所（`subscription.rs:415` 他）と同じ `?e.kind`（Debug 表記）に
揃える。`node_id` / `endpoint` / `target` / `group_id` は `Option` で渡す —
tracing の `impl<T: Value> Value for Option<T>` は `None` のとき
**フィールドごと省略する**（`tracing-core-0.1.36` `field.rs:791`）ので、
`node_id=Some(42)` にはならず `grep node_id=42` が効く形を保てる。

### 2. level 方針は純関数で釘打ちする

`server.rs` の private 関数として置き、テストは同モジュールに書く（matd 固有の
運用方針なので `mat-core` には出さない）。ログは `run_op` が返った直後・応答を
書く前に出す。

```rust
enum OpLogClass { Failed, Rejected, Slow, Ok }
const SLOW_OP_MS: u128 = 300;
fn classify_op_log(result: &Result<Value, MatError>, elapsed_ms: u128) -> OpLogClass
```

- **Failed（warn）** — `Timeout` / `Unreachable` / `SessionFailed` / `Other` /
  `CommissionFailed` / `MatdUnavailable` / `ChildNotFound` / `ChildFailed`。
  経路そのものの問題。`journalctl -p warning` で劣化だけを抽出できる。
  （`CommissionFailed` / `Child*` は matd 経路では発生しないが、網羅 match の
  ために分類は決めておく。）
- **Rejected（info）** — `StoreMissing` / `StoreParse` / `NodeNotCommissioned` /
  `DeviceRejected` / `ParseError`。要求側・意味の問題なので warn を汚さない。
- **Slow（info）** — 成功かつ `elapsed_ms >= 300`。
- **Ok（debug）** — それ以外。既定 level は info なので journald には出ない。

閾値 300ms の根拠: warm セッションの実測が 71-149ms なので、300ms を超えた成功は
すでに「普段と違う」= 弱リンク化 / メッシュ劣化の前兆である。cold op（CASE 確立で
実測 1.2-1.6s）は必ず Slow に該当するが、直前に「確立」の 1 行（下記 4）が出るので
区別できる。

`ErrorKind` を網羅 match することで、**将来 variant が増えたら level の判断を
コンパイラが強制する**。

### 3. 初期化を直す（ANSI と `MAT_LOG=""`）

`MAT_LOG` / `RUST_LOG` の選択ロジックだけを `mat-core` の純関数に置く。
**新しい依存は増やさない**（`tracing-subscriber` は各バイナリ側に残す）。

```rust
// mat-core
/// ログフィルタ指定を選ぶ。空文字・空白のみは「未設定」として扱う
/// （EnvFilter として有効なため、そのまま渡すと全 OFF になる）。
pub fn log_filter_spec(mat_log: Option<&str>, rust_log: Option<&str>) -> Option<String>
```

- `matd`: `.with_ansi(false)` 固定（デーモンなので無条件）。既定 level は info を維持。
- `mat`: `.with_ansi(std::io::stderr().is_terminal())`（対話では色あり、パイプや
  mando 経由では色なし）。既定 level は warn を維持。
- 無効なフィルタ文字列のときに既定 level へ落ちる現在の挙動は維持する。

### 4. warm/cold の可視化

`native.rs::with_session` で slot が空のとき、確立の**前**に
`tracing::info!(node_id, "no warm session; establishing")` を 1 行。再確立側
（`:142` / `:158`）が既に info なので形が揃う。確立の成否は op ログ側に出るため
成功時の 2 行目は出さない。頻度は「matd 起動後にノードごと 1 回 + churn の回数」
なので info で問題ない（churn の回数こそ見たい値である）。

### 5. listen の接続/切断

- `handle_conn` の listen ack 送信直後に
  `info!(node_id, endpoint, cluster, attribute, "listen client attached")`
  （フィルタの 4 フィールド。いずれも `Option` なので未指定は省略される）。
- `stream_events` に `delivered: u64` を持たせ、正常終了 2 経路
  （クライアント切断 / broadcast チャンネル閉）で
  `info!(delivered, reason, "listen client detached")`。`reason` は
  `client_disconnected` / `channel_closed`。
- 既存の lag warn（`server.rs:193`）には `delivered` を足すだけにする。
- **書き込みエラーでの切断には新しい行を足さない** — 既存の
  `connection handler ended with error`（`server.rs:83`）に出る。この経路だけ
  `delivered` が取れないのは意図的な割り切り（`?` の伝播を変えないため）。

### 6. `Op` に足すアクセサ（`protocol.rs`）

`node_id()` と同形の網羅 match で 4 つ。op を増やしたときコンパイラが漏れを
強制するのが狙い。

- `name() -> &'static str` — serde の `op` タグと同じ snake_case（`read` /
  `group_invoke` …）。`Op` は `Deserialize` のみなのでタグ文字列を再利用できず、
  手書きの網羅 match が最も素直。
- `group_id() -> Option<u16>` — group 系 5 op（全て `group_id: u16`）。node_id を
  持たない op の識別に使う。
- `endpoint() -> Option<u16>` — `Describe` / `Ping` / `Shutdown` / `Listen` 以外の
  13 op が `endpoint: u16` を持つ（`Listen` の `endpoint` は `Option<u16>` だが
  dispatch に到達しないので `None` を返す）。
- `log_target() -> Option<String>` — `Read` / `Write` は `cluster/attribute`、
  `Invoke` / `GroupInvoke` は `cluster/command`、それ以外は `None`（op 名だけで
  足りる）。

## スコープ

- **挙動変更はゼロ。** 応答 JSON のスキーマ・exit code・エラー分類・タイミングは
  一切変えない。追加するのは stderr のログと、ログ初期化の 2 つの不具合修正のみ。
- **触らない**: `matd status` op の新設（Tier 2 として別途）／MRP・CASE・resolve
  層へのログ追加（op ログを見てから必要性を判断する）／購読ライフサイクルの
  ログ（既に 12 箇所あり手厚い）／`mat` 直経路の op ログ（既にある）。
- `Op::Listen` は `dispatch` に到達しない（`handle_conn` が先取りする）ので op
  ログには出ない。listen は上記 5 の 2 行が受け持つ。
- `Ping` / `Shutdown` も op ログに載る（debug / info）。`mat` の matd 自動発見は
  素の `connect()` なので Ping は実質テスト専用であり、量の問題はない。

## テスト

`task check` に載る単体テストのみ。実時間は増やさない。

- `classify_op_log` を `ErrorKind` 全 variant と境界（299ms / 300ms）で網羅。
  これが level 方針の唯一の釘。
- `Op::name()` / `group_id()` / `log_target()` の代表ケース（網羅性はコンパイラが
  保証するので全 variant は書かない）。
- `log_filter_spec` の `Some("")` / 空白のみ / `None` / 有効値 / `RUST_LOG`
  フォールバック（純関数なので環境変数を汚さない）。
- ANSI とログ本文は単体テストしない（builder 呼び出し 1 行）。`cat -v` で目視する。

## 実機 E2E（マージ前・ユーザー承認済みの方式）

jarvis で `.new` を **別 socket に 30 秒だけ**立てて実測し、即停止する。本番
matd は差し替えない。代償は本番購読が 1 回再確立する程度（隔離 matd と本番 matd の
追い出し合戦は既知だが、短時間なので許容する）。

確認する項目:

1. `matd op ok`（debug）が既定 info では出ないこと、`MAT_LOG=debug` で出ること
2. warm read → `matd op ok` の `elapsed_ms` が 3 桁未満、cold read → `no warm
   session; establishing` の直後に `matd op slow`
3. 到達しない node への read → `matd op failed` が warn で出て `kind` / `detail`
   が付くこと
4. `mat listen` を張って切る → `listen client attached` / `detached
   reason=client_disconnected delivered=N`
5. `cat -v` 相当で ANSI エスケープが消えていること、`grep 'node_id=<id>'` が
   実際に引っかかること
6. `MAT_LOG=""` で既定 info のログが出ること（現状は全 OFF）

## ドキュメント

- README の matd 運用節に、出るログ行の一覧・level 方針・300ms 閾値・
  `MAT_LOG=""` の扱い・ANSI 無効化を追記する。
- ARCHITECTURE.md は設計判断に変更が無いので触らない。

## バージョン

1.2.1 → **1.3.0**（挙動変更ゼロだが観測面の機能追加）。

## 呼び出し側への影響

`mat` / mando / casad への影響は無い（ソケットプロトコルと応答スキーマは不変）。
運用面では journald の行数が増えるが、既定 info で増えるのは「失敗」「300ms 以上の
成功」「session 確立」「listen 接着/切断」だけであり、正常時の定常負荷はほぼ
変わらない。

デプロイまでの暫定策として、コード修正なしで ANSI だけ消す手段がある:
matd の unit に `Environment=NO_COLOR=1` を足す（jarvis-iac 側の別変更）。
1.3.0 をデプロイすれば不要になる。
