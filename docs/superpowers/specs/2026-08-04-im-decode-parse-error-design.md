# IM デコード失敗の誤分類修正 + 購読耐性（安定性監査 Tier 2 ⑨）

日付: 2026-08-04 / 対象バージョン: 1.20.0

## 背景 / 問題

監査⑨（2026-07-25）: IM ペイロードのデコード失敗が 2 つの別問題を起こす。

1. **誤分類** — `mat-native/src/lib.rs` の `map_session_err` は
   `SessionError::Im(_)` を一括で `ErrorKind::DeviceRejected` に写像する。
   しかし `ImError` には「デバイスが拒否した」系（`StatusResponse` /
   `AttributeStatus` / `CommandStatus`）と「応答は届いたがこちらのデコーダが
   解釈できなかった」系（`Tlv` / `Malformed` / `UnsupportedValue`）が同居して
   いる。後者を `device_rejected`(exit 4) と報告すると、エラー `kind` を根拠に
   回復方針を決める呼び出し側（AI・mando 等）を誤誘導する。メッセージ層の
   破損は既に `SessionError::Message(_) → ParseError` の規律がある（v1 品質
   修正 4）— 監査の共通パターン「正しい規律の前例が別の関数に未適用」の一例。
2. **購読死** — 購読の 2 経路（`session.rs` `subscribe_wildcard` の priming
   ループ / `next_subscription_report`）は `decode_report_data_message` の失敗を
   `?` で伝播し、購読ごと殺す。matd は再購読するが、デバイスが決定的に同じ
   非デコード可能レポートを priming に含める個体だと「再購読 → priming で
   再死」のループ = **恒久盲目**になる。

op ログ観測（7/27〜8/2）では実地発生ゼロ（週内の `kind=DeviceRejected` は
全て正規のデバイス拒否）。機構は実在するが野生で踏んでいない — 本件は
ユーザー方針「安定化につながることは基本的に全部やる」に基づく保険。

## 成功基準（ユーザー承認済み）

1. デコード失敗が `parse_error` に分類される（`device_rejected` は本当の
   デバイス拒否だけになる）。
2. 非デコード可能なレポートが priming / live のどちらに来ても購読が生存する
   （恒久盲目シナリオの根絶）。

## 決定

**案A: session 層で salvage。** デコーダ（`im.rs`）は strict なまま、耐性は
購読ライフサイクルを持つ層（`session.rs` の購読 2 経路）にだけ入れる。

- 却下した案B（デコーダの寛容化・部分 salvage）: 現実の失敗は TLV ストリーム
  層（Reader が読めない要素型）で、コンテナ境界まで skip する術がなく salvage
  不能。read 経路含む全呼び出し元の契約が変わり、エラーが静かに隠れる。
- 却下した案C（matd ポンプ層で `parse_error` なら continue）: `parse_error`
  は `Message(_)`（セッション異常の兆候）からも来るため分類で continue/break
  を決めるのは脆い。priming での失敗は establish 自体が Err になるので matd
  層では救えず、恒久盲目を塞げない。

変更は `crates/mat-native/src/lib.rs`（分類）と
`crates/mat-controller/src/session.rs`（購読 2 経路）に閉じる。

## 設計

### 1. 誤分類修正（`map_session_err`）

`SessionError::Im(e)` を variant で分割する:

- `StatusResponse` / `AttributeStatus` / `CommandStatus` →
  `ErrorKind::DeviceRejected`（コマンドは届き、デバイスが拒否した）
- `Tlv` / `Malformed` / `UnsupportedValue` → `ErrorKind::ParseError`
  （応答は来たが解釈不能 — `Message(_)` と同じ規律）

wildcard は使わず、`ImError` の variant 追加時にここで分類を決めさせる
（`map_resolve_err` と同じ流儀）。one-shot read/invoke がデコード失敗した
場合の exit は 4→1 に変わるが、これは誤分類の是正そのもの。JSON スキーマ・
`kind` の語彙は不変。

