# mat-device `net::runtime::run` 段階分割 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `crates/mat-device/src/net/runtime.rs` の 399 行 `run()`（709–1108 行）を、**挙動不変・機械的**に「起動 / 受信ループ / セッション確立 / IM ディスパッチ / 購読 / groupcast 受信 / タイマー」の段階ごとのメソッドへ分割し、`run()` 自体は 2 段呼び出しの薄い入口にする。

**Architecture:** `run()` のループ内ローカル変数（`pase_salt` / `window` / `mdns_ctx` / `mdns_retry` / `current_session` / `subscription` / `replay`）と引数（`transport` / `port` / `config` / `node` / `comm_server` / `group`）を、新設の private 構造体 `Runtime` に持たせる。`ServeState` が借用する 6 項目（`node` / `comm_server` / `mdns` / `subscription` / `window` / `config`）は `Runtime` 直下ではなくサブ構造体 `NodeState` にまとめ、`NodeState::serve_state(&mut self) -> ServeState<'_>` で従来の `ServeState` を組み立てる（`Runtime` のメソッド内で `self.current_session` と `self.state.serve_state()` が **別フィールドの分離借用** になり、borrow checker を通すための構造）。`tokio::select!` の各 branch 本体は `Runtime` の `on_*` メソッドへ移す。`ServeState` / `serve_secured` / `serve_secured_message` / 各 `*_deadline` 関数・`MdnsRetry` などの既存の自由関数・型は **一切変えない**（テストが直接使っている）。

**Tech Stack:** Rust 2021 workspace、tokio。検証は `cargo fmt` / `cargo clippy -D warnings` / `cargo test -p mat-device -p matv`、テスト名の多重集合比較、`task check`、実 NIC を使う `task e2e:device:m1` / `task e2e:device:m3`（実デバイスは使わない — matv バイナリ相手の E2E）。

**Spec:** ユーザー指示（2026-09-05 セッション）: 「`net/runtime.rs` の 395 行 `run()` を段階（受信ループ / セッション確立 / IM ディスパッチ / 購読 / groupcast 受信）に分割（挙動不変）。触る範囲は mat-device と matv のみ。並行セッションが mat-controller / mat-core / mat-native を触るので、そこは変更しない。検証は task check と task e2e:device:m1 / m3。実機は使わない」。監査バックログはメモリ `mat-code-audit-2026-08-31`。

## Global Constraints

- **挙動不変**: `select!` の branch の順序・各 branch 内の処理順序・ログ文言（`tracing::*` のメッセージとフィールド）・`continue` / `return` の意味を 1 つも変えない。移動する処理は本体をそのままコピーし、`window` → `self.state.window` のような **参照の付け替えだけ** を行う。ロジックの「ついで修正」は禁止。気づいた点はタスク報告に書くだけ。
- **編集範囲**: `crates/mat-device/src/net/runtime.rs` と `crates/mat-device/src/device.rs`（doc コメント 1 箇所、Task 5）のみ。`crates/mat-controller` / `crates/mat-core` / `crates/mat-native` / `crates/mat` / `crates/matd` は **絶対に編集しない**（並行セッションのレーン）。`crates/matv` も本計画では変更不要。
- **既存の型・関数は不変**: `ServeState` / `ServeOutcome` / `serve_secured` / `drain_buffered_requests` / `serve_secured_message` / `serve_read_request_chunked` / `serve_subscribe_request` / `send_subscription_report` / `sync_group_joins` / `mdns_retry_deadline` / `subscription_deadline` / `commissioning_window_deadline` / `fail_safe_expiry_deadline` / `bring_up_mdns` / `MdnsCtx` / `MdnsRetry` / `CommissioningWindow` / `classify_unsecured` / `admit_unsecured` / `random_session_id` / `pase_config_for_window` は **シグネチャも本体も変えない**（`tests` mod が `serve_secured(.., &mut ServeState { .. })` を直接呼んでいる）。
- **`run()` の公開シグネチャ不変**: `pub(crate) async fn run(transport: Arc<Transport>, local_addr: SocketAddr, config: DeviceConfig, node: Node, comm_server: CommissioningServer, group: GroupRx) -> Result<(), DeviceError>`（`device.rs:551` の呼び出しをそのまま通す）。
- **新設項目はすべて private**（`struct Runtime` / `struct NodeState` / `impl` のメソッドに `pub` を付けない）。mat-device の公開 API は変えない。
- **テスト不変**: mat-device + matv の `--all-targets` のテストは移動前後で **同じ名前の多重集合（375 個）**。テストを削除・改名・統合しない。新規テストの追加も **不要**（純粋な機械分割。分割で新しく生まれる `install_session` の性質は既存の `serve_secured_*` / E2E が覆う）。
- **各タスク終了時**: `cargo fmt --all` → `cargo clippy -p mat-device -p matv --all-targets -- -D warnings` → `cargo test -p mat-device -p matv` → テスト名比較、が全部通ること。最終タスクで `task check` と E2E。
- **コミット**: タスクごとに 1 つ。`git` は `/usr/bin/git` をフルパスで呼ぶ（rtk フックが `git`→`rtk git` に書き換えると worktree 隔離チェックに弾かれる）。`git add` は `crates/mat-device/src/net/runtime.rs`（Task 5 では `crates/mat-device/src/device.rs` と本計画ファイルも）だけ。メッセージ末尾に
  `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` と
  `Claude-Session: https://claude.ai/code/session_01DbRmGXELPVLTCHBym3RZaS` を付ける。
- **ワークツリー**: `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/matv-runtime-split`（ブランチ `worktree-matv-runtime-split`）。パスはすべてこの下の相対パス。元リポジトリ root には `cd` しない。`git stash` は使わない（stash スタックは他セッションと共有）。
- **前提コミット**: 本計画の前に、同 worktree で陳腐化コメント修正（`core/group_key_management.rs` / `net/runtime.rs` / `net/mdns.rs` のファイル名・行番号参照をモジュールパス参照に）が別コミットとして入っている。Task 1 は `git status` がクリーンな状態から始めること。

## File Structure

変更するのは実質 1 ファイル。`runtime.rs` 内の配置（すべて既存 `run()` の位置 = `fail_safe_expiry_deadline` の直後、`sync_group_joins` の直前）:

