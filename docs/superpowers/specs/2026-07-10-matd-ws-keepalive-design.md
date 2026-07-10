# matd ws keepalive + warm セッション温存リカバリ 設計（issue #7 / #8 短期緩和）

2026-07-10。issue [#7](https://github.com/nogu3/mat/issues/7)（アイドル 3 分超後の初回コマンドが必ず
`child_failed`）と [#8](https://github.com/nogu3/mat/issues/8)（chip-tool interactive server の
CPU 100% busy-loop）への matd 側の対応。

## 背景（確定済みの事実）

- chip-tool `interactive server`（libwebsockets）は最終トラフィックの **180 秒後**に ws PING を送り、
  PONG が **20 秒以内**に返らないと接続を閉じる（jarvis の journal で 2 回とも 180.0s / 20.0s を実測）。
- matd（`crates/matd/src/backend.rs`）はコマンド実行中（`exchange` の `next_text` ループ）しか
  ws を poll しない。tungstenite は poll されていなければ PONG を返せないため、アイドル中の PING は
  必ず無応答 → 切断。次のコマンドが死んだソケットを踏んで `Broken pipe` → `child_failed` を 1 回返す。
- さらに `run_cmdline` のエラー経路は `teardown` で**健在な子プロセスごと kill** するため、
  warm CASE セッションも毎回失われる（失敗直後の操作が遅い一因）。
- #8: chip-tool interactive server は spawn した瞬間から ws 接続の有無と無関係に CPU 1 コアを
  ほぼ 100% 消費する。原因は上流 `examples/common/websocket-server/WebSocketServer.cpp` の
  `while (mRunning) { lws_service(mContext, -1); }`（busy-spin）。上流 issue
  [project-chip/connectedhomeip#29971](https://github.com/project-chip/connectedhomeip/issues/29971)
  が 2023-10 から open のまま修正 PR なし。**子が存在する限り焼かれる**ので、matd 側の唯一の
  レバーは子の寿命（idle-timeout）。

## ゴール / 非ゴール

**ゴール**

1. アイドル後の初回コマンドが失敗しない（#7 の決定論的な 1 回失敗の解消）。
2. ws だけが死んだ場合に warm CASE セッション（＝子プロセス）を守る。
3. CPU 焼き（#8）を「使用中 + idle-timeout」に有限化する運用に耐える構造にする
   （keepalive が reap を妨げない）。

**非ゴール**

- #8 の根本修正（上流のバグ。journal 証拠を #29971 にコメントで還元するのみ）。
- バックエンド置換（Phase 5）。別セッションで議論する（matc fork 路線が最有力、matter.js が
  フォールバック — 調査結果は memory `phase5-backend-research` 参照）。
- systemd CPUQuota 等の ops 側追加緩和（いつでも独立に足せる）。

## 設計

変更は `crates/matd/src/backend.rs` と `crates/matd/src/server.rs` に閉じる。mat の JSON スキーマ・
サブコマンド・エラー kind は不変。

### 1. keepalive タスク（#7 の本丸）

- `server::serve` の reaper と並ぶ常駐タスクとして keepalive ループを追加する。周期
  `KEEPALIVE_INTERVAL = 45s`（180s の PING 周期に対し十分短く、300s 未満）。
- 各 tick で `conn` の Mutex を `try_lock` する:
  - **取れない** = コマンド実行中で ws は poll されている（PING は自動応答される）→ 何もしない。
  - **取れて ws がある** → **matd 側から ws Ping を送る**（受け身に PING を待って PONG を
    返すのではなく、こちらから 45 秒ごとに送信トラフィックを作る。chip-tool 側の
    「180 秒無トラフィックで PING」がそもそも発火しなくなり、20 秒期限との競争も消える。
    送信失敗 = 切断の即検知にもなる）。続けて短い timeout（例 100ms）で `ws.next()` を
    poll してドレインする。読めたフレームの扱い:
    - `Ping`/`Pong`/`Frame` → 無視（tokio-tungstenite が Ping への Pong を自動キューし、
      次の送信/poll で流す）。
    - `Text`/`Binary` → アイドル中に来る想定外の遅延応答。debug ログに残して捨てる
      （次コマンドへの混線を防ぐ）。
    - `Close`/`None`/`Err` → 切断を先回りで検知。**子は殺さず** ws だけ捨てる（`conn.ws = None`）。
      次のコマンドの `ensure_connected` が遅延再接続する（即時再接続はしない —
      アイドル中の再接続ループを避け、構造も単純になる）。
  - timeout 経過（何も来ない）が正常系。
- `ensure_connected` に子の生死確認を足す: `child.try_wait()` で子が既に死んでいれば
  respawn してから接続する（死んだ子に `connect_with_retry` で 20 秒粘るのを防ぐ）。
- keepalive は `last_used` を更新**しない**（reap の判定を妨げない。keepalive で延命すると
  #8 の焼きが無限化する）。
- Spawn / Connect 両モードで動かす（PING を打つのは ws サーバ側であり、子の有無と無関係）。

### 2. 温存リカバリ: 子を殺さず ws だけ張り直す

`run_cmdline` のエラー経路を、現在の「無条件 `teardown`（子ごと kill）」から次に変える:

- **送信失敗**（`ws send failed`）: ソケットは送信前から死んでいた。ws を捨てて
  `ensure_connected` で張り直し（子は温存。子が死んでいれば respawn される）、
  **1 回だけ透過リトライ**する。リトライも失敗したら `teardown` してエラーを返す。
- **受信失敗・timeout・Close**: コマンドが chip-tool に届いて実行された可能性を排除できないため
  **リトライしない**（toggle 等の二重実行を避ける）。ws を捨てて子は温存し（`conn.ws = None`、
  次コマンドで遅延再接続）、エラーはそのまま返す。遅延応答の混線は「古い ws を捨てる」ことで
  従来どおり断たれる。
- Connect モードでは「子の温存」は自明（そもそも子を持たない）。ws 張り直しのみ同じ扱い。
- **保険: 連続失敗 2 回で従来どおり `teardown`**。子を温存し続けると「chip-tool が wedge して
  全コマンド timeout」のとき永久に回復しない。`Conn` に連続失敗カウンタを持ち、成功で 0 に
  戻し、受信失敗が 2 連続したら子ごと畳んで次回フル再確立（respawn）に委ねる。

keepalive（1）が入れば決定論的な切断は消えるので、この経路は chip-tool クラッシュ等の
まれな事象への保険になる。

### 3. reap 方針（#8 の有限化）

- **実行時訂正（2026-07-10）:** 「コード変更なし」は 1 点だけ覆った。子温存化（§2）により
  「ws=None・子生存」が普通の状態になるため、`reap_if_idle` の発火条件を `ws.is_some()` から
  `(ws.is_some() || child.is_some())` へ変更した。旧条件のままだと ws だけ捨てた温存状態の子を
  reap が見逃し、#8 の焼きが止まらない（テスト `reap_kills_child_even_without_ws` で固定済み）。
- `--idle-timeout` の既定 300s は維持。reaper の周期・仕組みは不変。
- 運用変更: jarvis の `--idle-timeout 86400`（24h）を **600〜900s** に変更する
  （デプロイ手順は memory `jarvis-matd-deploy` を更新）。
- 結果: 焼きは「使用中 + 10〜15 分」に有限化。放置後の初回だけ cold start（数秒）。
- README の matd 節に「chip-tool interactive server は稼働中 CPU 1 コアを消費し続ける
  （上流 #29971）ため、idle-timeout を無制限級に伸ばす運用は推奨しない」旨の注記を足す。

### 4. 上流・issue の後始末

- connectedhomeip #29971 に jarvis の journal 証拠（180.0s/20.0s の validity timing、
  `lws_service(-1)` spin、ps 実測）をコメントで還元する。
- mat #7 / #8 に本設計へのリンクとリリースバージョンを追記し、#7 はリリース後 close。
  #8 は上流待ちとして open のまま（matd 側の緩和済みを明記）。

## テスト戦略

既存の fake ws サーバ（`crates/matd/tests/integration.rs`）を拡張する。実 chip-tool は使わない。

1. **keepalive が生存トラフィックを作る**: `keepalive_tick()` を直接呼び、fake が
   ws Ping を受信したら通過（周期タイマーはテストせず tick を直接呼ぶ —
   sleep 依存を避け決定的にする。tick の呼び出しはテストが握る `Arc` 経由でできる）。
2. **アイドル中の切断から温存回復**: fake が Close を送って接続を落とす → 次の
   `run_cmdline` が成功する（Connect モードで再接続を検証。子プロセスの温存は
   backend 単体テストで `child` が `Some` のまま変わらないことを確認）。
3. **送信失敗の透過リトライ**: 接続を落とした直後の `run_cmdline` が内部リトライで
   成功し、呼び出し側にはエラーが見えない。
4. **受信失敗はリトライしない**: fake が「受信後に応答せず切断」したとき、エラーが
   1 回返り、かつ fake へのコマンド着信が 1 回だけであること（二重実行しない）。
5. **keepalive が reap を妨げない**: 短い idle-timeout で keepalive 稼働中でも
   `reap_if_idle` が発火してセッションが畳まれる。
6. 既存テスト全通過（`task check`）。

## 受け入れ基準（実機 E2E、jarvis）— **全項目合格（2026-07-10 夕、v0.16.0）**

実測: ①294 秒放置後の 1 コマンド一発成功（旧版は必失敗）②放置中 `LWS_CALLBACK_CLOSED`
ゼロ ③最終操作 17:51 → 18:07:45 に reap 発火（`tearing down idle chip-tool session
idle=900s`）で chip-tool 消滅、cold start 2.0 秒で回復 ④group invoke（matd 経由）
off/on とも 7/7 配達。jarvis は systemd unit（`/etc/systemd/system/matd.service`）で
`--idle-timeout 900` に変更済み。

1. matd 起動 → 操作 → **4 分放置** → 次の 1 コマンドが一発成功（現状は必ず 1 回失敗）。
2. 放置中の journal に LWS の切断（`LWS_CALLBACK_CLOSED`）が出ない。
3. idle-timeout（600〜900s）超の放置後に chip-tool プロセスが消えている（ps で確認）＝
   CPU 焼きが止まっている。次の 1 コマンドは cold start で成功。
4. group 送信（`mat group invoke` 相当、matd 経由）が従来どおり 7/7 配達。

## リリース

- バージョン 0.16.0（mat / matd 同時）。aarch64-musl クロスビルド → jarvis へ配布
  （memory `jarvis-matd-deploy` / `aarch64-musl-rust-lldクロスビルド` の手順）。
- jarvis の起動引数 `--idle-timeout` を 600〜900s へ変更。
