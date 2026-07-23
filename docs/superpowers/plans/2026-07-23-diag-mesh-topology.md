# `mat diag mesh` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Thread メッシュ全体のトポロジー（ノード + リンク品質エッジ）を 1 JSON で返す `mat diag mesh` を実装する。

**Architecture:** `diag node` と同型の専用コマンド層。`commands::diag::mesh` が対象列挙・alias 逆引き・emit を担い、`native_direct::diag_mesh_probe` が engine を 1 度構築して各ノードへ CASE → cluster 53（既存 `ops::diag_thread`）+ cluster 0x33（新 `ops::thread_identity`）を読む。グラフ組み立ては `mat-core::mesh` の純関数。matd 経路は既存の `Command::Diag → Unsupported` で自動的に直経路のみ。

**Tech Stack:** Rust workspace（mat / mat-native / mat-core）、serde_json、tokio current_thread、FakeConn（mat-native::test_support）、assert_cmd 統合テスト。

**Spec:** `docs/superpowers/specs/2026-07-23-diag-mesh-topology-design.md`

## Global Constraints

- stdout は純 JSON のみ。`timestamp`（ISO 8601）必須 — `mat_core::output::emit` が付与する。
- プロトコルコード（TLV/CASE）はバックエンド crate のみ。コマンド層は JSON の組み立てだけ。
- コミット前に必ず `task check`（fmt:check + clippy -D warnings + test）。
- リポジトリは public。テスト・ドキュメントの ExtAddress / IP / node_id はダミー値のみ（RFC 5737 / 適当な hex）。
- バージョンは 1.1.0（workspace `Cargo.toml`、Task 7 で bump）。
- コミットはこのセッションで編集したファイルのみ `git add`（untracked の `thread-map.html` は絶対に含めない）。
- コミットメッセージは既存流儀（`feat(mat): …` / `docs: …`、日本語本文）。

## 事前知識（全タスク共通の背景）

- `ops::diag_thread`（`crates/mat-native/src/ops.rs:131`）は cluster 0x35 の wildcard read 1 発で `fields`（`neighbor_table` / `route_table` / `routing_role` / `partition_id` / `network_name` / `channel` 等）を返す。テーブル行は field id → chip-tool 表記名に改名済み（`ExtAddress` は **u64 数値**、`Rloc16` も数値）。
- TLV octet-string は JSON では **小文字 hex 文字列**になる（`mat-controller/src/im.rs::hex_lower`）。cluster 0x33 `NetworkInterfaces`（attr 0x0000）の struct は context tag 数値キー: `"0"`=Name, `"1"`=IsOperational, `"4"`=HardwareAddress(octstr→hex), `"6"`=IPv6Addresses(list of octstr→hex 32桁), `"7"`=Type（**4 = Thread**）。
- RLOC IPv6 = `<mesh-local-prefix 8B> 00 00 00 ff fe 00 <rloc16 2B>`。RLOC16 の上位 6bit が RouterId（router アドレスは下位 10bit が 0）。
- cluster 53 RoutingRoleEnum: 2=SED, 3=EndDevice, 4=REED, 5=Router, 6=Leader。
- `matd_client::to_op` は `Command::Diag { .. }` を一律 `Unsupported` にする（`crates/mat/src/matd_client.rs:385`）→ matd 側の変更は不要。

---

### Task 1: aliases.toml の `[thread]` セクションと node alias 逆引き

**Files:**
- Modify: `crates/mat-core/src/alias.rs`

**Interfaces:**
- Produces: `AliasBook::node_alias_of(&self, node_id: u64) -> Option<&str>`
- Produces: `AliasBook::thread_labels(&self) -> std::collections::BTreeMap<String, String>`（キーは大文字 16 桁 hex に正規化済み）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/alias.rs` の `#[cfg(test)] mod tests` に追加（既存テストの tempdir + `aliases.toml` 書き込みパターンを踏襲。既存 helper があれば流用）:

```rust
#[test]
fn thread_section_parses_and_normalizes_keys() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("aliases.toml"),
        "[nodes]\nhall_motion = 42\n\n[thread]\n\"aabbccddeeff0011\" = \"otbr-br\"\n",
    )
    .unwrap();
    let book = AliasBook::load(dir.path()).unwrap();
    let labels = book.thread_labels();
    assert_eq!(labels.get("AABBCCDDEEFF0011").map(String::as_str), Some("otbr-br"));
}

#[test]
fn thread_section_rejects_non_hex_key() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("aliases.toml"),
        "[thread]\n\"not-hex\" = \"x\"\n",
    )
    .unwrap();
    let err = AliasBook::load(dir.path()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::StoreParse);
}

#[test]
fn node_alias_reverse_lookup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("aliases.toml"),
        "[nodes]\nhall_motion = 42\nporch_light = 7\n",
    )
    .unwrap();
    let book = AliasBook::load(dir.path()).unwrap();
    assert_eq!(book.node_alias_of(42), Some("hall_motion"));
    assert_eq!(book.node_alias_of(99), None);
}

#[test]
fn node_alias_reverse_lookup_absent_file_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let book = AliasBook::load(dir.path()).unwrap();
    assert_eq!(book.node_alias_of(1), None);
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-core thread_section node_alias`（2 コマンドに分けても可: `cargo test -p mat-core alias::`）
Expected: FAIL（`thread_labels` / `node_alias_of` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

`AliasFile` にフィールド追加（`colors` の下）:

```rust
    /// `[thread]`: Thread ExtAddress（16 桁 hex）→ 表示ラベル。`mat diag mesh` が
    /// 未知メッシュ参加者（BR / 他 fabric デバイス）のラベル付けに使う。
    #[serde(default)]
    thread: BTreeMap<String, String>,
```

`Default for AliasFile` に `thread: BTreeMap::new(),` を追加。

`validate()` の末尾（colors 検証の後）に追加:

```rust
        for (key, label) in &file.thread {
            let hex_ok = key.len() == 16 && key.bytes().all(|b| b.is_ascii_hexdigit());
            if !hex_ok || label.is_empty() {
                return Err(MatError::store_parse(format!(
                    "invalid thread entry '{key}' in {} (key must be 16 hex chars = Thread ExtAddress, label must be non-empty)",
                    path.display()
                )));
            }
        }
```

`impl AliasBook` にメソッド追加:

```rust
    /// node_id → alias の逆引き（複数定義時は BTreeMap 順の先勝ち）。
    /// `mat diag mesh` の出力ラベル用。
    pub fn node_alias_of(&self, node_id: u64) -> Option<&str> {
        self.file
            .nodes
            .iter()
            .find(|(_, &v)| v == node_id)
            .map(|(k, _)| k.as_str())
    }

    /// `[thread]` の ExtAddress → ラベル表（キーを大文字 hex へ正規化して返す）。
    pub fn thread_labels(&self) -> BTreeMap<String, String> {
        self.file
            .thread
            .iter()
            .map(|(k, v)| (k.to_ascii_uppercase(), v.clone()))
            .collect()
    }
```

- [ ] **Step 4: テスト成功を確認**

