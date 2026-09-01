# op 単一ソース化 — 設計 spec（監査④、2026-09-02）

## 目的

1 op を追加するたびに `mat` / `matd` の両側で TLV 符号化と成功 body 組立を書き直す
現状（2026-08-31 監査④「op サーフェス多重表現」）を解消する。**op → TLV → body**
を `mat-native` に 1 箇所だけ置き、`mat`（one-shot 直経路）と `matd`（warm
セッション）は「セッションの取り方」だけを差し替える。

数値目標: 実コード -700 行前後（測定は `task check` 緑後に `git diff --stat`）。

## 現状（問題の実体）

| 層 | 場所 | 内容 |
|---|---|---|
| CLI→op | `mat/native_direct.rs` `classify` / `classify_strict` | `Command`→`NativeOp` の 2 段 match（2 段構造は chip-tool fallback の遺物） |
| CLI→wire | `mat/matd_client.rs` `to_op` | `Command`→JSON 手書き |
| 実行(直) | `mat/native_direct.rs` `op_*` 20 本 + `run_op` + `execute` の node_id match + `budget_applies` | establish→TLV→body→close |
| 実行(matd) | `matd/native.rs` op 別メソッド 9 本、`matd/server.rs` `is_native_hotpath` / `native_op` / `native_group_params` / `group_provision` | with_session→TLV→body |
| 予算対象 | `NativeOp::budget_applies` と `matd_client::attach_deadline`（JSON の `node_id` キー有無） | 同じ判定を別々に |

TLV 符号化（`encode_move_to_level_fields` 等）と body 組立（`body::level_success`
等）は直経路と matd で丸ごと二重。名前解決（`classify_write` / `classify_invoke`）
は matd 側で `is_native_hotpath` と `native_op` の 2 回走る。

## 設計

### 1. `mat_native::op` — 解決済み op と実行の単一ソース（新モジュール）

```rust
/// 単一ノード宛 op。値はすべて解決済み（ID・raw 値・エコー用の入力文字列）。
#[derive(Debug, Clone)]
pub struct NodeOp { pub node_id: u64, pub kind: NodeOpKind }

#[derive(Debug, Clone)]
pub enum NodeOpKind {
    On { endpoint: u16 },
    Off { endpoint: u16 },
    Color { endpoint: u16, color: ResolvedColor, transition: u16 },
    ColorTemp { endpoint: u16, kelvin: u32, mireds: u16, transition: u16 },
    Level { endpoint: u16, percent: u8, level: u8, transition: u16 },
    Read { endpoint: u16, cluster_in: String, attribute_in: String, cluster: u32, attribute: u32 },
    Write { endpoint: u16, cluster_in: String, attribute_in: String, cluster: u32, attribute: u32,
            value_in: String, value: ScalarValue, timed: bool },
    Invoke { endpoint: u16, cluster_in: String, command_in: String, cluster: u32, command: u32,
             fields_tlv: Option<Vec<u8>>, timed: bool },
    Describe,
    DiagThread { endpoint: u16 },
    OpenWindow { timeout: u32, iteration: u32, discriminator: u16 },
}

/// groupcast op（unacknowledged、"sent" のみ報告）。
#[derive(Debug, Clone)]
pub struct GroupOp { pub group_id: u16, pub endpoint: u16, pub kind: GroupOpKind }

#[derive(Debug, Clone)]
pub enum GroupOpKind {
    Invoke { cluster_in: String, command_in: String, cluster: u32, command: u32, fields_tlv: Option<Vec<u8>> },
    Color { color: ResolvedColor, transition: u16 },
    ColorTemp { kelvin: u32, mireds: u16, transition: u16 },
    Level { percent: u8, level: u8, transition: u16 },
}

/// group provision の入力（直経路・matd 共通）。
#[derive(Debug, Clone)]
pub struct ProvisionParams { pub group_id: u16, pub node_ids: Vec<u64>, pub keyset_id: u16,
    pub name: String, pub endpoint: u16, pub epoch_key: Option<String>, pub rebind: bool }
```

