# Refactor lane `cli-daemon` (mat CLI + matd) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Execute the `cli-daemon` lane of the 2026-09-12 refactor backlog: delete dead code (Tier 1), fold duplicated boilerplate in `mat` / `matd` into shared helpers (Tier 3), split the two oversized `matd` files mechanically (Tier 5), and dedupe test scaffolding (Tier 6) — with **zero behavior change**.

**Architecture:** Pure refactor. New shared helpers land in `mat-core` (`error.rs`, `log.rs`, `alias.rs` — additions only) and in `mat`'s `native_direct.rs`. `matd/server.rs` and `matd/subscription.rs` become directories of submodules with `pub(crate)` re-exports so every existing path keeps compiling. Tests pin every user-visible string that already exists.

**Tech Stack:** Rust workspace (`cargo`, `task check` = fmt:check + clippy `-D warnings` + doc:check `-D warnings` + test). tokio, tracing / tracing-subscriber, serde_json, clap.

**Spec:** `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/cli-daemon.md` (+ `_common.md`, and `audit.md` Tier 1/3/5/6 lines).

## Global Constraints

- Worktree: `/home/noguk/ghq/github.com/nogu3/mat-wt/cli-daemon`, branch `refactor/cli-daemon`. Never merge / push / release / deploy.
- **Files you may touch:** `crates/mat/**` (except `crates/mat/src/probe.rs`), `crates/matd/**`, `crates/mat-core/src/error.rs`, `crates/mat-core/src/log.rs`, `crates/mat-core/src/alias.rs` (one added function), `crates/mat-core/Cargo.toml` (deps needed by `log.rs` only — see Task 1). Nothing else. Do **not** touch `crates/matv`, `crates/mat-native`, `crates/mat-controller`, `crates/mat-device`, `crates/mat/src/probe.rs`.
- **Names reserved by another lane in `error.rs`:** `ErrorKind::as_str`, `MatError::prefixed`. Do not define those.
- **Zero behavior change**: every existing stderr wording, JSON wire shape, exit code and log message stays byte-identical. In particular keep verbatim:
  - `"iface auto-selected (native default)"` (mat) — grepped by `crates/mat/tests/integration.rs`.
  - `"iface auto-selected (matd native default)"`, `"thread iface auto-detected (matd groupcast egress)"`, `"thread iface auto-detected (groupcast egress)"`.
  - `" — run \`mat fabric init\` to bootstrap the credential store"` suffix.
  - `"unknown error kind from matd; mapping to \`other\` for the exit code"`.
  - `"listen client attached"` (grepped by `scripts/e2e-device-m3.sh`).
  - The wire golden tests in `crates/mat/src/matd_client/to_op.rs` and every existing test assertion.
