# mat diag node Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `mat diag node` — a subcommand that classifies *why* a commissioned node is unreachable into a single `verdict` (e.g. `link_starved`, `fabric_missing`) plus evidence and a recommended action.

**Architecture:** Pure data model + parsers + decision tree live in a new `mat-core::diag` module (fully unit-tested). The orchestration in `commands/diag.rs::node()` gathers checks (operational read + thread signal via `chip-tool`; optional `ping6` + `avahi-browse` probes under `--deep`), builds a typed `Checks`, runs `derive_verdict`, and emits the result JSON. Follows the existing `diag thread` partial-results style.

**Tech Stack:** Rust (workspace), clap(derive), serde/serde_json, existing `mat_core` helpers (`classify_failure`, `parse_struct_list`, `parse_read_value`, `Store`, `ChipTool`, `output::emit`).

## Global Constraints

- stdout is pure structured JSON only; diagnostics go to stderr (`tracing`). (CLAUDE.md ②③)
- Do not speak the protocol; delegate Matter work to `chip-tool`. Default path uses only `chip-tool`; `--deep` adds `ping6`/`avahi-browse` probes (opt-in).
- `timestamp` field required, ISO 8601 — added automatically by `output::emit`.
- Never put real node_ids / IPs / fabric ids in code or fixtures; use RFC 5737 (`192.0.2.0/24`) and dummy values.
- Diagnostic command must NOT exit non-zero on an unreachable node — it always returns a JSON verdict with exit 0. (Only `ChildNotFound`/`StoreMissing`/`NodeNotCommissioned` propagate as errors.)
- `task check` (fmt + clippy -D warnings + test) must pass before any commit.

---

### Task 1: `mat-core::diag` data model

**Files:**
- Create: `crates/mat-core/src/diag.rs`
- Modify: `crates/mat-core/src/lib.rs` (add `pub mod diag;`)

**Interfaces:**
- Produces (types used by later tasks):
  - `MatterInstance { compressed_fabric: String, node_id: u64 }` (PartialEq, Clone, Debug)
  - `Ping6Stats { loss_pct: u8, rtt_ms: Option<f64> }`
  - `IpCheck { ok: bool, loss_pct: u8, rtt_ms: Option<f64>, method: &'static str }`
  - `MdnsCheck { advertised_self_fabric: Option<bool>, advertised_any_fabric: bool }`
  - `OperationalCheck { resolved: bool, kind: Option<ErrorKind> }`
  - `ThreadCheck { neighbor_count: usize, best_lqi: Option<u8>, routing_role: Option<i64> }`
  - `Checks { ip: Option<IpCheck>, mdns: Option<MdnsCheck>, operational: Option<OperationalCheck>, thread: Option<ThreadCheck> }` (Default, Serialize)
  - `VerdictKind` enum (Serialize snake_case): `Ok, IpUnreachable, LinkStarved, FabricMissing, NotAdvertised, Unresolvable, SessionFailed, DeviceRejected, Unknown`
  - `Verdict { verdict: VerdictKind, summary: String, recommendation: String }`
  - `pub const LQI_WEAK: u8 = 20;` `pub const LOSS_WEAK: u8 = 30;`

- [ ] **Step 1: Write the module with types and a smoke test**

Create `crates/mat-core/src/diag.rs`:

```rust
//! `mat diag node` の診断データモデルと純ロジック（パーサ + verdict 決定木）。
//!
//! 副作用なし。`mat` 側 `commands/diag.rs::node()` がチェックを集めて `Checks` を
//! 組み、[`derive_verdict`] で原因 `verdict` を導く。chip-tool には触れない。

use serde::Serialize;

use crate::error::ErrorKind;

/// 弱リンク判定の閾値。best LQI がこれ未満 / loss% がこれ以上なら「弱い」。
pub const LQI_WEAK: u8 = 20;
pub const LOSS_WEAK: u8 = 30;

/// mDNS に見えた `_matter._tcp` の1インスタンス（`<CFID>-<nodeid>`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatterInstance {
    /// compressed fabric id（16桁 hex、大文字正規化）。
    pub compressed_fabric: String,
    pub node_id: u64,
}

/// ping6 統計。
#[derive(Debug, Clone, PartialEq)]
pub struct Ping6Stats {
    pub loss_pct: u8,
    pub rtt_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IpCheck {
    pub ok: bool,
    pub loss_pct: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    pub method: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MdnsCheck {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertised_self_fabric: Option<bool>,
    pub advertised_any_fabric: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationalCheck {
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ErrorKind>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadCheck {
    pub neighbor_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_lqi: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_role: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Checks {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<IpCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdns: Option<MdnsCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operational: Option<OperationalCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadCheck>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Ok,
    IpUnreachable,
    LinkStarved,
    FabricMissing,
    NotAdvertised,
    Unresolvable,
    SessionFailed,
    DeviceRejected,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub verdict: VerdictKind,
    pub summary: String,
    pub recommendation: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_types_construct() {
        let c = Checks::default();
        assert!(c.operational.is_none());
        assert_eq!(LQI_WEAK, 20);
    }
}
```