Run: `cargo test -p mat-core`
Expected: PASS（既存テスト含め全緑）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-core/src/alias.rs
git commit -m "feat(mat-core): aliases.toml に [thread] セクションと node alias 逆引きを追加"
```

---

### Task 2: `mat-core::mesh` — 型と純ヘルパ（自己同定・role・ID 正準化）

**Files:**
- Create: `crates/mat-core/src/mesh.rs`
- Modify: `crates/mat-core/src/lib.rs`（`pub mod mesh;` 追加）

**Interfaces:**
- Produces（Task 3, 4, 6 が使用）:
  - `pub struct NodeInput { pub node_id: u64, pub alias: Option<String>, pub probe: Result<ProbeData, ProbeFailure> }`
  - `pub struct ProbeData { pub thread: serde_json::Map<String, serde_json::Value>, pub identity: Option<Identity> }`
  - `pub struct Identity { pub ext_address: String, pub ipv6: Vec<String> }`
  - `pub struct ProbeFailure { pub kind: ErrorKind, pub detail: String }`
  - `pub fn ext_hex_from_u64(v: u64) -> String`（大文字 16 桁）
  - `pub fn canon_ext_hex(s: &str) -> Option<String>`
  - `pub fn derive_rloc16(mesh_local_prefix_hex: &str, ipv6_hex: &[String]) -> Option<u16>`
  - `pub fn role_from_routing_role(v: i64) -> &'static str`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/mesh.rs` を新規作成し、まずテストから（モジュール冒頭は Step 3 で埋める。コンパイルを通すため型スタブと同時でも良いが、TDD の意図はロジックのテスト先行）:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_hex_from_u64_is_upper_16() {
        assert_eq!(ext_hex_from_u64(0x0011223344556677), "0011223344556677");
        assert_eq!(ext_hex_from_u64(0xAABBCCDDEEFF0011), "AABBCCDDEEFF0011");
    }

    #[test]
    fn canon_ext_hex_normalizes_or_rejects() {
        assert_eq!(canon_ext_hex("aabbccddeeff0011").as_deref(), Some("AABBCCDDEEFF0011"));
        assert_eq!(canon_ext_hex("zzbbccddeeff0011"), None);
        assert_eq!(canon_ext_hex("aabb"), None);
    }

    #[test]
    fn derive_rloc16_finds_rloc_address() {
        // prefix fd00112233445566 + 000000fffe00 + 1400
        let addrs = vec![
            "fe800000000000000011223344556677".to_string(), // link-local
            "fd001122334455660000000000abcdef".to_string(), // ML-EID（fffe00 なし）
            "fd00112233445566000000fffe001400".to_string(), // RLOC
        ];
        assert_eq!(derive_rloc16("fd00112233445566", &addrs), Some(0x1400));
    }

    #[test]
    fn derive_rloc16_none_without_match() {
        let addrs = vec!["fe800000000000000011223344556677".to_string()];
        assert_eq!(derive_rloc16("fd00112233445566", &addrs), None);
        // prefix が 16 桁 hex 未満なら常に None
        assert_eq!(derive_rloc16("fd00", &addrs), None);
    }

    #[test]
    fn derive_rloc16_tolerates_long_prefix_encoding() {
        // 一部デバイスが prefix を長い octstr で返しても先頭 8B を prefix とみなす
        let addrs = vec!["fd00112233445566000000fffe002c00".to_string()];
        assert_eq!(derive_rloc16("fd0011223344556600", &addrs), Some(0x2c00));
    }

    #[test]
    fn role_mapping_matches_cluster53_enum() {
        assert_eq!(role_from_routing_role(6), "leader");
        assert_eq!(role_from_routing_role(5), "router");
        assert_eq!(role_from_routing_role(4), "reed");
        assert_eq!(role_from_routing_role(3), "child");
        assert_eq!(role_from_routing_role(2), "sed");
        assert_eq!(role_from_routing_role(0), "unknown");
    }

    #[test]
    fn router_id_only_for_router_addresses() {
        assert_eq!(router_id_of(0x1400), Some(5)); // 0x1400 >> 10 = 5
        assert_eq!(router_id_of(0x1401), None);    // child index 付きは router ではない
    }
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-core mesh::`
Expected: FAIL（コンパイルエラー — 関数未定義）

- [ ] **Step 3: 実装**

`crates/mat-core/src/mesh.rs` の本体:

```rust
//! `mat diag mesh` の純ロジック: per-node 収集結果（cluster 53 スナップショット +
//! cluster 0x33 自己同定）から Thread メッシュのトポロジーグラフを組み立てる。
//! 副作用なし。収集（CASE/IM）は `mat` 側 `native_direct::diag_mesh_probe` の担当。

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::ErrorKind;

/// per-node 収集の入力 1 件。
#[derive(Debug)]
pub struct NodeInput {
    pub node_id: u64,
    /// aliases.toml の node alias 逆引き結果。
    pub alias: Option<String>,
    pub probe: Result<ProbeData, ProbeFailure>,
}

/// probe 成功時のデータ。
#[derive(Debug)]
pub struct ProbeData {
    /// `ops::diag_thread` の fields（`neighbor_table` / `route_table` /
    /// `routing_role` / `partition_id` / `leader_router_id` /
    /// `mesh_local_prefix` / `network_name` / `channel`）。
    pub thread: Map<String, Value>,
    /// cluster 0x33 由来の自己同定（読めなければ None — エッジは他ノード視点のみ）。
    pub identity: Option<Identity>,
}

/// cluster 0x33 NetworkInterfaces の Thread インターフェース情報。
#[derive(Debug, Clone)]
pub struct Identity {
    /// HardwareAddress（hex 文字列、大文字小文字不問 — 正準化はこちらで行う）。
    pub ext_address: String,
    /// IPv6Addresses（各 32 桁 hex）。
    pub ipv6: Vec<String>,
}

/// probe 失敗時の記録（JSON の `probe_error` へ）。
#[derive(Debug)]
pub struct ProbeFailure {
    pub kind: ErrorKind,
    pub detail: String,
}

/// テーブル行の ExtAddress（u64）→ 正準 16 桁大文字 hex。
pub fn ext_hex_from_u64(v: u64) -> String {
    format!("{v:016X}")
}

/// hex 文字列を正準形（大文字 16 桁）へ。16 桁 hex でなければ None。
pub fn canon_ext_hex(s: &str) -> Option<String> {
    (s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| s.to_ascii_uppercase())
}

/// mesh-local-prefix（hex、先頭 8B を prefix とみなす）と IPv6 一覧から自 RLOC16 を
/// 導出。RLOC = `<prefix 8B> 00 00 00 ff fe 00 <rloc16 2B>`。
pub fn derive_rloc16(mesh_local_prefix_hex: &str, ipv6_hex: &[String]) -> Option<u16> {
    let p = mesh_local_prefix_hex.to_ascii_lowercase();
    if p.len() < 16 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let prefix = &p[..16];
    for a in ipv6_hex {
        let a = a.to_ascii_lowercase();
        if a.len() == 32 && a.starts_with(prefix) && &a[16..28] == "000000fffe00" {
            return u16::from_str_radix(&a[28..32], 16).ok();
        }
    }
    None
}

/// cluster 53 RoutingRoleEnum → 出力 role 文字列。
pub fn role_from_routing_role(v: i64) -> &'static str {
    match v {
        6 => "leader",
        5 => "router",
        4 => "reed",
        3 => "child",
        2 => "sed",
        _ => "unknown",
    }
}

/// RLOC16 から RouterId。router アドレス（下位 10bit = 0）のみ Some。
fn router_id_of(rloc16: u16) -> Option<u8> {
    ((rloc16 & 0x03FF) == 0).then(|| (rloc16 >> 10) as u8)
}

/// "0x1400" 形式。
fn rloc16_str(r: u16) -> String {
    format!("{r:#06x}")
}
```

（`router_id_of` / `rloc16_str` は Task 3 の `build_graph` が使う。unused warning が出る場合は Task 3 まで `#[allow(dead_code)]` を付けず、`pub(crate)` にもせず、このタスクでは `#[cfg(test)]` から参照されているので warning は出ない — clippy が dead_code を出したら `#[allow(dead_code)] // Task 3 (build_graph) で使用` を一時付与し Task 3 で外す。）

`crates/mat-core/src/lib.rs` に `pub mod mesh;` を追加（既存の `pub mod` 列のアルファベット順の位置）。

- [ ] **Step 4: テスト成功を確認**

