# C2: README 二層化 + 真実性スイープ + mdBook サイト 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** README を軽量ピッチ層（Quickstart 付き、200行以内）に再構成し、リファレンス層を `docs/` 5ファイルへ verbatim 移設、mdBook + GitHub Pages でサイト公開する。あわせて stale な chip-tool 現在形記述を一掃する。

**Architecture:** ドキュメントとメタデータのみの変更（`src/` のコード挙動は不変）。README の各 `##` セクションを見出し境界で切り貼りして `docs/*.md` へ移し、README は新しい上層のみ書き直す。`docs/` を mdBook の `src` にし、`SUMMARY.md` 掲載の 6 章だけがサイトになる（`docs/superpowers/` は掲載しないので出力されない — ビルド出力で機械確認）。デプロイは GitHub Actions → GitHub Pages。

**Tech Stack:** Markdown, mdBook (0.4.x), GitHub Actions (`actions/upload-pages-artifact` + `actions/deploy-pages`), Task。

**Spec:** `docs/superpowers/specs/2026-07-23-readme-restructure-accuracy-sweep-design.md`

## Global Constraints

- リポジトリは **public**。実クレデンシャル・実IP・実 node_id・実証明書は書かない。ダミー値は RFC 5737 `192.0.2.0/24`、既存 README のダミー setup-code `MT:Y.K9042C00KA0648G00`、架空 node_id（5 等）を使う。
- 移設は **verbatim の切り貼り**。真実性スイープとリンク張り替え以外の文言変更をしない。
- 「chip-tool は退役した」という歴史記述、「chip-tool 記法」「chip-tool 互換 INI」という互換仕様参照は**温存**。現在形で chip-tool がバックエンドと読める記述のみ修正。
- 刺さるピッチコピー本体は書かない（骨組みの箇条書きまで。作り込みはフェーズ A）。
- 新 README は目安 200 行以内。
- `src/` のロジックは触らない。
- コミットは各タスク末尾で行い、そのタスクで編集したファイルのみ `git add`（パス明示。`git add -A` 禁止）。
- 作業ブランチ: `docs/readme-restructure-c2`（main と同期済み、spec 改訂コミット b5d549c を含む）。
- コミットメッセージ末尾に付ける定型:
  ```
  Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VCx1L8mz29knxgNHFcAzNC
  ```

## セクション → 移設先マッピング（リンク張り替えの正）

| 旧 README セクション（`##`） | 移設先 |
|---|---|
| `## Commands`（`### Discover and commissioning` 〜 `#### Ops that never route through matd` を含む全部） | `docs/commands.md` |
| `## Credential store` | `docs/configuration.md` |
| `## Aliases (aliases.toml, optional)` | `docs/configuration.md` |
| `## Subscriptions (subscriptions.toml, optional, matd only)` | `docs/configuration.md` |
| `## Errors and exit codes` | `docs/errors.md` |
| `## Logs (stderr)` | `docs/errors.md` |
| `## Backend` | `docs/backend.md` |
| `## Development` | `docs/development.md` |
| `## Manual E2E (with real devices; not in CI)` | `docs/development.md` |
| `## Status` / `## Requirements` / `## Install` / `## Contributing` / `## License` | README に残留（Task 4 で再構成） |

---

### Task 1: ユーザーの CLAUDE.md 編集を取り込む

セッション開始時点で未コミットだった CLAUDE.md の変更は、ユーザー自身による chip-tool 記述整理（C2 の真実性スイープと同方向）。ユーザー承認済み（2026-08-05）なので先頭で取り込む。

**Files:**
- Commit only: `CLAUDE.md`（編集はしない。既にワーキングツリーにある変更をそのままコミット）

**Interfaces:**
- Produces: クリーンなワーキングツリー。以後のタスクは CLAUDE.md の diff を気にせず作業できる。

- [ ] **Step 1: 現状確認**

Run: `git status --short && git diff --stat CLAUDE.md`
Expected: ` M CLAUDE.md` のみ（他に未コミット変更が無いこと）。

- [ ] **Step 2: コミット**

```bash
git add CLAUDE.md
git commit -m "docs: CLAUDE.md の chip-tool 歴史記述を整理（ユーザー編集の取り込み）"
```

- [ ] **Step 3: クリーン確認**

Run: `git status --short`
Expected: 出力なし（未追跡ファイルを除く）。

