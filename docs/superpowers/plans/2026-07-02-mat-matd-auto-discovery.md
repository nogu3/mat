# mat の matd 自動発見 実装プラン

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat` が既定で matd を自動発見する — 既定ソケットへ connect を試み、matd がいれば warm セッション経由、いなければ従来どおり直 chip-tool にフォールバックする。

**Architecture:** `matd_client::resolve_socket`（Option 返し）を 3 状態 enum `Route`（`Forced` / `Auto` / `Direct`）を返す `resolve_route` に置き換え、`main.rs` で match する。自動モードは connect 成功時にその stream をそのまま本リクエストに使い（再接続しない = probe と送信の間の隙間ゼロ）、connect 失敗時のみ直経路へフォールスルーする。接続後のエラーは matd 経路のエラーとしてそのまま返す（二重実行防止）。matd 側の変更は無し。

**Tech Stack:** Rust（同期 std のみ: `std::os::unix::net::UnixStream`）、clap(derive)、tracing、テストは assert_cmd + predicates + tempfile（既存踏襲）。

**Spec:** `docs/superpowers/specs/2026-07-02-mat-matd-auto-discovery-design.md`

## Global Constraints

- コミット前に必ず `task check`（fmt:check + clippy `-D warnings` + test）を通す。
- stdout は純粋な構造化 JSON のみ。診断は stderr の `tracing`（経路ログは info レベル）。
- テストは実 chip-tool / 実 matd 不要で CI で回ること（fake-chip-tool.sh + テスト内 UnixListener）。
- リポジトリは public。サンプル値はダミーのみ（IP は RFC 5737 `192.0.2.0/24`）。
- コミットメッセージは日本語 Conventional Commits（既存ログ踏襲、`Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` を付ける）。
- `MAT_MATD` の解釈: truthy = `1`/`true`/`yes`/`on`、falsy = `0`/`false`/`no`/`off`（大小無視）。どちらでもない値は未設定と同じ（自動）。

---

### Task 1: 既存統合テストを直経路に固定（ヘルメティック化）

自動検出が既定になると、開発機で実 matd が動いている場合に既存テストがそれを
拾ってしまう。先回りして既存テストの `mat()` ヘルパーを `MAT_MATD=0`（強制直）で
固定する。現時点で `MAT_MATD=0` は「truthy でない」= 従来どおり直経路なので、
この変更単体では挙動不変（安全な先行コミット）。

**Files:**
- Modify: `crates/mat/tests/integration.rs`（`fn mat()` ヘルパーのみ）

**Interfaces:**
- Produces: 既存統合テスト全部が MAT_MATD の環境値・実 matd の有無に依存しなくなる。

- [ ] **Step 1: `mat()` ヘルパーに `MAT_MATD=0` を追加**

`crates/mat/tests/integration.rs` の `fn mat()` を以下に変更:

```rust
/// fake chip-tool を使う `mat` コマンド。store は与えられた dir。
/// MAT_MATD=0 で直経路に固定する（matd 自動検出が既定のため、開発機で実 matd が
/// 動いていても拾わない）。
fn mat(store: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_CHIP_TOOL_BIN", fake_chip_tool())
        .env("MAT_MATD", "0")
        .arg("--store")
        .arg(store);
    c
}
```

- [ ] **Step 2: テストが全部通ることを確認**

Run: `cargo test -p mat`
Expected: 全 PASS（挙動不変の確認）

- [ ] **Step 3: コミット**

```bash
git add crates/mat/tests/integration.rs
git commit -m "test(mat): 統合テストを MAT_MATD=0 で直経路に固定（自動検出導入に備える）"
```

---

### Task 2: 経路解決 `Route` と自動検出ディスパッチの実装

本体。`resolve_socket` → `resolve_route`（3 状態）、`exchange` の接続/送受信分離、
自動モード用 `dispatch_auto`、`main.rs` の配線。TDD: 先に統合テスト（新ファイル）と
単体テストを書いて落とし、実装で通す。

**Files:**
- Create: `crates/mat/tests/matd_auto.rs`
- Modify: `crates/mat/src/matd_client.rs`
- Modify: `crates/mat/src/main.rs:27-36`

**Interfaces:**
- Consumes: `mat_core::socket::default_socket_path()`（既存、変更なし）、
  `crates/mat/tests/fixtures/fake-chip-tool.sh`（既存、変更なし）。
- Produces:
  - `pub enum Route { Forced(PathBuf), Auto(PathBuf), Direct }`（`Debug, PartialEq, Eq` derive）
  - `pub fn resolve_route(flag: &Option<Option<PathBuf>>, env_socket: Option<OsString>, env_enable: Option<OsString>) -> Route`
  - `pub fn dispatch_auto(socket: &Path, command: &Command) -> Option<ExitCode>`
    （`None` = 直経路で実行すべき: 非対応 op か connect 失敗。`Some(code)` = matd 経路で完結）
  - `pub fn dispatch(socket: &Path, command: &Command) -> ExitCode`（既存、変更なし = 強制 matd 用）
  - `resolve_socket` は削除（呼び出し元は main.rs のみ）。

- [ ] **Step 1: 統合テストを新ファイルに書く（失敗するテスト）**

`crates/mat/tests/matd_auto.rs` を新規作成:

```rust
//! matd 自動発見の統合テスト。fake matd（tmp の UnixListener）と fake chip-tool で
//! 経路選択（自動 / MAT_MATD=0 / stale socket / 非対応 op）を検証する。実 matd 不要。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn fake_chip_tool() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-chip-tool.sh")
}

