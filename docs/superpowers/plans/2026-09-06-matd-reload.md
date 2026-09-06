# matd reload（IPK ホットリロード）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat fabric rotate-ipk` の後に matd を restart せず、socket admin op `reload`（CLI `matd reload`）で新 IPK を取り込めるようにする。

**Architecture:** 差し替え点は `mat-native::CaseEstablisher` が持つ `Arc<FabricCredentials>` 1 箇所。`Establisher` trait に `reload_credentials(&self) -> Result<bool, MatError>`（既定 = 非対応）を足し、`CaseEstablisher` だけが KVS を読み直して identity を照合し `RwLock<Arc<_>>` を swap する。matd は `Op::Reload` を admin op（Status / NodeTouched と同じく `dispatch` で短絡）として受け、`DaemonInfo.reloads` に回数/時刻を記録して `status` に載せる。`mat fabric rotate-ipk` は commit（`status: "rotated"`）後に `node_touched` ヒントと同じ流儀で `{"op":"reload"}` を撃ち、ack を読んで body に `matd_reload` を載せる。group 状態は matd が毎送信で KVS を読むので対象外（provision / remove の「restart matd」note は削除）。

**Tech Stack:** Rust workspace（`mat-native` / `matd` / `mat`(crate 名 `matterctl`)）、tokio、serde_json、std `RwLock` / `AtomicU64`、bash e2e（`scripts/e2e-device-m3.sh`、matv 実機仮想デバイス）。

**Spec:** `docs/superpowers/specs/2026-09-06-matd-reload-design.md`

## Global Constraints

- CLAUDE.md の設計規則: stdout は純粋 JSON のみ、diagnostics は stderr の `tracing`、`timestamp` は ISO 8601（`mat_core::output::now_iso8601`）。
- 鍵（`ipk_operational` / epoch）は絶対にログ・body に出さない。reload 応答は `"ipk": "changed" | "unchanged"` の 2 値のみ。
- `Establisher` のテスト用 Fake（`FakeEstablisher` / `ScriptedEstablisher` 等）は既定実装のまま無変更で通ること。
- 既存の warm session / 購読 session には触らない（reload は per-node Mutex を取らない）。
- rotate の exit code / `status` は `matd_reload` の値で変わらない（情報フィールド）。
- worktree 内の git は `/usr/bin/git` の単純コマンド。編集は Edit ツール。各 Task の最後に `cargo fmt` を通してからコミット。
- ワークスペース全体の検証は最後に `task check`（fmt:check + clippy + test）。

---

## File Structure

| ファイル | 責務 / 変更 |
|---|---|
| `crates/mat-native/src/lib.rs` | `Establisher::reload_credentials` 既定実装、`CaseEstablisher` の `RwLock<Arc<FabricCredentials>>` 化 + `cfg` 保持 + 実装、`check_identity` 純関数、`Engine::reload_credentials` 委譲、テスト |
| `crates/matd/src/protocol.rs` | `Op::Reload`（admin op、name `"reload"`）+ テスト |
| `crates/matd/src/server.rs` | `ReloadStats`、`DaemonInfo.reloads`、`dispatch` の Reload 腕、`status_body` の `reloads`、`to_device_op` 群の腕、テスト |
| `crates/matd/src/main.rs` | `Command::Reload` → `admin_op("reload")`、`DaemonInfo` 初期化 |
| `crates/mat/src/matd_client.rs` | `MatdReload` enum + `hint_reload()` / `hint_reload_at()` + テスト |
| `crates/mat/src/commands/fabric.rs` | `attach_matd_reload` 純関数 + `run_rotate_ipk` からの呼び出し + テスト |
| `crates/mat-native/src/rotate_ipk.rs` | Rotated の note 文言 + テスト更新 |
| `crates/mat/src/native_direct.rs` | `PROVISION_NOTE` 削除（note: None）+ テスト更新 |
| `crates/mat-native/src/runner.rs` | テストの note 文言のみ |
| `docs/commands.md` / `CLAUDE.md` / spec 2026-09-05 §6 | 文言更新 |
| `scripts/e2e-device-m3.sh` | reload ステップ 3 つ |

---

### Task 1: `Establisher::reload_credentials` と `CaseEstablisher` の差し替え（mat-native）

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（trait `Establisher` L220-236、`case_establisher` L403-419、`struct CaseEstablisher` L532-536、`impl Establisher for CaseEstablisher` L538-600、`impl Engine` の末尾、`mod dedicated_op_socket_tests` L1467-）
- Modify: `docs/superpowers/specs/2026-09-06-matd-reload-design.md` §4.3（identity 照合と KVS 読みの置き場所を CaseEstablisher に訂正）

**Interfaces:**
- Consumes: 既存 `load_fabric_credentials(&NativeConfig) -> Result<FabricCredentials, MatError>`、`FabricCredentials { ipk_operational, fabric_id, node_id, root_public_key, .. }`。
- Produces（後続 Task が使う）:
  - `trait Establisher { fn reload_credentials(&self) -> Result<bool, MatError> { Err(other "credential reload not supported by this establisher") } }`
  - `impl Engine { pub fn reload_credentials(&self) -> Result<bool, MatError> }`（`self.establisher.reload_credentials()` へ委譲）
  - `pub(crate) fn check_identity(current: &FabricCredentials, fresh: &FabricCredentials) -> Result<(), MatError>`

- [ ] **Step 1: 失敗するテストを書く（swap / identity / 既定非対応）**

`crates/mat-native/src/lib.rs` の `mod tests`（L892 から始まる既存モジュール）の末尾に追加:

```rust
    #[test]
    fn establisher_reload_is_unsupported_by_default() {
        use crate::test_support::FakeEstablisher;
        let engine = Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        let err = engine.reload_credentials().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("not supported"), "detail={}", err.detail);
    }

    #[test]
    fn check_identity_accepts_same_fabric_and_rejects_changes() {
        let a = fake_creds([0xCC; 16], 0x1234, 0x1B669, [0xAA; 65]);
        // 同じ identity、IPK だけ違う = OK（ローテーション後の姿）。
        let b = fake_creds([0xDD; 16], 0x1234, 0x1B669, [0xAA; 65]);
        check_identity(&a, &b).expect("ipk change alone is fine");
        // fabric_id / node_id / root 公開鍵のどれが変わっても other で拒否。
        for (label, other) in [
            ("fabric_id", fake_creds([0xCC; 16], 0x9999, 0x1B669, [0xAA; 65])),
            ("node_id", fake_creds([0xCC; 16], 0x1234, 0x77, [0xAA; 65])),
            ("root key", fake_creds([0xCC; 16], 0x1234, 0x1B669, [0xAB; 65])),
        ] {
            let err = check_identity(&a, &other).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Other, "{label}");
            assert!(err.detail.contains("restart matd"), "{label}: {}", err.detail);
        }
    }

    /// テスト用: 証明書無しの `FabricCredentials`（identity 照合と swap だけに使う）。
    fn fake_creds(
        ipk: [u8; 16],
        fabric_id: u64,
        node_id: u64,
        root_public_key: [u8; 65],
    ) -> FabricCredentials {
        FabricCredentials {
            rcac_tlv: Vec::new(),
            icac_tlv: None,
            noc_tlv: Vec::new(),
            op_public_key: [0u8; 65],
            op_private_key: [0u8; 32],
            ipk_operational: ipk,
            node_id,
            fabric_id,
            root_public_key,
        }
    }

    #[test]
    fn case_establisher_swap_reports_ipk_change() {
        let est = CaseEstablisher {
            creds: std::sync::RwLock::new(Arc::new(fake_creds(
                [0xCC; 16],
                1,
                2,
                [0xAA; 65],
            ))),
            scope_id: 0,
            resolver: Arc::new(OneShotResolver),
            cfg: NativeConfig {
                store: std::path::PathBuf::from("/nonexistent"),
                iface: "lo".into(),
                thread_iface: None,
                fabric_index: 1,
                issuer_index: 0,
            },
        };
        // 同じ IPK → false、違う IPK → true、その後は新しい値が見える。
        assert!(!est.swap_credentials(fake_creds([0xCC; 16], 1, 2, [0xAA; 65])));
        assert!(est.swap_credentials(fake_creds([0xDD; 16], 1, 2, [0xAA; 65])));
        assert_eq!(est.creds().ipk_operational, [0xDD; 16]);
    }

    /// KVS が無い store で reload すると store_missing（`load_fabric_credentials`
    /// と同じ写像）。swap は起きない。
    #[test]
    fn case_establisher_reload_maps_missing_store_to_store_missing() {
        let dir = tempfile::tempdir().unwrap();
        let est = CaseEstablisher {
            creds: std::sync::RwLock::new(Arc::new(fake_creds(
                [0xCC; 16],
                1,
                2,
                [0xAA; 65],
            ))),
            scope_id: 0,
            resolver: Arc::new(OneShotResolver),
            cfg: NativeConfig {
                store: dir.path().to_path_buf(),
                iface: "lo".into(),
                thread_iface: None,
                fabric_index: 1,
                issuer_index: 0,
            },
        };
        let err = est.reload_credentials().unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreMissing);
        assert_eq!(est.creds().ipk_operational, [0xCC; 16]);
    }
```

`FabricCredentials` のフィールドはすべて `pub`（`crates/mat-controller/src/fabric.rs` L92-101）なので struct literal で作れる。`mod tests` の先頭 `use` に `FabricCredentials` が無ければ `use mat_controller::fabric::FabricCredentials;` を足す（lib.rs 本体で既に import されている名前なら `use super::*;` で足りる — コンパイルエラーで判断）。

- [ ] **Step 2: 失敗するテストを書く（swap が次の establish に効く — 実 CASE 応答器）**

`mod dedicated_op_socket_tests`（L1467-）の末尾、`concurrent_establishes_use_dedicated_sockets` の後に追加。既存テストと同じ fixture（`case_ts`）・同じ `FixedPortResolver` を使う:

```rust
    /// reload の釘打ち: 間違った IPK で建てた確立器は CASE に失敗し、正しい
    /// 資格情報へ swap した直後の establish は成功する（進行中セッション無し
    /// の最小形 — swap が「次の確立から効く」ことを実 CASE で確認する）。
    #[tokio::test]
    async fn swapped_credentials_are_used_by_the_next_establish() {
        let noc = MatterCert::parse(case_ts::NODE01_NOC).expect("parse fixture NOC");
        let responder_node_id = noc.node_id().expect("node id");
        let fabric_id = noc.fabric_id().expect("fabric id");
        let op_priv: [u8; 32] = case_ts::NODE01_PRIV.try_into().unwrap();

        // 応答器 2 つ（1 回目の失敗で 1 つ目が終わっても 2 回目が着く先を持つ）。
        let mut handles = Vec::new();
        let mut ports = Vec::new();
        for _ in 0..2 {
            let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap();
            ports.push(t.local_addr().unwrap().port());
            handles.push(tokio::spawn(case_ts::responder_task(
                t,
                case_ts::INITIATOR_NODE_ID,
                responder_node_id,
                case_ts::NODE01_NOC.to_vec(),
                case_ts::ICA01.to_vec(),
                op_priv,
                case_ts::ROOT01_CHIP.to_vec(),
            )));
        }

        let materials = |ipk: [u8; 16]| SelfIssueMaterials {
            rcac: case_ts::ROOT01_CHIP.to_vec(),
            root_private_key: case_ts::ROOT01_PRIV.try_into().unwrap(),
            ipk_operational: ipk,
            node_id: case_ts::INITIATOR_NODE_ID,
            fabric_id,
        };
        let wrong = FabricCredentials::from_self_issued(materials([0xDD; 16])).expect("creds");
        let right = FabricCredentials::from_self_issued(materials(case_ts::IPK)).expect("creds");
        let est = CaseEstablisher {
            creds: std::sync::RwLock::new(Arc::new(wrong)),
            scope_id: 0,
            resolver: Arc::new(FixedPortResolver {
                ports,
                next: AtomicUsize::new(0),
            }),
            cfg: NativeConfig {
                store: std::path::PathBuf::from("/nonexistent"),
                iface: "lo".into(),
                thread_iface: None,
                fabric_index: 1,
                issuer_index: 0,
            },
        };

        let err = est
            .establish(responder_node_id)
            .await
            .expect_err("wrong IPK must not establish");
        assert_eq!(err.kind, ErrorKind::SessionFailed, "detail={}", err.detail);

        assert!(est.swap_credentials(right), "IPK differs → changed");
        let mut conn = est
            .establish(responder_node_id)
            .await
            .expect("establish with the swapped credentials");
        assert!(!conn.read_onoff(1).await.expect("read after swap"));

        for h in handles {
            h.abort();
        }
    }
```

- [ ] **Step 3: テストが失敗（コンパイルエラー）することを確認**

Run: `cargo test -p mat-native --lib reload_ -- --nocapture 2>&1 | tail -20`
Expected: `error[E0599]`（`reload_credentials` / `swap_credentials` / `check_identity` が無い、`CaseEstablisher` に `cfg` フィールドが無い）。

- [ ] **Step 4: 実装**

(a) trait `Establisher`（L220-236）に既定メソッドを追加:

```rust
    /// 資格情報を KVS から読み直して差し替える（IPK ローテーション後の
    /// `matd reload`）。戻り値は `ipk_operational` が変わったか。既定は非対応 —
    /// 実確立器（CaseEstablisher）だけが上書きする。同期メソッド（I/O は
    /// KVS の 1 回読みだけ、ネットワークには触れない）。
    fn reload_credentials(&self) -> Result<bool, MatError> {
        Err(MatError::new(
            ErrorKind::Other,
            "credential reload not supported by this establisher",
        ))
    }
```

(b) `CaseEstablisher`（L532-536）を変更:

```rust
/// 実確立器: 保持した資格情報で mDNS 解決 → CASE。op セッションのソケットは
/// ノードごとに専用（監査#3）— 共有ソケットは group multicast 送信のみ。
/// `creds` は `reload_credentials` で丸ごと差し替わる（`RwLock<Arc<_>>`: 読み手
/// は Arc をクローンして走るので、進行中の確立は旧資格情報で完走し、次の確立
/// から新しい方を使う）。`cfg` は差し替え時に KVS を読み直すための起動設定。
struct CaseEstablisher {
    creds: std::sync::RwLock<Arc<FabricCredentials>>,
    scope_id: u32,
    resolver: Arc<dyn Resolver>,
    cfg: NativeConfig,
}

impl CaseEstablisher {
    /// 現在の資格情報（Arc クローン）。poison は中身をそのまま使う
    /// （SubHealth と同じ規律 — panic 中のスレッドが壊せる不変条件は無い）。
    fn creds(&self) -> Arc<FabricCredentials> {
        Arc::clone(
            &self
                .creds
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// 資格情報を差し替え、`ipk_operational` が変わったかを返す。
    fn swap_credentials(&self, fresh: FabricCredentials) -> bool {
        let mut slot = self
            .creds
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = slot.ipk_operational != fresh.ipk_operational;
        *slot = Arc::new(fresh);
        changed
    }
}

/// reload の identity 照合: fabric_id / node_id / root 公開鍵が起動時と違う
/// store は「別の fabric」なので reload では受けず restart を案内する（warm
/// session・購読・CFID がすべて別物になるため）。
pub(crate) fn check_identity(
    current: &FabricCredentials,
    fresh: &FabricCredentials,
) -> Result<(), MatError> {
    if current.fabric_id != fresh.fabric_id
        || current.node_id != fresh.node_id
        || current.root_public_key != fresh.root_public_key
    {
        return Err(MatError::new(
            ErrorKind::Other,
            "fabric identity changed (fabric_id/node_id/root key) since start-up; restart matd",
        ));
    }
    Ok(())
}
```

(c) `impl Establisher for CaseEstablisher`: `establish` と `establish_subscription` の冒頭に `let creds = self.creds();` を足し、`self.creds.root_public_key` → `creds.root_public_key`、`self.creds.fabric_id` → `creds.fabric_id`、`case::establish_any(&peers, &self.creds, ...)` → `case::establish_any(&peers, &creds, ...)` に置き換える（両メソッドとも）。末尾に追加:

```rust
    fn reload_credentials(&self) -> Result<bool, MatError> {
        let fresh = load_fabric_credentials(&self.cfg)?;
        check_identity(&self.creds(), &fresh)?;
        Ok(self.swap_credentials(fresh))
    }
```

(d) `case_establisher(cfg, creds, resolver)`（L403-419）の構築を変更:

```rust
    Ok(Box::new(CaseEstablisher {
        creds: std::sync::RwLock::new(Arc::new(creds)),
        scope_id,
        resolver,
        cfg: cfg.clone(),
    }))
```

(e) `impl Engine` に委譲メソッドを追加（`with_parts` の直後）:

```rust
    /// 資格情報（IPK を含む）を KVS から読み直して確立器へ差し替える
    /// （`matd reload`）。既存 session には触れない — 次の確立から効く。
    /// 戻り値は `ipk_operational` が変わったか。
    pub fn reload_credentials(&self) -> Result<bool, MatError> {
        self.establisher.reload_credentials()
    }
```

(f) 既存テスト `concurrent_establishes_use_dedicated_sockets`（L1543 付近）の struct literal を `creds: std::sync::RwLock::new(Arc::new(creds))` + `cfg: NativeConfig { store: std::path::PathBuf::from("/nonexistent"), iface: "lo".into(), thread_iface: None, fabric_index: 1, issuer_index: 0 }` に合わせる。

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat-native --lib 2>&1 | tail -5`
Expected: 全 PASS（新規 5 本を含む）。`cargo clippy -p mat-native --all-targets -- -D warnings` も警告ゼロ。

- [ ] **Step 6: spec §4.3 を実装に合わせて訂正**

`docs/superpowers/specs/2026-09-06-matd-reload-design.md` の「### 4.3 `Engine::reload_credentials(&self, cfg: &NativeConfig) -> Result<bool, MatError>`」節を次に置き換える:

```markdown
### 4.3 `Engine::reload_credentials(&self) -> Result<bool, MatError>`

`self.establisher.reload_credentials()` へ委譲するだけ。KVS の読み直し・identity
照合・swap はすべて `CaseEstablisher` の中（`cfg: NativeConfig` を構築時に
控える）:

1. `load_fabric_credentials(&self.cfg)`（既存。KVS 読みは `kvs` の flock 規律に従う）。
2. `check_identity(current, fresh)`: fabric_id / node_id / root 公開鍵の不一致は
   `other`「fabric identity changed ...; restart matd」。
3. `swap_credentials(fresh)` の結果（IPK が変わったか）を返す。

