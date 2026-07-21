# matd 購読 priming 軽量化 + 確立失敗観測性 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 弱リンクノードで購読再確立が数十分〜数時間失敗し続ける問題を、`<store>/subscriptions.toml` による購読パス絞り込み（priming 1〜2 チャンク化）と確立失敗の観測性改善で恒久修正する。

**Architecture:** SubscribeRequest の AttributePathIB を「full wildcard 1 本」から「endpoint wildcard + cluster 指定 × N」へ一般化する（ワイヤ変更はここだけ）。クラスタ集合は matd 起動時に `<store>/subscriptions.toml` から読み（無し = 従来の full wildcard、挙動不変）、`SubscribeConn::subscribe_wildcard(clusters)` 経由でセッション層へ渡す。matd の再購読ループには失敗ストリーク状態を足し「初回失敗 info / 10 分 warn / 復帰時ダウン時間+試行回数」を出す。

**Tech Stack:** Rust (tokio)、TLV は自前 `mat_controller::tlv`、toml + serde、tracing。

**Spec:** `docs/superpowers/specs/2026-07-21-matd-subscribe-priming-weight-fix-design.md`

## Global Constraints

- ファイル無し = full wildcard（挙動不変）。既存テストは無改変で通ること（シグネチャ追随の `&[]` / `None` 追加のみ可）。
- toml のパース失敗・未知クラスタ名・空リストは **matd 起動拒否**（`store_parse` / exit 10）。silent fallback 禁止。
- matd socket プロトコル・`mat listen`・イベントスキーマ・warm op 経路は無変更。
- `mat`（one-shot CLI）は subscriptions.toml を読まない。
- 作業ブランチ: 現 worktree（`worktree-feat-matd-subscribe-listen`）で継続。各タスク末尾でコミット。
- コミット前に `cargo fmt` を通すこと（最終タスクで `task check` = fmt:check + clippy + test）。

---

### Task 1: `encode_subscribe_request` のパス列挙対応（mat-controller/im.rs）

**Files:**
- Modify: `crates/mat-controller/src/im.rs:719`（`encode_subscribe_request_wildcard` を一般化）
- Modify: `crates/mat-controller/src/session.rs:980`（呼び出し追随、`&[]`）
- Test: `crates/mat-controller/src/im.rs`（tests モジュール、`subscribe_request_wildcard_shape` 付近）

**Interfaces:**
- Produces: `pub fn encode_subscribe_request(min_interval_floor_s: u16, max_interval_ceiling_s: u16, keep_subscriptions: bool, clusters: &[u32]) -> Vec<u8>`（旧 `encode_subscribe_request_wildcard` は削除・改名）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/im.rs` の tests モジュール、既存 `subscribe_request_wildcard_shape` の直後に追加:

```rust
    #[test]
    fn subscribe_request_cluster_paths_shape() {
        // clusters 非空: AttributePathIB を「cluster(Context(3)) のみ指定」で
        // クラスタ数ぶん並べる（endpoint/attribute は省略 = wildcard）。
        let b = encode_subscribe_request(0, 300, false, &[0x0006, 0x0402]);
        let mut r = Reader::new(&b);
        // 外殻 struct → keep/min/max を読み飛ばして AttributeRequests へ。
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        r.next().unwrap().unwrap(); // KeepSubscriptions
        r.next().unwrap().unwrap(); // MinIntervalFloorSeconds
        r.next().unwrap().unwrap(); // MaxIntervalCeilingSeconds
        let el = r.next().unwrap().unwrap(); // AttributeRequests
        assert_eq!(el.tag, Tag::Context(3));
        assert!(matches!(el.value, Value::ArrayStart));
        // path 1: list { Context(3) = 0x0006 }
        assert!(matches!(r.next().unwrap().unwrap().value, Value::ListStart));
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(3));
        assert_eq!(el.value, Value::Uint(0x0006));
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        ));
        // path 2: list { Context(3) = 0x0402 }
        assert!(matches!(r.next().unwrap().unwrap().value, Value::ListStart));
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(3));
        assert_eq!(el.value, Value::Uint(0x0402));
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        ));
        // AttributeRequests 閉じ → IsFabricFiltered
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        ));
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(7));
        assert_eq!(el.value, Value::Bool(true));
    }
```

同時に既存 `subscribe_request_wildcard_shape` の呼び出しを新シグネチャへ:

```rust
        let b = encode_subscribe_request(0, 3600, false, &[]);