**名前解決・換算のコンストラクタ**（mat / matd が同じ規則を共有する唯一の入口）:

- `NodeOpKind::read(endpoint, cluster_in, attribute_in) -> Result<Self, MatError>` —
  `ids::resolve_cluster` + `resolve_attribute`。未解決は `MatError::unresolved_op()`。
- `NodeOpKind::write(endpoint, cluster_in, attribute_in, value_in)` —
  `ids::classify_write`。`NotNative` → `unresolved_op`、`Reject(msg)` → `parse_error(msg)`。
- `NodeOpKind::invoke(endpoint, cluster_in, command_in, args)` — `ids::classify_invoke`
  同上。`fields` 非空なら `encode_command_fields` で TLV 化。
- `GroupOpKind::invoke(cluster_in, command_in, args)` — 同上（timed は捨てる）。
  現行 `classify` の `GroupOnOff` 高速路（onoff の引数なし on/off/toggle）は
  `classify_invoke("onoff", ...)` と同じ ID に落ちるため **廃止して `Invoke` に統合**。
- `NodeOpKind::color_temp(endpoint, kelvin: Option<u32>, mireds: Option<u16>, transition)` /
  `level(endpoint, percent, transition)` と group 版 — 現 `mat/units.rs` の
  `resolve_color_temp` / `resolve_level` をこのモジュールへ移設（`mat/units.rs` は削除）。
  matd は換算済み値を wire から受けるので struct リテラルで直接組む。
- color は `mat_core::color::resolve_spec` の結果（`ResolvedColor`）をそのまま持つ。

**実行（op → TLV → body の唯一の場所）**:

```rust
pub async fn run_node_op(conn: &mut dyn NodeConn, op: &NodeOp) -> Result<Value, MatError>;
pub async fn run_group_op(engine: &Engine, op: &GroupOp) -> Result<Value, MatError>;
pub async fn run_group_bump(engine: &Engine) -> Result<Value, MatError>;
```

- `run_node_op` は `NodeOpKind` の網羅 match 1 本。各腕が `NodeConn` 呼び出し
  （`invoke` / `read_onoff` / `read_json` / `write_tlv` / `open_window` /
  `ops::describe` / `ops::diag_thread`）と `mat_core::body::*` の組立を行う。
  - `Read` は `cluster == CLUSTER_ON_OFF && attribute == ATTR_ON_OFF` のとき
    `read_onoff`（bool）、それ以外は `read_json`。現行の両経路の挙動を保つ
    （数値 ID `6`/`0` 指定も同じ腕に落ちるが JSON は `Bool` で同形）。
  - 直経路が持っていた `tracing::info!("… executed (native direct)")` は
    `run_node_op` 内の 1 行 `tracing::debug!(node_id, op = kind.name(), "node op executed")`
    に置き換え、経路タグは各バイナリの呼び出し側ログ（matd は `log_op`、mat は
    `main.rs` の既存 debug）に任せる。
- `run_group_op` は `engine.group` が `None` なら `group_ctx_unconfigured`、
  `GroupOutcome::Unavailable` は `group_unavailable`、`Sent { egress }` で
  `body::group_*_sent` を組む。`run_group_bump` も同型。
- `NodeOpKind::budget_applies(&self) -> bool`: `--op-timeout-ms` / matd `deadline_ms`
  の対象（On/Off/Color/ColorTemp/Level/Read/Write/Invoke/Describe = true、
  DiagThread/OpenWindow = false）。判定はここだけ。
- `NodeOpKind::name(&self) -> &'static str`（snake_case、ログ用）。