`NativeBackend`（matd）は `engine()` を既に公開しているので追加 API は不要。
matd 側は `NativeConfig` を持ち回らない（確立器が自分で持つ）。
```

- [ ] **Step 7: fmt + コミット**

```bash
cargo fmt -p mat-native
/usr/bin/git add crates/mat-native/src/lib.rs docs/superpowers/specs/2026-09-06-matd-reload-design.md
/usr/bin/git commit -m "feat(mat-native): Establisher::reload_credentials — CaseEstablisher の資格情報を KVS から読み直して差し替える

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1QPi6tbLcbuhj5536tend"
```

---

### Task 2: matd の `Op::Reload`、`ReloadStats`、`status.reloads`、CLI `matd reload`

**Files:**
- Modify: `crates/matd/src/protocol.rs`（`enum Op` L186-201、`node_id()` L214-230、`name()` L244-256、`group_id()` / `endpoint()` / `log_path()` の網羅 match、テスト L700-712 付近）
- Modify: `crates/matd/src/server.rs`（`DaemonInfo` L50-55、`dispatch` L504-560、`to_device_op` L844、L905-921 の網羅 match、`status_body` L950-981、テスト L1004-1040 / L1345-1360）
- Modify: `crates/matd/src/main.rs`（`enum Command` L65-71、`run` L119-125、`DaemonInfo` 初期化 L286-291）

**Interfaces:**
- Consumes: `mat_native::Engine::reload_credentials(&self) -> Result<bool, MatError>`（Task 1）、`NativeBackend::engine()`。
- Produces: ワイヤ `{"op":"reload"}` → `{"reloaded":true,"ipk":"changed"|"unchanged","reload_count":N,"timestamp":...}`、`status` の `"reloads":{"count":N,"last_at":"<ISO>"|null}`、CLI `matd reload`。Task 3 の `hint_reload` はこの応答形を判定する。

- [ ] **Step 1: protocol のテストを書く**

`crates/matd/src/protocol.rs` のテストモジュール、`status_has_no_node_and_matches_wire_tag` の直後に追加:

```rust
    #[test]
    fn reload_has_no_node_and_matches_wire_tag() {
        // admin op（`matd reload` / rotate-ipk 後の mat が送る）。native の確立器
        // の資格情報を差し替えるが、デバイス・ワイヤ・per-node Mutex には触れない。
        let r = parse(r#"{"op":"reload"}"#);
        assert!(matches!(r.op, Op::Reload));
        assert_eq!(r.op.node_id(), None);
        assert_eq!(r.op.group_id(), None);
        assert_eq!(r.op.endpoint(), None);
        assert_eq!(r.op.log_path(), None);
        assert_eq!(r.op.name(), "reload");
    }
```

- [ ] **Step 2: server のテストを書く**

`crates/matd/src/server.rs` の `mod tests`、`dispatch_status_reports_native_unavailable` の直後に追加。`DaemonInfo` を作るヘルパも足す（既存テストの struct literal は Step 4 でこのヘルパに寄せる）:

```rust
    fn test_daemon() -> DaemonInfo {
        DaemonInfo {
            version: "test",
            started: std::time::Instant::now(),
            iface: "lo".into(),
            fabric_index: 2,
            reloads: ReloadStats::default(),
        }
    }

    /// reload に成功する確立器（Task 1 の既定実装を上書き）。establish は使わない。
    struct ReloadOkEstablisher {
        changed: bool,
    }
    #[async_trait::async_trait]
    impl mat_native::Establisher for ReloadOkEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            Err(MatError::new(ErrorKind::Other, "not used"))
        }
        fn reload_credentials(&self) -> Result<bool, MatError> {
            Ok(self.changed)
        }
    }

    /// reload 成功: 応答形・回数・status への反映。
    #[tokio::test]
    async fn dispatch_reload_swaps_credentials_and_counts() {
        let (_dir, store_path) = make_store();
        let native =
            NativeBackend::with_establisher(Box::new(ReloadOkEstablisher { changed: true }));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let daemon = test_daemon();
        let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Event>(8);
        drop(rx);

        let (body, is_shutdown) =
            dispatch(r#"{"op":"reload","id":9}"#, &state, &store_path, &health, &daemon, &events)
                .await;
        assert!(!is_shutdown);
        assert_eq!(body["id"], 9);
        assert_eq!(body["reloaded"], true);
        assert_eq!(body["ipk"], "changed");
        assert_eq!(body["reload_count"], 1);
        assert!(body["timestamp"].is_string());

        let (status, _) =
            dispatch(r#"{"op":"status"}"#, &state, &store_path, &health, &daemon, &events).await;
        assert_eq!(status["reloads"]["count"], 1);
        assert!(status["reloads"]["last_at"].is_string());

        // 2 回目: unchanged でも回数は進む。
        let (body, _) =
            dispatch(r#"{"op":"reload"}"#, &state, &store_path, &health, &daemon, &events).await;
        assert_eq!(body["reload_count"], 2);
    }

    /// 確立器が reload 非対応（既定実装）→ other、回数は進まず status も未 reload。
    #[tokio::test]
    async fn dispatch_reload_unsupported_establisher_is_other_and_not_counted() {
        use crate::native::test_support::FakeEstablisher;
        let (_dir, store_path) = make_store();
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let daemon = test_daemon();
        let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Event>(8);
        drop(rx);

        let (body, _) =
            dispatch(r#"{"op":"reload"}"#, &state, &store_path, &health, &daemon, &events).await;
        assert_eq!(body["error"]["kind"], "other");
        assert!(body["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("not supported"));

        let (status, _) =
            dispatch(r#"{"op":"status"}"#, &state, &store_path, &health, &daemon, &events).await;
        assert_eq!(status["reloads"]["count"], 0);
        assert!(status["reloads"]["last_at"].is_null());
    }

    /// native が起動時 Unavailable → そのエラーをそのまま返す（他 op と同じ規律）。
    #[tokio::test]
    async fn dispatch_reload_reports_native_unavailable() {
        let (_dir, store_path) = make_store();
        let state = NativeState::Unavailable(MatError::store_missing("no KVS materials"));
        let health = SubHealth::new(None);
        let daemon = test_daemon();
        let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Event>(8);
        drop(rx);

        let (body, _) =
            dispatch(r#"{"op":"reload"}"#, &state, &store_path, &health, &daemon, &events).await;
        assert_eq!(body["error"]["kind"], "store_missing");
        assert_eq!(body["error"]["detail"], "no KVS materials");
    }
```

`FakeEstablisher` の import パスは同ファイルの既存テスト（L1364 `use crate::native::test_support::{write_group_fixture_ini, FakeEstablisher};`）に合わせる。`SubHealth::new` の引数形は既存テスト（`SubHealth::new(Some(vec![0x0006]))`）と同じ型なので `None` で wildcard。

また既存テスト `to_device_op_rejects_non_device_ops_without_panic`（L1345）の配列に `Op::Reload,` を足す。

- [ ] **Step 3: テストが失敗（コンパイルエラー）することを確認**

Run: `cargo test -p matd --lib reload 2>&1 | tail -20`
Expected: `error[E0599]: no variant named Reload` / `no field reloads`。

- [ ] **Step 4: 実装**

(a) `protocol.rs` `enum Op` の `NodeTouched` の前に追加:

```rust
    /// 確立器の資格情報（IPK を含む）を KVS から読み直す admin op（`matd reload`、
    /// および `mat fabric rotate-ipk` が commit 後に送る）。native の確立器には
    /// 触るが、デバイス・ワイヤ・per-node Mutex には触れない — Status と同じく
    /// dispatch で短絡する。`node_id()` は None。
    Reload,
```

`node_id()` / `group_id()` / `endpoint()` / `log_path()` / `name()` の網羅 match に `Op::Reload` を admin 群（`Op::Status` の隣）へ足す。`name()` は `Op::Reload => "reload"`。コンパイラの「non-exhaustive patterns」に従って全箇所を埋める（`_ =>` は使わない — 既存が全列挙なので）。

(b) `server.rs` `DaemonInfo` の直前に `ReloadStats` を追加し、`DaemonInfo` にフィールドを足す:

```rust
/// `reload` の回数と最終時刻（`status` の `reloads`）。`&self` で更新できるよう
/// Atomic + Mutex（`DaemonInfo` は `Arc` 共有）。
#[derive(Default)]
pub struct ReloadStats {
    count: std::sync::atomic::AtomicU64,
    last_at: std::sync::Mutex<Option<String>>,
}

impl ReloadStats {
    /// 成功 1 回を記録し、この回を含む累計を返す。
    pub fn record(&self) -> u64 {
        use std::sync::atomic::Ordering;
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        *self
            .last_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(now_iso8601());
        n
    }

    /// `status` 用 snapshot: `{"count": N, "last_at": "<ISO 8601>" | null}`。
    pub fn snapshot(&self) -> Value {
        use std::sync::atomic::Ordering;
        let last_at = self
            .last_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        json!({ "count": self.count.load(Ordering::SeqCst), "last_at": last_at })
    }
}

/// `status` が返すデーモン基本情報（起動時に確定する値 + reload 統計）。
pub struct DaemonInfo {
    pub version: &'static str,
    pub started: std::time::Instant,
    pub iface: String,
    pub fabric_index: u8,
    pub reloads: ReloadStats,
}
```

`now_iso8601` は `dispatch` が既に使っている名前（`Value::String(now_iso8601())`）なので同じ import で足りる。

(c) `dispatch` の `match &req.op` に `Op::NodeTouched` 腕の直後で追加:

```rust
        // 確立器の資格情報を差し替えるだけ（warm session / 購読 / per-node
        // Mutex には触れない）。失敗時はメモリも回数も変えない。
        Op::Reload => reload_body(native, daemon),
```

`status_body` の直前に追加:

```rust
/// `reload`: 確立器へ KVS の資格情報（IPK を含む）を読み直させ、成功なら回数を
/// 進める。`Unavailable`（起動時の構築失敗）は他 op と同じくそのエラーを返す —
/// reload での復帰はスコープ外（restart が唯一の復帰手段）。
fn reload_body(native: &NativeState, daemon: &DaemonInfo) -> Result<Value, MatError> {
    let backend = match native {
        NativeState::Ready(b) => b,
        NativeState::Unavailable(e) => return Err(e.clone()),
    };
    let changed = backend.engine().reload_credentials()?;
    let count = daemon.reloads.record();
    tracing::info!(ipk_changed = changed, reload_count = count, "credentials reloaded from kvs");
    Ok(json!({
        "reloaded": true,
        "ipk": if changed { "changed" } else { "unchanged" },
        "reload_count": count,
    }))
}
```

`status_body` の `json!` に `"reloads": daemon.reloads.snapshot(),` を `"listen_clients"` の前に足す。

(d) `to_device_op`（L844）の admin 群 `Op::Listen { .. } | Op::Ping | Op::Status | Op::Shutdown | Op::NodeTouched { .. }` に `| Op::Reload` を足す。L905-921 の網羅 match にも `| Op::Reload` を足す。他に `Op` を全列挙している match があればコンパイラに従う。

(e) `main.rs`:

```rust
    /// 稼働中 matd に KVS の資格情報（IPK）を読み直させる（socket 経由、
    /// `mat fabric rotate-ipk` の後に。warm session と購読はそのまま）。
    Reload,
```
を `enum Command` に追加し、`run` に `Some(Command::Reload) => admin_op(cli.socket, "reload").await,` を足す。`admin_op` の doc コメント「stop / status:」を「stop / status / reload:」に。`DaemonInfo` 初期化（L286）に `reloads: server::ReloadStats::default(),` を足す。

(f) 既存テスト `dispatch_status_reports_native_unavailable` の `DaemonInfo { ... }` を `test_daemon()` 呼び出しに置き換える（`SubHealth::new(Some(vec![0x0006]))` などその他はそのまま）。

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p matd 2>&1 | tail -5`
Expected: 全 PASS（新規 4 本を含む）。`cargo clippy -p matd --all-targets -- -D warnings` も警告ゼロ。

- [ ] **Step 6: CLI を手で確認（socket 無し）**

Run: `cargo run -q -p matd -- reload --socket /tmp/no-such-matd.sock; echo "exit=$?"`
Expected: stderr に `{"error":{"kind":"other","detail":"matd not running at /tmp/no-such-matd.sock (...)"}}`、`exit=1`（`matd stop` と同じ契約）。

- [ ] **Step 7: fmt + コミット**

```bash
cargo fmt -p matd
/usr/bin/git add crates/matd/src/protocol.rs crates/matd/src/server.rs crates/matd/src/main.rs
/usr/bin/git commit -m "feat(matd): reload admin op + \`matd reload\` — 資格情報（IPK）を KVS から読み直し、status に reloads を載せる

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1QPi6tbLcbuhj5536tend"
```

---

### Task 3: `mat fabric rotate-ipk` の自動 reload（`matd_reload` フィールド）と note の更新

**Files:**
- Modify: `crates/mat/src/matd_client.rs`（`hint_node_touched` L376-398、`send_hint_line` L407-419、テスト L1167-1215）
- Modify: `crates/mat/src/commands/fabric.rs`（`run_rotate_ipk` L116-174、テストモジュール L71-）
- Modify: `crates/mat-native/src/rotate_ipk.rs`（`note()` L123-141、テスト L659）
- Modify: `crates/mat/src/native_direct.rs`（`PROVISION_NOTE` L28-30、L170、テスト L603-655）
- Modify: `crates/mat-native/src/runner.rs`（テスト L341 / L348 の文言）

**Interfaces:**
- Consumes: matd 応答 `{"reloaded":true,...}`（Task 2）、既存 `sockets_from_env_or_default` / `connect_candidates`、`RotateOutcome { status: RotateStatus, .. }` / `RotateOutcome::body(fabric_index) -> Value`。
- Produces:
  - `pub(crate) enum MatdReload { Reloaded, NotRunning, Failed }` + `impl MatdReload { pub(crate) fn as_str(self) -> &'static str }`（`"reloaded"` / `"not_running"` / `"failed"`）
  - `pub(crate) fn hint_reload() -> MatdReload`、`fn hint_reload_at(sockets: &[PathBuf]) -> MatdReload`
  - `fn attach_matd_reload(body: &mut Value, status: &RotateStatus, hint: impl FnOnce() -> MatdReload)`（fabric.rs 内、テスト対象）

- [ ] **Step 1: matd_client のテストを書く**

`crates/mat/src/matd_client.rs` のテスト、`hint_node_touched_is_silent_without_matd` の直後に追加:

```rust
    /// listener を 1 本立て、受けた要求行と固定応答を返すテスト用 matd。
    fn one_shot_matd(reply: &'static [u8]) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap();
            let mut conn = conn;
            conn.write_all(reply).unwrap();
            req
        });
        (dir, path, server)
    }

    #[test]
    fn hint_reload_reports_reloaded_on_ack() {
        let (_dir, path, server) =
            one_shot_matd(b"{\"reloaded\":true,\"ipk\":\"changed\",\"reload_count\":1}\n");
        assert_eq!(hint_reload_at(&[path]), MatdReload::Reloaded);
        let req: Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(req, json!({"op": "reload"}));
    }

    /// 旧 matd（reload 未対応）の parse_error、その他のエラー応答は failed。
    #[test]
    fn hint_reload_reports_failed_on_error_response() {
        let (_dir, path, server) =
            one_shot_matd(b"{\"error\":{\"kind\":\"parse_error\",\"detail\":\"unknown op\"}}\n");
        assert_eq!(hint_reload_at(&[path]), MatdReload::Failed);
        server.join().unwrap();
    }

    #[test]
    fn hint_reload_reports_failed_on_non_json_response() {
        let (_dir, path, server) = one_shot_matd(b"garbage\n");
        assert_eq!(hint_reload_at(&[path]), MatdReload::Failed);
        server.join().unwrap();
    }

    #[test]
    fn hint_reload_reports_not_running_without_matd() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such.sock");
        assert_eq!(hint_reload_at(&[missing]), MatdReload::NotRunning);
    }

    #[test]
    fn matd_reload_as_str_is_the_wire_vocabulary() {
        assert_eq!(MatdReload::Reloaded.as_str(), "reloaded");
        assert_eq!(MatdReload::NotRunning.as_str(), "not_running");
        assert_eq!(MatdReload::Failed.as_str(), "failed");
    }
```

- [ ] **Step 2: fabric.rs のテストを書く**

`crates/mat/src/commands/fabric.rs` のテストモジュール末尾に追加:

```rust
    #[test]
    fn attach_matd_reload_only_after_commit() {
        use crate::matd_client::MatdReload;
        use mat_native::rotate_ipk::RotateStatus;
        use std::cell::Cell;

        // rotated: hint が 1 回呼ばれ、結果が body に載る。
        let called = Cell::new(0);
        let mut body = json!({ "status": "rotated" });
        attach_matd_reload(&mut body, &RotateStatus::Rotated, || {
            called.set(called.get() + 1);
            MatdReload::Reloaded
        });
        assert_eq!(called.get(), 1);
        assert_eq!(body["matd_reload"], "reloaded");

        // controller 側 epoch が変わらない結果では撃たない・載せない。
        for status in [
            RotateStatus::Pending,
            RotateStatus::CaughtUp,
            RotateStatus::CatchUpIncomplete,
            RotateStatus::Aborted,
            RotateStatus::Idle,
        ] {
            let called = Cell::new(0);
            let mut body = json!({ "status": status.as_str() });
            attach_matd_reload(&mut body, &status, || {
                called.set(called.get() + 1);
                MatdReload::Reloaded
            });
            assert_eq!(called.get(), 0, "{}", status.as_str());
            assert!(body.get("matd_reload").is_none(), "{}", status.as_str());
        }
    }
```

- [ ] **Step 3: 既存テストの文言を先に更新（失敗を確認する対象に含める）**

- `crates/mat-native/src/rotate_ipk.rs` L659: `assert!(body["note"].as_str().unwrap().contains("restart"));` → `assert!(body["note"].as_str().unwrap().contains("matd reload"));`
- `crates/mat/src/native_direct.rs` テスト `run_with_engine_provision_attaches_direct_path_note`（L603-655）: 名前を `run_with_engine_provision_has_no_note` に、doc コメントを「直経路 provision も note 無し（matd は送信ごとに KVS の group 資格情報を読むので、restart / reload は不要）」に、最後の assert を `assert!(body.get("note").is_none(), "body={body}");` に。
- `crates/mat-native/src/runner.rs` L341 `Some("restart matd")` → `Some("note text")`、L348 `assert_eq!(body["note"], "restart matd");` → `assert_eq!(body["note"], "note text");`（note 引数の機構自体は残す）。

- [ ] **Step 4: テストが失敗することを確認**

Run: `cargo test -p matterctl --lib hint_reload attach_matd_reload 2>&1 | tail -20`
Expected: コンパイルエラー（`MatdReload` / `hint_reload_at` / `attach_matd_reload` が無い）。
Run: `cargo test -p mat-native --lib rotate_all_ok_commits_and_records_prev 2>&1 | tail -5`
Expected: FAIL（note に `matd reload` が無い）。

- [ ] **Step 5: 実装**

(a) `matd_client.rs`、`hint_node_touched` の直前に追加:

```rust
/// `mat fabric rotate-ipk` が commit 後に matd へ送る `reload` ヒントの結果
/// （body の `matd_reload`）。`node_touched` と同じ best-effort 送信路だが、
/// こちらは応答を読んで状態語にする — 操作者が「稼働中の matd が新 IPK を
/// 取り込んだか」を rotate の出力だけで判断できるようにするため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatdReload {
    /// matd が `{"reloaded":true}` を返した。
    Reloaded,
    /// 候補 socket のどれにも接続できない（matd 不在）。
    NotRunning,
    /// 接続はできたが ack が無い: エラー応答（旧 matd の `parse_error` を含む）、
    /// timeout、非 JSON。
    Failed,
}

impl MatdReload {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MatdReload::Reloaded => "reloaded",
            MatdReload::NotRunning => "not_running",
            MatdReload::Failed => "failed",
        }
    }
}

/// 稼働中の matd に資格情報（IPK）の読み直しを頼む（`mat fabric rotate-ipk` の
/// commit 後）。socket 候補・接続失敗の扱い・300 ms の read 上限は
/// [`hint_node_touched`] と同じ。結果は呼び出し側の exit code に影響しない。
pub(crate) fn hint_reload() -> MatdReload {
    let sockets = sockets_from_env_or_default(std::env::var_os("MAT_MATD_SOCKET"));
    hint_reload_at(&sockets)
}

/// [`hint_reload`] の socket 候補注入版（テスト用に env 非依存の核）。
fn hint_reload_at(sockets: &[PathBuf]) -> MatdReload {
    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            tracing::debug!(error = %detail, "reload hint: matd unreachable");
            return MatdReload::NotRunning;
        }
    };
    match send_reload_line(stream) {
        Ok(true) => MatdReload::Reloaded,
        Ok(false) => {
            tracing::warn!(socket = %socket.display(), "reload hint: matd did not acknowledge (old matd, or reload failed — run `matd reload`)");
            MatdReload::Failed
        }
        Err(e) => {
            tracing::warn!(socket = %socket.display(), error = %e, "reload hint: send/recv failed");
            MatdReload::Failed
        }
    }
}

/// `{"op":"reload"}` を 1 行送り、応答 1 行が `reloaded: true` なら Ok(true)。
/// 応答が JSON でない・`reloaded` が無い/false・timeout は Ok(false)（送受信
/// 自体は成立した）、I/O エラーは Err。
fn send_reload_line(mut stream: UnixStream) -> std::io::Result<bool> {
    let mut line = serde_json::to_vec(&json!({ "op": "reload" }))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    if reader.read_line(&mut resp).is_err() {
        return Ok(false); // timeout 等 = ack 無し
    }
    let acked = serde_json::from_str::<Value>(&resp)
        .ok()
        .and_then(|v| v.get("reloaded")?.as_bool())
        .unwrap_or(false);
    Ok(acked)
}
```

（`Value` / `json!` / `BufReader` / `BufRead` / `Write` / `Duration` は同ファイルの既存 import を使う。無ければ足す。）

(b) `commands/fabric.rs`: `use mat_native::rotate_ipk::{self, RotateIpkParams, RotateMode, RotateStatus};` に変更し、`use crate::matd_client::MatdReload;` を足す。`run_rotate_ipk` の

```rust
    output::emit(outcome.body(cfg.fabric_index));
```
を
```rust
    let mut body = outcome.body(cfg.fabric_index);
    attach_matd_reload(&mut body, &outcome.status, crate::matd_client::hint_reload);
    output::emit(body);
```
に変え、関数の直後に追加:

```rust
/// commit（`rotated`）のときだけ稼働中 matd へ reload を頼み、結果を body の
/// `matd_reload` に載せる。pending / catch-up / abort / idle は controller 側の
/// 現行 epoch が変わらないので撃たない。`hint` は差し替え可能（テスト用）。
fn attach_matd_reload(
    body: &mut Value,
    status: &RotateStatus,
    hint: impl FnOnce() -> MatdReload,
) {
    if matches!(status, RotateStatus::Rotated) {
        body["matd_reload"] = json!(hint().as_str());
    }
}
```

`Value` は `serde_json::Value`（既存 import は `use serde_json::json;` なので `use serde_json::{json, Value};` に）。

(c) `rotate_ipk.rs` `note()` の `Rotated` 文言を次に変更:

```rust
            RotateStatus::Rotated => Some(
                "matd_reload says whether a running matd picked up the new IPK (reloaded / \
                 not_running / failed); on failed, run `matd reload` (or restart matd) before \
                 the next rotation; nodes left out of --nodes need \
                 `mat fabric rotate-ipk --catch-up --nodes <N>`",
            ),
```

(d) `native_direct.rs`: `PROVISION_NOTE` 定数（L28-30）とその doc コメントを削除し、L170 を `mat_native::runner::provision(&runner, engine, p, None).await` に。L603 付近の doc コメント参照も Step 3 の書き換えで消えていることを確認。

- [ ] **Step 6: テストが通ることを確認**

Run: `cargo test -p matterctl -p mat-native 2>&1 | tail -5`
Expected: 全 PASS。`cargo clippy -p matterctl -p mat-native --all-targets -- -D warnings` も警告ゼロ。

- [ ] **Step 7: fmt + コミット**

```bash
cargo fmt -p matterctl -p mat-native
/usr/bin/git add crates/mat/src/matd_client.rs crates/mat/src/commands/fabric.rs crates/mat/src/native_direct.rs crates/mat-native/src/rotate_ipk.rs crates/mat-native/src/runner.rs
/usr/bin/git commit -m "feat(mat): rotate-ipk が commit 後に matd へ reload を頼み matd_reload を返す — provision の restart note は削除

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1QPi6tbLcbuhj5536tend"
```

---

### Task 4: docs / CLAUDE.md の文言更新

**Files:**
- Modify: `docs/commands.md`（rotate-ipk 出力例 L180-187、rotate の matd 段落 L235-238、provision Outputs L905-915、`--rebind` 節 L968-973、matd 節 `matd stop` / `matd status` L1088-1133、「Routing through matd」L1051-）
- Modify: `CLAUDE.md`（Backend 節 L113-116 の rotate-ipk 文）
- Modify: `docs/superpowers/specs/2026-09-05-ipk-rotation-design.md` §6

**Interfaces:** なし（文書のみ）。用語は Task 2/3 の実装どおり: `matd reload`、`{"op":"reload"}`、`reloaded` / `ipk` / `reload_count`、`status.reloads.{count,last_at}`、`matd_reload: reloaded|not_running|failed`。

- [ ] **Step 1: rotate-ipk の出力例と段落**

L180-187 の JSON 例を:

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "fabric_index": 1,
  "status": "rotated",
  "nodes": [ { "node_id": 5, "status": "ok" }, { "node_id": 6, "status": "ok" } ],
  "matd_reload": "reloaded",
  "note": "matd_reload says whether a running matd picked up the new IPK (reloaded / not_running / failed); on failed, run `matd reload` (or restart matd) before the next rotation; nodes left out of --nodes need `mat fabric rotate-ipk --catch-up --nodes <N>`"
}
```

L235-238 の段落「**matd** loads the IPK at start-up and has no reload: …」を次に置き換える:

```markdown
- **matd** loads the IPK at start-up and keeps it in memory. A committed
  rotation (`status: "rotated"`) asks a running `matd` to reload it over the
  socket and reports the outcome as `matd_reload`: `reloaded` (the daemon now
  holds the new IPK — no restart), `not_running` (no daemon answered the
  probed socket), or `failed` (an old `matd` without the reload op, or the
  reload itself failed — run `matd reload`, or restart it). The field is
  informational: it never changes the exit code. Until it reloads, `matd`
  keeps working on the previous epoch (the nodes accept both) but its CASE
  attempts would fail after the *next* rotation drops that epoch. Existing
  warm sessions and resident subscriptions are untouched by the reload; the
  new IPK is used from the next CASE establishment on. `commission` picks the
  new epoch up immediately.
```

- [ ] **Step 2: provision の出力例と `--rebind` 節**

L905-915 の 3 例のうち、`provision --rebind via the direct path also notes ...` と `provision when the controller-side write went native ...` の 2 例（コメント行込み）を削除し、代わりに 1 行コメントを残す:

```json
// provision — all listed nodes succeeded (provision stops at the first failure).
// The same shape on the direct path and through matd: matd re-reads the
// group's credentials from the KVS on every send, so no note, reload or
// restart follows a provision.
{ "timestamp": "...", "group_id": 1, "keyset_id": 42, "name": "living", "endpoint": 1, "nodes": [5, 6, 7], "status": "provisioned" }
```

L968-973 の「After a direct-path `--rebind`, restart `matd` if it is running (it may still hold the old group state in memory; the KVS is already updated) — the output `note` says so (see Outputs above).」を「A direct-path `--rebind` needs no follow-up on `matd`: it re-reads the group's operational credentials from the KVS on every send (see "Pick one group sender" below).」に置き換える。

- [ ] **Step 3: matd 節に `matd reload` と `reloads` を追加**

L1094（`matd stop` のコードブロックの後）に追加:

```markdown
Make a running daemon pick up a rotated IPK with `matd reload` (what
`mat fabric rotate-ipk` sends automatically on commit — see `matd_reload`
there). It re-reads the fabric credentials from the KVS and swaps them into
the CASE establisher; warm sessions and resident subscriptions stay up, and
the new IPK applies from the next establishment on. Group credentials are not
part of it (they are read on every send). A store whose fabric identity
changed (fabric id, node id or root key) is refused with `other` — restart
instead:

```bash
matd reload                           # default socket
matd reload --socket /run/mat/matd.sock
```

```json
{"timestamp": "2026-06-03T12:34:56+09:00", "reloaded": true, "ipk": "changed", "reload_count": 1}
```

`ipk` is `changed` or `unchanged` (the key itself is never printed). If the
native backend failed to build at startup, `reload` returns that error like
every other op (the daemon must be restarted once the store is fixed).
```

`matd status` の JSON 例（L1111-1129）に `"listen_clients": 1,` の前へ
`"reloads": {"count": 1, "last_at": "2026-06-03T12:00:00+09:00"},` を足し、直後の説明段落に
「`reloads` counts successful `matd reload`s since start-up (`last_at` is `null` before the first).」を 1 文足す。

「Routing through `matd`」節（L1051-）の admin op の列挙（`status` / `stop` を挙げている箇所）があれば `reload` を同列に足す。無ければ追加しない。

- [ ] **Step 4: CLAUDE.md と旧 spec**

`CLAUDE.md` L113-116 の「IPK rotation is `mat fabric rotate-ipk` (direct-only; pending / prev epochs live at ...; the controller switches only after every listed node holds both epochs).」の末尾に「A running `matd` picks the new IPK up via the `reload` admin op (`matd reload`; rotate-ipk sends it on commit and reports `matd_reload`) — no restart.」を足す。

`docs/superpowers/specs/2026-09-05-ipk-rotation-design.md` §6 の「**次回ローテーションの前に必ず restart** が必要 — `note` と docs に明記。」の直後に「（2026-09-06 追記: `matd reload` で置き換え済み — `docs/superpowers/specs/2026-09-06-matd-reload-design.md`）」を足す。

- [ ] **Step 5: 残存する「restart」文言の確認**

Run: `/usr/bin/grep -rn "restart" docs/commands.md CLAUDE.md README.md crates/*/src | /usr/bin/grep -i "ipk\|group state\|reload group" | /usr/bin/grep -v "spec\|matd reload\|or restart\|restart instead\|restart matd\"" `
Expected: 出力なし（rotate / provision 由来の「restart matd」案内が残っていない）。残っていれば本 Task の趣旨に沿って直す。

- [ ] **Step 6: コミット**

```bash
/usr/bin/git add docs/commands.md CLAUDE.md docs/superpowers/specs/2026-09-05-ipk-rotation-design.md
/usr/bin/git commit -m "docs: matd reload — rotate-ipk の matd_reload、matd status の reloads、provision の restart 案内を削除

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1QPi6tbLcbuhj5536tend"
```

---

### Task 5: e2e-device-m3 に reload ステップを足して実行

**Files:**
- Modify: `scripts/e2e-device-m3.sh`（L324-347 の established 待ちを関数化、L347 と L349 の間に reload / rotate ステップ）

**Interfaces:**
- Consumes: `matd reload --socket`、`matd status` の `reloads.count`、`mat fabric rotate-ipk` の `matd_reload`（Task 2/3）。既存ヘルパ `json_get`（flat top-level 用）、`matd_node_state`。

- [ ] **Step 1: established 待ちを関数化**

L324-347 のブロックを関数 `wait_matd_established()` に切り出し、元の位置では関数を呼ぶ:

```bash
# matd の node $NODE_ID への常駐 Subscribe が established になるまで待つ
# （budget TIMEOUT_S 秒）。起動直後と、rotate-ipk の proof CASE が matv の唯一
# session を奪った後の再確立の両方で使う。
wait_matd_established() {
    local why="$1"
    echo "==> waiting for matd's resident Subscribe to node $NODE_ID ($why; established, budget ${TIMEOUT_S}s)" >&2
    local established="" status_json="" state deadline=$((SECONDS + TIMEOUT_S))
    while ((SECONDS < deadline)); do
        if ! kill -0 "$MATD_PID" 2>/dev/null; then
            echo "matd exited while waiting for the subscription:" >&2
            cat "$MATD_STDERR" >&2
            exit 1
        fi
        status_json="$(./target/release/matd status --socket "$MATD_SOCK" 2>/dev/null)" || true
        state="$(matd_node_state "$status_json" "$NODE_ID")"
        if [[ "$state" == "established" ]]; then
            established=1
            break
        fi
        sleep 0.3
    done
    if [[ -z "$established" ]]; then
        echo "matd status (last seen): $status_json" >&2
        echo "timed out waiting for matd's subscription to node $NODE_ID to reach established ($why)" >&2
        exit 1
    fi
    echo "==> matd subscription to node $NODE_ID: established ($why)" >&2
}
```

関数定義は `matd_node_state` の定義の後（L120 付近、`cleanup` の前）に置き、元の位置（L324）は `wait_matd_established "start-up"` の 1 行にする。

- [ ] **Step 2: reload / rotate ステップを追加**

`wait_matd_established "start-up"` の直後、`LISTEN_TIMEOUT_MS=` の前に追加:

```bash
# `reloads.count` を status JSON から取る（jq 無し環境向けに python3 でも）。
matd_reload_count() {
    printf '%s' "$1" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
print((d.get("reloads") or {}).get("count", ""))
'
}

echo "==> matd reload (same store: expect ipk=unchanged, reload_count=1, subscription untouched)" >&2
RELOAD_JSON="$(./target/release/matd reload --socket "$MATD_SOCK")"
echo "$RELOAD_JSON"
# json_get は jq なら `true`、python3 フォールバックなら `True` を出す — 両方を受ける。
[[ "$(json_get reloaded "$RELOAD_JSON")" =~ ^[Tt]rue$ ]] || { echo "reload not acked: $RELOAD_JSON" >&2; exit 1; }
[[ "$(json_get ipk "$RELOAD_JSON")" == "unchanged" ]] || { echo "ipk should be unchanged: $RELOAD_JSON" >&2; exit 1; }
[[ "$(json_get reload_count "$RELOAD_JSON")" == "1" ]] || { echo "reload_count should be 1: $RELOAD_JSON" >&2; exit 1; }
STATUS_JSON="$(./target/release/matd status --socket "$MATD_SOCK")"
[[ "$(matd_reload_count "$STATUS_JSON")" == "1" ]] || { echo "status.reloads.count should be 1: $STATUS_JSON" >&2; exit 1; }
[[ "$(matd_node_state "$STATUS_JSON" "$NODE_ID")" == "established" ]] || { echo "reload must not touch the subscription: $STATUS_JSON" >&2; exit 1; }
echo "==> PASS: matd reload (unchanged) kept the subscription up" >&2

echo "==> mat fabric rotate-ipk (direct path; matv accepts KeySetWrite(0)) — expect matd_reload=reloaded" >&2
ROTATE_JSON="$(MAT_STORE="$MAT_STORE_DIR" MAT_MATD_SOCKET="$MATD_SOCK" ./target/release/mat --iface "$IFACE" fabric rotate-ipk)"
echo "$ROTATE_JSON"
[[ "$(json_get status "$ROTATE_JSON")" == "rotated" ]] || { echo "rotate-ipk did not commit: $ROTATE_JSON" >&2; exit 1; }
[[ "$(json_get matd_reload "$ROTATE_JSON")" == "reloaded" ]] || { echo "matd_reload should be reloaded: $ROTATE_JSON" >&2; exit 1; }
STATUS_JSON="$(./target/release/matd status --socket "$MATD_SOCK")"
[[ "$(matd_reload_count "$STATUS_JSON")" == "2" ]] || { echo "status.reloads.count should be 2 after rotate: $STATUS_JSON" >&2; exit 1; }
echo "==> PASS: rotate-ipk committed and matd reloaded the new IPK (count=2)" >&2

# rotate の proof CASE が matv の唯一 session を奪うので購読は一度落ちる。
# 再確立（= reload 後の新 IPK での cold establish）を待ってから先へ進む。
wait_matd_established "after rotate-ipk"

echo "==> mat on via matd after the rotation (matd must establish with the new IPK)" >&2
ON_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat on --node "$NODE_ID" --endpoint "$DEVICE_EP" --matd "$MATD_SOCK")"
echo "$ON_JSON" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat off --node "$NODE_ID" --endpoint "$DEVICE_EP" --matd "$MATD_SOCK" >&2
echo "==> PASS: unicast through matd after the rotation" >&2
wait_matd_established "after post-rotate ops"
```

`mat on` / `mat off` を挟むのは、既存の listen ステップが「`mat on` で on-off=true のイベントを待つ」ため、事前に off へ戻しておく必要があるから。

- [ ] **Step 3: シェル構文チェック**

Run: `bash -n scripts/e2e-device-m3.sh && echo OK`
Expected: `OK`

- [ ] **Step 4: e2e を実行**

Run: `MAT_E2E_IFACE=${MAT_E2E_IFACE:-eth1} bash scripts/e2e-device-m3.sh 2>&1 | tail -40`
（iface は環境依存。`ip -o link | awk -F': ' '{print $2}'` で up かつ multicast な非 loopback を選ぶ。既存の m3 が通っていた環境設定に合わせる。）
Expected: `==> PASS: matd reload (unchanged) kept the subscription up`、`==> PASS: rotate-ipk committed and matd reloaded the new IPK (count=2)`、`==> PASS: unicast through matd after the rotation`、そして既存の最終 `ALL PASS` 行。

失敗したら原因を切り分ける（matd の stderr は `$WORKDIR/matd.stderr.log`）。想定される罠: (1) rotate の直後 matd の購読が `down` のまま backoff 中 → `wait_matd_established` の budget（TIMEOUT_S、既定 30 秒）内に戻るはず。5 秒 backoff × 数回で足りなければ `MAT_E2E_TIMEOUT_S=60`。(2) `json_get` の bool 表記（Step 2 末尾の注記）。

- [ ] **Step 5: コミット**

```bash
/usr/bin/git add scripts/e2e-device-m3.sh
/usr/bin/git commit -m "test(e2e): m3 に matd reload — unchanged reload、rotate-ipk 経由の自動 reload、購読維持と新 IPK での再確立

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01D1QPi6tbLcbuhj5536tend"
```

---

## 最終工程（メインセッションが実施、subagent には任せない）

1. `task check`（fmt:check + clippy + test）合格を確認。
2. 実機スモーク（hogar-matd コンテナ）: 他セッション（mat-af）へ事前連絡してから。matd を新ビルドで差し替える必要があるため despliegue skill の手順で hogar-matd をデプロイ → `docker exec hogar-matd matd --socket /run/matd/matd.sock reload` → `ipk: unchanged` / `reload_count: 1` → `matd status` で購読 19 本 `established` のまま・`reloads.count == 1` → matd 経由の warm read が通る。本番 fabric で rotate / provision は回さない。
3. main へ rebase → `git merge --no-ff worktree-matd-reload` → push。
4. メモリ更新: jarvis-matd-deploy の「rotate → matd restart → 検証」を「rotate（`matd_reload: reloaded` を確認）→ 検証」へ。
