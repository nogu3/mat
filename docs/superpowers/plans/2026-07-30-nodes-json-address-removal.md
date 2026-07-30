# nodes.json address 削除 + atomic write 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `NodeRecord.address` と `mat commission --target` を撤去して stale アドレスによる診断誤りを構造的に無くし、同時に nodes.json / aliases.toml の書き込みを atomic 化する（Issue #18 + 監査 Tier 3、1.13.0）。

**Architecture:** 読み出し側から順に address 依存を剥がし（discover → diag）、次に書き込み側（--target）、最後にフィールド本体を消す。各タスクが独立にコンパイル・テスト可能。atomic write は独立タスクで先行。

**Tech Stack:** Rust (stable), serde/serde_json, clap 4, tempfile（テスト）。spec: `docs/superpowers/specs/2026-07-30-nodes-json-address-removal-design.md`

## Global Constraints

- ブランチ `fix/issue-18-nodes-json-address` で作業（main 直コミット禁止）。
- 各タスク完了時に `cargo test -p <crate>`、最終タスクで `task check`（fmt:check + clippy -D warnings + 全テスト）。
- stdout 純 JSON / stderr tracing / 台帳以外の状態を持たない、の設計ルールを壊さない。
- コミットメッセージは既存スタイル（`feat(mat-core): ...` / `fix: ...`、日本語本文可）。
- **実機 E2E（jarvis）合格までは main にマージしない**（リポジトリ運用ルール）。

---

### Task 1: mat-core に `fsatomic::write_atomic` 新設 + nodes.json / aliases.toml へ適用

**Files:**
- Create: `crates/mat-core/src/fsatomic.rs`
- Modify: `crates/mat-core/src/lib.rs`（`pub mod fsatomic;` 追加、モジュール一覧のアルファベット順を維持: `error` と `group` の間）
- Modify: `crates/mat-core/src/store.rs:117-128`（`save_ledger`）
- Modify: `crates/mat-core/src/alias.rs:344-365`（`insert_node_alias` の書き込み部）

**Interfaces:**
- Produces: `mat_core::fsatomic::write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()>` — 同一ディレクトリ `.tmp` + fsync + rename。後続タスクは使わない（このタスクで完結）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/fsatomic.rs` を新規作成（新規モジュールはコンパイルが通らないとテストも走らないため、実装とテストを一緒に書き、Step 2 で挙動を確認する）:

```rust
//! Atomic なファイル置換（tmp + fsync + rename）。
//!
//! `std::fs::write` は O_TRUNC 上書きのため、電源断・クラッシュのタイミングで
//! ファイル全体を失う。`mat-controller` の group counter persist と同じ規律を
//! 台帳（nodes.json）と aliases.toml に適用するための共有ヘルパ。

use std::io::{self, Write};
use std::path::Path;

/// `path` と同一ディレクトリの `.tmp` へ書き込み → fsync → rename で置換する。
/// 途中で落ちても既存ファイルは無傷（tmp が残るだけ）。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_content_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes.json");
        write_atomic(&path, b"{\"v\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"v\":1}");
        assert!(!dir.path().join("nodes.tmp").exists());
    }

    #[test]
    fn replaces_existing_content_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliases.toml");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!dir.path().join("aliases.tmp").exists());
    }
}
```

`crates/mat-core/src/lib.rs` のモジュール一覧に `pub mod fsatomic;` を追加（`pub mod error;` の直後）。

- [ ] **Step 2: テスト実行**

Run: `cargo test -p mat-core fsatomic`
Expected: PASS（2 件）

- [ ] **Step 3: `save_ledger` を atomic 化**

`crates/mat-core/src/store.rs` の `save_ledger` の `std::fs::write(...)` 呼び出しを置換:

```rust
    fn save_ledger(&self) -> Result<(), MatError> {
        let path = Self::ledger_path(&self.root);
        let text = serde_json::to_string_pretty(&self.ledger).map_err(|e| {
            MatError::new(ErrorKind::Other, format!("cannot serialize ledger: {e}"))
        })?;
        crate::fsatomic::write_atomic(&path, text.as_bytes()).map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("cannot write {}: {e}", path.display()),
            )
        })
    }