| 項目 | 責務 |
|---|---|
| `pub(crate) async fn run(..)` | 入口。`Runtime::boot(..).await.serve_forever().await` の 2 段呼び出しのみ。既存の doc コメントはそのまま残す |
| `struct NodeState` | `ServeState` が借用する 6 項目の **所有側**: `node: Node` / `comm_server: CommissioningServer` / `mdns: Option<MdnsCtx>` / `subscription: Option<ActiveSubscription>` / `window: CommissioningWindow` / `config: DeviceConfig`。`fn serve_state(&mut self) -> ServeState<'_>` |
| `struct Runtime` | ループ全体の状態: `transport: Arc<Transport>` / `port: u16` / `pase_salt: [u8; 16]` / `mdns_retry: Option<MdnsRetry>` / `current_session: Option<(u16, SecureSession, u8)>` / `replay: GroupReplayGuard` / `group: GroupRx` / `state: NodeState` |
| `Runtime::boot(..) -> Runtime` (async) | 起動段階: PASE salt 生成・commissioning window の起動時ポリシー・初回 `bring_up_mdns`（現 717–751 行） |
| `Runtime::serve_forever(&mut self) -> Result<(), DeviceError>` (async) | 受信ループ: `sync_group_joins` + `tokio::select!` 6 branch。各 branch は `self.on_*` を呼ぶだけ |
| `Runtime::on_unicast_datagram(&mut self, datagram: &[u8], peer: SocketAddr)` (async) | ユニキャスト受信 1 件の分類（`MessageHeader` decode → group-session 除外 → unsecured / secured 振り分け）（現 776–789 行 + 振り分け） |
| `Runtime::on_unsecured_datagram(&mut self, header: MessageHeader, body: &[u8], peer: SocketAddr)` (async) | セッション確立の前段: `ProtocolHeader` decode・initiator / standalone-ack フィルタ・`classify_unsecured` / `admit_unsecured`（現 790–835 行）→ `establish_session` |
| `Runtime::establish_session(&mut self, flow: UnsecuredFlow, first: IncomingMessage, peer: SocketAddr)` (async) | セッション確立: PASE / CASE の `drive_established` と成功時の `install_session`（現 836–897 行） |
| `Runtime::install_session(&mut self, local_session_id: u16, session: SecureSession, fabric_index: u8)` | PASE / CASE 共通の「current_session を置き換え、旧セッションの購読を落とす」2 行 |
| `Runtime::on_secured_datagram(&mut self, datagram: &[u8], header: &MessageHeader, peer: SocketAddr)` (async) | IM ディスパッチ: current session 有無 / session id 一致チェック → `serve_secured` → `DropSession` 処理（現 900–945 行） |
| `Runtime::on_mdns_retry(&mut self)` (async) | mDNS 再試行 branch（現 946–971 行） |
| `Runtime::on_subscription_due(&mut self)` (async) | 購読レポート branch（現 972–1019 行） |
| `Runtime::on_commissioning_window_expired(&mut self)` (async) | window 満了 branch（現 1020–1044 行） |
| `Runtime::on_fail_safe_expired(&mut self)` (async) | fail-safe 満了 branch（現 1045–1076 行） |
| `Runtime::on_group_datagram(&mut self, datagram: &[u8], from: SocketAddr)` | groupcast 受信 branch（現 1077–1095 行、同期） |

### テスト名比較コマンド（全タスク共通）

移動前の名前一覧は `/tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/d0b43e30-d460-4588-8395-ff9983f9d75f/scratchpad/tests-before.txt`（375 行、`sort` 済み、フルパス名）。各タスクの最後にこれを実行し **差分ゼロ** を確認する:

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/matv-runtime-split
cargo test -p mat-device -p matv --all-targets -- --list 2>/dev/null | /usr/bin/grep ': test$' | sed 's/: test$//' | sort > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/d0b43e30-d460-4588-8395-ff9983f9d75f/scratchpad/tests-after.txt
diff /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/d0b43e30-d460-4588-8395-ff9983f9d75f/scratchpad/tests-before.txt /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/d0b43e30-d460-4588-8395-ff9983f9d75f/scratchpad/tests-after.txt && echo SAME
```

Expected: `SAME`。

### 各タスク共通の検証コマンド

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/matv-runtime-split
cargo fmt --all
cargo clippy -p mat-device -p matv --all-targets -- -D warnings
cargo test -p mat-device -p matv 2>&1 | /usr/bin/grep -E "^test result|FAILED|panicked"
```

Expected: clippy は warning 0、`test result: ok.` のみ（`FAILED` / `panicked` の行が出ない）。その後、上のテスト名比較で `SAME`。

### `tokio::select!` と借用について（Task 1 の実装者向け）

`tokio::select!` は **branch の future 群を 1 つのブロック内に置き、いずれかが完了したらその値を取り出してブロックを抜けてから handler を実行する**。だから `() = mdns_retry_deadline(&self.mdns_retry) => self.on_mdns_retry().await` のように、future 側で `&self.x` を不変借用し handler 側で `&mut self` を取る書き方が通る（現行コードの `mdns_retry_deadline(&mdns_retry) => { .. mdns_retry = None; .. }` と同じ理屈）。`transport.recv_from(&mut buf)` の `buf` はループローカルのままにし、handler では `&buf[..n]` を渡す。

---

### Task 1: `Runtime` / `NodeState` を導入し、`run()` を `boot` + `serve_forever` に分ける（branch 本体はまだインライン）

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs:700-1108`（`run()` とその doc コメント）

**Interfaces:**
- Consumes: 既存の `bring_up_mdns` / `MdnsCtx` / `MdnsRetry` / `CommissioningWindow` / `COMMISSIONING_WINDOW_DURATION` / `GroupReplayGuard` / `ActiveSubscription` / `SecureSession`。
- Produces: `struct NodeState { node, comm_server, mdns, subscription, window, config }` + `NodeState::serve_state(&mut self) -> ServeState<'_>`；`struct Runtime { transport, port, pase_salt, mdns_retry, current_session, replay, group, state }` + `Runtime::boot(..)` / `Runtime::serve_forever(&mut self)`。Task 2–4 はこのフィールド名をそのまま使う。