/// 自動検出モード（MAT_MATD 未設定）の mat。probe 先は MAT_MATD_SOCKET で tmp に
/// 固定し、開発機で実 matd が動いていても拾わないようにする。
fn mat_auto(store: &Path, socket: &Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_CHIP_TOOL_BIN", fake_chip_tool())
        .env("MAT_MATD_SOCKET", socket)
        .env_remove("MAT_MATD")
        .arg("--store")
        .arg(store);
    c
}

/// 直経路（MAT_MATD=0）の mat。ストア準備用。
fn mat_direct(store: &Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_CHIP_TOOL_BIN", fake_chip_tool())
        .env("MAT_MATD", "0")
        .arg("--store")
        .arg(store);
    c
}

/// fake chip-tool 直経路で node 5 を commission 済みにしたストア。
fn store_with_node5() -> TempDir {
    let store = TempDir::new().unwrap();
    mat_direct(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
            "--node",
            "5",
        ])
        .assert()
        .success();
    store
}

/// fake matd: 1 接続を受け、1 行読んでマーカー入り応答を 1 行返して終了する。
/// join の戻り値は受信したリクエスト行（op の検証用）。
fn spawn_fake_matd(socket: PathBuf) -> JoinHandle<String> {
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut req = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut req)
            .unwrap();
        stream
            .write_all(b"{\"via\":\"fake-matd\",\"value\":true}\n")
            .unwrap();
        req
    })
}

#[test]
fn auto_routes_to_live_matd() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone());

    mat_auto(store.path(), &socket)
        .args(["read", "--node", "5", "--cluster", "onoff", "--attribute", "on-off"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // fake matd に read op が届いている（= matd 経路で実行された）。
    let req = matd.join().unwrap();
    assert!(req.contains("\"op\":\"read\""), "request line: {req}");
}

#[test]
fn auto_falls_back_when_socket_missing() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock"); // bind しない = 存在しないパス

    mat_auto(store.path(), &socket)
        .args(["read", "--node", "5", "--cluster", "onoff", "--attribute", "on-off"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""))
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn auto_falls_back_on_stale_socket() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // bind 後すぐ drop: ファイルは残るが誰も listen していない（ECONNREFUSED）。
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());

    mat_auto(store.path(), &socket)
        .args(["read", "--node", "5", "--cluster", "onoff", "--attribute", "on-off"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""));
}

#[test]
fn mat_matd_zero_forces_direct_even_with_live_matd() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let _listener = UnixListener::bind(&socket).unwrap(); // 生きているが使われないはず

    mat_auto(store.path(), &socket)
        .env("MAT_MATD", "0") // env_remove の後に上書き
        .args(["read", "--node", "5", "--cluster", "onoff", "--attribute", "on-off"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""))
        .stdout(predicate::str::contains("fake-matd").not());
}