Add to `crates/mat-core/src/lib.rs` after `pub mod error;` (keep alpha-ish order):

```rust
pub mod diag;
```

- [ ] **Step 2: Build and test**

Run: `cargo test -p mat-core diag::`
Expected: PASS (1 test `smoke_types_construct`).

- [ ] **Step 3: Commit**

```bash
git add crates/mat-core/src/diag.rs crates/mat-core/src/lib.rs
git commit -m "feat(diag): mat-core diag データモデル（Checks/Verdict/閾値）"
```

---

### Task 2: `parse_ping6`

**Files:**
- Modify: `crates/mat-core/src/diag.rs`

**Interfaces:**
- Produces: `pub fn parse_ping6(stdout: &str) -> Option<Ping6Stats>`

- [ ] **Step 1: Write the failing tests**

Add inside `crates/mat-core/src/diag.rs` `mod tests`:

```rust
    #[test]
    fn ping6_zero_loss_with_rtt() {
        let s = "PING x(x) 56 data bytes\n\
                 3 packets transmitted, 3 received, 0% packet loss, time 2003ms\n\
                 rtt min/avg/max/mdev = 46.773/56.351/61.236/6.773 ms\n";
        let p = parse_ping6(s).unwrap();
        assert_eq!(p.loss_pct, 0);
        assert_eq!(p.rtt_ms, Some(56.351));
    }

    #[test]
    fn ping6_total_loss_no_rtt() {
        let s = "3 packets transmitted, 0 received, 100% packet loss, time 2002ms\n";
        let p = parse_ping6(s).unwrap();
        assert_eq!(p.loss_pct, 100);
        assert_eq!(p.rtt_ms, None);
    }

    #[test]
    fn ping6_unparseable_is_none() {
        assert!(parse_ping6("ping: command not found\n").is_none());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mat-core diag::tests::ping6`
Expected: FAIL (cannot find function `parse_ping6`).

- [ ] **Step 3: Implement**

Add to `crates/mat-core/src/diag.rs` (above `mod tests`):

```rust
/// `ping6` の統計サマリ行をパースする。`loss%` 行が無ければ `None`（未実行/失敗）。
pub fn parse_ping6(stdout: &str) -> Option<Ping6Stats> {
    let mut loss_pct: Option<u8> = None;
    let mut rtt_ms: Option<f64> = None;
    for line in stdout.lines() {
        if let Some(idx) = line.find("% packet loss") {
            let head = &line[..idx];
            let num = head
                .rsplit(|c: char| c == ' ' || c == ',')
                .find(|t| !t.is_empty());
            if let Some(v) = num.and_then(|t| t.trim().parse::<f64>().ok()) {
                loss_pct = Some(v.round() as u8);
            }
        }
        if (line.contains("rtt ") || line.contains("round-trip")) && line.contains('=') {
            if let Some(rest) = line.split('=').nth(1) {
                // 例: " 46.773/56.351/61.236/6.773 ms" → avg は2番目。
                if let Some(avg) = rest.trim().split('/').nth(1) {
                    rtt_ms = avg.trim().parse::<f64>().ok();
                }
            }
        }
    }
    loss_pct.map(|loss_pct| Ping6Stats { loss_pct, rtt_ms })
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p mat-core diag::tests::ping6`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/mat-core/src/diag.rs
git commit -m "feat(diag): parse_ping6（loss% / avg rtt 抽出）"
```

---

### Task 3: `parse_avahi_matter`

**Files:**
- Modify: `crates/mat-core/src/diag.rs`

**Interfaces:**
- Produces: `pub fn parse_avahi_matter(stdout: &str) -> Vec<MatterInstance>`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    #[test]
    fn avahi_extracts_instances_human_format() {
        let s = "+   eth0 IPv6 00AABB1122CC3344-0000000000000005   _matter._tcp   local\n\
                 =   eth0 IPv6 00AABB1122CC3344-0000000000000005   _matter._tcp   local\n\
                 +   eth0 IPv6 0011223344556677-000000000000004F   _matter._tcp   local\n";
        let v = parse_avahi_matter(s);
        assert_eq!(v.len(), 2); // dedup の =/+ 重複は1件
        assert_eq!(v[0].compressed_fabric, "00AABB1122CC3344");
        assert_eq!(v[0].node_id, 5);
        assert_eq!(v[1].node_id, 0x4F);
    }

    #[test]
    fn avahi_handles_parseable_semicolons() {
        let s = "+;eth0;IPv6;00AABB1122CC3344-0000000000000005;_matter._tcp;local\n";
        let v = parse_avahi_matter(s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].node_id, 5);
    }

    #[test]
    fn avahi_empty_or_noise_is_empty() {
        assert!(parse_avahi_matter("").is_empty());
        assert!(parse_avahi_matter("avahi-browse: command not found\n").is_empty());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mat-core diag::tests::avahi`