```

- [ ] **Step 2: テストが失敗する（コンパイルエラー）ことを確認**

Run: `cargo test -p mat-controller subscribe_request -- --nocapture 2>&1 | head -20`
Expected: `encode_subscribe_request` 未定義のコンパイルエラー。

- [ ] **Step 3: 実装**

`crates/mat-controller/src/im.rs:716` 付近、`encode_subscribe_request_wildcard` を以下で**置き換える**（旧名の関数は残さない）:

```rust
/// SubscribeRequestMessage (spec §8.10)。`clusters` が空なら全フィールド省略の
/// AttributePathIB 1 本（= 全 endpoint / 全 cluster / 全 attribute の full
/// wildcard）。非空なら「endpoint wildcard + cluster 指定 + attribute wildcard」
/// の AttributePathIB をクラスタ数ぶん並べる（priming 軽量化 — 弱リンクでは
/// full wildcard priming の数十往復が完走できない）。EventRequests は載せない
/// （v1 は attribute report のみ）。
pub fn encode_subscribe_request(
    min_interval_floor_s: u16,
    max_interval_ceiling_s: u16,
    keep_subscriptions: bool,
    clusters: &[u32],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), keep_subscriptions);
    w.put_uint(Tag::Context(1), u64::from(min_interval_floor_s));
    w.put_uint(Tag::Context(2), u64::from(max_interval_ceiling_s));
    w.start_array(Tag::Context(3)); // AttributeRequests
    if clusters.is_empty() {
        w.start_list(Tag::Anonymous); // AttributePathIB（全省略 = wildcard）
        w.end_container();
    } else {
        for &cluster in clusters {
            w.start_list(Tag::Anonymous); // AttributePathIB
            w.put_uint(Tag::Context(3), u64::from(cluster)); // Cluster のみ指定
            w.end_container();
        }
    }
    w.end_container();
    // IsFabricFiltered = true: read と同じ既定（encode_read_request のコメント参照）。
    w.put_bool(Tag::Context(7), true);
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}
```

呼び出し追随 `crates/mat-controller/src/session.rs:980`:

```rust
        let req = im::encode_subscribe_request(
            min_interval_floor_s,
            max_interval_ceiling_s,
            keep_subscriptions,
            &[],
        );
```

（`&[]` は Task 2 で `clusters` パラメータに置き換わる暫定。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller subscribe_request`
Expected: `subscribe_request_wildcard_shape` / `subscribe_request_cluster_paths_shape` とも PASS。

- [ ] **Step 5: crate 全体のテストと fmt**

Run: `cargo test -p mat-controller && cargo fmt`
Expected: 全 PASS（既存 subscribe ハンドシェイクテスト含む）。

- [ ] **Step 6: Commit**

```bash
git add crates/mat-controller/src/im.rs crates/mat-controller/src/session.rs
git commit -m "feat(im): SubscribeRequest をクラスタパス列挙対応に一般化（空=wildcard）"
```

---

### Task 2: `SecureSession::subscribe_wildcard` にクラスタ集合パラメータ（session.rs）

**Files:**
- Modify: `crates/mat-controller/src/session.rs:965`（シグネチャ + Task 1 の `&[]` を差し替え）
- Modify: `crates/mat-native/src/lib.rs:506`（呼び出し追随、`&[]`）
- Test: `crates/mat-controller/src/session.rs`（tests、`subscribe_wildcard_handshake_with_chunked_priming` 付近）

**Interfaces:**
- Consumes: Task 1 の `encode_subscribe_request(min, max, keep, clusters)`
- Produces: `pub async fn subscribe_wildcard(&mut self, min_interval_floor_s: u16, max_interval_ceiling_s: u16, keep_subscriptions: bool, clusters: &[u32], cfg: &MrpConfig) -> Result<(SubscribeResponse, Vec<ReportDataMessage>), SessionError>`（`clusters` は `cfg` の直前）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/session.rs` tests、`subscribe_wildcard_handshake_with_chunked_priming` の直後に追加。ワイヤに cluster パスが乗ることをデバイス側で検証する:

```rust
    /// 絞り込み購読: SubscribeRequest の AttributeRequests に指定クラスタの
    /// AttributePathIB が列挙されてワイヤに乗る（priming 軽量化の釘打ち）。
    #[tokio::test]
    async fn subscribe_wildcard_sends_cluster_paths_when_narrowed() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_SUBSCRIBE_REQUEST);
            // SubscribeRequest 中の Uint な Context(3) は AttributePathIB の
            // cluster だけ（トップの Context(3) は ArrayStart、IsFabricFiltered
            // は Context(7)）なので、素朴な全要素走査で拾える。
            use crate::tlv::{Reader, Tag, Value};
            let mut r = Reader::new(&body);
            let mut clusters = Vec::new();
            while let Some(el) = r.next().unwrap() {
                if el.tag == Tag::Context(3) {
                    if let Value::Uint(v) = el.value {
                        clusters.push(u32::try_from(v).unwrap());
                    }
                }
            }
            assert_eq!(clusters, vec![0x0006, 0x0402]);
            let ex = p.exchange_id;
            // priming 1 チャンク（more=false）→ StatusResponse(0) → SubscribeResponse
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                None,
                false,
                9100,
                &subscription_report_payload(43, false, false),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p2, body) = open_from_controller(&buf[..n]);
            assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_SUBSCRIBE_RESPONSE,
                None,
                false,
                9101,
                &subscribe_response_payload(43, 300),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        });

        let (resp, priming) = s
            .subscribe_wildcard(0, 300, false, &[0x0006, 0x0402], &fast_cfg())
            .await
            .unwrap();
        assert_eq!(resp.subscription_id, 43);
        assert_eq!(priming.len(), 1);
        dev_task.await.unwrap();
    }
