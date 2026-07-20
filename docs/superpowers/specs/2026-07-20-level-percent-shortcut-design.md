# `mat level --percent` — 明るさショートカット（Issue #10）

日付: 2026-07-20 / 対象: `mat` / `matd` / `mat-native` / `mat-controller::im`
ユーザー承認済み設計（Issue #10 の提案を採用、下限は 0–100 許容で確定）。

## 動機（Issue #10 要約）

mando の明るさスライダー（1–100%）を Matter LevelControl `MoveToLevel`
（生値 0–254）へ落とす換算が、現状 jarvis の mando config の
`sh -c '$(( $1 * 254 / 100 ))'` ラッパーに漏れている。単位換算は protocol 層
= mat の責務（`color-temp --kelvin` / `color --rgb` と同じパターン）。
完成後 mando config は `["mat", "level", "--percent", "{brightness}"]` になる。

## CLI（`color-temp` / `color` と同型）

```
mat level --node <N|ALIAS> [--endpoint E] --percent <0..=100> [--transition DS]
mat group level --group <ID|ALIAS> [--endpoint E] --percent <0..=100>
```

- `--percent`: `value_parser!(u8).range(0..=100)`。**0 を許容**（消灯相当の
  挙動はデバイス依存。mando は 1–100 しか送らない）。
- `--transition`: 0.1 秒単位 u16、既定 0 = 即時（`color-temp` と共通の約束）。
  group 版には付けない（`group color-temp` も transition 0 固定なら合わせる —
  実装時に既存 group 形の実引数を確認して同型にする）。
- 換算: `level = round(percent / 100 * 254)`（`color` の
  `round(v / full * 254)` と同じ約束、255 は予約値）。換算は CLI 層
  （arg parse 直後）。ワイヤ・matd プロトコルには**生値 level を渡す**
  （既存の kelvin→mireds が CLI 層換算で Op は mireds、と同じ）。
- デバイス対応範囲（min/max level）外はデバイス側 clamp。mat は事前 read /
  検証をしない（既存ショートカットと同方針）。
- alias 解決（node / group / endpoint）は既存の `NodeRef` / `EndpointRef` /
  group ref の仕組みに乗るだけ。

## ワイヤ

LevelControl (cluster 0x0008) `MoveToLevel` (command 0x00)、fields は
生成テーブル準拠で positional: `level(u8), transition-time(u16),
options-mask(u16)=0, options-override(u16)=0`。ExecuteIfOff は立てない
（mask=0 → デバイスの Options 属性に従う）。`mat-controller::im` に
`encode_move_to_level_fields(level, transition)`（
`encode_move_to_color_temperature_fields` と同型）と必要な定数を追加。

## 経路

- **unicast**: matd 対応 op に `Op::Level { node_id, endpoint, level,
  transition }` を追加し、On/Off/Color/ColorTemp と同じく常時
  `is_native_hotpath`。engine（`mat-native`）に `level()` メソッド（M7 固定形
  アームと同じ形、**native マーカー必須** — M8c-3 ゲート1 の教訓）。mat 直
  経路（`native_direct`）にも同型アーム。
- **group**: `Op::GroupLevel { group_id, endpoint, level }` を
  `GroupColorTemp`/`GroupColor` と同じ専用形 native groupcast に追加
  （unacknowledged multicast、"sent" のみ報告、点灯中でないと反映されない
  制約は `group color` と同じ）。
- **プロトコル互換**: Op enum への variant 追加。旧 matd に新 op を投げると
  matd 側で decode 失敗（既存の op 追加時と同じ版ずれ挙動）。mat/matd は
  同時デプロイが前提（従来どおり）。

## 出力（stdout JSON）

`color-temp` の成功 body と同型:

- unicast: `{ timestamp, node_id, endpoint, cluster: "levelcontrol",
  command: "move-to-level", level, transition, status: "success" }`
  （既存 ColorTemp body のフィールド名に合わせて実装時に確定 — mireds→level
  の置換形）
- group: GroupColorTemp の body 同型（`command: "move-to-level"`、
  `status: "sent"`）。

## テスト（TDD）

- CLI: percent→level 換算（0→0 / 100→254 / 50→127、丸め）と arg 範囲。
- im: `encode_move_to_level_fields` の TLV バイト列（color-temp の既存
  テストと同型、生成テーブルの field 並びと一致）。
- matd: `is_native_hotpath(Op::Level)` / GroupLevel の native_group_params /
  server 統合テスト（既存 ColorTemp テストの sibling）。
- **sibling 全数確認**（0.23.1 の教訓）: color-temp が現れる箇所を grep し、
  level 版の追加漏れ（README の matd 対応リスト、cli doc、server の op 分岐、
  fixed-form アーム、native マーカー）をゼロにする。

## やらないこと

- mando 側 config の `sh -c` ラッパー撤去（マージ後、別作業。jarvis-iac /
  デプロイ作業時に実施）。
- MoveToLevelWithOnOff variant（ExecuteIfOff 相当の on/off 連動）— 需要が
  出たら別 issue。
- 明るさの読み出しショートカット（`read` で足りる）。

## 受け入れ

`task check` 全 green + レビュー（requesting-code-review）。実機 E2E は
デプロイ時のスモークで代替（ショートカットは既存 invoke 経路の薄い皮）。
完了時に Issue #10 クローズ。
