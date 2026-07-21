# matd socket 既定パスの subdir 化 + mat の候補探索 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd の既定 bind を `$XDG_RUNTIME_DIR/matd/matd.sock`（systemd `RuntimeDirectory` 慣習）へ移し、mat の既定探索を「subdir 新既定 → flat 旧既定」の候補 connect 試行にして、ssh から `MAT_MATD_SOCKET` 前置きなしで matd を発見できるようにする。

**Architecture:** 既定パスの定義は従来どおり `mat-core::socket` に一元化し、mat 側探索用の `default_socket_candidates()` を追加（env 注入の純関数でテスト）。`mat::matd_client::Route` の `Auto`/`Forced` が候補 `Vec<PathBuf>` を持ち、connect 失敗で次候補へ。matd は既定パス時のみ親ディレクトリを 0700 で自動作成する。

**Tech Stack:** Rust（workspace 0.26.0 → 0.27.0）。既存クレート構成のまま、新規依存なし。

**Spec:** `docs/superpowers/specs/2026-07-21-matd-socket-subdir-default-design.md`

## Global Constraints

- stdout は純粋 JSON のみ、診断は stderr の `tracing`（CLAUDE.md 設計ルール 2/3）。
- 明示指定の優先順は不変: `--matd <path>` > `MAT_MATD_SOCKET`（非空）> 既定候補。明示時は候補探索しない（1 本のみ）。
- `XDG_RUNTIME_DIR` 不在時の既定は従来どおり `/tmp/matd.sock` の 1 本（subdir 化しない — /tmp 直下の固定名 dir は squatting 面が増えるだけ）。
- 全候補 connect 失敗時の挙動は現行踏襲: Auto → native 直経路フォールバック、`listen` → `matd_unavailable`（exit 13）、Forced → エラー（detail に試行した全候補を列挙）。
- コミット毎に `task check`（fmt:check + clippy -D warnings + test）を通す。
- コメント密度・日本語コメントの流儀は周辺コードに合わせる。

---

### Task 1: `mat-core::socket` — 新既定パス + 候補リスト + dir 作成ヘルパ

**Files:**
- Modify: `crates/mat-core/src/socket.rs`（全面書き換え、現 15 行）

**Interfaces:**
- Produces:
  - `pub fn default_socket_path() -> PathBuf` — matd の bind 既定。`$XDG_RUNTIME_DIR/matd/matd.sock`、XDG 不在なら `/tmp/matd.sock`（**戻り値が subdir 形式に変わる**）
  - `pub fn default_socket_candidates() -> Vec<PathBuf>` — mat の探索候補（順序保証: subdir → flat。XDG 不在なら `/tmp/matd.sock` の 1 本）
  - `pub fn ensure_socket_dir(socket: &Path) -> std::io::Result<()>` — 親 dir を 0700 で作成（存在時は no-op）
- Consumes: なし（葉モジュール）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/socket.rs` の末尾に `#[cfg(test)]` モジュールを追加（env 注入の純関数 `*_from` を直接テストするので env 変更レースは無い）:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_path_is_subdir_under_xdg() {
        assert_eq!(
            default_socket_path_from(Some("/run/user/1000".into())),
            PathBuf::from("/run/user/1000/matd/matd.sock")
        );
    }

    #[test]
    fn default_path_falls_back_to_flat_tmp_without_xdg() {
        assert_eq!(
            default_socket_path_from(None),
            PathBuf::from("/tmp/matd.sock")
        );
    }

    #[test]
    fn candidates_are_subdir_then_flat_under_xdg() {
        assert_eq!(
            default_socket_candidates_from(Some("/run/user/1000".into())),
            vec![
                PathBuf::from("/run/user/1000/matd/matd.sock"),
                PathBuf::from("/run/user/1000/matd.sock"),
            ]
        );
    }

    #[test]
    fn candidates_are_single_tmp_without_xdg() {
        assert_eq!(
            default_socket_candidates_from(None),
            vec![PathBuf::from("/tmp/matd.sock")]
        );
    }

    #[test]
    fn ensure_socket_dir_creates_0700_parent_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("matd").join("matd.sock");

        ensure_socket_dir(&sock).unwrap();
        let meta = std::fs::metadata(sock.parent().unwrap()).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);

        // 既存 dir でも成功する（冪等）。
        ensure_socket_dir(&sock).unwrap();
    }
}
```

`mat-core` の `Cargo.toml` に `tempfile` が dev-dependency として無ければ追加する（workspace 内の他クレートに合わせ `tempfile.workspace = true` 形式。workspace 定義が無い場合のみバージョン直書き）。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-core socket -- --nocapture`
Expected: コンパイルエラー（`default_socket_path_from` 等が未定義）