---

### Task 2: Cargo description の真実性スイープ

**Files:**
- Modify: `crates/mat/Cargo.toml:3`
- Modify: `crates/mat-core/Cargo.toml:3`

**Interfaces:**
- Produces: chip-tool 現在形を含まない description 2 件。他タスクからの依存なし（独立）。

- [ ] **Step 1: 失敗する検証を先に確認（現状 = stale）**

Run: `grep -n "chip-tool" crates/mat/Cargo.toml crates/mat-core/Cargo.toml`
Expected: 2 行ヒット（`normalizes chip-tool output` / `chip-tool output parsing` — これが直す対象）。

- [ ] **Step 2: description を書き換え**

`crates/mat/Cargo.toml` 3 行目:

```toml
description = "Matter device control CLI — drives a native Rust Matter controller in-process and emits pure structured JSON"
```

`crates/mat-core/Cargo.toml` 3 行目:

```toml
description = "Shared core for mat/matd — mat's JSON schema, error/exit-code classification, credential store"
```

- [ ] **Step 3: 検証**

Run: `grep -n "chip-tool" crates/mat/Cargo.toml crates/mat-core/Cargo.toml; cargo metadata --no-deps --format-version 1 > /dev/null && echo TOML_OK`
Expected: grep はヒットなし（exit 1）、`TOML_OK` が出る。

- [ ] **Step 4: コミット**

```bash
git add crates/mat/Cargo.toml crates/mat-core/Cargo.toml
git commit -m "docs(cargo): mat / mat-core の description を native backend の実態に更新"
```

---

### Task 3: README → docs/ 5ファイル分離（verbatim 移設）

README の `## Commands` 以降のリファレンス層を、見出し境界で 5 ファイルへ切り貼りする。**このタスクでは文言変更・リンク修正をしない**（Task 5/6 の担当）。行番号はズレるので必ず**見出し文字列**で境界を取ること。

**Files:**
- Create: `docs/commands.md`
- Create: `docs/configuration.md`
- Create: `docs/errors.md`
- Create: `docs/backend.md`
- Create: `docs/development.md`
- Modify: `README.md`（移設したセクションを削除）

**Interfaces:**
- Produces: 上記 5 ファイル。見出しルール = 各ファイル先頭に新 H1、**移設した `##` 見出し行のうち H1 と重複する単独セクションの見出し行（`## Commands` / `## Backend`）だけを削除**し、それ以外の見出し（`##`/`###`/`####`）と本文は一字一句そのまま。これで既存のセクションアンカー（例 `#aliases-aliasestoml-optional`）が全部保存される。
- Produces: 各ファイルの H1 = `# Commands` / `# Configuration` / `# Errors and exit codes` / `# Backend` / `# Development`。

- [ ] **Step 1: 移設境界の確認**

Run: `grep -n '^## ' README.md`
Expected: `## Commands` から `## Manual E2E ...` までの各セクション開始行が出る（この行番号を切り貼りに使う）。

- [ ] **Step 2: `docs/commands.md` を作成**

先頭に `# Commands` の H1（+ 空行）を置き、その下に README の `## Commands` の**次の行**（`### Discover and commissioning` の直前の空行以降）から `## Credential store` の**直前の行**までを verbatim で貼る。元の `## Commands` 見出し行自体はコピーしない（H1 と重複するため）。

- [ ] **Step 3: `docs/configuration.md` を作成**

```markdown
# Configuration

```
の下に、README の `## Credential store` 見出し行**から** `## Errors and exit codes` の直前の行**まで**（`## Credential store` + `## Aliases ...` + `## Subscriptions ...` の 3 セクション、見出し行込み）を verbatim で貼る。

- [ ] **Step 4: `docs/errors.md` を作成**

先頭に `# Errors and exit codes` の H1 を置き、README の `## Errors and exit codes` の**次の行**から `## Logs (stderr)` の直前までを verbatim で貼り、続けて `## Logs (stderr)` セクション（見出し行込み、`## Backend` の直前まで）を verbatim で貼る。

- [ ] **Step 5: `docs/backend.md` を作成**

先頭に `# Backend` の H1 を置き、README の `## Backend` の**次の行**から `## Development` の直前までを verbatim で貼る。

- [ ] **Step 6: `docs/development.md` を作成**