Expected: FAIL (cannot find function `parse_avahi_matter`).

- [ ] **Step 3: Implement**

Add to `crates/mat-core/src/diag.rs`:

```rust
/// `avahi-browse -rt _matter._tcp` 出力から `<CFID>-<nodeid>` インスタンスを抽出。
/// 人間形式（空白区切り）と `-p` 形式（`;` 区切り）の両方に対応。dedup する。
pub fn parse_avahi_matter(stdout: &str) -> Vec<MatterInstance> {
    let mut out: Vec<MatterInstance> = Vec::new();
    for line in stdout.lines() {
        if !line.contains("_matter._tcp") {
            continue;
        }
        for tok in line.split(|c: char| c.is_whitespace() || c == ';') {
            if let Some((fab, node)) = tok.split_once('-') {
                let fab_ok = fab.len() == 16 && fab.bytes().all(|b| b.is_ascii_hexdigit());
                let node_ok = !node.is_empty() && node.bytes().all(|b| b.is_ascii_hexdigit());
                if fab_ok && node_ok {
                    if let Ok(node_id) = u64::from_str_radix(node, 16) {
                        let inst = MatterInstance {
                            compressed_fabric: fab.to_ascii_uppercase(),
                            node_id,
                        };
                        if !out.contains(&inst) {
                            out.push(inst);
                        }
                    }
                }
            }
        }
    }
    out
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p mat-core diag::tests::avahi`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/mat-core/src/diag.rs
git commit -m "feat(diag): parse_avahi_matter（_matter._tcp の fabric/node 抽出）"
```

---

### Task 4: `parse_compressed_fabric_id`

**Files:**
- Modify: `crates/mat-core/src/diag.rs`

**Interfaces:**
- Produces: `pub fn parse_compressed_fabric_id(stderr: &str) -> Option<String>`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    #[test]
    fn cfid_extracted_from_chip_log() {
        let s = "[FP] Fabric index 0x1 ... Compressed FabricId 0x00AABB1122CC3344, FabricId 0x1";
        assert_eq!(
            parse_compressed_fabric_id(s).as_deref(),
            Some("00AABB1122CC3344")
        );
    }

    #[test]
    fn cfid_absent_is_none() {
        assert!(parse_compressed_fabric_id("no fabric here").is_none());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mat-core diag::tests::cfid`
Expected: FAIL (cannot find function `parse_compressed_fabric_id`).

- [ ] **Step 3: Implement**

Add to `crates/mat-core/src/diag.rs`:

```rust
/// chip-tool ログの `Compressed FabricId 0x<hex>` から自 fabric の compressed id を抽出。
pub fn parse_compressed_fabric_id(stderr: &str) -> Option<String> {
    let marker = "Compressed FabricId 0x";
    let start = stderr.find(marker)? + marker.len();
    let hex: String = stderr[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    (hex.len() >= 8).then(|| hex.to_ascii_uppercase())
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p mat-core diag::tests::cfid`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/mat-core/src/diag.rs
git commit -m "feat(diag): parse_compressed_fabric_id（自 fabric の CFID 抽出）"
```

---

### Task 5: `derive_verdict` decision tree

**Files:**
- Modify: `crates/mat-core/src/diag.rs`

**Interfaces:**
- Consumes: `Checks`, `IpCheck`, `MdnsCheck`, `OperationalCheck`, `ThreadCheck`, `ErrorKind`
- Produces: `pub fn derive_verdict(checks: &Checks) -> Verdict`

- [ ] **Step 1: Write the failing tests (all branches)**

Add to `mod tests`:

```rust
    fn op(resolved: bool, kind: Option<ErrorKind>) -> OperationalCheck {
        OperationalCheck { resolved, kind }
    }

    #[test]
    fn verdict_ok_when_resolved() {
        let c = Checks { operational: Some(op(true, None)), ..Default::default() };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::Ok);
    }

    #[test]
    fn verdict_ip_unreachable() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck { ok: false, loss_pct: 100, rtt_ms: None, method: "ping6" }),
            ..Default::default()
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::IpUnreachable);
    }

    #[test]
    fn verdict_link_starved_when_not_advertised_and_weak() {
        // 今回の実機ケース: ip 生存(loss 50%)・自/any 広告なし・op timeout。
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck { ok: true, loss_pct: 50, rtt_ms: Some(168.0), method: "ping6" }),
            mdns: Some(MdnsCheck { advertised_self_fabric: Some(false), advertised_any_fabric: false }),
            thread: None,
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::LinkStarved);
    }

    #[test]
    fn verdict_link_starved_via_weak_thread() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck { ok: true, loss_pct: 0, rtt_ms: Some(50.0), method: "ping6" }),
            mdns: Some(MdnsCheck { advertised_self_fabric: Some(false), advertised_any_fabric: false }),
            thread: Some(ThreadCheck { neighbor_count: 1, best_lqi: Some(3), routing_role: Some(2) }),
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::LinkStarved);
    }

    #[test]
    fn verdict_fabric_missing() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck { ok: true, loss_pct: 0, rtt_ms: Some(50.0), method: "ping6" }),
            mdns: Some(MdnsCheck { advertised_self_fabric: Some(false), advertised_any_fabric: true }),
            thread: None,
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::FabricMissing);
    }

    #[test]
    fn verdict_not_advertised_without_weak_evidence() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck { ok: true, loss_pct: 0, rtt_ms: Some(20.0), method: "ping6" }),
            mdns: Some(MdnsCheck { advertised_self_fabric: Some(false), advertised_any_fabric: false }),
            thread: None,
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::NotAdvertised);
    }

    #[test]
    fn verdict_unresolvable_when_mdns_unknown_timeout() {
        // --deep 無し: ip/mdns は None。op timeout → unresolvable。
        let c = Checks { operational: Some(op(false, Some(ErrorKind::Timeout))), ..Default::default() };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::Unresolvable);
    }

    #[test]
    fn verdict_session_failed() {
        let c = Checks { operational: Some(op(false, Some(ErrorKind::SessionFailed))), ..Default::default() };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::SessionFailed);
    }

    #[test]
    fn verdict_device_rejected() {
        let c = Checks { operational: Some(op(false, Some(ErrorKind::DeviceRejected))), ..Default::default() };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::DeviceRejected);
    }

    #[test]
    fn verdict_unknown_fallback() {
        let c = Checks { operational: Some(op(false, Some(ErrorKind::Other))), ..Default::default() };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::Unknown);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mat-core diag::tests::verdict`
Expected: FAIL (cannot find function `derive_verdict`).

- [ ] **Step 3: Implement**

Add to `crates/mat-core/src/diag.rs`:

```rust
/// thread 診断 or ip loss から「弱リンク」か判定。
fn weak_link(checks: &Checks) -> bool {
    let thread_weak = checks.thread.as_ref().is_some_and(|t| {
        t.neighbor_count <= 1 || t.best_lqi.is_some_and(|l| l < LQI_WEAK)
    });
    let ip_weak = checks.ip.as_ref().is_some_and(|i| i.loss_pct >= LOSS_WEAK);
    thread_weak || ip_weak
}

fn verdict(kind: VerdictKind, summary: &str, rec: &str) -> Verdict {
    Verdict { verdict: kind, summary: summary.to_string(), recommendation: rec.to_string() }
}

