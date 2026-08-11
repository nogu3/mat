# Tier 3: BTP / BLE — 実機 BLE commission の信頼性 — 設計

2026-08-11。commission/証明書監査（2026-08-06）Tier 3 の 5 件を 1 ブランチで実装する。
対象は `crates/mat-controller` の `btp.rs` / `ble.rs`（feature "ble" だが btp.rs 本体は
feature 無関係）。バージョンは 1.27.0。

## 背景

Tier 3 は BTP セッション状態機械の会計バグと BLE スキャンの検出漏れのまとめ。
中核は `peer_acked` 初期値 0 のオフバイワン: 自分の初回データも seq=0 なので
`a.wrapping_sub(peer_acked)` が 0 になり **seq 0 への ack が永久に未計上**。恒久的に
幽霊 unacked=1 が残り、(1) 実効ウィンドウが -1（window_size=1 のデバイスは 2 通目で
必ず死ぬ）(2) 全 ack 済みの健全セッションでも 15s アイドルでウォッチドッグ切断
（「ConnectNetwork 後の Thread 参加待ちが 15s 超えると commission が落ちる」という
具体症状に直結）。

## 変更 1: 送受シーケンス初期値の番兵化（btp.rs `SessionState`）

- `peer_acked: 0` → `0u8.wrapping_sub(1)`（=255、「まだ何も ack されていない」の
  番兵）。seq=0 への ack が `newly = 0.wrapping_sub(255) = 1` と正しく計上される。
- 受信側にも同型の問題があるため `last_rx_seq` も 255 初期化に揃える
  （次の期待 seq = `last_rx_seq.wrapping_add(1)` = 0。変更 2 の seq 検証が使う）。
  `last_rx_seq` を ack 値として送る経路は `pending_ack == true`（= sequenced
  フレーム受信実績あり）のときだけなので、番兵 255 が線に漏れることはない。
- `Option<u8>` 案は却下 — 送受 2 箇所を Option 化するより番兵の方が変更が小さく、
  既存の wrap 会計（`wrapping_sub` / `wrapping_add`）と自然に噛み合う。

**是正（2026-08-11、実機 E2E で発覚）**: `last_rx_seq` の 255 番兵は誤りだった。
BTP では peripheral の handshake response が暗黙に seq 0 を消費する（CHIP
BtpEngine::Init: central 側 `mRxNextSeqNum = 1`、peripheral 側は handshake
response 送信でウィンドウを 1 消費し ack を待つ）。よって初回データフレームの
期待 seq は 1 であり、`last_rx_seq` は 0 初期化（= handshake response 受信済み
相当）が正しい。255 番兵だと実機（Nanoleaf）の正しい初回フレーム seq=1 を
out-of-order 誤判定して PASE が落ちる。`peer_acked = 255` は正しいまま
（handshake request は seq を消費しない非対称）。

## 変更 2: 受信 ack / seq の妥当性検証（btp.rs `process_incoming`）— spec 準拠 close

ユーザー決定 2026-08-11: spec 準拠でセッション close（warn-only ではない）。
GATT indication は ATT 層で信頼配送されるため、ack/seq 異常は本物のバグか破損で
あり、壊れた会計のまま進めて別の場所で謎の失敗になるより即 close がよい。

- **ack 検証**: `newly = a.wrapping_sub(st.peer_acked)` が `> st.unacked` なら
  「送っていないフレームへの ack」= `BtpError::Protocol("ack for unsent frame")`。
  `newly == 0`（重複 ack、piggyback で毎フレーム同値が来る）は合法のまま。
  従来の `saturating_sub` は不正 ack を黙って飲み込んでいた。
- **seq 検証**: `s != st.last_rx_seq.wrapping_add(1)` なら
  `BtpError::Protocol("out-of-order seq")`。
- エラーは既存の呼び出し元 2 箇所（`run_session` の `Err(e)` 分岐 /
  `send_message` の `?`）がそのまま session close に落とす。`run_session` 側の
  ログ文言 "btp reassembly failed" は ack/seq 違反も通るため
  "btp protocol violation" 程度に汎用化する。

## 変更 3: BLE スキャンの PropertiesChanged 対応（ble.rs `find_commissionable`）

