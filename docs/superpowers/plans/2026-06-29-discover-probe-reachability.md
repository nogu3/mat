# `mat discover --probe` 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat discover` に opt-in `--probe` フラグを追加し、commissioned ノードのライブ到達性（mDNS 広告の有無）を `reachable` / `stale` フィールドとして反映する。

**Architecture:** mDNS ブラウズ（`avahi-browse -rt _matter._tcp`）を 1 回だけ実行して全 commissioned ノードを `node_id` で照合する。照合ロジックは `mat-core` の純関数に切り出してユニットテストし、mDNS プローブ（プロセス起動）は `mat` バイナリ側の共有 `probe` モジュールに集約して `diag` と `discover` の両方から使う。

**Tech Stack:** Rust / clap（CLI）/ serde_json / assert_cmd + predicates（統合テスト）/ Task（`task check`）。

## Global Constraints

- stdout は純粋な構造化 JSON のみ。人間向け装飾なし。chip-tool 出力を素通ししない。（CLAUDE.md ルール2）
- 診断・警告は stderr へ `tracing` の構造化ログで出す。（CLAUDE.md ルール3）
- 認証情報 KVS 以外の永続状態を持たない。`mat-core` は副作用なし（プロセス起動はバイナリ側）。（CLAUDE.md ルール4）
- リポジトリは public。実 IP / node_id / 証明書をコミットしない。サンプルは RFC 5737 `192.0.2.0/24` のダミーのみ。
- 各コミット前に `task check`（fmt:check + clippy `-D warnings` + test）が通ること。
- `--probe` 無しの `mat discover` 出力は現状と完全に同一（後方互換）。`reachable` / `stale` フィールドを付与しない。

## ファイル構成

- **新規** `crates/mat-core/src/reachability.rs` — 純関数 `resolve()` と `NodeReachability` 型 + ユニットテスト。副作用なし。
- **新規** `crates/mat/src/probe.rs` — `mdns()`（avahi-browse 実行 + パース）。`diag` の私的 `probe_mdns` を移設した共有版。
- 変更 `crates/mat-core/src/lib.rs` — `pub mod reachability;` を追加。
- 変更 `crates/mat/src/main.rs` — `mod probe;` 追加、`Discover { probe }` ディスパッチ。
- 変更 `crates/mat/src/cli.rs` — `Command::Discover` を `{ probe: bool }` struct variant に。
- 変更 `crates/mat/src/commands/discover.rs` — `run(store_path, probe)`、到達性反映。
- 変更 `crates/mat/src/commands/diag.rs` — 私的 `probe_mdns` を削除し `crate::probe::mdns()` を使用。
- 変更 `crates/mat/src/matd_client.rs` — `Command::Discover` パターンを struct variant 形に追従（挙動不変）。
- 変更 `crates/mat/tests/integration.rs` — `discover --probe` 統合テスト追加。
- 変更 `README.md` — `--probe` と `reachable` / `stale` の説明追記。

---

### Task 1: `mat-core` 到達性照合の純関数

**Files:**
- Create: `crates/mat-core/src/reachability.rs`
- Modify: `crates/mat-core/src/lib.rs`（`pub mod reachability;` を追加）
- Test: `crates/mat-core/src/reachability.rs`（同ファイル末尾 `#[cfg(test)]`）

**Interfaces:**
- Consumes: `mat_core::diag::MatterInstance`（既存。`{ compressed_fabric: String, node_id: u64, addresses: Vec<String> }`）
- Produces:
  - `pub struct NodeReachability { pub reachable: bool, pub live_address: Option<String> }`
  - `pub fn resolve(node_id: u64, ledger_address: Option<&str>, instances: &[MatterInstance]) -> NodeReachability`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/reachability.rs` を新規作成し、まずテストだけを書く（本体は後でコンパイルを通す最小スタブ）。

```rust
//! commissioned ノードの台帳エントリを、ライブ mDNS インスタンス一覧に照合して
//! 到達性を判定する純ロジック。副作用なし（プロセス起動はバイナリ側 `probe` が担う）。
//!
//! 照合は `node_id` で行う（台帳 node_id は自 fabric が採番した値）。同一 node_id が
//! 別 fabric で広告されると偽陽性の可能性があるが、当面はベストエフォート。fabric
//! 厳密判別には compressed-fabric-id（CASE を要する重い経路）が必要なため避ける。

use crate::diag::MatterInstance;