- [ ] **Step 3: 実装**

`crates/mat-core/src/socket.rs` 全体を以下へ:

```rust
//! 上流（`mat --matd` クライアント）⇔ `matd` の unix socket 既定パス。
//!
//! `mat`（既定探索）と `matd`（`--socket` 省略時の bind）が同じ既定を指すよう、
//! 一箇所で定義する。0.27.0 で既定を systemd の `RuntimeDirectory=matd` 慣習
//! （`$XDG_RUNTIME_DIR/matd/matd.sock`）へ移行。mat 側の探索は移行期互換のため
//! 旧 flat パス（`$XDG_RUNTIME_DIR/matd.sock`）も第 2 候補として connect を試す。

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// `matd` の既定 bind パス: `$XDG_RUNTIME_DIR/matd/matd.sock`、XDG 不在なら
/// `/tmp/matd.sock`（/tmp 直下に固定名 dir を作ると他ユーザーの dir squatting
/// 面が増えるだけなので flat のまま）。
pub fn default_socket_path() -> PathBuf {
    default_socket_path_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// [`default_socket_path`] の env 注入版（テスト用に純関数）。
pub fn default_socket_path_from(xdg_runtime_dir: Option<OsString>) -> PathBuf {
    match xdg_runtime_dir {
        Some(dir) => PathBuf::from(dir).join("matd").join("matd.sock"),
        None => PathBuf::from("/tmp/matd.sock"),
    }
}

/// `mat` の既定探索候補（順に connect を試す）: subdir 新既定 → flat 旧既定。
/// XDG 不在なら `/tmp/matd.sock` の 1 本。stale socket は connect が失敗する
/// ので自然に次候補へ進む。
pub fn default_socket_candidates() -> Vec<PathBuf> {
    default_socket_candidates_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// [`default_socket_candidates`] の env 注入版（テスト用に純関数）。
pub fn default_socket_candidates_from(xdg_runtime_dir: Option<OsString>) -> Vec<PathBuf> {
    match xdg_runtime_dir {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            vec![dir.join("matd").join("matd.sock"), dir.join("matd.sock")]
        }
        None => vec![PathBuf::from("/tmp/matd.sock")],
    }
}

/// socket の親ディレクトリを 0700 で作成する（存在すれば no-op）。matd が
/// 既定パスで bind する前に呼ぶ（明示 `--socket` の親不在は従来どおり bind
/// エラーに任せるので呼ばない）。
pub fn ensure_socket_dir(socket: &Path) -> io::Result<()> {
    let Some(dir) = socket.parent() else {
        return Ok(());
    };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}
```