#[test]
fn auto_keeps_unsupported_ops_on_direct_path() {
    let store = TempDir::new().unwrap(); // discover は空ストアで動く
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // 生きた listener。自動モードでも discover は probe されず直経路のはず
    // （probe されると accept されないまま exit 2 側の経路に落ちてテストが失敗する）。
    let _listener = UnixListener::bind(&socket).unwrap();

    mat_auto(store.path(), &socket)
        .arg("discover")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"devices\""));
}
```

- [ ] **Step 2: 統合テストが落ちることを確認**

Run: `cargo test -p mat --test matd_auto`
Expected: `auto_routes_to_live_matd` が FAIL（自動検出が無いので直経路の応答になり
`"via":"fake-matd"` を含まない）。フォールバック系 4 本は現行挙動でも PASS しうる
（未設定 = 直経路のため）— それで正しい。

- [ ] **Step 3: `matd_client.rs` に `Route` / `resolve_route` / `dispatch_auto` を実装**

`crates/mat/src/matd_client.rs` の変更:

(a) モジュール冒頭コメント（1〜2 行目）を差し替え:

```rust
//! mat → matd クライアント経路。
//!
//! 経路は 3 状態: `--matd` / `MAT_MATD=truthy` で**強制 matd**（接続失敗はエラー、
//! フォールバック無し）、`MAT_MATD=falsy` で**強制直 chip-tool**、どちらも無ければ
//! **自動検出**（既定ソケットへ connect を試み、matd がいればそちら、いなければ
//! 直経路にフォールバック）。`MAT_MATD_SOCKET` は「どのソケットか」の指定のみで
//! 経路は変えない。
```

（3 行目以下の「matd は unix socket 上で…」の段落は現状維持）

(b) `resolve_socket` とその doc コメントを削除し、以下に置き換え:

```rust
/// mat の実行経路。`resolve_route` が決める。
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// 明示有効化（`--matd` / `MAT_MATD=truthy`）: matd 固定。接続失敗はエラー、
    /// 非対応 op は exit 2。フォールバックしない。
    Forced(PathBuf),
    /// 既定（どちらも未設定）: socket へ connect を試み、成功なら matd、
    /// 失敗なら直 chip-tool にフォールバック。
    Auto(PathBuf),
    /// 明示無効化（`MAT_MATD=falsy`）: 常に直 chip-tool。probe もしない。
    Direct,
}

