# node / group / endpoint alias 解決（optional）設計

日付: 2026-07-02
ステータス: 承認済み（実装前）
改訂: 2026-07-03 — alias ファイルを TOML 化（`aliases.json` → `aliases.toml`、
toml crate、JSON フォールバックなし）。本ドキュメントのファイル名・サンプルは
この改訂を反映済み（当初設計は JSON ベースだった）。

## 背景 / 目的

`mat` は node_id / GroupId / endpoint を数値でのみ受け取り、人間向けの名前解決は
out of scope としてきた。しかし実運用では「node 5 = リビングの照明」のような対応を
毎回覚えて数値を打つのは、人にも AI エージェントにも誤りやすい。

そこで **optional な alias 解決**を CLI 層に追加する。store 配下に alias 設定
ファイル（`aliases.toml`）が**あれば**名前→数値の解決を行い、**無ければ完全に
従来動作**。ワイヤ上・chip-tool / matd に渡る値は引き続き数値のみで、名前解決は
`mat` のローカルな前処理に閉じる。

これはスコープの改訂を含む: CLAUDE.md / ARCHITECTURE.md の「名前解決は out of
scope」を「**ワイヤ・バックエンド連携は数値のみ。ローカルな optional alias
ファイルの解決だけ CLI 層で行う**」に緩める。

## alias ファイル

`<store>/aliases.toml`（store は `--store` > `MAT_STORE` > XDG の既存解決に従う。
credential KVS 配下に置くので「状態は KVS のみ」の設計原則の枠内）:

```toml
version = 1

[nodes]
living-light = 5
hall-sensor = 12

[groups]
all-lights = 258

[endpoints.living-light]
main = 1
night = 2

[endpoints.12]
pir = 3
```

- **nodes**: alias → node_id（u64）。
- **groups**: alias → GroupId（u16）。
- **endpoints**: ノード配下定義。外側キーはノード alias または node_id の数字
  文字列、内側は alias → endpoint 番号（u16）。endpoint 番号はノードごとに意味が
  違うため、グローバル辞書にはしない（誤爆防止）。
- ファイルが無い / セクションが無いのは**正常**（解決なしで従来動作）。
- 壊れた TOML・スキーマ不一致は `store_parse`（exit 10）。
- **alias 名の純数字は禁止**（ロード時に検証、違反は `store_parse`）。数値指定と
  の衝突・シャドーイングを構造的に排除する。空文字も禁止。
- 同一セクション内のキー重複は TOML の仕様上パースエラー（`store_parse`）になる。
  `nodes` と `groups` で同名は可（名前空間が別）。

編集は手編集（AI エージェントが書くのも想定）を基本とし、CLI からの書き込み経路は
`mat commission --alias` のみ（後述）。

## 解決ルール

対象引数と解決先:

| 引数 | コマンド | 解決先 |
|---|---|---|
| `-n/--node` | read / write / invoke / describe / on / off / color-temp / open-window / diag thread / diag node | `nodes` |
| `--nodes`（複数） | group provision | `nodes`（各要素独立に解決） |
| `-g/--group` | group provision / group invoke | `groups` |
| `-e/--endpoint` | node を取る全コマンド | `endpoints`（当該ノードの定義） |

- 値が**数値として parse できればそのまま使う（最優先・従来互換）**。できなければ
  aliases.toml を引く。
- 未知の alias は **exit 2（CLI 引数エラー）**。stderr の structured error は
  `{"error":{"kind":"other","detail":"unknown node alias 'x' (known: living-light, hall-sensor)"}}`
  のように既知 alias を列挙して AI が自己修復できる具体性にする。
- alias 指定が来たのに aliases.toml が無い場合も同様に exit 2
  （`detail` に「no aliases.toml in store」を含める）。
- endpoint alias の解決キーは「ユーザーが `-n` に渡した表記」ではなく**解決後の
  node**: `endpoints` の外側キー（ノード alias / 数字文字列）を node_id に正規化
  して引く。`-n 5 -e main` でも `-n living-light -e main` でも同じ結果になる。
- endpoint alias 解決は当該ノードの定義のみを見る。他ノードの定義にある名前を
  渡してもエラー（exit 2）。