**body の移設**: 直経路専用だった `diag thread` / `open-window` / `group grant` の
成功 body を `mat_core::body` へ移す（`diag_thread_success` / `open_window_success` /
`group_grant_success`）。`mat/commands/{diag,open_window,group}.rs` の対応 `emit_*`
は削除し、`body.rs` 冒頭 doc の「直経路専用 op は mat 側に残す」記述を更新する。
`group provision` の note（直経路のみ付く）は `group_provision_success` の
`note: Option<&str>` 引数で呼び出し側が決める（現行どおり）。

### 2. `NodeRunner` — セッション取得戦略の差し替え点

```rust
/// 「node_id のセッションを取り、f を 1 回（matd は再確立後に再送で 2 回まで）
/// 呼ぶ」だけを抽象化する。
pub trait NodeRunner: Sync {
    async fn with_node<T, F>(&self, node_id: u64, deadline: Option<Instant>, f: F)
        -> Result<T, MatError>
    where
        T: Send,
        F: for<'a> Fn(&'a mut Box<dyn NodeConn>)
            -> Pin<Box<dyn Future<Output = Result<T, MatError>> + Send + 'a>> + Send + Sync;
}
```

シグネチャは現 `matd::native::NativeBackend::with_session` と同一（`Fn` なのは
matd の再送のため。closure 環境の借用は不可 — `provision_node` 既存 doc の
「値を async ブロックへ move」規律を踏襲）。ジェネリックなので静的ディスパッチ。

実装:

- **mat**: `native_direct::OneShotRunner<'e>(&'e Engine)` — `establish` → `f` →
  `close` → `matd_client::hint_node_touched(node_id)`（現 `finish_conn`）。
  `deadline` は無視（直経路の予算は `execute` が future 全体に `tokio::time::timeout`
  を掛ける現行方式のまま — Issue #22 の「timeout 時も hint だけ撃つ」を維持）。
- **matd**: `impl NodeRunner for NativeBackend` = 現 `with_session` の本体（warm
  slot・Timeout 1 回再確立・`RETRY_MIN_BUDGET`・`on_new_session` 発火は不変）。

`NodeRunner` の上に立つ共有関数（`mat_native::op`）:

```rust
pub async fn run_node(r: &impl NodeRunner, op: &NodeOp, deadline) -> Result<Value, MatError>;
pub async fn provision(r: &impl NodeRunner, engine: &Engine, p: &ProvisionParams, note: Option<&str>)
    -> Result<Value, MatError>;   // group_settings ctx 検査 → resolve_epoch_key → write_group_provision
                                  // → 各 node へ ops::provision_node（"node {id}: " 前置）→ body
pub async fn grant(r: &impl NodeRunner, group_id: u16, node_ids: &[u64]) -> Result<Value, MatError>;
```

`Store::require_node`（commission 済み確認、exit 11）は現行どおり呼び出し側
（mat は `execute` 冒頭、matd は `run_op`）— store の開き方が経路で違う（matd は
台帳更新を拾うため毎回開き直す）ので共有しない。

### 3. `mat` 側 — `Command` → `DeviceOp` の match 1 本

新ファイル `mat/src/device_op.rs`:

```rust
pub(crate) enum DeviceOp {
    Node(NodeOp),
    Group(GroupOp),
    GroupProvision(ProvisionParams),
    GroupGrant { group_id: u16, node_ids: Vec<u64> },
    GroupBump,
}

/// 専用コマンド層を持つ op（discover / commission / fabric / listen / diag node /
/// diag mesh）は `Dedicated(name)`。name は `--matd` 強制時の unsupported 文言用。
pub(crate) enum Dispatch { Device(DeviceOp), Dedicated(&'static str) }

pub(crate) fn classify(command: &Command) -> Result<Dispatch, MatError>;
```

- `classify` は `Command` の網羅 match（`_` 無し）。alias 未解決は `id()?` で
  内部エラー、名前未解決 / 値符号化不能 / 不正 color spec は `?` でそのまま伝播
  （現 `classify` → `classify_strict` → `unresolved_op_error` の 3 段を 1 段に）。
  `open-window` の `resolve_discriminator` はここで適用。`provision` の name 既定
  `grp<id>` もここ。
