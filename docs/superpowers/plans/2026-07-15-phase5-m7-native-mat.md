# Phase 5 M7: native 版 mat + 本番 matd native 化 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mat one-shot 直経路に matd と同じ native ホットパス（unicast on/off/color/color-temp/onoff read + group 3 形）を実装し、本番 jarvis の matd を native 有効で運用開始する。

**Architecture:** matd の `native.rs` からプロセス形態非依存のコアを新共有クレート `mat-native` に抽出し、matd（warm 常駐）と mat（one-shot: 確立→1 op→破棄）の両方から使う。group counter は `PersistedGroupCounter` に flock を足してプロセス間共有。有効化は mat 側 `MAT_IFACE`（opt-in、未設定=挙動不変）。

**Tech Stack:** Rust (workspace: mat-core / mat-controller / mat-native(新) / mat / matd)、tokio、rustix(flock)、clap。

**Spec:** `docs/superpowers/specs/2026-07-15-phase5-m7-native-mat-design.md`

## Global Constraints

- **作業場所**: worktree `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/phase5-m1-controller-core`、ブランチ `matter-controller`。**各タスク冒頭で必ず `pwd` と `git branch --show-current` を確認**（サブエージェントの shell はメイン repo の main で始まる既知の罠）。
- 各タスクのコミット前に `task check`（fmt:check + clippy -D warnings + test）を worktree で全通過させる。
- **stdout は純粋 JSON のみ**。診断は stderr（`tracing`）。JSON スキーマは既存と完全一致（native 化で 1 フィールドも変えない）。
- **挙動不変の原則**: `MAT_IFACE` 未設定の mat、および matd の外部挙動（socket protocol / フォールバック分岐）は変えない。既存テストは無改変で通す（Task 2/3 のリファクタ回帰ガード）。
- repo は public。実 IP / 実 node_id / 実証明書 / 実鍵をコード・テスト・ドキュメントに書かない（ダミーは RFC 5737 / 適当な 0xDD 鍵等）。
- コミットメッセージは既存の流儀（`feat(controller): ... (M7 TaskN)` 等の日本語 Conventional Commits）。
- バージョンは Task 7 で workspace 一括 0.17.0。
- 対象 op のパリティ基準は matd の `is_native_hotpath` / `native_group_params`（`crates/matd/src/server.rs:253-300` 付近）。これと差を作らない。

---

### Task 1: PersistedGroupCounter にプロセス間排他（flock）

**Files:**
- Modify: `crates/mat-controller/Cargo.toml`
- Modify: `crates/mat-controller/src/group.rs`（`PersistedGroupCounter`）

**Interfaces:**
- Produces: `PersistedGroupCounter::load(path, chip_tool_gdc)` — シグネチャ不変。ただし別プロセス（別 open file description）が保持中は `io::ErrorKind::WouldBlock` の `Err`。ロックはインスタンス生存中保持、Drop で解放。
- Consumes: 既存の `matd::native`（counter load 失敗→`GroupOutcome::Unavailable` に落ちる既存経路。変更不要）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/group.rs` の `mod tests` に追加:

```rust
#[test]
fn counter_load_is_exclusive_across_handles() {
    let p = tmp_counter_path("flock");
    let _ = std::fs::remove_file(&p);
    let first = PersistedGroupCounter::load(&p, 0).unwrap();
    // 保持中の 2 度目の load は WouldBlock（別プロセスの matd/one-shot 相当。
    // flock は open file description 単位なので同一プロセスでも競合する）。
    let err = PersistedGroupCounter::load(&p, 0).expect_err("second load must fail while held");
    assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    // 解放後は再取得できる。
    drop(first);
    let _again = PersistedGroupCounter::load(&p, 0).expect("load after release");
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(PathBuf::from(format!("{}.lock", p.display())));
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller counter_load_is_exclusive -- --nocapture`
Expected: FAIL（2 度目の load が成功してしまう）

- [ ] **Step 3: 実装**

`crates/mat-controller/Cargo.toml` の `[dependencies]` に追加（matd と同じ指定）:

```toml
rustix = { version = "1", features = ["fs"] }
```

`group.rs` の `PersistedGroupCounter`:

```rust
pub struct PersistedGroupCounter {
    next: u32,
    ceiling: u32,
    path: PathBuf,
    /// プロセス間排他（advisory flock、`<path>.lock` に取る）。counter 本体は
    /// tmp+rename で置換されるため本体 fd への flock は rename 後に無効化される
    /// —— ロックは安定した別ファイルに取り、インスタンス生存中保持する
    /// （Drop で OS が解放。matd 常駐中は one-shot の load が WouldBlock になり、
    /// native 送信元の counter 混在を構造的に防ぐ）。
    _lock: std::fs::File,
}
```

`load()` の先頭（既存の read より前）にロック取得を挿入:

```rust
pub fn load(path: &Path, chip_tool_gdc: u32) -> io::Result<Self> {
    use rustix::fs::{flock, FlockOperation};
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(lock_path))?;
    flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        if e == rustix::io::Errno::WOULDBLOCK {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "group counter is locked by another process (matd running?)",
            )
        } else {
            io::Error::other(e)
        }
    })?;
    // ...既存の read_to_string / parse / jump-ahead はそのまま...
    let mut c = Self {
        next: start,
        ceiling: start,
        path: path.to_path_buf(),
        _lock: lock,
    };
    c.persist(start.wrapping_add(COUNTER_EPOCH))?;
    Ok(c)
}
```

既存テストの構築箇所（`Self { next, ceiling, path }` を直書きしている箇所は無い —
全て `load()` 経由なので他の変更は不要）。`counter_reload_never_reuses_values` は
`drop(c)` 済みなのでそのまま通る。テスト終了時の `.lock` ファイル掃除は
新テストのみでよい（他テストは tmp 名がユニークで実害なし）。

- [ ] **Step 4: テスト全通過を確認**

Run: `cargo test -p mat-controller group::`
Expected: PASS（既存 counter テスト含む全件）

- [ ] **Step 5: matd 側の挙動確認（コード変更なし）**

`crates/matd/src/native.rs` の `group_invoke` は counter load 失敗を
`GroupOutcome::Unavailable(format!("group counter store: {e}"))` に落とす既存分岐が
あるので WouldBlock も自然に chip-tool フォールバックになる。確認のみ:

Run: `cargo test -p matd`
Expected: PASS（無改変）

- [ ] **Step 6: Commit**

```bash
git add crates/mat-controller/Cargo.toml crates/mat-controller/src/group.rs
git commit -m "feat(controller): PersistedGroupCounterにflockプロセス間排他 (M7 Task1)"
```

---

### Task 2: 共有クレート `crates/mat-native` 新設（コア抽出）

matd の `native.rs` からプロセス形態非依存のコアを**コピーして**新クレートに置く
（matd 側の削除・切替は Task 3。本タスク終了時点では重複が存在するが、両方とも
テスト付きでコンパイル・全通過する）。

**Files:**
- Modify: `Cargo.toml`（workspace members + workspace.dependencies）
- Create: `crates/mat-native/Cargo.toml`
- Create: `crates/mat-native/src/lib.rs`
- Create: `crates/mat-native/src/group.rs`
- Create: `crates/mat-native/src/test_support.rs`
- 移設元（読むだけ・変更しない）: `crates/matd/src/native.rs`

**Interfaces:**
- Produces（mat / matd 両方が Task 3/4/5 で使う公開 API）:
  - `mat_native::NativeConfig { pub store: PathBuf, pub iface: String, pub fabric_index: u8, pub issuer_index: u8 }`
  - `#[async_trait] pub trait NodeConn: Send { async fn read_onoff(&mut self, endpoint: u16) -> Result<bool, MatError>; async fn invoke(&mut self, endpoint: u16, cluster: u32, command: u32, fields: Option<Vec<u8>>) -> Result<(), MatError>; }`
  - `#[async_trait] pub trait Establisher: Send + Sync { async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError>; }`
  - `pub struct Engine { pub establisher: Box<dyn Establisher>, pub group: Option<group::GroupCtx> }`
  - `impl Engine { pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError>; pub fn with_parts(establisher: Box<dyn Establisher>, group: Option<group::GroupCtx>) -> Self }`
  - `mat_native::group::GroupCtx`（フィールドは現 matd 版と同一、全 `pub`）
  - `mat_native::group::GroupOutcome { Sent, Unavailable(String) }`
  - `pub async fn group::send(ctx: &GroupCtx, group_id: u16, cluster: u32, command: u32, fields: Option<Vec<u8>>) -> Result<GroupOutcome, MatError>`（現 `NativeBackend::group_invoke` の本体）
  - `mat_native::test_support`（feature `test-support` or 自 crate test 時のみ）: `FakeConn` / `FakeEstablisher` / `write_group_fixture_ini` / `McastCandidate` / `multicast_capable_interfaces`（全て `pub` 化）