- [ ] **Step 1: 開始状態の確認**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/matv-runtime-split
/usr/bin/git status --short   # 空であること
/usr/bin/git log --oneline -2  # 先頭が「陳腐化コメント」修正コミット
sed -n 700,760p crates/mat-device/src/net/runtime.rs
```

- [ ] **Step 2: `run()` の直前に `NodeState` / `Runtime` と `boot` を追加し、`run()` を 2 段呼び出しに置き換える**

`run()` の doc コメント（`/// Runs the device: binds nothing itself ...`）は **そのまま残し**、関数本体だけを次に置き換える。`run()` の旧本体 717–751 行（`let mut pase_salt` から `bring_up_mdns` の `match` の閉じ `}` まで）はコメント込みで `boot` に移す。

```rust
pub(crate) async fn run(
    transport: Arc<Transport>,
    local_addr: SocketAddr,
    config: DeviceConfig,
    node: Node,
    comm_server: CommissioningServer,
    group: GroupRx,
) -> Result<(), DeviceError> {
    Runtime::boot(transport, local_addr, config, node, comm_server, group)
        .await
        .serve_forever()
        .await
}

/// The node-side state every secured message may touch, owned here and
/// lent out as a `ServeState` (the borrowed view `serve_secured`/
/// `serve_secured_message` take) via `serve_state`. Kept as its own struct
/// rather than flattened into `Runtime` so a `Runtime` method can borrow
/// `self.current_session` mutably *and* build a `ServeState` from
/// `self.state` in the same expression — disjoint fields, so the borrow
/// checker allows it; a `serve_state(&mut self)` on `Runtime` itself would
/// borrow all of `Runtime` and conflict with the session borrow.
struct NodeState {
    node: Node,
    comm_server: CommissioningServer,
    /// `None` while `bring_up_mdns` hasn't succeeded (see `run`'s doc).
    mdns: Option<MdnsCtx>,
    /// The node's single active subscription (spec §8.10, Task 12). Tied to
    /// the session that created it: a new PASE/CASE session drops it
    /// (`Runtime::install_session`), since its reports could only ever go
    /// out over the session it was subscribed on.
    subscription: Option<ActiveSubscription>,
    window: CommissioningWindow,
    config: DeviceConfig,
}

impl NodeState {
    /// The `ServeState` view of this state — what one secured message (or
    /// one drained buffered request) is allowed to touch.
    fn serve_state(&mut self) -> ServeState<'_> {
        ServeState {
            node: &mut self.node,
            comm_server: &self.comm_server,
            mdns: self.mdns.as_ref(),
            subscription: &mut self.subscription,
            window: &mut self.window,
            config: &self.config,
        }
    }
}

/// Everything `run`'s loop carries between iterations, so the per-branch
/// handlers (`on_*`) can be plain methods instead of one 400-line
/// `select!` body. Built once by `boot`, driven forever by `serve_forever`.
struct Runtime {
    transport: Arc<Transport>,
    port: u16,
    /// A fresh random PASE salt each boot — see `boot`.
    pase_salt: [u8; 16],
    mdns_retry: Option<MdnsRetry>,
    /// The current secured session: `(local_session_id, session,
    /// fabric_index)`. Third element: the session's fabric index (spec
    /// §7.9) — `0` for PASE (no fabric yet), the CASE-selected fabric
    /// otherwise. Carried through to every `ReadRequest` this session
    /// serves via `ReadCtx` (`serve_secured`/`serve_secured_message`), so
    /// e.g. Operational Credentials' `CurrentFabricIndex` reflects the
    /// reading session, not a hardcoded value.
    current_session: Option<(u16, SecureSession, u8)>,
    replay: GroupReplayGuard,
    group: GroupRx,
    state: NodeState,
}