/// チェック結果から最尤の原因 `verdict` ＋ summary ＋ recommendation を導く（純関数）。
pub fn derive_verdict(checks: &Checks) -> Verdict {
    // 解決できた = 制御可能のはず。
    if checks.operational.as_ref().is_some_and(|o| o.resolved) {
        return verdict(
            VerdictKind::Ok,
            "Operational discovery succeeded; the node should be controllable.",
            "No action needed.",
        );
    }

    // IP 不達（--deep 時のみ判定可能）。
    if let Some(ip) = &checks.ip {
        if !ip.ok {
            return verdict(
                VerdictKind::IpUnreachable,
                "The node does not respond to ping; it is off the network at the IP layer.",
                "Check power, the Thread Border Router, and network routing.",
            );
        }
    }

    // mDNS 広告の有無で判定（--deep 時のみ mdns が埋まる）。
    if let Some(mdns) = &checks.mdns {
        // 自 fabric を広告していない（false）か、CFID 不明（None）の時に分岐。
        if mdns.advertised_self_fabric != Some(true) {
            if mdns.advertised_any_fabric {
                if mdns.advertised_self_fabric == Some(false) {
                    return verdict(
                        VerdictKind::FabricMissing,
                        "Device advertises Matter under other fabrics but not ours; our fabric was likely removed.",
                        "Re-commission via multi-admin share from a controller that still has the device.",
                    );
                }
                // any 広告ありだが自 fabric 不明 → 解決失敗の一般原因へ委ねる。
            } else if weak_link(checks) {
                return verdict(
                    VerdictKind::LinkStarved,
                    "IP reachable but not advertising Matter on any fabric; weak Thread link — SRP registration likely incomplete.",
                    "Improve the Thread link (move the device near a router) or wait; do NOT factory reset — the fabric is intact.",
                );
            } else {
                return verdict(
                    VerdictKind::NotAdvertised,
                    "Not advertising Matter on any fabric, but no strong weak-link evidence.",
                    "Re-run with --deep after a power cycle; verify the Thread link.",
                );
            }
        }
    }

    // ここまで来たら operational の失敗種別で分類。
    match checks.operational.as_ref().and_then(|o| o.kind) {
        Some(ErrorKind::SessionFailed) => verdict(
            VerdictKind::SessionFailed,
            "Resolved but CASE session establishment failed.",
            "Retry; check operational credentials (CASE) state.",
        ),
        Some(ErrorKind::Timeout) | Some(ErrorKind::Unreachable) => verdict(
            VerdictKind::Unresolvable,
            "Operational discovery / resolution timed out (mDNS may be present but not resolvable now).",
            "Retry; transient mDNS/resolution failure. Use --deep to distinguish link_starved vs fabric_missing.",
        ),
        Some(ErrorKind::DeviceRejected) => verdict(
            VerdictKind::DeviceRejected,
            "CASE established but the command was rejected by the device.",
            "Check endpoint / cluster / ACL.",
        ),
        _ => verdict(
            VerdictKind::Unknown,
            "Could not classify the failure; inspect the checks.",
            "Inspect the `checks` object and chip-tool stderr.",
        ),
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p mat-core diag::`
Expected: PASS (all parser + verdict tests).

- [ ] **Step 5: Commit**

```bash
git add crates/mat-core/src/diag.rs
git commit -m "feat(diag): derive_verdict 決定木（link_starved/fabric_missing 等）"
```

---

### Task 6: CLI wiring + `node()` default path (chip-tool only)

**Files:**
- Modify: `crates/mat/src/cli.rs` (add `DiagCommand::Node`)
- Modify: `crates/mat/src/main.rs:112-116` (dispatch `DiagCommand::Node`)
- Modify: `crates/mat/src/commands/diag.rs` (add `node()` + `read_thread_signal()`)
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: `mat_core::diag::{Checks, OperationalCheck, ThreadCheck, derive_verdict, parse_compressed_fabric_id}`, `classify_failure`, `parse_struct_list`, `parse_read_value`, `ChipTool`, `Store`, `output::emit`
- Produces: `pub fn node(store_path: &Path, node_id: u64, endpoint: u16, deep: bool) -> Result<(), MatError>`

- [ ] **Step 1: Write failing integration tests**

Add to `crates/mat/tests/integration.rs`:

```rust
#[test]
fn diag_node_success_verdict_ok() {
    let store = store_with_node5();
    mat(store.path())
        .args(["diag", "node", "--node", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"verdict\":\"ok\""))
        .stdout(predicate::str::contains("\"checks\""))
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn diag_node_timeout_is_unresolvable_exit0() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args(["diag", "node", "--node", "5"])
        .assert()
        .success() // 診断は落ちない
        .stdout(predicate::str::contains("\"verdict\":\"unresolvable\""));
}

#[test]
fn diag_node_reject_is_device_rejected_exit0() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "reject")
        .args(["diag", "node", "--node", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"verdict\":\"device_rejected\""));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mat --test integration diag_node`
Expected: FAIL (unrecognized subcommand `node` / compile error).

- [ ] **Step 3: Add the CLI variant**

In `crates/mat/src/cli.rs`, inside `enum DiagCommand`, after the `Thread { .. }` variant:

```rust
    /// commissioned ノードが「なぜ制御できないか」を層別チェックして verdict で返す。
    /// 既定は chip-tool 完結。`--deep` で ping6 / mDNS ブラウズも実施し、
    /// link_starved（弱リンク）と fabric_missing（fabric 脱落）まで切り分ける。
    Node {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
        /// エンドポイント番号（既定 0 — 診断は通常 ep0）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 0)]
        endpoint: u16,
        /// 補助プローブ（ping6 / avahi-browse）も実施して深掘りする。
        #[arg(long)]
        deep: bool,
    },
```

In `crates/mat/src/main.rs`, extend the `Command::Diag` match arm:

```rust
        Command::Diag { action } => match action {
            DiagCommand::Thread { node_id, endpoint } => {
                commands::diag::thread(&store_path, *node_id, *endpoint)
            }
            DiagCommand::Node {
                node_id,
                endpoint,
                deep,
            } => commands::diag::node(&store_path, *node_id, *endpoint, *deep),
        },
```

- [ ] **Step 4: Implement `node()` default path**

In `crates/mat/src/commands/diag.rs`, update imports and add functions.

Add to the `use` block:

```rust
use mat_core::diag::{
    derive_verdict, parse_compressed_fabric_id, Checks, OperationalCheck, ThreadCheck,
};
use mat_core::parse::{parse_read_value, parse_struct_list};
```
(`parse_read_value`/`parse_struct_list` may already be imported — keep a single import.)

Add the functions:

```rust
/// `mat diag node` — 到達不能の根本原因を層別チェックで分類する。
pub fn node(store_path: &Path, node_id: u64, endpoint: u16, deep: bool) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    let rec = store.require_node(node_id)?;
    let address = rec.address.clone();
    let chip = ChipTool::new(store.root());

    let mut checks = Checks::default();
    let mut unavailable: Vec<Value> = Vec::new();
    let mut self_cfid: Option<String> = None;

    // operational: 軽量な descriptor read（ep0、全ノード共通）で解決を試す。
    let op_out = chip.run([
        "descriptor".to_string(),
        "read".to_string(),
        "parts-list".to_string(),
        node_id.to_string(),
        "0".to_string(),
    ])?; // ChildNotFound はここで伝播（診断不能）。
    if let Some(cfid) = parse_compressed_fabric_id(&op_out.stderr) {
        self_cfid = Some(cfid);
    }
    let op_kind = classify_failure(&op_out.stdout, &op_out.stderr);
    let resolved = op_kind.is_none() && op_out.success();
    checks.operational = Some(OperationalCheck { resolved, kind: op_kind });

    // thread: neighbor-table の LQI と routing-role（部分結果可）。
    match read_thread_signal(&chip, node_id, endpoint) {
        Ok(tc) => checks.thread = Some(tc),
        Err(e) => unavailable.push(json!({ "check": "thread", "kind": e.kind })),
    }

    if deep {
        deep_probes(&mut checks, &mut unavailable, node_id, address, self_cfid);
    } else {
        unavailable.push(json!({ "check": "ip", "reason": "skipped_no_deep" }));
        unavailable.push(json!({ "check": "mdns", "reason": "skipped_no_deep" }));
    }

    let v = derive_verdict(&checks);

    let mut body = Map::new();
    body.insert("node_id".to_string(), json!(node_id));
    body.insert("endpoint".to_string(), json!(endpoint));
    body.insert(
        "verdict".to_string(),
        serde_json::to_value(v.verdict).unwrap_or(Value::Null),
    );
    body.insert("summary".to_string(), json!(v.summary));
    body.insert(
        "checks".to_string(),
        serde_json::to_value(&checks)
            .map_err(|e| MatError::parse_error(format!("serialize checks: {e}")))?,
    );
    if !unavailable.is_empty() {
        body.insert("unavailable".to_string(), Value::Array(unavailable));
    }
    body.insert("recommendation".to_string(), json!(v.recommendation));
    output::emit(Value::Object(body));
    Ok(())
}