（テストモジュールは Step 1 のものを同ファイル末尾に維持。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core socket`
Expected: 5 テスト PASS

注意: この時点で workspace 全体は **まだ壊れていない**（`default_socket_path` の呼び出し側は署名不変、戻り値が変わっただけ）。ただし `crates/mat/src/matd_client.rs` の既存テスト `resolve_route_three_states` は `default_socket_path()` を記号的に参照しているので通り続ける。`cargo test --workspace` で回帰が無いことも見る。

- [ ] **Step 5: コミット**

```bash
task check
git add crates/mat-core/src/socket.rs crates/mat-core/Cargo.toml Cargo.lock
git commit -m "feat(mat-core): matd socket 既定を \$XDG_RUNTIME_DIR/matd/matd.sock へ、mat 探索候補と dir 作成ヘルパ追加"
```

---

### Task 2: matd — 既定 bind の subdir 化（dir 自動作成）+ ヘルプ文

**Files:**
- Modify: `crates/matd/src/main.rs:36-37`（`--socket` ヘルプ文）、`crates/matd/src/main.rs:103-111`（serve の既定パス解決）

**Interfaces:**
- Consumes: Task 1 の `mat_core::socket::{default_socket_path, ensure_socket_dir}`
- Produces: なし（バイナリの挙動変更のみ）。`stop`（`crates/matd/src/main.rs:226`）は `default_socket_path` 経由で自動追従する — コード変更不要。

- [ ] **Step 1: serve の既定パス解決に dir 作成を差し込む**

`crates/matd/src/main.rs` の `serve_daemon` 冒頭（現 104–107 行）:

```rust
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(mat_core::socket::default_socket_path);
```

を以下へ（lock ファイルは `<socket>.lock` で同 dir に置かれるため、`lock::acquire` より**前**に dir を作る）:

```rust
    // 既定パス（$XDG_RUNTIME_DIR/matd/matd.sock）のときだけ親 dir を 0700 で
    // 用意する。明示 --socket の親不在は従来どおり bind エラーに任せる。
    // lock ファイル（<socket>.lock）も同 dir なので acquire より前に作る。
    let socket = match cli.socket.clone() {
        Some(p) => p,
        None => {
            let p = mat_core::socket::default_socket_path();
            mat_core::socket::ensure_socket_dir(&p).map_err(|e| {
                MatError::new(
                    ErrorKind::Other,
                    format!("failed to create socket dir for {}: {e}", p.display()),
                )
            })?;
            p
        }
    };
```

- [ ] **Step 2: `--socket` ヘルプ文を更新**

`crates/matd/src/main.rs:36` の doc コメント:

```rust
    /// 上流 unix socket のパス。未指定なら $XDG_RUNTIME_DIR/matd/matd.sock
    /// （dir は 0700 で自動作成。XDG 不在なら /tmp/matd.sock）。serve / stop 両方が使う。
```

- [ ] **Step 3: 動作確認（手元スモーク）**

Run: `cargo build -p matd && XDG_RUNTIME_DIR=$(mktemp -d) target/debug/matd --help > /dev/null && XDG_RUNTIME_DIR=$(mktemp -d) sh -c 'target/debug/matd --fabric-index 1 & MATD_PID=$!; sleep 2; ls -ld "$XDG_RUNTIME_DIR/matd" "$XDG_RUNTIME_DIR/matd/matd.sock"; kill $MATD_PID'`
Expected: `drwx------` の `matd/` dir と `matd.sock` が既定パスに出来ている（native backend 構築の warn は出てよい — socket bind とは独立に起動する設計）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: 既存テスト全 PASS（lock.rs のテストは tempdir 明示パスなので影響なし）

- [ ] **Step 5: コミット**

```bash
task check
git add crates/matd/src/main.rs
git commit -m "feat(matd): 既定 bind を \$XDG_RUNTIME_DIR/matd/matd.sock へ（親 dir 0700 自動作成）"
```

---

### Task 3: mat — 既定探索を候補リスト化（subdir → flat の connect 試行）

**Files:**
- Modify: `crates/mat/src/matd_client.rs`（`Route` / `resolve_route` / `dispatch` / `dispatch_auto` / `dispatch_listen` / import / 既存テスト）
- Modify: `crates/mat/src/main.rs:72-108`（Route の受け側）

**Interfaces:**
- Consumes: Task 1 の `mat_core::socket::default_socket_candidates`
- Produces（`crates/mat/src/main.rs` が使う）:
  - `pub enum Route { Forced(Vec<PathBuf>), Auto(Vec<PathBuf>), Direct }`
  - `pub fn dispatch(sockets: &[PathBuf], command: &Command) -> ExitCode`
  - `pub fn dispatch_auto(sockets: &[PathBuf], command: &Command) -> Option<ExitCode>`
  - `pub fn dispatch_listen(sockets: &[PathBuf], command: &Command) -> ExitCode`
  - `resolve_route` の署名は不変（戻り値の中身だけ Vec 化）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/src/matd_client.rs` の既存テスト `resolve_route_three_states`（現 693–738 行）を候補リスト前提に書き換え、`connect_candidates` の単体テストを追加する:

```rust
    #[test]
    fn resolve_route_three_states() {
        let some_path = PathBuf::from("/x/y.sock");
        let dflt = mat_core::socket::default_socket_candidates();

        // --matd <path> → 強制 matd（明示パスが MAT_MATD_SOCKET より優先、候補 1 本）。
        assert_eq!(
            resolve_route(
                &Some(Some(some_path.clone())),
                Some("/env.sock".into()),
                None
            ),
            Route::Forced(vec![some_path])
        );
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET（1 本）> 既定候補。
        assert_eq!(
            resolve_route(&Some(None), None, None),
            Route::Forced(dflt.clone())
        );
        assert_eq!(
            resolve_route(&Some(None), Some("/env.sock".into()), None),
            Route::Forced(vec![PathBuf::from("/env.sock")])
        );
        // MAT_MATD=truthy → 強制 matd。
        assert_eq!(
            resolve_route(&None, None, Some("1".into())),
            Route::Forced(dflt.clone())
        );
        // MAT_MATD=falsy → 強制直。socket env が設定されていても probe しない。
        assert_eq!(resolve_route(&None, None, Some("0".into())), Route::Direct);
        assert_eq!(
            resolve_route(&None, Some("/env.sock".into()), Some("off".into())),
            Route::Direct
        );
        // 未設定 → 自動。probe 先は MAT_MATD_SOCKET（非空、1 本）> 既定候補。
        assert_eq!(resolve_route(&None, None, None), Route::Auto(dflt.clone()));
        assert_eq!(
            resolve_route(&None, Some("/env.sock".into()), None),
            Route::Auto(vec![PathBuf::from("/env.sock")])
        );
        // truthy でも falsy でもない値 → 未設定と同じ（自動）。
        assert_eq!(
            resolve_route(&None, None, Some("abc".into())),
            Route::Auto(dflt)
        );
    }

    #[test]
    fn connect_candidates_falls_through_to_second_socket() {
        // 候補 1 = 存在しないパス、候補 2 = 生きた listener → 候補 2 で繋がる。
        let dir = tempfile::tempdir().unwrap();
        let dead = dir.path().join("matd").join("matd.sock"); // 不在（dir ごと無い）
        let alive = dir.path().join("matd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&alive).unwrap();

        // 戻り値の &Path は候補スライスを借用するため、候補は変数に束縛してから渡す。
        let candidates = [dead, alive.clone()];
        let (_stream, used) = connect_candidates(&candidates).expect("second candidate connects");
        assert_eq!(used, alive.as_path());
    }

    #[test]
    fn connect_candidates_skips_stale_socket_file() {
        // 候補 1 = stale socket ファイル（listener 死亡済み）→ connect 失敗で候補 2 へ。
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap()); // ファイルは残る
        assert!(stale.exists());
        let alive = dir.path().join("alive.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&alive).unwrap();

        let candidates = [stale, alive.clone()];
        let (_stream, used) = connect_candidates(&candidates).expect("stale is skipped");
        assert_eq!(used, alive.as_path());
    }

    #[test]
    fn connect_candidates_error_lists_all_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.sock");
        let b = dir.path().join("b.sock");
        let err = connect_candidates(&[a.clone(), b.clone()]).unwrap_err();
        assert!(err.contains(&a.display().to_string()), "got: {err}");
        assert!(err.contains(&b.display().to_string()), "got: {err}");
    }
```