impl Runtime {
    /// Boot-time setup that used to open `run`: the PASE salt, the
    /// commissioning window's boot policy, and the first (best-effort)
    /// `bring_up_mdns` attempt.
    async fn boot(
        transport: Arc<Transport>,
        local_addr: SocketAddr,
        config: DeviceConfig,
        node: Node,
        comm_server: CommissioningServer,
        group: GroupRx,
    ) -> Self {
        let port = local_addr.port();
        // （旧 run() 717–724 行のコメントと `pase_salt` 生成をそのまま）
        let mut pase_salt = [0u8; 16];
        getrandom::getrandom(&mut pase_salt).expect("os rng");
        // （旧 run() 725–732 行のコメントをそのまま）
        let window = if comm_server.fabrics().is_empty() {
            CommissioningWindow::Open {
                until: Instant::now() + COMMISSIONING_WINDOW_DURATION,
            }
        } else {
            CommissioningWindow::Closed
        };
        let mut mdns_ctx: Option<MdnsCtx> = None;
        let mut mdns_retry: Option<MdnsRetry> = None;
        match bring_up_mdns(&config, port, &comm_server, &window).await {
            Ok(ctx) => mdns_ctx = Some(ctx),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    iface = %config.iface,
                    "mDNS advertiser did not come up — device still serves PASE/CASE/IM to peers that already have its address; retrying in the background"
                );
                mdns_retry = Some(MdnsRetry::new());
            }
        }
        Self {
            transport,
            port,
            pase_salt,
            mdns_retry,
            current_session: None,
            replay: GroupReplayGuard::new(),
            group,
            state: NodeState {
                node,
                comm_server,
                mdns: mdns_ctx,
                subscription: None,
                window,
                config,
            },
        }
    }

    /// The receive loop: `sync_group_joins` then one `select!` over the
    /// unicast socket, the mDNS retry timer, the subscription report
    /// deadline, the commissioning-window deadline, the fail-safe deadline,
    /// and the group socket. Never returns on its own (`run`'s doc).
    async fn serve_forever(&mut self) -> Result<(), DeviceError> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut gbuf = [0u8; MAX_DATAGRAM];
        loop {
            sync_group_joins(&mut self.group, &self.state.comm_server);
            tokio::select! {
                recv = self.transport.recv_from(&mut buf) => {
                    // （旧 run() 773–945 行の branch 本体をそのまま。ただし
                    //  ローカル変数参照を self.* に付け替える。付け替え表は Step 3）
                }
                () = mdns_retry_deadline(&self.mdns_retry) => {
                    // （旧 946–971 行、同様に付け替え）
                }
                () = subscription_deadline(&self.state.subscription) => {
                    // （旧 972–1019 行）
                }
                () = commissioning_window_deadline(&self.state.window) => {
                    // （旧 1020–1044 行）
                }
                () = fail_safe_expiry_deadline(&self.state.comm_server) => {
                    // （旧 1045–1076 行）
                }
                grecv = group_recv(&self.group.socket, &mut gbuf) => {
                    // （旧 1077–1095 行）
                }
            }
        }
    }
}
```

`boot` 内の「（旧 run() … のコメントをそのまま）」は、旧 `run()` の該当行のコメントを **一字も変えずに** そこへ置くという意味（この計画に転記しないのは行数節約のため。`sed -n 717,751p` で確認できる）。

- [ ] **Step 3: branch 本体の参照付け替え**

旧 `run()` のループ内ローカル → `Runtime` フィールドの対応表。branch 本体は Step 2 の各コメント位置へ **そのまま貼り付け**、以下だけを機械的に置換する:

| 旧 | 新 |
|---|---|
| `window`（値・`&window`・`&mut window`・`window = ...`・`window.is_open()`） | `self.state.window` |
| `mdns_ctx`（`mdns_ctx.as_ref()`・`mdns_ctx = Some(ctx)`） | `self.state.mdns` |
| `mdns_retry` | `self.mdns_retry` |
| `current_session` | `self.current_session` |
| `subscription` | `self.state.subscription` |
| `&mut node` / `node.handle_group_invoke(..)` | `&mut self.state.node` / `self.state.node.handle_group_invoke(..)` |
| `&comm_server` / `comm_server.xxx()` | `&self.state.comm_server` / `self.state.comm_server.xxx()` |
| `&config` / `config.passcode` | `&self.state.config` / `self.state.config.passcode` |
| `&transport` / `Arc::clone(&transport)` | `&self.transport` / `Arc::clone(&self.transport)` |
| `&pase_salt` | `&self.pase_salt` |
| `port` | `self.port` |
| `&mut group` / `&group.gk_store` / `&group.membership` / `&group.socket` | `&mut self.group` / `&self.group.gk_store` / ... |
| `&mut replay` | `&mut self.replay` |
| `&mut ServeState { node: &mut node, comm_server: &comm_server, mdns: mdns_ctx.as_ref(), subscription: &mut subscription, window: &mut window, config: &config }`（2 箇所） | `&mut self.state.serve_state()` |

`serve_secured` 呼び出し箇所（旧 924–935 行）は、`current_session.as_mut()` で `session` / `fabric_index` を借りながら `self.state.serve_state()` を渡す形になる:

```rust
                let Some((sid, session, fabric_index)) = self.current_session.as_mut() else {
                    // （旧 ログ・continue をそのまま）
                };
                // （旧 session id 不一致チェックをそのまま）
                let outcome = serve_secured(
                    &buf[..n],
                    peer,
                    session,
                    *fabric_index,
                    &mut self.state.serve_state(),
                )
                .await;
                // （旧 Task 6 コメントをそのまま）
                if outcome == ServeOutcome::DropSession {
                    self.current_session = None;
                }
```

購読 branch（旧 993–1017 行）も同様に `(self.state.subscription.as_mut(), self.current_session.as_mut())` … **ここだけ注意**: `send_subscription_report(session, *fabric_index, &mut node, sub)` は `subscription` と `node` を同時に可変借用する。両方 `self.state` の別フィールドなので `&mut self.state.node` と `self.state.subscription.as_mut()` を **直接フィールドで** 書けば通る（`serve_state()` を経由しない）。その後の `drain_buffered_requests(session, *fabric_index, &mut self.state.serve_state())` は `current_session` の借用と `self.state` の借用が分離しているので通る。

- [ ] **Step 4: 検証（fmt / clippy / test / テスト名比較）**

「各タスク共通の検証コマンド」と「テスト名比較コマンド」を実行。Expected: clippy warning 0、全 `test result: ok.`、`SAME`。

もし clippy が `needless_pass_by_ref_mut` や `too_many_lines` 系ではなく **借用エラー** を出したら、Step 3 の注意（`serve_state()` は `current_session` の借用と同時にだけ使い、`subscription` と `node` を同時に触る箇所は直接フィールド）を見直す。

- [ ] **Step 5: 分割前後の差分が「移動 + 参照付け替え」だけであることを目視確認**

```bash
/usr/bin/git diff --stat
/usr/bin/git diff crates/mat-device/src/net/runtime.rs | /usr/bin/grep '^[-+]' | /usr/bin/grep -v '^[-+][-+]' | /usr/bin/grep -c 'tracing::'
```

2 つ目のコマンドは「移動で消えた `tracing::` 行数と増えた行数」を合算した数。`-` と `+` が同数（= ログ行が 1 つも増減していない）であることを `git diff | grep '^-.*tracing' | wc -l` と `grep '^+.*tracing'` で別々に数えて確認する。Expected: 両者が等しい。

- [ ] **Step 6: Commit**

```bash
/usr/bin/git add crates/mat-device/src/net/runtime.rs
/usr/bin/git commit -m "refactor(mat-device): runtime::run を Runtime::boot + serve_forever に分離（挙動不変、branch 本体はまだインライン）

- ループ状態を struct Runtime / NodeState へ。NodeState::serve_state() が
  従来の ServeState を組み立てる（current_session との分離借用のため）
- run() は boot().serve_forever() の 2 段呼び出しのみ
- ログ文言・branch 順序・処理順序は不変。テスト名の多重集合 375 個不変

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DbRmGXELPVLTCHBym3RZaS"
```

---

### Task 2: タイマー 3 branch と groupcast 受信を `on_*` メソッドへ

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`（Task 1 の `Runtime::serve_forever` と `impl Runtime`）

**Interfaces:**
- Consumes: Task 1 の `Runtime` フィールド（`mdns_retry` / `state.mdns` / `state.window` / `state.comm_server` / `state.config` / `port` / `group` / `replay` / `state.node` / `state.subscription`）。
- Produces: `async fn on_mdns_retry(&mut self)` / `async fn on_commissioning_window_expired(&mut self)` / `async fn on_fail_safe_expired(&mut self)` / `fn on_group_datagram(&mut self, datagram: &[u8], from: SocketAddr)`。

