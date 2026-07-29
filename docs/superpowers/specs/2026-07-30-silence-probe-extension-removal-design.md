# 無音 probe 延長の撤去（Issue #15 帰結）

- 日付: 2026-07-30
- 対象: `crates/matd/src/subscription.rs`（主）、`crates/mat-native/src/lib.rs` /
  `test_support.rs`（`SubscribeConn::probe` の削除）、ARCHITECTURE.md（stale 記述の是正）
- 出自: Issue #15「無音 deadline による購読 teardown と 5 分 backoff で、
  センサーが最長 47 分 blind になる」の残項目
  （対応案 1 = probe と 3 = BACKOFF_MAX 60s 化は 1.6.0 で実装済み）
- バージョン: 1.11.0

## 背景と実測

Issue #15 の残項目は「probe 偽陽性率の実測を見て `SILENCE_SLACK` を決め直す」
だった。jarvis 本番 journal（probe 実装後 2026-07-27〜07-30、matd 1.6.0〜1.10.0）
の実測:

- **probe 自体は約 9 割通る**（pass 212 vs fail 23）— 無音 deadline 到達時点で
  CASE セッションは生きている。
- しかし **probe 延長後にデバイス発メッセージが再開した例はほぼゼロ**:
  3 日間で 1 回目延長 108 件 → 104 件がそのまま 2 回目延長 → 104 件全部が
  extensions exhausted で teardown。回復 4/108（約 4%）。1.10.0 の綺麗な窓
  （07-29 22:14〜07-30 朝）では **0/18**。
- **exhausted teardown 後の再購読は安い**: `down_s` 中央値 **9s**、p90 34s、
  平均 28s（n=104）。
- blind 総量は 1.6.0（probe + backoff 60s 化）で 3.7h/日 → 約 50 分/日相当まで
  改善済み。node16（人感）は直近 13 時間で再確立ゼロ。

### 判定

1. **`SILENCE_SLACK` 引き上げ（issue 対応案 2）は棄却。** slack が救う対象
   =「keep-alive を 1 発だけ取りこぼした購読」は実測上ほぼ存在しない。330s
   無音になったノードはその後も無音のまま — デバイス側の silent discard
   （ARCHITECTURE 記載の実機知見と整合）か、keep-alive を出さない FW
   （node16 型）。引き上げは真の死の検知だけを遅らせる。
2. **probe 延長（キャップ 2）は純損失。** 救済がほぼゼロなのに、死んだ購読の
   検知を 330s → 最大 990s に 3 倍化する。再購読が中央値 9s で済む以上、
   660s の様子見に価値がない。撤去する。

probe の設計意図（偽陽性の計測）は達成された — その計測結果が「probe は
不要」だった、という帰結。

## 変更

### matd（`subscription.rs`）

`PumpEnd::Silence` 到達で probe を撃たず**即 teardown** → 既存の再購読ループへ。

- 削除: `SILENCE_PROBE_MAX`、`should_probe()`、`probes_used`、probe 分岐
  （pass の re-arm / fail / exhausted の 3 ログ）、対応ユニットテスト
  （`should_probe_only_for_proven_silence_under_cap` 等）。
- ログは 0.28.0 形式に戻す: `report pump ended (silence past deadline)` +
  `silent_s`。
- **無変更**: `SILENCE_SLACK = 30s`（deadline = max_interval + 30s。実測で
  330s 判定はほぼ正しい死の信号だった）、born-dead / op 相関経路、
  backoff（5s 開始・上限 60s）、`PumpEnd` の 3 値自体。
- モジュール先頭の doc コメント（probe 延長の記述）を追随。

### mat-native

呼び手が消えるため `SubscribeConn::probe()` を純減で撤去:

- `lib.rs`: トレイトメソッド宣言 + 実装（read ベースの生存確認）+
  fake 契約テスト（`fake_sub_conn_probe_succeeds_counts_and_fails_when_injected`）。
- `test_support.rs`: `FakeSubConn` / `FakeEstablisher` の `fail_probe` /
  `probe_calls` フィールドと probe 実装。

### ドキュメント

- ARCHITECTURE.md 購読節の stale 是正（このセクションは「現在の設計」を
  記述しているため）: 「指数 backoff 上限 5min」→ 60s（1.6.0）、
  「MaxInterval の 1.5 倍を超える無音」→ max_interval + 30s（0.28.0）。
- 本件の M レコード追加: 実測値（4%/0% 救済・再購読 9s・SLACK 棄却根拠）ごと
  記録し、将来「延長を復活させたくなった」ときに同じ議論を繰り返さないように
  する。
- README: matd の購読死活の説明に probe 延長への言及があれば追随
  （grep では未検出 — 実装時に再確認）。
- CHANGELOG / Cargo.toml: 1.11.0。

## テスト

- 既存 FakeConn ベースの pump テスト群を「無音 deadline 到達 → 即 teardown」
  期待に更新（probe 注入前提のテストは削除）。
- born-dead / op 相関のテストが probe 削除で壊れないことを確認（これらは
  probe 対象外だったので原理上無風のはず）。
- `task check`（fmt:check + clippy + test）。

## 実機 E2E（マージ前・必須）

jarvis の隔離 matd 方式（別 socket + store コピー + 台帳 1 ノード）:

1. 購読 1 ノードで隔離 matd を起動、established を確認。
2. 無音 teardown が deadline（約 330s）で `silence past deadline` として発火
   し、probe 関連ログが一切出ないことを確認。
3. 再購読が数秒〜数十秒（backoff 1 段以内）で established に戻ることを確認。

合格後にマージ → 本番デプロイ → スモーク。デプロイ後、Issue #15 に実測
サマリ（SLACK 引き上げ棄却の根拠含む）をコメントしてクローズ。

## やらないこと

- `SILENCE_SLACK` / deadline の変更（実測が現値を支持）。
- `MaxIntervalCeiling = 300s` の短縮（検知をさらに速くする別の手だが、
  Thread メッシュの keep-alive 負荷とのトレードオフが未計測。必要になったら
  別 issue）。
- ノード種別ごとの deadline / backoff 分岐（YAGNI — node16 型 FW も現行
  一律値で実害が出ていない）。