- [ ] **Step 1: workspace 配線**

`Cargo.toml`（root）:

```toml
members = ["crates/mat-core", "crates/mat", "crates/matd", "crates/mat-controller", "crates/mat-native"]
```

`[workspace.dependencies]` に追加:

```toml
mat-native = { path = "crates/mat-native" }
```

- [ ] **Step 2: crate 雛形**

`crates/mat-native/Cargo.toml`:

```toml
[package]
name = "mat-native"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Shared native backend engine (mat one-shot / matd resident) on mat-controller"

[features]
# 他 crate のテストから fake / fixture を使うための注入面。
test-support = ["dep:base64ct"]

[dependencies]
mat-core.workspace = true
mat-controller.workspace = true
async-trait.workspace = true
tokio = { version = "1", features = ["sync", "net", "time", "rt", "macros"] }
tracing.workspace = true
base64ct = { version = "1", features = ["alloc"], optional = true }

[dev-dependencies]
tempfile.workspace = true
base64ct = { version = "1", features = ["alloc"] }
```

- [ ] **Step 3: コア移設（コピー）**

`crates/matd/src/native.rs` から以下を **そのまま**（doc コメント込みで）
`crates/mat-native/src/lib.rs` / `src/group.rs` / `src/test_support.rs` にコピーし、
指示どおりに可視性・名前を変える。ロジックは 1 行も変えない。

`src/lib.rs`（`//!` doc は「mat one-shot / matd 常駐の両方が使う native エンジン。
warm セッションの保持方針は呼び出し側の責務」の旨で書き直す）:

- `NativeConfig`（そのまま、`pub`）
- `NodeConn` / `Establisher`（`pub(crate)` → `pub` に変更、trait メソッドもそのまま）
- 現 `NativeBackend::build` の本体 → `Engine::build` に移す。`Self::with_parts(...)`
  の戻りを `Engine { establisher: Box::new(establisher), group: Some(group) }` 相当に。
  `Engine::with_parts` は `pub fn`（cfg(test) を外す — mat / matd のテストが使う）。
- `CaseEstablisher` / `SessionConn` / `map_session_err` / `RESOLVE_TIMEOUT`
  （private のまま移設）
- 末尾に `pub mod group;` と
  `#[cfg(any(test, feature = "test-support"))] pub mod test_support;`

```rust
/// native エンジン: 確立器 + （任意の）group 送信コンテキスト。
/// warm セッションを保持するか（matd）、確立→1 op→破棄するか（mat one-shot）は
/// 呼び出し側が決める —— Engine 自体はセッションを持たない。
pub struct Engine {
    pub establisher: Box<dyn Establisher>,
    pub group: Option<group::GroupCtx>,
}
```

`src/group.rs`:

- `GroupCtx`（フィールド全 `pub`）/ `GroupOutcome`（`pub`）
- 現 `NativeBackend::group_invoke` の本体 → `pub async fn send(ctx: &GroupCtx, group_id: u16, cluster: u32, command: u32, fields: Option<Vec<u8>>) -> Result<GroupOutcome, MatError>`
  （`let Some(ctx) = &self.group else ...` の分岐だけ呼び出し側責務になるため削除。
  それ以外は同一）

`src/test_support.rs`:

- 現 `test_support` mod の中身を全て `pub` で移設（`FakeConn` / `FakeEstablisher` /
  `keymap_blob` / `keyset_blob`（この 2 つは private のまま）/
  `write_group_fixture_ini` / `McastCandidate` / `multicast_capable_interfaces`）。

- [ ] **Step 4: テスト移設**

現 `native.rs` の `mod tests` から、warm セッション管理に依存**しない**テストを
mat-native に移す（`Engine` / `group::send` を使う形に呼び替え）:

- `build_fails_cleanly_without_kvs` → `Engine::build` で同内容（lib.rs の tests）
- `group_invoke_sends_multicast_and_reports_sent` → `group::send(&ctx, ...)` 直呼びに
  書き換えて group.rs の tests へ（`NativeBackend::with_parts` 経由をやめ、
  未 provision group 99 の Unavailable 分岐も同様に `send` 直呼び）
- `group_invoke_without_ctx_is_unavailable` は「ctx なし」の分岐が呼び出し側に
  移ったため mat-native では**書かない**（Task 3 の matd 側と Task 5 の mat 側で
  それぞれ担保）

