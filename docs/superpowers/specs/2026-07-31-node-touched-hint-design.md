# node-touched ヒント — 直経路 op 後の購読即時張り直し（Issue #20 実効修正）

日付: 2026-07-31 / 対象バージョン: 1.15.0

## 背景 / 問題

Aqara FP300 (node16) は購読レポート/keep-alive を「同一ファブリックの最新 CASE
セッション」に送る。直経路 op（diag 等）が作るセッションが最新になると、以後の
レポートがそこへ吸われて購読が silent 死し、matd は 330s 無音 deadline まで盲目
（= 書斎ライト最大 5.5 分「つかない」）。

1.14.0 の CloseSession（teardown 時の後始末送信）は実機 E2E 不合格: 双方向 pcap
で CloseSession(38B) がワイヤに乗っているのに、FP300 は 4 分後の keep-alive を
閉じたセッションへ送って死んだ（2 回再現）。FW が CloseSession を無視するか、
SED の indirect 配送が次回起床＝手遅れにしか届かないか — どちらにせよ
**デバイス側の行儀に期待する修正は成立しない**。CloseSession は行儀の良い
デバイス向けの衛生として維持する（Issue #20 コメント 2026-07-31 参照）。

## 決定

**セッションを新設した側が matd に自己申告し、matd は該当ノードの購読を即座に
張り直す。** デバイスの再アンカー挙動を止めるのではなく、「最新セッション」を
即座に生きた購読セッションへ塗り替える。盲目窓 = 5.5 分 → 再購読 1 回分
（実測 CASE+priming ≈ 1.5〜10 秒）。取り逃した遷移は既存の priming diff 救済が
従来通り回収する。

対象は 2 経路:

1. **mat 直経路 op**（外部プロセス）: op 完了後、matd ソケットへ
   `node-touched` ヒントを fire-and-forget 送信。
2. **matd 自身の warm op セッション新設**（2026-07-30 17:09 の hold-time write
   事故と同型）: cold establish で op を実行した後、同じ再購読トリガを内部呼び出し。

### 却下した代替案

- op 相関グレース(10s)に乗せる — 静穏な部屋では健全購読でも 10s 無発信のため
  結局張り直しになり、同じ結果を複雑に達成するだけ。
- diag を matd 経由に許す — 「diag は常に直経路」の設計原則（README の op 一覧）を
  壊す上、matd 不在時の直経路には効かない。ヒント方式は原則を保ったまま同じ効果。
- セッション統合（chip-sdk 構造）— 引き続き大工事。将来課題のまま。

## 実装

### 1. mat 側（crates/mat）

- 直経路 op の終了処理（1.14.0 で導入した「成否によらず close」の直後）に追加:
  **CASE セッションを実際に確立した場合のみ**、matd ソケットが存在すれば
  `node-touched` を送る。
  - ソケット発見は既存の経路選択（matd 自動発見）と同じロジックを再利用。
  - fire-and-forget: 短いタイムアウト、失敗は無視（tracing debug のみ）。
    op の exit code / stdout JSON には一切影響しない。
  - matd 経由で実行された op は対象外（matd 自身が知っている — 経路2で扱う）。
  - `diag mesh` は触った全ノードについて 1 件ずつ送る（全購読の張り直しに
    なるが、手動デバッグ操作なので許容 — spec 判断）。

### 2. matd ソケットプロトコル（crates/matd）

- 新 op `node-touched`（引数: node_id）。応答は即時 ack（再購読完了を待たない）。
- 受信時の挙動（内部トリガ＝経路2も同じ関数に集約）:
  - 該当ノードの購読 pump が稼働中 → pump を即終了（終了理由
    `touched: direct-path session superseded` — 既存の「subscription lost;
    resubscribing」フローに乗せる）→ **バックオフ無しで即再確立**。
  - バックオフ待ち（down）中 → バックオフを打ち切って即再試行。
  - 再確立が既に進行中 → フラグが残り、確立直後の pump が次スライスで
    Touched 終了して**もう1サイクル**張り直す（実装の実挙動、2026-07-31 最終
    レビューで確定）。ヒント到着 = より新しいセッションの存在なので保守的に
    正しく、フラグは消費されるため有界。「何もしない」ではない。
  - 該当ノードに購読が無い（台帳外 / listen 対象外）→ no-op。
  - INFO ログは external（server 受信時 `source="external"`）のみ。internal は
    `no warm session; establishing` と `pump ended (touched...)` の隣接で判読
    （専用ログは未実装 — 必要になったら追加）。
- 未知 op として旧 matd に送った場合のエラーは mat 側で握りつぶす
  （バージョン混在期の後方互換）。

### 3. 経路2の内部トリガ

- `with_session()` が cold establish（`no warm session; establishing`）で
  セッションを新設して op を完了した後、同じ再購読トリガを呼ぶ。
- 判定は「新設したか」のみ。既存 warm セッションの再利用では発火しない
  （最新セッションが変わっていないため）。
- Timeout 再確立（resend-establish）も新設に含める。

## テスト

- unit: `node-touched` の parse / 応答形。pump 終了理由の新 variant。
- fake 統合（matd）: (a) ヒント → pump 即終了 → バックオフ無し再確立の順序、
  (b) 再確立進行中のヒントが no-op、(c) 購読の無いノードへのヒントが no-op、
  (d) cold establish 後の内部トリガ発火 / warm 再利用では非発火。
- fake 統合（mat）: 直経路 op 成功/失敗後にヒント送信、establish 失敗時は
  非送信、matd 不在時は静かにスキップ。
- **実機 E2E（マージ前必須）**: 1.14.0 E2E と同一手順
  （node16 購読 established → `mat diag thread --node 16`）で、
  (1) journal にヒント起因の再購読が diag 完了から数秒以内に出る、
  (2) 330s 無音 deadline による「subscription lost」が出ない、
  (3) 従来 5.5 分だった盲目窓が 1 回の再購読時間（≈10 秒以下）に短縮、
  を確認。加えて matd 経由 op（経路2、cold establish を誘発）でも同様に確認。

## リスク / 非目標

- 追加コスト: ヒント 1 往復（unix socket）+ 対象ノード 1 回の再購読。
  診断操作時のみ発生し、定常運転には影響しない。
- ヒントが失われた場合（matd 不在・旧バージョン等）は現状維持
  （330s deadline で回収）— 悪化はしない。
- 非目標: Issue #21（連続レポート死）は別メカニズムであり本 spec では扱わない。
  commission 経路のヒント（コミッション直後は購読が存在しないため不要）。
  セッション統合。CloseSession の撤去（衛生として残す）。