```

同時に既存 `subscribe_wildcard_handshake_with_chunked_priming` の呼び出しへ `&[]` を挿入:

```rust
        let (resp, priming) = s
            .subscribe_wildcard(0, 3600, false, &[], &fast_cfg())
            .await
            .unwrap();
```

- [ ] **Step 2: コンパイルエラーを確認**

Run: `cargo test -p mat-controller subscribe_wildcard 2>&1 | head -20`
Expected: 引数個数不一致のコンパイルエラー。

- [ ] **Step 3: 実装**

`crates/mat-controller/src/session.rs:965` のシグネチャに `clusters: &[u32]` を追加（`cfg` の直前）し、Task 1 で入れた暫定 `&[]` を `clusters` に置き換える:

```rust
    pub async fn subscribe_wildcard(
        &mut self,
        min_interval_floor_s: u16,
        max_interval_ceiling_s: u16,
        keep_subscriptions: bool,
        clusters: &[u32],
        cfg: &MrpConfig,
    ) -> Result<
        (
            crate::im::SubscribeResponse,
            Vec<crate::im::ReportDataMessage>,
        ),
        SessionError,
    > {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_subscribe_request(
            min_interval_floor_s,
            max_interval_ceiling_s,
            keep_subscriptions,
            clusters,
        );
```

doc コメントも「wildcard Subscribe を張る」→「Subscribe を張る（`clusters` 空 = full wildcard、非空 = クラスタ絞り込み）」に更新。

呼び出し追随 `crates/mat-native/src/lib.rs:506`（`SubscriptionSession::subscribe_wildcard` 内）:

```rust
            .subscribe_wildcard(
                SUBSCRIBE_MIN_INTERVAL_FLOOR_S,
                SUBSCRIBE_MAX_INTERVAL_CEILING_S,
                SUBSCRIBE_KEEP_SUBSCRIPTIONS,
                &[],
                &self.mrp,
            )
```

（この `&[]` は Task 3 で trait パラメータに置き換わる暫定。）

- [ ] **Step 4: テスト**

Run: `cargo test -p mat-controller && cargo test -p mat-native && cargo fmt`
Expected: 新テスト含め全 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/session.rs crates/mat-native/src/lib.rs
git commit -m "feat(session): subscribe_wildcard にクラスタ集合パラメータ（空=従来 wildcard）"
```

---

### Task 3: `SubscribeConn` trait 経由でクラスタ集合を配線（mat-native + matd）

**Files:**
- Modify: `crates/mat-native/src/lib.rs:166`（trait）、`:501`（実装）、`:946`（テスト追随）
- Modify: `crates/mat-native/src/test_support.rs`（FakeSubConn / FakeEstablisher に記録用フィールド）
- Modify: `crates/matd/src/subscription.rs`（`spawn_subscription_manager` / ループにクラスタ集合を通す）
- Modify: `crates/matd/src/main.rs:196`（暫定 `None` を渡す）
- Test: `crates/matd/src/subscription.rs`（tests に `manager_passes_clusters_to_subscribe`）

**Interfaces:**
- Consumes: Task 2 の `SecureSession::subscribe_wildcard(min, max, keep, clusters, cfg)`
- Produces:
  - `SubscribeConn::subscribe_wildcard(&mut self, clusters: &[u32])`（trait メソッドにパラメータ追加）
  - `pub fn spawn_subscription_manager(native: Arc<NativeState>, store_path: PathBuf, events: broadcast::Sender<Event>, clusters: Option<Vec<u32>>) -> Vec<JoinHandle<()>>`（`None` = full wildcard）
  - `FakeEstablisher::sub_clusters: Arc<Mutex<Vec<u32>>>`（直近の subscribe_wildcard が受けた clusters の記録 — matd テストが検証に使う）

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/subscription.rs` tests、`manager_emits_priming_events_from_fake_subscription` の直後に追加:

```rust
    /// manager 経路: subscriptions.toml 由来のクラスタ集合が SubscribeConn::
    /// subscribe_wildcard まで届く（絞り込みの配線の釘打ち）。
    #[tokio::test]
    async fn manager_passes_clusters_to_subscribe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-21T00:00:00+09:00".into(),
            })
            .unwrap();

        let est = FakeEstablisher::default();
        let seen = std::sync::Arc::clone(&est.sub_clusters);
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _handles = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            Some(vec![0x0006, 0x0406]),
        );

        // priming イベントが届いた時点で subscribe_wildcard は呼ばれている。
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![0x0006, 0x0406]);
    }