`mat` クレートに `tempfile` の dev-dependency が無ければ追加（既存 tests/ で使用中なので通常は済んでいる）。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat --lib matd_client`
Expected: コンパイルエラー（`Route::Forced(Vec<_>)` / `connect_candidates` 未定義）

- [ ] **Step 3: 実装**

`crates/mat/src/matd_client.rs`:

(a) import 変更（現 29 行）: `use mat_core::socket::default_socket_path;` → `use mat_core::socket::default_socket_candidates;`

(b) `Route`（現 31–42 行）:

```rust
/// mat の実行経路。`resolve_route` が決める。socket は探索候補リスト
/// （明示指定は 1 本、既定は subdir 新既定 → flat 旧既定の順で connect 試行）。
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// 明示有効化（`--matd` / `MAT_MATD=truthy`）: matd 固定。全候補接続失敗は
    /// エラー、非対応 op は exit 2。フォールバックしない。
    Forced(Vec<PathBuf>),
    /// 既定（どちらも未設定）: 候補へ順に connect を試み、成功なら matd、
    /// 全滅なら mat 自身の native 直経路にフォールバック。
    Auto(Vec<PathBuf>),
    /// 明示無効化（`MAT_MATD=falsy`）: 常に native 直経路。probe もしない。
    Direct,
}
```

(c) `resolve_route`（現 52–77 行）: doc コメントの「既定パス」を「既定候補（subdir → flat）」へ改め、本体を:

```rust
pub fn resolve_route(
    flag: &Option<Option<PathBuf>>,
    env_socket: Option<OsString>,
    env_enable: Option<OsString>,
) -> Route {
    match flag {
        // --matd <path> → 明示パスで強制 matd（候補 1 本）。
        Some(Some(path)) => Route::Forced(vec![path.clone()]),
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET > 既定候補。
        Some(None) => Route::Forced(sockets_from_env_or_default(env_socket)),
        None => match env_enable.as_deref() {
            Some(v) if is_truthy(v) => Route::Forced(sockets_from_env_or_default(env_socket)),
            Some(v) if is_falsy(v) => Route::Direct,
            // 未設定（or 解釈不能な値）→ 自動検出。
            _ => Route::Auto(sockets_from_env_or_default(env_socket)),
        },
    }
}

/// 有効化済みのときに使う socket 候補: `MAT_MATD_SOCKET`（非空、1 本）> 既定候補。
fn sockets_from_env_or_default(env_socket: Option<OsString>) -> Vec<PathBuf> {
    env_socket
        .filter(|s| !s.is_empty())
        .map(|s| vec![PathBuf::from(s)])
        .unwrap_or_else(default_socket_candidates)
}
```

（旧 `socket_from_env_or_default` は削除。）

(d) 候補 connect ヘルパ（`exchange` の位置、現 358–363 行の `exchange` を置き換え）:

```rust
/// 候補 socket へ順に connect し、最初に成功した stream と使用パスを返す。
/// 全滅は Err（試行した全パスと各エラーを列挙 — Forced 経路のエラー detail 用）。
fn connect_candidates(sockets: &[PathBuf]) -> Result<(UnixStream, &Path), String> {
    let mut attempts = Vec::new();
    for socket in sockets {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok((stream, socket)),
            Err(e) => attempts.push(format!("{} ({e})", socket.display())),
        }
    }
    Err(format!("could not connect to matd at {}", attempts.join(", ")))
}
```

（`exchange` は `dispatch` からしか呼ばれていないことを `grep -n "exchange(" crates/mat/src/matd_client.rs` で確認してから削除。）

(e) `dispatch`（現 97–113 行）:

```rust
/// `--matd` 指定時のディスパッチ。非対応サブコマンドは CLI 利用の誤り（exit 2）。
pub fn dispatch(sockets: &[PathBuf], command: &Command) -> ExitCode {
    let op = match to_op(command) {
        Ok(op) => op,
        Err(detail) => {
            emit_error(ErrorKind::Other, &detail);
            return ExitCode::from(2);
        }
    };

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            emit_error(ErrorKind::Other, &detail);
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(socket = %socket.display(), "using matd (forced)");

    match exchange_on_stream(stream, &op) {
        Ok(resp) => emit_response(resp),
        Err(detail) => {
            emit_error(ErrorKind::Other, &detail);
            ExitCode::FAILURE
        }
    }
}
```

(f) `dispatch_auto`（現 121–145 行）:

```rust
pub fn dispatch_auto(sockets: &[PathBuf], command: &Command) -> Option<ExitCode> {
    // matd 非対応 op（discover / commission / open-window / diag）は probe せず直経路。
    let op = to_op(command).ok()?;

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            tracing::info!(
                error = %detail,
                "matd not reachable, falling back to direct native backend"
            );
            return None;
        }
    };
    tracing::info!(socket = %socket.display(), "using matd (auto-detected)");

    Some(match exchange_on_stream(stream, &op) {
        Ok(resp) => emit_response(resp),
        Err(detail) => {
            emit_error(ErrorKind::Other, &detail);
            ExitCode::FAILURE
        }
    })
}
```

（doc コメント「connect した stream をそのまま本リクエストに使う…」は現行のまま維持。）

(g) `dispatch_listen`（現 432–472 行）: 署名を `pub fn dispatch_listen(sockets: &[PathBuf], command: &Command) -> ExitCode` にし、接続部（現 451–463 行）を:

```rust
    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            emit_error(
                ErrorKind::MatdUnavailable,
                &format!("{detail}; `mat listen` requires a running matd"),
            );
            return ExitCode::from(ErrorKind::MatdUnavailable.exit_code());
        }
    };
    tracing::info!(socket = %socket.display(), "listening via matd");
