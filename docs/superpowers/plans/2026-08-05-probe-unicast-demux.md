# probe 並行 resolve の unicast demux（監査⑩ 完結）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** probe の並行 resolve を単一共有ソケット + instance 名 demux に置き換え、avahi の unicast 応答が別ノードのソケットに吸われて健全ノードが `reachable:false` になる誤報を根絶する。

**Architecture:** `mat-controller::dnssd` に `resolve_operational_many`（1 ソケットで N instance を fold）を新設し、既存 `resolve_operational` はその 1 要素 wrapper に置き換え（単発と並行を同一エンジン化）。`crates/mat/src/probe.rs` の JoinSet 並行実行を単一 await に置き換える。

**Tech Stack:** Rust workspace（crates: mat-controller / mat）、tokio、Task ランナー（`task check`）、jarvis 実機 E2E。

**Spec:** `docs/superpowers/specs/2026-08-05-probe-unicast-demux-design.md`（真因の pcap 証拠と設計判断はここ）

## Global Constraints

- 作業ブランチは既存の **`fix/tier2-probe-resolve-timeout`**（checkout 済み。新しいブランチを作らない）。バージョンは **1.21.0 のまま**（バンプ済み、変更しない）。
- `resolve_operational` の公開シグネチャは不変（wrapper 化のみ）。単発呼び出し元（mat-native establish / diag / commissioning / matd）は一切変更しない。
- `OPERATIONAL_RESOLVE_TIMEOUT`（8s）・`QUERY_RESEND_INTERVAL`（1s）の値は変更しない。
- `browse` / `resolve_commissionable` / `resolve_all` / `OperationalCache` は無変更。
- probe の出力スキーマ（`reachable` true/false/null）・エラー分類（`StoreMissing` / `Unreachable` / `Other`）・exit code は不変。
- コミット前に `task check`（fmt:check + clippy + test）必須。既知の例外: `group_sender_multicast_loops_back_locally` はこの WSL2 環境で main でも落ちる既存の環境起因失敗（無関係と検証済み）— これ 1 件のみ許容。
- コミット対象はそのタスクで編集したファイルのみ（リポジトリに他の未コミット変更があるが触らない・含めない）。
- コミットメッセージ末尾に次の 2 行のフッターを付ける:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01VCx1L8mz29knxgNHFcAzNC`
- main マージは jarvis 実機 E2E 合格後のみ（Task 3 → Task 4 の順序厳守）。

---

### Task 1: `resolve_operational_many` 新設 + `resolve_operational` wrapper 化（dnssd.rs）

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`
  - `resolve_operational`（576 行目付近）の直前に per-node fold 構造体と `resolve_operational_many` を追加
  - `resolve_operational` 本体を wrapper に置き換え
  - `bind_mdns_socket` doc（124-150 行目付近）の誤前提を修正
  - `#[cfg(test)] mod tests` に unicast responder 足場 + demux テストを追加

**Interfaces:**
- Consumes: 既存ヘルパ `bind_mdns_socket` / `encode_query(id, &[(&str, u16)])` / `parse_message` / `push_aaaa` / `prune_aaaa` / `txt_u32` / `is_link_local` / `operational_instance`、定数 `MDNS_GROUP` / `MDNS_PORT` / `TYPE_SRV` / `TYPE_TXT` / `TYPE_AAAA` / `QUERY_RESEND_INTERVAL`、型 `ResolvedNode` / `DnssdError` / `RData`。
- Produces: `pub async fn resolve_operational_many(scope_id: u32, compressed_fabric_id: &[u8; 8], node_ids: &[u64], timeout: Duration) -> Result<Vec<(u64, Result<ResolvedNode, DnssdError>)>, DnssdError>` — 外側 `Err` = ソケット bind/send の I/O 失敗（全ノード共倒れ）、内側 per-node = `Ok(ResolvedNode)` / `Err(Timeout)`。結果順は `node_ids` の入力順。Task 2 の probe.rs がこれを呼ぶ。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/dnssd.rs` の `mod tests` 内、`spawn_multicast_announcer`（1642 行目付近）の直後に responder 足場を追加:

```rust
    /// avahi（SRP advertising proxy）型 responder の模擬: クエリを受信し、
    /// **問い合わせ元アドレスへの unicast でのみ**応答する（QU 準拠。
    /// 2026-08-05 jarvis pcap で確定した挙動）。served に無い instance には
    /// 応答しない。unicast は同一ポート多重 bind の 1 ソケットにしか配達
    /// されないため、並行 resolve がソケットを共有しない限り他ノード宛の
    /// 答えを黙殺する — という本番機序をそのまま再現する。
    fn spawn_unicast_responder(
        scope_id: u32,
        served: Vec<(String, Vec<u8>)>,
    ) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let sock = bind_mdns_socket(scope_id)?;
        Ok(tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    continue;
                };
                // 簡易クエリ判定: instance の先頭ラベル（16+1+16 hex で一意）が
                // ワイヤに現れていればそのインスタンスへの質問とみなす。
                for (service, msg) in &served {
                    let first_label = service.split('.').next().unwrap_or("");
                    if !first_label.is_empty()
                        && buf[..n]
                            .windows(first_label.len())
                            .any(|w| w == first_label.as_bytes())
                    {
                        let _ = sock.send_to(msg, from).await;
                    }
                }
            }
        }))
    }