- `native_direct.rs` は `NativeOp` / `classify*` / `op_*` / `run_op` を全削除し、
  `run(&DeviceOp, ...)` = `Store::open` + `require_node` + engine build +
  `OneShotRunner` で `run_node` / `run_group_op` / `provision(note=Some(..))` /
  `grant` / `run_group_bump` を呼ぶだけにする。`emit` は `output::emit(body)` 1 箇所。
  `diag_im_probe` / `mesh_probe_one` 等の diag 補助はそのまま残す。
- `matd_client::to_op(&DeviceOp) -> Result<Value, ToOpError>`: `Node` は `kind`
  で wire JSON を組む（`cluster_in` 等の入力文字列を載せる — **wire は名前のまま**、
  契約不変）。`DiagThread` / `OpenWindow` / `GroupGrant` は `Unsupported`。
  `attach_deadline` は JSON の `node_id` キー探索をやめ
  `NodeOpKind::budget_applies()` を受け取る。
- `main.rs`: `resolve_command` の後に `classify` を 1 回呼び、`Dispatch::Device`
  なら経路解決（matd → 直）、`Dedicated(name)` なら **Forced 経路では現行どおり
  unsupported（exit 2）**、それ以外は専用コマンド層へ。Listen / Fabric の先取りは
  現行位置のまま。

**挙動差（意図的、1 点）**: matd 経路でも名前解決を mat 側で先に行うため、未知の
cluster / attribute / command 名は matd へ送る前に手元で `parse_error`（exit 1）に
なる。kind・exit code・error JSON の形は現行（matd が返す `unresolved_op`）と同一。

### 4. `matd` 側 — wire `Op` → op の match 1 本

- `protocol.rs` の `Op` とその helper（`node_id` / `name` / `group_id` / `endpoint` /
  `log_path`）は **ワイヤ契約なので不変**。`op_state_target` / `op_report_expectation`
  （born-dead 判定）も matd 固有なので不変。
- `server.rs` に `fn to_device_op(op: &Op) -> Result<MatdOp, MatError>` を置く
  （`MatdOp = Node(NodeOp) | Group(GroupOp) | Provision(ProvisionParams) | Bump`）。
  Ping / Shutdown / Listen / Status / NodeTouched は `run_op` 冒頭の既存早期 return
  が先取りするので、ここでは `parse_error("internal: … dispatch invariant violated")`。
  `is_native_hotpath` / `native_op` / `native_group_params` / `group_provision` /
  `SentBodyBuilder` は削除。
- `run_op`: `to_device_op` → `Node` は `require_node` → `run_node(native, &op, deadline)`
  → 成功時 `note_op_expectation(op, health)`（wire Op のまま）。`Group` は
  `Store::open` 前提チェック → `run_group_op(native.engine(), ..)`。`Provision` は
  `require_node` 全ノード → `provision(native, native.engine(), &p, None)`。`Bump` は
  `run_group_bump`。
- `native.rs`: op 別メソッド（`read_onoff` / `on` / `off` / `color` / `color_temp` /
  `level` / `read_json` / `write_tlv` / `invoke_generic` / `describe` /
  `provision_node` / `group_invoke` / `group_bump`）を削除し、`impl NodeRunner` と
  `pub fn engine(&self) -> &Engine` を足す。`establish_subscription` / `drop_session` /
  `group_settings_ctx` / `set_on_new_session` / `with_*` コンストラクタは不変。

### 5. 残すもの（変更しない）

- `cli.rs`（clap）と `resolve.rs`（alias 一括解決、網羅 match は設計意図）。
- `protocol.rs`（ワイヤ契約）。
- `mat-native::ops`（describe / diag_thread / provision_node / ensure_group_acl の
  純粋ロジック）— `op` モジュールから呼ぶ側に回るだけ。
