# C2: README 二層化 ＋ 真実性スイープ 設計

- **日付**: 2026-07-23
- **状態**: 承認済み（実装計画待ち）
- **文脈**: このリポジトリ（`mat`）を英語圏 OSS として発見・採用されやすくする取り組み。
  最終ゴールは **A（discoverability: crates.io / 刺さるピッチ / デモ）**。本 spec は
  その足場となる **C2** — 実態とズレたドキュメント/メタの一掃と、README を「ピッチ層 ＋
  リファレンス層」の二層に再構成する“器づくり”までを対象とする。

## 背景と問題

- リポジトリは既に public / MIT / `repository` メタ設定済み。「英語化」は課題ではない。
- しかし実態とのズレが残る：
  - `crates/mat/Cargo.toml` と `crates/mat-core/Cargo.toml` の description が今も
    chip-tool 前提の現在形（chip-tool は 0.22.0 / M8c-3 で退役し native のみ）。
  - README は 1309 行。`## Status → Requirements → Install` の直後、55〜929 行が
    巨大なコマンドリファレンス（約875行）。**30秒ピッチも example-first の Quickstart も
    無く**、初見の読者がいきなりマニュアルの壁に当たる。
- OSS として人目に付く箇所の古い記述は信頼性に響く。A に入る前に足場を固める。

## スコープ

### 本 spec に含む（C2）
1. 真実性スイープ（stale な chip-tool 現在形記述の修正、歴史記述は温存）。
2. README を approach ②（ピッチ README ＋ 詳細を `docs/` へ分離）で二層化。
3. 分離に伴う相対リンク整合。
4. 検証（`task check` ＋ リンク切れ検査 ＋ 通しレビュー）。

### 本 spec に含まない（＝A に残す / YAGNI）
- 刺さるピッチ**コピー本体**の作り込み（C2 は骨組みの箇条書きまで）。
- crates.io 公開、追加バッジ、デモ GIF / asciinema、ロゴ。
- 機能変更・コード挙動変更（本作業はドキュメントと Cargo メタのみ。`src/` のロジックは触らない）。

### 前提・非対象
- セッション開始時点で未コミットだった `crates/mat-native/src/lib.rs` の実験的変更
  （`SUBSCRIBE_MAX_INTERVAL_CEILING_S` 300→3600, コミット禁止）と未追跡 `thread-map.html`
  は本作業と無関係。**触らない・コミットしない**。

## 設計

### Part 1 — 真実性スイープ（truth pass）

方針: 実態（chip-tool 退役・native のみ）とズレた**現在形**の記述を修正する。
「chip-tool は退役した」という**歴史的記述は温存**する。

| 対象 | 方針 |
|---|---|
| `crates/mat/Cargo.toml` description | chip-tool 前提の現在形を native backend 前提の文言へ修正 |
| `crates/mat-core/Cargo.toml` description | chip-tool 前提を実態（`mat` の JSON schema / error・exit-code 分類 / credential store）へ修正 |
| README の chip-tool 言及（18箇所） | 退役の歴史記述は残す。現在形で「chip-tool が今のバックエンド」と読める箇所のみ修正（分離後の移設先で適用） |
| ARCHITECTURE.md（122箇所） | 設計記録として原則温存。現在形で「今のバックエンド」と読める箇所のみ spot-fix（監査。ほぼ無い想定） |
| CLAUDE.md（11箇所） | 既に「retired」と正しく枠付け済のため原則対象外。present-tense 誤りがあれば直す |

修正後の description は「description ≠ 現在形の chip-tool 記述」であることを確認する
（`grep -n "chip-tool" crates/*/Cargo.toml` が現在形の記述を返さない）。

### Part 2 — README 二層化（approach ②）

ユーザー向けリファレンスは `docs/` 直下に新設する。`docs/superpowers/`（内部の計画・spec）
とは分離する。