```

続けて、`resolve_operational_receives_multicast_only_response` テストの直後にテスト本体を追加:

```rust
    /// 並行 resolve の本丸回帰（監査⑩ 完結）: unicast でしか応答しない
    /// responder（avahi 型）に対し、複数 instance の同時 resolve が単一共有
    /// ソケットで全て解決できること。応答が来ない instance を 1 つ混ぜ、
    /// それだけが Timeout になる（無応答ノードのソケットが他ノードの
    /// unicast を吸うブラックホールの根絶）ことも釘打ちする。
    #[tokio::test]
    async fn resolve_operational_many_demuxes_unicast_only_responses() {
        let cfid: [u8; 8] = 0xAB7D_E088_02E0_CD54u64.to_be_bytes();
        let silent: u64 = 7;
        let served: Vec<(String, Vec<u8>)> = [5u64, 6]
            .iter()
            .map(|&id| {
                let service =
                    format!("{}._matter._tcp.local", operational_instance(&cfid, id));
                let msg = synth_response(
                    &service,
                    &format!("ucastonly-{id}.local"),
                    5540,
                    &["SII=5000"],
                    format!("fd00::{id}").parse().unwrap(),
                );
                (service, msg)
            })
            .collect();
        let mut tried = Vec::new();
        for (name, idx) in multicast_ifaces() {
            let Ok(responder) = spawn_unicast_responder(idx, served.clone()) else {
                tried.push(format!("{name}(idx={idx}): responder bind failed"));
                continue;
            };
            let res = resolve_operational_many(
                idx,
                &cfid,
                &[5, 6, silent],
                Duration::from_millis(1500),
            )
            .await;
            responder.abort();
            match res {
                Ok(results) => {
                    let ok: Vec<u64> = results
                        .iter()
                        .filter(|(_, r)| r.is_ok())
                        .map(|(id, _)| *id)
                        .collect();
                    if ok == vec![5, 6] {
                        for (id, r) in &results {
                            if *id == silent {
                                assert!(
                                    matches!(r, Err(DnssdError::Timeout { .. })),
                                    "silent node must time out: {r:?}"
                                );
                            }
                        }
                        return; // 最初に届いた iface で十分 — PASS。
                    }
                    tried.push(format!("{name}(idx={idx}): resolved only {ok:?}"));
                }
                Err(e) => tried.push(format!("{name}(idx={idx}): {e:?}")),
            }
        }
        panic!(
            "no multicast-capable interface delivered the unicast-only answers \
             to resolve_operational_many; tried: {tried:?}"
        );
    }
```

- [ ] **Step 2: テストが失敗（コンパイルエラー）することを確認**

Run: `cargo test -p mat-controller resolve_operational_many_demuxes 2>&1 | tail -20`
Expected: FAIL — `resolve_operational_many` 未定義のコンパイルエラー。

- [ ] **Step 3: `resolve_operational_many` を実装し、`resolve_operational` を wrapper 化**

`resolve_operational`（576 行目付近）の直前に追加:

```rust
/// [`resolve_operational_many`] の per-node fold 状態。単発 resolver が
/// ローカル変数で持っていたものの持ち上げ。
struct OperationalQuery {
    node_id: u64,
    service: String,
    srv: Option<(u16, String)>,
    txt: Option<Vec<Vec<u8>>>,
    aaaa: Vec<(String, Ipv6Addr)>,
    aaaa_queried: bool,
    resolved: Option<ResolvedNode>,
}

