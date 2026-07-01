# matd stop コマンド + 二重起動ガード Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `matd` に `matd stop`（socket 経由の graceful shutdown）と flock による単一インスタンス保証を追加する。

**Architecture:** 停止は既存 unix socket に新 `Op::Shutdown` を送り、serve ループを break させて既存の graceful shutdown 経路（chip-tool 子 kill + socket 削除）を再利用する。二重起動は起動時に `<socket>.lock` へ排他 flock を取得し、chip-tool 起動より前に失敗させる。

**Tech Stack:** Rust, tokio, clap(derive), rustix(flock), serde_json。テストは既存の fake ws ハーネス + assert_cmd。

## Global Constraints

- stdout は純粋な構造化 JSON のみ（人間装飾禁止）。診断は stderr の `tracing`。
- エラーは `{"error":{"kind":..,"detail":..}}`。二重起動 / stop 空振りは `kind: "other"` exit 1。
- 認証情報・実 IP・実 node_id をコミットしない（public repo）。
- `flock` は `rustix`（`fs` feature、既に依存ツリーに v1.1.4 あり）で呼び `unsafe` を書かない。
- 変更コミットは本セッションで編集したファイルのみ `git add`。
- 各タスク後に `task check`（fmt:check + clippy -D warnings + test）が通ること。

---

### Task 1: `Op::Shutdown` を protocol に追加

**Files:**
- Modify: `crates/matd/src/protocol.rs`（`Op` enum、`node_id()`、`to_cmdline()`、tests）