- [ ] **Step 1: `serve_forever` の 4 branch をメソッド呼び出しに置き換え、本体を `impl Runtime` のメソッドへ移す**

`serve_forever` の `select!`:

```rust
                () = mdns_retry_deadline(&self.mdns_retry) => self.on_mdns_retry().await,
                () = subscription_deadline(&self.state.subscription) => {
                    // （Task 1 のまま。Task 4 で移す）
                }
                () = commissioning_window_deadline(&self.state.window) => {
                    self.on_commissioning_window_expired().await
                }
                () = fail_safe_expiry_deadline(&self.state.comm_server) => {
                    self.on_fail_safe_expired().await
                }
                grecv = group_recv(&self.group.socket, &mut gbuf) => {
                    let Ok((n, from)) = grecv else { continue };
                    self.on_group_datagram(&gbuf[..n], from);
                }
```

メソッド（各 branch の旧コメントは **メソッドの doc コメント（`///`）ではなく本体先頭の `//` コメントとして** そのまま残す。doc コメントは下の 1–2 行を新規に付ける）:

```rust
    /// The mDNS retry timer fired (`MdnsRetry`): try `bring_up_mdns` again.
    async fn on_mdns_retry(&mut self) {
        // （旧 branch 本体をそのまま: `window.is_open()` read *now* ... のコメント、
        //  match bring_up_mdns(&self.state.config, self.port, &self.state.comm_server, &self.state.window).await { .. }）
    }

    /// The boot window's 15-minute bound or an ECM window's
    /// `CommissioningTimeout` elapsed: close the window and pull the
    /// commissionable advert.
    async fn on_commissioning_window_expired(&mut self) {
        // （旧 branch 本体をそのまま）
    }

    /// The fail-safe timer lapsed without `CommissioningComplete`: roll
    /// back the uncommitted fabric (if any) and its operational advert.
    async fn on_fail_safe_expired(&mut self) {
        // （旧 branch 本体をそのまま）
    }

    /// One datagram off the group socket: authenticate/classify it
    /// (`classify_group_datagram`), apply the invokes, and mark what changed
    /// for the active subscription.
    fn on_group_datagram(&mut self, datagram: &[u8], from: SocketAddr) {
        let fabrics = self.state.comm_server.fabrics();
        let deps = GroupRxDeps { fabrics: &fabrics, gk_store: &self.group.gk_store, membership: &self.group.membership };
        match classify_group_datagram(datagram, &deps, &mut self.replay) {
            // （旧 branch 本体をそのまま。`&gbuf[..n]` → `datagram`、
            //  `len = n` → `len = datagram.len()`）
        }
    }
```

注意: 旧 groupcast branch の `Err(reason) => tracing::debug!(peer = %from, len = n, ...)` の `n` は `datagram.len()` に置き換える（値は同じ）。

- [ ] **Step 2: 検証（fmt / clippy / test / テスト名比較）**

「各タスク共通の検証コマンド」と「テスト名比較コマンド」を実行。Expected: clippy warning 0、全 `test result: ok.`、`SAME`。

- [ ] **Step 3: Commit**

```bash
/usr/bin/git add crates/mat-device/src/net/runtime.rs
/usr/bin/git commit -m "refactor(mat-device): runtime のタイマー 3 branch と groupcast 受信を Runtime::on_* へ（挙動不変）

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DbRmGXELPVLTCHBym3RZaS"
```

---

### Task 3: ユニキャスト受信 → セッション確立（unsecured 経路）をメソッドへ

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`（`Runtime::serve_forever` の `recv` branch と `impl Runtime`）

**Interfaces:**
- Consumes: Task 1 の `Runtime` フィールド、既存 `MessageHeader` / `ProtocolHeader` / `SESSION_TYPE_MASK` / `PROTOCOL_ID_SECURE_CHANNEL` / `OPCODE_MRP_STANDALONE_ACK` / `classify_unsecured` / `admit_unsecured` / `UnsecuredFlow` / `random_session_id` / `pase_config_for_window` / `crate::net::pase::drive_established` / `crate::net::case::drive_established` / `SecureSession::new_device_role`。
- Produces: `async fn on_unicast_datagram(&mut self, datagram: &[u8], peer: SocketAddr)` / `async fn on_unsecured_datagram(&mut self, header: MessageHeader, body: &[u8], peer: SocketAddr)` / `async fn establish_session(&mut self, flow: UnsecuredFlow, first: mat_controller::exchange::IncomingMessage, peer: SocketAddr)` / `fn install_session(&mut self, local_session_id: u16, session: SecureSession, fabric_index: u8)`。secured 経路は **この Task では `on_unicast_datagram` の中にインラインのまま**（Task 4 で `on_secured_datagram` へ）。

- [ ] **Step 1: `recv` branch を `on_unicast_datagram` 呼び出しにする**

```rust
                recv = self.transport.recv_from(&mut buf) => {
                    let (n, peer) = match recv {
                        Ok(v) => v,
                        Err(_) => continue, // best-effort responder — a transient recv error isn't fatal
                    };
                    self.on_unicast_datagram(&buf[..n], peer).await;
                }