impl OperationalQuery {
    /// SRV + target 一致アドレス ≥1 が揃っていれば完成させる。
    fn try_finish(&mut self) {
        if self.resolved.is_some() {
            return;
        }
        let Some((port, target)) = &self.srv else {
            return;
        };
        let mut addresses: Vec<Ipv6Addr> = Vec::new();
        for (name, addr) in &self.aaaa {
            if name.eq_ignore_ascii_case(target) && !addresses.contains(addr) {
                addresses.push(*addr);
            }
        }
        if addresses.is_empty() {
            return;
        }
        // Non-link-local first (stable sort keeps response order within
        // each class).
        addresses.sort_by_key(is_link_local);
        let strings = self.txt.as_deref().unwrap_or(&[]);
        self.resolved = Some(ResolvedNode {
            port: *port,
            addresses,
            session_idle_interval_ms: txt_u32(strings, "SII"),
            session_active_interval_ms: txt_u32(strings, "SAI"),
        });
    }
}

/// Resolves many operational nodes over ONE shared mDNS socket, folding
/// answers per instance name. Sharing the socket is load-bearing, not an
/// optimization: a responder honoring the QU bit (avahi as the SRP
/// advertising proxy — jarvis pcap, 2026-08-05) answers by unicast, and a
/// unicast datagram to the shared port 5353 is delivered to only ONE bound
/// socket. With per-node sockets (the pre-1.21.0 probe) every answer lands
/// on an arbitrary socket whose per-instance filter silently discards it
/// (audit ⑩'s real mechanism). Queries for unresolved instances are resent
/// every second until `timeout`.
///
/// The outer `Err` is a socket-level I/O failure (bind/send — the whole
/// batch is unresolvable, e.g. an interface without multicast). Per-node
/// results are `Ok(ResolvedNode)` or `Err(Timeout)`, in `node_ids` order.
pub async fn resolve_operational_many(
    scope_id: u32,
    compressed_fabric_id: &[u8; 8],
    node_ids: &[u64],
    timeout: Duration,
) -> Result<Vec<(u64, Result<ResolvedNode, DnssdError>)>, DnssdError> {
    if node_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sock = bind_mdns_socket(scope_id).map_err(DnssdError::Io)?;
    let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));
    let mut queries: Vec<OperationalQuery> = node_ids
        .iter()
        .map(|&node_id| OperationalQuery {
            node_id,
            service: format!(
                "{}._matter._tcp.local",
                operational_instance(compressed_fabric_id, node_id)
            ),
            srv: None,
            txt: None,
            aaaa: Vec::new(),
            aaaa_queried: false,
            resolved: None,
        })
        .collect();

    let deadline = Instant::now() + timeout;
    let mut next_send = Instant::now();
    let mut buf = [0u8; 1500];
    loop {
        if queries.iter().all(|q| q.resolved.is_some()) {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_send {
            for q in queries.iter().filter(|q| q.resolved.is_none()) {
                let msg =
                    encode_query(0, &[(&q.service, TYPE_SRV), (&q.service, TYPE_TXT)]);
                sock.send_to(&msg, dest).await.map_err(DnssdError::Io)?;
                if let Some((_, target)) = &q.srv {
                    let msg = encode_query(0, &[(target.as_str(), TYPE_AAAA)]);
                    sock.send_to(&msg, dest).await.map_err(DnssdError::Io)?;
                }
            }
            next_send = now + QUERY_RESEND_INTERVAL;
        }
        let wait = deadline.min(next_send).saturating_duration_since(now);
        let Ok(recv) = tokio::time::timeout(wait, sock.recv_from(&mut buf)).await else {
            continue;
        };
        let (n, _) = recv.map_err(DnssdError::Io)?;
        // Somebody else's malformed datagram must not abort our resolve.
        let Ok(records) = parse_message(&buf[..n]) else {
            continue;
        };
        for r in records {
            match r.rdata {
                RData::Srv { port, target } => {
                    if let Some(q) = queries.iter_mut().find(|q| {
                        q.resolved.is_none() && r.name.eq_ignore_ascii_case(&q.service)
                    }) {
                        prune_aaaa(&mut q.aaaa, &target);
                        q.srv = Some((port, target));
                    }
                }
                RData::Txt(strings) => {
                    if let Some(q) = queries.iter_mut().find(|q| {
                        q.resolved.is_none() && r.name.eq_ignore_ascii_case(&q.service)
                    }) {
                        q.txt = Some(strings);
                    }
                }
                RData::Aaaa(addr) => {
                    // AAAA は instance 名を持たない — SRV target 既知なら
                    // その名前で、未知なら候補として各未解決ノードに fold。
                    for q in queries.iter_mut().filter(|q| q.resolved.is_none()) {
                        let target = q.srv.as_ref().map(|(_, t)| t.as_str());
                        push_aaaa(&mut q.aaaa, target, r.name.clone(), addr);
                    }
                }
                _ => {}
            }
        }
        let mut followups: Vec<String> = Vec::new();
        for q in queries.iter_mut() {
            q.try_finish();
            if q.resolved.is_none() && !q.aaaa_queried {
                if let Some((_, target)) = &q.srv {
                    followups.push(target.clone());
                    q.aaaa_queried = true;
                }
            }
        }
        for target in followups {
            let msg = encode_query(0, &[(target.as_str(), TYPE_AAAA)]);
            sock.send_to(&msg, dest).await.map_err(DnssdError::Io)?;
        }
    }
    Ok(queries
        .into_iter()
        .map(|q| match q.resolved {
            Some(node) => (q.node_id, Ok(node)),
            None => (
                q.node_id,
                Err(DnssdError::Timeout { instance: q.service }),
            ),
        })
        .collect())
}
```

補足: clippy が戻り値型に `clippy::type_complexity` を出した場合は、`pub type ResolvedManyEntry = (u64, Result<ResolvedNode, DnssdError>);` を `resolve_operational_many` の直前に定義し、戻り値を `Result<Vec<ResolvedManyEntry>, DnssdError>` に置き換えて解消する（意味は同一）。

既存の `resolve_operational`（doc コメントごと 572-661 行目）を次で**置き換える**（本体のループを削除し wrapper 化）:

```rust
/// Resolves one operational node — a thin wrapper over
/// [`resolve_operational_many`] with a single-element batch, so the single
/// and concurrent paths share one engine and cannot diverge (audit ⑩).
pub async fn resolve_operational(
    scope_id: u32,
    compressed_fabric_id: &[u8; 8],
    node_id: u64,
    timeout: Duration,
) -> Result<ResolvedNode, DnssdError> {
    let mut results =
        resolve_operational_many(scope_id, compressed_fabric_id, &[node_id], timeout).await?;
    match results.pop() {
        Some((_, res)) => res,
        None => Err(DnssdError::Malformed(
            "resolve_operational_many returned no result",
        )),
    }
}
```

- [ ] **Step 4: `bind_mdns_socket` doc の誤前提を修正**

124-150 行目付近の doc コメントのうち、次の一文:

```
/// (observed intermittently across all nodes, 2026-07-19). Unicast delivery to
/// the shared port is not guaranteed to reach us, but we rely on the multicast
/// answer this responder sends anyway.
```

を次に置き換える（前段の OTBR multicast 観測の記述は真のまま残す）:

```
/// (observed intermittently across all nodes, 2026-07-19). Unicast delivery
/// to the shared port reaches only ONE bound socket (the most recently
/// bound), and a real responder — avahi acting as the SRP advertising
/// proxy, captured on the wire 2026-08-05 — honors QU and answers by
/// unicast. Concurrent resolvers must therefore share one socket
/// ([`resolve_operational_many`]); per-node sockets silently lose answers
/// to whichever socket bound last (audit ⑩'s real mechanism).
```

- [ ] **Step 5: 新テストが通ることを確認**

Run: `cargo test -p mat-controller resolve_operational_many_demuxes 2>&1 | tail -5`
Expected: PASS（ok 1 passed）。

- [ ] **Step 6: 既存テスト全体 + lint を確認**

Run: `task check`
Expected: fmt:check / clippy 成功、テストは `group_sender_multicast_loops_back_locally`（既知の環境起因）以外すべて成功。特に `resolve_operational_receives_multicast_only_response` が wrapper 経由で成功していること（単発挙動不変の釘）。

- [ ] **Step 7: コミット**

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "fix(mat-controller): 並行 resolve を単一共有ソケット + instance demux 化（監査⑩ 完結）"
```