```

既存テスト `manager_emits_priming_events_from_fake_subscription` の spawn 呼び出しには第4引数 `None` を追加:

```rust
        let _handles = spawn_subscription_manager(state, dir.path().to_path_buf(), tx, None);
```

- [ ] **Step 2: コンパイルエラーを確認**

Run: `cargo test -p matd manager_ 2>&1 | head -20`
Expected: `sub_clusters` フィールド不在 / 引数個数不一致のコンパイルエラー。

- [ ] **Step 3: 実装（4 ファイル）**

**(a) trait** `crates/mat-native/src/lib.rs:166`:

```rust
#[async_trait]
pub trait SubscribeConn: Send {
    /// Subscribe を張り、成立情報と priming report 群を返す。`clusters` 空 =
    /// full wildcard、非空 = 「endpoint wildcard + cluster 指定」のパス列挙
    /// （priming 軽量化 — subscriptions.toml 由来）。
    async fn subscribe_wildcard(
        &mut self,
        clusters: &[u32],
    ) -> Result<(SubscriptionInfo, Vec<mat_controller::im::ReportDataMessage>), MatError>;
```

**(b) 実装** `crates/mat-native/src/lib.rs:501`（Task 2 の暫定 `&[]` を置き換え）:

```rust
    async fn subscribe_wildcard(
        &mut self,
        clusters: &[u32],
    ) -> Result<(SubscriptionInfo, Vec<mat_controller::im::ReportDataMessage>), MatError> {
        let (resp, priming) = self
            .session
            .subscribe_wildcard(
                SUBSCRIBE_MIN_INTERVAL_FLOOR_S,
                SUBSCRIBE_MAX_INTERVAL_CEILING_S,
                SUBSCRIBE_KEEP_SUBSCRIPTIONS,
                clusters,
                &self.mrp,
            )
            .await
            .map_err(map_session_err)?;
```

`crates/mat-native/src/lib.rs:946` 付近のテスト呼び出しは `conn.subscribe_wildcard(&[])` に追随。

**(c) fake** `crates/mat-native/src/test_support.rs`。`FakeSubConn` に記録先を追加:

```rust
pub struct FakeSubConn {
    pub max_interval_s: u16,
    pub priming: Vec<mat_controller::im::ReportDataMessage>,
    pub live: std::collections::VecDeque<mat_controller::im::ReportDataMessage>,
    /// subscribe_wildcard が受けた clusters の記録先（FakeEstablisher と共有）。
    pub seen_clusters: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
}
```

`Default` 実装に `seen_clusters: std::sync::Arc::default(),` を追加。`impl SubscribeConn for FakeSubConn` を新シグネチャにし、冒頭で記録:

```rust
    async fn subscribe_wildcard(
        &mut self,
        clusters: &[u32],
    ) -> Result<
        (
            crate::SubscriptionInfo,
            Vec<mat_controller::im::ReportDataMessage>,
        ),
        MatError,
    > {
        *self.seen_clusters.lock().unwrap() = clusters.to_vec();
        Ok((
```

`FakeEstablisher` に共有フィールドを追加し、`Default` 実装（`test_support.rs:257`）にも `sub_clusters: std::sync::Arc::default(),` を追加:

```rust
pub struct FakeEstablisher {
    pub calls: std::sync::Arc<AtomicUsize>,
    pub fail_first_send: bool,
    pub fail_kind: ErrorKind,
    /// 直近の establish_subscription が返した FakeSubConn の seen_clusters と
    /// 共有される記録先（matd の manager テストが検証に使う）。
    pub sub_clusters: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
}
```

`establish_subscription` は共有 Arc を FakeSubConn へ渡す:

```rust
    async fn establish_subscription(
        &self,
        _node_id: u64,
    ) -> Result<Box<dyn crate::SubscribeConn>, MatError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeSubConn {
            seen_clusters: std::sync::Arc::clone(&self.sub_clusters),
            ..Default::default()
        }))
    }
```

**(d) matd 配線** `crates/matd/src/subscription.rs`。`spawn_subscription_manager` に `clusters: Option<Vec<u32>>` を追加し、各ノードタスクへ `Arc<[u32]>` で配る:

```rust
pub fn spawn_subscription_manager(
    native: Arc<NativeState>,
    store_path: PathBuf,
    events: broadcast::Sender<Event>,
    clusters: Option<Vec<u32>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let node_ids: Vec<u64> = match Store::open(&store_path) {
        Ok(store) => store.nodes().map(|n| n.node_id).collect(),
        Err(e) => {
            tracing::warn!(error = %e.detail, "subscription manager: store unreadable; no subscriptions");
            return Vec::new();
        }
    };
    // None = subscriptions.toml 無し = full wildcard（空 slice がワイヤ上の wildcard 形）。
    let clusters: Arc<[u32]> = clusters.unwrap_or_default().into();
    tracing::info!(nodes = node_ids.len(), "subscription manager starting");
    node_ids
        .into_iter()
        .map(|node_id| {
            let native = Arc::clone(&native);
            let events = events.clone();
            let clusters = Arc::clone(&clusters);
            tokio::spawn(
                async move { node_subscription_loop(node_id, native, events, clusters).await },
            )
        })
        .collect()
}
```

`node_subscription_loop` / `run_subscription_once` に `clusters` を通す（ログ側は Task 5 でさらに変わる — ここでは引数追加のみ）:

```rust
async fn node_subscription_loop(
    node_id: u64,
    native: Arc<NativeState>,
    events: broadcast::Sender<Event>,
    clusters: Arc<[u32]>,
) {
```

ループ内の呼び出しは `run_subscription_once(node_id, backend, &events, &clusters).await`、`run_subscription_once` のシグネチャに `clusters: &[u32]` を追加し、購読は:

```rust
    let (info, priming) = conn.subscribe_wildcard(clusters).await?;
```

**(e) main 暫定** `crates/matd/src/main.rs:196` の呼び出しに第4引数 `None` を追加（Task 4 で実配線）。

- [ ] **Step 4: テスト**

Run: `cargo test -p mat-native && cargo test -p matd && cargo fmt`
Expected: 新テスト `manager_passes_clusters_to_subscribe` 含め全 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-native/src/lib.rs crates/mat-native/src/test_support.rs crates/matd/src/subscription.rs crates/matd/src/main.rs
git commit -m "feat(matd): クラスタ集合を SubscribeConn 経由で購読まで配線"
```

---

### Task 4: `<store>/subscriptions.toml` ローダ + matd 起動配線

**Files:**
- Create: `crates/matd/src/subscribe_config.rs`
- Modify: `crates/matd/src/lib.rs`（`pub mod subscribe_config;` 追加）
- Modify: `crates/matd/Cargo.toml`（`toml.workspace = true` を dependencies に追加）
- Modify: `crates/matd/src/main.rs`（load + fail-fast + spawn へ渡す）
- Test: `crates/matd/src/subscribe_config.rs`（tests モジュール）

**Interfaces:**
- Consumes: `mat_core::ids::resolve_cluster(&str) -> Option<u32>`（名前・数値両対応）、`mat_core::error::{ErrorKind::StoreParse, MatError}`
- Produces: `pub fn load(store_root: &Path) -> Result<Option<Vec<u32>>, MatError>`、`pub const SUBSCRIPTIONS_FILE: &str = "subscriptions.toml"`

- [ ] **Step 1: 失敗するテストを含む新モジュールを書く**

`crates/matd/src/subscribe_config.rs` を新規作成（テスト込み・実装は最小スタブでなく最初から本実装で良い — TDD の骨子はテストを同時に書き、Step 2 で red を確認してから緑にする流れを維持する。ここではファイル新規なので実装とテストを同一ステップで書き、テスト内容が仕様を規定する）:

```rust
//! `<store>/subscriptions.toml` — matd 常駐購読のクラスタ絞り込み設定。
//!
//! 無し = full wildcard（挙動不変、aliases.toml と同じ absent-file 規律）。
//! 壊れ・未知クラスタ名・空リストは `store_parse` — matd は起動を拒否する
//! （黙って wildcard に落ちると弱リンク対策が無効化されたことに気づけない
//! ため、silent fallback はしない）。`mat`（one-shot）はこのファイルを読まない。

use std::path::Path;

use mat_core::error::{ErrorKind, MatError};

pub const SUBSCRIPTIONS_FILE: &str = "subscriptions.toml";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubscriptions {
    clusters: Vec<String>,
}

/// subscriptions.toml を読む。無ければ `Ok(None)`（= full wildcard）。
/// クラスタ名は chip-tool 記法（`mat-core::ids`）、数値文字列（`"0x0006"` /
/// `"6"`）も可（ids に無いクラスタの escape hatch — generic read と同じ規律）。
/// 重複は除去（順序は初出順を保持）。
pub fn load(store_root: &Path) -> Result<Option<Vec<u32>>, MatError> {
    let path = store_root.join(SUBSCRIPTIONS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(MatError::new(
                ErrorKind::StoreParse,
                format!("subscriptions.toml unreadable: {e}"),
            ));
        }
    };
    let raw: RawSubscriptions = toml::from_str(&text)
        .map_err(|e| MatError::new(ErrorKind::StoreParse, format!("subscriptions.toml: {e}")))?;
    if raw.clusters.is_empty() {
        return Err(MatError::new(
            ErrorKind::StoreParse,
            "subscriptions.toml: clusters must not be empty (delete the file for full wildcard)",
        ));
    }
    let mut ids: Vec<u32> = Vec::new();
    for name in &raw.clusters {
        let id = mat_core::ids::resolve_cluster(name).ok_or_else(|| {
            MatError::new(
                ErrorKind::StoreParse,
                format!("subscriptions.toml: unknown cluster '{name}'"),
            )
        })?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(Some(ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, body: &str) {
        std::fs::write(dir.join(SUBSCRIPTIONS_FILE), body).unwrap();
    }

    #[test]
    fn absent_file_means_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
    }

    #[test]
    fn resolves_names_and_numerics_dedup_in_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"clusters = ["occupancysensing", "onoff", "0x0402", "6"]"#,
        );
        // "6" = 0x0006 = onoff の重複 → 除去。初出順を保持。
        assert_eq!(
            load(dir.path()).unwrap(),
            Some(vec![0x0406, 0x0006, 0x0402])
        );
    }

    #[test]
    fn unknown_cluster_name_is_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"clusters = ["nosuchcluster"]"#);
        let e = load(dir.path()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::StoreParse);
        assert!(e.detail.contains("nosuchcluster"));
    }

    #[test]
    fn empty_list_is_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "clusters = []");
        assert_eq!(load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
    }

    #[test]
    fn broken_toml_and_unknown_key_are_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "clusters = [broken");
        assert_eq!(load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
        write(dir.path(), "clusterz = [\"onoff\"]");
        assert_eq!(load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
    }
}
```

`crates/matd/src/lib.rs` のモジュール一覧に追加（アルファベット順の位置）:

```rust
pub mod subscribe_config;
```

`crates/matd/Cargo.toml` の `[dependencies]` に追加:

```toml
toml.workspace = true
```

（`tempfile` が matd の dev-dependencies に無ければ `tempfile.workspace = true` を `[dev-dependencies]` に追加 — `subscription.rs` の既存テストが使っているので既にあるはず。）

- [ ] **Step 2: テストが通ることを確認**

Run: `cargo test -p matd subscribe_config`
Expected: 5 テスト全 PASS。（コンパイルエラーが出た場合は resolve_cluster の挙動・ErrorKind 名を実コードで確認して直す。）

- [ ] **Step 3: main 配線**

`crates/matd/src/main.rs` の `serve_daemon`、`spawn_subscription_manager` 呼び出しの直前に:

```rust
    // 常駐購読のクラスタ絞り込み（subscriptions.toml、無し = full wildcard）。
    // 設定不備は fail-fast: 黙って wildcard に落ちると弱リンク対策が無効化
    // されたことに気づけない（ambiguous iface autodetect と同じ規律）。
    let sub_clusters = match matd::subscribe_config::load(&store_path) {
        Ok(c) => c,
        Err(e) => {
            e.emit();
            std::process::exit(e.kind.exit_code() as i32);
        }
    };
    if let Some(c) = &sub_clusters {
        tracing::info!(
            clusters = c.len(),
            "subscriptions.toml loaded; narrowing resident subscribe paths"
        );
    }
```

Task 3 で入れた暫定 `None` を `sub_clusters` に置き換える:

```rust
    let _sub_handles = matd::subscription::spawn_subscription_manager(
        std::sync::Arc::clone(&native),
        store_path.clone(),
        events_tx.clone(),
        sub_clusters,
    );
```

- [ ] **Step 4: テスト**

Run: `cargo test -p matd && cargo fmt`
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/subscribe_config.rs crates/matd/src/lib.rs crates/matd/src/main.rs crates/matd/Cargo.toml Cargo.lock
git commit -m "feat(matd): subscriptions.toml で購読クラスタを絞り込み（無し=wildcard、不備=起動拒否）"
```

---

### Task 5: 確立失敗の観測性（初回 info / 10 分 warn / 復帰時ダウン時間）

**Files:**
- Modify: `crates/matd/src/subscription.rs`（ループのストリーク状態 + 純関数 `classify_failure`）
- Test: `crates/matd/src/subscription.rs`（tests）

**Interfaces:**
- Consumes: Task 3 後の `node_subscription_loop(node_id, native, events, clusters)` / `run_subscription_once(node_id, backend, events, clusters)`
- Produces: `pub(crate) enum FailureLog { First, StuckWarn, Quiet }` と `pub(crate) fn classify_failure(consecutive_failures: u32, down_for: Duration, warned: bool) -> FailureLog`

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/subscription.rs` tests に追加:

```rust
    #[test]
    fn failure_log_first_then_quiet_then_single_warn() {
        use std::time::Duration;
        // 1 回目の失敗は info（First）。
        assert!(matches!(
            classify_failure(1, Duration::from_secs(3), false),
            FailureLog::First
        ));
        // 2 回目以降は debug（Quiet）。
        assert!(matches!(
            classify_failure(2, Duration::from_secs(20), false),
            FailureLog::Quiet
        ));
        // 未確立 10 分超で warn（StuckWarn）— 一度だけ。
        assert!(matches!(
            classify_failure(5, Duration::from_secs(601), false),
            FailureLog::StuckWarn
        ));
        assert!(matches!(
            classify_failure(6, Duration::from_secs(900), true),
            FailureLog::Quiet
        ));
        // 初回失敗が既に 10 分超（あり得ないが）でも First 優先で情報は出る。
        assert!(matches!(
            classify_failure(1, Duration::from_secs(700), false),
            FailureLog::First
        ));
    }
```

- [ ] **Step 2: コンパイルエラーを確認**

Run: `cargo test -p matd failure_log 2>&1 | head -10`
Expected: `classify_failure` / `FailureLog` 未定義のコンパイルエラー。

- [ ] **Step 3: 実装**

`crates/matd/src/subscription.rs`、定数群の直後に追加:

```rust
/// 未確立がこの時間続いたら warn を 1 回出す（弱リンクノードの長期ブラインドを
/// 本番 info/warn レベルで可視化する — 実測で盲目窓が数時間に達した反省）。
const STUCK_WARN_AFTER: Duration = Duration::from_secs(600);

/// 確立失敗ログの出し分け（純関数 — 時計はループ側が持つ）。
/// 毎試行 info は常駐ノイズ（弱リンクはバックオフ上限 5 分毎に永久に失敗し
/// 続ける）なので、状態遷移 + 間引きで出す — spec ①。
#[derive(Debug)]
pub(crate) enum FailureLog {
    /// 成功（or 起動）後の最初の失敗: info。
    First,
    /// 未確立 STUCK_WARN_AFTER 超・未警告: warn を 1 回。
    StuckWarn,
    /// それ以外: debug。
    Quiet,
}

pub(crate) fn classify_failure(
    consecutive_failures: u32,
    down_for: Duration,
    warned: bool,
) -> FailureLog {
    if consecutive_failures == 1 {
        FailureLog::First
    } else if !warned && down_for >= STUCK_WARN_AFTER {
        FailureLog::StuckWarn
    } else {
        FailureLog::Quiet
    }
}
```

`node_subscription_loop` をストリーク状態付きに書き換え:

```rust
async fn node_subscription_loop(
    node_id: u64,
    native: Arc<NativeState>,
    events: broadcast::Sender<Event>,
    clusters: Arc<[u32]>,
) {
    let NativeState::Ready(backend) = &*native else {
        return;
    };
    let mut backoff = Duration::ZERO;
    // ダウン起点（起動 or 購読喪失）とその後の失敗ストリーク。established で
    // リセットされる（run_subscription_once が確立ログにダウン時間を載せる）。
    let mut down_since = tokio::time::Instant::now();
    let mut failures: u32 = 0;
    let mut warned = false;
    loop {
        match run_subscription_once(node_id, backend, &events, &clusters, down_since, failures)
            .await
        {
            Ok(()) => {
                // 購読が成立して喪失した: 状態遷移なので info、状態リセット。
                tracing::info!(node_id, "subscription lost; resubscribing");
                backoff = Duration::ZERO;
                down_since = tokio::time::Instant::now();
                failures = 0;
                warned = false;
            }
            Err(e) => {
                failures += 1;
                match classify_failure(failures, down_since.elapsed(), warned) {
                    FailureLog::First => {
                        tracing::info!(
                            node_id,
                            kind = ?e.kind,
                            detail = %e.detail,
                            "subscription attempt failed; retrying with backoff"
                        );
                    }
                    FailureLog::StuckWarn => {
                        warned = true;
                        tracing::warn!(
                            node_id,
                            attempts = failures,
                            down_s = down_since.elapsed().as_secs(),
                            kind = ?e.kind,
                            detail = %e.detail,
                            "subscription still not established"
                        );
                    }
                    FailureLog::Quiet => {
                        tracing::debug!(node_id, kind = ?e.kind, detail = %e.detail, "subscription attempt failed");
                    }
                }
            }
        }
        backoff = next_backoff(backoff);
        tokio::time::sleep(backoff).await;
    }
}
```

`run_subscription_once` はダウン文脈を受けて確立ログに載せる:

```rust
async fn run_subscription_once(
    node_id: u64,
    backend: &crate::native::NativeBackend,
    events: &broadcast::Sender<Event>,
    clusters: &[u32],
    down_since: tokio::time::Instant,
    prior_failures: u32,
) -> Result<(), mat_core::error::MatError> {
    let mut conn = backend.establish_subscription(node_id).await?;
    let (info, priming) = conn.subscribe_wildcard(clusters).await?;
    tracing::info!(
        node_id,
        subscription_id = info.subscription_id,
        max_interval_s = info.max_interval_s,
        down_s = down_since.elapsed().as_secs(),
        attempts = prior_failures + 1,
        "subscription established"
    );
```

（残りの pump ループは無改変。）

- [ ] **Step 4: テスト**

Run: `cargo test -p matd && cargo fmt`
Expected: `failure_log_first_then_quiet_then_single_warn` 含め全 PASS。既存 manager テスト 2 本も無改変で PASS（ログ変更は挙動に影響しない）。

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): 購読確立失敗を初回info/10分warn化、確立ログにダウン時間と試行回数"
```

---

### Task 6: ドキュメント + バージョン 0.26.0 + 全体チェック

**Files:**
- Modify: `README.md`（subscriptions.toml の節 + listen 契約の注記）
- Modify: `CLAUDE.md`（スコープ注記に subscriptions.toml を 1 行）
- Modify: `Cargo.toml`（workspace version 0.25.0 → 0.26.0）+ `Cargo.lock`

**Interfaces:**
- Consumes: Task 4 の仕様（無し = wildcard、不備 = 起動拒否 exit 10）

- [ ] **Step 1: README に節を追加**

`README.md` の `mat listen` / matd の説明の近く（`aliases.toml` の節があればその後）に追加。既存の文体・見出しレベルに合わせること:

````markdown
### subscriptions.toml（matd の購読クラスタ絞り込み）

`<store>/subscriptions.toml` があると、matd の常駐 Subscribe は full wildcard
ではなく列挙クラスタの購読パスだけを張る。full wildcard の priming（全属性の
初回ダンプ、数十回の往復）は弱い Thread リンクで完走できず購読が数十分〜
数時間確立しないことがある — クラスタを絞ると priming が 1〜2 チャンクに
収まり、read が通る品質のリンクなら購読も確立できる。

```toml
clusters = [
  "onoff",
  "occupancysensing",
  "temperaturemeasurement",
]
```

- クラスタ名は chip-tool 記法（`mat read` と同じ）。数値（`"0x0006"` / `"6"`）も可。
- **ファイル無し = 従来どおり full wildcard**（挙動不変）。
- パース失敗・未知クラスタ名・空リストは matd が**起動を拒否**する
  （`store_parse` / exit 10）。黙って wildcard に落ちることはしない。
- このファイルがあるとき、`mat listen` に流れるのは列挙クラスタのイベント
  のみ。集合外のクラスタは listen のフィルタに指定しても一切イベントが来ない。
- 読み込みは matd 起動時 1 回（変更後は `systemctl --user restart matd` 等で再起動）。
- `mat`（one-shot）はこのファイルを読まない。
````

`mat listen` の既存説明にも 1 行注記: 「（`subscriptions.toml` がある場合、流れるのは列挙クラスタのみ — 上記参照）」。

- [ ] **Step 2: CLAUDE.md のスコープ注記**

`CLAUDE.md` の「Scope reminders」内、aliases.toml の例外を述べた項の近くに 1 文追加:

```markdown
  `<store>/subscriptions.toml` も同じ absent-file 規律（matd 専用: 常駐購読の
  クラスタ絞り込み。無し = full wildcard。不備は matd 起動拒否）。
```

- [ ] **Step 3: バージョン**

`Cargo.toml`（workspace root）: `version = "0.25.0"` → `version = "0.26.0"`。
Run: `cargo build -p matd 2>&1 | tail -1`（Cargo.lock 更新のため）。

- [ ] **Step 4: CI 相当の全体チェック**

Run: `task check`
Expected: fmt:check + clippy(-D warnings) + 全テスト PASS。

- [ ] **Step 5: Commit**

```bash
git add README.md CLAUDE.md Cargo.toml Cargo.lock
git commit -m "docs+chore(release): subscriptions.toml と購読観測性、0.26.0"
```

---

### Task 7: 実機デプロイ + E2E（メインセッションで実施 — subagent には出さない）

spec の合格条件。despliegue skill（dist:arm64 → scp → install → restart）と
[[jarvis-matd-deploy]] の手順に従う。**このタスクだけは subagent ではなく
メインセッション（オペレータ）が行う**（ssh 鍵・実機操作・観察判断が要るため）。

- [ ] jarvis の `~/.config/mat/subscriptions.toml` に spec 記載の 9 クラスタを配置
- [ ] 0.26.0 を dist:arm64 でビルド → jarvis へデプロイ → `systemctl --user restart matd`
- [ ] journal で確認: `subscriptions.toml loaded`、各ノードの `subscription established` に `down_s` / `attempts` が載る
- [ ] **node6（弱リンク当人）が established になる**こと（最重要ゲート。数分以内を期待 — priming 1〜2 チャンク化の効果測定）
- [ ] node6 で `mat on/off` → `mat listen` E2E が exit 0
- [ ] 健全ノード（node8 等）でも on→listen E2E exit 0（回帰なし）
- [ ] jarvis-iac に subscriptions.toml 配置を反映（`--check` → 差分確認 → 適用 → commit）
- [ ] メモリ（[[matd-subscribe-listen]] / [[jarvis-matd-deploy]]）へ結果を記録