/// neighbor-table（LQI/隣接数）＋ routing-role を読む。neighbor-table が読めなければ Err。
fn read_thread_signal(
    chip: &ChipTool,
    node_id: u64,
    endpoint: u16,
) -> Result<ThreadCheck, MatError> {
    let nt = read_attr(chip, node_id, endpoint, "neighbor-table")?;
    let rows = parse_struct_list(&nt);
    let neighbor_count = rows.len();
    let best_lqi = rows
        .iter()
        .filter_map(|r| r.get("Lqi").and_then(Value::as_u64))
        .map(|v| v as u8)
        .max();
    // routing-role は best-effort（失敗しても thread シグナルは返す）。
    let routing_role = read_attr(chip, node_id, endpoint, "routing-role")
        .ok()
        .and_then(|s| parse_read_value(&s))
        .and_then(|v| v.as_i64());
    Ok(ThreadCheck {
        neighbor_count,
        best_lqi,
        routing_role,
    })
}
```

Add a `deep_probes` stub for now (filled in Task 7), so the default path compiles:

```rust
/// `--deep` の補助プローブ（ping6 / mDNS）。Task 7 で実装。
fn deep_probes(
    _checks: &mut Checks,
    unavailable: &mut Vec<Value>,
    _node_id: u64,
    _address: Option<String>,
    _self_cfid: Option<String>,
) {
    unavailable.push(json!({ "check": "ip", "reason": "deep_not_implemented" }));
    unavailable.push(json!({ "check": "mdns", "reason": "deep_not_implemented" }));
}
```

Note: `read_attr` already exists in `diag.rs` (used by `thread()`). Reuse it.

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p mat --test integration diag_node`
Expected: PASS (3 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/mat/src/cli.rs crates/mat/src/main.rs crates/mat/src/commands/diag.rs crates/mat/tests/integration.rs
git commit -m "feat(diag): mat diag node 既定パス（operational+thread→verdict）"
```

---

### Task 7: `--deep` probes (ping6 + avahi-browse)

**Files:**
- Modify: `crates/mat/src/commands/diag.rs` (replace `deep_probes` stub; add `probe_ping6`, `probe_mdns`)
- Create: `crates/mat/tests/fixtures/fake-ping6.sh`
- Create: `crates/mat/tests/fixtures/fake-avahi-browse.sh`
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: `mat_core::diag::{IpCheck, MdnsCheck, MatterInstance, parse_ping6, parse_avahi_matter}`
- Probe binaries overridable via `MAT_PING6_BIN` / `MAT_AVAHI_BROWSE_BIN` (default `ping6` / `avahi-browse`), mirroring `MAT_CHIP_TOOL_BIN`.

- [ ] **Step 1: Create fake probe fixtures**

`crates/mat/tests/fixtures/fake-ping6.sh`:

```sh
#!/bin/sh
# テスト用ダミー ping6。FAKE_PING_LOSS（既定 50）% でロスを報告する。
loss="${FAKE_PING_LOSS:-50}"
echo "PING target 56 data bytes"
echo "3 packets transmitted, 1 received, ${loss}% packet loss, time 2002ms"
if [ "$loss" != "100" ]; then
  echo "rtt min/avg/max/mdev = 90.000/168.000/200.000/40.000 ms"
fi
exit 0
```

`crates/mat/tests/fixtures/fake-avahi-browse.sh`:

```sh
#!/bin/sh
# テスト用ダミー avahi-browse。FAKE_AVAHI_OUT のパス内容をそのまま吐く。
# 未指定なら「該当ノードの広告なし」を模す（他 fabric の無関係ノードのみ）。
if [ -n "$FAKE_AVAHI_OUT" ] && [ -f "$FAKE_AVAHI_OUT" ]; then
  cat "$FAKE_AVAHI_OUT"
else
  echo "+   eth0 IPv6 0011223344556677-00000000000000FF   _matter._tcp   local"
fi
exit 0
```

Make them executable:

```bash
chmod +x crates/mat/tests/fixtures/fake-ping6.sh crates/mat/tests/fixtures/fake-avahi-browse.sh
```

- [ ] **Step 2: Write the failing integration test**

Add to `crates/mat/tests/integration.rs` (near the other diag tests):

```rust
fn fake_ping6() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fake-ping6.sh")
}
fn fake_avahi() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fake-avahi-browse.sh")
}