（フッター 2 行を忘れずに付ける — Global Constraints 参照。）

---

### Task 2: probe.rs の JoinSet を `resolve_operational_many` に置き換え

**Files:**
- Modify: `crates/mat/src/probe.rs`

**Interfaces:**
- Consumes: Task 1 の `dnssd::resolve_operational_many(scope_id, &cfid, node_ids, timeout) -> Result<Vec<(u64, Result<ResolvedNode, DnssdError>)>, DnssdError>`（結果は `node_ids` 順）、既存 `dnssd::OPERATIONAL_RESOLVE_TIMEOUT`。
- Produces: `crate::probe::mdns` の外部挙動（シグネチャ・スキーマ・エラー分類）は不変 — Task 3 の E2E が検証する。

- [ ] **Step 1: resolve ブロックを置き換える**

`resolve_ledger_nodes` 内の、tokio runtime 構築（`let rt = ...`）**より後**の
`let results: Vec<(u64, Result<dnssd::ResolvedNode, dnssd::DnssdError>)> = rt.block_on(async { ... });`（JoinSet ブロック全体）と、その後の `all_io_err` 判定ブロック（`// 全ノードが Io エラーの場合は...` コメントから `}` まで）を、次で**まとめて置き換える**:

```rust
    // 単一共有ソケットで全ノードを resolve する（監査⑩: ノード毎ソケット
    // だと avahi の unicast 応答が最新 bind の 1 ソケットに吸われ、他ノード
    // の resolver が答えを黙殺する）。外側 Err はソケット bind/send の I/O
    // 失敗 = 全ノード共倒れの環境問題（例: MAT_IFACE=lo では multicast send
    // 自体が失敗する。フォールバック先は無い — Task 11 で撤去済み）。
    let results = rt
        .block_on(dnssd::resolve_operational_many(
            scope_id,
            &cfid,
            p.node_ids,
            dnssd::OPERATIONAL_RESOLVE_TIMEOUT,
        ))
        .map_err(|e| {
            MatError::new(
                ErrorKind::Unreachable,
                format!("native mDNS probe failed on {}: {e}", p.iface),
            )
        })?;
```