### 2. priming 耐性（`subscribe_wildcard`）

REPORT_DATA チャンクの `decode_report_data_message` 失敗を伝播せず:

1. warn ログ（exchange_id・payload 長・エラー内容。payload 先頭 hex は
   debug で — 未知エンコーディングの事後診断材料）。
2. **空の `ReportDataMessage` を priming に push する** — `MAX_REPORT_CHUNKS`
   ガードが非デコード可能チャンクにも効き続ける（push しないと無限チャンクの
   flood 防御が消える）。
3. 通常チャンクと同じく `StatusResponse(0)` を send_reliable して
   次チャンク / SubscribeResponse 待ちを続行する。

購読は成立し、失われるのはそのチャンクの属性値のみ（matd の state cache は
そのデバイスの次のレポートで自己回復する）。

### 3. live pump 耐性（`next_subscription_report`）

デコード失敗時:

1. warn ログ（同上）。
2. `suppress_response` が読めないので **false とみなして `respond_status(0)`**
   する。根拠: 1.16.0 のワイヤ実測で実デバイスの購読レポートは suppress=false
   + StatusResponse 期待（report → ack+SR → device ack で完結）。suppress=true
   の相手に余計な SR を送っても exchange 終端で無害。respond 失敗は既存の
   `deferred_sub_err` 持ち越し規律をそのまま使う（レポート — ここでは空 rd —
   は届け、エラーは次呼び出しで返す）。
3. **空の `ReportDataMessage` を返す** — matd ポンプは keep-alive と同じ扱い
   （`last_msg` リセット）。非デコード可能でも認証済み（MIC 検証済み）の
   デバイス発メッセージなので、生存の証拠としては正しい。MRP ack は screen が
   受信時に済ませているため再送嵐にもならない。

### エラー処理

新たな失敗経路は増えない。デコード失敗の握り込みは購読 2 経路だけで、
握り込んだ事実は毎回 warn に出る（journal で観測可能）。連続失敗 cap は
付けない（ユーザー決定 — 全レポート非デコード可能の個体は warn の継続出力で
発見する）。

## テスト

単体（mat-native）:

1. **分類分割の pin**: `Im(Malformed)` → `parse_error`、`Im(StatusResponse)`
   → `device_rejected`（既存の分類テスト群に追加）。

単体（mat-controller session — 既存の test-responder 足場）:

2. **priming salvage**: garbage REPORT_DATA チャンク → 正常チャンク →
   SubscribeResponse の応答列で `subscribe_wildcard` が Ok、priming は
   [空 rd, 正常 rd]、garbage チャンクにも StatusResponse(0) が送られる。
3. **live salvage**: garbage report 受信で `next_subscription_report` が
   空 rd を返し StatusResponse(0) を送る。次の正常 report は通常配送。
4. **deferred 持ち越し無回帰**: 既存テスト
   `respond_status_failure_defers_error_and_still_delivers_report` が通る。
5. **flood 防御**: garbage チャンクだけを `MAX_REPORT_CHUNKS` 超で流すと
   `Malformed("too many report chunks")` で失敗する（ガード維持の pin）。

実機 E2E（マージ前必須 — 非デコード可能レポートは実機で誘発不能のため
無回帰スモークのみ）:

- 隔離 matd スモーク（`*.new` 方式）: 購読確立 attempts=1、`matd status`
  健全、warm read exit0、WARN 0。

## リリース

- バージョン 1.20.0、fix ブランチ（`fix/tier2-im-decode-parse-error`）。
- これまでの監査項目と同じ流儀: spec/plan を docs/superpowers/ に置き、
  実機スモーク合格後に main へマージ。

## スコープ外

- デコーダ（`im.rs`）の寛容化・部分 salvage。
- one-shot read/invoke 経路の耐性（分類修正のみ。一発 op は呼び出し側
  リトライが正しい回復）。
- 連続デコード失敗 cap / ゾンビ購読の強制 teardown。
- イベント/ステータスへのデコード失敗カウンタ露出（warn ログで足りる）。
