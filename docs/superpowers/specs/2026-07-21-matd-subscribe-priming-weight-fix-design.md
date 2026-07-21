# matd 購読の priming 軽量化 + 確立失敗の観測性

日付: 2026-07-21 / 対象: `mat-controller`(im) + `mat-native` + `matd`(subscription)

## 目的

弱リンクノードで購読の再確立が数十分〜数時間失敗し続ける問題を直す。
0.25.0 本番デプロイ後の経過観察（2026-07-21 朝）で確定した root-cause への恒久修正。

## Root-cause（隔離 debug matd での実測、2026-07-21）

- full-wildcard Subscribe の priming は node6 実測で **12+ チャンク × ~34 reports
  （300+ 属性）= 数十回の連続した信頼性往復**。各往復はデバイス側 ~5s の
  exchange タイムアウト制約下にある。
- 弱リンク（node6 = 20〜47% loss）では全チャンク連続成功の確率が極小 →
  再確立は運任せで数十分〜数時間かかる。**同じ瞬間に read（1〜2 往復）は成功する**
  （対照実験で確証: subscribe 連続失敗中に read exit 0）。
- 健全リンクは速攻回復する（実測: node5 = 42 秒、node8 = 1.5 分、node11 = 2 分）。
  問題は弱リンクノード（node6/14）に限られる。
- 失敗の 3 形態: ①priming 途中 MRP ack timeout ②チャンク応答遅延→デバイス
  0x80 INVALID_ACTION ③リンク悪化時は CASE 自体が失敗。枯渇・slot 追い出しではない。
- 皮肉: priming の大半は list/struct（ACL 等）で matd は scalar-only 契約により
  **受信後に捨てる**。捨てるデータのために重いハンドシェイクを払っていた。
- 併せて観測性ギャップ: 確立前の失敗は `matd::subscription` で debug に落ち、
  本番 info ログでは「transport bound の繰り返し」しか見えず成否が判別できない。

## 決定（ユーザー承認 2026-07-21）

- **② 購読パスの設定化**: `<store>/subscriptions.toml` を最小形で前倒し。
  クラスタ絞りをコードに埋め込まない（このためだけのコード内 curated リストは
  入れない）。**ファイル無し = 従来の full wildcard（挙動不変）** — aliases.toml
  と同じ「absent-file = no behavior change」規律。
- **① 確立失敗ログは「状態遷移 + 間引き」**: 毎試行 info は常駐ノイズ（弱リンク
  ノードはバックオフ上限 5 分毎に永久に失敗し続ける）なので出さない。

## 設計

### ① 確立失敗の観測性（`matd/src/subscription.rs`）

再購読ループ（`node_subscription_loop`）に失敗ストリーク状態を持たせる:

- 成功（または起動）後の**最初の失敗**を理由付き `info` で 1 回出す
  （kind/detail 付き）。以降の連続失敗は従来通り debug。
- 未確立が **10 分**を超えたら `warn` を 1 回:
  「subscription still not established」+ 試行回数 + 最新の失敗理由。
- `established` 復帰時の既存 info に**ダウン時間（秒）と試行回数**を追加
  （盲目窓の実測がログから読めるように）。ダウン時間の起点は購読喪失時刻
  （起動直後は起動時刻）。ストリーク状態はここでリセット。

### ② `<store>/subscriptions.toml`

**フォーマット（v1: フラットなクラスタ列挙のみ）:**

```toml
clusters = ["onoff", "levelcontrol", "occupancysensing", "temperaturemeasurement"]
```

- 名前は chip-tool 記法（`mat-core::ids::resolve_cluster` で解決）。
  数値文字列（`"0x0006"` / `"6"`）も可（ids に無いクラスタの escape hatch、
  generic read/write と同じ規律）。
- **未知の名前・パース失敗・空リストは matd 起動拒否**（ambiguous iface
  autodetect と同じ「設定異常は大声で」の規律。黙って wildcard に落ちて
  弱リンク対策が無効化されるのが最悪の劣化なので、silent fallback はしない）。
- 読み込みは matd 起動時 1 回。ホットリロード無し（変更は restart）。
- per-node 粒度・attribute 粒度・DataVersionFilter は将来枠のまま（YAGNI）。
- `mat`（one-shot CLI）はこのファイルを読まない — 購読は matd 専用機能。

**ワイヤ（`mat-controller::im` + `session`）:**

- SubscribeRequest の AttributePathIB を「endpoint wildcard + cluster 指定 +
  attribute wildcard」× クラスタ数で列挙する。
- `encode_subscribe_request_wildcard(min, max, keep)` を
  `encode_subscribe_request(min, max, keep, paths)` に一般化
  （`paths` 空 = full wildcard の特殊形。既存呼び出しは wildcard のまま）。
- `SecureSession::subscribe_wildcard` はパス集合を受け取る形に拡張
  （ハンドシェイク・チャンク処理・StatusResponse 応答は無改変）。
- **プロトコル変更はここだけ**。matd socket プロトコル・`mat listen`・
  イベントスキーマ・既存 warm op 経路は無変更。

**配線（`mat-native` → `matd`）:**

- matd 起動時に `<store>/subscriptions.toml` をパースし、クラスタ ID 集合を
  `NativeBackend`（購読確立経路）へ渡す。`establish_subscription` /
  `subscribe_wildcard` の購読パラメータとして使う。

**効果の見込み:**

~9 クラスタ指定で priming は 300+ 属性 → 数十属性（1〜2 チャンク）になり、
read が通る品質のリンクなら購読も確立できる。指定クラスタ内の list 属性
（powersource の fault リスト等）は従来通り受信後に scalar-only フィルタで
落ちる（量が小さいので問題にしない）。

**契約上の帰結（README に明記）:**

toml がある場合、listen に流れるのは列挙クラスタのイベントのみ。集合外の
クラスタは listen のフィルタに指定しても一切イベントが来ない。

## テスト

- toml パース unit test: 名前解決 / 数値 / 未知名エラー / 空リストエラー /
  ファイル無し = None（wildcard）。
- `encode_subscribe_request` の TLV encode test: パス列挙形とワイルドカード形。
- FakeConn ハンドシェイクテスト: パス集合が SubscribeRequest に反映されること。
  既存 wildcard テストは無改変で通ること（挙動不変の証明）。
- ログ①のストリーク遷移 unit test（初回失敗 info / 10 分 warn / 復帰リセット）
  はロジックを純関数に切り出してテスト（時計はモック可能な形に）。

## 実機 E2E（合格条件）

- jarvis に curated toml（センサー + 照明系 ~9 クラスタ）を配置してデプロイ。
- **node6（弱リンク当人）で established になること・`mat listen` が通ること**。
- 健全ノード（node8 等）の on→listen E2E が従来通り exit 0。
- 確立失敗 info / 復帰時ダウン時間ログが journal で読めること。

## デプロイ

- jarvis の `~/.config/mat/subscriptions.toml` に curated リストを配置。
  jarvis-iac への反映もセットで行う。配置する具体リスト（家の実機構成 =
  センサー: 人感/開閉/温湿度/照度、アクチュエータ: 照明、+電池残量）:

  ```toml
  clusters = [
    "onoff",
    "levelcontrol",
    "colorcontrol",
    "occupancysensing",
    "booleanstate",
    "temperaturemeasurement",
    "relativehumiditymeasurement",
    "illuminancemeasurement",
    "powersource",
  ]
  ```
- リリース: 0.26.0（マイナー: 新設定ファイル + ログ改善）。