#[test]
fn diag_node_deep_link_starved() {
    // operational timeout + ip 生存(50%ロス) + mDNS に node5 広告なし → link_starved。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .env("MAT_PING6_BIN", fake_ping6())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .args(["diag", "node", "--node", "5", "--deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"verdict\":\"link_starved\""))
        .stdout(predicate::str::contains("\"ip\""))
        .stdout(predicate::str::contains("\"loss_pct\":50"));
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p mat --test integration diag_node_deep`
Expected: FAIL (verdict is not `link_starved` — stub marks ip/mdns unavailable).

- [ ] **Step 4: Implement the probes**

In `crates/mat/src/commands/diag.rs`, add imports:

```rust
use std::ffi::OsString;
use std::process::Command as StdCommand;

use mat_core::diag::{parse_avahi_matter, parse_ping6, IpCheck, MdnsCheck};
```

Replace the `deep_probes` stub with:

```rust
/// `--deep` の補助プローブ。ping6（IP生存）と avahi-browse（mDNS広告）を実施。
fn deep_probes(
    checks: &mut Checks,
    unavailable: &mut Vec<Value>,
    node_id: u64,
    address: Option<String>,
    self_cfid: Option<String>,
) {
    // ip: ping6
    match address.as_deref() {
        Some(addr) => match probe_ping6(addr) {
            Ok(stats) => {
                checks.ip = Some(IpCheck {
                    ok: stats.loss_pct < 100,
                    loss_pct: stats.loss_pct,
                    rtt_ms: stats.rtt_ms,
                    method: "ping6",
                })
            }
            Err(e) => unavailable.push(json!({ "check": "ip", "kind": e.kind, "detail": e.detail })),
        },
        None => unavailable.push(json!({ "check": "ip", "reason": "no_address_in_store" })),
    }

    // mdns: avahi-browse
    match probe_mdns() {
        Ok(instances) => {
            let any = instances.iter().any(|i| i.node_id == node_id);
            let self_fabric = self_cfid.as_ref().map(|cfid| {
                instances
                    .iter()
                    .any(|i| i.node_id == node_id && &i.compressed_fabric == cfid)
            });
            checks.mdns = Some(MdnsCheck {
                advertised_self_fabric: self_fabric,
                advertised_any_fabric: any,
            });
        }
        Err(e) => unavailable.push(json!({ "check": "mdns", "kind": e.kind, "detail": e.detail })),
    }
}

/// `ping6 -c3 -W2 <addr>` を実行して統計をパース。バイナリは `MAT_PING6_BIN` で上書き可。
fn probe_ping6(addr: &str) -> Result<mat_core::diag::Ping6Stats, MatError> {
    let bin = std::env::var_os("MAT_PING6_BIN").unwrap_or_else(|| OsString::from("ping6"));
    let out = StdCommand::new(&bin)
        .args(["-c", "3", "-W", "2", addr])
        .output()
        .map_err(|e| MatError::new(ErrorKind::Other, format!("ping6 spawn failed ({bin:?}): {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    tracing::debug!(%text, "ping6 output");
    parse_ping6(&text).ok_or_else(|| MatError::parse_error("ping6 output unparseable"))
}

/// `avahi-browse -rt _matter._tcp` を実行して `_matter._tcp` インスタンスを得る。
/// バイナリは `MAT_AVAHI_BROWSE_BIN` で上書き可。
fn probe_mdns() -> Result<Vec<mat_core::diag::MatterInstance>, MatError> {
    let bin =
        std::env::var_os("MAT_AVAHI_BROWSE_BIN").unwrap_or_else(|| OsString::from("avahi-browse"));
    let out = StdCommand::new(&bin)
        .args(["-rt", "_matter._tcp"])
        .output()
        .map_err(|e| {
            MatError::new(ErrorKind::Other, format!("avahi-browse spawn failed ({bin:?}): {e}"))
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    tracing::debug!(%text, "avahi-browse output");
    Ok(parse_avahi_matter(&text))
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p mat --test integration diag_node_deep`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/mat/src/commands/diag.rs crates/mat/tests/fixtures/fake-ping6.sh crates/mat/tests/fixtures/fake-avahi-browse.sh crates/mat/tests/integration.rs
git commit -m "feat(diag): mat diag node --deep（ping6/avahi-browse プローブ）"
```

---

### Task 8: Docs + full check

**Files:**
- Modify: `README.md` (document `mat diag node` under the diag/usage section)

- [ ] **Step 1: Document the command**

Add a `mat diag node` entry to `README.md` near the existing `mat diag thread` docs. Use dummy values only:

```markdown
#### `mat diag node` — why is a node unreachable?

Classifies why a commissioned node can't be controlled into a single `verdict`
with evidence and a recommended action.

```bash
mat diag node --node 1            # chip-tool only (fast)
mat diag node --node 1 --deep     # also probe ping6 + mDNS (avahi-browse)
```

`verdict` values: `ok`, `ip_unreachable`, `link_starved`, `fabric_missing`,
`not_advertised`, `unresolvable`, `session_failed`, `device_rejected`, `unknown`.
`--deep` is required to distinguish `link_starved` (weak Thread link, SRP not
registered — fabric intact) from `fabric_missing` (removed from our fabric).
The command always exits `0` with a JSON verdict, even when the node is fully
unreachable. `--deep` shells out to `ping6` and `avahi-browse`
(override with `MAT_PING6_BIN` / `MAT_AVAHI_BROWSE_BIN`).
```

- [ ] **Step 2: Run the full CI-equivalent check**

Run: `task check`
Expected: fmt clean, clippy clean (`-D warnings`), all tests pass (existing + new diag tests).

If clippy flags `mat_core::diag::Ping6Stats` / `MatterInstance` fully-qualified paths, add them to the `use mat_core::diag::{...}` import and drop the inline paths.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: README に mat diag node を追記"
```

---

## Self-Review notes (for the implementer)

- **Spec coverage:** Tasks 1–5 implement the data model + parsers + verdict tree (spec §「各チェック」「verdict 導出」). Task 6 wires CLI + default chip-tool path + partial results (§「コマンド面」「部分結果」). Task 7 implements `--deep` probes (§ 方針C). Task 8 covers docs.
- **Operational attribute:** decided = `descriptor read parts-list <node> 0` (universal ep0 attribute; supported by the existing fake-chip-tool fixture).
- **Weak-link thresholds:** `LQI_WEAK=20`, `LOSS_WEAK=30` (§ 未確定 → fixed here; covered by `derive_verdict` tests).
- **Probe binaries:** overridable via `MAT_PING6_BIN` / `MAT_AVAHI_BROWSE_BIN` (testability; mirrors `MAT_CHIP_TOOL_BIN`).
- **Never-fail invariant:** `node()` only returns `Err` for `ChildNotFound` / `StoreMissing` / `NodeNotCommissioned`; all reachability failures become a JSON verdict (exit 0).
