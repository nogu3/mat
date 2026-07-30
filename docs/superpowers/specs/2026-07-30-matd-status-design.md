# `matd status` — 購読とデーモンの現況を問う観測口（安定性監査 Tier 2 ⑥）

日付: 2026-07-30
対象: 安定性監査バックログ（2026-07-25）Tier 2 ⑥「購読の現況を問う口が無い」

## 背景と目的

matd の常駐購読は established / born-dead / backoff 再試行のライフサイクルを
持つが、その現況を問う口が無い。運用では `ss -uanp` の UDP ソケット数で
購読の生存を**間接推定**しており、これはこの欠落の代替でしかない
（監査⑥の指摘そのもの）。E2E 検証・障害切り分けのたびにソケット台帳の
算術（op×N + 購読×N + group1 + mDNS1）で判断している。

`matd status` を追加し、購読のノード別状態とデーモン基本情報を 1 回の
socket 往復で JSON として返す。op ログ（1.3.0）を 1〜2 週観測してから
Tier 2 の残り（⑤⑧⑨⑩）を選び直す計画の、観測そのものの質を上げる下地。
⑨（IM デコード失敗で購読が静かに死ぬ）の発生も status で観測可能になる。

## CLI / プロトコル

```
matd status
```

- `matd stop` と同型: matd バイナリのサブコマンドとして socket へ
  `{"op":"status"}` を 1 行送り、応答 JSON 1 行を stdout へそのまま出す
  （純粋 JSON、装飾なし）。
- `main.rs` の `send_shutdown` は `send_admin_op(socket, op)` に一般化して
  stop / status で共用する。
- プロトコルに `Op::Status` を追加（`Ping` / `Shutdown` と同じ node 無し
  admin op。`name() = "status"`。dispatch 内で完結し、デバイス・ワイヤには
  一切触れない）。
- matd 不在時は `matd stop` の前例踏襲:
  `{"error":{"kind":"other","detail":"matd not running at ..."}}` で exit 1。
  `matd_unavailable` / exit 13 は `mat listen` 専用の契約なので広げない。
- `mat` 側には入れない（入り口は matd のみ。運用は jarvis 上の ssh 経由で
  足りる。mat の CLI 面積と README/スキーマ説明を増やさない）。

## 状態レジストリ — SubHealth 拡張

購読ライフサイクル状態は現在 `node_subscription_loop` /
`run_subscription_once` のローカル変数に閉じている。server と購読 pump の
共有点として既に両側へ Arc 配線済みの `SubHealth` に、node_id →
`NodeSubStatus` のマップを足す（新しい配線ゼロ。SubHealth の役割は
「op 相関ヘルス」から「購読ランタイム状態の共有点」へ広がる — doc
コメントで明示する）。

```rust
// subscription.rs
pub struct SubHealth {
    clusters: Vec<u32>,
    pending: Mutex<HashMap<u64, Instant>>,
    values: Mutex<HashMap<ValueKey, Value>>,
    // 新規: 購読ライフサイクル状態（status op が読む）
    status: Mutex<HashMap<u64, NodeSubStatus>>,
}

enum NodeSubStatus {
    /// spawn 直後〜初回確立前のみ。喪失後の再試行中は Down のまま
    /// （attempts が増える — down_since / classify_failure と同じ見方）。
    Establishing { since: Instant },
    /// 購読成立中。last_device_msg はデバイス発メッセージ
    /// （keep-alive 含む）受信のたび更新。
    Established {
        since: Instant,
        subscription_id: u32,
        max_interval_s: u16,
        last_device_msg: Instant,
    },
    /// 確立失敗 or 購読喪失で backoff 中（再確立まで持続）。
    Down {
        since: Instant,
        attempts: u32,
        backoff_s: u64,
        /// kind + detail（応答の {"kind","detail"} にそのまま写す）。
        last_error: MatError,
    },
}
```

- 遷移点は既存のログ出力箇所と 1:1（**ログに出す状態遷移はレジストリにも
  書く**、が規律）: spawn 時 = Establishing、`subscription established` =
  Established、確立失敗 / `report pump ended` = Down（`last_error` は
  確立失敗の `MatError` kind/detail、または pump 終了理由
  `op-grace` / `born-dead` / `silence` / セッションエラー detail の文字列）。
- pump のメッセージ受信毎の `last_device_msg` 更新は Mutex タッチ 1 回
  （レポートは低頻度なのでコストは無視できる。既存 `clear_pending` と同じ
  呼び出し密度）。
- 書き込みは `health.mark_*()`、status op は `health.status_snapshot()` で
  読む。ephemeral なプロセス内状態のみ（設計ルール 4 の永続状態には該当
  しない）。

## 応答スキーマ

