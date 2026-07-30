# CloseSession — 放置 CASE セッションの後始末（Issue #20）

日付: 2026-07-30 / 対象バージョン: 1.14.0

## 背景 / 問題

mat は CASE セッションの後始末をしない（CloseSession 送信なし・resumption なし、
`mat-controller/src/case.rs` 冒頭コメント）。直経路 op（`diag` は仕様上常に直経路）や
matd の warm op セッション新設は、使い捨てセッションをデバイス上に残して終了する。

Aqara FP300（node16 = 書斎人感）の FW は購読レポート/keep-alive を「同一ファブリックの
**最新**セッション」に送るため、放置セッションが最新になった瞬間からレポートが
ブラックホール化し、MRP 再送 5 回の空振り後にデバイスが購読を silent discard する。
matd は 330s 無音 deadline まで盲目（= 書斎ライト「つかない」の正体）。

ワイヤ実証: 購読 established → `mat diag thread --node 16` 1発 → 予測 ±1 秒で購読死
（証拠 pcap = jarvis:`~/node16-sessionsteal-20260730.pcap`、Issue #20 に詳細）。
HA (chip-sdk) が無事なのは op/購読で 1 セッションを使い回すため。

## 決定

**全 teardown 経路で、セッションを手放す直前に CloseSession を best-effort 送信する。**

CloseSession = Secure Channel StatusReport
`{general_code=SUCCESS(0), protocol_id=0x0000, protocol_code=CloseSession(2)}`。
送信は暗号化 1 データグラム・MRP 再送なし・ack 待ちなし。前例 = `pase.rs:363-380` の
abort StatusReport（「unwinding 経路を MRP の ~4.7s 予算でブロックしない」）。
購読 teardown への待ち追加が純損失という実測結論（`subscription.rs:9`、spec
2026-07-30 probe 撤去）にも整合する。

### 却下した代替案

- `send_reliable` での確実配達 — teardown が最悪 ~4.7s ブロック。却下。
- op/購読セッション統合（chip-sdk 構造）— 構造的根治だが engine 再設計級。
  将来課題として記録のみ（本 spec の非対象）。
- per-node 無音 deadline 短縮 — 緩和策であり根治でない。非対象。

## 実装

### 1. プロトコル層（mat-controller）

- 定数 `SC_PROTOCOL_CODE_CLOSE_SESSION: u16 = 2` を追加。
  注意: 既存 `SC_PROTOCOL_CODE_INVALID_PARAMETER = 2`（general=FAILURE 側）と数値が
  同じだが意味空間が違う（general=SUCCESS 側）。別定数として持つ。
- `SecureSession::send_close_session(&mut self)`（pub, async）:
  新規 exchange id で `encode_status_report(0, 0, 2)` を `seal(needs_ack=false)` し
  `transport.send_to` 1 発。送信エラーは無視して返る（best-effort）。
  実装は `send_standalone_ack()`（`session.rs:236`）と同型。

### 2. trait 配線（mat-native）

- `NodeConn`（`lib.rs:43`）と `SubscribeConn`（`lib.rs:164`）の両 trait に
  `async fn close(&mut self)` を追加。デフォルト実装 = no-op（fake が自動で満たす）。
- `SessionConn` / `SubscriptionSession` は `session.send_close_session()` に委譲。
- テスト fake（`test_support.rs` の FakeConn/FakeSubConn、`ops.rs` FailingConn、
  `native_direct.rs` 内テスト）は close 呼び出しを記録できるようにする。

### 3. 呼び出し 3 箇所

1. **mat 直経路**（`mat/src/native_direct.rs`）: `run_op` 配下の
   establish → op を小ヘルパに集約し、**op の成否によらず** conn を手放す前に
   `close()`。対象は `establisher.establish(..)` の全 16 サイト（`diag mesh` は
   全ノード分のセッションを作るため効果最大）。
2. **matd warm op**（`matd/src/native.rs`）: `drop_session()`（`:159`）と
   `with_session()` 内の `*guard = None` 3 箇所（`:223` Timeout / `:260` retry 失敗 /
   `:278` lazy re-establish）で、drop 前に `close()`。Timeout arm は相手が死んでいる
   可能性が高いが、待ちゼロなのでコスト無しで送る。
3. **matd 購読**（`matd/src/subscription.rs`）: `run_subscription_once()` の pump 終了
   全経路（`PumpEnd::OpGrace` / `BornDeadSilence` / `Silence` / セッションエラー）で
   conn を手放す前に `close()`。旧購読の論理削除は従来通り KeepSubscriptions=false が
   担い、本変更はセッションの後始末を足すだけ。

## テスト

- unit（mat-controller）: `send_close_session` が (a) needs_ack=false で 1 回だけ
  送る (b) payload が `parse_status_report` で (0, 0x0000, 2) に round-trip する。
- fake 統合: 直経路 op（成功/失敗両方）・warm drop 各経路・購読 pump 終了各経路で
  close が 1 回呼ばれること。
- 既存テストの回帰なし（`task check`）。
- **実機 E2E（マージ前必須、memory: e2e-before-merge）**: jarvis で
  購読 established を確認 → `MAT_FABRIC_INDEX=2 mat diag thread --node 16` →
  keep-alive が購読ソケットに届き続け matd の無音 deadline が**発火しない**ことを
  `matd status`（for_s が継続増加）と pcap で確認。修正前は同手順で ±1 秒で死ぬ
  ことが実証済みなので、生存 = 修正の直接証明になる。

## リスク / 非目標

- 追加コストは teardown 毎に暗号化データグラム 1 発のみ。ロストしても現状と同じ
  （悪化なし）。デバイス側は CloseSession 受信でセッションを即 evict する（意図通り）。
- 非目標: セッション統合、resumption、per-node deadline 調整、受信側 CloseSession
  処理（コントローラがデバイスから受ける CloseSession への応答は現状維持）。