/// 1 ノードの照合結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeReachability {
    /// ライブ mDNS に当該 node_id の広告が見つかったか。
    pub reachable: bool,
    /// 解決できたライブアドレス（見つからなければ None）。台帳アドレスが一致
    /// インスタンスの addresses に含まれればそれを、無ければ先頭アドレスを返す。
    pub live_address: Option<String>,
}

/// `node_id` でライブインスタンスに照合する。
pub fn resolve(
    node_id: u64,
    ledger_address: Option<&str>,
    instances: &[MatterInstance],
) -> NodeReachability {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(node_id: u64, addrs: &[&str]) -> MatterInstance {
        MatterInstance {
            compressed_fabric: "00AABB1122CC3344".to_string(),
            node_id,
            addresses: addrs.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn matched_returns_reachable_with_first_live_address() {
        let instances = [inst(5, &["192.0.2.99"])];
        let r = resolve(5, Some("192.0.2.10"), &instances);
        assert!(r.reachable);
        assert_eq!(r.live_address, Some("192.0.2.99".to_string()));
    }

    #[test]
    fn matched_prefers_ledger_address_when_present() {
        let instances = [inst(5, &["192.0.2.99", "192.0.2.10"])];
        let r = resolve(5, Some("192.0.2.10"), &instances);
        assert!(r.reachable);
        assert_eq!(r.live_address, Some("192.0.2.10".to_string()));
    }

    #[test]
    fn not_matched_returns_unreachable() {
        let instances = [inst(255, &["192.0.2.50"])];
        let r = resolve(5, Some("192.0.2.10"), &instances);
        assert!(!r.reachable);
        assert_eq!(r.live_address, None);
    }

    #[test]
    fn matched_announce_only_is_reachable_without_address() {
        let instances = [inst(5, &[])];
        let r = resolve(5, None, &instances);
        assert!(r.reachable);
        assert_eq!(r.live_address, None);
    }
}
```

`crates/mat-core/src/lib.rs` に `pub mod reachability;` を追加（既存の `pub mod parse;` の並びに合わせアルファベット順 = `pub mod parse;` の後、`pub mod socket;` の前）。

```rust
pub mod parse;
pub mod reachability;
pub mod socket;
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-core reachability`
Expected: コンパイルは通り（`todo!()` 本体）、4 テストとも実行時に `not yet implemented` で panic（FAIL）。

- [ ] **Step 3: 本体を実装**

`resolve` の `todo!()` を実装に差し替える。

```rust
pub fn resolve(
    node_id: u64,
    ledger_address: Option<&str>,
    instances: &[MatterInstance],
) -> NodeReachability {
    let matched: Vec<&MatterInstance> =
        instances.iter().filter(|i| i.node_id == node_id).collect();
    if matched.is_empty() {
        return NodeReachability {
            reachable: false,
            live_address: None,
        };
    }
    // 台帳アドレスが一致インスタンスの addresses に含まれればそれを優先（安定性）。
    if let Some(addr) = ledger_address {
        if matched.iter().any(|i| i.addresses.iter().any(|a| a == addr)) {
            return NodeReachability {
                reachable: true,
                live_address: Some(addr.to_string()),
            };
        }
    }
    // 含まれなければ最初の非空アドレスの先頭を採る（announce のみなら None）。
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

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core reachability`
Expected: 4 テストとも PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-core/src/reachability.rs crates/mat-core/src/lib.rs
git commit -m "feat(reachability): commissioned ノードの mDNS 照合純関数を追加

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: mDNS プローブを共有 `probe` モジュールへ集約

`diag.rs` の私的 `probe_mdns()` を `mat` バイナリの共有モジュール `probe::mdns()` に移し、`diag` をそれに繋ぎ替える。`discover` から再利用できるようにするための前段。挙動は不変で、既存 diag 統合テストが回帰ガード。

**Files:**
- Create: `crates/mat/src/probe.rs`
- Modify: `crates/mat/src/main.rs`（`mod probe;` を追加）
- Modify: `crates/mat/src/commands/diag.rs`（`probe_mdns` 削除、呼び出しを `crate::probe::mdns()` に、未使用 import 整理）
- Test: 既存 `crates/mat/tests/integration.rs` の diag node 系テスト（回帰ガード）

**Interfaces:**
- Produces: `pub fn mdns() -> Result<Vec<mat_core::diag::MatterInstance>, mat_core::error::MatError>`（`MAT_AVAHI_BROWSE_BIN` でバイナリ上書き可。バイナリ不在は `ErrorKind::ChildNotFound`、その他 spawn 失敗は `ErrorKind::Other`）

- [ ] **Step 1: `probe` モジュールを作成**

`crates/mat/src/probe.rs` を新規作成（`diag.rs` の `probe_mdns` 本体をそのまま移植）。

```rust
//! mDNS プローブ（`avahi-browse`）。プロセス起動を伴うため副作用なしの `mat-core`
//! ではなくバイナリ側に置く。`diag node --deep` と `discover --probe` が共有する。

use std::ffi::OsString;
use std::process::Command as StdCommand;

use mat_core::diag::{parse_avahi_matter, MatterInstance};
use mat_core::error::{ErrorKind, MatError};

/// `avahi-browse -rt _matter._tcp` を実行して `_matter._tcp` インスタンスを得る。
/// バイナリは `MAT_AVAHI_BROWSE_BIN` で上書き可。
pub fn mdns() -> Result<Vec<MatterInstance>, MatError> {
    let bin =
        std::env::var_os("MAT_AVAHI_BROWSE_BIN").unwrap_or_else(|| OsString::from("avahi-browse"));
    let out = StdCommand::new(&bin)
        .args(["-rt", "_matter._tcp"])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MatError::child_not_found(format!("avahi-browse not found ({bin:?})"))
            } else {
                MatError::new(
                    ErrorKind::Other,
                    format!("avahi-browse spawn failed ({bin:?}): {e}"),
                )
            }
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    let stderr_text = String::from_utf8_lossy(&out.stderr);
    tracing::debug!(%text, "avahi-browse stdout");
    tracing::debug!(%stderr_text, "avahi-browse stderr");
    Ok(parse_avahi_matter(&text))
}
```

`crates/mat/src/main.rs` の `mod` 宣言群に `mod probe;` を追加（`mod matd_client;` と `mod runner;` の間、アルファベット順）。

```rust
mod cli;
mod commands;
mod matd_client;
mod probe;
mod runner;
```

- [ ] **Step 2: `diag.rs` を繋ぎ替え**

`crates/mat/src/commands/diag.rs` で以下を行う。

1. 私的関数 `probe_mdns()`（`/// `avahi-browse -rt _matter._tcp` を実行して…` のコメントごと関数本体）を **削除**する。
2. `deep_probes` 内の呼び出し `match probe_mdns() {` を `match crate::probe::mdns() {` に変更する。
3. import 整理: 行 26 付近の `use mat_core::diag::{ ... parse_avahi_matter ... };` から `parse_avahi_matter` を**削除**する（移設先でのみ使用、ここでは未使用 = clippy `-D warnings` で落ちる）。`OsString` / `StdCommand` は `probe_ping6` が引き続き使うため**残す**。

- [ ] **Step 3: ビルドと既存テストで回帰確認**

Run: `cargo test -p mat --test integration diag_node`
Expected: `diag_node_deep_link_starved` / `diag_node_deep_ip_unreachable` / `diag_node_deep_fabric_missing` を含む diag node テストが全て PASS（挙動不変）。

Run: `cargo clippy -p mat -- -D warnings`
Expected: 警告ゼロ（未使用 import が残っていないこと）。

- [ ] **Step 4: コミット**

```bash
git add crates/mat/src/probe.rs crates/mat/src/main.rs crates/mat/src/commands/diag.rs
git commit -m "refactor(probe): mDNS プローブを共有 probe モジュールへ集約

diag の私的 probe_mdns を crate::probe::mdns へ移し discover からも
再利用できるようにする。挙動は不変。

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: `--probe` フラグと `discover` の到達性反映

CLI に `--probe` を追加し、`discover` で commissioned ノードの到達性を反映する。

**Files:**
- Modify: `crates/mat/src/cli.rs`（`Command::Discover` を struct variant 化）
- Modify: `crates/mat/src/main.rs`（ディスパッチ更新）
- Modify: `crates/mat/src/matd_client.rs`（`Command::Discover` パターン追従、テスト追従）
- Modify: `crates/mat/src/commands/discover.rs`（`run(store_path, probe)`、到達性反映）
- Test: `crates/mat/tests/integration.rs`（Task 4 で追加）

**Interfaces:**
- Consumes: `mat_core::reachability::resolve`（Task 1）、`crate::probe::mdns`（Task 2）、`mat_core::diag::MatterInstance`
- Produces: `pub fn run(store_path: &std::path::Path, probe: bool) -> Result<(), MatError>`

- [ ] **Step 1: CLI フラグを追加**

`crates/mat/src/cli.rs` の `Command::Discover` を unit variant から struct variant へ変更する。

変更前:
```rust
    /// commissionable / commissioned ノードを mDNS で探索する。
    Discover,
```
変更後:
```rust
    /// commissionable / commissioned ノードを mDNS で探索する。
    Discover {
        /// commissioned ノードのライブ到達性を mDNS で確認し reachable を付与する。
        #[arg(long)]
        probe: bool,
    },
```

- [ ] **Step 2: ディスパッチと matd_client を追従**

`crates/mat/src/main.rs` 行 40:
変更前:
```rust
        Command::Discover => commands::discover::run(&store_path),
```
変更後:
```rust
        Command::Discover { probe } => commands::discover::run(&store_path, *probe),
```

`crates/mat/src/matd_client.rs` 行 156:
変更前:
```rust
        Command::Discover => return Err(unsupported("discover")),
```
変更後:
```rust
        Command::Discover { .. } => return Err(unsupported("discover")),
```

`crates/mat/src/matd_client.rs` のテスト（行 307 付近）:
変更前:
```rust
        assert!(to_op(&Command::Discover).is_err());
```
変更後:
```rust
        assert!(to_op(&Command::Discover { probe: false }).is_err());
```

- [ ] **Step 3: `discover.rs` を実装**

`crates/mat/src/commands/discover.rs` を以下に置き換える（import の追加 + `run` シグネチャ変更 + commissioned ループの到達性反映）。

```rust
//! `mat discover` — commissionable / commissioned ノードを探索する。
//!
//! commissionable は `chip-tool discover commissionables` の mDNS 探索結果、
//! commissioned は `mat` の台帳（KVS）から読む。両者を1つの `devices` 配列にまとめる。
//!
//! `--probe` 指定時は commissioned ノードについても mDNS を 1 回ブラウズして
//! ライブ到達性を判定し、`reachable`（true/false/null）と、不達時の `stale` を付与する。
//! 既定（`--probe` 無し）は台帳をそのまま出す高速経路で、出力は従来と完全に同一。
//!
//! commissionable 探索は認証情報不要のため、store 無しでも動く（無ければ空ストアを
//! bootstrap し、commissioned は空配列になる）。

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::runner::ChipTool;
use mat_core::diag::MatterInstance;
use mat_core::error::MatError;
use mat_core::output;
use mat_core::parse::parse_commissionables;
use mat_core::reachability::resolve;
use mat_core::store::Store;

pub fn run(store_path: &Path, probe: bool) -> Result<(), MatError> {
    // discover の commissionable 探索は認証情報不要。store 無しでも動くべきなので
    // open ではなく open_or_init（無ければ空ストアを bootstrap）。commissioned は
    // 台帳から読むが、空ストアなら空配列になるだけ。
    let store = Store::open_or_init(store_path)?;
    let chip = ChipTool::new(store.root());

    // commissionable 探索。chip-tool は探索を時間で打ち切るため非 0 終了もあり得る。
    // ここでは exit code で失敗扱いにせず、得られた行をパースする（child_not_found
    // = exit 12 だけは run() がエラーで返す）。
    let out = chip.run(["discover", "commissionables"])?;
    let commissionable = parse_commissionables(&out.stdout);

    let mut devices = Vec::new();
    for d in &commissionable {
        let mut v = serde_json::to_value(d).map_err(|e| {
            MatError::parse_error(format!("cannot serialize discovered device: {e}"))
        })?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("state".into(), json!("commissionable"));
        }
        devices.push(v);
    }

    // --probe: commissioned ノードのライブ到達性を判定するため mDNS を 1 回だけ
    // ブラウズする。None = 未実施 or 実施不能（後者は reachable:null）。
    let instances: Option<Vec<MatterInstance>> = if probe {
        match crate::probe::mdns() {
            Ok(list) => Some(list),
            Err(e) => {
                tracing::warn!(
                    detail = %e.detail,
                    kind = ?e.kind,
                    "discover --probe: mDNS browse failed; reachability unknown"
                );
                None
            }
        }
    } else {
        None
    };

    for n in store.nodes() {
        let mut obj = Map::new();
        obj.insert("state".into(), json!("commissioned"));
        obj.insert("node_id".into(), json!(n.node_id));
        obj.insert("commissioned_at".into(), json!(n.commissioned_at));
        match (probe, instances.as_deref()) {
            // 既定: 台帳そのまま（従来出力と同一）。
            (false, _) => {
                obj.insert("address".into(), json!(n.address));
            }
            // --probe だがプローブ実施不能 → 到達性不明。
            (true, None) => {
                obj.insert("address".into(), json!(n.address));
                obj.insert("reachable".into(), Value::Null);
            }
            // --probe 成功 → node_id 照合で到達性判定。
            (true, Some(list)) => {
                let r = resolve(n.node_id, n.address.as_deref(), list);
                obj.insert("reachable".into(), json!(r.reachable));
                if r.reachable {
                    // ライブ解決アドレスを優先、無ければ台帳値（announce のみ等）。
                    let addr = r.live_address.or_else(|| n.address.clone());
                    obj.insert("address".into(), json!(addr));
                } else {
                    // 据え置きの台帳値に stale 印を付ける。
                    obj.insert("address".into(), json!(n.address));
                    obj.insert("stale".into(), json!(true));
                }
            }
        }
        devices.push(Value::Object(obj));
    }

    output::emit(json!({ "devices": devices }));
    Ok(())
}
```

- [ ] **Step 4: ビルドと既存テストの確認**

Run: `cargo test -p mat --test integration discover`
Expected: 既存 `discover_lists_commissionable_devices` / `discover_with_missing_store_bootstraps_and_succeeds` / `discover_with_missing_chip_tool_exits_12` が PASS（`--probe` 無しは従来通り）。

Run: `cargo test -p mat matd_client` （あるいは `cargo test -p mat --lib`）
Expected: `discover_and_commission_are_unsupported` を含む matd_client テストが PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/mat/src/cli.rs crates/mat/src/main.rs crates/mat/src/matd_client.rs crates/mat/src/commands/discover.rs
git commit -m "feat(discover): --probe で commissioned ノードのライブ到達性を反映

mDNS を1回ブラウズして node_id 照合し reachable/stale を付与。プローブ
不能時は reachable:null。--probe 無しの出力は従来と同一。

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: `discover --probe` 統合テスト

fake-chip-tool / fake-avahi フィクスチャで end-to-end を検証する。`store_with_node5()`（node 5 を target `192.0.2.10` で commission 済み）と `fake_avahi()`（`FAKE_AVAHI_ADDR` 設定時に node 5 を当該アドレスで広告、未設定時は他 fabric の node FF のみ）を再利用する。

**Files:**
- Modify: `crates/mat/tests/integration.rs`（テスト 4 件を追加。`store_with_node5` / `fake_avahi` ヘルパは既存）

**Interfaces:**
- Consumes: 既存ヘルパ `mat(&Path) -> Command`、`store_with_node5() -> TempDir`、`fake_avahi() -> PathBuf`

- [ ] **Step 1: テストを追加**

`crates/mat/tests/integration.rs` の末尾（`fake_avahi()` ヘルパ定義より後の任意の位置）に以下 4 テストを追加する。

```rust
#[test]
fn discover_probe_reports_reachable_with_live_address() {
    // node 5 を commission 済み（台帳 address = 192.0.2.10）。avahi が node 5 を
    // 別アドレス 192.0.2.99 で広告 → reachable:true、address はライブ値に更新。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .env("FAKE_AVAHI_ADDR", "192.0.2.99")
        .args(["discover", "--probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"state\":\"commissioned\""))
        .stdout(predicate::str::contains("\"reachable\":true"))
        .stdout(predicate::str::contains("\"address\":\"192.0.2.99\""));
}

#[test]
fn discover_probe_reports_unreachable_and_stale() {
    // avahi に node 5 の広告なし（既定出力は node FF のみ）→ reachable:false、
    // stale:true、address は台帳の据え置き値 192.0.2.10。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .args(["discover", "--probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"reachable\":false"))
        .stdout(predicate::str::contains("\"stale\":true"))
        .stdout(predicate::str::contains("\"address\":\"192.0.2.10\""));
}

#[test]
fn discover_without_probe_omits_reachable() {
    // --probe 無しは従来出力（reachable/stale を付与しない）。後方互換。
    let store = store_with_node5();
    mat(store.path())
        .arg("discover")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"state\":\"commissioned\""))
        .stdout(predicate::str::contains("\"reachable\"").not())
        .stdout(predicate::str::contains("\"stale\"").not());
}