これ以降の `let cfid_hex = ...` からのフォールドループ（`Ok(node) => ... / Err(Timeout) => debug / Err(e) => debug`）と `tracing::info!` はそのまま残す（per-node の型が同じなので無修正で通る）。

- [ ] **Step 2: doc コメントを更新**

(a) モジュール doc（ファイル先頭 `//!` ブロック）の Task 1（窓統一）で追記した段落の直後に追加:

```rust
//!
//! resolve はノード毎ソケットの並行実行ではなく **単一共有ソケット**
//! （`dnssd::resolve_operational_many`）で行う（監査⑩ 完結、1.21.0）。
//! avahi（SRP advertising proxy）は QU に unicast で応答し、unicast は
//! 同一ポート多重 bind の 1 ソケットにしか配達されないため、ノード毎
//! ソケットでは他ノード宛の答えが黙殺され健全ノードを誤報していた。
```

(b) `mdns` / `resolve_ledger_nodes` の関数 doc にある「全ノード I/O エラーのみ `Unreachable`」の記述（2 箇所）を「ソケット bind/send の I/O 失敗（全ノード共倒れ）は `Unreachable`」に改める。

- [ ] **Step 3: `task check` で全テスト・lint 通過を確認**

Run: `task check`
Expected: fmt:check / clippy 成功、テストは既知の環境起因 1 件以外すべて成功（probe.rs の既存 2 テスト — `store_missing` 分類・空台帳 short-circuit — は無修正で通る）。

- [ ] **Step 4: コミット**

```bash
git add crates/mat/src/probe.rs
git commit -m "fix(mat): probe の並行 resolve を共有ソケット demux に置き換え（監査⑩ 完結）"
```

（フッター 2 行を忘れずに付ける — Global Constraints 参照。）

---

### Task 3: jarvis 実機 E2E 再実行（マージ前必須）

**Files:** なし（検証のみ。リポジトリ変更はない）

**Interfaces:**
- Consumes: Task 1-2 の挙動（共有ソケット demux + 8s 窓）、バージョン表示 1.21.0。
- Produces: E2E 合否（合格が Task 4 マージの前提条件）。

