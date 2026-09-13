# Refactor 2 — lane bins Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mat / matd / mat-core の監査残課題（hex panic バグ修正・iface 自動選択ヘルパ共通化・hex 手書き撤去・deferred 小物・Taskfile check 追加）を挙動変更なしで片付ける。

**Architecture:** 純関数への寄せ（`mat_core::hex::decode`、`mat_native::iface_select`）と改名・doc 修正のみ。プロトコル/ワイヤ/KVS には触れない。

**Tech Stack:** Rust workspace（cargo）、Task（Taskfile.yml）、tracing。

**Spec:** `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/bins.md` + `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/_common.md`

## Global Constraints

- 担当ファイル以外は触らない: crates/mat/**、crates/matd/**（`crates/matd/src/native.rs`・`crates/matd/tests/integration.rs` は除外）、crates/mat-core/**、`crates/mat-native/src/iface_select.rs`、`Taskfile.yml`、`ARCHITECTURE.md`、`docs/**`。
- 挙動変更 0（Task 1 のバグ修正を除く）。テストが pin する文言は逐語維持: `iface auto-selected (native default)`（mat）、`iface auto-selected (matd native default)`（matd）、`thread iface auto-detected (groupcast egress)`（mat）、`thread iface auto-detected (matd groupcast egress)`（matd）、`invalid hex literal: …` / `odd-length hex literal: …` / `bytes value must use hex: prefix`、`invalid --thread-dataset: expected hex bytes`。
- cargo は常に `CARGO_BUILD_JOBS=3` を前置。テストはフォアグラウンド。
- 各タスク末で `cargo fmt` と対象クレートのテスト。コミットはブランチ `refactor2/bins` に。main へのマージ・push はしない。
- コミットメッセージ末尾:
  ```
  Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_017YVBamL631H1zbdYoYaWGk
  ```
- 行番号は a5fcf4f 時点。着手前に grep で再確認する。

---

### Task 1: `parse_hex_bytes` の非 ASCII panic 修正（TDD）

**Files:**
- Modify: `crates/mat-core/src/ids.rs`（`fn parse_hex_bytes` ~236、tests モジュール ~803-820 付近）
- Test: `crates/mat/tests/integration.rs`（CLI 統合テスト 1 本追加）

**Interfaces:**
- Consumes: `mat_core::hex::decode(&str) -> Option<Vec<u8>>`（奇数長・非 hex は None、`s.get()` を使うので非 ASCII で panic しない、`""` → `Some(vec![])`）。
- Produces: なし（`parse_hex_bytes` のシグネチャ不変）。

背景: `h = "ああ"` は 6 バイト（偶数）なので奇数長チェックを通り、`&h[0..2]` が char boundary で panic する。呼び手は `parse_value_typed`（Bytes 型）、JSON 経由の Bytes フィールド（~391）、`parse_scalar_inferred`（~481、Err なら Str にフォールバック）。

- [ ] **Step 1: mat-core に失敗テストを書く**

`crates/mat-core/src/ids.rs` の tests モジュール、`hex:zz` を assert している既存テスト（~818）の直後に追加:

```rust
    #[test]
    fn non_ascii_hex_literal_is_parse_error_not_panic() {
        // "ああ" は 6 バイト（偶数長）なので奇数長チェックを抜け、旧実装は
        // バイトスライスが char boundary を割って panic していた。
        let err = parse_value_typed("hex:ああ", &Ty::Scalar(TypeTag::Bytes)).unwrap_err();
        assert!(err.contains("invalid hex literal"), "{err}");
        // 型推定経路（数値 ID 直指定）も panic せず Str に落ちる。
        assert_eq!(
            parse_scalar_inferred("hex:ああ"),
            ArgValue::Str("hex:ああ".into())
        );
    }
```

（`parse_value_typed` の戻り値のエラー型が `String` でない場合は既存テスト ~1322 の `unwrap_err()` の扱いに合わせて `.to_string()` 等を調整する。`parse_scalar_inferred` が非 hex の文字列を `ArgValue::Str` で返すことは既存テスト ~991 付近で確認する。）

- [ ] **Step 2: 失敗を確認**

Run: `CARGO_BUILD_JOBS=3 cargo test -p mat-core non_ascii_hex_literal`
Expected: FAIL（panic: `byte index 2 is not a char boundary`）

- [ ] **Step 3: 実装**

`parse_hex_bytes` を置換:

```rust
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let h = s
        .strip_prefix("hex:")
        .ok_or("bytes value must use hex: prefix")?;
    if h.len() % 2 != 0 {
        return Err(format!("odd-length hex literal: {}", short_literal(s)));
    }
    crate::hex::decode(h).ok_or_else(|| format!("invalid hex literal: {}", short_literal(s)))
}
```

（奇数長チェックは文言を分けるため残す。clippy が `is_multiple_of` を求めるなら元の式のまま触らない — 既存コードが通っているので変えない。）

- [ ] **Step 4: 通過を確認**

Run: `CARGO_BUILD_JOBS=3 cargo test -p mat-core`
Expected: PASS（既存の hex 系テスト ~803/818/1322 含む）

- [ ] **Step 5: CLI 統合テストを書く**

Bytes 型引数を持つ invoke/write を 1 つ選ぶ（例: `ids_gen.rs` を `TypeTag::Bytes` で grep し、コマンド引数として Bytes を取るもの — 候補 `operationalcredentials` の `add-trusted-root-certificate`。CLI の invoke 引数形は `crates/mat/src/cli.rs` の `Invoke {` を読んで合わせる）。**まず Task 1 の Step 3 を一時的に revert した状態（`git stash` は使わず、手で旧実装に戻す）で `cargo run -p matterctl --bin mat -- --store <tmp> …` 相当を実行し panic（exit 101）することを確認**してから、実装を戻して下記テストを通す。値のパースがバックエンド到達前に起きることもこの手動実行で確かめる（到達前でなければ別経路 — 例えば `write` の Bytes 属性 — を選ぶ）。

`crates/mat/tests/integration.rs` の CLI 引数エラー節の末尾に追加（`mat()` ヘルパは `MAT_IFACE=lo` を固定、`store_with_node5()` は node 5 入りの store）:

```rust
/// 非 ASCII の hex リテラルは panic（exit 101・stdout 汚染）ではなく
/// parse_error + exit 1。旧 `parse_hex_bytes` は偶数バイト長の非 ASCII で
/// char boundary を割って panic していた。
#[test]
fn non_ascii_hex_value_is_parse_error_not_panic() {
    let store = store_with_node5();
    let out = mat(store.path())
        .args([
            "invoke",
            "--node",
            "5",
            "--cluster",
            "operationalcredentials",
            "--command",
            "add-trusted-root-certificate",
            "hex:ああ",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "stdout must stay clean");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("\"parse_error\""), "{stderr}");
    assert!(stderr.contains("invalid hex literal"), "{stderr}");
}
```

（引数の並び・フラグ名は実際の CLI に合わせて修正すること。exit code が 1 以外（例: parse_error が 2 に写る）なら `docs/errors.md` を確認し、実際の parse_error の exit code を assert する — ただしタスク指示は「parse_error・exit 1」なので、違っていたら DONE に記録するため報告する。）

- [ ] **Step 6: テスト通過**

Run: `CARGO_BUILD_JOBS=3 cargo test -p matterctl --test integration non_ascii_hex`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add crates/mat-core/src/ids.rs crates/mat/tests/integration.rs
git commit -m "fix(mat-core): 非 ASCII の hex リテラルで parse_hex_bytes が panic するのを parse_error に（hex::decode へ寄せる）"
```

---

### Task 2: iface 自動選択ヘルパを `mat_native::iface_select` に 1 本化

**Files:**
- Modify: `crates/mat-native/src/iface_select.rs`（関数 2 本追加）
- Modify: `crates/mat/src/main.rs:~305-331`（`select_iface` / `select_thread_iface` 削除、呼び出し ~208/212 を置換）
- Modify: `crates/matd/src/main.rs:~110-136`（同上、呼び出し ~170/174）

**Interfaces:**
- Produces:
  ```rust
  pub fn select_iface(explicit: Option<&str>, label: &str) -> Result<String, MatError>
  pub fn select_thread_iface(explicit: Option<&str>, label: &str) -> Option<crate::ThreadIfaceChoice>
  ```
  ログ: `tracing::info!(iface = %i, "iface auto-selected ({label})")` / `tracing::info!(iface = %n, "thread iface auto-detected ({label})")`。
  mat は label `"native default"` / `"groupcast egress"`、matd は `"matd native default"` / `"matd groupcast egress"`。

既知の挙動差分（許容・DONE に記録）: tracing の target が `mat` / `matd` から `mat_native::iface_select` に変わる（fmt 出力の `INFO <target>:` 部分）。メッセージ本文は逐語維持。

- [ ] **Step 1: ヘルパ追加**

`crates/mat-native/src/iface_select.rs` の `autodetect()` の直後に:

```rust
/// Matter 用 iface の決定: 明示指定（`MAT_IFACE` / `--iface` 等）を優先し、
/// 未設定なら [`autodetect`]（候補 0 / 複数はハードエラー）。`label` は
/// 自動選択時の info ログに入る呼び手識別（`mat` = `"native default"`、
/// `matd` = `"matd native default"` — E2E / 統合テストが逐語 grep する）。
pub fn select_iface(explicit: Option<&str>, label: &str) -> Result<String, MatError> {
    match explicit {
        Some(i) => Ok(i.to_string()),
        None => {
            let i = autodetect()?;
            tracing::info!(iface = %i, "iface auto-selected ({label})");
            Ok(i)
        }
    }
}

/// groupcast の Thread TUN 追加送出先: 明示指定を優先、未設定なら wpan* を
/// 自動検出（失敗は None のまま — LAN 単独送出）。`label` は自動検出時の
/// info ログの識別（`mat` = `"groupcast egress"`、`matd` = `"matd groupcast egress"`）。
pub fn select_thread_iface(explicit: Option<&str>, label: &str) -> Option<crate::ThreadIfaceChoice> {
    match explicit {
        Some(n) => Some(crate::ThreadIfaceChoice::Explicit(n.to_string())),
        None => detect_thread_iface_auto().map(|n| {
            tracing::info!(iface = %n, "thread iface auto-detected ({label})");
            crate::ThreadIfaceChoice::Auto(n)
        }),
    }
}
```

（`ThreadIfaceChoice` の実際のパスを `grep -rn "enum ThreadIfaceChoice" crates/mat-native` で確認。mat-native に tracing 依存があることも Cargo.toml で確認。）

- [ ] **Step 2: mat 側の置換**

`crates/mat/src/main.rs` の `fn select_iface` / `fn select_thread_iface`（直前の doc コメント含む）を削除し、呼び出しを:

```rust
    let iface_owned = match mat_native::iface_select::select_iface(args.iface.as_deref(), "native default") {
```
```rust
    let thread_iface =
        mat_native::iface_select::select_thread_iface(args.thread_iface.as_deref(), "groupcast egress");
```

（`match` の腕は元のまま。不要になった import があれば削除。）

- [ ] **Step 3: matd 側の置換**

`crates/matd/src/main.rs` 同様に削除し:

```rust
    let iface = mat_native::iface_select::select_iface(cli.iface.as_deref(), "matd native default")?;
```
```rust
    let thread_iface = mat_native::iface_select::select_thread_iface(
        cli.thread_iface.as_deref(),
        "matd groupcast egress",
    );
```

- [ ] **Step 4: 逐語確認とテスト**

Run: `grep -rn "auto-selected\|auto-detected" crates/mat crates/matd crates/mat-native/src`
Expected: `iface_select.rs` の 2 行のみ（format 形）。

Run: `CARGO_BUILD_JOBS=3 cargo clippy -p mat-native -p matterctl -p matd --all-targets -- -D warnings && CARGO_BUILD_JOBS=3 cargo test -p mat-native iface_select && CARGO_BUILD_JOBS=3 cargo test -p matterctl --test integration no_iface_env && CARGO_BUILD_JOBS=3 cargo test -p matd`
Expected: PASS

手動: `env -u MAT_IFACE MAT_MATD=0 MAT_LOG=info cargo run -q -p matterctl --bin mat -- --store "$(mktemp -d)" read --node 1 --cluster onoff --attribute on-off 2>&1 | grep -m1 "iface auto"` の出力に `iface auto-selected (native default)` か `iface autodetect` が含まれること。

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/mat-native/src/iface_select.rs crates/mat/src/main.rs crates/matd/src/main.rs
git commit -m "refactor(iface): mat/matd の select_iface・select_thread_iface を mat_native::iface_select に一本化（ログ文言は label 引数で逐語維持）"
```

---

### Task 3: commission の `decode_hex` 手書き → `mat_core::hex::decode`

**Files:**
- Modify: `crates/mat/src/commands/commission.rs`（`fn decode_hex` ~96、呼び出し ~50、tests ~158-176）

**Interfaces:**
- Consumes: `mat_core::hex::decode`（`""` は `Some(vec![])` を返す点に注意 — 旧 `decode_hex` は空を None にしていた）。

- [ ] **Step 1: 他の手書き hex を棚卸し**

Run: `grep -rn "from_str_radix\|:02x\|:02X" crates/mat/src crates/matd/src`
`from_str_radix(…, 16)` を 2 桁ずつ回すデコード / `{:02x}` を collect するエンコードがあれば同じタスクで `mat_core::hex::{decode, encode_lower, encode_upper}` に寄せる（matd に mat-core 依存があることを Cargo.toml で確認）。単発のパース（例: `0x` 付き数値）は対象外。見つけたものは報告する。

- [ ] **Step 2: 置換**

`decode_hex` を空拒否だけ残す薄い形にする（テストが `decode_hex` を直接呼んでいるので名前は残す）:

```rust
/// 偶数桁の hex 文字列 → bytes。空は拒否（空 dataset は無意味）。
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() {
        return None;
    }
    mat_core::hex::decode(s)
}
```

- [ ] **Step 3: テスト**

Run: `CARGO_BUILD_JOBS=3 cargo test -p matterctl decode_hex`
Expected: 既存 4 本（odd / empty / valid / non-ascii）PASS

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add crates/mat/src/commands/commission.rs  # + Step 1 で触ったファイル
git commit -m "refactor(mat): commission の decode_hex 手書きを mat_core::hex::decode へ寄せる"
```

---

### Task 4: deferred 小物（改名・陳腐化 doc・mesh の冗長ブロック）

**Files:**
- Modify: `crates/matd/src/server/mod.rs:~892-894`
- Modify: `ARCHITECTURE.md:~875-878`
- Modify: `docs/superpowers/specs/2026-09-06-matd-reload-design.md:~80`
- Modify: `crates/mat-core/src/mesh.rs`（`fn rescue_identities` ~426-553、フェーズ 5 doc ~669）
- Modify: `crates/mat-core/src/group.rs:~97-107`

- [ ] **Step 1: matd テスト改名**

`group_provision_without_group_settings_ctx_is_internal_error` → `group_provision_without_group_settings_is_other_error`。doc コメント（~892）を実態に:

```rust
    /// engine に group_settings が無い（`with_establisher` のテスト注入）と
    /// `mat_native::runner::provision` が kind `other` のハードエラーを返す。
```

（`mat-native/src/runner.rs` の `provision` で `let Some(gs) = &engine.group_settings else {` の分岐が返すエラーを読み、記述が正しいか確認。）

- [ ] **Step 2: ARCHITECTURE.md**

~875 の「matd（`server.rs::group_provision`）も対称: `NativeBackend::group_settings_ctx()` が `Some` なら…フォールバックする」を歴史記述の文脈を壊さない範囲で現行に直す。前後（~860-890）を読んでから、例えば:

```
    （`server.rs::group_provision`）も対称だった（当時は
    `NativeBackend::group_settings_ctx()` の `Some`/`None` で分岐）。
    現行は mat / matd とも `mat_native::runner::provision` を共有し、
    engine の `group_settings` が無ければフォールバックせず kind `other`
    のハードエラーになる（chip-tool 退役済み）。
```

の要旨で、元の段落の後続文（fake ws のテスト実証など）と矛盾しないよう最小限に整える。現行の実装事実は `crates/mat-native/src/runner.rs` と `crates/matd/src/server/mod.rs` で確認してから書く。

- [ ] **Step 3: reload spec の detail 例**

`docs/superpowers/specs/2026-09-06-matd-reload-design.md` の表で:
- `native: read KVS credentials: ...` → `native: read KVS credentials: ... — run \`mat fabric init\``
- NOC 行の「同上」も実装（`crates/mat-native/src/lib.rs` の `native: self-issue NOC: {e} — run \`mat fabric init\``）に合わせ、必要なら `native: self-issue NOC: ... — run \`mat fabric init\`` と明記。
Markdown 表内のバッククォート入れ子は、セル全体を `` `` … `` `` のダブルバッククォートで囲む等で崩れないようにする。

- [ ] **Step 4: mesh.rs**

(a) `rescue_identities` 内の `let mut ident_by = …;` 直後の `{` とそれに対応する `}`（`ident_by.insert(node, "rloc16");` のループ・`if` を閉じた後、「救済が成立しなかったノードの rloc16 は出力しない」コメントの直前）を取り除き、中身を 1 段デデントする。借用上ブロックが必要ならコンパイルエラーになるのでそのときは戻して報告する（`direct_*` / `rescue_rloc` は move 済み or 以降未使用なので不要のはず）。コード・コメントの内容は変えない（pure move）。
(b) ~669 `/// \`build_graph\` フェーズ 5: ノード出力: fabric ノード（入力順）→ 未知参加者（ext 昇順）。` → `/// \`build_graph\` フェーズ 5: ノード出力 — fabric ノード（入力順）→ 未知参加者（ext 昇順）。`（他フェーズ doc の `フェーズ N: 名詞（補足）。` 形に揃える）。

- [ ] **Step 5: group.rs テスト名**

実態: 2 回生成が異なることと、string 版が 32 桁小文字 hex であることだけを見ている（bytes 版との一致は検証していない）。

```rust
    #[test]
    fn generated_epoch_keys_differ_and_string_form_is_32_lowercase_hex() {
        let a = generate_epoch_key_bytes();
        let b = generate_epoch_key_bytes();
        assert_ne!(a, b);
        // string 版は 32 桁の小文字 hex（16 バイト鍵）。
        let s = generate_epoch_key();
```

（以降の assert は元のまま。）

- [ ] **Step 6: テスト**

Run: `CARGO_BUILD_JOBS=3 cargo test -p mat-core && CARGO_BUILD_JOBS=3 cargo test -p matd group_provision_without && CARGO_BUILD_JOBS=3 cargo clippy -p mat-core -p matd --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add crates/matd/src/server/mod.rs ARCHITECTURE.md docs/superpowers/specs/2026-09-06-matd-reload-design.md crates/mat-core/src/mesh.rs crates/mat-core/src/group.rs
git commit -m "chore: deferred 小物 — matd テスト改名・ARCHITECTURE の group_settings_ctx 陳腐化記述・reload spec detail 例・mesh rescue_identities の冗長ブロック/doc・group テスト名"
```

---

### Task 5: Taskfile `check` に mat-device core ガード追加

**Files:**
- Modify: `Taskfile.yml`（`check` ~58、`doc:check` ~51 の近く）

- [ ] **Step 1: task 追加**

`doc:check` の後に:

```yaml
  check:device-core:
    desc: mat-device の I/O-free core ガード（CI 相当: --no-default-features でビルドが通ること）
    cmds:
      - cargo check -p mat-device --no-default-features
```

`check` の cmds を:

```yaml
    desc: CI 相当（fmt:check + clippy + doc:check + check:device-core + test）
    cmds:
      - task: fmt:check
      - task: clippy
      - task: doc:check
      - task: check:device-core
      - task: test
```

（順序は `.github/workflows/ci.yml` の該当 step の位置に合わせる — ci.yml を読んで test より前か後か確認し合わせる。`CLAUDE.md` の「`task check` (CI equivalent: fmt:check + clippy + doc:check + test)」は担当外ファイルなので触らず、報告のみ。）

- [ ] **Step 2: 実行確認**

Run: `CARGO_BUILD_JOBS=3 task check:device-core && task --list | grep device-core`
Expected: 成功、一覧に表示

- [ ] **Step 3: Commit**

```bash
git add Taskfile.yml
git commit -m "build(task): check に CI 同等の mat-device --no-default-features ガード（check:device-core）を追加"
```

---

## 最終検証

- `CARGO_BUILD_JOBS=3 cargo test -p mat-core -p matterctl -p matd -p mat-native`
- `CARGO_BUILD_JOBS=3 task check`
- 結果を `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/bins.DONE.md` に記録。