```json
{
  "timestamp": "2026-07-30T12:34:56+09:00",
  "version": "1.12.0",
  "uptime_s": 86400,
  "native": "ready",
  "iface": "wpan0",
  "fabric_index": 1,
  "store": "/home/user/.config/mat",
  "subscribed_clusters": ["onoff", "occupancysensing"],
  "listen_clients": 1,
  "nodes": [
    {"node_id": 5, "state": "established", "for_s": 3600,
     "subscription_id": 7, "max_interval_s": 300,
     "last_device_msg_ago_s": 42, "pending_op_ago_s": null},
    {"node_id": 6, "state": "down", "for_s": 120, "attempts": 14,
     "backoff_s": 60, "last_error": {"kind": "unreachable", "detail": "..."}},
    {"node_id": 7, "state": "establishing", "for_s": 2}
  ]
}
```

- `native`: 正常時 `"ready"`。`NativeState::Unavailable` なら保持している
  構築エラーの `{"kind": "store_missing", "detail": "..."}`（このとき
  購読マネージャは未稼働なので `nodes` は空 — 「matd は生きているが
  native が死んでいる」ケースが status から直接見える）。
- `subscribed_clusters`: subscriptions.toml 由来（`mat-core::ids` にあれば
  chip-tool 記法名、無ければ数値 — listen イベントと同じ規律）。
  ファイル無し = full wildcard は `null`。
- `listen_clients`: イベント broadcast の `receiver_count()`。注意:
  現状 `main.rs` は `_events_rx` を名前付き束縛でプロセス生存中ずっと
  保持しているため、そのままだと 1 過大になる — 初期 receiver を即 drop
  する（`drop(_events_rx)` か束縛を `_` に変える）修正を含める。
- 期間系は全て「今からの経過秒」（`for_s` / `..._ago_s`）— ログの
  `down_s` / `silent_s` と同じ流儀。ISO タイムスタンプは応答生成時刻の
  `timestamp` のみ（内部時計は `tokio::time::Instant` で ISO 変換不能な
  ため、経過秒が正直な表現）。
- `pending_op_ago_s`: SubHealth の op 相関 pending（未消化の状態変更 op
  からの経過秒）。通常は `null`、値が入っていれば「op は成功したのに
  デバイス発が来ていない」観測中の瞬間。
- `down` の `last_error`: 確立失敗は `{"kind","detail"}`、pump 終了理由は
  `{"kind":"other","detail":"born-dead: ..."}` の形に正規化。
- `nodes` は node_id 昇順で安定出力。
- デーモン基本情報（version / 起動時刻 / iface / fabric_index / store）は
  起動時に組む `DaemonInfo` 構造体を `serve` へ渡して参照する。
  version は `env!("CARGO_PKG_VERSION")`。

## エラー

新種なし。status op 自体は失敗しない（レジストリ snapshot と定数の JSON
化のみ）。socket 接続不能は上記のとおり `other` / exit 1。

## テスト

- protocol: `{"op":"status"}` のパース（node 無し・`name()` 一致・
  log アクセサ None）。
- SubHealth: `mark_*` 遷移と `status_snapshot()` の単体テスト
  （Establishing → Established → Down → Established の往復、
  last_device_msg 更新、snapshot の JSON 形）。
- manager 統合（FakeEstablisher、既存足場 `spawn_manager` 流用）:
  priming 受信後の snapshot が `established`（subscription_id 付き）、
  `note_op` で殺した後は `down`（attempts / last_error 付き）、再確立で
  `established` に戻る。
- server dispatch: status op が Ready / Unavailable 両方で期待スキーマを
  返す（既存 dispatch テスト群と同型。Unavailable では nodes 空 +
  native にエラー形）。
- 実機 E2E（マージ前必須）: jarvis の隔離 matd で status を叩き、
  `ss -uanp` のソケット台帳と `nodes` の established 集合が整合すること、
  ノード沈黙誘発で down → 再確立の遷移が status に現れることを確認。

## ドキュメントとリリース

- README: matd 節に status サブコマンドとスキーマ例（ダミー値のみ —
  RFC 5737 / 例示 node_id）。
- ARCHITECTURE.md に監査⑥の記録追記。バージョン 1.12.0。
- マージ後: jarvis デプロイ + 運用メモの「生存確認は `ss -uanp`」を
  status 併記へ更新。

## スコープ外

- op 実行統計（実行数・失敗数・レイテンシ）: op ログ（1.3.0）と重複する
  カウンタ蓄積は持たない。
- `mat status` サブコマンド（入り口は matd のみ）。
- 属性値キャッシュの露出（`mat read` / listen の役割）。
- 購読の手動再確立・kill 等の操作系（status は読み取り専用）。