- warm / cold のセッション戦略の分離（`native.rs:212` doc の意図）。

結果、1 op 追加で触る場所は cli / resolve / `device_op::classify` / `to_op` /
`protocol::Op`(+helper) / `to_device_op` / `NodeOpKind` + `run_node_op` の 7 箇所
（現状 ~15）。TLV と body は `run_node_op` / `run_group_op` の 1 箇所。

## エラー・exit code

現行の対応表を変えない。

| 事象 | kind | exit |
|---|---|---|
| 未知の cluster/attribute/command 名 | `parse_error`（`unresolved_op` 文言） | 1 |
| 値が符号化不能（list/struct 等） | `parse_error`（classify の msg） | 1 |
| 不正 color spec | `resolve_spec` 本来の kind | 現行どおり |
| 未 commission | `node_not_commissioned` | 11 |
| group ctx 未構成（テスト注入のみ） | `other` | 現行どおり |
| groupcast 不可（未 provision・KVS 不備） | `store_parse` | 10 |
| 予算超過 | `timeout` | 3 |
| `--matd` 強制 × 非対応 op | `other` | 2 |

## テスト

- **mat-native `op` ユニット**（`test_support::FakeConn`）: `run_node_op` 全腕の
  「呼ばれた NodeConn メソッド + 引数 TLV + 返る body」を釘打ち。名前解決
  コンストラクタの unresolved / reject / 数値 ID 受理。`budget_applies` の表。
  `provision` / `grant` のループ（`FakeEstablisher` + `OneShot` 相当の最小 runner）。
- **直経路 / matd の body 同形**は構造的に保証されるため個別テストは不要になる。
  既存の body スキーマテスト（`native_generic_read_body_matches_expected_schema` 等）
  は matd `run_op` 経由のまま残す（回帰の受け皿）。
- **mat**: `native_direct.rs` の classify 系テストは `device_op::classify` へ移植
  （期待値は `DeviceOp` 形）。`OneShotRunner` の close-on-success / close-on-failure /
  no-retry / deadline→hint テストは維持。`matd_client` の `to_op` golden テストは
  `DeviceOp` 入力に書き換え、**期待 JSON は 1 文字も変えない**（wire 契約の釘）。
  `attach_deadline` テストは `budget_applies` 入力へ。
- **matd**: `is_native_hotpath` / `native_group_params` テストは `to_device_op` へ移植。
  `tests/integration.rs`（fake establisher × socket end-to-end）は無変更で通ること。
- `task check` 緑（fmt / clippy / 全テスト）。
- `task e2e:device:m3`（matv × matd × mat listen）合格。
- **実機 E2E（マージ前必須）**: hogar-matd コンテナ内で musl 静的 x86_64 新バイナリ
  （`*.new`、本番未置換）: 直経路 `read node23`、matd 経由 `read node24` /
  `describe node24`、`group invoke`（無変化パターン）、エラー経路 `node99`
  = exit 11。matd 経由は隔離 matd 方式（本番 matd を止めず、新バイナリの matd を
  別ソケット・別 store コピーで起動して `MAT_MATD_SOCKET` で向ける）。

## semver / 版

`matd::native::NativeBackend` は `pub` なので op 別メソッド削除は major 判定になる
見込み。CLAUDE.md の規則どおり `task semver` の結果に従い、次回 publish は
ワークスペース major（2.0.0）前提で進める（監査①の pub API 削除で既に major 相当）。
本変更ではバージョンは上げない（publish は別セッション）。

## 非目標

- wire `Op` 型の共有クレート化（案 B）— direct-only op が wire に無く変換が 2 系統に戻る。
- `protocol.rs` helper 5 本の統合、`op_state_target` の `NodeOp` 化。
- `im.rs` / `session.rs` の分割（監査の別項目）。
- 新 op の追加、`unpair` 等の機能ギャップ（監査「足りない機能 3」）。