- After each task: `cargo fmt`, `cargo test -p matterctl -p matd` (the `mat` crate's package name is `matterctl`), then commit on the branch. Final task runs `task check`.
- Commit only files edited in this session. Commit message trailer (from the session reminder):
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01FrG9if64fAcdQoSMvLrSWJ
  ```
- Five sibling sessions share the machine; cargo may be slow. Wait, don't retry blindly.
- Output-truncation trap: the `rtk` shell proxy rewrites `grep`/`wc`; for counts use `/usr/bin/grep` / `/usr/bin/wc`.
- Baseline test counts (before any change) are recorded in the scratchpad file `baseline-tests.txt` (per test binary `test result:` lines). Tier 5 splits must keep the matd totals identical.

---

### Task 1: mat-core helpers — `emit_exit`, `with_fabric_init_hint`, `from_wire`, `log::init_stderr`, `log::reset_sigpipe`

**Files:**
- Modify: `crates/mat-core/src/error.rs` (append to `impl ErrorKind`, `impl MatError`, tests)
- Modify: `crates/mat-core/src/log.rs` (append two functions; adjust module doc)
- Modify: `crates/mat-core/Cargo.toml` (add `tracing-subscriber.workspace = true`; add `[target.'cfg(unix)'.dependencies] libc.workspace = true`)

**Interfaces:**
- Produces:
  - `impl ErrorKind { pub fn from_wire(kind: Option<&serde_json::Value>) -> ErrorKind }`
  - `impl MatError { pub fn emit_exit(&self) -> std::process::ExitCode }`
  - `impl MatError { pub fn with_fabric_init_hint(self) -> MatError }`
  - `mat_core::log::init_stderr(default_filter: &str, ansi: bool)`
  - `mat_core::log::reset_sigpipe()`

- [ ] **Step 1: Write failing tests in `error.rs`** (inside the existing `mod tests`):

```rust
    #[test]
    fn emit_exit_maps_kind_to_exit_code() {
        let e = MatError::new(ErrorKind::NodeNotCommissioned, "x");
        assert_eq!(e.emit_exit(), std::process::ExitCode::from(11));
        assert_eq!(
            MatError::new(ErrorKind::Timeout, "x").emit_exit(),
            std::process::ExitCode::from(3)
        );
    }

    #[test]
    fn fabric_init_hint_is_added_once_and_only_for_store_missing() {
        let e = MatError::store_missing("no KVS").with_fabric_init_hint();
        assert_eq!(
            e.detail,
            "no KVS — run `mat fabric init` to bootstrap the credential store"
        );
        // idempotent: a detail that already carries the hint is left alone.
        let again = e.clone().with_fabric_init_hint();
        assert_eq!(again.detail, e.detail);
        // other kinds are untouched.
        let other = MatError::new(ErrorKind::Unreachable, "node 5").with_fabric_init_hint();
        assert_eq!(other.detail, "node 5");
    }

    #[test]
    fn from_wire_decodes_known_kind_and_falls_back_to_other() {
        assert_eq!(
            ErrorKind::from_wire(Some(&serde_json::json!("store_missing"))),
            ErrorKind::StoreMissing
        );
        assert_eq!(
            ErrorKind::from_wire(Some(&serde_json::json!("not_a_kind_we_know"))),
            ErrorKind::Other
        );
        assert_eq!(ErrorKind::from_wire(None), ErrorKind::Other);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mat-core error::tests`
Expected: compile error (`emit_exit`, `with_fabric_init_hint`, `from_wire` not found).

- [ ] **Step 3: Implement in `error.rs`**

Append inside `impl ErrorKind` (after `exit_code`):

```rust
    /// matd 応答 / admin 応答の `error.kind` を `ErrorKind` へ逆引きする。
    /// 未知の kind（新しい matd / 壊れた応答）は warn を 1 行出して `Other`
    /// （exit 1）に倒す。`mat` の `matd_client::emit_response` と `matd` の
    /// `admin_response_to_result` が同じ規律を共有する（逐語コピーの一本化）。
    pub fn from_wire(kind: Option<&serde_json::Value>) -> ErrorKind {
        match kind.and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok()) {
            Some(k) => k,
            None => {
                let raw_kind = kind.cloned().unwrap_or(serde_json::Value::Null);
                tracing::warn!(
                    kind = %raw_kind,
                    "unknown error kind from matd; mapping to `other` for the exit code"
                );
                ErrorKind::Other
            }
        }
    }
```

Append inside `impl MatError` (after `emit`):

```rust
    /// `emit()` して、この kind の exit code を `ExitCode` で返す。CLI の
    /// `Err(e) => { e.emit(); ExitCode::from(e.kind.exit_code()) }` の定型を畳む。
    pub fn emit_exit(&self) -> std::process::ExitCode {
        self.emit();
        std::process::ExitCode::from(self.kind.exit_code())
    }

    /// エンジン構築失敗の写像: `store_missing` に「`mat fabric init` で資材を
    /// 作れ」の誘導を足す（二重付与はしない）。他 kind はそのまま。`mat` の
    /// 直経路と `matd` 起動時の両方が使う。
    pub fn with_fabric_init_hint(mut self) -> Self {
        if self.kind == ErrorKind::StoreMissing && !self.detail.contains("mat fabric init") {
            self.detail = format!(
                "{} — run `mat fabric init` to bootstrap the credential store",
                self.detail
            );
        }
        self
    }
```

- [ ] **Step 4: Add deps + implement `log.rs`**

`crates/mat-core/Cargo.toml` — add under `[dependencies]`: `tracing-subscriber.workspace = true`, and add:

```toml
[target.'cfg(unix)'.dependencies]
libc.workspace = true
```

Replace the module doc's second paragraph (`//! subscriber の組み立ては各バイナリ ... 純関数で置く。`) with:

```rust
//! フィルタ指定の選択規則は純関数（`log_filter_candidates`）、subscriber の
//! 組み立ては [`init_stderr`]（`mat` / `matd` 共有。2026-09-12 の監査で
//! 3 バイナリの逐語コピーを一本化 — `tracing-subscriber` 依存はここに集約）。
//! SIGPIPE の既定化 [`reset_sigpipe`] も同じ理由でここに置く。
```

Append after `log_filter_candidates_from_env`:

```rust
/// 診断ログを stderr に出す subscriber を初期化する。レベルは `MAT_LOG`
/// （無ければ `RUST_LOG`）で制御、どちらも無い / パース不能なら
/// `default_filter`（`mat` は `"warn"`、`matd` は `"info"`）。空文字は未設定
/// 扱い、パースできない指定は次の候補へ送る（[`log_filter_candidates`]）。
/// `ansi` は呼び手が決める（`mat` は tty かつ `NO_COLOR` 未設定のときだけ、
/// `matd` は journald に ANSI を書かないよう常に false）。stdout は JSON 専用
/// なので絶対に汚さない。プロセスで 1 回だけ呼ぶこと（2 回目は panic —
/// `tracing_subscriber::fmt().init()` の性質）。
pub fn init_stderr(default_filter: &str, ansi: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = log_filter_candidates_from_env()
        .into_iter()
        .find_map(|s| EnvFilter::try_new(&s).ok())
        .unwrap_or_else(|| EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .with_writer(std::io::stderr)
        .init();
}

/// SIGPIPE を既定動作（プロセス終了）に戻す。Rust の runtime は SIGPIPE を
/// 無視して起動するので、`mat ... | head -1` / `matd status | jq` のように
/// stdout のパイプ先が先に閉じると `println!` が EPIPE で panic し、stderr に
/// "failed printing to stdout: Broken pipe" を吐いて exit 101 になる。通常の
/// CLI と同じく黙って SIGPIPE で終わらせる（stderr のエラー JSON には影響
/// しない — パイプ先が閉じているのは stdout だけ）。unix 以外は no-op。
/// プロセス起動直後・スレッド生成前に 1 回呼ぶ。
pub fn reset_sigpipe() {
    #[cfg(unix)]
    // SAFETY: SIG_DFL の設定はプロセス起動直後・スレッド生成前の 1 回だけで、
    // 副作用は「EPIPE の代わりに SIGPIPE で終了する」に限られる。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p mat-core`
Expected: all pass including the 3 new tests.

- [ ] **Step 6: Commit**

```bash
git add crates/mat-core/src/error.rs crates/mat-core/src/log.rs crates/mat-core/Cargo.toml Cargo.lock
git commit -m "refactor(mat-core): emit_exit / with_fabric_init_hint / from_wire / log::init_stderr+reset_sigpipe を追加（mat・matd の逐語コピー一本化の受け皿）"
```

---

### Task 2: Tier 1 — `Option<native_direct::Config>` → non-Option; matd dead code; stale comments

**Files:**
- Modify: `crates/mat/src/main.rs` (lines ~198-300: `native_cfg`)
- Modify: `crates/mat/src/commands/unpair.rs`, `discover.rs`, `commission.rs`, `fabric.rs` (`run_rotate_ipk`), `diag.rs` (`node`, `mesh`)
- Modify: `crates/matd/src/native.rs` (remove `build`, `group_settings_ctx` + 2 tests, `GroupOutcome` re-export)
- Modify: `crates/mat/tests/integration.rs:~455-460` (stale comment)

**Interfaces:**
- Produces: every `commands::*` entry point takes `native: &native_direct::Config<'_>` (non-Option). Callers in `main.rs` pass `&native_cfg`.

- [ ] **Step 1: `main.rs`** — replace

```rust
    let native_cfg = Some(native_direct::Config { ... });
    if let Some(cfg) = &native_cfg {
        if let Some(op) = device_op {
            return match native_direct::run(op, &store_path, cfg, args.op_timeout_ms) { ... };
        }
    }
```

with

```rust
    let native_cfg = native_direct::Config {
        iface: &iface_owned,
        thread_iface,
        fabric_index: args.fabric_index,
        issuer_index: args.issuer_index,
    };
    if let Some(op) = device_op {
        return match native_direct::run(op, &store_path, &native_cfg, args.op_timeout_ms) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::debug!(kind = ?e.kind, detail = %e.detail, "native direct failed");
                e.emit();
                ExitCode::from(e.kind.exit_code())
            }
        };
    }
```

and change every `native_cfg.as_ref()` in the `Dedicated` match below to `&native_cfg` (discover, commission, unpair, diag node, diag mesh, fabric rotate-ipk = 6 sites).

- [ ] **Step 2: commands** — in each of the 6 functions change the parameter type `native: Option<&crate::native_direct::Config<'_>>` → `cfg: &crate::native_direct::Config<'_>` (unpair/discover/commission/fabric/diag keep their existing local name `cfg`), and delete the `let cfg = native.ok_or_else(|| MatError::new(ErrorKind::Other, "...: native backend not configured (internal)"))?;` block in each:
  - `unpair.rs::run` (`"unpair: native backend not configured (internal)"`)
  - `discover.rs::run` (`"discover: ..."`)
  - `commission.rs::run` (`"commission: ..."`)
  - `fabric.rs::run_rotate_ipk` (`"rotate-ipk: ..."`)
  - `diag.rs::node` (`"diag node: ..."`)
  - `diag.rs::mesh` (`"diag mesh: ..."`) — here the `ok_or_else` lives inside the `else` branch; the branch becomes `crate::native_direct::diag_mesh_probe(cfg, store.root(), &targets)?`.
  Remove now-unused `ErrorKind` imports if clippy flags them (check each file: `discover.rs` still uses `ErrorKind::Unreachable`; `unpair.rs` uses it in `is_device_side_failure`; `commission.rs` uses it in `native_commission`; `fabric.rs` uses it; `diag.rs` uses it).
  Confirm with `/usr/bin/grep -rn "native backend not configured" crates/mat` → 0 hits.

- [ ] **Step 3: `matd/native.rs`** —
  - line 18: `pub use mat_native::group::{GroupCtx, GroupOutcome};` → `pub use mat_native::group::GroupCtx;`
  - delete `pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError>` (with its doc, lines ~95-99) and fix the doc on `build_with_resolver` from `/// [\`Self::build\`] と同じだが Resolver を注入する（...）` to `/// KVS から資格情報を1回読み、NOC を自己発行し、UDP transport を bind、iface の scope_id を解決して実確立器を構築する（Resolver は注入 — matd は CachingResolver を渡す）。プロセス寿命で不変。`
  - fix the `Debug` impl comment `（build のテスト）` → `（build_with_resolver のテスト）`.
  - delete `pub fn group_settings_ctx(...)` with its doc (lines ~153-158) and the two tests `group_settings_ctx_reflects_injected_value` / `group_settings_ctx_is_none_without_injection`.
  - `/usr/bin/grep -rn "group_settings_ctx\|GroupOutcome" crates/matd crates/mat` → only comment mentions in `server.rs` (lines ~1925, 1958, 2066) and `matd/tests/integration.rs:433` may remain; update `server.rs:1925` comment `（group_settings_ctx を注入すれば ...）` → `（group_settings を注入すれば ...）` and `:1958` `group_settings_ctx が未構成` → `group_settings が未構成`. Leave historical mentions of `GroupOutcome::Unavailable` (they describe old behavior).

- [ ] **Step 4: stale comment `crates/mat/tests/integration.rs` (~line 455-460)** — replace the comment inside `group_provision_rejects_bad_epoch_key`:

```rust
    // require_node(5) はここでは通る（台帳にある）。epoch key の検証は
    // controller state 書込（KVS/chip-tool）より前に走るので、この失敗は
    // バックエンドに一切触れない（`provision_controller_state` 冒頭で
    // `resolve_epoch_key` を呼ぶ — `crates/mat/src/commands/group.rs` 参照）。
```
with
```rust
    // require_node(5) はここでは通る（台帳にある）。epoch key の検証は
    // engine 構築より前に走るので、この失敗はバックエンドに一切触れない
    // （`native_direct::execute` 冒頭で `resolve_epoch_key` を呼ぶ）。
```

- [ ] **Step 5: Build + test**

Run: `cargo fmt && cargo clippy -p matterctl -p matd --all-targets -- -D warnings && cargo test -p matterctl -p matd`
Expected: green; matd unit test count = baseline − 2.

- [ ] **Step 6: Commit**

```bash
git add crates/mat/src crates/mat/tests/integration.rs crates/matd/src/native.rs crates/matd/src/server.rs
git commit -m "refactor(tier1): native_direct::Config を非 Option 化（死枝 6 本削除）、matd NativeBackend::build / group_settings_ctx / GroupOutcome 再輸出を削除"
```

---

### Task 3: mat engine/runtime helpers — `Config::to_native`, `block_on`, `with_engine`

**Files:**
- Modify: `crates/mat/src/native_direct.rs` (`Config`, `execute`, `diag_im_probe`, `diag_mesh_probe`, remove `map_engine_build_error`)
- Modify: `crates/mat/src/commands/commission.rs` (`native_commission`), `fabric.rs` (`run_rotate_ipk`), `discover.rs` (`native_commissionables`)

**Interfaces:**
- Produces (all `pub(crate)` in `native_direct`):
  - `impl Config<'_> { pub(crate) fn to_native(&self, store_root: &Path) -> NativeConfig }`
  - `pub(crate) fn block_on<T>(fut: impl std::future::Future<Output = T>) -> Result<T, MatError>` — builds a current-thread tokio runtime (`enable_all`), `Err(Other, "tokio runtime: {e}")` only if the runtime cannot be built.
  - `pub(crate) fn with_engine<T, Fut>(cfg: &Config<'_>, store_root: &Path, f: impl FnOnce(Engine) -> Fut) -> Result<T, MatError> where Fut: Future<Output = Result<T, MatError>>` — `to_native` → `block_on` → `Engine::build(..).await.map_err(MatError::with_fabric_init_hint)?` → `f(engine).await`.
- Consumes: `MatError::with_fabric_init_hint` (Task 1).

- [ ] **Step 1: Write a failing unit test in `native_direct.rs` tests**

```rust
    /// `Config::to_native` は store root と CLI 由来 4 フィールドをそのまま写す。
    #[test]
    fn config_to_native_copies_every_field() {
        let cfg = Config {
            iface: "lo",
            thread_iface: Some(mat_native::ThreadIfaceChoice::Explicit("wpan0".into())),
            fabric_index: 2,
            issuer_index: 1,
        };
        let n = cfg.to_native(std::path::Path::new("/tmp/store"));
        assert_eq!(n.store, std::path::PathBuf::from("/tmp/store"));
        assert_eq!(n.iface, "lo");
        assert!(matches!(n.thread_iface, Some(mat_native::ThreadIfaceChoice::Explicit(ref s)) if s == "wpan0"));
        assert_eq!(n.fabric_index, 2);
        assert_eq!(n.issuer_index, 1);
    }

    /// `block_on` は current-thread runtime で future を完走させる。
    #[test]
    fn block_on_runs_future_to_completion() {
        assert_eq!(block_on(async { 41 + 1 }).unwrap(), 42);
    }

    /// `with_engine` の build 失敗は store_missing + `mat fabric init` 誘導。
    #[test]
    fn with_engine_maps_build_failure_with_fabric_init_hint() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            iface: "lo",
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        };
        let err = with_engine(&cfg, dir.path(), |_engine| async { Ok::<(), MatError>(()) })
            .unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::StoreMissing);
        assert!(err.detail.contains("mat fabric init"), "{}", err.detail);
    }
```

(If `ThreadIfaceChoice` does not derive `PartialEq`, the `matches!` form above avoids needing it.)

- [ ] **Step 2: Run to verify fail** — `cargo test -p matterctl native_direct::tests` → compile error.

- [ ] **Step 3: Implement**

In `native_direct.rs` after `pub(crate) struct Config<'a> {..}` add:

```rust
impl Config<'_> {
    /// `mat-native` の `NativeConfig` へ写す（store root は呼び手が持つ）。
    pub(crate) fn to_native(&self, store_root: &Path) -> NativeConfig {
        NativeConfig {
            store: store_root.to_path_buf(),
            iface: self.iface.to_string(),
            thread_iface: self.thread_iface.clone(),
            fabric_index: self.fabric_index,
            issuer_index: self.issuer_index,
        }
    }
}

/// one-shot CLI 用の current-thread tokio runtime で future を完走させる。
/// runtime 構築失敗のみ Err（`Other`, "tokio runtime: …"）。
pub(crate) fn block_on<T>(fut: impl std::future::Future<Output = T>) -> Result<T, MatError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            MatError::new(
                mat_core::error::ErrorKind::Other,
                format!("tokio runtime: {e}"),
            )
        })?;
    Ok(rt.block_on(fut))
}

/// engine を 1 度構築して `f` に渡す（直経路の共通足場）。構築失敗は
/// `MatError::with_fabric_init_hint` で store_missing に `mat fabric init`
/// 誘導を付す（M8c-3: chip-tool フォールバック撤去後のハードエラー化）。
pub(crate) fn with_engine<T, Fut>(
    cfg: &Config<'_>,
    store_root: &Path,
    f: impl FnOnce(Engine) -> Fut,
) -> Result<T, MatError>
where
    Fut: std::future::Future<Output = Result<T, MatError>>,
{
    let native_cfg = cfg.to_native(store_root);
    block_on(async {
        let engine = Engine::build(&native_cfg)
            .await
            .map_err(MatError::with_fabric_init_hint)?;
        f(engine).await
    })?
}
```

Then rewrite the three call sites:

`execute` — replace from `let rt = tokio::runtime::Builder...` through `})?;` with:

```rust
    let hint = hint_for(op);
    let body = with_engine(cfg, store.root(), |engine| async move {
        let budget = op.budget_applies();
        if budget && op_timeout_ms > 0 {
            // 直経路にも matd 経路と同じ予算セマンティクス（exit 3）。
            run_op_with_deadline(run_with_engine(&engine, op), op_timeout_ms, node_id, hint).await
        } else {
            run_with_engine(&engine, op).await
        }
    })?;
```

`diag_im_probe` body becomes:

```rust
    with_engine(cfg, store_root, |engine| async move {
        Ok(diag_im_with_engine(&engine, node_id, endpoint).await)
    })
```

`diag_mesh_probe` body becomes:

```rust
    with_engine(cfg, store_root, |engine| async move {
        let mut out = Vec::new();
        for &node_id in targets {
            let result = mesh_probe_one(&engine, node_id).await;
            if let Err(e) = &result {
                tracing::warn!(node_id, kind = ?e.kind, detail = %e.detail,
                    "mesh probe failed for node; continuing");
            }
            out.push(MeshProbeItem { node_id, result });
        }
        Ok(out)
    })
```

Delete `pub(crate) fn map_engine_build_error` and its doc; update the `diag_im_probe` doc (`... 「run」の build 失敗と同じ写像 — store_missing に \`mat fabric init\` 誘導を付す`) to reference `with_engine`. Drop the now-unused `use mat_native::{Engine, NativeConfig};` parts only if unused (both are still used).

`commission.rs::native_commission` — replace `let ncfg = mat_native::NativeConfig {..}; let rt = ...; rt.block_on(mat_native::commission::commission(&ncfg, &req))` with:

```rust
    let ncfg = cfg.to_native(store.root());
    crate::native_direct::block_on(mat_native::commission::commission(&ncfg, &req))?
```

`fabric.rs::run_rotate_ipk` — replace the `NativeConfig {..}` literal + runtime + `.map_err(crate::native_direct::map_engine_build_error)?` with:

```rust
    let native_cfg = cfg.to_native(store.root());
    let outcome = crate::native_direct::block_on(rotate_ipk::run(&native_cfg, &params))?
        .map_err(MatError::with_fabric_init_hint)?;
```
and remove the now-unused `use mat_native::NativeConfig;`.

`discover.rs::native_commissionables` — keep signature `Result<Vec<DiscoveredDevice>, Box<dyn std::error::Error>>` and wording; replace the runtime block with:

```rust
    let scope_id = mat_controller::dnssd::iface_index(iface)?;
    let list = crate::native_direct::block_on(mat_controller::dnssd::browse_commissionable(
        scope_id,
        mat_controller::dnssd::BROWSE_WINDOW,
    ))
    .map_err(|e| Box::<dyn std::error::Error>::from(e.detail))??;
```

Verify: `/usr/bin/grep -rn "new_current_thread" crates/mat/src` → only `native_direct.rs` (`block_on` + the two existing deadline unit tests) and `probe.rs` (other lane, untouched). `/usr/bin/grep -rn "NativeConfig {" crates/mat/src` → only `to_native`.

- [ ] **Step 4: Test** — `cargo fmt && cargo clippy -p matterctl --all-targets -- -D warnings && cargo test -p matterctl`. Green.

- [ ] **Step 5: Commit**

```bash
git add crates/mat/src
git commit -m "refactor(mat): Engine 起動 20 行×3 / NativeConfig リテラル×5 / tokio runtime×6 を Config::to_native + block_on + with_engine に集約、map_engine_build_error → MatError::with_fabric_init_hint"
```

---

### Task 4: mat exit/emit sweep — `emit_exit` ×13, dispatch tails, `listen` field passing, hint.rs send line

**Files:**
- Modify: `crates/mat/src/main.rs`
- Modify: `crates/mat/src/matd_client/mod.rs` (`dispatch`, `dispatch_auto`, `unsupported_exit` stays)
- Modify: `crates/mat/src/matd_client/listen.rs` (`dispatch_listen` signature, `ListenParams`)
- Modify: `crates/mat/src/matd_client/hint.rs` (`request_line`, tests)

**Interfaces:**
- Consumes: `MatError::emit_exit` (Task 1).
- Produces:
  - `pub(crate) struct ListenParams { pub node: Option<u64>, pub endpoint: Option<u16>, pub cluster: Option<String>, pub attribute: Option<String>, pub event: Option<String>, pub count: u32, pub timeout_ms: u64, pub reconnect: bool }` in `matd_client::listen`, re-exported from `matd_client` as `pub(crate) use listen::ListenParams;`
  - `pub fn dispatch_listen(sockets: &[PathBuf], p: &ListenParams) -> ExitCode`
  - `fn exchange_and_emit(stream: UnixStream, mut op_json: Value, op: &DeviceOp, op_timeout_ms: u64) -> ExitCode` (private, `matd_client/mod.rs`)
  - `fn request_line(stream: UnixStream, op: &Value, read_timeout: Duration) -> std::io::Result<Option<String>>` (private, `hint.rs`)

- [ ] **Step 1: `emit_exit` sweep.** Replace every `e.emit(); ExitCode::from(e.kind.exit_code())` / `return ExitCode::from(e.kind.exit_code())` pair with `e.emit_exit()` (keep any preceding `tracing::debug!` line). Sites:
  - `main.rs`: fabric init/list result (1), group list (1), listen Direct arm (`MatError::new(MatdUnavailable, "...").emit(); ExitCode::from(...)` → `MatError::new(...).emit_exit()`) (1), `classify` Err (1), iface autodetect Err (1), `native_direct::run` Err (1), final `result` Err (1).
  - **Do not** touch the two `return ExitCode::from(2)` sites (`validate_transport`, `resolve_command`'s `_ => ExitCode::from(2)`) — those are deliberate exit-2 overrides; but `ErrorKind::StoreParse => ExitCode::from(e.kind.exit_code())` may stay as is.
  - `matd_client/mod.rs::dispatch`: `MatError::new(MatdUnavailable, &detail).emit(); return ExitCode::from(...)` → `return MatError::new(ErrorKind::MatdUnavailable, &detail).emit_exit();` (1); exchange Err arm (1). `dispatch_auto` exchange Err arm (1). (The `Err(detail) => { MatError::new(Other, &detail).emit(); return ExitCode::from(2) }` exit-2 site stays.)
  - `matd_client/listen.rs`: `dispatch_listen` connect Err (1), `run_listen_stream` Err (1), `finish_on_timeout` (1). The two "internal bug" `node_id.id()` / `endpoint.id()` arms disappear in Step 3.
  Verify: `/usr/bin/grep -rn "ExitCode::from(e.kind.exit_code())\|ExitCode::from(ErrorKind::[A-Za-z]*.exit_code())" crates/mat/src` → 0 hits except the `StoreParse` arm in `main.rs`.

- [ ] **Step 2: `dispatch` / `dispatch_auto` tails.** Add to `matd_client/mod.rs`:

```rust
/// 接続済み stream で op を 1 往復して結果を出力する（`dispatch` / `dispatch_auto`
/// の共通末尾）。予算対象 op には deadline_ms を付与し read timeout を掛ける。
fn exchange_and_emit(
    stream: std::os::unix::net::UnixStream,
    mut op_json: Value,
    op: &DeviceOp,
    op_timeout_ms: u64,
) -> ExitCode {
    let read_timeout = attach_deadline(&mut op_json, op.budget_applies(), op_timeout_ms);
    match exchange_on_stream(stream, &op_json, read_timeout) {
        Ok(resp) => emit_response(resp),
        Err(e) => e.emit_exit(),
    }
}
```
`dispatch` ends with `exchange_and_emit(stream, op_json, op, op_timeout_ms)`; `dispatch_auto` ends with `Some(exchange_and_emit(stream, op_json, op, op_timeout_ms))`. Make `op_json` bindings non-`mut` there. The `#[cfg(doc)] use std::os::unix::net::UnixStream;` at the top can become a real `use std::os::unix::net::UnixStream;` (drop the `#[cfg(doc)]` and the comment above it) since it is now used.

- [ ] **Step 3: `dispatch_listen` field passing.** In `listen.rs`:

```rust
/// `mat listen` の引数（alias は main で数値に確定済み）。`Command::Listen` を
/// ここで再分解しないため、「非 Listen command が来た」「未解決 alias が届いた」
/// の internal-bug アームが不要になる。
pub(crate) struct ListenParams {
    pub node: Option<u64>,
    pub endpoint: Option<u16>,
    pub cluster: Option<String>,
    pub attribute: Option<String>,
    pub event: Option<String>,
    pub count: u32,
    pub timeout_ms: u64,
    pub reconnect: bool,
}

pub fn dispatch_listen(sockets: &[PathBuf], p: &ListenParams) -> ExitCode {
    let op = listen_request_json(p.node, p.endpoint, &p.cluster, &p.attribute, &p.event);
    if p.reconnect {
        return run_listen_reconnecting(sockets, &op, p.count, p.timeout_ms);
    }
    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            return MatError::new(
                ErrorKind::MatdUnavailable,
                format!("{detail}; `mat listen` requires a running matd"),
            )
            .emit_exit();
        }
    };
    tracing::info!(socket = %socket.display(), "listening via matd");
    match run_listen_stream(stream, &op, p.count, p.timeout_ms) {
        Ok(code) => code,
        Err(detail) => MatError::new(ErrorKind::MatdUnavailable, &detail).emit_exit(),
    }
}
```
Remove `use crate::cli::Command;` and `use mat_core::alias::NodeRef;` from `listen.rs`. In `matd_client/mod.rs`: `pub use listen::dispatch_listen;` → `pub use listen::{dispatch_listen, ListenParams};`.

In `main.rs` replace the `if let Command::Listen { .. } = &command { return match resolve_route(...) {...} }` block with:

```rust
    if let Command::Listen {
        node_id,
        endpoint,
        cluster,
        attribute,
        event,
        count,
        timeout_ms,
        reconnect,
    } = &command
    {
        // alias は resolve 層で Id に確定済み。ここで数値に落として listen 層へ
        // フィールドで渡す（Command の再分解を listen 層に持ち込まない）。
        let ids = node_id
            .as_ref()
            .map(mat_core::alias::NodeRef::id)
            .transpose()
            .and_then(|n| {
                endpoint
                    .as_ref()
                    .map(mat_core::alias::EndpointRef::id)
                    .transpose()
                    .map(|e| (n, e))
            });
        let (node, endpoint) = match ids {
            Ok(v) => v,
            Err(e) => return e.emit_exit(),
        };
        let params = matd_client::ListenParams {
            node,
            endpoint,
            cluster: cluster.clone(),
            attribute: attribute.clone(),
            event: event.clone(),
            count: *count,
            timeout_ms: *timeout_ms,
            reconnect: *reconnect,
        };
        return match matd_client::resolve_route(
            &args.matd,
            std::env::var_os("MAT_MATD_SOCKET"),
            std::env::var_os("MAT_MATD"),
        ) {
            matd_client::Route::Forced(sockets) | matd_client::Route::Auto(sockets) => {
                matd_client::dispatch_listen(&sockets, &params)
            }
            matd_client::Route::Direct => MatError::new(
                ErrorKind::MatdUnavailable,
                "`mat listen` requires matd (MAT_MATD=0 disables it)",
            )
            .emit_exit(),
        };
    }
```

- [ ] **Step 4: `hint.rs` send line unification.** Add:

```rust
/// `op` を 1 行送り、応答 1 行を `read_timeout` まで待つ。write 失敗は Err、
/// read 失敗（timeout 等）は `Ok(None)`（送受信自体は成立 — 「応答なし」）。
/// `node_touched` と `reload` の 2 本のヒント送信路が共有する核。
fn request_line(
    mut stream: UnixStream,
    op: &Value,
    read_timeout: Duration,
) -> std::io::Result<Option<String>> {
    let mut line = serde_json::to_vec(op)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.set_read_timeout(Some(read_timeout))?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    Ok(reader.read_line(&mut resp).ok().map(|_| resp))
}
```
`send_reload_line` body → `Ok(match request_line(stream, &json!({ "op": "reload" }), RELOAD_ACK_TIMEOUT)? { Some(resp) => classify_reload_ack(&resp), None => ReloadAck::NoAck })` (keep its doc). `send_hint_line` body → `request_line(stream, &json!({ "op": "node_touched", "node_id": node_id }), Duration::from_millis(300)).map(|_| ())` (keep doc; the 300 ms rationale stays). Then rewrite the first two tests (`hint_node_touched_sends_op_line_to_matd`, `hint_node_touched_ignores_old_matd_parse_error_response`) to use the existing `one_shot_matd` helper (move the helper above them):

```rust
    #[test]
    fn hint_node_touched_sends_op_line_to_matd() {
        let (_dir, path, server) = one_shot_matd(b"{\"resubscribing\":true}\n");
        hint_node_touched_at(&[path], 42);
        let v: Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(v, json!({"op":"node_touched","node_id":42}));
    }

    #[test]
    fn hint_node_touched_ignores_old_matd_parse_error_response() {
        let (_dir, path, server) =
            one_shot_matd(b"{\"error\":{\"kind\":\"parse_error\",\"detail\":\"unknown op\"}}\n");
        hint_node_touched_at(&[path], 7); // panic せず完走すること
        server.join().unwrap();
    }
```

- [ ] **Step 5: Test** — `cargo fmt && cargo clippy -p matterctl --all-targets -- -D warnings && cargo test -p matterctl` (this includes `tests/listen.rs`, `tests/matd_auto.rs`, `tests/integration.rs` which pin the listen exit codes / wording). Green.

- [ ] **Step 6: Commit**

```bash
git add crates/mat/src
git commit -m "refactor(mat): e.emit()+ExitCode 定型×13 → MatError::emit_exit、dispatch/dispatch_auto 末尾と hint 送信路を共通化、listen は Command 再分解を止めてフィールド渡し"
```

---

### Task 5: `resolve.rs` node+endpoint → `AliasBook::resolve_node_endpoint`

**Files:**
- Modify: `crates/mat-core/src/alias.rs` (one added method after `resolve_endpoint`, + 1 test)
- Modify: `crates/mat/src/resolve.rs` (10 sites)

**Interfaces:**
- Produces: `impl AliasBook { pub fn resolve_node_endpoint(&self, node: &NodeRef, endpoint: &EndpointRef) -> Result<(NodeRef, EndpointRef), MatError> }` — returns `(NodeRef::Id(n), EndpointRef::Id(e))`.

- [ ] **Step 1: Failing test in `alias.rs` tests** (look at the existing tests module for the fixture style — there is a `load` from a temp dir with TOML; reuse the same pattern):

```rust
    #[test]
    fn resolve_node_endpoint_returns_id_wrapped_pair() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("aliases.toml"),
            "[nodes]\nliving-light = 5\n\n[endpoints.living-light]\nnight = 2\n",
        )
        .unwrap();
        let book = AliasBook::load(dir.path()).unwrap();
        let (n, e) = book
            .resolve_node_endpoint(
                &NodeRef::Alias("living-light".into()),
                &EndpointRef::Alias("night".into()),
            )
            .unwrap();
        assert_eq!(n, NodeRef::Id(5));
        assert_eq!(e, EndpointRef::Id(2));
        // 数値はパススルー。
        let (n, e) = book
            .resolve_node_endpoint(&NodeRef::Id(7), &EndpointRef::Id(1))
            .unwrap();
        assert_eq!((n, e), (NodeRef::Id(7), EndpointRef::Id(1)));
    }
```

- [ ] **Step 2: Run** `cargo test -p mat-core alias::tests::resolve_node_endpoint` → compile error.

- [ ] **Step 3: Implement** after `resolve_endpoint`:

```rust
    /// node を解決してから、その node 文脈で endpoint を解決し、両方を `Id` に
    /// 包んで返す。`mat` の CLI 層が node+endpoint を取る 10 サブコマンドで
    /// 「解決 → 再包装」を毎回書かないためのまとめ。
    pub fn resolve_node_endpoint(
        &self,
        node: &NodeRef,
        endpoint: &EndpointRef,
    ) -> Result<(NodeRef, EndpointRef), MatError> {
        let n = self.resolve_node(node)?;
        let e = self.resolve_endpoint(n, endpoint)?;
        Ok((NodeRef::Id(n), EndpointRef::Id(e)))
    }
```

- [ ] **Step 4: Apply in `resolve.rs`** — for each of Read, Write, Invoke, On, Off, ColorTemp, Level, Color, `DiagCommand::Thread`, `DiagCommand::Node` replace

```rust
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Read {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                cluster,
                attribute,
            }
```
with
```rust
            let (node_id, endpoint) = book.resolve_node_endpoint(&node_id, &endpoint)?;
            Command::Read {
                node_id,
                endpoint,
                cluster,
                attribute,
            }
```
(`Color` keeps `spec: resolve_color_spec(&book, spec)?`.) The `Listen` arm keeps its hand-written logic (node optional). `EndpointRef` import in `resolve.rs` stays (Listen arm uses it).

- [ ] **Step 5: Test** — `cargo fmt && cargo test -p mat-core -p matterctl resolve` and the full `cargo test -p matterctl`. Green (`resolve::tests` pin alias behavior).

- [ ] **Step 6: Commit**

```bash
git add crates/mat-core/src/alias.rs crates/mat/src/resolve.rs
git commit -m "refactor(mat): node+endpoint の解決→再包装×10 を AliasBook::resolve_node_endpoint へ"
```

---

### Task 6: matd/common — `write_line` ×5, `from_wire`, `init_stderr`/`reset_sigpipe`, iface helpers, `process::exit` → `Err`, bare unwrap

**Files:**
- Modify: `crates/matd/src/server.rs` (5 NDJSON writes)
- Modify: `crates/matd/src/main.rs`
- Modify: `crates/mat/src/main.rs` (init + iface helpers)
- Modify: `crates/mat/src/matd_client/stream.rs` (`emit_response`)
- Modify: `crates/mat/Cargo.toml` (drop `tracing-subscriber`, drop `[target.'cfg(unix)'.dependencies] libc`), `crates/matd/Cargo.toml` (move `tracing-subscriber` to `[dev-dependencies]` — unit tests in `server.rs`/`subscription.rs` still use it; drop `libc`)

**Interfaces:**
- Consumes: `ErrorKind::from_wire`, `MatError::emit_exit`, `log::init_stderr`, `log::reset_sigpipe` (Task 1).
- Produces:
  - `server.rs`: `async fn write_line(write_half: &mut tokio::net::unix::OwnedWriteHalf, v: &Value) -> std::io::Result<()>` (private; Task 7 moves it to `server/wire.rs`).
  - `mat/main.rs` and `matd/main.rs` each get two private fns of identical shape: `fn select_iface(explicit: &Option<String>) -> Result<String, MatError>` and `fn select_thread_iface(explicit: &Option<String>) -> Option<mat_native::ThreadIfaceChoice>` (only the log strings differ).

- [ ] **Step 1: `server.rs` `write_line`.** Add near `error_response`:

```rust
/// 応答 / イベント 1 件を NDJSON 1 行で書き、flush する。JSON 化不能（実質
/// 到達しない）は `{}` を書く — 行を欠かして相手の枚数勘定を狂わせない。
async fn write_line(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    v: &Value,
) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    buf.push(b'\n');
    write_half.write_all(&buf).await?;
    write_half.flush().await
}
```
Replace the 5 copies (`let mut buf = serde_json::to_vec(..).unwrap_or_else(..); buf.push(b'\n'); write_half.write_all(&buf).await?; write_half.flush().await?;`): listen-rejected (`write_line(&mut write_half, &error_response(req.id, &e)).await?;`), ack (`write_line(&mut write_half, &ack).await?;`), op response (`write_line(&mut write_half, &response).await?;` — the comment "応答をワイヤに出し切ってから停止を発火する" stays above the `if is_shutdown`), `stream_events` event (`write_line(write_half, &ev.to_json()).await?;`), lag body (`write_line(write_half, &body).await?;`). Keep `ack` `mut` (it is mutated to insert `id`).

- [ ] **Step 2: `from_wire`.** `mat/src/matd_client/stream.rs::emit_response`: replace the `let kind = match err.get("kind").and_then(..) { Some(k) => k, None => { ...warn...; ErrorKind::Other } };` block with `let kind = ErrorKind::from_wire(err.get("kind"));`. `matd/src/main.rs::admin_response_to_result`: same replacement (`let kind = ErrorKind::from_wire(err.get("kind"));`). Existing tests (`admin_response_unknown_kind_falls_back_to_other`, etc.) pin behavior.

- [ ] **Step 3: log init + SIGPIPE.** `mat/src/main.rs`: `main()` starts with

```rust
    mat_core::log::reset_sigpipe();
    // 対話 tty では色を許すが、パイプや mando 経由では ANSI を出さない
    // （構造化ログを grep できる形に保つ）。`NO_COLOR` も尊重する —
    // with_ansi はライブラリ既定の NO_COLOR 判定を無条件に上書きするので、
    // ここで自分で見る必要がある（https://no-color.org/）。既定 level は warn。
    mat_core::log::init_stderr(
        "warn",
        std::io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
    );
```
Delete `fn reset_sigpipe()` and `fn init_tracing()` and `use tracing_subscriber::{fmt, EnvFilter};` from `mat/main.rs`. `matd/src/main.rs::main`: replace the `unsafe { libc::signal(...) }` block and the whole `tracing_subscriber::fmt()...init()` block with

```rust
    // `matd status | jq` のようにパイプ先が先に閉じても EPIPE panic しない
    // （常駐 serve は stdout に書かないので影響しない）。
    mat_core::log::reset_sigpipe();
    // レベルは mat 本体と同じく `MAT_LOG`（無ければ `RUST_LOG`）、既定は info
    // （常駐デーモンなので状態遷移は既定で残す）。デーモンなので ANSI は常に
    // 無効 — tracing-subscriber は tty 判定をせず、既定では journald に
    // `^[[3mnode_id^[[0m^[[2m=^[[0m42` を書いて `grep node_id=42` が空振りする。
    mat_core::log::init_stderr("info", false);
```
Cargo: `mat/Cargo.toml` remove `tracing-subscriber.workspace = true` and the `[target.'cfg(unix)'.dependencies]` table; `matd/Cargo.toml` remove `tracing-subscriber.workspace = true` from `[dependencies]`, add it under `[dev-dependencies]`, remove the `[target.'cfg(unix)'.dependencies]` table. Confirm with `/usr/bin/grep -rn "libc::\|tracing_subscriber" crates/mat/src crates/matd/src` → only the two `#[cfg(test)]` CapturingWriter sites in matd.

- [ ] **Step 4: matd `process::exit` → `Err`, `main` → `ExitCode`, bare unwrap.**
  - `matd/src/main.rs::main`: signature `fn main() -> std::process::ExitCode`; runtime failure → `return MatError::new(ErrorKind::Other, format!("failed to start tokio runtime: {e}")).emit_exit();`; the final `if let Err(e) = runtime.block_on(run(Cli::parse())) { e.emit(); std::process::exit(...) }` → `match runtime.block_on(run(Cli::parse())) { Ok(()) => std::process::ExitCode::SUCCESS, Err(e) => e.emit_exit() }`.
  - `serve_daemon`: the iface block becomes `let iface = select_iface(&cli.iface)?;` and thread iface `let thread_iface = select_thread_iface(&cli.thread_iface);`; the `sub_config` block becomes `let sub_config = matd::subscribe_config::load(&store_path)?;` (keep the comment above it). Verify `/usr/bin/grep -n "process::exit" crates/matd/src/main.rs` → 0.
  - `send_admin_op`: `let mut line = serde_json::to_vec(&serde_json::json!({ "op": op })).unwrap();` → `.map_err(|e| MatError::new(ErrorKind::Other, format!("failed to encode {op}: {e}")))?;`
  - `serve_daemon`'s engine-build `Err(mut e)` arm: replace the hand-rolled `if e.kind == StoreMissing && !e.detail.contains(...) { e.detail = format!(...) }` with `let e = e.with_fabric_init_hint();` (make the arm `Err(e)`), keep the `tracing::warn!` and `NativeState::Unavailable(e)`. Update the comment to `// mat 側の直経路（native_direct::with_engine）と同じ写像。`.
  - Add to `matd/src/main.rs` (module level, doc'd):

```rust
/// native の iface: 明示指定を優先、未設定なら自動検出。候補 0 / 複数は
/// ハードエラー（全 op が死ぬ設定不備なので fail-fast — 起動拒否）。
/// `mat` の `main.rs` と同型（ログ文言だけ違う）。
fn select_iface(explicit: &Option<String>) -> Result<String, MatError> {
    match explicit {
        Some(i) => Ok(i.clone()),
        None => {
            let i = mat_native::iface_select::autodetect()?;
            tracing::info!(iface = %i, "iface auto-selected (matd native default)");
            Ok(i)
        }
    }
}

/// groupcast の Thread TUN 追加送出先: 明示指定を優先、未設定なら wpan* を
/// 自動検出（失敗は None のまま — LAN 単独送出）。`mat` と同型。
fn select_thread_iface(explicit: &Option<String>) -> Option<mat_native::ThreadIfaceChoice> {
    match explicit {
        Some(n) => Some(mat_native::ThreadIfaceChoice::Explicit(n.clone())),
        None => mat_native::iface_select::detect_thread_iface_auto().map(|n| {
            tracing::info!(iface = %n, "thread iface auto-detected (matd groupcast egress)");
            mat_native::ThreadIfaceChoice::Auto(n)
        }),
    }
}
```
  - Same two fns in `mat/src/main.rs` with log strings `"iface auto-selected (native default)"` and `"thread iface auto-detected (groupcast egress)"`, doc `/// ... 候補 0 / 複数はハードエラー（黙って落とさない — spec 設計 3）。matd の main.rs と同型。`. Use them: `let iface_owned = match select_iface(&args.iface) { Ok(i) => i, Err(e) => return e.emit_exit() }; let thread_iface = select_thread_iface(&args.thread_iface);`.

- [ ] **Step 5: Test** — `cargo fmt && cargo clippy -p matterctl -p matd --all-targets -- -D warnings && cargo test -p matterctl -p matd`. `crates/mat/tests/integration.rs` (`iface auto-selected (native default)` grep) and `crates/matd/tests/cli.rs` must pass. Also manually confirm the matd binary still logs without ANSI: `cargo run -p matd -- --socket /nonexistent/dir/x.sock 2>&1 | head -3` should show plain text, exit non-zero, no `^[[`.

- [ ] **Step 6: Commit**

```bash
git add crates/mat crates/matd Cargo.lock
git commit -m "refactor(matd/mat): NDJSON 書き出し×5 → write_line、ErrorKind::from_wire、log::init_stderr+reset_sigpipe へ移行（mat/matd の libc・tracing-subscriber 直依存を撤去）、matd の async 内 process::exit を Err 返却に、iface 選択ヘルパを mat/matd 同型に"
```

---

### Task 7: Tier 5 — split `matd/src/server.rs` → `server/{mod,listen,wire,oplog}.rs`

**Files:**
- Move: `crates/matd/src/server.rs` → `crates/matd/src/server/mod.rs` (`git mv`)
- Create: `crates/matd/src/server/listen.rs`, `crates/matd/src/server/wire.rs`, `crates/matd/src/server/oplog.rs`

**Interfaces:**
- Produces (paths preserved via re-exports in `server/mod.rs`): `pub(crate) use wire::{note_op_expectation, to_device_op, MatdOp};` `pub(crate) use listen::ListenFilter;` — `crate::server::to_device_op`, `crate::server::note_op_expectation`, `crate::server::ListenFilter` keep working. `pub enum NativeState`, `ReloadStats`, `DaemonInfo`, `pub async fn serve` stay in `mod.rs`.

Pure move: **no line of production code changes** except `use` lines, visibility (`fn` → `pub(super) fn` where a sibling needs it), and module declarations. Do the move with `sed -n 'A,Bp'` into new files, never by retyping.

- [ ] **Step 1: Record baseline** — `cargo test -p matd --lib 2>&1 | /usr/bin/grep "test result"` → note N unit tests.

- [ ] **Step 2: `git mv crates/matd/src/server.rs crates/matd/src/server/mod.rs`** and add after the module doc / imports:

```rust
mod listen;
mod oplog;
mod wire;

pub(crate) use listen::ListenFilter;
pub(crate) use wire::{note_op_expectation, to_device_op, MatdOp};
use oplog::log_op;
use wire::{error_response, write_line};
```

- [ ] **Step 3: Cut production items into the submodules** (line numbers are for the pre-Task-6 file; re-locate by symbol, not by number):
  - `server/listen.rs`: `async fn stream_events(...)` (from its doc `/// listen ストリーム: ...` to its closing brace) and `pub(crate) struct ListenFilter` + `impl ListenFilter` (through `matches`). `stream_events` becomes `pub(super) async fn`. Module doc: `//! \`listen\` op: ack 後の接続を占有してフィルタ一致イベントを NDJSON で流し続ける（[\`stream_events\`]）とそのフィルタ（[\`ListenFilter\`]）。`
  - `server/wire.rs`: `write_line`, `error_response` (both `pub(super)`), `pub(crate) enum MatdOp`, `pub(crate) fn to_device_op`, `fn op_report_expectation`, `fn op_state_target`, `pub(crate) fn note_op_expectation`. Module doc: `//! ワイヤ層: NDJSON 1 行の書き出し・エラー応答の形、そして \`protocol::Op\` → \`mat_native::op\`（[\`to_device_op\`]）と op → 購読レポート期待（[\`note_op_expectation\`]）の写像。`
  - `server/oplog.rs`: `const SLOW_OP_MS`, `enum OpLogClass`, `fn classify_op_log`, `pub(super) fn log_op`. Module doc: `//! op ログ 1 行の level 方針（warn = 経路の問題、info = 要求側の問題 / 遅い成功、debug = 通常成功）。`
  - Leave in `mod.rs`: `NativeState`, `ReloadStats`, `DaemonInfo`, `serve`, `handle_conn`, `OpTurn`, `abort_op`, `DEFAULT_OP_BUDGET`, `op_deadline`, `dispatch`, `run_op`, `require_node`, `reload_body`, `status_body`.
  - Each new file gets only the `use` lines it needs (`use serde_json::{json, Value}; use mat_core::error::{ErrorKind, MatError};` etc. — let the compiler tell you; remove unused imports from `mod.rs`).

- [ ] **Step 4: Distribute tests.** Move each `#[test]`/`#[tokio::test]` to the file whose item it exercises:
  - → `listen.rs` tests: `listen_filter_matches_by_resolved_ids`, `listen_filter_event_lines_match_by_cluster_but_never_with_an_attribute_filter`, `listen_filter_rejects_attribute_and_event_together`, `listen_filter_event_filter_2x2_and_specific_id`.
  - → `wire.rs` tests: `to_device_op_maps_node_ops_with_resolved_ids`, `to_device_op_applies_timed_override`, `to_device_op_rejects_unresolved_names_and_unencodable_values`, `to_device_op_maps_group_ops_and_shortcuts`, `to_device_op_rejects_non_device_ops_without_panic`, `op_report_expectation_only_when_value_actually_changes`, plus helper `group_on_op` if only wire tests use it (check with grep; if `mod.rs` tests also use it, keep it in `mod.rs` tests as `pub(super)` and import).
  - → `oplog.rs` tests: `err`, `path_failures_are_warn_worthy`, `request_side_errors_do_not_pollute_warn`, `slow_threshold_is_inclusive_at_300ms`, `elapsed_time_does_not_change_error_classification`, `CapturingWriter` + its `MakeWriter` impl, `capture_log_op`, `read_op`, `log_op_omits_absent_fields_entirely`, `log_op_emits_grep_friendly_fields_on_ok`, `log_op_level_and_message_follow_classification`.
  - Everything else stays in `mod.rs` tests. Helpers used across files (`make_store`, `store_with_node_5`, `test_daemon`, `ScriptedEstablisher`, `group_on_op`) stay in `mod.rs`'s `mod tests` marked `pub(super)`; child test modules import with `use crate::server::tests::make_store;` (a child of `server` may name `server::tests` items that are `pub(super)`).

- [ ] **Step 5: Verify pure move.** `cargo fmt && cargo clippy -p matd --all-targets -- -D warnings && cargo test -p matd`. Unit-test count must equal the Step 1 baseline. Run `git diff -M --stat HEAD` and eyeball `git diff -M HEAD -- crates/matd/src/server/mod.rs` — only deletions of moved blocks plus the `mod`/`use` lines. `/usr/bin/wc -l crates/matd/src/server/*.rs` — no file above ~1300 lines.

- [ ] **Step 6: Commit**

```bash
git add crates/matd/src/server.rs crates/matd/src/server
git commit -m "refactor(matd): server.rs (2537 行) を server/{mod,listen,wire,oplog}.rs に機械分割（pub(crate) 再輸出でパス維持、挙動変更 0）"
```

---

### Task 8: Tier 5 — split `matd/src/subscription.rs` → `subscription/{mod,health,pump,events}.rs`

**Files:**
- Move: `crates/matd/src/subscription.rs` → `crates/matd/src/subscription/mod.rs` (`git mv`)
- Create: `crates/matd/src/subscription/health.rs`, `crates/matd/src/subscription/pump.rs`
- Modify: `crates/matd/src/subscription/events.rs` (receives attribute `Event`)

**Interfaces:**
- Produces (re-exports in `subscription/mod.rs` so `crate::subscription::{SubHealth, Emitted, Event, EventItem, events_from_event_reports, events_from_report, events_from_report_at, spawn_subscription_manager, LEDGER_RESCAN_INTERVAL, ...}` all still resolve):
  ```rust
  mod events;
  mod health;
  mod pump;
  pub use events::{events_from_event_reports, events_from_report, events_from_report_at, Emitted, Event, EventItem};
  pub use health::SubHealth;
  pub(crate) use health::{classify_against_cache, classify_failure, FailureLog, NodeSubStatus, ValueKey};
  pub(crate) use pump::{pump_verdict, silence_deadline, PumpEnd};
  ```
  (Adjust the `pub(crate)` list to whatever `server/`, `main.rs`, `tests/` and the tests actually name — grep `crate::subscription::` and `subscription::` across `crates/matd`.)

Pure move, same discipline as Task 7.

- [ ] **Step 1: Record baseline** — `cargo test -p matd --lib 2>&1 | /usr/bin/grep "test result"`.

- [ ] **Step 2: `git mv`** and add the module declarations/re-exports above (replace the existing `mod events; pub use events::{events_from_event_reports, Emitted, EventItem};`).

- [ ] **Step 3: Cut production items:**
  - `subscription/events.rs` (append): `pub struct Event {..}` with its doc, `impl Event { pub fn to_json }`, `pub fn events_from_report`, `pub fn events_from_report_at`. Remove `use super::Event;` from `events.rs` (it is now local). Update the events.rs module doc line `//! 属性行（\`super::Event\`）は無改変で` → `//! 属性行（[\`Event\`]）は無改変で`.
  - `subscription/health.rs`: `STUCK_WARN_AFTER`, `pub(crate) enum NodeSubStatus`, `pub struct SubHealth` + `struct TouchedState` + `impl SubHealth` (entire block through `status_nodes`), `pub(crate) enum FailureLog`, `pub(crate) fn classify_failure`, `pub(crate) type ValueKey`, `pub(crate) fn classify_against_cache`. Module doc: `//! op 相関ヘルス表 + 購読ランタイム状態（[\`SubHealth\`]）と、priming 差分回復の値キャッシュ判定（[\`classify_against_cache\`]）。`. `STUCK_WARN_AFTER` is also read by pump (`classify_failure` call sites) — keep it here as `pub(super) const` if pump references it directly; otherwise private.
  - `subscription/pump.rs`: `BACKOFF_INITIAL`, `BACKOFF_MAX`, `PUMP_SLICE`, `OP_GRACE`, `SILENCE_SLACK`, `silence_deadline`, `PumpEnd`, `pump_verdict`, `next_backoff`, `jittered_backoff`, `pub(super) async fn node_subscription_loop`, `enum PrimingRule` + impl, `fn emit_event_lines`, `async fn run_subscription_once`. Module doc: `//! 1 ノードの購読ループ（[\`node_subscription_loop\`]）: resolve → 専用 CASE → wildcard Subscribe → ポンプ。失敗・死亡は指数 backoff で再購読、op 相関 + 無音 deadline の死活判定は純関数 [\`pump_verdict\`]。`
  - Leave in `mod.rs`: module doc, `LEDGER_RESCAN_INTERVAL`, `STAGGER_STEP`, `stagger_delay`, `pub(crate) struct SubscribeScope` (make fields `pub(super)` since pump reads them), `pub fn spawn_subscription_manager`, and any supervisor helpers between them.
  - Fix the stale sentence in the old `mod events;` comment (`このファイルが既に約 2400 行あるので別モジュールへ置く。`) — delete it.

- [ ] **Step 4: Distribute tests.** Harness (`spawn_manager`, `AttrRx`, `spawn_manager_with`, `collect_event_lines`) stays in `mod.rs` tests as `pub(super)` items (`pub(super) fn spawn_manager`, `pub(super) struct AttrRx` with `pub(super) async fn recv`). Move:
  - → `events.rs` tests: `event_json_uses_chip_tool_names_and_numeric_fallback`, `events_from_report_keeps_scalars_and_drops_containers`, `list_append_elements_never_produce_recovered_events` (if it only uses `events_from_report*` + `Event`; if it uses `classify_against_cache`, put it in `health.rs`).
  - → `health.rs` tests: `failure_log_first_then_quiet_then_single_warn`, `sub_health_notes_and_clears_pending_respecting_clusters`, `classify_against_cache_promotes_only_changed_priming`, `classify_promotion_emits_info_log_with_old_and_new_values` (with its `Buf` MakeWriter), `sub_health_observe_updates_shared_value_cache`, `subhealth_survives_poisoned_locks`, `clusters_exposes_narrowing_none_for_wildcard`, `note_event_number_keeps_the_max`, `note_event_number_rewinds_only_for_live_reports`.
  - → `pump.rs` tests: `backoff_doubles_from_5s_capped_at_60s`, `jittered_backoff_range_preserves_median`, `silence_deadline_is_max_interval_plus_slack`, `pump_verdict_prioritizes_op_grace_then_silence`.
  - Everything that spins the manager (`manager_*`, `op_grace_*`, `live_report_*`, `priming_*`, `establish_failures_*`, `pump_session_error_*`, `silent_subscription_*`, `proven_silence_*`, `backoff_resets_*`, `initial_batch_*`, `status_nodes_*`, `touched_*`, `subscribe_failure_closes_session`, `note_op_expectation_wires_level_cluster_cache`, `noop_op_*`, `changing_op_*`, `live_events_*`, `live_event_below_*`, `event_scope_off_*`, `stagger_delay_spreads_batches_only`) stays in `mod.rs` tests.

- [ ] **Step 5: Verify pure move.** `cargo fmt && cargo clippy -p matd --all-targets -- -D warnings && cargo test -p matd`. Unit-test count equals Step 1. `/usr/bin/wc -l crates/matd/src/subscription/*.rs` — `mod.rs` should now be mostly supervisor + manager tests.

- [ ] **Step 6: Commit**

```bash
git add crates/matd/src/subscription.rs crates/matd/src/subscription
git commit -m "refactor(matd): subscription.rs (2937 行) を subscription/{mod,health,pump,events}.rs に機械分割（属性 Event は events.rs へ、再輸出でパス維持、挙動変更 0）"
```

---

### Task 9: Tier 6 — test scaffolding dedupe (`connect_with_retry`, `mat/tests/common`, fake matd unification)

**Files:**
- Modify: `crates/matd/tests/integration.rs`
- Create: `crates/mat/tests/common/mod.rs`
- Modify: `crates/mat/tests/integration.rs`, `crates/mat/tests/matd_auto.rs`, `crates/mat/tests/listen.rs`

**Interfaces:**
- `matd/tests/integration.rs`: `async fn connect_with_retry(socket: &Path) -> UnixStream` (250 × 20 ms, panics with `"could not connect to matd socket"`).
- `mat/tests/common/mod.rs` (`#![allow(dead_code)]` at top — each test binary uses a subset):
  ```rust
  pub const NODE5_LEDGER: &str = r#"{"version":1,"nodes":{"5":{"node_id":5,"address":"192.0.2.10","commissioned_at":"2026-01-01T00:00:00+09:00"}}}"#;
  pub fn store_with_node5() -> TempDir
  pub fn mat(store: &Path) -> Command                 // MAT_IFACE=lo, MAT_MATD=0, --store
  pub fn mat_auto(store: &Path, socket: &Path) -> Command   // MAT_IFACE=lo, MAT_MATD_SOCKET, env_remove MAT_MATD, --store
  pub fn mat_listen(socket: &Path, extra: &[&str]) -> Command
  /// fake matd: 接続ごとに (書き出す行, hold_ms) を順に消費。戻り値 = 各接続で受けた要求行。
  pub fn spawn_fake_matd(socket: PathBuf, sessions: Vec<(Vec<&'static str>, u64)>) -> JoinHandle<Vec<String>>
  ```

- [ ] **Step 1: matd `connect_with_retry`.** Add near `roundtrip`:

```rust
/// serve が bind するまで待って connect する。並列テスト（既定）でランタイムが
/// 飽和すると spawn した serve タスクの socket bind が遅れるので、窓は広め
/// （250×20ms=5s）に取る（テストの意味は不変、決定化のためだけ）。
async fn connect_with_retry(socket: &std::path::Path) -> UnixStream {
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(socket).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("could not connect to matd socket");
}
```
Replace the 8 `let mut x = None; for _ in 0..250 {..} let x = x.expect("could not connect to matd socket");` loops (in `roundtrip`, `client_disconnect_aborts_op_and_drops_slot`, `pipelined_second_request_is_buffered_not_lost`, `invalid_request_json_is_parse_error`, `listen_acks_then_streams_filtered_events`, `listen_with_event_wildcard_receives_only_event_lines`, `listen_with_no_filter_receives_both_attribute_and_event_lines`, `lagged_listener_gets_error_line_and_disconnect`) with `let stream = connect_with_retry(&socket).await;` (binding name as each site uses, e.g. `let mut a = connect_with_retry(&socket).await;`). Also `status_op_returns_daemon_snapshot` does a bare `UnixStream::connect(&socket).await.unwrap()` — leave it (not a retry copy). Verify `/usr/bin/grep -c "0..250" crates/matd/tests/integration.rs` → 1.

- [ ] **Step 2: `mat/tests/common/mod.rs`.** Create with the helpers above, bodies copied verbatim from the current `integration.rs` (`mat`, `store_with_node5`), `matd_auto.rs` (`mat_auto`), `listen.rs` (`mat_listen`), and:

```rust
/// fake matd。接続ごとに `sessions` の 1 要素を消費: 要求 1 行を読み、`lines`
/// を順に書き、`hold_ms` だけ接続を開いたまま保持してから閉じる（EOF）。
/// 戻り値は各接続で受けた要求行（op の検証用）。
pub fn spawn_fake_matd(
    socket: PathBuf,
    sessions: Vec<(Vec<&'static str>, u64)>,
) -> JoinHandle<Vec<String>> {
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let mut reqs = Vec::new();
        for (lines, hold_ms) in sessions {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut req)
                .unwrap();
            for l in lines {
                stream.write_all(l.as_bytes()).unwrap();
            }
            stream.flush().unwrap();
            if hold_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(hold_ms));
            }
            reqs.push(req);
            // drop(stream) = EOF
        }
        reqs
    })
}
```
Each test file adds `mod common;` and `use common::{...};`, deletes its local copies. Keep each file's existing doc comment on why `MAT_IFACE=lo` / `MAT_MATD=0` (move it onto the common fn docs).

- [ ] **Step 3: fake matd callers.**
  - `matd_auto.rs`: `spawn_fake_matd(socket.clone())` → `spawn_fake_matd(socket.clone(), vec![(vec!["{\"via\":\"fake-matd\",\"value\":true}\n"], 0)])`; `let req = matd.join().unwrap();` → `let req = &matd.join().unwrap()[0];` (adjust `req.contains(...)` — works on `&String`). Define `const FAKE_RESP: &str = "{\"via\":\"fake-matd\",\"value\":true}\n";` at file top to keep call sites short.
  - `listen.rs`: delete `spawn_fake_matd_stream` and `spawn_fake_matd_sessions`; add local shims that keep call sites unchanged:

```rust
/// ack + イベント N 行 を 1 接続で流す fake matd（`hold_ms` = 送信後の保持時間）。
fn spawn_fake_matd_stream(socket: PathBuf, events: usize, hold_ms: u64) -> JoinHandle<Vec<String>> {
    spawn_fake_matd_sessions(socket, vec![(events, hold_ms)])
}

/// 接続ごとに (イベント数, hold_ms) を順に消費する fake matd。
fn spawn_fake_matd_sessions(socket: PathBuf, sessions: Vec<(usize, u64)>) -> JoinHandle<Vec<String>> {
    let sessions = sessions
        .into_iter()
        .map(|(events, hold_ms)| {
            let mut lines = vec![ACK];
            lines.extend(std::iter::repeat_n(EVENT, events));
            (lines, hold_ms)
        })
        .collect();
    spawn_fake_matd(socket, sessions)
}
```
  (`std::iter::repeat_n` is stable since 1.82; MSRV is 1.87.) The two tests that read the request (`listen_count_reached_exits_zero_with_events_on_stdout`, `listen_filters_are_forwarded_in_request`) change `let req = matd.join().unwrap();` → `let req = &matd.join().unwrap()[0];`. `listen_reconnect_waits_for_matd_to_appear` has `.join().unwrap();` inside a thread — the returned `Vec<String>` is simply dropped; fine.

- [ ] **Step 4: Test** — `cargo fmt && cargo clippy -p matterctl -p matd --all-targets -- -D warnings && cargo test -p matterctl -p matd`. Integration test counts equal baseline.

- [ ] **Step 5: Commit**

```bash
git add crates/matd/tests/integration.rs crates/mat/tests
git commit -m "refactor(tests): matd 接続リトライ×8 → connect_with_retry、mat の store_with_node5/mat/mat_auto/mat_listen と fake matd×3 を tests/common へ"
```

---

### Task 10: Final verification + DONE report

**Files:**
- Create: `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/cli-daemon.DONE.md`

- [ ] **Step 1:** `task check` in the worktree (fmt:check + clippy + doc:check + test, whole workspace). Must be green. If `doc:check` flags a broken intra-doc link introduced by a move (e.g. `[\`stream_events\`]`), fix the link — do not weaken the lint.
- [ ] **Step 2:** `cargo clippy --all-targets --features ble -p matterctl -- -D warnings` (CI also compiles the ble feature) — if `bluer`/libdbus is missing locally and it fails at link time only, note it in DONE.
- [ ] **Step 3:** `git status` clean except the DONE file (which lives outside the repo); `git log --oneline main..HEAD` lists the 9 commits.
- [ ] **Step 4:** Write `cli-daemon.DONE.md` with sections: やった項目（task → commit sha）/ 見送った項目と理由 / task check 結果（verbatim last lines）/ 実機 E2E が必要か（**必要**: Engine 起動統合 `with_engine` と matd server/subscription 分割は本番経路 → マージ時に親がスモーク）/ フォローアップ（`matv` の `init_stderr`/`reset_sigpipe` 呼び替え = device レーン; `commands/diag.rs::probe_error_kind` の `unwrap_or(Null)` → `ErrorKind::as_str()` = native-core レーン到着後; iface 選択ヘルパの mat-native への共通化; FakeEstablisher group provision fixture ×3 → `FakeConn::with_group_provision_fixture()` 到着後; `mat-core` に `tracing-subscriber`/`libc` 依存が増えた事実）。

---

## Self-review

- **Spec coverage:** Tier 1 (Task 2), Tier 3 mat (Tasks 3, 4, 5), Tier 3 matd/common (Task 6; `commands/diag.rs:280` deferred as instructed), Tier 5 (Tasks 7, 8), Tier 6 (Task 9; FakeEstablisher fixture deferred as instructed), 見送り items untouched, verification (Task 10). mat-core additions limited to the named functions plus the `resolve_node_endpoint` the spec allows.
- **Placeholder scan:** none.
- **Type consistency:** `Config::to_native(&self, store_root: &Path) -> NativeConfig`; `block_on<T>(fut) -> Result<T, MatError>`; `with_engine(cfg, store_root, FnOnce(Engine) -> Fut)`; `ListenParams` owned `Option<String>` fields matching `listen_request_json(&Option<String>, ..)`; `write_line(&mut OwnedWriteHalf, &Value)`; `from_wire(Option<&Value>)`; `spawn_fake_matd(PathBuf, Vec<(Vec<&'static str>, u64)>) -> JoinHandle<Vec<String>>` — used consistently across Tasks 1–9.