前提知識:
- jarvis = aarch64 実機。ssh ホスト名 `jarvis`、本番バイナリ `~/.local/bin/mat`（1.20.0、置換禁止）。検証は `~/.local/bin/mat.new` を上書き転送して行う。
- 非対話 ssh の直経路は `MAT_FABRIC_INDEX=2` 前置き必須。
- `discover --probe` は読み取り専用（CASE 確立なし・KVS 書込なし）で本番 matd に無害。
- 台帳 15 ノード。前回 E2E のベースライン: established = `{5..14,16..19}`（14 ノード）、node15 のみ down（慢性 mDNS 未解決）。旧 1.20.0 probe は `{16,17,18,19}` のみ true / 所要 6.0s。修正前の 1.21.0（窓統一のみ）は同集合 / 11.0s。

- [ ] **Step 1: aarch64 バイナリを再ビルドして転送**

```bash
task dist:arm64
scp dist/arm64/mat jarvis:~/.local/bin/mat.new
ssh jarvis 'chmod +x ~/.local/bin/mat.new && ~/.local/bin/mat.new --version'
```

Expected: `mat 1.21.0`。

- [ ] **Step 2: 現況確認 + 新バイナリで probe 実行（2 回）**

```bash
ssh jarvis 'matd status 2>/dev/null || ~/.local/bin/mat matd status' > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/matd-status-2.json 2>&1
ssh jarvis 'time MAT_FABRIC_INDEX=2 ~/.local/bin/mat.new discover --probe' > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/probe-fix.json 2> /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/probe-fix.time
ssh jarvis 'time MAT_FABRIC_INDEX=2 ~/.local/bin/mat.new discover --probe' > /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/probe-fix-rerun.json 2> /tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/dfc97fa7-139e-44a7-912f-abf851ae4080/scratchpad/probe-fix-rerun.time
```

（`matd status` はどちらの形でも取れなければ省略可 — 直前の established 集合はベースラインで既知。）

- [ ] **Step 3: 合否判定**

判定基準（すべて満たすこと）:
1. **`matd status` の established ノードがすべて `reachable:true`**（今回の本丸。前回ベースラインなら 5–14 と 16–19 の 14 ノード。matd 側の一時的な down がある場合はそのノードのみ除外して判断）。
2. node15（慢性 mDNS 未解決・avahi にも広告なし）は `reachable:false` のまま。
3. 所要: 全ノード即応なら数秒以内（未解決が node15 だけなら 8 秒強で頭打ち = 窓が効いている）。2 回の実行で reachable 集合が一致（再現性）。
4. stderr に想定外の WARN/ERROR なし。
5. JSON スキーマ不変（`timestamp` / `node_id` / `reachable` / `state` / 到達ノードのみ `address`）。
6. 単発経路の無回帰: `ssh jarvis 'MAT_FABRIC_INDEX=2 ~/.local/bin/mat.new diag node -n 8 --deep'` が exit 0 で `"resolved":true`。

広範な `reachable:false`（avahi 広告消失を伴う）が出た場合はインフラ障害（SRP stopped 再発）を疑い、`ssh jarvis 'avahi-browse -rpt _matter._tcp'` で広告有無を確認してから判定する。インフラ障害なら中断してユーザーに報告（E2E 自体が無意味）。

- [ ] **Step 4: 後始末（`.new` は残置）**

`mat.new` は昇格待ちとしてそのまま残す。scratchpad の記録は残してよい。

---

### Task 4: main マージ + push（E2E 合格後のみ）

**Files:** なし(git 操作のみ)

**Interfaces:**
- Consumes: Task 3 の合格判定。

- [ ] **Step 1: main へ no-ff マージ**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat
git checkout main
git merge --no-ff fix/tier2-probe-resolve-timeout -m "Merge fix/tier2-probe-resolve-timeout: 1.21.0 — probe resolve 窓統一 + 並行 resolve unicast demux（安定性監査 Tier 2 ⑩ 完結）"
```

- [ ] **Step 2: push（HTTPS 経由 — ssh 鍵は 1Password agent が拒否することがある）**

```bash
git push https://github.com/nogu3/mat.git main
git fetch https://github.com/nogu3/mat.git main && git update-ref refs/remotes/origin/main FETCH_HEAD
```

Expected: origin/main が新 merge commit と一致。

- [ ] **Step 3: ブランチ削除**

```bash
git branch -d fix/tier2-probe-resolve-timeout
```
