# C2: README 二層化 ＋ 真実性スイープ ＋ mdBook サイト 設計

- **日付**: 2026-07-23（改訂 2026-08-05: mdBook サイト化を追加、Quickstart 範囲を確定、
  行数・セクション構成を現状に更新）
- **状態**: 承認済み（実装計画待ち）
- **文脈**: このリポジトリ（`mat`）を英語圏 OSS として発見・採用されやすくする取り組み。
  最終ゴールは **A（discoverability: crates.io / 刺さるピッチ / デモ）**。本 spec は
  その足場となる **C2** — 実態とズレたドキュメント/メタの一掃、README の「ピッチ層 ＋
  リファレンス層」二層化、そしてリファレンス層を mdBook で GitHub Pages に公開する
  “器づくり”までを対象とする。

## 背景と問題

- リポジトリは既に public / MIT / `repository` メタ設定済み。「英語化」は課題ではない。
- しかし実態とのズレが残る：
  - `crates/mat/Cargo.toml` と `crates/mat-core/Cargo.toml` の description が今も
    chip-tool 前提の現在形（chip-tool は 0.22.0 / M8c-3 で退役し native のみ）。
  - README は 1545 行（2026-08-05 時点）。`## Status → Requirements → Install` の直後、
    55〜1111 行が巨大なコマンドリファレンス（約1057行）。**30秒ピッチも example-first の
    Quickstart も無く**、初見の読者がいきなりマニュアルの壁に当たる。
- OSS として人目に付く箇所の古い記述は信頼性に響く。A に入る前に足場を固める。

## スコープ

### 本 spec に含む（C2）
1. 真実性スイープ（stale な chip-tool 現在形記述の修正、歴史記述は温存）。
2. README を approach ②（ピッチ README ＋ 詳細を `docs/` へ分離）で二層化。
3. リファレンス層の mdBook サイト化 ＋ GitHub Pages 自動デプロイ。
4. 分離に伴う相対リンク整合。
5. 検証（`task check` ＋ リンク切れ検査 ＋ mdBook ビルド検査 ＋ 通しレビュー）。

### 本 spec に含まない（＝A に残す / YAGNI）
- 刺さるピッチ**コピー本体**の作り込み（C2 は骨組みの箇条書きまで）。
- crates.io 公開、追加バッジ、デモ GIF / asciinema、ロゴ、repo description の作り込み。
- 機能変更・コード挙動変更（本作業はドキュメント・Cargo メタ・CI workflow 追加のみ。
  `src/` のロジックは触らない）。

### 前提・非対象
- セッション開始時点で未コミットの `CLAUDE.md` 変更（ユーザー編集）は本作業と無関係。
  **コミットに含めない**。

## 設計

### Part 1 — 真実性スイープ（truth pass）

方針: 実態（chip-tool 退役・native のみ）とズレた**現在形**の記述を修正する。
「chip-tool は退役した」という**歴史的記述は温存**する。「chip-tool 記法」
「chip-tool 互換 INI」のような互換仕様への参照は正しい現在形であり対象外。

| 対象 | 方針 |
|---|---|
| `crates/mat/Cargo.toml` description | chip-tool 前提の現在形を native backend 前提の文言へ修正 |
| `crates/mat-core/Cargo.toml` description | chip-tool 前提を実態（`mat` の JSON schema / error・exit-code 分類 / credential store）へ修正 |
| README の chip-tool 言及（約20箇所） | 退役の歴史記述・互換仕様参照は残す。現在形で「chip-tool が今のバックエンド」と読める箇所のみ修正（分離後の移設先で適用） |
| ARCHITECTURE.md | 設計記録として原則温存。現在形で「今のバックエンド」と読める箇所のみ spot-fix（監査。ほぼ無い想定） |
| CLAUDE.md | 既に「retired」と正しく枠付け済のため原則対象外。present-tense 誤りがあれば直す |

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
4. **Quickstart（新設・目玉）** — example-first。**`install → fabric init → commission →
   on / read` の全工程**（2026-08-05 決定: commission まで含める。初見の読者がゼロから
   動かす導線を README 内で完結させる）と、その **JSON 出力例**（stdout の schema が
   一目で伝わる形）。出力例のダミー値は public repo 規律に従う（RFC 5737
   `192.0.2.0/24`、架空 node_id 等）。
5. Install / Requirements — 既存を流用（必要なら軽く整える）
6. **Documentation（新設 TOC）** — ドキュメントサイト URL を主導線とし、補助として
   リポジトリ内 `docs/*.md` への相対リンク表も併記（サイト未達でも GitHub 上で読める）
7. Status — 既存を流用・簡潔化
8. Contributing / License — 既存を流用

**`docs/` へ移設（リファレンス層、5ファイル）**:

| ファイル | 移設元セクション（2026-08-05 時点の行範囲） |
|---|---|
| `docs/commands.md` | `## Commands`（55〜1111 行の全リファレンス） |
| `docs/configuration.md` | `## Credential store` ＋ `## Aliases` ＋ `## Subscriptions` |
| `docs/errors.md` | `## Errors and exit codes` ＋ `## Logs (stderr)` |
| `docs/backend.md` | `## Backend`（interface 自動検出・環境変数含む） |
| `docs/development.md` | `## Development` ＋ `## Manual E2E` |

各 `docs/*.md` は先頭に H1 見出しを持つ。**「← README に戻る」導線は置かない**
（サイト上では README がページとして存在せずリンクが壊れる。サイトは mdBook の
サイドバー、GitHub 上はパンくずで導線が足りる）。

### Part 2b — mdBook サイト（GitHub Pages）