/// 経路と socket パスを決める（純粋関数; env は注入）。
///
/// - `--matd [<path>]` or `MAT_MATD=truthy` → `Forced`
/// - `MAT_MATD=falsy`（`0`/`false`/`no`/`off`） → `Direct`
/// - どちらも無し（truthy/falsy どちらでもない値も同じ） → `Auto`
///
/// socket パスの優先順: `--matd <path>`（明示）> `MAT_MATD_SOCKET=<path>`（非空）>
/// 既定パス。`MAT_MATD_SOCKET` はパス指定のみで経路は変えない。
pub fn resolve_route(
    flag: &Option<Option<PathBuf>>,
    env_socket: Option<OsString>,
    env_enable: Option<OsString>,
) -> Route {
    match flag {
        // --matd <path> → 明示パスで強制 matd。
        Some(Some(path)) => Route::Forced(path.clone()),
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET > 既定。
        Some(None) => Route::Forced(socket_from_env_or_default(env_socket)),
        None => match env_enable.as_deref() {
            Some(v) if is_truthy(v) => Route::Forced(socket_from_env_or_default(env_socket)),
            Some(v) if is_falsy(v) => Route::Direct,
            // 未設定（or 解釈不能な値）→ 自動検出。
            _ => Route::Auto(socket_from_env_or_default(env_socket)),
        },
    }
}
```

(c) `is_truthy` の直後に `is_falsy` を追加:

```rust
/// `MAT_MATD` の否定判定。`0` / `false` / `no` / `off`（大小無視）を無効化とみなす。
/// truthy とも falsy とも解釈できない値は「未設定」と同じ（自動検出）。
fn is_falsy(v: &OsStr) -> bool {
    matches!(
        v.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}
```

(d) `dispatch` の直後に `dispatch_auto` を追加:

```rust
/// 自動検出モードのディスパッチ。matd 経路で完結した場合のみ `Some(exit code)`。
/// `None` = 呼び出し側が直 chip-tool 経路で実行すべき（matd 非対応 op / connect 失敗）。
///
/// connect した stream をそのまま本リクエストに使う（probe 後の再接続はしない）ので、
/// フォールバックが起きるのは 1 バイトも送る前だけ。接続後のエラーは matd 経路の
/// エラーとしてそのまま返し、直経路で再実行しない（write / invoke の二重実行防止）。
pub fn dispatch_auto(socket: &Path, command: &Command) -> Option<ExitCode> {
    // matd 非対応 op（discover / commission / open-window / diag）は probe せず直経路。
    let op = to_op(command).ok()?;

    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(
                socket = %socket.display(),
                error = %e,
                "matd not reachable, falling back to direct chip-tool"
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

(e) `exchange` を接続と送受信に分離（`dispatch` からの呼び出しは変更不要）:

```rust
/// matd へ接続して 1 行送り 1 行受け取る。接続/送受信の失敗は detail 文字列で返す。
fn exchange(socket: &Path, op: &Value) -> Result<Value, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("could not connect to matd at {}: {e}", socket.display()))?;
    exchange_on_stream(stream, op)
}

/// 接続済み stream で 1 行送り 1 行受け取る（自動検出は probe した接続を使い回す）。
fn exchange_on_stream(mut stream: UnixStream, op: &Value) -> Result<Value, String> {
    let mut line = serde_json::to_vec(op).map_err(|e| format!("failed to encode request: {e}"))?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|e| format!("failed to send request to matd: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    let n = reader
        .read_line(&mut resp)
        .map_err(|e| format!("failed to read response from matd: {e}"))?;
    if n == 0 {
        return Err("matd closed the connection without responding".to_string());
    }
    serde_json::from_str(&resp).map_err(|e| format!("matd response was not JSON: {e}; body={resp}"))
}
```

(f) 単体テスト: `resolve_socket_precedence` テストを削除し、以下に置き換え:

```rust
    #[test]
    fn resolve_route_three_states() {
        let some_path = PathBuf::from("/x/y.sock");
        let dflt = default_socket_path();

        // --matd <path> → 強制 matd（明示パスが MAT_MATD_SOCKET より優先）。
        assert_eq!(
            resolve_route(
                &Some(Some(some_path.clone())),
                Some("/env.sock".into()),
                None
            ),
            Route::Forced(some_path)
        );
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET > 既定。
        assert_eq!(
            resolve_route(&Some(None), None, None),
            Route::Forced(dflt.clone())
        );
        assert_eq!(
            resolve_route(&Some(None), Some("/env.sock".into()), None),
            Route::Forced(PathBuf::from("/env.sock"))
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
        // 未設定 → 自動。probe 先は MAT_MATD_SOCKET（非空）> 既定。
        assert_eq!(resolve_route(&None, None, None), Route::Auto(dflt.clone()));
        assert_eq!(
            resolve_route(&None, Some("/env.sock".into()), None),
            Route::Auto(PathBuf::from("/env.sock"))
        );
        // truthy でも falsy でもない値 → 未設定と同じ（自動）。
        assert_eq!(resolve_route(&None, None, Some("abc".into())), Route::Auto(dflt));
    }
```

- [ ] **Step 4: `main.rs` の配線を差し替え**

`crates/mat/src/main.rs` の 27〜36 行目（コメント + `if let Some(socket) = ...` ブロック）を
以下に置き換え:

```rust
    // 経路解決（matd_client::resolve_route）: --matd / MAT_MATD=truthy は強制 matd、
    // MAT_MATD=falsy は強制直、どちらも無ければ自動検出（connect 成功時のみ matd 経由、
    // 失敗時と非対応 op は下の直 chip-tool 経路へフォールスルー）。store の locate は
    // 不要（node 解決は matd 側が KVS で行う）。
    match matd_client::resolve_route(
        &args.matd,
        std::env::var_os("MAT_MATD_SOCKET"),
        std::env::var_os("MAT_MATD"),
    ) {
        matd_client::Route::Forced(socket) => {
            return matd_client::dispatch(&socket, &args.command)
        }
        matd_client::Route::Auto(socket) => {
            if let Some(code) = matd_client::dispatch_auto(&socket, &args.command) {
                return code;
            }
        }
        matd_client::Route::Direct => {}
    }
```

- [ ] **Step 5: 全テストが通ることを確認**

Run: `cargo test -p mat`
Expected: 全 PASS（`matd_auto` の 5 本、`matd_client` 単体、既存 `integration.rs` 全部）

- [ ] **Step 6: `task check`**

Run: `task check`
Expected: fmt:check / clippy（`-D warnings`）/ 全 crate テスト PASS

- [ ] **Step 7: コミット**

```bash
git add crates/mat/src/matd_client.rs crates/mat/src/main.rs crates/mat/tests/matd_auto.rs
git commit -m "feat(mat): matd を既定で自動発見（connect probe、失敗時は直 chip-tool）"
```

---

### Task 3: ドキュメント同期（CLI ヘルプ / README）

**Files:**
- Modify: `crates/mat/src/cli.rs:22-30`（`--matd` の doc コメント）
- Modify: `README.md`（"Routing through `matd`" 節: 367-412 行目付近）

**Interfaces:**
- Consumes: Task 2 の 3 状態セマンティクス（強制 matd / 強制直 / 自動）。

- [ ] **Step 1: `cli.rs` の `--matd` ヘルプを 3 状態の説明に更新**

`crates/mat/src/cli.rs` の `pub matd` フィールドの doc コメント（22〜28 行目）を差し替え:

```rust
    /// matd の unix socket 経由での実行を強制する（接続失敗はエラー、フォールバック無し）。
    /// 値を省略すると socket は `MAT_MATD_SOCKET` があればそれ、無ければ既定パス
    /// （`$XDG_RUNTIME_DIR/matd.sock`、無ければ `/tmp/matd.sock`）。
    /// 本フラグが無くても mat は既定で matd を**自動発見**する: 上記の socket へ接続を
    /// 試み、matd がいればそちら、いなければ直 chip-tool にフォールバック。
    /// `MAT_MATD=1` は本フラグ相当（強制）、`MAT_MATD=0` は自動発見の無効化（常に直経路）。
    /// `MAT_MATD_SOCKET` は socket パスの指定のみで経路は変えない。
    /// matd 対応は read/write/invoke/on/off/describe/group のみ
    /// （discover/commission/open-window/diag は常に直経路; 本フラグ明示時は exit 2）。
```

- [ ] **Step 2: README の "Routing through `matd`" 節を更新**

`README.md` 367〜370 行目の導入段落を差し替え:

```markdown
By default each `mat` call spawns `chip-tool` and pays a fresh CASE handshake.
With a running `matd` the call is routed through its warm session instead —
same subcommands, same JSON on stdout, but the handshake is skipped on repeated
calls. `mat` **auto-detects** `matd`: for supported subcommands it tries a
connect on the default socket, uses `matd` when something answers, and silently
falls back to the direct chip-tool path when nothing does (missing and stale
sockets alike).
```

372〜389 行目のコードブロックを差し替え:

```bash
# Start the resident daemon (separate binary; see ARCHITECTURE.md / matd --help).
# With no --socket it uses the default path ($XDG_RUNTIME_DIR/matd.sock, else
# /tmp/matd.sock) — the same default mat probes below.
matd &

# No flag needed: mat finds the running matd on the default socket by itself.
mat read --node 5 --cluster onoff --attribute on-off
mat describe --node 5
mat group invoke --group 1 --cluster onoff --command on

# Force the matd path (connection failure becomes an error instead of a
# fallback); pass a path to use a non-default socket.
mat --matd read --node 5 --cluster onoff --attribute on-off
mat --matd /run/mat/matd.sock on --node 5
export MAT_MATD=1                       # same, for a whole shell session

# Opt out (always direct chip-tool, no probing):
MAT_MATD=0 mat read --node 5 --cluster onoff --attribute on-off
# export MAT_MATD_SOCKET=/run/mat/matd.sock   # pins which socket to probe/use
```

405〜412 行目の箇条書き（"Routing is **enabled** only by..." と "Supported over
matd..." の 2 項目）を差し替え（"Socket path precedence" と "node_id
commissioning is re-checked..." の項目は現状維持）:

```markdown
- Route selection: `--matd` / `MAT_MATD=<truthy>` **force** the matd path
  (connection failure is an error, no fallback). `MAT_MATD=<falsy>`
  (`0`/`false`/`no`/`off`) forces the direct path, no probing. Otherwise
  (default) `mat` **auto-detects**: it probes the socket with a connect and
  falls back to the direct path when nobody answers. `MAT_MATD_SOCKET` just
  selects *which* socket in every mode.
- Once connected, errors are reported from the matd path as-is — `mat` never
  re-runs the command on the direct path (no double execution of writes).
  Which path ran is logged to stderr at info level (`MAT_LOG=info`).
- Supported over matd: `read` / `write` / `invoke` / `on` / `off` / `describe` /
  `group`. `discover` / `commission` / `open-window` / `diag` are direct-only:
  auto-detection skips them silently; explicit `--matd` exits `2`.
```

- [ ] **Step 3: `task check`**

Run: `task check`
Expected: 全 PASS（cli.rs は doc コメントのみだが fmt / clippy を通す）

- [ ] **Step 4: コミット**

```bash
git add crates/mat/src/cli.rs README.md
git commit -m "docs: matd 自動発見の 3 状態（強制/自動/無効）を CLI ヘルプと README に反映"
```