**Interfaces:**
- Produces: `Op::Shutdown`（フィールド無しの unit variant）。`node_id()` → `None`、`to_cmdline()` → `None`。JSON タグは `{"op":"shutdown"}`。

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/protocol.rs` の `mod tests` に追加:

```rust
    #[test]
    fn shutdown_has_no_node_or_cmdline() {
        // admin op。chip-tool には触れないので node_id も cmdline も持たない。
        let r = parse(r#"{"op":"shutdown"}"#);
        assert!(matches!(r.op, Op::Shutdown));
        assert_eq!(r.op.node_id(), None);
        assert!(r.op.to_cmdline().is_none());
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd --lib protocol::tests::shutdown_has_no_node_or_cmdline`
Expected: コンパイルエラー（`Op::Shutdown` 未定義）

- [ ] **Step 3: 最小実装**

`Op` enum の末尾（`Ping` の後）に追加:

```rust
    /// デーモンを停止する admin op（chip-tool には触れない）。`matd stop` が送る。
    /// Ping と同じく node も cmdline も持たない。
    Shutdown,
```

`node_id()` の `None` を返す arm に `Op::Shutdown` を追加:

```rust
            Op::GroupProvision { .. } | Op::GroupInvoke { .. } | Op::Ping | Op::Shutdown => None,
```

`to_cmdline()` の `return None` する arm に `Op::Shutdown` を追加:

```rust
            Op::Describe { .. }
            | Op::GroupProvision { .. }
            | Op::GroupInvoke { .. }
            | Op::Ping
            | Op::Shutdown => return None,
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --lib protocol::tests::shutdown_has_no_node_or_cmdline`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/protocol.rs
git commit -m "feat(matd): protocol に Shutdown op を追加"
```

---

### Task 2: 単一インスタンスロック（`lock.rs`）

**Files:**
- Create: `crates/matd/src/lock.rs`
- Modify: `crates/matd/src/lib.rs`（`pub mod lock;`）
- Modify: `crates/matd/Cargo.toml`（`rustix` 依存追加）

**Interfaces:**
- Produces:
  - `pub fn lock_path(socket_path: &Path) -> PathBuf`（socket_path + `.lock`）
  - `pub fn acquire(socket_path: &Path) -> Result<std::fs::File, MatError>`
    （排他 flock 取得。競合時 `ErrorKind::Other` + detail に `"already running"`。返す `File` を保持する限りロック継続、Drop で解放）

- [ ] **Step 1: `rustix` 依存を追加**

`crates/matd/Cargo.toml` の `[dependencies]` 末尾（`futures-util = ...` の後）に追加:

```toml
rustix = { version = "1", features = ["fs"] }
```

- [ ] **Step 2: `lock.rs` を作成（テスト付き）**

`crates/matd/src/lock.rs` を新規作成:

```rust
//! 単一インスタンスガード。`<socket>.lock` に排他 advisory ロック（flock）を取り、
//! 二重起動を防ぐ。ロックは open file description に紐づき、プロセス終了（kill/crash
//! 含む）で OS が自動解放するため stale 状態が残らない。取得した `File` を保持する
//! 限りロックは有効。

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use rustix::fs::{flock, FlockOperation};

use mat_core::error::{ErrorKind, MatError};

/// ロックファイルパス（socket パス + `.lock`）。
pub fn lock_path(socket_path: &Path) -> PathBuf {
    let mut p = socket_path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// 排他ロックを取得する。既に別 matd が保持していれば `Err`（`ErrorKind::Other`）。
/// 返す `File` はプロセス生存中保持すること（Drop でロック解放）。
pub fn acquire(socket_path: &Path) -> Result<File, MatError> {
    let path = lock_path(socket_path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("failed to open matd lock file {}: {e}", path.display()),
            )
        })?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        // ロック競合 = 別の matd が稼働中。
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Err(MatError::new(
            ErrorKind::Other,
            format!("matd already running (lock held at {})", path.display()),
        )),
        Err(e) => Err(MatError::new(
            ErrorKind::Other,
            format!("failed to lock matd lock file {}: {e}", path.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn second_acquire_fails_then_succeeds_after_release() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("matd.sock");

        let first = acquire(&sock).expect("first acquire should succeed");
        // 保持中は 2 度目が失敗（別 open file description なので同一プロセスでも競合）。
        let err = acquire(&sock).expect_err("second acquire must fail while held");
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(
            err.detail.contains("already running"),
            "detail should say already running, got: {}",
            err.detail
        );

        // 解放すれば再取得できる。
        drop(first);
        let _again = acquire(&sock).expect("acquire after release should succeed");
    }

    #[test]
    fn lock_path_appends_suffix() {
        assert_eq!(
            lock_path(Path::new("/run/mat/matd.sock")),
            PathBuf::from("/run/mat/matd.sock.lock")
        );
    }
}
```

- [ ] **Step 3: `lib.rs` に公開**

`crates/matd/src/lib.rs` の `pub mod server;` の後に追加:

```rust
pub mod lock;
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --lib lock::tests`
Expected: PASS（2 tests）。`rustix` がダウンロード/ビルドされる。

- [ ] **Step 5: コミット**

```bash
git add crates/matd/Cargo.toml Cargo.lock crates/matd/src/lock.rs crates/matd/src/lib.rs
git commit -m "feat(matd): flock による単一インスタンスロックを追加"
```

---

### Task 3: server に shutdown を配線 + ロックファイル掃除

**Files:**
- Modify: `crates/matd/src/server.rs`（`serve`、`handle_conn`、`dispatch`、`run_op`）
- Test: `crates/matd/tests/integration.rs`（新テスト追加）

**Interfaces:**
- Consumes: `Op::Shutdown`（Task 1）、`crate::lock::lock_path`（Task 2）。
- Produces: shutdown op に `{"stopping": true}` を返し、serve ループを終了させる。`serve` のシグネチャは不変（`serve(&Path, PathBuf, Arc<ChipToolBackend>) -> io::Result<()>`）。

- [ ] **Step 1: 失敗する統合テストを書く**

`crates/matd/tests/integration.rs` の末尾に追加:

```rust
/// `matd stop` 相当: shutdown op を送ると `{"stopping":true}` が返り、serve ループが
/// 自然終了する（abort ではなく JoinHandle が完了する）。
#[tokio::test]
async fn shutdown_op_stops_server() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(&socket, &[json!({"id":1,"op":"shutdown"})]).await;
    assert_eq!(resps[0]["stopping"], json!(true));
    assert_eq!(resps[0]["id"], json!(1));

    // serve ループが break して JoinHandle が完了する。
    let ended = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(ended.is_ok(), "serve did not shut down after shutdown op");
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd --test integration shutdown_op_stops_server`
Expected: FAIL（`stopping` が返らず、`handle` が完了しない → timeout で panic）

- [ ] **Step 3: server を実装**

`crates/matd/src/server.rs` の import に `Notify` を追加（`use std::sync::Arc;` の後あたり）:

```rust
use tokio::sync::Notify;
```

`serve` の reaper 定義の後、`let store_path = Arc::new(store_path);` の前に shutdown 通知を作る:

```rust
    // shutdown op（`matd stop`）で serve ループを抜けるための通知。
    let shutdown = Arc::new(Notify::new());
```

accept を spawn する箇所に `shutdown` のクローンを渡す。`tokio::select!` の accept arm を次に置き換え、`ctrl_c` arm の後に shutdown arm を足す:

```rust
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted?;
                let backend = Arc::clone(&backend);
                let store_path = Arc::clone(&store_path);
                let shutdown = Arc::clone(&shutdown);
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, backend, store_path, shutdown).await {
                        tracing::warn!(error = %e, "connection handler ended with error");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl-C, shutting down");
                break;
            }
            _ = shutdown.notified() => {
                tracing::info!("received shutdown op, shutting down");
                break;
            }
        }
```

graceful shutdown の後始末に lock ファイル削除を追加（`let _ = std::fs::remove_file(socket_path);` の後）:

```rust
    let _ = std::fs::remove_file(crate::lock::lock_path(socket_path));
```

`handle_conn` に `shutdown` 引数を足し、応答を flush してから shutdown を発火する:

```rust
async fn handle_conn(
    stream: UnixStream,
    backend: Arc<ChipToolBackend>,
    store_path: Arc<PathBuf>,
    shutdown: Arc<Notify>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let (response, is_shutdown) = dispatch(&line, &backend, &store_path).await;
        let mut buf = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
        buf.push(b'\n');
        write_half.write_all(&buf).await?;
        // 応答をワイヤに出し切ってから停止を発火する（クライアントが確実に受け取る）。
        write_half.flush().await?;
        if is_shutdown {
            shutdown.notify_one();
            break;
        }
    }
    Ok(())
}
```

`dispatch` を `(Value, bool)` を返すよう変更（bool = shutdown 要求か）:

```rust
async fn dispatch(line: &str, backend: &ChipToolBackend, store_path: &Path) -> (Value, bool) {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return (
                error_response(
                    None,
                    &MatError::parse_error(format!("invalid request JSON: {e}")),
                ),
                false,
            )
        }
    };
    let id = req.id.clone();
    let is_shutdown = matches!(req.op, Op::Shutdown);

    let body = match run_op(&req.op, backend, store_path).await {
        Ok(mut body) => {
            if let Value::Object(map) = &mut body {
                if let Some(id) = id {
                    map.insert("id".into(), id);
                }
                map.entry("timestamp".to_string())
                    .or_insert_with(|| Value::String(now_iso8601()));
            }
            body
        }
        Err(e) => error_response(id, &e),
    };
    (body, is_shutdown)
}
```

`run_op` の `match op` に shutdown arm を追加（`Op::Ping =>` の後）:

```rust
        // Shutdown は chip-tool に触れず即応。serve ループの終了は handle_conn が発火する。
        Op::Shutdown => Ok(json!({ "stopping": true })),
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --test integration shutdown_op_stops_server`
Expected: PASS

- [ ] **Step 5: 既存テストの回帰確認**

Run: `cargo test -p matd`
Expected: 全 PASS（`read_invoke_ping_and_errors` 等の既存統合テストも緑）

- [ ] **Step 6: コミット**

```bash
git add crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): shutdown op で serve を停止し lock ファイルも掃除"
```

---

### Task 4: CLI に `stop` サブコマンド + ロック配線（`main.rs`）

**Files:**
- Modify: `crates/matd/src/main.rs`（`Cli`、`Command`、`run`、serve/stop 分岐）
- Modify: `crates/matd/Cargo.toml`（`[dev-dependencies]` に assert_cmd/predicates）
- Test: `crates/matd/tests/cli.rs`（新規）

**Interfaces:**
- Consumes: `matd::lock::acquire`（Task 2）、`Op::Shutdown` JSON（Task 1/3）、`mat_core::socket::default_socket_path`。
- Produces: `matd`（サブコマンド無し）= serve、`matd stop [--socket]` = 停止。

- [ ] **Step 1: 失敗する CLI テストを書く**

`crates/matd/tests/cli.rs` を新規作成:

```rust
//! matd の CLI 面のテスト（chip-tool 不要な経路のみ）。

use assert_cmd::Command;
use predicates::prelude::*;

/// stop 先の matd が居なければ「not running」エラーで exit 1。chip-tool は不要。
#[test]
fn stop_without_running_daemon_errors() {
    let sock = std::env::temp_dir().join(format!("matd-cli-nostop-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);

    Command::cargo_bin("matd")
        .unwrap()
        .args(["stop", "--socket", sock.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not running"));
}
```

- [ ] **Step 2: dev-dependencies を追加**

`crates/matd/Cargo.toml` の `[dev-dependencies]` を次に置き換え:

```toml
[dev-dependencies]
tempfile.workspace = true
assert_cmd.workspace = true
predicates.workspace = true
```

- [ ] **Step 3: テストが失敗することを確認**

Run: `cargo test -p matd --test cli`
Expected: FAIL（`stop` サブコマンド未実装 → clap がエラー、"not running" が出ない）

- [ ] **Step 4: `main.rs` を実装**

import を追加（`use std::path::PathBuf;` の付近）:

```rust
use std::path::Path;

use serde_json::Value;
```

`Cli` 構造体の `--socket` を global にし、末尾に subcommand を追加:

```rust
    /// 上流 unix socket のパス。未指定なら $XDG_RUNTIME_DIR/matd.sock（無ければ /tmp）。
    /// serve / stop 両方が使う。
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
```

`Cli` の末尾フィールドとして追加:

```rust
    #[command(subcommand)]
    command: Option<Command>,
```

`Cli` の後にサブコマンド定義を追加:

```rust
/// matd のサブコマンド。無指定は serve（従来どおり）。
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// 稼働中の matd を停止する（socket 経由で graceful shutdown）。
    Stop,
}
```

`run` を分岐に置き換え、既存の serve 本体を `serve_daemon` に切り出す:

```rust
async fn run(cli: Cli) -> Result<(), MatError> {
    match cli.command {
        Some(Command::Stop) => stop(cli.socket).await,
        None => serve_daemon(cli).await,
    }
}

/// serve: 単一インスタンスロックを取ってから chip-tool を起こし、socket を bind する。
async fn serve_daemon(cli: Cli) -> Result<(), MatError> {
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(mat_core::socket::default_socket_path);

    // 二重起動ガード。chip-tool 起動・socket bind より前に取る（rival chip-tool を
    // 起こさない）。_lock はプロセス生存中保持する（Drop でロック解放）。
    let _lock = matd::lock::acquire(&socket)?;

    let store_path = Store::locate(cli.store);
    // 認証情報必須レイヤ。ストアが無ければ早めに exit 10。
    Store::open(&store_path)?;

    let idle = std::time::Duration::from_secs(cli.idle_timeout);
    let backend = if cli.connect {
        ChipToolBackend::connect(cli.port, idle).await?
    } else {
        ChipToolBackend::spawn(&store_path, cli.port, idle).await?
    };

    server::serve(&socket, store_path, Arc::new(backend))
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("socket server failed: {e}")))
}

/// stop: 稼働中 matd の socket に shutdown op を送る。居なければ「not running」で exit 1。
async fn stop(socket: Option<PathBuf>) -> Result<(), MatError> {
    let socket = socket.unwrap_or_else(mat_core::socket::default_socket_path);
    let resp = send_shutdown(&socket).await?;
    // 成功応答は stdout（純粋 JSON）。
    println!("{resp}");
    Ok(())
}

/// socket に `{"op":"shutdown"}` を送り応答 1 行を読む。接続不能は「not running」。
async fn send_shutdown(socket: &Path) -> Result<Value, MatError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(socket).await.map_err(|e| {
        // 応答なしで拒否 = stale socket が残っているだけのことがある。掃除する。
        if e.kind() == std::io::ErrorKind::ConnectionRefused {
            let _ = std::fs::remove_file(socket);
        }
        MatError::new(
            ErrorKind::Other,
            format!("matd not running at {} ({e})", socket.display()),
        )
    })?;

    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(b"{\"op\":\"shutdown\"}\n")
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to send shutdown: {e}")))?;

    let mut lines = BufReader::new(read_half).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to read response: {e}")))?
        .ok_or_else(|| {
            MatError::new(
                ErrorKind::Other,
                "matd closed the connection without responding".to_string(),
            )
        })?;
    serde_json::from_str(&line)
        .map_err(|e| MatError::parse_error(format!("matd response was not JSON: {e}; body={line}")))
}
```

- [ ] **Step 5: CLI テストが通ることを確認**

Run: `cargo test -p matd --test cli`
Expected: PASS

- [ ] **Step 6: 全体テスト**

Run: `cargo test -p matd`
Expected: 全 PASS（protocol/lock unit + integration + cli）

- [ ] **Step 7: コミット**

```bash
git add crates/matd/src/main.rs crates/matd/Cargo.toml Cargo.lock crates/matd/tests/cli.rs
git commit -m "feat(matd): stop サブコマンドと二重起動ガードを配線"
```

---

### Task 5: README と メモリ更新

**Files:**
- Modify: `README.md`（matd セクション）
- Create: メモリ追記（`matd-port9100-orphan` の更新 or `matd stop` の新メモリ）

**Interfaces:** なし（ドキュメントのみ）。

- [ ] **Step 1: README の matd セクションを更新**

`README.md` の「Routing through `matd`」節（`matd &` を紹介している箇所、376 行付近）の直後に、停止と単一インスタンスの説明を追加する。挿入する内容:

```markdown
Stop the daemon with `matd stop` (do **not** `kill` it — that orphans the
child `chip-tool` holding the ws port). `matd stop` sends a shutdown request
over the same socket and triggers a graceful teardown (child `chip-tool`
killed, socket removed):

```bash
matd stop                      # default socket
matd stop --socket /run/mat/matd.sock
```

Only one `matd` per socket: startup takes an exclusive `flock` on
`<socket>.lock`, so a second launch exits 1 with
`matd already running (lock held at ...)` instead of hijacking the socket or
spawning a rival `chip-tool`.
```

（既存文書の言い回し・コードフェンスのネストに合わせて調整すること。行番号は変わり得るので `matd &` を含む段落を grep で特定して直後に置く。）

- [ ] **Step 2: README のビルドを壊していないか確認（リンク/整形の目視）**

Run: `grep -n "matd stop" README.md`
Expected: 追記した 2 箇所（コード例と本文）がヒットする

- [ ] **Step 3: メモリを更新**

`/home/noguk/.claude/projects/-home-noguk-ghq-github-com-nogu3-mat/memory/matd-port9100-orphan.md` の本文に「`matd stop` で graceful に止めれば子 chip-tool を孤児化させない（v0.7 以降）。`kill` は最終手段」を追記し、`How to apply` を更新する。`MEMORY.md` の該当行の hook も «`matd stop` 追加後» に合わせて微修正。

- [ ] **Step 4: コミット**

```bash
git add README.md
git commit -m "docs(readme): matd stop と単一インスタンス動作を説明"
```

（メモリファイルは repo 外なので `git add` 対象外。別途 Write で更新する。）

---

### 最終確認

- [ ] **`task check` で CI 相当を通す**

Run: `task check`
Expected: fmt:check / clippy(-D warnings) / test すべて緑

- [ ] **手動スモーク（chip-tool のある環境 / 実機で任意）**

```bash
matd &                                  # 起動
matd                                     # 2個目 → exit 1 "already running"
matd stop                                # {"stopping":true} が出て matd 終了
pgrep -af chip-tool                      # 子 chip-tool が残っていないこと
```

## Self-Review

- **Spec coverage:** CLI 形（Task 4）/ flock ガード（Task 2 + 配線 Task 4）/ shutdown op（Task 1 + 配線 Task 3）/ stop クライアント（Task 4）/ docs+tests（Task 3,4,5）— spec の全節にタスクが対応。stop 空振り時 exit 1（Task 4 の cli テスト）も網羅。
- **Placeholder scan:** TBD/TODO・曖昧指示なし。全コードステップに実コードあり。
- **Type consistency:** `Op::Shutdown`（Task 1）を Task 3 の `run_op`/`dispatch` が参照。`lock::acquire`/`lock::lock_path`（Task 2）を Task 3（lock_path）・Task 4（acquire）が参照。`dispatch` の戻り値 `(Value, bool)` は Task 3 の `handle_conn` と整合。`serve` シグネチャは不変で既存呼び出し（テストハーネス）と互換。