リファレンス層 5 ファイルをそのまま章とする mdBook サイトを
`https://nogu3.github.io/mat/` で公開する。ツールは mdBook
（Rust エコシステム標準、単一バイナリ、検索内蔵、Node 不要）。

- **`book.toml`（リポジトリ直下）**: `src = "docs"`、`title = "mat"`、
  `git-repository-url` を設定。
- **`docs/SUMMARY.md`**: 章立て = Introduction（`docs/index.md`）→ Commands →
  Configuration → Errors and exit codes → Backend → Development。
- **`docs/index.md`（新設ランディング）**: What is mat の短い要約 ＋ GitHub リポジトリ
  への導線。刺さるコピー本体は A（README と同じ骨組みルール）。
- **ARCHITECTURE.md はサイトに含めない**: 設計記録・ロードマップの内部文書として
  repo-only のまま。サイトはユーザー向けリファレンスに限定。
- **`.github/workflows/docs.yml`（新設）**: main への push（paths: `docs/**`,
  `book.toml`, workflow 自身）で mdBook の**プリビルトバイナリを取得**（`cargo install`
  はしない）→ `mdbook build` → 公式 Pages フロー（`actions/upload-pages-artifact` ＋
  `actions/deploy-pages`）でデプロイ。
- **一度きりのセットアップ**: GitHub Pages の source を「GitHub Actions」に設定し、
  リポジトリの homepage メタをサイト URL にする（`gh repo edit --homepage`）。
- **公開範囲の担保**: `docs/superpowers/`（内部 spec・計画、日本語）がサイトに
  **含まれない**こと。mdBook は `SUMMARY.md` 未掲載の `.md` をレンダリングしない仕様
  だが、公開物なのでビルド出力の機械検査を Part 4 に置く。

### Part 3 — リンク整合

- README 内アンカー（例 `[Backend](#backend)`）→ `docs/*.md` への相対リンクに張り替え。
- 移設コンテンツ間の相互参照（例 Commands 内から「Groupcast below」）もクロスファイル化。
  `docs/*.md` **間**の参照は相対リンク（GitHub と mdBook サイトの両方で機能する）。
- `docs/*.md` から README / ARCHITECTURE への参照は **GitHub の絶対 URL**
  （`https://github.com/nogu3/mat/blob/main/...`）にする。相対 `../README.md` は
  サイト上で 404 になるため使わない。
- `CLAUDE.md` の `README.md#errors-and-exit-codes` → `docs/errors.md` に追従。
- `ARCHITECTURE.md` → README への深リンクを監査・修正（移設先の `docs/*.md` へ）。

### Part 4 — 検証

- `task check`（fmt:check ＋ clippy ＋ test）通過。Cargo description 変更が既存を壊さないこと。
- **相対リンク切れ検査**: 全 `*.md` から `](#...)` `](./...)` `](../...)` `](docs/...)`
  `](README.md#...)` を grep し、リンク先ファイル/アンカーの実在を確認。
- **mdBook ビルド検査**: `mdbook build` がローカルで通り、出力（`book/`）に
  `docs/superpowers/` 由来のページが存在しないこと。
- 新 README を先頭から通しで人手レビュー（Quickstart のコマンドが現行 CLI と一致するか含む）。
- デプロイ後、サイトの主要ページと検索が生きていることを目視確認。

## 受け入れ条件

1. `crates/mat/Cargo.toml` / `crates/mat-core/Cargo.toml` の description が chip-tool
   現在形を含まず実態を表す。
2. README.md が二層の上層のみ（ピッチ骨組み ＋ Quickstart ＋ Install/Requirements ＋
   Documentation TOC ＋ Status ＋ Contributing/License）で構成され、目安 200 行以内。
3. `docs/commands.md` `docs/configuration.md` `docs/errors.md` `docs/backend.md`
   `docs/development.md` が存在し、旧 README の該当内容を欠落なく保持する。
4. `book.toml` / `docs/SUMMARY.md` / `docs/index.md` / `.github/workflows/docs.yml` が
   存在し、`mdbook build` がローカルで通る。
5. mdBook ビルド出力に `docs/superpowers/` 由来のページが無い。
6. デプロイ後 `https://nogu3.github.io/mat/` で主要ページが閲覧でき、リポジトリの
   homepage メタがサイト URL を指す。
7. リポジトリ内の相対リンク切れがゼロ（Part 4 の grep 検査で確認）。
8. `task check` が通る。
9. `src/` のコード挙動は不変（本作業はドキュメント・Cargo メタ・CI workflow のみ）。
10. セッション開始時の未コミット変更（CLAUDE.md のユーザー編集）を巻き込まない。

## リスクと緩和

- **移設時の内容欠落**: 移設は「切り貼り」であり書き換えではない。移設前後で該当行数/
  内容を突き合わせ、真実性スイープの修正のみを差分とする。
- **リンク切れの見落とし**: Part 4 の grep 検査を受け入れ条件に格上げして機械的に潰す。
- **内部 spec の意図しない公開**: `src = "docs"` の下に `docs/superpowers/` が同居する。
  mdBook の仕様（SUMMARY 未掲載 .md は出力しない）に依存せず、ビルド出力の機械検査
  （受け入れ条件 5）で担保する。
- **外部からの deep link 破損**: README のセクションアンカーが外部に貼られている可能性。
  影響は限定的とみなし対応しないが、主要セクションの Documentation TOC で導線を担保する。
- **Pages 初回セットアップの手作業**: Pages source の設定は一度きりの操作として
  実装計画に明記し、デプロイ検証（受け入れ条件 6）で漏れを検出する。