- 解決は **clap parse 直後に `main.rs` のディスパッチで一括実施**。
  `commands/*.rs` の関数シグネチャ、matd プロトコル、chip-tool へ渡る値は
  すべて従来どおり数値のまま（matd 側変更ゼロ）。

## commission --alias（作成経路）

`mat commission --alias <name>` を追加:

- commission **成功後**に aliases.toml の `nodes` へ `<name> → 採番 node_id` を
  追記して保存（ファイルが無ければ作る）。
- 名前の妥当性（純数字でない・空でない・未使用）は **commission を始める前に
  事前検証**し、NG なら exit 2。commission 成功後に alias 書き込みだけ失敗する
  中途半端な状態を作らない。
- `--alias` 無しの commission は aliases.toml に触れない。

alias の削除・改名は手編集で行う（`mat alias` 管理サブコマンドは YAGNI、
必要になったら追加）。

## 実装配置

- **mat-core** に `alias.rs` 新設:
  - `AliasBook::load(store_root) -> Result<AliasBook, MatError>`
    （ファイル無し → 空の book、パース失敗 → `store_parse`）
  - `resolve_node(&str) -> Result<u64, MatError>` / `resolve_group(&str) -> Result<u16, MatError>`
  - `resolve_endpoint(node_id: u64, &str) -> Result<u16, MatError>`
  - `insert_node_alias(&mut self, name, node_id)` + `save(store_root)`
    （commission --alias 用）
  - 数値パススルー（`"5"` → 5）は resolve 側で吸収し、呼び出し元は常に
    resolve を通すだけにする。
- **mat の cli.rs**: 対象引数の型を `u64/u16` → `String`（`--nodes` は
  `Vec<String>`）に変更。ヘルプ文に「数値または alias」を明記。
- **mat の main.rs**: ディスパッチで `AliasBook::load` →各引数を解決してから
  既存の `run(...)` を数値で呼ぶ。alias を1つも使っていない場合でも load は
  走るが、ファイル無しは即 empty book なのでコスト無視できる。

## 出力・エラー・ドキュメント

- stdout の JSON スキーマは**不変**（`node_id` / `endpoint` / `group_id` は数値の
  まま）。alias のエコーバックは入れない（欲しくなったら optional フィールドを
  後付け）。
- exit code 表への追加は無し（未知 alias = 既存の 2、壊れ aliases.toml = 既存の
  10 に載る）。
- CLAUDE.md「Scope reminders」/ ARCHITECTURE.md の out of scope 記述を改訂
  （名前解決の carve-out を明記）。README に aliases.toml の書式・解決ルール・
  `commission --alias` を追記。

## テスト

- **単体（mat-core `alias.rs`）**:
  - 数値パススルー / nodes・groups・endpoints の alias ヒット
  - 未知 alias → エラー（detail に既知 alias 列挙）
  - ファイル無し → 空 book（数値パススルーは通る、alias はエラー）
  - 純数字 alias / 空文字 alias → `store_parse`
  - 壊れ TOML → `store_parse`
  - endpoint の外側キーが alias 表記でも数字文字列でも同じ解決になる
  - `insert_node_alias` の重複検出と保存 / 再ロード round-trip
- **統合（`crates/mat/tests`、fake-chip-tool）**:
  - `-n <alias>` 指定で chip-tool に数値 node_id が渡る
  - `-e <alias>`（ノード配下定義）解決
  - `group invoke -g <alias>` 解決
  - 未知 alias → exit 2 + structured error
  - `commission --alias` が aliases.toml を作成する / 重複名は事前に exit 2
- 既存テストは全て無変更で通ること（optional 性の確認）。

## スコープ外

- cluster / attribute / command 名の alias（もともと chip-tool の名前文字列を
  渡すため不要）。
- 「リビングの照明ぜんぶ」のような論理グループ解決（従来どおり上層の責務。
  `groups` alias は wire GroupId の別名にすぎない）。
- `mat alias` 管理サブコマンド（list / set / rm）— 手編集で足りる（YAGNI）。
- discover 出力への alias 付与 — 今回はしない。
- matd 側の変更 — 不要。
