# `mat diag mesh` — Thread メッシュトポロジーの JSON 出力（1.1.0）

日付: 2026-07-23 / 対象: `mat`（CLI / commands / native_direct）/ `mat-native::ops` /
`mat-core`（新 `mesh.rs`, aliases）/ Issue [#12](https://github.com/nogu3/mat/issues/12)

## 目的 / 背景

Thread メッシュの健康度（どこ↔どこが繋がり、どのリンクが弱いか）を機械可読
JSON で一発出力する。現状は per-node の `mat diag thread` を手集計して静的 HTML
に焼き込むしかない。目的は**通信経路の弱さをソフト不具合と切り分けて特定する**
こと。mando での可視化・10 分周期の定期更新は別 issue / 別リポジトリ（本スペックの
スコープ外。mat は一発・状態レスのまま）。

## 決定（ユーザー承認 2026-07-23）

### スコープ

- mat 側のみ。新サブコマンド `mat diag mesh`。
- 収集範囲は**メッシュ全参加者**: 問い合わせは自 fabric の commission 済み
  ノードのみだが、テーブルに現れた未知参加者（HA 専用デバイス・OTBR BR）も
  ExtAddress/RLOC16 ベースの「unknown ノード」としてグラフに含める。
- 自己同定は **案 A 主（cluster 0x33 併読）+ 案 B 補完（テーブル相関）**。

### コマンドと経路

- `mat diag mesh [--nodes <N|ALIAS>...]`。省略時は store の全 commission 済み
  ノードが対象。endpoint は ep0 固定（診断クラスタの慣例）。
- 他の diag 同様**直経路のみ**。matd ソケットプロトコルには入れない
  （CLAUDE.md / README の direct-only op リストに追記）。
- 収集は逐次（ノード数 8 で数十秒想定）。per-node タイムアウトは既存 engine
  の既定に従う。

### 収集フロー

対象ノードごとに CASE 接続 1 回で 2 read:

1. **cluster 53 (Thread Network Diagnostics) wildcard read** — 既存
   `ops::diag_thread` を再利用。neighbor-table / route-table / routing-role /
   partition-id に加え、`leader-router-id` / `mesh-local-prefix` を取り込む
   （wildcard 応答には既に含まれる — 抽出キーの追加のみ）。
2. **cluster 0x33 (General Diagnostics) wildcard read**（新規パーサ）—
   `NetworkInterfaces`（attr 0x0000, list-of-struct）から Thread インター
   フェース（type = Thread）の `HardwareAddress`（8 byte）= **自 ExtAddress**。
   同エントリの IPv6 アドレス一覧から `<mesh-local-prefix> + 00ff:fe00:xxxx`
   形の RLOC アドレスを探し **自 RLOC16** を導出。

集約は純ロジック（`mat-core::mesh`）:

- 全ノードの neighbor/route-table 行から ExtAddress↔RLOC16 対を集め突合。
- 未知参加者はテーブル側から ExtAddress キーでノード化。role 推定:
  route-table に RouterId 付きで載る → `router`、neighbor-table 行の
  `IsChild` → `child`（RxOnWhenIdle=false なら `sed`）。実装はこれに加え、
  route-table に載らず neighbor-table のみで観測された参加者でも、RLOC16
  が router 型（下位10bit=0、`IsChild` でない）なら `router` と推定する
  （意図した挙動）。
- `leader-router-id` と RouterId が一致する router を `leader` にマーク。
- **案 B の実態**は未知参加者の同定・マージ。fabric ノードで 0x33 read が
  失敗した場合は自己同定不能のまま出力（そのノードのテーブルは自己同定
  無しでは他ノードと突合できず、エッジは他ノード視点の分だけ成立）。

### ラベリング

- 自 fabric ノード: `aliases.toml` の node alias を**逆引き**し `alias` を付与。
- 未知参加者: `aliases.toml` に任意の **`[thread]` セクション**を新設。
  キー = ExtAddress（16 桁 hex、大文字小文字は不問で正規化）、値 = ラベル。
  例: `"AABBCCDDEEFF0011" = "otbr-br"`。マッチしたノードに `label` を付与。
  ファイル / セクション不在 = ラベル無しで動作不変（absent-file 規律踏襲）。

### JSON スキーマ

```json
{
  "timestamp": "2026-07-23T12:34:56+09:00",
  "network": {
    "name": "MyThread", "channel": 25,
    "partition_ids": [123456], "leader_router_id": 5
  },
  "nodes": [
    { "id": "ext:0011223344556677", "ext_address": "0011223344556677",
      "rloc16": "0x1400", "router_id": 5, "role": "router",
      "node_id": 42, "alias": "hall_motion", "probed": true },
    { "id": "ext:AABBCCDDEEFF0011", "ext_address": "AABBCCDDEEFF0011",
      "rloc16": "0x2000", "router_id": 8, "role": "router",
      "label": "otbr-br" },
    { "id": "ext:1122334455667788", "ext_address": "1122334455667788",
      "node_id": 5, "alias": "bedroom_light", "probed": false,
      "probe_error": { "kind": "unreachable", "detail": "…" } }
  ],
  "edges": [
    { "a": "ext:0011223344556677", "b": "ext:AABBCCDDEEFF0011",
      "a_sees_b": { "lqi": 140, "avg_rssi": -60, "last_rssi": -58,
                    "frame_error_rate": 2, "age": 12 },
      "b_sees_a": null,
      "route": { "lqi_in": 3, "lqi_out": 3, "path_cost": 1 } }
  ]
}
```

- ノードの安定キー `id` は `ext:<HEX16>`、ExtAddress 不明なら `rloc:<hex>`
  （予約 — 現実装では未使用。RLOC16 導出は ext 正準化の成功に依存するため、
  ext 正準化が失敗するケースでは rloc16 導出も併せて失敗し、この分岐には
  到達しない）、どちらも無い fabric ノード（0x33 が読めず probe 失敗等）は
  `node:<node_id>`。
- `role`: `leader` / `router` / `reed` / `child` / `sed` / `unknown`。
  fabric ノードは routing-role（cluster 53 スカラー）から、未知参加者は
  テーブル相関から推定。
- エッジは**無向 1 本に双方向実測を併記**: `a_sees_b` = a の neighbor-table
  の b 行（= a が受信した b の電波品質。LQI 0–255 / RSSI dBm /
  FrameErrorRate %）。片側しか観測が無ければ他方は `null`。
- router–router は route-table 由来の `route`（LQIIn/LQIOut 0–3 / PathCost）
  を併記（`LinkEstablished` = true の行のみ。双方向で食い違えば a 側優先で
  1 つに正規化はせず a→b 視点の行を採用）。
- **生値のみ**。weak/strong の分類閾値は出力に焼き込まない（表示側 =
  mando の仕事）。
- `probed` は fabric ノードのみ出す（未知参加者は省略）。省略系フィールドは
  すべて absent（`null` を出さない）。ただし `a_sees_b` / `b_sees_a` は
  どちらも nullable — a/b は辞書順で決まるため観測が片側だけの場合は
  どちらが null になるかは定まらず、両方あり得る（route のみで
  neighbor-table 双方向の観測が無いエッジは両方 null）。

### エラー / exit code

- 1 ノードでも probe 成功 → exit 0。失敗ノードは JSON 内 `probe_error`。
- 全ノード probe 失敗 → 最頻の失敗 kind をトップレベルエラーへ写像
  （例: 全 unreachable → `unreachable` / exit 5。同数タイなら先勝ち）。
- 対象 0 ノード（store 空 or `--nodes` が全部未 commission）→ 既存規約:
  store 空は空グラフで exit 0、未 commission 指定は `node_not_commissioned`
  / exit 11。
- store 不在/parse は従来どおり exit 10。

### 実装配置

- `mat-core::mesh`（新規）: グラフ組み立て・ExtAddress↔RLOC16 突合・role
  推定・エッジマージの純関数群 + スキーマ構造体（Serialize）。
- `mat-core::aliases`: node alias 逆引き + `[thread]` セクションのパース追加。
- `mat-native::ops`: `general_diag_network_interfaces()`（cluster 0x33 read +
  Thread iface 抽出）を追加。`diag_thread` は流用（`leader-router-id` /
  `mesh-local-prefix` を SCALARS へ追加 — `diag thread` の出力にも増えるが
  additive でスキーマ互換）。
- `mat::native_direct`: `op_diag_mesh`（対象列挙 → per-node 収集ループ →
  mat-core::mesh へ委譲）。
- `mat::cli` / `commands::diag`: `DiagCommand::Mesh` と emit。

### テスト

- `mat-core::mesh`: 突合・role 推定・エッジマージ・部分失敗のユニットテスト
  （実機 diag thread の JSON 形状をサンプルに）。
- `mat-native`: FakeConn で cluster 0x33 パース（Thread iface 抽出 / RLOC16
  導出 / 読めないデバイス）のテスト。
- `mat`: バイナリ統合テストでスキーマ形状・exit code をピン。
- マージ前に jarvis 実機 E2E（メモリ e2e-before-merge の規律）: 全ノード
  収集が通ること、BR がグラフに現れること、弱リンクの実測が既知の実態
  （静的 HTML 版）と整合すること。

## 代替案(不採用)

- **BR (otbr) の router table を直接読む**: BR には全ルータのリンク品質が
  揃っているが、mat がプロトコル/外部コントローラを喋らない設計原則に反する。
- **相関のみ（案 B 単独）**: 追加 read 不要だが自己同定の失敗ケースが残り、
  ロジックも複雑化。0x33 併読は 1 セッション内の read 1 発で確実。
- **weak/strong 分類を mat が出す**: 閾値は表示・運用の関心事で、スキーマに
  焼き込むと後で変えづらい。生値のみとし分類は消費側へ。

## ドキュメント・波及

- README: `diag mesh` の説明・出力例・direct-only リスト・`[thread]`
  セクションを追記。CLAUDE.md の direct-only op リストにも `diag mesh`。
- バージョン: 1.1.0（additive な新機能）。
- Issue #12 は mat 側完了でクローズせず、mando 可視化・定期更新を残タスク
  としてコメント（または mando 側 companion issue へ分割）。