```

- [ ] **Step 2: `on_unicast_datagram` / `on_unsecured_datagram` / `establish_session` / `install_session` を追加**

旧 branch の `continue` は、メソッドでは `return` に置き換える（ループの次周へ進む意味は同じ。`select!` の handler を抜ければループ先頭へ戻る）。`MessageHeader::decode(&buf[..n])` の `off` は `datagram` 基準でそのまま使える。

```rust
    /// One datagram off the unicast socket: decode the message header,
    /// drop group-session traffic (the group socket serves those), then
    /// route unsecured traffic to `on_unsecured_datagram` and secured
    /// traffic to the current session.
    async fn on_unicast_datagram(&mut self, datagram: &[u8], peer: SocketAddr) {
        let n = datagram.len();
        let Ok((header, off)) = MessageHeader::decode(datagram) else {
            tracing::debug!(peer = %peer, len = n, "datagram dropped: header decode failed");
            return;
        };
        if header.security_flags & SESSION_TYPE_MASK != 0 {
            tracing::debug!(peer = %peer, security_flags = header.security_flags, "group-session datagram on the unicast socket dropped (the group socket serves those)");
            return;
        }
        if header.session_id == 0 && header.security_flags == 0 {
            self.on_unsecured_datagram(header, &datagram[off..], peer).await;
            return;
        }

        // Secured traffic: only ever the current session (sequential,
        // one-at-a-time — see module doc).
        // （旧 secured 経路 900–945 行を Task 1 の形のまま。`&buf[..n]` → `datagram`、
        //  `continue` → `return`。Task 4 で on_secured_datagram へ移す）
    }

    /// An unsecured (`session_id == 0`) datagram: decode the protocol
    /// header, drop what can't start a session (non-initiator, standalone
    /// ack), classify (`classify_unsecured`) and admit (`admit_unsecured`)
    /// it, then hand a PASE/CASE opener to `establish_session`. `body` is
    /// the datagram from the protocol header onward (`MessageHeader::
    /// decode`'s `off`).
    async fn on_unsecured_datagram(&mut self, header: MessageHeader, body: &[u8], peer: SocketAddr) {
        let Ok((proto, body_off)) = ProtocolHeader::decode(body) else {
            tracing::debug!(peer = %peer, "unsecured datagram dropped: protocol header decode failed");
            return;
        };
        // （旧 795–835 行をそのまま: initiator チェック、standalone ack チェック、
        //  `first` の構築（payload: body[body_off..].to_vec()）、classify、
        //  "unsecured datagram received" ログ、admit_unsecured(flow, self.state.window.is_open())。
        //  `continue` → `return`）
        self.establish_session(flow, first, peer).await;
    }

    /// Session establishment: drive one PASE (`net::pase`) or CASE
    /// (`net::case`) handshake to completion for `first`, the opener just
    /// received, and on success make the result the current session
    /// (`install_session`). `UnsecuredFlow::Ignore` is a no-op.
    async fn establish_session(
        &mut self,
        flow: UnsecuredFlow,
        first: mat_controller::exchange::IncomingMessage,
        peer: SocketAddr,
    ) {
        match flow {
            UnsecuredFlow::Pase => {
                let local_session_id = random_session_id();
                let outcome = crate::net::pase::drive_established(
                    &self.transport,
                    peer,
                    first,
                    pase_config_for_window(
                        &self.state.window,
                        self.state.config.passcode,
                        &self.pase_salt,
                        local_session_id,
                    ),
                )
                .await;
                match outcome {
                    Ok((keys, peer_session_id)) => {
                        tracing::debug!(
                            local_session_id,
                            peer_session_id,
                            peer = %peer,
                            "PASE established"
                        );
                        let session = SecureSession::new_device_role(
                            Arc::clone(&self.transport),
                            peer,
                            local_session_id,
                            peer_session_id,
                            keys,
                            0, // PASE: both sides are node id 0 (spec §4.13)
                            0,
                        );
                        self.install_session(local_session_id, session, 0); // PASE: no fabric yet
                    }
                    // （旧コメントをそのまま）
                    Err(e) => tracing::debug!(error = %e, peer = %peer, "PASE failed"),
                }
            }
            UnsecuredFlow::Case => {
                let local_session_id = random_session_id();
                let fabrics = self.state.comm_server.fabrics();
                let outcome = crate::net::case::drive_established(
                    Arc::clone(&self.transport),
                    peer,
                    first,
                    fabrics,
                    local_session_id,
                )
                .await;
                match outcome {
                    Ok((session, fabric_index)) => {
                        tracing::debug!(
                            local_session_id,
                            fabric_index,
                            peer = %peer,
                            "CASE established"
                        );
                        self.install_session(local_session_id, session, fabric_index);
                    }
                    Err(e) => tracing::debug!(error = %e, peer = %peer, "CASE failed"),
                }
            }
            UnsecuredFlow::Ignore => {}
        }
    }

    /// Makes `session` the current session, replacing whatever was there.
    /// The active subscription (if any) belonged to the replaced session
    /// and is dropped with it — its reports could only ever have gone out
    /// over that session.
    fn install_session(&mut self, local_session_id: u16, session: SecureSession, fabric_index: u8) {
        self.current_session = Some((local_session_id, session, fabric_index));
        self.state.subscription = None;
    }
```

- [ ] **Step 3: 検証（fmt / clippy / test / テスト名比較）**

「各タスク共通の検証コマンド」と「テスト名比較コマンド」を実行。Expected: clippy warning 0、全 `test result: ok.`、`SAME`。

- [ ] **Step 4: Commit**

```bash
/usr/bin/git add crates/mat-device/src/net/runtime.rs
/usr/bin/git commit -m "refactor(mat-device): runtime のユニキャスト受信とセッション確立を Runtime::on_unicast_datagram / on_unsecured_datagram / establish_session へ（挙動不変）

PASE / CASE 成功時の current_session 置き換え + 購読破棄を install_session に共通化

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DbRmGXELPVLTCHBym3RZaS"
```

---

### Task 4: IM ディスパッチ（secured 経路）と購読レポート branch をメソッドへ

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`（`Runtime::on_unicast_datagram` の secured 部分、`serve_forever` の購読 branch、`impl Runtime`）

**Interfaces:**
- Consumes: Task 1 の `NodeState::serve_state`、Task 3 の `on_unicast_datagram`、既存 `serve_secured` / `drain_buffered_requests` / `send_subscription_report` / `ServeOutcome`。
- Produces: `async fn on_secured_datagram(&mut self, datagram: &[u8], header: &MessageHeader, peer: SocketAddr)` / `async fn on_subscription_due(&mut self)`。

- [ ] **Step 1: `on_unicast_datagram` の secured 部分を `on_secured_datagram` へ**

`on_unicast_datagram` の末尾を:

```rust
        self.on_secured_datagram(datagram, &header, peer).await;
    }
```

に置き換え、メソッドを追加:

```rust
    /// IM dispatch for one secured datagram: it must belong to the current
    /// session (sequential, one-at-a-time — see module doc), then
    /// `serve_secured` decrypts/screens it, serves the Interaction Model
    /// request, and drains any cross-exchange requests buffered meanwhile.
    /// A `RemoveFabric` that removed this session's own fabric ends the
    /// session (Task 6).
    async fn on_secured_datagram(&mut self, datagram: &[u8], header: &MessageHeader, peer: SocketAddr) {
        // Secured traffic: only ever the current session (sequential,
        // one-at-a-time — see module doc).
        let Some((sid, session, fabric_index)) = self.current_session.as_mut() else {
            tracing::debug!(
                session_id = header.session_id,
                peer = %peer,
                "secured datagram dropped: no session established"
            );
            return;
        };
        if header.session_id != *sid {
            tracing::debug!(
                session_id = header.session_id,
                current_session_id = *sid,
                peer = %peer,
                "secured datagram dropped: session id does not match the current session"
            );
            return;
        }
        let outcome = serve_secured(
            datagram,
            peer,
            session,
            *fabric_index,
            &mut self.state.serve_state(),
        )
        .await;
        // Task 6: a `RemoveFabric` that removed this session's own
        // fabric — `session`/`fabric_index` (borrowed out of
        // `current_session` above) are no longer used past this
        // point in this iteration, so this is the first place the
        // borrow checker lets `current_session` be reassigned.
        if outcome == ServeOutcome::DropSession {
            self.current_session = None;
        }
    }
```

- [ ] **Step 2: 購読 branch を `on_subscription_due` へ**

`serve_forever`:

```rust
                () = subscription_deadline(&self.state.subscription) => self.on_subscription_due().await,
```

メソッド（旧 branch 先頭の長いコメント「The active subscription is due for a report ... exactly like the ones landing during a reply's ack-wait.」は本体先頭の `//` コメントとしてそのまま残す）:

```rust
    /// The active subscription's report deadline: send the dirty
    /// attributes (or an empty keep-alive) on a device-initiated exchange,
    /// drop the subscription if the report isn't acknowledged, then serve
    /// whatever requests were buffered during that ack-wait.
    async fn on_subscription_due(&mut self) {
        // （旧コメントをそのまま）
        let delivered = match (self.state.subscription.as_mut(), self.current_session.as_mut()) {
            (Some(sub), Some((_, session, fabric_index))) => {
                send_subscription_report(session, *fabric_index, &mut self.state.node, sub).await
            }
            // A subscription that outlived its session has nothing
            // to report over — drop it.
            _ => false,
        };
        if !delivered {
            tracing::debug!(
                subscription_id = self.state.subscription.as_ref().map(|s| s.id),
                "subscription dropped: report was not acknowledged"
            );
            self.state.subscription = None;
        }
        let mut drop_session = false;
        if let Some((_, session, fabric_index)) = self.current_session.as_mut() {
            drop_session = drain_buffered_requests(
                session,
                *fabric_index,
                &mut self.state.serve_state(),
            )
            .await
                == ServeOutcome::DropSession;
        }
        // Task 6: same reasoning as the datagram branch above — a
        // buffered `RemoveFabric` piggybacked on this session's own
        // fabric ends the session too, not just one arriving as a
        // fresh datagram.
        if drop_session {
            self.current_session = None;
        }
    }
```

- [ ] **Step 3: `serve_forever` が「`sync_group_joins` + 6 branch がそれぞれ 1〜5 行」になっていることを確認**

```bash
/usr/bin/grep -n "async fn serve_forever" -A 40 crates/mat-device/src/net/runtime.rs | head -45
```

Expected: `select!` 内に `tracing::` 呼び出しが残っていない（すべて `on_*` の中）。

- [ ] **Step 4: 検証（fmt / clippy / test / テスト名比較）**

「各タスク共通の検証コマンド」と「テスト名比較コマンド」を実行。Expected: clippy warning 0、全 `test result: ok.`、`SAME`。

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add crates/mat-device/src/net/runtime.rs
/usr/bin/git commit -m "refactor(mat-device): runtime の IM ディスパッチと購読レポートを Runtime::on_secured_datagram / on_subscription_due へ（挙動不変）