warm 系 4 テスト（`reuses_warm_session_for_same_node` /
`re_establishes_once_on_send_timeout` / `does_not_re_establish_on_device_rejected` /
`drops_session_on_session_fatal_error_without_retry`）は matd に残す（Task 3）。

- [ ] **Step 5: ビルド・テスト**

Run: `cargo test -p mat-native`
Expected: PASS（移設テスト全件。multicast テストは実行時 iface 発見方式なので
環境により PASS する iface を探す — 既存と同じ）

Run: `task check`
Expected: PASS（matd は無改変のまま重複コードと共存してコンパイルできる）

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/mat-native
git commit -m "feat(native): 共有エンジンcrate mat-native 新設（matd native.rsからコア抽出） (M7 Task2)"
```

---

### Task 3: matd を mat-native 消費に薄化（外部挙動不変）

**Files:**
- Modify: `crates/matd/Cargo.toml`
- Modify: `crates/matd/src/native.rs`（コア部分を削除し mat-native 委譲に）
- Modify: `crates/matd/src/server.rs`（`use` の付け替えのみ）

**Interfaces:**
- Consumes: Task 2 の `mat_native::{Engine, NativeConfig, Establisher, NodeConn}`、`mat_native::group::{GroupCtx, GroupOutcome, send}`、`mat_native::test_support`
- Produces（matd 内の既存契約を維持）: `matd::native::NativeBackend`（`build` / `read_onoff` / `on` / `off` / `color` / `color_temp` / `group_invoke` のシグネチャ不変）、`matd::native::{NativeConfig, GroupOutcome}`（re-export で温存 — `main.rs` / `server.rs` の参照先を壊さない）

- [ ] **Step 1: Cargo 配線**

`crates/matd/Cargo.toml` の `[dependencies]` に `mat-native.workspace = true`、
`[dev-dependencies]` に `mat-native = { workspace = true, features = ["test-support"] }` を追加。

- [ ] **Step 2: native.rs を書き換え**

残すもの: `NodeSlot` 型 / `NativeBackend`（`sessions` + `engine`）/ `with_session`
（establish 呼び出しを `self.engine.establisher.establish(...)` に）/
op メソッド群 / warm 系テスト 4 件（import を `mat_native::test_support::*` に）。

```rust
pub use mat_native::group::{GroupCtx, GroupOutcome};
pub use mat_native::{Establisher, NativeConfig, NodeConn};

/// warm CASE セッションを per-node に保持する native バックエンド。
/// エンジン（確立・group 送信）は mat-native と共有し、warm 保持だけが matd の責務。
pub struct NativeBackend {
    engine: mat_native::Engine,
    sessions: Mutex<HashMap<u64, NodeSlot>>,
}

impl NativeBackend {
    pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError> {
        Ok(Self::from_engine(mat_native::Engine::build(cfg).await?))
    }

    fn from_engine(engine: mat_native::Engine) -> Self {
        Self { engine, sessions: Mutex::new(HashMap::new()) }
    }

    /// テスト用: 任意の Establisher / group ctx を注入する。
    #[cfg(test)]
    pub(crate) fn with_establisher(establisher: Box<dyn Establisher>) -> Self {
        Self::from_engine(mat_native::Engine::with_parts(establisher, None))
    }

    pub(crate) fn with_parts(establisher: Box<dyn Establisher>, group: Option<GroupCtx>) -> Self {
        Self::from_engine(mat_native::Engine::with_parts(establisher, group))
    }

    pub async fn group_invoke(
        &self,
        group_id: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
    ) -> Result<GroupOutcome, MatError> {
        let Some(ctx) = &self.engine.group else {
            return Ok(GroupOutcome::Unavailable(
                "native group context not configured".into(),
            ));
        };
        mat_native::group::send(ctx, group_id, cluster, command, fields).await
    }
    // read_onoff / on / off / color / color_temp / with_session は現状のまま
}
```

削除するもの: `NativeConfig` / trait 定義 / `CaseEstablisher` / `SessionConn` /
`map_session_err` / `GroupCtx` / `GroupOutcome` 定義 / `group_invoke` 旧本体 /
`test_support` mod / mat-native へ移設済みテスト。`Debug` impl は
`NativeBackend` に残す（フィールド構成が変わるので `finish_non_exhaustive` のまま）。

`server.rs` は `use crate::native::...` のままで動くはず（re-export）。
`group_invoke_without_ctx_is_unavailable` テストを matd 側に残す
（`NativeBackend::with_establisher` 経由 — 「ctx なし」分岐の担保）。

- [ ] **Step 3: 回帰確認（無改変テストが通ることが受け入れ条件）**

Run: `cargo test -p matd`
Expected: PASS。**`server.rs` / `tests/` 配下の既存テストへの変更は import 行の
付け替え以外に一切無いこと**を `git diff --stat` で確認する。

Run: `task check`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/matd Cargo.lock
git commit -m "refactor(matd): native コアを mat-native 委譲に薄化（外部挙動不変） (M7 Task3)"
```

---

### Task 4: mat 直経路 native — CLI 配線と unicast ホットパス

**Files:**
- Modify: `crates/mat/Cargo.toml`
- Modify: `crates/mat/src/cli.rs`（グローバル引数 3 つ）
- Modify: `crates/mat/src/main.rs`（matd 経路の後・chip-tool 経路の前に native 分岐）
- Create: `crates/mat/src/native_direct.rs`
- Modify: `crates/mat/src/commands/invoke.rs` / `read.rs`（emit ヘルパ抽出）
- Test: `crates/mat/src/native_direct.rs`（unit）+ `crates/mat/tests/integration.rs`（フォールバック）

**Interfaces:**
- Consumes: `mat_native::{Engine, NativeConfig, Establisher}`、`mat_native::test_support::FakeEstablisher`、`mat_controller::im`（fields encoder）、`commands::invoke::resolve_color_temp`、`mat_core::color::resolve_spec`
- Produces:
  - `native_direct::Config<'a> { pub iface: &'a str, pub fabric_index: u8, pub issuer_index: u8 }`
  - `native_direct::try_run(command: &Command, store_path: &Path, cfg: &Config) -> Option<Result<(), MatError>>`（`None` = 非対象 op or エンジン構築不可/フォールバック — 呼び出し側が chip-tool 直へ）
  - `commands::invoke::emit_invoke_success(node_id, endpoint, cluster: &str, command: &str)` / `emit_color_temp_success(node_id, endpoint, kelvin, mireds, transition)` / `emit_color_success(node_id, endpoint, color: &ResolvedColor, transition)`（既存 run 系と native の両方が呼ぶ。Task 5 の group 版も同型）
  - `commands::read::emit_read_success(node_id, endpoint, cluster: &str, attribute: &str, value: serde_json::Value)`