Run: `cargo test -p mat-core mesh::`
Expected: PASS（7 テスト）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-core/src/mesh.rs crates/mat-core/src/lib.rs
git commit -m "feat(mat-core): mesh モジュール — 自己同定・role・ID 正準化の純ヘルパ"
```

---

### Task 3: `mat-core::mesh::build_graph` — グラフ組み立て

**Files:**
- Modify: `crates/mat-core/src/mesh.rs`

**Interfaces:**
- Consumes: Task 2 の全型・ヘルパ。
- Produces（Task 6 が使用）:
  - `pub fn build_graph(inputs: &[NodeInput], thread_labels: &BTreeMap<String, String>) -> MeshGraph`
  - `pub struct MeshGraph`（`Serialize`; フィールド `network: NetworkSummary`, `nodes: Vec<MeshNode>`, `edges: Vec<MeshEdge>`）

- [ ] **Step 1: 失敗するテストを書く**

`mesh.rs` の tests モジュールに追加。fixture helper と 4 テスト:

```rust
    use serde_json::json;

    /// fabric ノードの probe 成功入力を組む fixture。
    fn fabric_input(
        node_id: u64,
        alias: Option<&str>,
        ext: &str,
        rloc_addr: &str,
        thread_extra: Vec<(&str, Value)>,
    ) -> NodeInput {
        let mut thread = Map::new();
        thread.insert("network_name".into(), json!("TestNet"));
        thread.insert("channel".into(), json!(25));
        thread.insert("partition_id".into(), json!(123456));
        thread.insert("leader_router_id".into(), json!(8));
        thread.insert("mesh_local_prefix".into(), json!("fd00112233445566"));
        thread.insert("routing_role".into(), json!(5));
        thread.insert("neighbor_table".into(), json!([]));
        thread.insert("route_table".into(), json!([]));
        for (k, v) in thread_extra {
            thread.insert(k.into(), v);
        }
        NodeInput {
            node_id,
            alias: alias.map(str::to_string),
            probe: Ok(ProbeData {
                thread,
                identity: Some(Identity {
                    ext_address: ext.to_string(),
                    ipv6: vec![rloc_addr.to_string()],
                }),
            }),
        }
    }

    #[test]
    fn build_graph_two_fabric_nodes_and_unknown_br() {
        // node42 (ext 0011..., rloc 0x1400) が node7 (ext 8899..., rloc 0x0c01=child)
        // と BR (ext AABB..., rloc 0x2000, route-table 経由) を見る。
        let n16 = fabric_input(
            42,
            Some("hall_motion"),
            "0011223344556677",
            "fd00112233445566000000fffe001400",
            vec![
                (
                    "neighbor_table",
                    json!([
                        {"ExtAddress": 0x8899AABBCCDDEEFFu64, "Rloc16": 0x0c01, "Lqi": 140,
                         "AverageRssi": -60, "LastRssi": -58, "FrameErrorRate": 2, "Age": 12,
                         "RxOnWhenIdle": false, "IsChild": true},
                        {"ExtAddress": 0xAABBCCDDEEFF0011u64, "Rloc16": 0x2000, "Lqi": 200,
                         "AverageRssi": -50, "LastRssi": -49, "FrameErrorRate": 0, "Age": 3,
                         "RxOnWhenIdle": true, "IsChild": false}
                    ]),
                ),
                (
                    "route_table",
                    json!([
                        {"ExtAddress": 0xAABBCCDDEEFF0011u64, "Rloc16": 0x2000, "RouterId": 8,
                         "PathCost": 1, "LQIIn": 3, "LQIOut": 3, "Allocated": true,
                         "LinkEstablished": true}
                    ]),
                ),
            ],
        );
        let n5 = fabric_input(
            7,
            None,
            "8899AABBCCDDEEFF",
            "fd00112233445566000000fffe000c01",
            vec![
                ("routing_role", json!(3)),
                (
                    "neighbor_table",
                    json!([
                        {"ExtAddress": 0x0011223344556677u64, "Rloc16": 0x1400, "Lqi": 130,
                         "AverageRssi": -65, "LastRssi": -64, "FrameErrorRate": 5, "Age": 8,
                         "RxOnWhenIdle": true, "IsChild": false}
                    ]),
                ),
            ],
        );
        let labels = BTreeMap::from([("AABBCCDDEEFF0011".to_string(), "otbr-br".to_string())]);
        let g = build_graph(&[n16, n5], &labels);

        // network サマリ
        assert_eq!(g.network.name.as_deref(), Some("TestNet"));
        assert_eq!(g.network.channel, Some(25));
        assert_eq!(g.network.partition_ids, vec![123456]);
        assert_eq!(g.network.leader_router_id, Some(8));

        // ノード: fabric 2 + unknown BR 1
        assert_eq!(g.nodes.len(), 3);
        let n16o = g.nodes.iter().find(|n| n.node_id == Some(42)).unwrap();
        assert_eq!(n16o.id, "ext:0011223344556677");
        assert_eq!(n16o.rloc16.as_deref(), Some("0x1400"));
        assert_eq!(n16o.router_id, Some(5));
        assert_eq!(n16o.role, "router");
        assert_eq!(n16o.alias.as_deref(), Some("hall_motion"));
        assert_eq!(n16o.probed, Some(true));
        let br = g.nodes.iter().find(|n| n.node_id.is_none()).unwrap();
        assert_eq!(br.id, "ext:AABBCCDDEEFF0011");
        assert_eq!(br.label.as_deref(), Some("otbr-br"));
        // RouterId 8 = leader_router_id → leader マーク
        assert_eq!(br.role, "leader");
        assert_eq!(br.probed, None);

        // エッジ: n16–n5（双方向）と n16–BR（片方向 + route）
        assert_eq!(g.edges.len(), 2);
        let e_n16_n5 = g
            .edges
            .iter()
            .find(|e| e.a == "ext:0011223344556677" && e.b == "ext:8899AABBCCDDEEFF")
            .unwrap();
        // a=n16 の neighbor 行（b=n5 を測った値）が a_sees_b
        assert_eq!(e_n16_n5.a_sees_b.as_ref().unwrap().lqi, Some(140));
        assert_eq!(e_n16_n5.b_sees_a.as_ref().unwrap().avg_rssi, Some(-65));
        let e_n16_br = g
            .edges
            .iter()
            .find(|e| e.b == "ext:AABBCCDDEEFF0011")
            .unwrap();
        assert_eq!(e_n16_br.a_sees_b.as_ref().unwrap().lqi, Some(200));
        assert!(e_n16_br.b_sees_a.is_none());
        assert_eq!(e_n16_br.route.as_ref().unwrap().path_cost, Some(1));
    }

    #[test]
    fn build_graph_probe_failure_yields_node_fallback_id() {
        let bad = NodeInput {
            node_id: 7,
            alias: None,
            probe: Err(ProbeFailure {
                kind: ErrorKind::Unreachable,
                detail: "Node 7 is unreachable".into(),
            }),
        };
        let g = build_graph(&[bad], &BTreeMap::new());
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "node:7");
        assert_eq!(g.nodes[0].probed, Some(false));
        assert_eq!(g.nodes[0].role, "unknown");
        let pe = g.nodes[0].probe_error.as_ref().unwrap();
        assert_eq!(pe.kind, ErrorKind::Unreachable);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn build_graph_route_ignores_unestablished_links() {
        let n1 = fabric_input(
            1,
            None,
            "0011223344556677",
            "fd00112233445566000000fffe001400",
            vec![(
                "route_table",
                json!([
                    {"ExtAddress": 0xAABBCCDDEEFF0011u64, "Rloc16": 0x2000, "RouterId": 8,
                     "PathCost": 2, "LQIIn": 0, "LQIOut": 0, "Allocated": true,
                     "LinkEstablished": false}
                ]),
            )],
        );
        let g = build_graph(&[n1], &BTreeMap::new());
        // 直リンク未確立 → エッジなし。ただし参加者としてはノード化される。
        assert!(g.edges.is_empty());
        assert_eq!(g.nodes.len(), 2);
        let br = g.nodes.iter().find(|n| n.node_id.is_none()).unwrap();
        assert_eq!(br.role, "leader"); // RouterId 8 = leader_router_id
    }

    #[test]
    fn build_graph_sed_detection_from_neighbor_row() {
        let n1 = fabric_input(
            1,
            None,
            "0011223344556677",
            "fd00112233445566000000fffe001400",
            vec![(
                "neighbor_table",
                json!([
                    {"ExtAddress": 0x1122334455667788u64, "Rloc16": 0x1401, "Lqi": 100,
                     "AverageRssi": -70, "LastRssi": -70, "FrameErrorRate": 10, "Age": 1,
                     "RxOnWhenIdle": false, "IsChild": true}
                ]),
            )],
        );
        let g = build_graph(&[n1], &BTreeMap::new());
        let sed = g.nodes.iter().find(|n| n.node_id.is_none()).unwrap();
        assert_eq!(sed.role, "sed");
        assert_eq!(sed.rloc16.as_deref(), Some("0x1401"));
        assert_eq!(sed.router_id, None);
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-core mesh::`
Expected: FAIL（`build_graph` / 出力型 未定義）

- [ ] **Step 3: 実装**

`mesh.rs` に出力型と `build_graph` を追加:

```rust
/// `mat diag mesh` の出力全体（`timestamp` は emit 側が付与）。
#[derive(Debug, Serialize)]
pub struct MeshGraph {
    pub network: NetworkSummary,
    pub nodes: Vec<MeshNode>,
    pub edges: Vec<MeshEdge>,
}

#[derive(Debug, Serialize)]
pub struct NetworkSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<u64>,
    /// 全 probe 済みノードで観測した partition-id（複数 = メッシュ分断の兆候）。
    pub partition_ids: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leader_router_id: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MeshNode {
    /// 安定キー: `ext:<HEX16>` / `rloc:0x….` / `node:<node_id>`（同定不能 fabric ノード）。
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rloc16: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router_id: Option<u8>,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// fabric ノードのみ。未知参加者は省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<ProbeErrorOut>,
}

#[derive(Debug, Serialize)]
pub struct ProbeErrorOut {
    pub kind: ErrorKind,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct MeshEdge {
    pub a: String,
    pub b: String,
    /// a の neighbor-table の b 行（= a が受信した b の電波品質）。null = 観測なし。
    pub a_sees_b: Option<LinkMetrics>,
    pub b_sees_a: Option<LinkMetrics>,
    /// route-table 由来（LinkEstablished = true の行のみ。a 視点優先）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteMetrics>,
}

#[derive(Debug, Serialize)]
pub struct LinkMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lqi: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_rssi: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_rssi: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_error_rate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct RouteMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lqi_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lqi_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_cost: Option<u64>,
}

/// テーブル（`neighbor_table` / `route_table`）の行を object だけに絞って返す。
fn table_rows<'a>(fields: &'a Map<String, Value>, key: &str) -> Vec<&'a Map<String, Value>> {
    fields
        .get(key)
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