#[test]
fn discover_probe_with_missing_avahi_reports_reachable_null() {
    // avahi-browse バイナリ不在 → プローブ不能。reachable:null、stdout は純 JSON、
    // discover 全体は成功（commissionable 探索は別経路で有効なため）。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_AVAHI_BROWSE_BIN", "/nonexistent/avahi-browse-binary")
        .args(["discover", "--probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"reachable\":null"));
}
```

注: `predicate::str::contains(...).not()` を使うため、ファイル冒頭で `use predicates::prelude::*;`（または既存の `predicate` import に加えて `PredicateBooleanExt`）が有効であること。既存テストが `predicate::str::contains(...)` を使っているので prelude は導入済みのはず。もし `.not()` が解決しなければ `use predicates::prelude::PredicateBooleanExt;` を冒頭に追加する。

- [ ] **Step 2: テストを実行**

Run: `cargo test -p mat --test integration discover_probe`
Expected: `discover_probe_reports_reachable_with_live_address` / `discover_probe_reports_unreachable_and_stale` / `discover_probe_with_missing_avahi_reports_reachable_null` が PASS。

Run: `cargo test -p mat --test integration discover_without_probe`
Expected: `discover_without_probe_omits_reachable` が PASS。

- [ ] **Step 3: コミット**

```bash
git add crates/mat/tests/integration.rs
git commit -m "test(discover): --probe の到達性反映を fake-avahi で統合テスト