```

（後続の `run_listen_stream(stream, ...)` 呼び出しは不変。）

(h) `crates/mat/src/main.rs` の受け側（現 72–108 行）— 変数名を複数形へ:

```rust
    if let Command::Listen { .. } = &command {
        return match matd_client::resolve_route(
            &args.matd,
            std::env::var_os("MAT_MATD_SOCKET"),
            std::env::var_os("MAT_MATD"),
        ) {
            matd_client::Route::Forced(sockets) | matd_client::Route::Auto(sockets) => {
                matd_client::dispatch_listen(&sockets, &command)
            }
            matd_client::Route::Direct => {
                mat_core::error::MatError::new(
                    ErrorKind::MatdUnavailable,
                    "`mat listen` requires matd (MAT_MATD=0 disables it)",
                )
                .emit();
                ExitCode::from(ErrorKind::MatdUnavailable.exit_code())
            }
        };
    }
```

と（現 96–108 行）:

```rust
    match matd_client::resolve_route(
        &args.matd,
        std::env::var_os("MAT_MATD_SOCKET"),
        std::env::var_os("MAT_MATD"),
    ) {
        matd_client::Route::Forced(sockets) => {
            return matd_client::dispatch(&sockets, &command)
        }
        matd_client::Route::Auto(sockets) => {
            if let Some(code) = matd_client::dispatch_auto(&sockets, &command) {
                return code;
            }
        }
        matd_client::Route::Direct => {}
    }
```

（周辺の説明コメントは現行のまま。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat`
Expected: lib テスト（resolve_route / connect_candidates 系）+ 統合テスト（`tests/matd_auto.rs` / `tests/listen.rs` は `MAT_MATD_SOCKET` で 1 本固定なので無改修で PASS）

- [ ] **Step 5: コミット**

```bash
task check
git add crates/mat/src/matd_client.rs crates/mat/src/main.rs crates/mat/Cargo.toml Cargo.lock
git commit -m "feat(mat): matd 既定探索を候補リスト化（subdir 新既定 → flat 旧既定の connect 試行）"
```

---

### Task 4: ドキュメント + バージョン 0.27.0

**Files:**
- Modify: `README.md:610-613`（matd 起動例のコメント）、`README.md:655-662`（Route selection / precedence 箇条書き）
- Modify: `crates/mat/src/cli.rs:24-32`（`--matd` ヘルプ文）
- Modify: `Cargo.toml:6`（workspace version）、`Cargo.lock`