**新・軽量 README.md（〜180行目安）** のセクション順:
1. タイトル ＋ License バッジ
2. What is mat — 既存 intro（1〜16 行相当）を流用
3. Why mat — **骨組みの箇条書きのみ**（刺さるコピー本体は A）:
   pure JSON stdout / native pure-Rust（no chip-tool subprocess）/ one-shot ＋ 常駐 `matd` /
   optional alias 層 / AI・スクリプト親和
4. **Quickstart（新設・目玉）** — example-first。`install → fabric init → commission →
   read もしくは on` の一連コマンドと、その **JSON 出力例**（stdout の schema が一目で伝わる形）。
   出力例のダミー値は public repo 規律に従う（RFC 5737 `192.0.2.0/24`、架空 node_id 等）。
5. Install / Requirements — 既存を流用（必要なら軽く整える）
6. **Documentation（新設 TOC）** — 下記 `docs/*.md` へのリンク表
7. Status — 既存を流用・簡潔化
8. Contributing / License — 既存を流用

**`docs/` へ移設（リファレンス層、5ファイル）**:

| ファイル | 移設元セクション |
|---|---|
| `docs/commands.md` | `## Commands`（55〜929 行の全リファレンス） |
| `docs/configuration.md` | `## Credential store` ＋ `## Aliases` ＋ `## Subscriptions` |
| `docs/errors.md` | `## Errors and exit codes` |
| `docs/backend.md` | `## Backend`（interface 自動検出・環境変数含む） |
| `docs/development.md` | `## Development` ＋ `## Manual E2E` |

各 `docs/*.md` は先頭に H1 見出しと「← README に戻る」導線を持つ。

### Part 3 — リンク整合

- README 内アンカー（例 `[Backend](#backend)`）→ `docs/*.md` への相対リンクに張り替え。
- 移設コンテンツ間の相互参照（例 Commands 内から「Groupcast below」）もクロスファイル化。
- `CLAUDE.md` の `README.md#errors-and-exit-codes` → `docs/errors.md` に追従。
- `ARCHITECTURE.md` → README への深リンクを監査・修正。
- `docs/*.md` から ARCHITECTURE.md / README への相対パスが正しいこと（`docs/` 起点で `../`）。

### Part 4 — 検証

- `task check`（fmt:check ＋ clippy ＋ test）通過。Cargo description 変更が既存を壊さないこと。
- **相対リンク切れ検査**: 全 `*.md` から `](#...)` `](./...)` `](../...)` `](docs/...)`
  `](README.md#...)` を grep し、リンク先ファイル/アンカーの実在を確認。
- 新 README を先頭から通しで人手レビュー（Quickstart のコマンドが現行 CLI と一致するか含む）。

## 受け入れ条件

1. `crates/mat/Cargo.toml` / `crates/mat-core/Cargo.toml` の description が chip-tool
   現在形を含まず実態を表す。
2. README.md が二層の上層のみ（ピッチ骨組み ＋ Quickstart ＋ Install/Requirements ＋
   Documentation TOC ＋ Status ＋ Contributing/License）で構成され、目安 200 行以内。
3. `docs/commands.md` `docs/configuration.md` `docs/errors.md` `docs/backend.md`
   `docs/development.md` が存在し、旧 README の該当内容を欠落なく保持する。
4. リポジトリ内の相対リンク切れがゼロ（Part 4 の grep 検査で確認）。
5. `task check` が通る。
6. `src/` のコード挙動は不変（本作業はドキュメント/メタのみ）。
7. セッション開始時の未コミット変更（lib.rs 実験 / thread-map.html）を巻き込まない。

## リスクと緩和

- **移設時の内容欠落**: 移設は「切り貼り」であり書き換えではない。移設前後で該当行数/
  内容を突き合わせ、真実性スイープの修正のみを差分とする。
- **リンク切れの見落とし**: Part 4 の grep 検査を受け入れ条件に格上げして機械的に潰す。
- **外部からの deep link 破損**: README のセクションアンカーが外部に貼られている可能性。
  影響は限定的とみなし対応しないが、主要セクションの Documentation TOC で導線を担保する。