reachable:true(live addr)/false(stale)/null(probe不能) と --probe 無しの
後方互換を検証。

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: README ドキュメント更新

`--probe` と `reachable` / `stale` をユーザ向けに説明する。

**Files:**
- Modify: `README.md`（Discover 節）

- [ ] **Step 1: Discover 節を更新**

`README.md` の Discover 節（行 49–70 付近）を編集する。

コマンド例（行 52–53）を変更:
変更前:
```bash
# Discover commissionable / commissioned nodes
mat discover
```
変更後:
```bash
# Discover commissionable / commissioned nodes (ledger only, fast)
mat discover

# Also probe live reachability of commissioned nodes via mDNS
mat discover --probe
```

`discover` 出力例（行 60–70）の直後に、`--probe` の説明と出力例を追記する。`commission output:`（行 72 の `\`commission\` output:`）の直前に以下を挿入:

```markdown
With `--probe`, each `commissioned` node is checked against live mDNS
(`avahi-browse _matter._tcp`, one browse for all nodes) and annotated:

- `reachable: true` — advertising now; `address` is the live-resolved value
  (may differ from the ledger).
- `reachable: false` — not advertising; `address` is the last-known ledger
  value with `stale: true`.
- `reachable: null` — the mDNS probe could not run (e.g. `avahi-browse`
  missing); reachability is unknown. A diagnostic is logged to stderr.

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "devices": [
    { "state": "commissioned", "node_id": 5, "address": "192.0.2.99", "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": true },
    { "state": "commissioned", "node_id": 7, "address": "192.0.2.10", "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": false, "stale": true }
  ]
}
```