```

`store.rs` の既存テスト `upsert_then_persists_and_reloads` に tmp 不在の assert を 1 行追加:

```rust
        // 再オープンして永続を確認。
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store.require_node(7).unwrap().address.as_deref(),
            Some("192.0.2.10")
        );
        // atomic write の tmp が残らないこと。
        assert!(!dir.path().join("nodes.tmp").exists());
```

- [ ] **Step 4: `insert_node_alias` を atomic 化**

`crates/mat-core/src/alias.rs` の `insert_node_alias` 内 `std::fs::write(&path, text)` を置換:

```rust
        crate::fsatomic::write_atomic(&path, text.as_bytes()).map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("cannot write {}: {e}", path.display()),
            )
        })?;
```

（エラー kind / detail 形式は従来どおり。既存の roundtrip テスト `insert_node_alias_creates_file_and_roundtrips` がそのまま回帰テストになる。）

- [ ] **Step 5: テスト実行**

Run: `cargo test -p mat-core`
Expected: 全 PASS

- [ ] **Step 6: Commit**

```bash
git add crates/mat-core/src/fsatomic.rs crates/mat-core/src/lib.rs crates/mat-core/src/store.rs crates/mat-core/src/alias.rs
git commit -m "feat(mat-core): write_atomic ヘルパ — nodes.json/aliases.toml の torn write 解消（監査 Tier 3）"
```

---

### Task 2: discover — ledger address を出力から撤去、`stale` 廃止、`reachability::resolve` 簡素化

**Files:**
- Modify: `crates/mat-core/src/reachability.rs`（`resolve` から `ledger_address` 引数を削除）
- Modify: `crates/mat/src/commands/discover.rs:91-120`（出力分岐）と冒頭 doc comment

**Interfaces:**
- Produces: `mat_core::reachability::resolve(node_id: u64, instances: &[MatterInstance]) -> NodeReachability`（`NodeReachability { reachable: bool, live_address: Option<String> }` は不変）。Task 3 が diag の ping6 宛先取得に同じ関数を使う。

- [ ] **Step 1: reachability のテストを新シグネチャへ書き換え（先に RED を確認）**

`crates/mat-core/src/reachability.rs` のテストを次の 3 件に置き換える（`matched_prefers_ledger_address_when_present` は挙動ごと削除）:

```rust
    #[test]
    fn matched_returns_reachable_with_first_live_address() {
        let instances = [inst(5, &["192.0.2.99"])];
        let r = resolve(5, &instances);
        assert!(r.reachable);
        assert_eq!(r.live_address, Some("192.0.2.99".to_string()));
    }

    #[test]
    fn not_matched_returns_unreachable() {
        let instances = [inst(255, &["192.0.2.50"])];
        let r = resolve(5, &instances);
        assert!(!r.reachable);
        assert_eq!(r.live_address, None);
    }

    #[test]
    fn matched_announce_only_is_reachable_without_address() {
        let instances = [inst(5, &[])];
        let r = resolve(5, &instances);
        assert!(r.reachable);
        assert_eq!(r.live_address, None);
    }