serve_forever は sync_group_joins + select! 6 branch のディスパッチだけになった

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DbRmGXELPVLTCHBym3RZaS"
```

---

### Task 5: 仕上げ — doc 追従・`task check`・E2E m1 / m3・実測記録

**Files:**
- Modify: `crates/mat-device/src/device.rs:547`（`run` の doc コメント 1 行）
- Modify: `crates/mat-device/src/net/runtime.rs`（module doc / `run` doc の `run`'s loop 言及、必要なら）
- Modify: `docs/superpowers/plans/2026-09-05-matv-runtime-run-split.md`（実測記録の追記）

- [ ] **Step 1: 「`run`'s loop / `run`'s `select!`」言及の棚卸し**

```bash
/usr/bin/grep -n "run\`'s loop\|run\`'s \`select\|in \`run\`\|(\`run\`)\|run\`'s own\|run\`'s caller" crates/mat-device/src/net/runtime.rs crates/mat-device/src/device.rs
```

それぞれについて: 言及先が「ループ」「`select!` branch」なら `Runtime::serve_forever` / 該当 `on_*` メソッド名に置き換える。「`run`'s doc comment」（`run` の doc を指すもの）は `run` の doc が残っているのでそのまま。`device.rs:547` の `tracked entirely inside `net::runtime::run`'s loop` は `tracked entirely inside `net::runtime`'s `Runtime` (`serve_forever`'s loop)` に。**コメント以外は触らない。**

- [ ] **Step 2: 関数長の実測**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/matv-runtime-split/crates/mat-device/src && python3 - <<'EOF'
import re,os
for root,_,files in os.walk('.'):
  for f in files:
    if not f.endswith('.rs'): continue
    p=os.path.join(root,f); L=open(p,encoding='utf-8').read().split('\n')
    for i,l in enumerate(L):
        m=re.match(r'^(\s*)(pub(\([a-z]+\))? )?(async )?fn (\w+)',l)
        if not m: continue
        indent=len(m.group(1)); end=None
        for j in range(i+1,len(L)):
            if L[j].rstrip()=='}' and len(L[j])-len(L[j].lstrip())==indent: end=j;break
        if end and end-i>=100: print(f"{end-i:4d} {p}:{i+1} {m.group(5)}")
EOF
```

Expected: `run` と `serve_forever` は 100 行未満で一覧に出ない。`Runtime` の各 `on_*` / `establish_session` も 100 行未満。100 行以上で残るのは既存の `serve_secured_message`（260 行、本計画の対象外）と `serve_subscribe_request`（160 行、対象外）とテスト関数のみ。

- [ ] **Step 3: `task check`**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/matv-runtime-split && task check 2>&1 | tail -15
```

Expected: fmt:check / clippy / test すべて成功（末尾に `test result: ok.` 群、エラー無し）。

- [ ] **Step 4: E2E m1 / m3（実 NIC `eth1`、実デバイス不要）**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/matv-runtime-split
task e2e:device:m1 2>&1 | tail -25
task e2e:device:m3 2>&1 | tail -25
```

Expected: 各スクリプトが `set -e` で途中終了せず、末尾に成功メッセージ（m1: commission `"status":"success"` と unpair 後の exit 11 確認、m3: `mat listen` の on-off イベント受信）。失敗したら **修正せず** 出力末尾 40 行を報告に貼る（挙動不変の分割で E2E が落ちるなら分割にバグがあるので、コントローラ側の問題と決めつけない）。

- [ ] **Step 5: 実測記録を本計画ファイル末尾に追記**

```markdown
## 実測記録（2026-09-05）

- `run()`: 399 行 → N 行（`boot` X 行 / `serve_forever` Y 行 / `on_*` 最大 Z 行）
- テスト名多重集合 375 個不変、`task check` 合格、E2E m1 / m3 合格（`MAT_E2E_IFACE` 未指定 = `eth1`）
- 気づき: （分割中に見えた「ついで修正しなかった」点があれば）
```

- [ ] **Step 6: Commit**

```bash
/usr/bin/git add crates/mat-device/src/device.rs crates/mat-device/src/net/runtime.rs docs/superpowers/plans/2026-09-05-matv-runtime-run-split.md
/usr/bin/git commit -m "docs(mat-device): run ループ分割に伴う doc 言及の追従と実測記録

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DbRmGXELPVLTCHBym3RZaS"
```

---

## Self-Review

- **Spec coverage**: 受信ループ = `serve_forever`（Task 1, 4）、セッション確立 = `on_unsecured_datagram` / `establish_session`（Task 3）、IM ディスパッチ = `on_secured_datagram`（Task 4）、購読 = `on_subscription_due`（Task 4）、groupcast 受信 = `on_group_datagram`（Task 2）。挙動不変の担保 = ログ行数比較（Task 1 Step 5）+ テスト多重集合 + `task check` + E2E m1/m3（Task 5）。編集範囲 = mat-device のみ。
- **Placeholder scan**: 「（旧 … をそのまま）」は既存コード行の転記指示で、該当行番号を明示している（実装者は `sed -n` で読める）。
- **Type consistency**: `NodeState` のフィールド名（`node` / `comm_server` / `mdns` / `subscription` / `window` / `config`）は Task 1 で定義し Task 2–4 で同名参照。`Runtime` のフィールド（`transport` / `port` / `pase_salt` / `mdns_retry` / `current_session` / `replay` / `group` / `state`）も同様。`install_session(u16, SecureSession, u8)` は Task 3 の定義と呼び出しで一致。`on_secured_datagram(&[u8], &MessageHeader, SocketAddr)` は Task 4 の定義と `on_unicast_datagram` からの呼び出しで一致。

## 実測記録（2026-09-05）

- `run()`: 399 行 → 13 行（`Runtime::boot` を呼んで `serve_forever` に委譲するだけの薄いラッパー）。
  - `boot`: 62 行
  - `serve_forever`: 28 行（`select!` 本体、6 branch）
  - `on_*` / `establish_session` 各メソッド（`crates/mat-device/src/net/runtime.rs`、brace 対応で実測）:
    - `on_subscription_due`: 47 行
    - `on_unicast_datagram`: 18 行
    - `on_secured_datagram`: 42 行
    - `on_unsecured_datagram`: 54 行
    - `establish_session`: 74 行（最大）
    - `install_session`: 4 行
    - `on_mdns_retry`: 33 行
    - `on_commissioning_window_expired`: 27 行
    - `on_fail_safe_expired`: 31 行
    - `on_group_datagram`: 28 行
  - 全て 100 行未満。ブリーフ Step 2 の公式測定スクリプト（`crates/mat-device/src` 直下、`fn` 行から同一インデントの `}` 行までを数える簡易版）を実行した結果も一致:
    ```
    260 ./net/runtime.rs:1406 serve_secured_message
    160 ./net/runtime.rs:1756 serve_subscribe_request
    ```
    （この 2 件は本計画の対象外、想定どおり `run`/`serve_forever`/`on_*` は一覧に出ない。）
- テスト名多重集合 375 個不変（`cargo test -p mat-device -p matv --all-targets -- --list` の `sort` 済み一覧を Task 1 のベースライン `tests-before.txt` と diff → `SAME`、`wc -l` 双方 375）。
- `task check` 合格（fmt:check / clippy -D warnings / 全ワークスペーステスト、`test result: ok.` のみ、FAILED/panicked 0 件、exit 0）。
- E2E m1 / m3 合格（`MAT_E2E_IFACE` 未指定 = デフォルト `eth1`）:
  - m1: `mat commission` → `"status":"success"`、`mat unpair` 後の `RemoveFabric` ログ + node 1 の `read` が exit 11（ledger 削除確認）まで PASS。
  - m3: group provision/list/remove/re-provision/groupcast on-off に加え、`matd` 常駐 Subscribe 経由の `mat listen` が on-off=true イベントを受信して PASS。
- 気づき: ブリーフ Step 2 の測定スクリプトは `L[j].rstrip()=='}'`（末尾空白除去のみ、先頭空白は残る）という条件のため、`impl Runtime { .. }` 内のようにインデントされたメソッドの閉じ `}` には原理上マッチしない（`if end and end-i>=100` の `end` が `None` のままガードされるため、クラッシュせず単に集計対象から漏れる）。トップレベル関数（`serve_secured_message` 等）はインデント 0 なので正しく測れる。今回は Step 2 の「期待どおり一覧に出ない／出る」の判定自体は brief の意図通り成立しているため機能上の問題はないが、スクリプトを流用する将来のタスクは先頭空白ごと比較する `strip()` に直すべき（今回はコメントのみのタスクなのでスクリプト自体は直していない）。