- [ ] **Step 1: emit ヘルパ抽出（挙動不変リファクタ）**

`invoke.rs`: `run` / `run_color_temp` / `run_color` の `output::emit(json!({...}))` 部を
そのまま `pub(crate) fn emit_invoke_success(...)` / `emit_color_temp_success(...)` /
`emit_color_success(...)` に切り出し、既存関数はそれを呼ぶ。JSON の中身は
1 フィールドも変えない。`read.rs` も同様に `emit_read_success` を切り出す。

Run: `cargo test -p mat`
Expected: PASS（無改変で全通過 = スキーマ不変の証明）

- [ ] **Step 2: CLI 引数**

`cli.rs` の `Cli` に追加:

```rust
/// one-shot 直経路を native（mat-controller 内蔵）で実行する場合の
/// Thread mesh iface 名（例: eth0）。未設定なら従来どおり chip-tool 直。
/// 対象 op は on/off/color/color-temp/onoff on-off read と group の
/// onoff 引数なし on/off/toggle・color・color-temp のみ（他は chip-tool 直）。
/// matd 稼働中は matd 自動発見が優先される。
#[arg(long, global = true, env = "MAT_IFACE", value_name = "IFACE")]
pub iface: Option<String>,

/// native 直経路が読む KVS fabric テーブルの index。
#[arg(long, global = true, env = "MAT_FABRIC_INDEX", default_value_t = 1, value_name = "N")]
pub fabric_index: u8,

/// native 直経路の CA issuer index。
#[arg(long, global = true, env = "MAT_ISSUER_INDEX", default_value_t = 0, value_name = "N")]
pub issuer_index: u8,
```

- [ ] **Step 3: classify の失敗するテストを書く**

`native_direct.rs` を新規作成し、まず shape 判定の unit テスト（`mod tests`）:

```rust
#[test]
fn on_off_read_onoff_and_color_shapes_are_native() {
    use mat_core::alias::{EndpointRef, NodeRef};
    let on = Command::On { node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1) };
    assert!(matches!(classify(&on), Some(NativeOp::On { node_id: 5, endpoint: 1 })));
    let read = Command::Read {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "onoff".into(), attribute: "on-off".into(),
    };
    assert!(matches!(classify(&read), Some(NativeOp::ReadOnOff { .. })));
    // 汎用 read（onoff on-off 以外）は非対象 —— matd の is_native_hotpath とパリティ。
    let other = Command::Read {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "levelcontrol".into(), attribute: "current-level".into(),
    };
    assert!(classify(&other).is_none());
    // discover / describe / write / diag 等は非対象。
    assert!(classify(&Command::Discover { probe: false }).is_none());
}
```

（Color/ColorTemp/Group 形のテストも同型で 1 本ずつ。Group は Task 5 で追加。）

Run: `cargo test -p mat native_direct`
Expected: FAIL（コンパイルエラー: classify 未定義）

- [ ] **Step 4: native_direct 実装（unicast）**

```rust
//! one-shot 直経路の native 実行（M7）。
//!
//! matd 稼働中は matd が優先（main.rs の経路順）。ここに来るのは直経路のみで、
//! `MAT_IFACE` 設定時に native 対象 op を mat-controller で in-process 実行する。
//! warm セッションは持たない: 確立 → 1 op → 破棄（設計ルール 4）。matd と違い
//! Timeout 再確立はしない（確立直後の session が stale なことはない）。
//! エンジン構築失敗（KVS 不備等）と group native 不可は warn を出して
//! chip-tool 直へフォールバック（matd の起動時フォールバックと同型）。

use std::path::Path;

use mat_core::error::MatError;
use mat_core::store::Store;
use mat_native::{Engine, NativeConfig};

use crate::cli::Command;

pub(crate) struct Config<'a> {
    pub iface: &'a str,
    pub fabric_index: u8,
    pub issuer_index: u8,
}

/// native 対象 op の分類（matd の is_native_hotpath / native_group_params と対）。
#[derive(Debug)]
pub(crate) enum NativeOp {
    On { node_id: u64, endpoint: u16 },
    Off { node_id: u64, endpoint: u16 },
    ReadOnOff { node_id: u64, endpoint: u16 },
    Color { node_id: u64, endpoint: u16, color: mat_core::color::ResolvedColor, transition: u16 },
    ColorTemp { node_id: u64, endpoint: u16, kelvin: u32, mireds: u16, transition: u16 },
    // Group 3 形は Task 5 で追加。
}

pub(crate) fn classify(command: &Command) -> Option<NativeOp> {
    match command {
        Command::On { node_id, endpoint } => Some(NativeOp::On { node_id: node_id.id(), endpoint: endpoint.id() }),
        Command::Off { node_id, endpoint } => Some(NativeOp::Off { node_id: node_id.id(), endpoint: endpoint.id() }),
        Command::Read { node_id, endpoint, cluster, attribute }
            if cluster == "onoff" && attribute == "on-off" =>
        {
            Some(NativeOp::ReadOnOff { node_id: node_id.id(), endpoint: endpoint.id() })
        }
        Command::ColorTemp { node_id, endpoint, kelvin, mireds, transition } => {
            let (mireds, kelvin) = crate::commands::invoke::resolve_color_temp(*kelvin, *mireds);
            Some(NativeOp::ColorTemp { node_id: node_id.id(), endpoint: endpoint.id(), kelvin, mireds, transition: *transition })
        }
        Command::Color { node_id, endpoint, spec, transition } => {
            // 不正 color spec はここで None → chip-tool 経路が同一エラーを出す
            // （resolve は決定的なので挙動は一致する）。
            let c = mat_core::color::resolve_spec(spec.name.as_deref(), spec.rgb.as_deref(), spec.hue, spec.sat).ok()?;
            Some(NativeOp::Color { node_id: node_id.id(), endpoint: endpoint.id(), color: c, transition: *transition })
        }
        _ => None,
    }
}

enum Executed {
    Done,
    Fallback,
}

/// 直経路 native の入口。None = chip-tool 直で実行すべき
/// （非対象 op / エンジン構築不可 / group native 不可）。
pub(crate) fn try_run(command: &Command, store_path: &Path, cfg: &Config) -> Option<Result<(), MatError>> {
    let op = classify(command)?;
    match execute(&op, store_path, cfg) {
        Ok(Executed::Done) => Some(Ok(())),
        Ok(Executed::Fallback) => None,
        Err(e) => Some(Err(e)),
    }
}

fn execute(op: &NativeOp, store_path: &Path, cfg: &Config) -> Result<Executed, MatError> {
    // store / commission チェックは chip-tool 経路と同一の順序・エラー（exit 10/11）。
    let store = Store::open(store_path)?;
    let node_id = match op {
        NativeOp::On { node_id, .. }
        | NativeOp::Off { node_id, .. }
        | NativeOp::ReadOnOff { node_id, .. }
        | NativeOp::Color { node_id, .. }
        | NativeOp::ColorTemp { node_id, .. } => Some(*node_id),
    };
    if let Some(id) = node_id {
        store.require_node(id)?;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MatError::new(mat_core::error::ErrorKind::Other, format!("tokio runtime: {e}")))?;
    rt.block_on(async {
        let native_cfg = NativeConfig {
            store: store.root().to_path_buf(),
            iface: cfg.iface.to_string(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = match Engine::build(&native_cfg).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e.detail, "native direct build failed; falling back to chip-tool");
                return Ok(Executed::Fallback);
            }
        };
        run_op(&engine, op).await?;
        Ok(Executed::Done)
    })
}

/// 確立 → 1 op → 破棄。値を返す op（read）は emit まで行う。
async fn run_op(engine: &Engine, op: &NativeOp) -> Result<(), MatError> {
    use mat_controller::im;
    match op {
        NativeOp::On { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(*endpoint, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None).await?;
            crate::commands::invoke::emit_invoke_success(*node_id, *endpoint, "onoff", "on");
        }
        NativeOp::Off { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(*endpoint, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_OFF, None).await?;
            crate::commands::invoke::emit_invoke_success(*node_id, *endpoint, "onoff", "off");
        }
        NativeOp::ReadOnOff { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            let v = conn.read_onoff(*endpoint).await?;
            crate::commands::read::emit_read_success(*node_id, *endpoint, "onoff", "on-off", serde_json::json!(v));
        }
        NativeOp::Color { node_id, endpoint, color, transition } => {
            let fields = im::encode_move_to_hue_and_saturation_fields(color.hue_raw, color.sat_raw, *transition);
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(*endpoint, im::CLUSTER_COLOR_CONTROL, im::CMD_MOVE_TO_HUE_AND_SATURATION, Some(fields)).await?;
            crate::commands::invoke::emit_color_success(*node_id, *endpoint, color, *transition);
        }
        NativeOp::ColorTemp { node_id, endpoint, kelvin, mireds, transition } => {
            let fields = im::encode_move_to_color_temperature_fields(*mireds, *transition);
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(*endpoint, im::CLUSTER_COLOR_CONTROL, im::CMD_MOVE_TO_COLOR_TEMPERATURE, Some(fields)).await?;
            crate::commands::invoke::emit_color_temp_success(*node_id, *endpoint, *kelvin, *mireds, *transition);
        }
    }
    Ok(())
}
```