```

Run: `cargo test -p mat-core reachability`
Expected: コンパイルエラー（引数が 3 個のまま）— これが RED。

- [ ] **Step 2: `resolve` 本体を書き換え**

```rust
/// `node_id` でライブインスタンスに照合する。
pub fn resolve(node_id: u64, instances: &[MatterInstance]) -> NodeReachability {
    let matched: Vec<&MatterInstance> = instances.iter().filter(|i| i.node_id == node_id).collect();
    if matched.is_empty() {
        return NodeReachability {
            reachable: false,
            live_address: None,
        };
    }
    // 最初の非空アドレスの先頭を採る（announce のみなら None）。
    let live = matched
        .iter()
        .flat_map(|i| i.addresses.iter())
        .next()
        .cloned();
    NodeReachability {
        reachable: true,
        live_address: live,
    }
}
```

`live_address` の doc comment（「台帳アドレスが一致…」の 2 行）を「一致インスタンスの先頭アドレス（無ければ None）」へ更新。ファイル冒頭 doc の照合説明は node_id ベースのままなので変更不要。

Run: `cargo test -p mat-core reachability`
Expected: PASS（3 件）

- [ ] **Step 3: discover の出力分岐を書き換え**

`crates/mat/src/commands/discover.rs` の `for n in store.nodes()` ループ内 `match (probe, instances.as_deref())` を置換:

```rust
        match (probe, instances.as_deref()) {
            // 既定: 台帳そのまま（address は台帳から消えるため出力しない）。
            (false, _) => {}
            // --probe だがプローブ実施不能 → 到達性不明。
            (true, None) => {
                obj.insert("reachable".into(), Value::Null);
            }
            // --probe 成功 → node_id 照合で到達性判定。address はライブ解決値のみ。
            (true, Some(list)) => {
                let r = resolve(n.node_id, list);
                obj.insert("reachable".into(), json!(r.reachable));
                if let Some(addr) = r.live_address {
                    obj.insert("address".into(), json!(addr));
                }
            }
        }
```

ファイル冒頭 doc comment の「`reachable`（true/false/null）と、不達時の `stale` を付与する」を「`reachable`（true/false/null）を付与し、到達時のみライブ解決アドレスを `address` に出す（`stale` は 1.13.0 で廃止 — 台帳が address を持たなくなったため）」へ更新。

- [ ] **Step 4: ビルド・テスト**

Run: `cargo test -p mat-core -p mat`
Expected: 全 PASS（discover はユニットテストなし。`mat/tests/integration.rs` の discover 系はサンドボックス環境ではネットワーク不可の挙動を見るもので address 出力に依存しない）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-core/src/reachability.rs crates/mat/src/commands/discover.rs
git commit -m "feat: discover から台帳 address を撤去 — --probe はライブ解決値のみ、stale 廃止（Issue #18）"
```

---

### Task 3: diag node --deep — 「mDNS 解決 → ping6」へ反転、node_id 照合へ一本化

**Files:**
- Modify: `crates/mat/src/commands/diag.rs:47-190`（`node` と `deep_probes`）

**Interfaces:**
- Consumes: `mat_core::reachability::resolve(node_id, &instances).live_address`（Task 2）
- Produces: なし（コマンド出力のみ）。`unavailable` の ip チェック kind に `"mdns_unresolved"` を新設（`"no_address_in_store"` は消滅）。

- [ ] **Step 1: `node()` から address 読み出しを撤去**

`crates/mat/src/commands/diag.rs` の `node()`:

```rust
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
```

（`let rec = ...` / `let address = rec.address.clone();` を削除。`require_node` は存在チェックとして残す。）

`deep_probes` 呼び出しから `address` 引数を外す:

```rust
    if deep {
        deep_probes(
            &mut checks,
            &mut unavailable,
            node_id,
            self_cfid,
            cfg,
            store.root(),
        );
    }
```

- [ ] **Step 2: `deep_probes` を反転**

シグネチャから `address: Option<String>` を削除し、本体を「mdns → ping6」順に書き換える:

```rust
/// `--deep` の補助プローブ。mDNS 広告確認（native の targeted resolve）を先に
/// 実施し、解決できたライブアドレスへ ping6（IP生存）する。台帳は address を
/// 持たない（Issue #18 で撤去）ため、stale 値への誤診経路は存在しない。
fn deep_probes(
    checks: &mut Checks,
    unavailable: &mut Vec<Value>,
    node_id: u64,
    self_cfid: Option<String>,
    cfg: &crate::native_direct::Config<'_>,
    store_root: &Path,
) {
    // mdns: native targeted resolve（dnssd 一本）。照合は node_id ベース。
    // ping6 の宛先もここで解決したライブアドレスから得る。
    let live_target = match crate::probe::mdns(crate::probe::NativeProbe {
        iface: cfg.iface,
        fabric_index: cfg.fabric_index,
        issuer_index: cfg.issuer_index,
        store_root,
        node_ids: std::slice::from_ref(&node_id),
    }) {
        Ok(instances) => {
            let advertised_any_fabric = instances.iter().any(|i| i.node_id == node_id);
            let advertised_self_fabric = self_cfid.as_ref().map(|cfid| {
                instances
                    .iter()
                    .any(|i| &i.compressed_fabric == cfid && i.node_id == node_id)
            });
            if self_cfid.is_none() {
                // native 経路では self_cfid は常に取れる（fabric 資材から算出）ため
                // 実質到達しない。防御的に残す。
                unavailable.push(json!({
                    "check": "mdns_self_fabric",
                    "kind": "cfid_unavailable",
                    "detail": "could not obtain self compressed-fabric-id"
                }));
            }
            checks.mdns = Some(MdnsCheck {
                advertised_self_fabric,
                advertised_any_fabric,
            });
            mat_core::reachability::resolve(node_id, &instances).live_address
        }
        Err(e) => {
            let kind_val = probe_error_kind(&e);
            unavailable.push(json!({ "check": "mdns", "kind": kind_val, "detail": e.detail }));
            None
        }
    };

    // ip: いま広告されているライブアドレスへ ping6。解決できなければ実施不能。
    match live_target.as_deref() {
        Some(addr) => match probe_ping6(addr) {
            Ok(stats) => {
                checks.ip = Some(IpCheck {
                    ok: stats.loss_pct < 100,
                    loss_pct: stats.loss_pct,
                    rtt_ms: stats.rtt_ms,
                    method: "ping6",
                })
            }
            Err(e) => {
                let kind_val = probe_error_kind(&e);
                unavailable.push(json!({ "check": "ip", "kind": kind_val, "detail": e.detail }))
            }
        },
        None => unavailable.push(json!({ "check": "ip", "kind": "mdns_unresolved" })),
    }
}
```

ファイル冒頭 doc comment の「`--deep` の補助プローブ（ping6 / native mDNS targeted resolve）」の記述はそのままで正しい（順序は書かれていない）。

- [ ] **Step 3: ビルド・テスト**

Run: `cargo test -p mat`
Expected: 全 PASS（`derive_verdict` は純関数のまま無変更。mat-core の diag テストも無変更で通ること）

- [ ] **Step 4: Commit**

```bash
git add crates/mat/src/commands/diag.rs
git commit -m "feat: diag node --deep を mDNS 解決→ping6 に反転 — 台帳 address 依存を撤去（Issue #18）"
```

---

### Task 4: `mat commission --target` 撤去

**Files:**
- Modify: `crates/mat/src/cli.rs:94-99`（`Commission` variant）
- Modify: `crates/mat/src/main.rs:168-183`（dispatch）
- Modify: `crates/mat/src/resolve.rs:24-46`（`Commission` arm）
- Modify: `crates/mat/src/matd_client.rs:1033-1041`（テストのコンストラクタ）
- Modify: `crates/mat/src/commands/commission.rs`（`run` / `record_success` シグネチャ、冒頭 doc）

**Interfaces:**
- Produces: `commands::commission::run(store_path, setup_code, node_id, alias, native, thread_dataset, transport)`（`target` 引数消滅）。`record_success` は当面 `address: None` を書く（フィールド本体は Task 5 で削除）。

- [ ] **Step 1: cli.rs から `target` フィールドを削除**

`Commission` variant から次の 3 行を削除:

```rust
        /// 対象の IP アドレスまたは DNS-SD ホスト名。
        #[arg(long, value_name = "HOST")]
        target: String,
```