**Interfaces:**
- Consumes: Task 1–3 の確定挙動（記述のみ）
- Produces: なし

- [ ] **Step 1: `--matd` ヘルプ文を更新**

`crates/mat/src/cli.rs:24-32` の doc コメントを:

```rust
    /// matd の unix socket 経由での実行を強制する（接続失敗はエラー、フォールバック無し）。
    /// 値を省略すると socket は `MAT_MATD_SOCKET` があればそれ（1 本）、無ければ既定候補
    /// （`$XDG_RUNTIME_DIR/matd/matd.sock` → 旧 `$XDG_RUNTIME_DIR/matd.sock` の順に
    /// connect 試行。XDG 不在なら `/tmp/matd.sock`）。
    /// 本フラグが無くても mat は既定で matd を**自動発見**する: 上記候補へ接続を
    /// 試み、matd がいればそちら、いなければ mat 自身の native 直経路で実行。
    /// `MAT_MATD=1` は本フラグ相当（強制）、`MAT_MATD=0` は自動発見の無効化（常に直経路）。
    /// `MAT_MATD_SOCKET` は socket パスの指定のみで経路は変えない。
    /// matd 対応は read/write/invoke/on/off/color-temp/color/level/describe/group のみ
    /// （discover/commission/open-window/diag/fabric は常に直経路; fabric 以外は本フラグ明示時は exit 2）。
```

- [ ] **Step 2: README を更新**

`README.md:611-613` のコメントを:

```bash
# Start the resident daemon (separate binary; see ARCHITECTURE.md / matd --help).
# With no --socket it binds the default path ($XDG_RUNTIME_DIR/matd/matd.sock,
# dir auto-created 0700; /tmp/matd.sock without XDG_RUNTIME_DIR) — the first
# default mat probes below.
matd &
```

`README.md:661-662` の precedence 箇条書きを:

```markdown
- Socket path precedence (all modes): `--matd <path>` > `MAT_MATD_SOCKET=<path>`
  (a single socket in both cases) > default candidates, probed in order:
  `$XDG_RUNTIME_DIR/matd/matd.sock` (the systemd `RuntimeDirectory=matd`
  convention, matd's own bind default) then the pre-0.27.0
  `$XDG_RUNTIME_DIR/matd.sock` (transition compat); just `/tmp/matd.sock`
  without `XDG_RUNTIME_DIR`. Stale sockets fail the connect and fall through
  naturally.
```

- [ ] **Step 3: バージョンを 0.27.0 へ**

`Cargo.toml:6` の `version = "0.26.0"` → `version = "0.27.0"`、続けて `cargo check --workspace` で `Cargo.lock` を追従させる。

- [ ] **Step 4: 全体検証**

Run: `task check`
Expected: fmt:check / clippy (-D warnings) / test 全 PASS

Run: `target/debug/mat --help | head -30`（または `cargo run -p mat -- --help`）
Expected: `--matd` ヘルプに新既定候補の記述

- [ ] **Step 5: コミット**

```bash
git add README.md crates/mat/src/cli.rs Cargo.toml Cargo.lock
git commit -m "docs+chore(release): matd socket 既定の subdir 化を文書化、0.27.0"
```

---

## 実機デプロイ（コード外・別セッション可）

プラン本体の完了条件には含めないが、忘れないよう記録:

- jarvis へ 0.27.0 デプロイ（`task dist:arm64` → scp → install → `systemctl --user restart matd`、[[jarvis-matd-deploy]] の手順）。unit は `--socket %t/matd/matd.sock` 明示なので挙動不変 — デプロイ後、素の ssh から `mat listen`（`MAT_MATD_SOCKET` 前置きなし）が exit 13 でなくなることを実機確認。
- 確認後、メモリ `jarvis-matd-deploy` の「ssh から listen は MAT_MATD_SOCKET 必須」を更新。
- （任意・別作業）jarvis-iac の matd unit から `--socket` フラグを落とす。