先頭に `# Development` の H1 を置き、README の `## Development` の**次の行**から `## Manual E2E (with real devices; not in CI)` の直前までを貼り、続けて `## Manual E2E ...` セクション（見出し行込み、`## Contributing` の直前まで）を verbatim で貼る。

- [ ] **Step 7: README から移設済みセクションを削除**

README から `## Commands` の行〜`## Contributing` の直前の行を削除する。残るのは: 先頭〜`## Install` セクション末尾、`## Contributing`、`## License`。

- [ ] **Step 8: 欠落なしの機械検証（行数勘定）**

Run:
```bash
wc -l README.md docs/commands.md docs/configuration.md docs/errors.md docs/backend.md docs/development.md
```
Expected: 合計 ≈ 1545 + (5 × H1 ヘッダ分の数行)。移設前の README 1545 行に対し、増分が各ファイルの H1+空行（計 10〜15 行程度）で説明できること。大きく合わない場合は欠落があるので突き合わせる。

Run: `grep -c '^## ' docs/configuration.md docs/development.md docs/errors.md`
Expected: configuration=3, development=1（Manual E2E）, errors=1（Logs (stderr)）。

- [ ] **Step 9: コミット**

```bash
git add README.md docs/commands.md docs/configuration.md docs/errors.md docs/backend.md docs/development.md
git commit -m "docs: README のリファレンス層を docs/ 5ファイルへ分離（verbatim 移設）"
```

---

### Task 4: 新 README 上層の書き起こし

README を spec のセクション順に書き直す。Install / Requirements / Contributing / License は Task 3 後の残留内容を流用。

**Files:**
- Modify: `README.md`（全面書き換え）

**Interfaces:**
- Consumes: Task 3 の残留 README（intro 1〜16 行、Status、Requirements、Install、Contributing、License）。
- Produces: 200 行以内の新 README。Documentation セクションはサイト URL `https://nogu3.github.io/mat/` と `docs/*.md` 相対リンクの両方を持つ。

- [ ] **Step 1: README を以下の構成・内容で書き換え**

以下をそのまま使う（`<既存〜>` は Task 3 後の README から verbatim 流用）:

````markdown
# mat

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

<既存 intro 5〜16 行: "`mat` is a CLI for controlling Matter devices. ..." から
"see [ARCHITECTURE.md](./ARCHITECTURE.md)." まで。ただし 7〜9 行目の丸括弧
"(`chip-tool` was the backend through Phase 5 M8c-2; as of **0.22.0** it is
fully retired — see [Backend](#backend).)" は削除する（歴史詳細は
docs/backend.md にあり、intro には不要。真実性スイープの先取りではなく
ピッチ層の簡素化）。>

## Why mat

- **Pure JSON in, pure JSON out** — one object per command on stdout; stable
  error `kind`s and exit codes. Built to be driven by scripts and AI agents.
- **Native pure-Rust controller** — TLV, CASE, IM, groupcast, mDNS, and
  commissioning (on-network and BLE+Thread) run in-process. No external
  controller subprocess.
- **One-shot CLI, optional resident daemon** — `mat` holds no state except the
  credential store; `matd` adds warm sessions and a resident Subscribe
  (`mat listen`).
- **Optional alias layer** — human names live in a local `aliases.toml` only;
  the wire stays numeric.

## Quickstart

```bash
# build & install -> ~/.cargo/bin/{mat,matd}
task install

# create your first fabric (writes the credential store; no network I/O)
mat fabric init

# commission a device (all values here are dummy)
mat commission --setup-code "MT:Y.K9042C00KA0648G00" --node 5

# control and read it
mat on --node 5
mat read --node 5 --cluster onoff --attribute on-off
```

Every command prints exactly one JSON object on stdout:

```json
// mat commission
{ "timestamp": "2026-06-06T12:34:56+09:00", "node_id": 5, "status": "success" }

// mat on — control is an invoke of the OnOff cluster's on command
{ "timestamp": "2026-06-06T12:34:57+09:00", "node_id": 5, "endpoint": 1, "cluster": "onoff", "command": "on", "status": "success" }

// mat read — the attribute's TLV value, normalized
{ "timestamp": "2026-06-06T12:34:58+09:00", "node_id": 5, "endpoint": 1, "cluster": "onoff", "attribute": "on-off", "value": true }
```

Errors are structured the same way (stderr, stable `kind` + exit code — see
[Errors and exit codes](./docs/errors.md)).

## Requirements

<既存 Requirements 本文を verbatim 流用。ただし文中の
`see [Backend](#backend)`（2箇所）を `see [Backend](./docs/backend.md)` に
張り替える。>

## Install

<既存 Install 本文を verbatim 流用。>

## Documentation

The full reference lives at **<https://nogu3.github.io/mat/>** (also browsable
in [`docs/`](./docs/)):