- [ ] **Step 2: コンパイルエラーを追って全パターンマッチを更新**

Run: `cargo build -p mat 2>&1 | head -40` でエラー箇所を確認し、以下をすべて更新:

`crates/mat/src/main.rs`（dispatch。`main.rs:33` の validate ブロックは `..` で受けており無変更）:

```rust
        Command::Commission {
            setup_code,
            node_id,
            alias,
            thread_dataset,
            transport,
        } => commands::commission::run(
            &store_path,
            setup_code,
            *node_id,
            alias.as_deref(),
            native_cfg.as_ref(),
            thread_dataset.as_deref(),
            *transport,
        ),
```

`crates/mat/src/resolve.rs`（`Commission` arm — 分配と再構築の両方から `target` を外す）:

```rust
        Command::Commission {
            setup_code,
            node_id,
            alias,
            thread_dataset,
            transport,
        } => {
            // 名前の妥当性・重複は commission 開始前に検証する（開始後に alias
            // 書き込みだけ失敗する中途半端な状態を作らない）。
            if let Some(name) = &alias {
                book.validate_new_node_alias(name)?;
            }
            Command::Commission {
                setup_code,
                node_id,
                alias,
                thread_dataset,
                transport,
            }
        }
```

`crates/mat/src/matd_client.rs` のテスト `discover_and_commission_are_unsupported`:

```rust
        assert!(to_op(&Command::Commission {
            setup_code: "MT:DUMMY".into(),
            node_id: None,
            alias: None,
            thread_dataset: None,
            transport: crate::cli::TransportArg::Auto,
        })
        .is_err());
```

`crates/mat/src/commands/commission.rs`:
- `run` のシグネチャから `target: &str,` を削除し、末尾を `record_success(&mut store, node_id, alias)` に。
- `record_success` から `target: &str,` 引数を削除し、`address: Some(target.to_string()),` を `address: None,` に（フィールド削除は Task 5）。
- 冒頭 doc comment の「`target`（IP/DNS）は台帳のメタとして記録する。」の一文を削除し、「コード内の discriminator から mDNS でノードを自前探索する。」だけ残す。

- [ ] **Step 3: ビルド・テスト**

Run: `cargo test -p mat`
Expected: 全 PASS

- [ ] **Step 4: Commit**

```bash
git add crates/mat/src/cli.rs crates/mat/src/main.rs crates/mat/src/resolve.rs crates/mat/src/matd_client.rs crates/mat/src/commands/commission.rs
git commit -m "feat!: mat commission --target を撤去 — ワイヤ未使用の台帳メタ専用引数（Issue #18）"
```

---

### Task 5: `NodeRecord.address` フィールド削除 + 旧形式互換ピン

**Files:**
- Modify: `crates/mat-core/src/store.rs`（フィールド削除 + 互換テスト追加 + 既存テスト更新）
- Modify: `crates/mat/src/commands/commission.rs`（`address: None,` 行を削除）
- Modify: `crates/matd/src/server.rs`（テストフィクスチャ 3 箇所: 1603/1619/1987 付近）
- Modify: `crates/matd/src/subscription.rs`（テストフィクスチャ 3 箇所: 765/1639/1679 付近）
- Modify: `crates/matd/tests/integration.rs:31` 付近（フィクスチャ）
- 注意: `crates/mat/tests/integration.rs:44` と `crates/mat/tests/matd_auto.rs:42` の **raw JSON フィクスチャ（address 付き）は意図的にそのまま残す** — 旧形式ファイルが読めることの実地ピンになる。

**Interfaces:**
- Produces: `NodeRecord { node_id: u64, commissioned_at: String }`（最終形）。

- [ ] **Step 1: 互換テストを先に書く（現行コードでも通ることを確認してから削除に進む）**

`crates/mat-core/src/store.rs` の tests に追加:

```rust
    #[test]
    fn old_format_ledger_with_address_parses_and_sheds_it_on_save() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("nodes.json"),
            r#"{"version":1,"nodes":{"5":{"node_id":5,"address":"192.0.2.10","commissioned_at":"2026-01-01T00:00:00+09:00"}}}"#,
        )
        .unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.require_node(5).unwrap().node_id, 5);
        // 次の台帳書き込みで旧フィールドは自然に消える。
        store
            .upsert_node(NodeRecord {
                node_id: 6,
                commissioned_at: "2026-01-02T00:00:00+09:00".into(),
            })
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join("nodes.json")).unwrap();
        assert!(!text.contains("address"));
    }
```

Run: `cargo test -p mat-core old_format`
Expected: コンパイルエラー（`NodeRecord` に `address` が必須のため）— これが RED。

- [ ] **Step 2: フィールドを削除**

`crates/mat-core/src/store.rs` の `NodeRecord`:

```rust
/// commission 済みノード1件の台帳エントリ。
///
/// address は保存しない（Issue #18 で撤去）: 実行時は常に mDNS 解決であり、
/// 保存した IP は Thread prefix 再構成で原理的に stale になる。旧形式ファイル
/// （address 付き）は serde が未知フィールドを無視して読め、次の書き込みで消える。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeRecord {
    pub node_id: u64,
    /// commission 完了時刻（ISO 8601）。
    pub commissioned_at: String,
}
```

- [ ] **Step 3: コンパイルエラーを追って全コンストラクタを更新**

Run: `cargo test --workspace 2>&1 | grep -E "^error" | head -20` で洗い出し、各 `NodeRecord { ... }` から `address: ...` 行を削除する:

- `crates/mat/src/commands/commission.rs` `record_success`（`address: None,` を削除）
- `crates/mat-core/src/store.rs` テスト `upsert_then_persists_and_reloads`（`address` 行削除、assert を `node_id` ベースへ:）

```rust
        // 再オープンして永続を確認。
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.require_node(7).unwrap().node_id, 7);
        // atomic write の tmp が残らないこと。
        assert!(!dir.path().join("nodes.tmp").exists());
```

- `crates/matd/src/server.rs` テストの `make_store` / `store_with_node_5` / 1987 付近の 3 フィクスチャ
- `crates/matd/src/subscription.rs` テストの 765 / 1639 / 1679 付近の 3 フィクスチャ
- `crates/matd/tests/integration.rs` の `make_store`

いずれも `address: Some("192.0.2.10".into()),` の 1 行を消すだけ。

- [ ] **Step 4: 全テスト実行**

Run: `cargo test --workspace`
Expected: 全 PASS（`crates/mat/tests/integration.rs` / `matd_auto.rs` の address 付き raw JSON フィクスチャが**変更なしで**通ること = 旧形式互換の実地確認）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-core/src/store.rs crates/mat/src/commands/commission.rs crates/matd/src/server.rs crates/matd/src/subscription.rs crates/matd/tests/integration.rs
git commit -m "feat!: NodeRecord.address を削除 — 台帳は node_id と commissioned_at のみ（Issue #18）"
```

---

### Task 6: README / ARCHITECTURE 更新 + 1.13.0 バンプ + 最終チェック

**Files:**
- Modify: `README.md`（下記 5 箇所）
- Modify: `ARCHITECTURE.md:118`
- Modify: `Cargo.toml`（workspace version `1.12.0` → `1.13.0`; `Cargo.lock` はビルドで追従）

**Interfaces:** なし（ドキュメントのみ）。

- [ ] **Step 1: README の commission 使用例から `--target` を削除**

対象行 68 / 100 / 194 / 1178 / 1425。例（line 68）:

```bash
mat commission --setup-code "MT:Y.K9042C00KA0648G00" --node 5
```

line 100（BLE 直行の例）:

```bash
# force BLE (skip mDNS entirely)
mat commission --setup-code "MT:Y.K9042C00KA0648G00" --transport ble
```

line 1425 も同様に `--target <device-ip-or-host>` を落とす。

- [ ] **Step 2: README の discover 出力例を更新（line 105-140 付近）**

台帳のみの例（line 110）から `"address": "192.0.2.10",` を削除:

```json
    { "state": "commissioned", "node_id": 5, "commissioned_at": "2026-06-06T12:00:00+09:00" }