`Cargo.toml`（mat）: `[dependencies]` に `mat-native.workspace = true`、
`mat-controller.workspace = true`、`tokio = { version = "1", features = ["rt", "net", "time"] }`、
`[dev-dependencies]` に `mat-native = { workspace = true, features = ["test-support"] }` と
`tokio = { version = "1", features = ["macros", "rt"] }` を追加。

`main.rs`（`mod native_direct;` 追加、matd 経路 match の直後）:

```rust
// native 直経路（M7）: MAT_IFACE 設定時、対象 op なら mat-controller で
// in-process 実行。None は chip-tool 直へフォールスルー。
if let Some(iface) = &args.iface {
    let cfg = native_direct::Config {
        iface,
        fabric_index: args.fabric_index,
        issuer_index: args.issuer_index,
    };
    if let Some(result) = native_direct::try_run(&command, &store_path, &cfg) {
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::debug!(kind = ?e.kind, detail = %e.detail, "native direct failed");
                e.emit();
                ExitCode::from(e.kind.exit_code())
            }
        };
    }
}
```

- [ ] **Step 5: one-shot 実行セマンティクスの unit テスト**

`native_direct.rs` の tests に（`FakeEstablisher` は `mat_native::test_support`）:

```rust
#[tokio::test]
async fn one_shot_does_not_retry_on_timeout() {
    use mat_core::error::ErrorKind;
    use mat_native::test_support::FakeEstablisher;
    use std::sync::atomic::Ordering;
    // 確立直後の送信 Timeout: one-shot は再確立せずそのまま返す（matd と違い
    // stale session はあり得ないため。chip-tool one-shot の失敗と同じ扱い）。
    let est = FakeEstablisher {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_first_send: true,
        fail_kind: ErrorKind::Timeout,
    };
    let calls = std::sync::Arc::clone(&est.calls);
    let engine = mat_native::Engine::with_parts(Box::new(est), None);
    let err = run_op(&engine, &NativeOp::ReadOnOff { node_id: 5, endpoint: 1 })
        .await
        .expect_err("timeout must surface");
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn one_shot_invoke_succeeds_via_engine() {
    use mat_native::test_support::FakeEstablisher;
    let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
    run_op(&engine, &NativeOp::On { node_id: 5, endpoint: 1 }).await.unwrap();
}
```

（注: `run_op` は emit で stdout に JSON を出す。テストバイナリの stdout 汚染は
cargo test では無害だが、`ReadOnOff` 成功系のような emit を伴う成功テストは
CLI 統合側の実機 E2E に委ね、unit はエラー分岐中心にする。）

- [ ] **Step 6: フォールバックの CLI 統合テスト**

`crates/mat/tests/integration.rs` に追加（既存の `mat(store)` / `store_with_node5()`
ヘルパをそのまま使う）:

```rust
#[test]
fn native_iface_without_kvs_falls_back_to_chip_tool() {
    // MAT_IFACE を立てても KVS（chip_tool_config.*.ini）が無ければ warn +
    // chip-tool 直へフォールバックし、既存どおり成功する。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_IFACE", "lo")
        .env("MAT_LOG", "warn")
        .args(["on", "--node", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""))
        .stdout(predicate::str::contains("\"command\":\"on\""))
        .stdout(predicate::str::contains("\"status\":\"success\""))
        .stderr(predicate::str::contains("falling back to chip-tool"));
}
```

（既存テスト自体は触らない — `MAT_IFACE` 未設定の全既存テストが
「未設定=挙動不変」の証明になる。）

- [ ] **Step 7: テスト・チェック**