Without `--probe` the output is unchanged (no `reachable` / `stale`); the
ledger is reported as-is and reflects no live reachability. Node-id matching
is best-effort (a cross-fabric node_id collision could false-positive); for a
deeper single-node check use `mat diag node --deep`.
```

注: `192.0.2.0/24` は RFC 5737 のダミー範囲。実 IP / 実 node_id を書かないこと。

- [ ] **Step 2: 整合性チェック**

Run: `grep -n "reachable\|--probe\|stale" README.md`
Expected: 追記した `--probe` / `reachable` / `stale` の記述が表示され、誤って実 IP を書いていないこと（`192.0.2.x` のみ）。

- [ ] **Step 3: コミット**

```bash
git add README.md
git commit -m "docs(readme): discover --probe と reachable/stale を説明

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### 最終確認（全タスク後）

- [ ] **Step 1: CI 相当チェック**

Run: `task check`
Expected: fmt:check / clippy（`-D warnings`）/ test 全て PASS。

- [ ] **Step 2: 受け入れ基準の確認**

- `mat discover`（フラグ無し）の出力が変更前と同一（`reachable` / `stale` なし）。
- `mat discover --probe` が commissioned ノードに `reachable` を付与し、不達時は `stale:true`、プローブ不能時は `reachable:null`。
- stdout は純 JSON、プローブ失敗の診断は stderr のみ。
- 到達性照合の純関数ユニットテストと `--probe` 統合テストが存在し PASS。