`adapter.discover_devices()` → `adapter.discover_devices_with_changes()` の 1 行
切替。BlueZ にキャッシュ済みのデバイスがスキャン開始後に広告を始めた場合
（=目の前でペアリングボタンを押すケース）、更新は `PropertiesChanged` でしか
来ず、現行コードは永久に検出できない。`discover_devices_with_changes()` は
プロパティ変更時にも `DeviceAdded` を再発行するため、既存のマッチループは
無変更で機能する。

bluer の再通知規律はコードから断定できない（監査時の申し送り）ため、実機
検証で「スキャン開始後にペアリングモード入り」を 1 回試して担保する。

## 変更 4: ウィンドウ待ち中の send_standalone ガード（btp.rs `send_message`）

ウィンドウ満杯待ちループ内でメッセージが完成したとき、現行は無条件に
`send_standalone` を呼ぶ（keepalive 経路には `st.unacked < params.window_size`
ガードがあるのにここだけ無い）。同じガードを追加し、満杯なら `pending_ack` を
立てたままにして後続の piggyback / keepalive flush / proactive ack に任せる。

## 変更 5: メッセージ長の宣言オーバーフロー拒否（btp.rs `send_message`）

冒頭で `msg.len() > usize::from(u16::MAX)` を
`BtpError::Protocol("message too long for btp")` で拒否。現行の `msg.len() as u16`
は 64KiB 超で宣言長が黙って壊れる（Matter メッセージは実際には遥かに小さいが、
防御として）。

## テスト（TDD、btp.rs ループバック）

- **seq-0 ack の計上**: `SessionState::new()` + tx 1 フレーム後に ack=0 を受けて
  `unacked == 0` / `peer_acked == 0` になること（`process_incoming` 直接）。
- **window_size=1 で 2 通目が送れる**: FakePeripheral（window=1）相手に 2 通
  連続送信が完了すること（監査の具体症状の回帰）。
- ~~健全セッションのウォッチドッグ生存~~（計画時に差し替え）: actor レベルの
  生存テストはバグを弁別できない。standalone ack 自体が sequenced で ack を
  誘発するため、素直に ack を返す相手とは 2.5s 周期の ack 往復が続き、バグ有り
  でも毎周期 `newly=1` の進捗が記録されて watchdog は発火しない（失われるのは
  初回 seq=0 の ack 計上 1 回だけ）。逆に相手が 15s 沈黙すれば修正後も keepalive
  ack が真に未 ack となり close する（正しい挙動）。ウォッチドッグ述語
  `unacked > 0` の健全性は上記「seq-0 ack の計上」会計テスト
  （全 ack 後に `unacked == 0` へ戻る）で担保する。
- **不正 ack で close**: 送っていない seq への ack（`newly > unacked`）で
  セッションが閉じ、Transport 側が BrokenPipe を観測すること。
- **連番飛び seq で close**: seq 0 の次に seq 2 を受けたら close。
- **64KiB 超の拒否**: `send_message` がエラーを返すこと（切り捨てではなく）。
- 既存テストは FakePeripheral が正しい連番（tx_seq 0 始まり）を刻んでいるので
  ほぼ無傷の想定。手組み `SessionState` の
  `ack_accounting_handles_seq_wrap_past_255` だけ初期値/前提の調整があり得る。
- ble.rs の切替はユニットテスト不能（bluer 実物依存）→ 実機検証で担保。

## 進め方

- ブランチ 1 本。5 項目 ≥ 4 タスクなので subagent-driven-development で実行。
- `task check` 全緑 → jarvis 通常 E2E（`*.new` で本番未置換のまま、既存規律
  どおり）→ **BLE commission 実走**: 余剰デバイスを decommission → jarvis から
  BLE+Thread で再 commission。ConnectNetwork 後の Thread 参加待ちが 15s を
  超えても切断されないこと、スキャン開始後にペアリングモード入りしたデバイスを
  検出できることを確認する。

## 監査バックログとの対応

| 監査項目 | 本設計 |
|---|---|
| btp.rs:288 peer_acked 初期値 0 のオフバイワン | 変更 1 |
| ble.rs:57 スキャンが DeviceAdded のみ監視 | 変更 3 |
| btp.rs:305/312 受信 ack 値・seq 連番の妥当性未検証 | 変更 2 |
| btp.rs:464 ウィンドウ待ち中の send_standalone が残量未確認 | 変更 4 |
| btp.rs:499 `len() as u16` 切り捨て | 変更 5 |