fn link_metrics(row: &Map<String, Value>) -> LinkMetrics {
    LinkMetrics {
        lqi: row.get("Lqi").and_then(Value::as_u64),
        avg_rssi: row.get("AverageRssi").and_then(Value::as_i64),
        last_rssi: row.get("LastRssi").and_then(Value::as_i64),
        frame_error_rate: row.get("FrameErrorRate").and_then(Value::as_u64),
        age: row.get("Age").and_then(Value::as_u64),
    }
}

fn ordered(x: String, y: String) -> (String, String) {
    if x <= y { (x, y) } else { (y, x) }
}

/// テーブル観測から集めた未知参加者の証拠。
#[derive(Default)]
struct Part {
    rloc16: Option<u16>,
    seen_as_router: bool,
    seen_as_child: bool,
    rx_on_when_idle: Option<bool>,
    route_router_id: Option<u64>,
}

/// per-node 収集結果からグラフを組み立てる（純関数）。
///
/// - 参加者は ExtAddress（正準 16 桁大文字 hex）キーで台帳化。fabric ノードの
///   自己同定（identity）とテーブル行の両方から集める。
/// - エッジは無向 1 本に双方向実測を併記。自己同定できないノードの行は
///   張れない（他ノード視点のみ成立）。
pub fn build_graph(
    inputs: &[NodeInput],
    thread_labels: &BTreeMap<String, String>,
) -> MeshGraph {
    // 1. network サマリ（最初に読めた値を採用）+ mesh-local-prefix。
    let mut name = None;
    let mut channel = None;
    let mut leader_router_id = None;
    let mut ml_prefix: Option<String> = None;
    let mut partition_ids: Vec<u64> = Vec::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        let f = &p.thread;
        if name.is_none() {
            name = f.get("network_name").and_then(Value::as_str).map(str::to_string);
        }
        if channel.is_none() {
            channel = f.get("channel").and_then(Value::as_u64);
        }
        if leader_router_id.is_none() {
            leader_router_id = f.get("leader_router_id").and_then(Value::as_u64);
        }
        if ml_prefix.is_none() {
            ml_prefix = f.get("mesh_local_prefix").and_then(Value::as_str).map(str::to_string);
        }
        if let Some(pid) = f.get("partition_id").and_then(Value::as_u64) {
            if !partition_ids.contains(&pid) {
                partition_ids.push(pid);
            }
        }
    }
    partition_ids.sort_unstable();

    // 2. fabric ノードの自己同定（node_id → 正準 ext hex / rloc16）。
    let mut self_ext: BTreeMap<u64, String> = BTreeMap::new();
    let mut self_rloc: BTreeMap<u64, u16> = BTreeMap::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        let Some(id) = &p.identity else { continue };
        let Some(ext) = canon_ext_hex(&id.ext_address) else { continue };
        self_ext.insert(inp.node_id, ext);
        if let Some(pref) = &ml_prefix {
            if let Some(r) = derive_rloc16(pref, &id.ipv6) {
                self_rloc.insert(inp.node_id, r);
            }
        }
    }

    // 3. 参加者台帳（ext hex → 証拠）。
    let mut parts: BTreeMap<String, Part> = BTreeMap::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        for row in table_rows(&p.thread, "neighbor_table") {
            let Some(ext) = row.get("ExtAddress").and_then(Value::as_u64).map(ext_hex_from_u64)
            else {
                continue;
            };
            let part = parts.entry(ext).or_default();
            if part.rloc16.is_none() {
                part.rloc16 = row
                    .get("Rloc16")
                    .and_then(Value::as_u64)
                    .and_then(|v| u16::try_from(v).ok());
            }
            if row.get("IsChild").and_then(Value::as_bool) == Some(true) {
                part.seen_as_child = true;
            }
            if let Some(rx) = row.get("RxOnWhenIdle").and_then(Value::as_bool) {
                part.rx_on_when_idle = Some(rx);
            }
        }
        for row in table_rows(&p.thread, "route_table") {
            let Some(ext) = row.get("ExtAddress").and_then(Value::as_u64).map(ext_hex_from_u64)
            else {
                continue;
            };
            let part = parts.entry(ext).or_default();
            if part.rloc16.is_none() {
                part.rloc16 = row
                    .get("Rloc16")
                    .and_then(Value::as_u64)
                    .and_then(|v| u16::try_from(v).ok());
            }
            part.seen_as_router = true;
            if part.route_router_id.is_none() {
                part.route_router_id = row.get("RouterId").and_then(Value::as_u64);
            }
        }
    }

    // 4. エッジ集約（無向、キーは辞書順ペア）。
    #[derive(Default)]
    struct EdgeAcc {
        a_sees_b: Option<LinkMetrics>,
        b_sees_a: Option<LinkMetrics>,
        route_a: Option<RouteMetrics>,
        route_b: Option<RouteMetrics>,
    }
    let mut edges: BTreeMap<(String, String), EdgeAcc> = BTreeMap::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        let Some(my_ext) = self_ext.get(&inp.node_id) else { continue };
        for row in table_rows(&p.thread, "neighbor_table") {
            let Some(other) = row.get("ExtAddress").and_then(Value::as_u64).map(ext_hex_from_u64)
            else {
                continue;
            };
            if other == *my_ext {
                continue;
            }
            let key = ordered(my_ext.clone(), other);
            let mine_is_a = key.0 == *my_ext;
            let acc = edges.entry(key).or_default();
            let m = link_metrics(row);
            if mine_is_a {
                acc.a_sees_b = Some(m);
            } else {
                acc.b_sees_a = Some(m);
            }
        }
        for row in table_rows(&p.thread, "route_table") {
            if row.get("LinkEstablished").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let Some(other) = row.get("ExtAddress").and_then(Value::as_u64).map(ext_hex_from_u64)
            else {
                continue;
            };
            if other == *my_ext {
                continue;
            }
            let key = ordered(my_ext.clone(), other);
            let mine_is_a = key.0 == *my_ext;
            let acc = edges.entry(key).or_default();
            let r = RouteMetrics {
                lqi_in: row.get("LQIIn").and_then(Value::as_u64),
                lqi_out: row.get("LQIOut").and_then(Value::as_u64),
                path_cost: row.get("PathCost").and_then(Value::as_u64),
            };
            if mine_is_a {
                acc.route_a = Some(r);
            } else {
                acc.route_b = Some(r);
            }
        }
    }

    // 5. ノード出力: fabric ノード（入力順）→ 未知参加者（ext 昇順）。
    let mut nodes: Vec<MeshNode> = Vec::new();
    let mut consumed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for inp in inputs {
        let ext = self_ext.get(&inp.node_id).cloned();
        if let Some(e) = &ext {
            consumed.insert(e.clone());
        }
        let rloc16 = self_rloc.get(&inp.node_id).copied().or_else(|| {
            ext.as_ref().and_then(|e| parts.get(e)).and_then(|p| p.rloc16)
        });
        let role = match &inp.probe {
            Ok(p) => p
                .thread
                .get("routing_role")
                .and_then(Value::as_i64)
                .map(role_from_routing_role)
                .unwrap_or("unknown"),
            Err(_) => "unknown",
        };
        let id = match (&ext, rloc16) {
            (Some(e), _) => format!("ext:{e}"),
            (None, Some(r)) => format!("rloc:{}", rloc16_str(r)),
            (None, None) => format!("node:{}", inp.node_id),
        };
        nodes.push(MeshNode {
            id,
            ext_address: ext.clone(),
            rloc16: rloc16.map(rloc16_str),
            router_id: rloc16.and_then(router_id_of),
            role: role.to_string(),
            node_id: Some(inp.node_id),
            alias: inp.alias.clone(),
            label: ext.as_ref().and_then(|e| thread_labels.get(e)).cloned(),
            probed: Some(inp.probe.is_ok()),
            probe_error: inp.probe.as_ref().err().map(|f| ProbeErrorOut {
                kind: f.kind,
                detail: f.detail.clone(),
            }),
        });
    }
    for (ext, part) in &parts {
        if consumed.contains(ext) {
            continue;
        }
        let router_id: Option<u8> = part
            .route_router_id
            .and_then(|v| u8::try_from(v).ok())
            .or_else(|| part.rloc16.and_then(router_id_of));
        let is_leader = leader_router_id.is_some()
            && router_id.map(u64::from) == leader_router_id;
        let role = if part.seen_as_router || router_id.is_some() {
            if is_leader { "leader" } else { "router" }
        } else if part.seen_as_child {
            if part.rx_on_when_idle == Some(false) { "sed" } else { "child" }
        } else {
            "unknown"
        };
        nodes.push(MeshNode {
            id: format!("ext:{ext}"),
            ext_address: Some(ext.clone()),
            rloc16: part.rloc16.map(rloc16_str),
            router_id: if role == "sed" || role == "child" { None } else { router_id },
            role: role.to_string(),
            node_id: None,
            alias: None,
            label: thread_labels.get(ext).cloned(),
            probed: None,
            probe_error: None,
        });
    }

    // 6. エッジ出力（キー昇順、route は a 視点優先）。
    let edges = edges
        .into_iter()
        .map(|((a, b), acc)| MeshEdge {
            a: format!("ext:{a}"),
            b: format!("ext:{b}"),
            a_sees_b: acc.a_sees_b,
            b_sees_a: acc.b_sees_a,
            route: acc.route_a.or(acc.route_b),
        })
        .collect();

    MeshGraph {
        network: NetworkSummary {
            name,
            channel,
            partition_ids,
            leader_router_id,
        },
        nodes,
        edges,
    }
}
```

Task 2 で `router_id_of` / `rloc16_str` に `#[allow(dead_code)]` を付けていた場合はここで外す。