Run: `cargo test -p mat` → PASS
Run: `task check` → PASS

- [ ] **Step 8: Commit**

```bash
git add crates/mat Cargo.lock
git commit -m "feat(mat): one-shot直経路のnative化 — MAT_IFACE配線とunicastホットパス (M7 Task4)"
```

---

### Task 5: mat 直経路 native — group 3 形とフォールバック

**Files:**
- Modify: `crates/mat/src/native_direct.rs`（NativeOp に Group 3 形を追加）
- Modify: `crates/mat/src/commands/group.rs`（emit ヘルパ抽出）
- Test: `crates/mat/src/native_direct.rs` + `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: `mat_native::group::{send, GroupOutcome}`、Task 4 の `classify`/`execute` 骨格、`mat_controller::im::{CLUSTER_ON_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_OFF, CMD_ON_OFF_TOGGLE, CLUSTER_COLOR_CONTROL, ...}`
- Produces: `commands::group::emit_invoke_sent(group_id, cluster: &str, command: &str, endpoint)` / `emit_color_temp_sent(group_id, kelvin, mireds, transition, endpoint)` / `emit_color_sent(group_id, color, transition, endpoint)`（既存 invoke/color_temp/color の emit 部と同一 JSON）

- [ ] **Step 1: emit ヘルパ抽出（挙動不変）**

`group.rs` の `invoke` / `color_temp` / `color` の `output::emit(...)` 部を
`pub(crate) fn emit_invoke_sent(...)` 等に切り出し、既存関数から呼ぶ。
JSON（`status: "sent"` と `note` を含む）は 1 フィールドも変えない。

Run: `cargo test -p mat` → PASS（無改変）

- [ ] **Step 2: classify 拡張（失敗するテストから）**

matd の `native_group_params`（`crates/matd/src/server.rs:268` 付近）と完全パリティ:
`GroupInvoke` は `cluster == "onoff" && args.is_empty()` かつ command が
on/off/toggle のときのみ。`GroupColor` / `GroupColorTemp` は常に native 対象。

tests 追加:

```rust
#[test]
fn group_onoff_no_args_is_native_but_generic_group_invoke_is_not() {
    use mat_core::alias::GroupRef;
    let native = Command::Group { action: GroupCommand::Invoke {
        group_id: GroupRef::Id(10), cluster: "onoff".into(), command: "toggle".into(),
        args: vec![], endpoint: 1,
    }};
    assert!(matches!(classify(&native), Some(NativeOp::GroupOnOff { group_id: 10, .. })));
    // 引数付き / onoff 以外は chip-tool へ（matd と同じ counter 混在 warn 対象外の形）。
    let generic = Command::Group { action: GroupCommand::Invoke {
        group_id: GroupRef::Id(10), cluster: "levelcontrol".into(),
        command: "move-to-level".into(), args: vec!["128".into()], endpoint: 1,
    }};
    assert!(classify(&generic).is_none());
    // provision / grant は常に chip-tool 直。
    let grant = Command::Group { action: GroupCommand::Grant {
        group_id: GroupRef::Id(10), node_ids: vec![],
    }};
    assert!(classify(&grant).is_none());
}
```

Run: `cargo test -p mat native_direct` → FAIL（GroupOnOff 未定義）

- [ ] **Step 3: 実装**

`NativeOp` に追加:

```rust
GroupOnOff { group_id: u16, command_id: u32, command: &'static str, endpoint: u16 },
GroupColor { group_id: u16, color: mat_core::color::ResolvedColor, transition: u16, endpoint: u16 },
GroupColorTemp { group_id: u16, kelvin: u32, mireds: u16, transition: u16, endpoint: u16 },
```

`classify` の `Command::Group` 腕（`GroupCommand::Invoke` の command 写像は
`"on" => (im::CMD_ON_OFF_ON, "on")` / `"off"` / `"toggle"`、他は `None`。
ColorTemp/Color は unicast と同じ resolve）。

`execute` 側: group 3 形は `require_node` をしない（chip-tool 経路の `send` と同じ —
`Store::open` のみ）。`run_op` に group 腕を追加:

```rust
NativeOp::GroupOnOff { group_id, command_id, command, endpoint } => {
    let Some(ctx) = &engine.group else {
        tracing::warn!("native group context not configured; falling back to chip-tool");
        return Ok(RunOutcome::Fallback);
    };
    match mat_native::group::send(ctx, *group_id, im::CLUSTER_ON_OFF, *command_id, None).await? {
        GroupOutcome::Sent => {
            crate::commands::group::emit_invoke_sent(*group_id, "onoff", command, *endpoint);
        }
        GroupOutcome::Unavailable(reason) => {
            tracing::warn!(group_id, reason, "native group send unavailable; falling back to chip-tool");
            return Ok(RunOutcome::Fallback);
        }
    }
}
// GroupColor / GroupColorTemp も同型（fields encoder + emit_color_sent / emit_color_temp_sent）
```

注意: これに伴い `run_op` の戻りを `Result<RunOutcome, MatError>`
（`RunOutcome { Done, Fallback }`）に変え、Task 4 の unicast 腕は `Done` を返す。
`execute` は `run_op` の `Fallback` を `Executed::Fallback` として返す。
Task 4 の unit テストの期待値もこの型に合わせて更新する（アサート内容は不変）。

- [ ] **Step 4: フォールバック統合テスト**

`integration.rs` に追加（fixture ini なし store → エンジン構築失敗で
chip-tool フォールバック）:

```rust
#[test]
fn native_iface_group_send_falls_back_without_kvs() {
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_IFACE", "lo")
        .env("MAT_LOG", "warn")
        .args(["group", "invoke", "--group", "10", "--cluster", "onoff", "--command", "toggle"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"sent\""))
        .stderr(predicate::str::contains("falling back to chip-tool"));
}
```

（`group invoke` の実引数形は既存の group テストに合わせて調整する —
まず `grep -n '"group"' crates/mat/tests/integration.rs` で既存の呼び形を確認。
counter flock 競合の Unavailable 化は Task 1 の unit テストと
`mat-native::group::send` 経由の挙動（load エラー→Unavailable）で担保済み —
CLI 層での再テストはしない。）

- [ ] **Step 5: テスト・チェック**

Run: `cargo test -p mat && task check` → PASS

- [ ] **Step 6: Commit**

```bash
git add crates/mat
git commit -m "feat(mat): 直経路nativeのgroup送信3形（onoff無引数/color/color-temp）とフォールバック (M7 Task5)"
```

---

### Task 6: fix-later 回収（M6 持ち越し 2 件）

**Files:**
- Modify: `crates/mat-controller/src/commissioning.rs`（doc 表 + 境界テスト）

**Interfaces:** なし（doc とテストのみ、プロダクションコード変更なし）

- [ ] **Step 1: ErrorKind 写像 doc 表を M6b spec 決定 4 に追従**

`commissioning.rs:1332` 付近の `CommissionError` doc 表に M6b で増えた variant の
行を追加する（M6b spec `2026-07-13-phase5-m6b-ble-thread-commissioning-design.md`
決定 4 の写像）。まず `CommissionError::Ble` の構築箇所を
`grep -n 'CommissionError::Ble' crates/mat-controller/src/` で列挙し、
scan（発見 timeout）に使われている step 名を確認してから書く:

```
/// | `Ble`（scan = 発見 timeout の step）                  | `timeout`          | 3    |
/// | `Ble`（それ以外: bluez-session / adapter / gatt /     | `unreachable`      | 5    |
/// |   btp-handshake / udp-bind 等の接続系）                |                    |      |
/// | `NetworkConfig { .. }`                                | `commission_failed`| 1    |
```

表頭の「spec 決定 5」参照を「M6a spec 決定 5 + M6b spec 決定 4」に改める。

- [ ] **Step 2: thread_ext_pan_id 境界テスト追加**

既存 `thread_dataset_ext_pan_id_extracts_type2` の隣に:

```rust
#[test]
fn thread_ext_pan_id_boundary_cases() {
    // type 2 だが長さ != 8 は読み飛ばし、後続の正しい type 2 を拾う。
    let mut ds = vec![0x02, 0x04, 0xAA, 0xBB, 0xCC, 0xDD];
    ds.extend_from_slice(&[0x02, 0x08, 1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(thread_ext_pan_id(&ds), Some([1, 2, 3, 4, 5, 6, 7, 8]));
    // TLV がバッファ末尾ちょうどで終わる正常形。
    let exact = [0x00, 0x01, 0xFF, 0x02, 0x08, 8, 7, 6, 5, 4, 3, 2, 1];
    assert_eq!(thread_ext_pan_id(&exact), Some([8, 7, 6, 5, 4, 3, 2, 1]));
    // 長さが残りを超える壊れ TLV は None（panic しない）。
    assert_eq!(thread_ext_pan_id(&[0x00, 0xFF, 0x01]), None);
    // 空 / ヘッダ未満。
    assert_eq!(thread_ext_pan_id(&[]), None);
    assert_eq!(thread_ext_pan_id(&[0x02]), None);
    // 長さ 0 の TLV を挟んでも走査が止まらない。
    let with_zero = [0x03, 0x00, 0x02, 0x08, 1, 1, 2, 2, 3, 3, 4, 4];
    assert_eq!(thread_ext_pan_id(&with_zero), Some([1, 1, 2, 2, 3, 3, 4, 4]));
}
```

- [ ] **Step 3: テスト・チェック・コミット**

Run: `cargo test -p mat-controller thread_ext_pan && task check` → PASS

```bash
git add crates/mat-controller/src/commissioning.rs
git commit -m "docs+test(controller): ErrorKind写像表をM6b決定4に追従、thread_ext_pan_id境界テスト (M7 Task6)"
```

---

### Task 7: ドキュメントとバージョン 0.17.0

**Files:**
- Modify: `Cargo.toml`（version 0.16.0 → 0.17.0）+ `Cargo.lock`（`cargo check` で更新）
- Modify: `README.md`
- Modify: `ARCHITECTURE.md`
- Modify: `CLAUDE.md`

**Interfaces:** なし

- [ ] **Step 1: version bump**

`[workspace.package] version = "0.17.0"`。`cargo check` で lock 更新。

- [ ] **Step 2: README**

- `MAT_IFACE` / `MAT_FABRIC_INDEX` / `MAT_ISSUER_INDEX`（グローバル `--iface` 等）の
  説明を環境変数の節に追加: opt-in、対象 op 一覧、未設定=従来どおり、
  matd 稼働中は matd 優先、エンジン構築失敗と group native 不可は chip-tool へ
  自動フォールバック（stderr に warn）。
- group counter の注意の節を更新: native 送信の counter は
  `<store>/native_group_counter` を one-shot / matd で flock 共有し、
  保持中の相手がいれば chip-tool フォールバックになる。
  「matd 稼働中の `MAT_MATD=0` 強制直 group 送信は禁止」の既存注記は維持
  （chip-tool フォールバック側の counter 混在は従来どおり残るため）。

- [ ] **Step 3: ARCHITECTURE.md**

- Phase 5 節に M7 の記録を追記: 決定（共有 crate `mat-native` 抽出 /
  `MAT_IFACE` opt-in / counter flock / main マージ解禁）と受け入れ基準の要約。
- 親 spec 由来の未決事項リストを更新: 「mat 直経路の載せ替え時期」= M7 で解決、
  「KVS フォーマット互換の保証範囲」= M8（chip-tool 完全廃止）で扱う、と明記。
- crate 一覧に `mat-native` を追加（mat/matd と mat-controller の間の共有エンジン層）。

- [ ] **Step 4: CLAUDE.md**

Backend 節に経路優先順位の一文を追記:
「実行経路は op 単位で matd 自動発見 → native 直（`MAT_IFACE` 設定時の対象 op のみ）→
chip-tool 直の順」。設計ルール 1 の文言が `mat-controller` のみを指している場合は
「backend crates（mat-controller / mat-native）」に広げる。

- [ ] **Step 5: チェック・コミット**

Run: `task check` → PASS

```bash
git add Cargo.toml Cargo.lock README.md ARCHITECTURE.md CLAUDE.md
git commit -m "docs: M7ドキュメント反映（MAT_IFACE直経路native/counter共有）と0.17.0 (M7 Task7)"
```

---

### Task 8: 実機 E2E ハーネス `task e2e:m7`

**Files:**
- Create: `scripts/e2e-m7.sh`
- Modify: `Taskfile.yml`（`e2e:m7` タスク追加 — `e2e:m5` の並びに同型で）

**Interfaces:**
- Consumes: Task 1–5 の全成果（クロスビルドした mat / matd バイナリ）
- Produces: spec 受け入れ基準 1〜3 を検証する半自動ハーネス（配達の目視確認は人間）

- [ ] **Step 1: スクリプト作成**

`scripts/e2e-m5.sh` の骨格（env 規約 / クロスビルド + `file` での arch 確認 /
scp 転送 / 本番 matd 非接触の警告コメント）を踏襲して `scripts/e2e-m7.sh` を書く。

```
必須 env: MAT_E2E_HOST / MAT_E2E_IFACE / MAT_E2E_NODE_ID（unicast 対象）
          MAT_E2E_GROUP_NODES（グループメンバー csv、目視確認の案内用）
任意 env: MAT_E2E_GROUP_ID（既定 10）/ MAT_E2E_ENDPOINT（既定 1）
          MAT_E2E_FABRIC_INDEX（既定 2）/ MAT_E2E_STORE / MAT_E2E_SOCKET（既定 /tmp/matd-m7.sock）
          MAT_E2E_CHIP_TOOL_BIN
```

フェーズ構成（各フェーズで停止して人間に配達目視を促す echo を入れる）:

1. **build+deploy**: `mat` と `matd` を aarch64-musl でクロスビルド
   （M5 と同じ rust-lld フラグ）、`file` で arch 検証、ssh 先の一時 dir へ scp。
2. **one-shot 直 native（受け入れ 1）**: ssh 先で `MAT_MATD=0 MAT_IFACE=$IFACE
   MAT_FABRIC_INDEX=$FABRIC_INDEX` を付けて一時 mat バイナリを実行:
   `read $NODE onoff on-off` → `on` → `read`（true 確認）→ `off` → `read`（false 確認）→
   `color $NODE red` → `color-temp $NODE --kelvin 2700` →
   `group off $GROUP` 相当（`group invoke $GROUP onoff off`）→ 目視 N/N 消灯 →
   `group invoke $GROUP onoff on` → 目視 → `group color-temp $GROUP --kelvin 2700` → 目視。
   各コマンドの stdout JSON に期待フィールド（`"value":true` 等）を grep で検証。
   stderr に "falling back" が**出ていない**ことも grep -v で確認（native で走った証明）。
3. **counter 共有（受け入れ 2）**: 一時 matd を native 有効
   （`--iface $IFACE --fabric-index $FABRIC_INDEX --socket $SOCKET --port 9110`）で
   nohup 起動 → `mat --matd $SOCKET group invoke $GROUP onoff off` → 目視 N/N →
   on に戻す → `matd stop --socket $SOCKET`。
   （one-shot が進めた counter ファイルを matd が jump-ahead で跨ぐ実証。）
4. **フォールバック（受け入れ 3）**: `MAT_MATD=0 MAT_IFACE=$IFACE` のまま
   `describe $NODE` と `diag thread $NODE` を実行 → chip-tool 経由で成功
   （native 対象外 op が壊れていない）。
5. cleanup: ssh 先の一時ファイル削除。

警告コメント（M5 と同じ）: E2E 中の group 送信で本番 matd（chip-tool）の
group 送信は counter 追い越しにより当面 drop する。直後に Task 9 の本番
native 化を行う前提で実行すること。

- [ ] **Step 2: Taskfile 追加**

`e2e:m5` の隣に:

```yaml
e2e:m7:
  desc: "M7 acceptance: one-shot native direct + counter sharing (jarvis)"
  cmds:
    - bash scripts/e2e-m7.sh
```

- [ ] **Step 3: 静的チェック・コミット**

Run: `bash -n scripts/e2e-m7.sh && task check` → PASS

```bash
git add scripts/e2e-m7.sh Taskfile.yml
git commit -m "test(e2e): M7実機ハーネス（one-shot native直経路 + counter共有） (M7 Task8)"
```

---

### Task 9（実機、人間と協働）: E2E → main マージ → 本番デプロイ

このタスクはユーザーと対話しながらメインセッションで実行する
（サブエージェントに委譲しない）。

- [ ] **Step 1: 実機 E2E**

Run: `MAT_E2E_HOST=<jarvis> MAT_E2E_IFACE=eth0 MAT_E2E_NODE_ID=<node> MAT_E2E_GROUP_NODES=<csv> task e2e:m7`
Expected: 受け入れ基準 1〜3 全通過（配達目視はユーザーに依頼）。
失敗したら systematic-debugging で根因を潰してから再走（部分成功で先に進まない）。

- [ ] **Step 2: main マージ（マージ解禁の実行）**

superpowers:finishing-a-development-branch を起動し、matter-controller → main の
統合方式（merge --no-ff / squash 等）をユーザーに確認して実行。
マージ後 main で `task check` を再実行して確認。

- [ ] **Step 3: 本番デプロイ（受け入れ 4〜5）**

1. main から aarch64-musl で mat / matd をクロスビルド（rust-lld フラグ、`file` 検証）。
2. jarvis へ scp、`~/.cargo/bin`（現行の配置先）へ差し替え。
3. systemd unit に `Environment=MAT_MATD_IFACE=eth0` と
   `Environment=MAT_MATD_FABRIC_INDEX=2` を追加（`sudo systemctl edit matd` の
   drop-in か unit 直編集はユーザーと確認）→ `sudo systemctl daemon-reload &&
   sudo systemctl restart matd`。
4. `journalctl -u matd -n 20` で "native backend enabled" を確認。
5. 本番受け入れ: warm unicast（on/off/color/color-temp、2 回目以降 ~100ms 台）、
   `mat group invoke 10 onoff off` → 7/7 目視 → on 復帰、describe / diag が
   chip-tool フォールバックで成功。
6. ロールバック手順の確認だけ行う（実行はしない）: env 2 行を外して restart で
   全 op chip-tool に戻る。

- [ ] **Step 4: 記録**

- spec（M7 design doc）に実行時の訂正・実機知見があれば追記コミット。
- メモリ `phase5-backend-research.md` と `MEMORY.md` を更新
  （M7 完了、本番 native 化、main マージ解禁の実施、次 = M8）。
- `jarvis-matd-deploy.md` に unit の env 追加を反映。

---

## Self-Review 済み事項

- spec 決定 1〜4・受け入れ基準 1〜5・fix-later 2 件・ドキュメント変更・スコープ外の
  各項目にタスクを対応付けた（決定 1→Task 2/3、決定 2→Task 4/5、決定 3→Task 1、
  決定 4→Task 9、受け入れ 1〜3→Task 8/9、4〜5→Task 9、fix-later→Task 6、
  docs→Task 7）。
- 型整合: `Engine::with_parts` / `group::send` / `GroupOutcome` /
  `emit_*` ヘルパの名前・引数は Task 2/3/4/5 間で一致させた。Task 5 で
  `run_op` の戻り型を `RunOutcome` に変える差分は Task 4 のテスト更新込みで明記。
- one-shot は Timeout 再確立をしない（matd との意図的な差 — 確立直後に stale は
  ない）ことを native_direct doc とテストの両方に固定した。