```

`--probe` の注釈（line 119-126）を置換:

```markdown
- `reachable: true` — advertising now; `address` is the live-resolved value.
- `reachable: false` — not advertising; no `address` (the ledger stores none —
  addresses are resolved live, never persisted).
- `reachable: null` — the mDNS probe could not run (e.g. an interface I/O
  error); reachability is unknown. A diagnostic is logged to stderr.
```

`--probe` の出力例（line 128-136）を置換:

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "devices": [
    { "state": "commissioned", "node_id": 5, "address": "192.0.2.99", "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": true },
    { "state": "commissioned", "node_id": 7, "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": false },
    { "state": "commissioned", "node_id": 9, "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": null }
  ]
}
```

直後の「Without `--probe` the output is unchanged (no `reachable` / `stale`); ...」を「Without `--probe` the output is unchanged (no `reachable`); ...」へ。

- [ ] **Step 3: README の diag node --deep 記述を更新（line 380-412 付近）**

- line 382: `mat diag node --node 5 --deep     # also probe native targeted mDNS + ping6 (to the live-resolved address)`
- verdict 注釈ブロック内「`--deep` adds the ping6 and mDNS evidence that distinguishes them.」を次へ置換:

```markdown
> `fabric_missing` (the device dropped our fabric); `--deep` adds the mDNS and
> ping6 evidence that distinguishes them. `--deep` resolves the node via a
> native targeted mDNS lookup first and pings the **live-resolved** address
> (the ledger stores no address — Issue #18); if the node is not advertising
> at all there is nothing to ping, the `ip` check lands in `unavailable` as
> `mdns_unresolved`, and the split then rests on the Thread-side evidence.
```

- [ ] **Step 4: ARCHITECTURE.md line 118 を更新**

`mat commission --target <host-or-ip> --setup-code <code>` → `mat commission --setup-code <code>`

- [ ] **Step 5: バージョンバンプ**

`Cargo.toml` の `version = "1.12.0"` → `"1.13.0"`。

Run: `cargo build --workspace`（Cargo.lock 追従）

- [ ] **Step 6: 最終チェック**

Run: `task check`
Expected: fmt:check / clippy (-D warnings) / 全テスト PASS

- [ ] **Step 7: Commit**

```bash
git add README.md ARCHITECTURE.md Cargo.toml Cargo.lock
git commit -m "chore: 1.13.0（nodes.json address 削除 + atomic write — Issue #18 + 監査 Tier 3）+ README/ARCHITECTURE"
```

---

## マージ前ゲート（計画外の手動作業 — 実装 subagent は行わない）

1. **実機 E2E（jarvis、マージ前必須）**: `task dist:arm64` → `*.new` として scp（本番未置換のまま検証）。
   - `mat discover`（既定）: address が出ないこと。
   - `mat discover --probe`: 到達ノードに `reachable:true` + ライブ address、不達/台帳のみノードに `reachable:false`（address / stale 無し）。
   - `mat diag node --deep`: 到達ノードで `ip.ok:true`（ライブアドレスへの ping6）と `mdns.advertised_self_fabric:true`。可能なら不達ノードで `ip` が `mdns_unresolved` に落ちること。
   - 直経路の非対話 ssh は `MAT_FABRIC_INDEX=2` 必須（運用メモ）。
2. **マージ後デプロイ時**: `jq 'del(.nodes[].address)' nodes.json` で既存ファイルを一掃（backup → install → restart の手順に含める）。matd は挙動不変だがバイナリペアは常に同時更新。
3. Issue #18 はデプロイ・スモーク合格後にクローズ。