**注意（fabric ノードの role と leader）:** fabric ノードは routing_role 直読なので leader は 6 で出る。未知参加者のみ RouterId × leader_router_id で leader を推定する。sed/child の未知参加者には `router_id` を出さない（child の RLOC16 上位 bit は親 router のもの。`router_id_of` は child アドレスで None を返し、さらに role 判定後のガードで route_router_id 経由の混入も防ぐ）。

- [ ] **Step 4: テスト成功を確認**

Run: `cargo test -p mat-core mesh::`
Expected: PASS（Task 2 の 7 + 今回の 4 = 11 テスト）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-core/src/mesh.rs
git commit -m "feat(mat-core): mesh::build_graph — トポロジーグラフ組み立て（参加者台帳・双方向エッジ・role推定）"
```

---

### Task 4: `mat-native::ops` — cluster 0x33 自己同定 + cluster 53 スカラー追加

**Files:**
- Modify: `crates/mat-native/src/ops.rs`

**Interfaces:**
- Consumes: `mat_core::mesh::Identity`（Task 2）、`NodeConn::read_json`（既存 trait）。
- Produces（Task 6 が使用）: `pub async fn thread_identity(conn: &mut dyn NodeConn, endpoint: u16) -> Result<Option<mat_core::mesh::Identity>, MatError>`
- 変更: `SCALARS` に `leader_router_id` / `mesh_local_prefix` 追加 → `diag thread` の出力にもキーが増える（additive、スキーマ互換）。

- [ ] **Step 1: 失敗するテストを書く**

`ops.rs` の tests モジュールに追加（既存の `FakeConn` パターン踏襲）:

```rust
    #[tokio::test]
    async fn thread_identity_picks_thread_interface() {
        // NetworkInterfaces: eth (type=2) と Thread (type=4)。Thread を選ぶ。
        let mut conn = FakeConn::scripted().with_read(
            0,
            0x0033,
            0x0000,
            serde_json::json!([
                {"0": "eth0", "1": true, "4": "aabbccddeeff", "6": [], "7": 2},
                {"0": "wpan0", "1": true, "4": "0011223344556677",
                 "6": ["fd00112233445566000000fffe001400",
                        "fe800000000000000011223344556677"],
                 "7": 4}
            ]),
        );
        let id = thread_identity(&mut conn, 0).await.unwrap().unwrap();
        assert_eq!(id.ext_address, "0011223344556677");
        assert_eq!(id.ipv6.len(), 2);
    }

    #[tokio::test]
    async fn thread_identity_none_without_thread_interface() {
        let mut conn = FakeConn::scripted().with_read(
            0,
            0x0033,
            0x0000,
            serde_json::json!([
                {"0": "eth0", "1": true, "4": "aabbccddeeff", "6": [], "7": 2}
            ]),
        );
        assert!(thread_identity(&mut conn, 0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn thread_identity_none_on_non_array() {
        let mut conn =
            FakeConn::scripted().with_read(0, 0x0033, 0x0000, serde_json::json!(null));
        assert!(thread_identity(&mut conn, 0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn diag_thread_includes_leader_and_ml_prefix_scalars() {
        // 既存 diag_thread_maps_names_and_partial_results と同じ組み方で、
        // leader-router-id(0x000d) と mesh-local-prefix(0x0005) が fields に載ること。
        let mut conn = FakeConn::scripted().with_cluster(
            0,
            0x0035,
            vec![
                (0x0001, serde_json::json!(5)),
                (0x000d, serde_json::json!(8)),
                (0x0005, serde_json::json!("fd00112233445566")),
            ],
        );
        let snap = diag_thread(&mut conn, 0).await.unwrap();
        assert_eq!(snap.fields["leader_router_id"], serde_json::json!(8));
        assert_eq!(
            snap.fields["mesh_local_prefix"],
            serde_json::json!("fd00112233445566")
        );
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-native thread_identity leader_and_ml`
Expected: FAIL（`thread_identity` 未定義 / `leader_router_id` キー不在）

- [ ] **Step 3: 実装**

`SCALARS` 定数に 2 行追加:

```rust
const SCALARS: &[(&str, &str)] = &[
    ("routing_role", "routing-role"),
    ("network_name", "network-name"),
    ("extended_pan_id", "extended-pan-id"),
    ("pan_id", "pan-id"),
    ("partition_id", "partition-id"),
    ("channel", "channel"),
    ("leader_router_id", "leader-router-id"),
    ("mesh_local_prefix", "mesh-local-prefix"),
];
```

`diag_thread` の下に追加:

```rust
/// General Diagnostics（cluster 0x33）NetworkInterfaces。
const CLUSTER_GENERAL_DIAG: u32 = 0x0033;
const ATTR_NETWORK_INTERFACES: u32 = 0x0000;
/// NetworkInterfaceStruct の InterfaceTypeEnum: 4 = Thread。
const IFACE_TYPE_THREAD: u64 = 4;

/// cluster 0x33 NetworkInterfaces から Thread インターフェースの自己同定情報
/// （HardwareAddress = 802.15.4 ExtAddress、IPv6 一覧）を取り出す。
/// Thread IF が無い / 形が想定外なら Ok(None)（mesh 収集は自己同定なしで続行可能
/// — read 自体の失敗は Err で伝播し、呼び出し側が None に丸めるか決める）。
/// struct のキーは context tag の 10 進文字列（`tlv_element_to_json` 参照）:
/// "0"=Name, "1"=IsOperational, "4"=HardwareAddress(octstr→hex),
/// "6"=IPv6Addresses(list of octstr→hex), "7"=Type。
pub async fn thread_identity(
    conn: &mut dyn NodeConn,
    endpoint: u16,
) -> Result<Option<mat_core::mesh::Identity>, MatError> {
    let v = conn
        .read_json(endpoint, CLUSTER_GENERAL_DIAG, ATTR_NETWORK_INTERFACES)
        .await?;
    let Some(items) = v.as_array() else {
        return Ok(None);
    };
    for item in items {
        let Some(o) = item.as_object() else { continue };
        if o.get("7").and_then(Value::as_u64) != Some(IFACE_TYPE_THREAD) {
            continue;
        }
        let Some(hw) = o.get("4").and_then(Value::as_str) else {
            continue;
        };
        let ipv6 = o
            .get("6")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        return Ok(Some(mat_core::mesh::Identity {
            ext_address: hw.to_string(),
            ipv6,
        }));
    }
    Ok(None)
}
```

- [ ] **Step 4: テスト成功を確認**

Run: `cargo test -p mat-native`
Expected: PASS（既存テスト含め全緑 — `diag_thread` 既存テストは fields キー増でも壊れない設計だが、全数実行で確認）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-native/src/ops.rs
git commit -m "feat(mat-native): thread_identity（cluster 0x33 自己同定）+ diag thread に leader/ml-prefix スカラー追加"
```

---

### Task 5: CLI — `DiagCommand::Mesh` + resolve + native_direct 除外

**Files:**
- Modify: `crates/mat/src/cli.rs`（`DiagCommand` に `Mesh` 追加）
- Modify: `crates/mat/src/resolve.rs`（`Diag` の match に `Mesh` arm）
- Modify: `crates/mat/src/native_direct.rs`（`run()` の早期 `None` match に `Mesh` 追加 + テスト）

**Interfaces:**
- Produces（Task 6 が使用）: `DiagCommand::Mesh { nodes: Vec<NodeRef> }`（resolve 後は全要素 `NodeRef::Id`）。
- `native_direct::run` は `Mesh` に対して `None` を返す（= 専用コマンド層行き）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/src/native_direct.rs` の tests モジュールに追加（既存 `describe_diag_thread_open_window_shapes_are_native` 付近のスタイル踏襲）:

```rust
    #[test]
    fn diag_mesh_is_excluded_from_native_direct_run() {
        use crate::cli::DiagCommand;
        let cmd = Command::Diag {
            action: DiagCommand::Mesh { nodes: vec![] },
        };
        let cfg = Config {
            iface: "lo",
            fabric_index: 1,
            issuer_index: 0,
        };
        // 専用コマンド層を持つ op は run() が None（store にも触れない —
        // 実在しないパスで良い）。
        assert!(run(&cmd, std::path::Path::new("/nonexistent"), &cfg).is_none());
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat diag_mesh_is_excluded`
Expected: FAIL（`DiagCommand::Mesh` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

`cli.rs` — `DiagCommand` に variant 追加（`Node` の後）:

```rust
    /// Thread メッシュ全体のトポロジー（ノード + リンク品質エッジ）を 1 JSON で
    /// 返す。自 fabric の commission 済みノードを順に診断（cluster 53 + 0x33）し、
    /// テーブルに現れた未知参加者（BR / 他 fabric デバイス）もグラフに含める。
    /// 直経路のみ・endpoint 0 固定。ラベルは aliases.toml（node alias 逆引き +
    /// `[thread]` セクション）。
    Mesh {
        /// 対象ノード（node_id または alias、1 つ以上）。省略時 = store の全
        /// commission 済みノード。
        #[arg(long = "nodes", num_args = 1.., value_name = "N|ALIAS")]
        nodes: Vec<NodeRef>,
    },
```

`resolve.rs` — `Command::Diag` の match に arm 追加（`DiagCommand::Node` の後）:

```rust
                DiagCommand::Mesh { nodes } => DiagCommand::Mesh {
                    nodes: nodes
                        .into_iter()
                        .map(|n| book.resolve_node(&n).map(NodeRef::Id))
                        .collect::<Result<_, MatError>>()?,
                },
```

`native_direct.rs` — `run()` の早期 return match を拡張:

```rust
        Command::Discover { .. }
        | Command::Commission { .. }
        | Command::Fabric { .. }
        | Command::Diag {
            action: DiagCommand::Node { .. },
        }
        | Command::Diag {
            action: DiagCommand::Mesh { .. },
        } => return None,
```

- [ ] **Step 4: テスト成功を確認**

Run: `cargo test -p mat`
Expected: PASS。ただし `main.rs` の dispatch はまだ無いので、`mat diag mesh` 実行は catch-all の internal parse_error になる（Task 6 で解消 — このタスクでは合格条件ではない）。

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat/src/cli.rs crates/mat/src/resolve.rs crates/mat/src/native_direct.rs
git commit -m "feat(mat): CLI に diag mesh を追加（resolve + native_direct 除外）"
```

---

### Task 6: 収集ループ + コマンド層 + main dispatch

**Files:**
- Modify: `crates/mat/src/native_direct.rs`（`diag_mesh_probe` + `MeshProbeItem`）
- Modify: `crates/mat/src/commands/diag.rs`（`mesh()` + `dominant_error()`）
- Modify: `crates/mat/src/main.rs`（dispatch arm）

**Interfaces:**
- Consumes: Task 2/3 の `mat_core::mesh::{NodeInput, ProbeData, ProbeFailure, build_graph}`、Task 4 の `ops::{diag_thread, thread_identity}`、Task 1 の `AliasBook::{node_alias_of, thread_labels}`、Task 5 の `DiagCommand::Mesh`。
- Produces:
  - `native_direct.rs`: `pub(crate) struct MeshProbeItem { pub node_id: u64, pub result: Result<mat_core::mesh::ProbeData, MatError> }`
  - `native_direct.rs`: `pub(crate) fn diag_mesh_probe(cfg: &Config<'_>, store_root: &Path, targets: &[u64]) -> Result<Vec<MeshProbeItem>, MatError>`
  - `commands/diag.rs`: `pub fn mesh(store_path: &Path, node_ids: &[u64], native: Option<&crate::native_direct::Config<'_>>) -> Result<(), MatError>`

- [ ] **Step 1: 失敗するテストを書く（dominant_error の純ロジック）**

`crates/mat/src/commands/diag.rs` に tests モジュールが無ければ末尾に作り、追加:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use mat_core::error::{ErrorKind, MatError};

    fn item(node_id: u64, kind: ErrorKind) -> crate::native_direct::MeshProbeItem {
        crate::native_direct::MeshProbeItem {
            node_id,
            result: Err(MatError::new(kind, format!("node {node_id} failed"))),
        }
    }

    #[test]
    fn dominant_error_picks_most_frequent_kind() {
        let items = vec![
            item(1, ErrorKind::Timeout),
            item(2, ErrorKind::Unreachable),
            item(3, ErrorKind::Unreachable),
        ];
        let e = dominant_error(&items);
        assert_eq!(e.kind, ErrorKind::Unreachable);
        assert!(e.detail.contains("node 1"));
        assert!(e.detail.contains("node 3"));
    }

    #[test]
    fn dominant_error_tie_is_first_seen() {
        let items = vec![
            item(1, ErrorKind::Timeout),
            item(2, ErrorKind::Unreachable),
        ];
        assert_eq!(dominant_error(&items).kind, ErrorKind::Timeout);
    }
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat dominant_error`
Expected: FAIL（`MeshProbeItem` / `dominant_error` 未定義）

- [ ] **Step 3: 実装 — native_direct::diag_mesh_probe**

`native_direct.rs` の `diag_im_probe` 群の近くに追加:

```rust
/// `mat diag mesh` の per-node 収集結果 1 件。
pub(crate) struct MeshProbeItem {
    pub node_id: u64,
    pub result: Result<mat_core::mesh::ProbeData, MatError>,
}

/// `mat diag mesh` の収集: engine を 1 度構築し、各対象ノードへ逐次
/// CASE → cluster 53（diag_thread）+ cluster 0x33（thread_identity）。
/// per-node の失敗は item の Err に畳む（部分結果）。エンジン構築失敗のみ
/// ハードエラー。
pub(crate) fn diag_mesh_probe(
    cfg: &Config<'_>,
    store_root: &Path,
    targets: &[u64],
) -> Result<Vec<MeshProbeItem>, MatError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            MatError::new(
                mat_core::error::ErrorKind::Other,
                format!("tokio runtime: {e}"),
            )
        })?;
    rt.block_on(async {
        let native_cfg = NativeConfig {
            store: store_root.to_path_buf(),
            iface: cfg.iface.to_string(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = Engine::build(&native_cfg)
            .await
            .map_err(map_engine_build_error)?;
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
}

/// 1 ノード分: CASE 確立 → cluster 53 → cluster 0x33。0x33 は補助情報なので
/// 読めなくても成功扱い（identity=None、warn ログのみ）。
async fn mesh_probe_one(
    engine: &Engine,
    node_id: u64,
) -> Result<mat_core::mesh::ProbeData, MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    let snap = mat_native::ops::diag_thread(&mut *conn, 0).await?;
    let identity = match mat_native::ops::thread_identity(&mut *conn, 0).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(node_id, kind = ?e.kind,
                "network-interfaces read failed; continuing without self-identity");
            None
        }
    };
    tracing::info!(node_id, has_identity = identity.is_some(),
        "mesh probe executed (native direct)");
    Ok(mat_core::mesh::ProbeData {
        thread: snap.fields,
        identity,
    })
}
```

- [ ] **Step 4: 実装 — commands::diag::mesh**

`crates/mat/src/commands/diag.rs` に追加（`use` に `mat_core::alias::AliasBook` と `mat_core::mesh` 系を足す）:

```rust
/// `mat diag mesh` — メッシュ全体のトポロジーを 1 JSON で返す。
/// `node_ids` 空 = store の全 commission 済みノード。probe の部分失敗は
/// JSON 内（`probed:false` + `probe_error`）に畳み、全滅時のみ最頻 kind を
/// トップレベルエラーへ写像する。
pub fn mesh(
    store_path: &Path,
    node_ids: &[u64],
    native: Option<&crate::native_direct::Config<'_>>,
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    let targets: Vec<u64> = if node_ids.is_empty() {
        store.nodes().map(|r| r.node_id).collect()
    } else {
        for &id in node_ids {
            store.require_node(id)?;
        }
        node_ids.to_vec()
    };
    let book = mat_core::alias::AliasBook::load(store.root())?;

    // 対象 0 = 空グラフで正常終了（バックエンド未接触）。
    let items = if targets.is_empty() {
        Vec::new()
    } else {
        let cfg = native.ok_or_else(|| {
            MatError::new(
                ErrorKind::Other,
                "diag mesh: native backend not configured (internal)",
            )
        })?;
        crate::native_direct::diag_mesh_probe(cfg, store.root(), &targets)?
    };

    if !items.is_empty() && items.iter().all(|i| i.result.is_err()) {
        return Err(dominant_error(&items));
    }

    let inputs: Vec<mat_core::mesh::NodeInput> = items
        .into_iter()
        .map(|i| mat_core::mesh::NodeInput {
            node_id: i.node_id,
            alias: book.node_alias_of(i.node_id).map(str::to_string),
            probe: i.result.map_err(|e| mat_core::mesh::ProbeFailure {
                kind: e.kind,
                detail: e.detail,
            }),
        })
        .collect();
    let graph = mat_core::mesh::build_graph(&inputs, &book.thread_labels());
    let body = serde_json::to_value(&graph)
        .map_err(|e| MatError::parse_error(format!("serialize mesh graph: {e}")))?;
    output::emit(body);
    Ok(())
}

/// 全ノード probe 失敗時のトップレベルエラー: 最頻の失敗 kind（同数タイは
/// 先勝ち）+ per-node detail の列挙。
fn dominant_error(items: &[crate::native_direct::MeshProbeItem]) -> MatError {
    let mut counts: Vec<(ErrorKind, usize)> = Vec::new();
    for it in items {
        if let Err(e) = &it.result {
            match counts.iter_mut().find(|(k, _)| *k == e.kind) {
                Some((_, c)) => *c += 1,
                None => counts.push((e.kind, 1)),
            }
        }
    }
    // 先勝ちタイ: 厳密により大きい時だけ更新。
    let mut best = ErrorKind::Other;
    let mut best_n = 0usize;
    for (k, n) in counts {
        if n > best_n {
            best = k;
            best_n = n;
        }
    }
    let detail: Vec<String> = items
        .iter()
        .filter_map(|it| {
            it.result
                .as_ref()
                .err()
                .map(|e| format!("node {}: {}", it.node_id, e.detail))
        })
        .collect();
    MatError::new(
        best,
        format!(
            "all {} mesh probes failed: {}",
            items.len(),
            detail.join("; ")
        ),
    )
}
```

- [ ] **Step 5: 実装 — main.rs dispatch**

`main.rs` の専用コマンド層 match（`DiagCommand::Node` arm の後）に追加:

```rust
        Command::Diag {
            action: DiagCommand::Mesh { nodes },
        } => nodes
            .iter()
            .map(mat_core::alias::NodeRef::id)
            .collect::<Result<Vec<u64>, MatError>>()
            .and_then(|ids| commands::diag::mesh(&store_path, &ids, native_cfg.as_ref())),
```

（`NodeRef::id` はフルパスで呼べば `use` 追加不要。既に `use` があるなら合わせる。）

- [ ] **Step 6: テスト成功を確認**

Run: `cargo test -p mat`
Expected: PASS（`dominant_error` 2 テスト含む）

- [ ] **Step 7: `task check` → コミット**

```bash
task check
git add crates/mat/src/native_direct.rs crates/mat/src/commands/diag.rs crates/mat/src/main.rs
git commit -m "feat(mat): diag mesh — 収集ループ・グラフ emit・全滅時の最頻 kind 写像"
```

---

### Task 7: 統合テスト + ドキュメント + バージョン 1.1.0

**Files:**
- Modify: `crates/mat/tests/integration.rs`
- Modify: `README.md`（diag mesh の節 + `[thread]` セクション + direct-only 記述確認）
- Modify: `CLAUDE.md`（direct-only op リストの確認 — 既に `diag` 一括表記なら無変更で可）
- Modify: `Cargo.toml`（workspace version 1.0.0 → 1.1.0）
- Modify: `docs/superpowers/specs/2026-07-23-diag-mesh-topology-design.md`（`node:<node_id>` フォールバック ID の追記）

**Interfaces:**
- Consumes: Task 1–6 の全成果（バイナリ実行）。

- [ ] **Step 1: 失敗する統合テストを書く**

`crates/mat/tests/integration.rs` に追加（既存 `mat()` / `store_with_node5()` helper と predicates パターンを踏襲）:

```rust
#[test]
fn diag_mesh_missing_store_exits_10() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("no-store");
    mat(&missing)
        .args(["diag", "mesh"])
        .assert()
        .failure()
        .code(10)
        .stderr(predicate::str::contains("store_missing"));
}

#[test]
fn diag_mesh_empty_store_emits_empty_graph_exit_0() {
    // store は存在するが commission 済みノード 0 → バックエンド未接触で
    // 空グラフ + timestamp を返す。
    let dir = TempDir::new().unwrap();
    mat(dir.path())
        .args(["diag", "mesh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"nodes\":[]"))
        .stdout(predicate::str::contains("\"edges\":[]"))
        .stdout(predicate::str::contains("timestamp"));
}

#[test]
fn diag_mesh_unknown_node_exits_11() {
    let dir = store_with_node5();
    mat(dir.path())
        .args(["diag", "mesh", "--nodes", "99"])
        .assert()
        .failure()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn diag_mesh_unknown_alias_exits_2() {
    let dir = store_with_node5();
    mat(dir.path())
        .args(["diag", "mesh", "--nodes", "no_such_alias"])
        .assert()
        .failure()
        .code(2);
}
```

注意: `diag_mesh_empty_store_emits_empty_graph_exit_0` は `TempDir::new()` の空ディレクトリを store として渡す。`Store::open` はディレクトリ実在のみ要求（台帳無し = ノード 0 件）なので成立する。stdout の JSON はコンパクト形（`serde_json` 既定）を想定 — もし `"nodes": []` 形で出る場合は predicate を実出力に合わせて調整してよい（スキーマの意味は変えない）。

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat --test integration diag_mesh`
Expected: 4 テストのうち少なくとも empty_store が現状の main dispatch 済みなら PASS の可能性あり — 全部 PASS ならこの Step は「回帰ピン確認」として扱い先へ進む。FAIL があれば Task 6 までの実装漏れなので修正。

- [ ] **Step 3: README 追記**

`README.md` の diag 節（`diag thread` / `diag node` の並び）に追加。内容:

- 概要: メッシュ全体のトポロジー（ノード + 双方向リンク品質 + route）を 1 JSON で返す。自 fabric ノードを順に診断し、未知参加者（OTBR BR / 他 fabric デバイス）もグラフ化。
- 実行例と出力例（スペックの JSON 概形を流用。ExtAddress はダミー hex、`timestamp` 付き）。
- `--nodes` の説明（省略 = 全 commission 済みノード）。
- `aliases.toml` の `[thread]` セクション例:

```toml
[thread]
# Thread ExtAddress (16 hex) -> 表示ラベル。mat diag mesh の未知参加者に名前を付ける。
"AABBCCDDEEFF0011" = "otbr-br"
```

- direct-only であること（matd 経路なし）。README に direct-only op の列挙があれば `diag mesh` を追記（`diag` 一括表記なら文言確認のみ）。
- 収集は逐次で、ノード数に比例して時間がかかる旨（8 ノードで数十秒目安）。
- exit code: 部分失敗は exit 0（JSON 内 `probe_error`）、全滅は最頻 kind（例: 全 unreachable → 5）。

CLAUDE.md の direct-only リストは「`diag`」一括表記（`discover` / `commission` / `fabric init` / `open-window` / `diag` / `group grant`）なので **無変更**。確認だけ行う。

- [ ] **Step 4: スペックへ ID フォールバック追記**

`docs/superpowers/specs/2026-07-23-diag-mesh-topology-design.md` の「JSON スキーマ」節、`id` の説明を修正:

> - ノードの安定キー `id` は `ext:<HEX16>`、ExtAddress 不明なら `rloc:<hex>`、どちらも無い fabric ノード（0x33 が読めず probe 失敗等）は `node:<node_id>`。

- [ ] **Step 5: バージョン bump**

`Cargo.toml`（workspace）の `version = "1.0.0"` → `version = "1.1.0"`。

- [ ] **Step 6: 全体確認**

Run: `task check`
Expected: fmt / clippy / 全テスト PASS

- [ ] **Step 7: コミット**

```bash
git add crates/mat/tests/integration.rs README.md Cargo.toml Cargo.lock \
        docs/superpowers/specs/2026-07-23-diag-mesh-topology-design.md
git commit -m "feat(mat): diag mesh 統合テスト + README/スペック追記、1.1.0"
```

（`Cargo.lock` は version bump で更新される場合のみ含める。）

---

### Task 8: 実機 E2E（jarvis）— マージ前ゲート【メインセッション実施】

**このタスクは subagent ではなくメインセッションで行う**（実機・ssh・ユーザー確認を伴う）。メモリ `e2e-before-merge` の規律: main マージ前に jarvis 実機 E2E。検証は `*.new` バイナリで行い、本番バイナリの置換はマージ後デプロイで行う。

- [ ] **Step 1:** `task dist:arm64` でクロスビルド（aarch64-gnu + BLE）
- [ ] **Step 2:** `scp dist/arm64/mat jarvis:~/mat.new`（scp 素で可 — メモリ `scp-use-scp-exe`）
- [ ] **Step 3:** jarvis 上で `MAT_FABRIC_INDEX=2 ~/mat.new diag mesh` を実行（直経路・非対話 ssh は `MAT_FABRIC_INDEX=2` 必須 — メモリ `jarvis-matd-deploy`）
- [ ] **Step 4:** 検証観点:
  - 全 commission 済みノードが `nodes` に載る（probe 失敗ノードも `probed:false` で残る）
  - OTBR BR（jarvis 自身の wpan0）が unknown ノードとしてグラフに現れる
  - 既知の弱リンク/強リンク（静的 HTML 版の手集計）とエッジ実測が整合する
  - SED（人感センサー等）が親ルータにぶら下がる形で出る
  - `aliases.toml` に `[thread]` で BR の ExtAddress を登録するとラベルが付く
  - partition_ids が単一（分断なし）
- [ ] **Step 5:** 結果をユーザーへ報告し、マージ判断を仰ぐ（finishing-a-development-branch）