| Page | Contents |
|---|---|
| [Commands](./docs/commands.md) | Every command with its JSON output: discover / commissioning, state operations, diagnostics, listen, multi-admin share, groupcast, routing through `matd` |
| [Configuration](./docs/configuration.md) | The credential store, `aliases.toml`, `subscriptions.toml` |
| [Errors and exit codes](./docs/errors.md) | Error schema, `kind` table, exit codes, stderr logs |
| [Backend](./docs/backend.md) | The native backend, interface auto-detection, environment variables |
| [Development](./docs/development.md) | Build / test tasks, manual E2E with real devices |

For the design background and roadmap, see
[ARCHITECTURE.md](./ARCHITECTURE.md).

## Status

Everything documented is implemented on the native backend and passes the
fake-connection / binary integration tests; real-device E2E has confirmed the
full op sweep runs natively with no fallback. Group *delivery* is
unacknowledged multicast by design, so per-device actuation cannot be
confirmed from the controller side (see
[Groupcast](./docs/commands.md#groupcast)).

## Contributing

<既存 Contributing 本文を verbatim 流用。>

## License

[MIT](./LICENSE).
````

- [ ] **Step 2: 行数検証**

Run: `wc -l README.md`
Expected: 200 行以内。

- [ ] **Step 3: コミット**

```bash
git add README.md
git commit -m "docs: README を軽量ピッチ層に再構成（Why mat 骨組み + Quickstart + Documentation TOC）"
```

---

### Task 5: 移設先コンテンツの真実性スイープ + ARCHITECTURE 監査

**Files:**
- Modify: `docs/commands.md` / `docs/configuration.md` / `docs/errors.md` / `docs/backend.md` / `docs/development.md`（該当箇所のみ）
- Modify: `ARCHITECTURE.md`（spot-fix があれば）

**Interfaces:**
- Consumes: Task 3 の 5 ファイル。
- Produces: 現在形の stale 記述ゼロの docs/。歴史記述・互換仕様参照は不変。

- [ ] **Step 1: docs/ の chip-tool 言及を全数レビュー**

Run: `grep -n "chip-tool" docs/commands.md docs/configuration.md docs/errors.md docs/backend.md docs/development.md`

各ヒットを次の 3 分類で判定し、**(c) だけ**修正する:
(a) 歴史記述（"retired in 0.22.0" 等）→ 温存。
(b) 互換仕様参照（"chip-tool form" の記法、"chip-tool-compatible INI"、"chip-tool keeps working"、fixed-epoch 互換の説明）→ 温存。
(c) 現在形で chip-tool がバックエンド/実行経路であると読める記述 → native の実態に書き換え。
判定に迷うものは温存し、コミットメッセージに列挙する。
（事前調査では現 README のヒットはほぼ (a)/(b)。例: `432` 行相当の "from the old chip-tool-log-parsing path, cannot occur on the native path" は (a) で温存。）

- [ ] **Step 2: ARCHITECTURE.md の現在形監査**

Run: `grep -n "chip-tool" ARCHITECTURE.md | grep -viE "retired|was |formerly|Phase|M8|record|history|compat"`
出たものを目視し、「今のバックエンドが chip-tool」と読める現在形だけ spot-fix（設計記録・過去の決定の記述は温存）。ほぼ無い想定。

- [ ] **Step 3: 検証（分類の再確認）**

Run: `grep -n "chip-tool" docs/*.md ARCHITECTURE.md | wc -l`
Expected: ヒットは残る（歴史・互換参照は温存が正）。Step 1 の (c) 分類がゼロになったことを自分の変更 diff（`git diff`）で確認する。

- [ ] **Step 4: コミット**

```bash
git add docs/commands.md docs/configuration.md docs/errors.md docs/backend.md docs/development.md ARCHITECTURE.md
git commit -m "docs: 真実性スイープ — chip-tool 現在形記述を native の実態へ修正（歴史・互換参照は温存）"
```
（ARCHITECTURE.md に変更が無ければ add から外す。）

---

### Task 6: リンク整合スイープ

**Files:**
- Modify: `docs/commands.md` / `docs/configuration.md` / `docs/errors.md` / `docs/backend.md` / `docs/development.md`
- Modify: `CLAUDE.md`（エラー表へのリンク 1 箇所）
- Modify: `ARCHITECTURE.md`（README 深リンクがあれば）
- Create: `/tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/c55a2920-7a90-45ec-ac39-2d7a6f7f9983/scratchpad/check_links.sh`（検証スクリプト、コミットしない）

**Interfaces:**
- Consumes: Task 3〜5 の全ファイル、冒頭の「セクション → 移設先マッピング」表。
- Produces: リポジトリ内相対リンク切れゼロ。ルール = docs/*.md **間**は相対リンク、docs/*.md から README / ARCHITECTURE へは GitHub 絶対 URL（`https://github.com/nogu3/mat/blob/main/README.md` 形式）。

- [ ] **Step 1: docs/ 内の旧 README アンカーを張り替え**

Run: `grep -n '](#' docs/commands.md docs/configuration.md docs/errors.md docs/backend.md docs/development.md`

各ヒットについて、リンク先セクションが**同じファイル内**ならそのまま、**別ファイル**ならマッピング表に従い `<file>.md#<同じアンカー>` へ書き換える（例: commands.md 内の `[Aliases](#aliases-aliasestoml-optional)` → `[Aliases](configuration.md#aliases-aliasestoml-optional)`）。アンカー名は見出しを変えていないので不変。

- [ ] **Step 2: docs/ から README / ARCHITECTURE への参照を絶対 URL 化**

Run: `grep -n 'README\|ARCHITECTURE' docs/*.md | grep ']('`
相対 `./README.md` / `../README.md` / `./ARCHITECTURE.md` 形式のリンクを `https://github.com/nogu3/mat/blob/main/README.md`（ARCHITECTURE も同形式）へ書き換える（mdBook サイト上で 404 になるため）。

- [ ] **Step 3: CLAUDE.md のリンク追従**

CLAUDE.md 内の `See the table in [README.md](./README.md#errors-and-exit-codes)` を
`See the table in [docs/errors.md](./docs/errors.md)` に書き換える。同様に
`the full list is in README ("Errors and exit codes")` を
`the full list is in docs/errors.md` に書き換える。

- [ ] **Step 4: ARCHITECTURE.md の README 深リンク監査**

Run: `grep -n 'README.md#' ARCHITECTURE.md`
ヒットした深リンクを移設先の `docs/*.md`（マッピング表）へ張り替える。ヒットなしなら何もしない。

- [ ] **Step 5: リンク検証スクリプトを書いて実行**

`<scratchpad>/check_links.sh` として保存（コミットしない）:

```bash
#!/usr/bin/env bash
# 全 *.md の相対リンク (](#...) ](./...) ](../...) ](docs/...) ](<file>.md...) を検証
set -u
fail=0
while IFS=$'\t' read -r file link; do
  target=${link#"]("}; target=${target%")"}
  path=${target%%#*}; anchor=""
  case "$target" in *#*) anchor=${target#*#};; esac
  if [ -n "$path" ]; then
    resolved="$(dirname "$file")/$path"
    [ -e "$resolved" ] || { echo "MISSING FILE: $file -> $target"; fail=1; continue; }
  else
    resolved="$file"
  fi
  if [ -n "$anchor" ] && [[ "$resolved" == *.md ]]; then
    if ! grep -hE '^#{1,6} ' "$resolved" \
        | sed -E 's/^#+ +//; s/`//g; s/[^a-zA-Z0-9 _-]//g; s/ +/-/g' \
        | tr '[:upper:]' '[:lower:]' | grep -qx "$anchor"; then
      echo "MISSING ANCHOR: $file -> $target"; fail=1
    fi
  fi
done < <(grep -rnoE '\]\((\.{1,2}/|docs/|#|[a-z-]+\.md)[^)]*\)' \
    --include='*.md' --exclude-dir=target --exclude-dir=book --exclude-dir=superpowers . \
    | sed -E 's/^([^:]+):[0-9]+:/\1\t/')
[ "$fail" = 0 ] && echo "LINKS OK"
exit $fail
```

Run: `bash <scratchpad>/check_links.sh`
Expected: `LINKS OK`（失敗したら該当リンクを直して再実行）。

- [ ] **Step 6: コミット**

```bash
git add docs/commands.md docs/configuration.md docs/errors.md docs/backend.md docs/development.md CLAUDE.md
git commit -m "docs: 分離に伴うリンク整合 — docs/ 間相対リンク化、README/ARCHITECTURE は絶対 URL、CLAUDE.md 追従"
```
（ARCHITECTURE.md に変更があれば add に含める。）

---

### Task 7: mdBook スキャフォールディングとローカルビルド検証

**Files:**
- Create: `book.toml`
- Create: `docs/SUMMARY.md`
- Create: `docs/index.md`
- Modify: `.gitignore`（`book/` を追加）

**Interfaces:**
- Consumes: Task 3〜6 の docs/ 5 ファイル。
- Produces: `mdbook build` が通る構成。サイト章立て = Introduction / Commands / Configuration / Errors and exit codes / Backend / Development。Task 8 の workflow はこの `book.toml` とビルド出力 `book/` を前提にする。

- [ ] **Step 1: mdbook をローカルに導入**

Run: `mdbook --version || cargo install mdbook --locked`
Expected: `mdbook v0.4.x`。

- [ ] **Step 2: `book.toml` を作成（リポジトリ直下）**

```toml
[book]
title = "mat"
description = "CLI for controlling Matter devices — a native Rust Matter controller with pure structured JSON output"
src = "docs"
language = "en"

[output.html]
git-repository-url = "https://github.com/nogu3/mat"
site-url = "/mat/"
```

- [ ] **Step 3: `docs/SUMMARY.md` を作成**

```markdown
# Summary

[Introduction](index.md)

- [Commands](commands.md)
- [Configuration](configuration.md)
- [Errors and exit codes](errors.md)
- [Backend](backend.md)
- [Development](development.md)
```

- [ ] **Step 4: `docs/index.md` を作成**

```markdown
# mat

`mat` is a CLI for controlling Matter devices: a from-scratch, pure-Rust
native Matter controller that runs in-process and prints pure structured
JSON on stdout.

This site is the command and configuration reference. For the project
overview, Quickstart, and design background, see the
[GitHub repository](https://github.com/nogu3/mat).

- [Commands](commands.md) — every command with its JSON output
- [Configuration](configuration.md) — the credential store, `aliases.toml`,
  `subscriptions.toml`
- [Errors and exit codes](errors.md) — error schema, `kind` table, exit
  codes, stderr logs
- [Backend](backend.md) — the native backend, interface auto-detection,
  environment variables
- [Development](development.md) — build / test tasks, manual E2E
```

- [ ] **Step 5: `.gitignore` に `book/` を追加**

既存の `.gitignore` 末尾に 1 行:

```
/book/
```

- [ ] **Step 6: ビルドして公開範囲を機械検証**

Run:
```bash
mdbook build
find book -path '*superpowers*' | wc -l
ls book/index.html book/commands.html book/configuration.html book/errors.html book/backend.html book/development.html
```
Expected: build 成功、`find` は **0**（内部 spec が出力されていない）、6 ファイルすべて存在。

- [ ] **Step 7: コミット**

```bash
git add book.toml docs/SUMMARY.md docs/index.md .gitignore
git commit -m "docs: mdBook スキャフォールディング（book.toml / SUMMARY / index、docs/ をサイト源に）"
```

---

### Task 8: GitHub Pages デプロイ workflow

**Files:**
- Create: `.github/workflows/docs.yml`

**Interfaces:**
- Consumes: Task 7 の `book.toml`（`src = "docs"`、出力 `book/`）。
- Produces: main への docs 変更 push で `https://nogu3.github.io/mat/` に自動デプロイされる workflow。

- [ ] **Step 1: `.github/workflows/docs.yml` を作成**

```yaml
name: docs

on:
  push:
    branches: [main]
    paths:
      - "docs/**"
      - "book.toml"
      - ".github/workflows/docs.yml"
  workflow_dispatch:

permissions:
  contents: read
  pages: write
  id-token: write

concurrency:
  group: pages
  cancel-in-progress: false

jobs:
  build:
    runs-on: ubuntu-latest
    env:
      MDBOOK_VERSION: v0.4.40
    steps:
      - uses: actions/checkout@v4
      - name: Install mdBook (prebuilt)
        run: |
          mkdir -p "$HOME/.local/bin"
          curl -sSL "https://github.com/rust-lang/mdBook/releases/download/${MDBOOK_VERSION}/mdbook-${MDBOOK_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
            | tar -xz -C "$HOME/.local/bin"
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - run: mdbook build
      - uses: actions/configure-pages@v5
      - uses: actions/upload-pages-artifact@v3
        with:
          path: book
  deploy:
    needs: build
    runs-on: ubuntu-latest
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - id: deployment
        uses: actions/deploy-pages@v4
```

- [ ] **Step 2: YAML 構文検証**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/docs.yml')); print('YAML_OK')"`
Expected: `YAML_OK`。

- [ ] **Step 3: コミット**

```bash
git add .github/workflows/docs.yml
git commit -m "ci: mdBook サイトの GitHub Pages 自動デプロイ workflow を追加"
```

---

### Task 9: 最終検証（マージ前ゲート）

**Files:** なし（検証のみ。修正が出たら該当タスクのファイルを直して追記コミット）

**Interfaces:**
- Consumes: Task 1〜8 の全成果物。

- [ ] **Step 1: `task check`**

Run: `task check`
Expected: fmt:check + clippy + test すべて成功。

- [ ] **Step 2: リンク検査の再実行**

Run: `bash <scratchpad>/check_links.sh`
Expected: `LINKS OK`。

- [ ] **Step 3: mdBook ビルドと公開範囲の再確認**

Run: `mdbook build && find book -path '*superpowers*' | wc -l && wc -l README.md`
Expected: build 成功、`0`、README ≤ 200 行。

- [ ] **Step 4: 受け入れ条件チェックリスト（spec の 1〜10）を目視確認**

spec `docs/superpowers/specs/2026-07-23-readme-restructure-accuracy-sweep-design.md` の受け入れ条件 1〜10 を一つずつ確認。6（デプロイ後のサイト閲覧）だけは Task 10 のマージ後に確認する。

- [ ] **Step 5: 新 README の通しレビュー**

README を先頭から読み、Quickstart のコマンドが現行 CLI と一致するか（`task install` / `mat fabric init` / `mat commission --setup-code ... --node` / `mat on --node` / `mat read --node --cluster --attribute`）、JSON 例が docs/commands.md の Outputs と矛盾しないかを確認。ユーザーにも通しレビューを依頼する。

---

### Task 10: マージ・Pages セットアップ・デプロイ検証

**このタスクはユーザーの通しレビュー承認後に実行**（superpowers:finishing-a-development-branch の手順に従う）。docs のみの変更なので実機 E2E は不要（コード挙動不変、Task 9 の `task check` がゲート）。

**Files:** なし（git 操作と GitHub 設定のみ）

- [ ] **Step 1: main へマージして push**

```bash
git checkout main
git merge --no-ff docs/readme-restructure-c2 -m "Merge docs/readme-restructure-c2: C2 — README 二層化 + 真実性スイープ + mdBook サイト"
git push origin main
```

- [ ] **Step 2: GitHub Pages を有効化（一度きり）**

Run: `gh api -X POST repos/nogu3/mat/pages -f build_type=workflow || gh api -X PUT repos/nogu3/mat/pages -f build_type=workflow`
Expected: Pages source が GitHub Actions になる（既に有効なら PUT 側が通る）。

- [ ] **Step 3: workflow の完走を確認**

Run: `gh run watch $(gh run list --workflow=docs.yml --limit 1 --json databaseId --jq '.[0].databaseId')`
Expected: `docs` workflow が success。

- [ ] **Step 4: サイトの目視確認**

`https://nogu3.github.io/mat/` を開き、Introduction / Commands / Errors の表示とサイドバー・検索が生きていることを確認（curl での 200 確認: `curl -s -o /dev/null -w '%{http_code}' https://nogu3.github.io/mat/commands.html` → `200`）。

- [ ] **Step 5: リポジトリの homepage をサイトに設定**

Run: `gh repo edit nogu3/mat --homepage "https://nogu3.github.io/mat/"`

- [ ] **Step 6: ブランチ削除**

```bash
git branch -d docs/readme-restructure-c2
git push origin --delete docs/readme-restructure-c2 2>/dev/null || true
```
