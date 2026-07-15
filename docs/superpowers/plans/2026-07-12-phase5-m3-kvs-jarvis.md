# Phase 5 M3: KVS 堅牢化 + jarvis 実機相乗り Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** chip-tool KVS からの相乗りを index≠id の fabric でも正しくし（node/fabric id を fabric テーブルの NOC から取得）、自前 mDNS で実機を解決し、jarvis の実機 Nanoleaf に CASE + onoff/色変更を通す。

**Architecture:** すべて `mat-controller` crate 内（+ E2E ハーネス）。`kvs` の id ソース差し替え、新モジュール `dnssd`（one-shot mDNS リゾルバ、依存追加なし）、`im` の colorcontrol 定数 + fields エンコーダ、`tests/live_jarvis.rs` + `scripts/e2e-m3.sh` + Taskfile `e2e:m3`。mat / matd は無変更。

**Tech Stack:** Rust / tokio（既存依存のみ。新規依存なし）。spec は `docs/superpowers/specs/2026-07-12-phase5-m3-kvs-jarvis-design.md`。

## Global Constraints

- 作業ディレクトリは worktree `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/phase5-m1-controller-core`、ブランチ **`matter-controller`**。**main には絶対にコミット/マージしない**（ユーザー決定 2026-07-10）。各 dispatch で `git branch --show-current` が `matter-controller` であることを確認してから編集すること。
- repo は public。**実 IP・実 node_id・実証明書・ホスト名の実値をコミットしない**（RFC 5737 / ダミー値のみ。E2E スクリプトは実値を必ず env で受け取る）。
- コミット前に `task check`（fmt:check + clippy -D warnings + test）を通す。
- ライブテストは `#[ignore]` とし CI では走らせない。
- プロトコルコードは `mat-controller` crate のみに置く（CLAUDE.md design rule 1）。
- 秘密鍵・IPK を持つ構造体に `#[derive(Debug)]` を付けない（既存の手書き REDACTED Debug の流儀を守る）。
- コミットメッセージ末尾: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`

---

### Task 1: kvs — node/fabric id を fabric テーブルの NOC（`f/<idx>/n`）から読む

M2b 最終レビュー繰り越し [Important #1]（fabric_id が KVS index 流用）と [Minor #3]（`LocalNodeId` の黙ったフォールバック）を、id ソースを chip-tool 自身の永続 NOC に一本化することで両方閉じる。`LocalNodeId` 読み出しと `DEFAULT_CONTROLLER_NODE_ID` は削除。

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`

**Interfaces:**
- Consumes: `crate::cert::MatterCert::parse(&[u8]) -> Result<MatterCert, CertError>`, `MatterCert::node_id() -> Option<u64>`, `MatterCert::fabric_id() -> Option<u64>`（M2 実装済み）
- Produces: `kvs::read_self_issue_materials(alpha_ini: &Path, main_ini: &Path, fabric_index: u8, issuer_index: u8) -> Result<SelfIssueMaterials, KvsError>`（シグネチャ不変。`SelfIssueMaterials.node_id` / `.fabric_id` の意味が「`f/<idx>/n` の NOC subject 由来」に変わる）。新エラー variant `KvsError::BadNoc { fabric_index: u8, reason: &'static str }`。`DEFAULT_CONTROLLER_NODE_ID` は削除（後続タスクは参照しないこと）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/kvs.rs` の `mod tests` に追加・置換。まずヘルパ（fixture の NOC は chip SDK テスト証明書で、node id / fabric id とも table index と異なる値を持つ）:

```rust
    /// node01_01 フィクスチャ（chip SDK テスト証明書）とその subject の実 id。
    /// 期待値はパーサ経由で取るが、cert パース自体は cert.rs 側でフィクスチャ
    /// 検証済みなので、ここでは「kvs がその値を配線しているか」だけを見る。
    fn noc_fixture() -> (&'static [u8], u64, u64) {
        let bytes: &[u8] = include_bytes!("../tests/fixtures/node01_01_chip.bin");
        let cert = crate::cert::MatterCert::parse(bytes).unwrap();
        (bytes, cert.node_id().unwrap(), cert.fabric_id().unwrap())
    }
```

既存 `reads_self_issue_materials` を次で置換（main ini に `f/1/n` を追加し、node/fabric id の期待値を NOC 由来に変更):

```rust
    #[test]
    fn reads_self_issue_materials() {
        // root 鍵は生 97B（TLV ラップ無し）
        let mut root_key = Vec::with_capacity(97);
        root_key.extend_from_slice(&[0xAA; 65]); // pub
        root_key.extend_from_slice(&[0xBB; 32]); // priv
        let ks = keyset_blob(&[0xCC; 16]);
        let (noc, node_id, fabric_id) = noc_fixture();

        let alpha = write_named_ini("alpha", &[("ExampleOpCredsCAKey0", &root_key)]);
        let main = write_named_ini(
            "main",
            &[
                // root cert (TLV form) と自 NOC は fabric table に入っている
                ("f/1/r", b"rcac-tlv-bytes"),
                ("f/1/n", noc),
                ("f/1/k/0", &ks),
            ],
        );

        let m = read_self_issue_materials(&alpha, &main, 1, 0).unwrap();
        assert_eq!(m.rcac, b"rcac-tlv-bytes");
        assert_eq!(m.root_private_key, [0xBB; 32]);
        assert_eq!(m.ipk_operational, [0xCC; 16]);
        assert_eq!(m.node_id, node_id);
        assert_eq!(m.fabric_id, fabric_id);
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }
```

既存 `local_node_id_overrides_default` を**削除**し、代わりに次の 3 テストを追加:

```rust
    #[test]
    fn ids_come_from_noc_subject_not_table_index() {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let ks = keyset_blob(&[0xCC; 16]);
        let (noc, node_id, fabric_id) = noc_fixture();
        // fabric テーブルの index 9 に置く — subject の id は 9 ではない
        let alpha = write_named_ini("alpha-idx", &[("ExampleOpCredsCAKey0", &root_key)]);
        let main = write_named_ini(
            "main-idx",
            &[("f/9/r", b"r"), ("f/9/n", noc), ("f/9/k/0", &ks)],
        );
        let m = read_self_issue_materials(&alpha, &main, 9, 0).unwrap();
        assert_ne!(fabric_id, 9, "fixture の fabric id が index と偶然一致すると本テストは無意味");
        assert_eq!(m.fabric_id, fabric_id);
        assert_eq!(m.node_id, node_id);
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }

    #[test]
    fn missing_noc_is_key_missing() {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let ks = keyset_blob(&[0xCC; 16]);
        let alpha = write_named_ini("alpha-non", &[("ExampleOpCredsCAKey0", &root_key)]);
        let main = write_named_ini("main-non", &[("f/1/r", b"r"), ("f/1/k/0", &ks)]);
        let err = read_self_issue_materials(&alpha, &main, 1, 0).unwrap_err();
        assert!(matches!(err, KvsError::KeyMissing(k) if k == "f/1/n"));
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }

    #[test]
    fn garbage_noc_is_bad_noc_naming_the_key() {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let ks = keyset_blob(&[0xCC; 16]);
        let alpha = write_named_ini("alpha-bad", &[("ExampleOpCredsCAKey0", &root_key)]);
        let main = write_named_ini(
            "main-bad",
            &[("f/1/r", b"r"), ("f/1/n", b"not a matter cert"), ("f/1/k/0", &ks)],
        );
        let err = read_self_issue_materials(&alpha, &main, 1, 0).unwrap_err();
        assert!(matches!(err, KvsError::BadNoc { fabric_index: 1, .. }));
        assert!(
            err.to_string().contains("f/1/n"),
            "エラーは実キー名を名指しすること: {err}"
        );
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller kvs`
Expected: `BadNoc` variant が無いためコンパイルエラー（それ自体が失敗確認）。

- [ ] **Step 3: 実装**

`KvsError` に variant 追加（`BadKeyset` の後）:

```rust
    BadNoc {
        fabric_index: u8,
        reason: &'static str,
    },
```

`Display` に腕を追加（`BadKeyset` の腕の後）:

```rust
            KvsError::BadNoc {
                fabric_index,
                reason,
            } => {
                write!(f, "kvs key \"f/{fabric_index}/n\": bad noc: {reason}")
            }
```

`SelfIssueMaterials` の doc コメントを更新:

```rust
/// CA materials chip-tool persists, needed to self-issue a NOC without going
/// through chip-tool. `root_private_key` comes from the *alpha* KVS (the CA's
/// own key pair); `rcac` (root cert, Matter-TLV form — its parsed public key is
/// the root public key), `ipk_operational`, and `node_id`/`fabric_id` (both
/// from the subject of chip-tool's own operational NOC at `f/<idx>/n` — the
/// identity the device ACLs actually admit; the KVS index is just a table
/// slot) come from the *main* KVS.
```

`DEFAULT_CONTROLLER_NODE_ID` 定数（doc コメント込み）を**削除**。先に `grep -rn DEFAULT_CONTROLLER_NODE_ID crates/` で参照が kvs.rs 内のみであることを確認（他にあればその参照も削除）。

`read_self_issue_materials` の `LocalNodeId` ブロック（`let node_id = match decode_b64(main_sec, "LocalNodeId")? { ... };`）と `fabric_id: u64::from(fabric_index)` を次で置換:

```rust
    // node id / fabric id come from the subject of chip-tool's own
    // operational NOC in the fabric table (`f/<idx>/n`, Matter-TLV): the
    // device ACLs admit exactly the identity in that cert, and its subject
    // carries the *operational* fabric id — the KVS index is just a table
    // slot and differs from the fabric id on any non-alpha fabric.
    let noc_key = format!("f/{fabric_index}/n");
    let noc_tlv = decode_b64(main_sec, &noc_key)?.ok_or(KvsError::KeyMissing(noc_key))?;
    let noc = crate::cert::MatterCert::parse(&noc_tlv).map_err(|_| KvsError::BadNoc {
        fabric_index,
        reason: "unparseable matter-tlv certificate",
    })?;
    let node_id = noc.node_id().ok_or(KvsError::BadNoc {
        fabric_index,
        reason: "subject missing node id (tag 17)",
    })?;
    let fabric_id = noc.fabric_id().ok_or(KvsError::BadNoc {
        fabric_index,
        reason: "subject missing fabric id (tag 21)",
    })?;

    Ok(SelfIssueMaterials {
        rcac,
        root_private_key,
        ipk_operational,
        node_id,
        fabric_id,
    })
```

`read_self_issue_materials` の doc コメントの「plus the IPK and controller node id」を「plus the IPK and the node/fabric id (from the fabric table's own NOC)」に更新。

モジュール先頭 doc も更新（M2b Minor 持ち越し「self-issue reader 未記載」の回収）:

```rust
//! Minimal reader for chip-tool's Linux ini KVS (connectedhomeip v1.4.2.0).
//!
//! Two readers: [`read_fabric_credentials`] (a full credential set including
//! the operational key, keys `f/<index>/{r,i,n,o}` and `f/<index>/k/0` —
//! chip-tool does not persist its *own* op key, so this path serves fixtures
//! and non-chip-tool stores), and [`read_self_issue_materials`] (what
//! self-issuing our own NOC needs: the root CA key from the alpha ini; the
//! root cert, our node/fabric id, and the IPK from the main ini fabric
//! table). Format facts (verified against SDK v1.4.2.0): `[Default]`
//! section, base64 values; the keyset stores the already derived
//! *operational* group key, not the epoch key.
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller kvs`
Expected: PASS（kvs テスト全件）

- [ ] **Step 5: 全体チェック**

Run: `task check`
Expected: 全通過（`live_case_im.rs` は `materials.node_id` 等フィールド参照のみなのでコンパイル不変。もし `DEFAULT_CONTROLLER_NODE_ID` 参照が残っていればここで検出される）

- [ ] **Step 6: コミット**

```bash
git add crates/mat-controller/src/kvs.rs
git commit -m "fix(mat-controller): node/fabric id from the fabric-table NOC, not LocalNodeId/index

M2b final-review carry-overs: fabric_id was the KVS table index (only
coincidentally correct on alpha where index 1 == id 1), and a
present-but-wrong-length LocalNodeId silently fell back to 112233. Both
resolved by reading the ids from the subject of chip-tool's own persisted
operational NOC (f/<idx>/n) — the identity device ACLs actually admit.
LocalNodeId reading and DEFAULT_CONTROLLER_NODE_ID are gone; a missing or
unparseable NOC is now a hard error naming the key.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: dnssd — one-shot mDNS リゾルバ（SRV/TXT/AAAA + SII→MRP 接続）

新モジュール。legacy unicast クエリ（RFC 6762 §6.7、source port ≠ 5353）で
`<instance>._matter._tcp.local` の SRV+TXT を引き、target の AAAA を（additional に
無ければ追撃クエリで）集める。TXT の SII を `MrpConfig::initial_interval` に接続。

**Files:**
- Create: `crates/mat-controller/src/dnssd.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod dnssd;` を `pub mod crypto;` の直後に挿入 — アルファベット順維持）

**Interfaces:**
- Consumes: `crate::exchange::MrpConfig { initial_interval: Duration, max_retries: u32, backoff: f64 }`（`Default` 実装あり）
- Produces:
  - `pub fn operational_instance(compressed_fabric_id: &[u8; 8], node_id: u64) -> String`
  - `pub fn iface_index(name: &str) -> std::io::Result<u32>`
  - `pub struct ResolvedNode { pub port: u16, pub addresses: Vec<Ipv6Addr>, pub session_idle_interval_ms: Option<u32>, pub session_active_interval_ms: Option<u32> }`
  - `impl ResolvedNode { pub fn mrp_config(&self) -> MrpConfig; pub fn socket_addrs(&self, scope_id: u32) -> Vec<SocketAddr> }`
  - `pub async fn resolve_operational(scope_id: u32, compressed_fabric_id: &[u8; 8], node_id: u64, timeout: Duration) -> Result<ResolvedNode, DnssdError>`
  - `pub enum DnssdError { Io(std::io::Error), Timeout { instance: String }, Malformed(&'static str) }`

- [ ] **Step 1: 失敗するテストを含むモジュール全体を書く**

`crates/mat-controller/src/dnssd.rs` を以下の内容で新規作成（テスト込み。実装は本 Step で全量書く — DNS codec はテストと不可分なため Step を分けない）:

```rust
//! Minimal one-shot mDNS/DNS-SD resolver for Matter operational services
//! (Matter spec §4.3; RFC 6762 legacy unicast queries; RFC 2782 SRV).
//!
//! Scope: resolve one `<CompressedFabricId>-<NodeId>._matter._tcp.local`
//! instance to IPv6 addresses + port + MRP intervals (TXT `SII`/`SAI`).
//! No browsing, no advertising, no cache: send a legacy unicast query
//! (source port ≠ 5353, so responders reply straight back to us), fold
//! responses until SRV + at least one AAAA for its target are in hand.
//! TXT is folded when it arrives in the same responses but is not waited
//! for — MRP falls back to the spec default interval without it.

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::exchange::MrpConfig;

const MDNS_GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_PORT: u16 = 5353;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 0x0001;
/// Matter spec §4.12.8: SESSION_IDLE_INTERVAL default and ceiling (ms).
const MRP_DEFAULT_IDLE_MS: u32 = 500;
const MRP_MAX_INTERVAL_MS: u32 = 3_600_000;
const QUERY_RESEND_INTERVAL: Duration = Duration::from_secs(1);

/// Resolver error. `Timeout` names the instance so the operator can
/// cross-check advertising with `avahi-browse -rtp _matter._tcp`.
#[derive(Debug)]
pub enum DnssdError {
    Io(std::io::Error),
    Timeout { instance: String },
    Malformed(&'static str),
}

impl std::fmt::Display for DnssdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DnssdError::Io(e) => write!(f, "dnssd: io error: {e}"),
            DnssdError::Timeout { instance } => {
                write!(f, "dnssd: no SRV+AAAA answer for \"{instance}\" within the deadline")
            }
            DnssdError::Malformed(m) => write!(f, "dnssd: malformed dns message: {m}"),
        }
    }
}

impl std::error::Error for DnssdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DnssdError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Operational instance name (spec §4.3.1): 16 uppercase hex digits each of
/// the compressed fabric id and the node id, joined by `-`.
pub fn operational_instance(compressed_fabric_id: &[u8; 8], node_id: u64) -> String {
    format!(
        "{:016X}-{:016X}",
        u64::from_be_bytes(*compressed_fabric_id),
        node_id
    )
}

/// Interface index for `name`, from `/sys/class/net/<name>/ifindex`
/// (Linux-only, which is every target mat supports).
pub fn iface_index(name: &str) -> std::io::Result<u32> {
    let text = std::fs::read_to_string(format!("/sys/class/net/{name}/ifindex"))?;
    text.trim()
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad ifindex"))
}

fn is_link_local(a: &Ipv6Addr) -> bool {
    (a.segments()[0] & 0xffc0) == 0xfe80
}

/// One resolved operational node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNode {
    pub port: u16,
    /// Non-link-local addresses sorted first (usable without a scope id).
    pub addresses: Vec<Ipv6Addr>,
    pub session_idle_interval_ms: Option<u32>,
    pub session_active_interval_ms: Option<u32>,
}

impl ResolvedNode {
    /// MRP config seeded from the device's advertised session *idle*
    /// interval (the session is idle until CASE completes), clamped to the
    /// spec ceiling; without TXT it falls back to the Matter default 500 ms.
    pub fn mrp_config(&self) -> MrpConfig {
        let ms = self
            .session_idle_interval_ms
            .unwrap_or(MRP_DEFAULT_IDLE_MS)
            .clamp(1, MRP_MAX_INTERVAL_MS);
        MrpConfig {
            initial_interval: Duration::from_millis(u64::from(ms)),
            ..MrpConfig::default()
        }
    }

    /// Socket addresses to try, in order. Link-local addresses need
    /// `scope_id`; global/ULA addresses take none.
    pub fn socket_addrs(&self, scope_id: u32) -> Vec<SocketAddr> {
        self.addresses
            .iter()
            .map(|a| {
                let scope = if is_link_local(a) { scope_id } else { 0 };
                SocketAddr::V6(SocketAddrV6::new(*a, self.port, 0, scope))
            })
            .collect()
    }
}

/// Appends `name` in DNS label form (RFC 1035 §3.1). Our names are fixed
/// service/host names, so an oversized label is a caller bug.
fn push_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.') {
        debug_assert!(!label.is_empty() && label.len() <= 63, "bad dns label");
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

/// One DNS query message (standard query, class IN) with the given
/// (name, qtype) questions. mDNS conventionally uses id 0.
fn encode_query(id: u16, questions: &[(&str, u16)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0, 0]); // flags
    out.extend_from_slice(&(questions.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // an/ns/ar counts
    for (name, qtype) in questions {
        push_name(&mut out, name);
        out.extend_from_slice(&qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
    }
    out
}

/// Reads a possibly-compressed name starting at `pos`. Returns the dotted
/// name and the offset just past the name *at its original location*.
/// Pointer chains are hop-bounded to reject compression loops.
fn read_name(buf: &[u8], mut pos: usize) -> Result<(String, usize), DnssdError> {
    let mut out = String::new();
    let mut next = None; // fixed at the first pointer
    let mut hops = 0u8;
    loop {
        let &len = buf.get(pos).ok_or(DnssdError::Malformed("name past end"))?;
        if len == 0 {
            return Ok((out, next.unwrap_or(pos + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let &lo = buf
                .get(pos + 1)
                .ok_or(DnssdError::Malformed("pointer past end"))?;
            if next.is_none() {
                next = Some(pos + 2);
            }
            pos = usize::from(len & 0x3F) << 8 | usize::from(lo);
            hops += 1;
            if hops > 32 {
                return Err(DnssdError::Malformed("compression pointer loop"));
            }
            continue;
        }
        if len & 0xC0 != 0 {
            return Err(DnssdError::Malformed("reserved label type"));
        }
        let label = buf
            .get(pos + 1..pos + 1 + usize::from(len))
            .ok_or(DnssdError::Malformed("label past end"))?;
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&String::from_utf8_lossy(label));
        pos += 1 + usize::from(len);
    }
}

enum RData {
    Srv { port: u16, target: String },
    Txt(Vec<Vec<u8>>),
    Aaaa(Ipv6Addr),
    Other,
}

struct Record {
    name: String,
    rdata: RData,
}

fn be16(buf: &[u8], pos: usize) -> Result<u16, DnssdError> {
    let b = buf
        .get(pos..pos + 2)
        .ok_or(DnssdError::Malformed("truncated"))?;
    Ok(u16::from_be_bytes(b.try_into().expect("2 bytes")))
}

/// Parses the answer + authority + additional records of one DNS message.
/// Record classes are ignored (mDNS is IN-only; the cache-flush bit lives in
/// the class field and must not break parsing).
fn parse_message(buf: &[u8]) -> Result<Vec<Record>, DnssdError> {
    if buf.len() < 12 {
        return Err(DnssdError::Malformed("short header"));
    }
    let qd = be16(buf, 4)?;
    let total =
        usize::from(be16(buf, 6)?) + usize::from(be16(buf, 8)?) + usize::from(be16(buf, 10)?);
    let mut pos = 12usize;
    for _ in 0..qd {
        let (_, p) = read_name(buf, pos)?;
        pos = p + 4; // qtype + qclass
        if pos > buf.len() {
            return Err(DnssdError::Malformed("truncated question"));
        }
    }
    let mut records = Vec::with_capacity(total);
    for _ in 0..total {
        let (name, p) = read_name(buf, pos)?;
        let rtype = be16(buf, p)?;
        let rdlen = usize::from(be16(buf, p + 8)?);
        let rdata_pos = p + 10;
        let rdata = buf
            .get(rdata_pos..rdata_pos + rdlen)
            .ok_or(DnssdError::Malformed("rdata past end"))?;
        let rdata = match rtype {
            TYPE_SRV => {
                if rdata.len() < 7 {
                    return Err(DnssdError::Malformed("short srv rdata"));
                }
                let port = u16::from_be_bytes([rdata[4], rdata[5]]);
                // The target may use compression relative to the whole
                // message, so read it at its absolute offset.
                let (target, _) = read_name(buf, rdata_pos + 6)?;
                RData::Srv { port, target }
            }
            TYPE_TXT => {
                let mut strings = Vec::new();
                let mut i = 0usize;
                while i < rdata.len() {
                    let n = usize::from(rdata[i]);
                    let s = rdata
                        .get(i + 1..i + 1 + n)
                        .ok_or(DnssdError::Malformed("txt string past end"))?;
                    strings.push(s.to_vec());
                    i += 1 + n;
                }
                RData::Txt(strings)
            }
            TYPE_AAAA => {
                let bytes: [u8; 16] = rdata
                    .try_into()
                    .map_err(|_| DnssdError::Malformed("aaaa rdata not 16 bytes"))?;
                RData::Aaaa(Ipv6Addr::from(bytes))
            }
            _ => RData::Other,
        };
        records.push(Record { name, rdata });
        pos = rdata_pos + rdlen;
    }
    Ok(records)
}

/// Extracts a decimal `key=value` (case-insensitive key) from TXT strings.
fn txt_u32(strings: &[Vec<u8>], key: &str) -> Option<u32> {
    for s in strings {
        let Ok(s) = std::str::from_utf8(s) else {
            continue;
        };
        let Some((k, v)) = s.split_once('=') else {
            continue;
        };
        if k.eq_ignore_ascii_case(key) {
            return v.parse().ok();
        }
    }
    None
}

/// Resolves one operational node via a one-shot legacy unicast mDNS query:
/// SRV + TXT for the instance in one message, then AAAA for the SRV target
/// if no bundled additional record carried it. The query is resent every
/// second until `timeout` elapses.
pub async fn resolve_operational(
    scope_id: u32,
    compressed_fabric_id: &[u8; 8],
    node_id: u64,
    timeout: Duration,
) -> Result<ResolvedNode, DnssdError> {
    let instance = operational_instance(compressed_fabric_id, node_id);
    let service = format!("{instance}._matter._tcp.local");
    let sock = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
        .await
        .map_err(DnssdError::Io)?;
    let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));

    let mut srv: Option<(u16, String)> = None;
    let mut txt: Option<Vec<Vec<u8>>> = None;
    let mut aaaa: Vec<(String, Ipv6Addr)> = Vec::new();
    let mut aaaa_queried = false;

    let deadline = Instant::now() + timeout;
    let mut next_send = Instant::now();
    let mut buf = [0u8; 1500];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_send {
            let q = encode_query(0, &[(&service, TYPE_SRV), (&service, TYPE_TXT)]);
            sock.send_to(&q, dest).await.map_err(DnssdError::Io)?;
            if let Some((_, target)) = &srv {
                let q = encode_query(0, &[(target.as_str(), TYPE_AAAA)]);
                sock.send_to(&q, dest).await.map_err(DnssdError::Io)?;
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
                RData::Srv { port, target } if r.name.eq_ignore_ascii_case(&service) => {
                    srv = Some((port, target));
                }
                RData::Txt(strings) if r.name.eq_ignore_ascii_case(&service) => {
                    txt = Some(strings);
                }
                RData::Aaaa(addr) => aaaa.push((r.name, addr)),
                _ => {}
            }
        }
        if let Some((port, target)) = &srv {
            let mut addresses: Vec<Ipv6Addr> = Vec::new();
            for (name, addr) in &aaaa {
                if name.eq_ignore_ascii_case(target) && !addresses.contains(addr) {
                    addresses.push(*addr);
                }
            }
            if !addresses.is_empty() {
                // Non-link-local first (stable sort keeps response order
                // within each class).
                addresses.sort_by_key(is_link_local);
                let strings = txt.as_deref().unwrap_or(&[]);
                return Ok(ResolvedNode {
                    port: *port,
                    addresses,
                    session_idle_interval_ms: txt_u32(strings, "SII"),
                    session_active_interval_ms: txt_u32(strings, "SAI"),
                });
            }
            if !aaaa_queried {
                let q = encode_query(0, &[(target.as_str(), TYPE_AAAA)]);
                sock.send_to(&q, dest).await.map_err(DnssdError::Io)?;
                aaaa_queried = true;
            }
        }
    }
    Err(DnssdError::Timeout { instance: service })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_name_matches_avahi_form() {
        // fabric.rs の spec テストベクタと同じ CFID
        let cfid = [0x87, 0xE1, 0xB0, 0x04, 0xE2, 0x35, 0xA1, 0x30];
        assert_eq!(
            operational_instance(&cfid, 0xCD55_44AA_7B13_EF14),
            "87E1B004E235A130-CD5544AA7B13EF14"
        );
        // 小さい node id は 0 埋め 16 桁
        assert_eq!(
            operational_instance(&cfid, 5),
            "87E1B004E235A130-0000000000000005"
        );
    }

    #[test]
    fn encodes_srv_query() {
        let q = encode_query(0, &[("a.local", TYPE_SRV)]);
        assert_eq!(
            q,
            [
                0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, // header: id 0, 1 question
                1, b'a', 5, b'l', b'o', b'c', b'a', b'l', 0, // qname a.local
                0, 33, 0, 1, // SRV, IN
            ]
        );
    }

    /// SRV + TXT + AAAA を 1 メッセージに合成。AAAA のレコード名は SRV rdata
    /// 内の target 名への圧縮ポインタで書き、クラスには cache-flush bit を
    /// 立てて実 mDNS 応答の形に寄せる。
    fn synth_response(
        service: &str,
        target: &str,
        port: u16,
        txt: &[&str],
        addr: Ipv6Addr,
    ) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&[0, 0, 0x84, 0x00]); // id 0, QR|AA
        m.extend_from_slice(&[0, 0, 0, 3, 0, 0, 0, 0]); // qd 0, an 3, ns/ar 0
        // --- SRV ---
        push_name(&mut m, service);
        m.extend_from_slice(&TYPE_SRV.to_be_bytes());
        m.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 120]); // cache-flush|IN, ttl
        let mut rdata = vec![0, 0, 0, 0]; // priority, weight
        rdata.extend_from_slice(&port.to_be_bytes());
        let mut tname = Vec::new();
        push_name(&mut tname, target);
        rdata.extend_from_slice(&tname);
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        let target_off = m.len() + 6; // rdata 先頭から 6B 目が target 名
        m.extend_from_slice(&rdata);
        // --- TXT ---
        push_name(&mut m, service);
        m.extend_from_slice(&TYPE_TXT.to_be_bytes());
        m.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 120]);
        let mut rdata = Vec::new();
        for s in txt {
            rdata.push(s.len() as u8);
            rdata.extend_from_slice(s.as_bytes());
        }
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&rdata);
        // --- AAAA（名前は SRV target への圧縮ポインタ）---
        m.extend_from_slice(&[0xC0 | (target_off >> 8) as u8, (target_off & 0xFF) as u8]);
        m.extend_from_slice(&TYPE_AAAA.to_be_bytes());
        m.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 120]);
        m.extend_from_slice(&16u16.to_be_bytes());
        m.extend_from_slice(&addr.octets());
        m
    }

    #[test]
    fn parses_srv_txt_aaaa_with_compression() {
        let addr: Ipv6Addr = "fd00::1234".parse().unwrap();
        let msg = synth_response(
            "0000000000000001-0000000000000002._matter._tcp.local",
            "dev.local",
            5540,
            &["SII=5000", "SAI=300", "T=1"],
            addr,
        );
        let records = parse_message(&msg).unwrap();
        assert_eq!(records.len(), 3);
        let RData::Srv { port, ref target } = records[0].rdata else {
            panic!("not srv");
        };
        assert_eq!(port, 5540);
        assert_eq!(target, "dev.local");
        let RData::Txt(ref strings) = records[1].rdata else {
            panic!("not txt");
        };
        assert_eq!(txt_u32(strings, "SII"), Some(5000));
        assert_eq!(txt_u32(strings, "sii"), Some(5000)); // key は大文字小文字非依存
        assert_eq!(txt_u32(strings, "SAI"), Some(300));
        assert_eq!(txt_u32(strings, "SAT"), None);
        // AAAA の圧縮名が SRV target に解決される
        assert_eq!(records[2].name, "dev.local");
        let RData::Aaaa(got) = records[2].rdata else {
            panic!("not aaaa");
        };
        assert_eq!(got, addr);
    }

    #[test]
    fn rejects_compression_pointer_loop() {
        // qd 0, an 1: レコード名 = 自分自身を指すポインタ
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        m.extend_from_slice(&[0xC0, 12]);
        assert!(matches!(
            parse_message(&m),
            Err(DnssdError::Malformed("compression pointer loop"))
        ));
    }

    #[test]
    fn mrp_config_uses_sii_and_clamps() {
        let mut node = ResolvedNode {
            port: 5540,
            addresses: vec![],
            session_idle_interval_ms: Some(5000),
            session_active_interval_ms: Some(300),
        };
        assert_eq!(node.mrp_config().initial_interval, Duration::from_millis(5000));
        node.session_idle_interval_ms = None;
        assert_eq!(node.mrp_config().initial_interval, Duration::from_millis(500));
        node.session_idle_interval_ms = Some(999_999_999);
        assert_eq!(
            node.mrp_config().initial_interval,
            Duration::from_millis(3_600_000)
        );
        // 再送回数/バックオフは既定を保つ
        let d = MrpConfig::default();
        assert_eq!(node.mrp_config().max_retries, d.max_retries);
    }

    #[test]
    fn socket_addrs_prefers_non_link_local_and_scopes_link_local() {
        let ll: Ipv6Addr = "fe80::1".parse().unwrap();
        let ula: Ipv6Addr = "fd00::2".parse().unwrap();
        let node = ResolvedNode {
            port: 5540,
            addresses: vec![ula, ll], // resolve_operational が非 LL 先頭で返す形
            session_idle_interval_ms: None,
            session_active_interval_ms: None,
        };
        let addrs = node.socket_addrs(7);
        assert_eq!(addrs.len(), 2);
        let SocketAddr::V6(a0) = addrs[0] else { panic!() };
        assert_eq!(*a0.ip(), ula);
        assert_eq!(a0.scope_id(), 0);
        assert_eq!(a0.port(), 5540);
        let SocketAddr::V6(a1) = addrs[1] else { panic!() };
        assert_eq!(*a1.ip(), ll);
        assert_eq!(a1.scope_id(), 7);
    }
}
```

- [ ] **Step 2: lib.rs に登録**

`crates/mat-controller/src/lib.rs` の `pub mod crypto;` の直後に:

```rust
pub mod dnssd;
```

- [ ] **Step 3: テスト実行**

Run: `cargo test -p mat-controller dnssd`
Expected: PASS（6 テスト）。コンパイルエラーが出たら本 Step で修正（`tokio::time::Instant::saturating_duration_since` は tokio 1.x にある。無い場合は `checked_duration_since(now).unwrap_or(Duration::ZERO)` に置換）。

- [ ] **Step 4: 全体チェック**

Run: `task check`
Expected: 全通過（clippy -D warnings 込み）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/dnssd.rs crates/mat-controller/src/lib.rs
git commit -m "feat(mat-controller): one-shot mDNS resolver for operational nodes

Legacy unicast query (RFC 6762 s6.7) for
<CompressedFabricId>-<NodeId>._matter._tcp.local: SRV+TXT in one message,
AAAA for the SRV target chased if not bundled, resent every second until
the caller's deadline. Hand-written DNS codec (compression pointers
hop-bounded), no new dependencies. TXT SII feeds MrpConfig
(M1 carry-over: connect MRP retransmission to advertised intervals);
non-link-local addresses are preferred, link-local ones get the caller's
scope id.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: im — colorcontrol 定数 + MoveToHueAndSaturation fields エンコーダ

初のフィールド付きコマンド。`encode_invoke_request` の `fields_tlv` スプライス（M2 実装済み）に載せる CommandFields を組む。

**Files:**
- Modify: `crates/mat-controller/src/im.rs`

**Interfaces:**
- Consumes: `crate::tlv::{Writer, Tag}`（`start_struct` / `put_uint` / `end_container` / `finish`）、既存 `encode_invoke_request(endpoint, cluster, command, fields_tlv: Option<&[u8]>)`
- Produces:
  - `pub const CLUSTER_COLOR_CONTROL: u32 = 0x0300;`
  - `pub const ATTR_CURRENT_HUE: u32 = 0x0000;`
  - `pub const ATTR_CURRENT_SATURATION: u32 = 0x0001;`
  - `pub const CMD_MOVE_TO_HUE_AND_SATURATION: u32 = 0x06;`
  - `pub fn encode_move_to_hue_and_saturation_fields(hue: u8, saturation: u8, transition_time_ds: u16) -> Vec<u8>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/im.rs` の `mod tests` に追加（既存テストの import 流儀に合わせる。`Reader`/`Value` が未 import なら `use crate::tlv::{Reader, Value};` を test モジュールに追加）:

```rust
    #[test]
    fn move_to_hue_and_saturation_fields_shape() {
        let fields = encode_move_to_hue_and_saturation_fields(200, 254, 10);
        let mut r = Reader::new(&fields);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let expect = [
            (0u8, 200u64), // hue
            (1, 254),      // saturation
            (2, 10),       // transition time (0.1s 単位)
            (3, 0),        // options mask
            (4, 0),        // options override
        ];
        for (tag, val) in expect {
            let el = r.next().unwrap().unwrap();
            assert_eq!((el.tag, el.value), (Tag::Context(tag), Value::Uint(val)));
        }
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        assert!(r.next().unwrap().is_none());
    }

    #[test]
    fn move_fields_splice_into_invoke_request() {
        // fields_tlv スプライス経路（well-formed 1 要素として受理され panic しない）
        let fields = encode_move_to_hue_and_saturation_fields(1, 2, 3);
        let req = encode_invoke_request(
            1,
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(&fields),
        );
        assert!(!req.is_empty());
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller im`
Expected: 定数/関数未定義でコンパイルエラー。

- [ ] **Step 3: 実装**

`CMD_ON_OFF_TOGGLE` の直後に定数群:

```rust
pub const CLUSTER_COLOR_CONTROL: u32 = 0x0300;
pub const ATTR_CURRENT_HUE: u32 = 0x0000;
pub const ATTR_CURRENT_SATURATION: u32 = 0x0001;
pub const CMD_MOVE_TO_HUE_AND_SATURATION: u32 = 0x06;
```

`encode_invoke_request` の直前に:

```rust
/// CommandFields for colorcontrol MoveToHueAndSaturation (cluster spec
/// §3.2.11.7): `{0: hue, 1: saturation, 2: transition_time (0.1 s units),
/// 3: options_mask, 4: options_override}`. Options are fixed to 0 (execute
/// unconditionally), which is what chip-tool sends by default too.
pub fn encode_move_to_hue_and_saturation_fields(
    hue: u8,
    saturation: u8,
    transition_time_ds: u16,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(hue));
    w.put_uint(Tag::Context(1), u64::from(saturation));
    w.put_uint(Tag::Context(2), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(3), 0);
    w.put_uint(Tag::Context(4), 0);
    w.end_container();
    w.finish()
}
```

（`Writer` が im.rs 本体で未 import なら `use crate::tlv::{...}` に追加。）

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller im`
Expected: PASS

- [ ] **Step 5: 全体チェック + コミット**

Run: `task check` → 全通過を確認してから:

```bash
git add crates/mat-controller/src/im.rs
git commit -m "feat(mat-controller): colorcontrol constants + MoveToHueAndSaturation fields

First fielded command over the M2 fields_tlv splice path: cluster 0x0300
constants (current-hue / current-saturation / MoveToHueAndSaturation) and
the CommandFields encoder (hue, saturation, transition time, options fixed
to 0 like chip-tool's default).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: jarvis 相乗りライブ E2E + ハーネス + docs（受け入れゲート）

Tasks 1–3 を統合する実機受け入れ。テストは `#[ignore]`、ハーネスがクロスビルド → 転送 → 実機実行する。**このタスクの Step 5（実機実行）はメインセッション（人間の環境情報を持つ側）で行い、subagent はコード + スクリプト + `task check` まで。**

**Files:**
- Create: `crates/mat-controller/tests/live_jarvis.rs`
- Create: `scripts/e2e-m3.sh`（実行権限付き）
- Modify: `Taskfile.yml`（`e2e:m2` の直後に `e2e:m3`）
- Modify: `ARCHITECTURE.md`（Phase 5 節の M2 行の後に M3 行 — **実機 E2E 合格後の Step 6 で**）

**Interfaces:**
- Consumes:
  - `kvs::read_self_issue_materials(&Path, &Path, u8, u8) -> Result<SelfIssueMaterials, KvsError>`（Task 1 の id ソース変更済み）
  - `fabric::FabricCredentials::from_self_issued(SelfIssueMaterials) -> Result<FabricCredentials, FabricError>`、`fabric::compressed_fabric_id(&[u8; 65], u64) -> [u8; 8]`
  - `dnssd::{resolve_operational, iface_index, ResolvedNode}`（Task 2）
  - `im::{CLUSTER_ON_OFF, ATTR_ON_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_OFF, CMD_ON_OFF_TOGGLE, CLUSTER_COLOR_CONTROL, ATTR_CURRENT_HUE, ATTR_CURRENT_SATURATION, CMD_MOVE_TO_HUE_AND_SATURATION, encode_move_to_hue_and_saturation_fields, ImValue}`（Task 3）
  - `case::establish(&UdpTransport, SocketAddr, &FabricCredentials, u64, &MrpConfig) -> Result<SecureSession, CaseError>`、`session.read_attribute(u16, u32, u32, &MrpConfig)`、`session.invoke(u16, u32, u32, Option<&[u8]>, &MrpConfig)`
- Produces: `task e2e:m3`（env: 必須 `MAT_E2E_HOST` `MAT_E2E_NODE_ID`、任意 `MAT_E2E_KVS_DIR` `MAT_E2E_IFACE` `MAT_E2E_FABRIC_INDEX` `MAT_E2E_ENDPOINT` `MAT_E2E_ISSUER_INDEX` `MAT_E2E_PEER`）

- [ ] **Step 1: ライブテストを書く**

`crates/mat-controller/tests/live_jarvis.rs` を新規作成:

```rust
//! Live E2E (M3): ride chip-tool's production fabric on a real device —
//! self-issued identity from the KVS, our own mDNS resolution, CASE, then
//! onoff round-trip and a colorcontrol change. Run via `task e2e:m3`
//! (cross-build → transfer → run on the controller host). Not in CI.
//!
//! Required env: MAT_E2E_KVS_DIR, MAT_E2E_NODE_ID, and MAT_E2E_IFACE
//! (or MAT_E2E_PEER to bypass mDNS when isolating failures).
//! Optional: MAT_E2E_FABRIC_INDEX (1), MAT_E2E_ENDPOINT (1),
//! MAT_E2E_ISSUER_INDEX (0).

use std::path::PathBuf;
use std::time::Duration;

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::{compressed_fabric_id, FabricCredentials};
use mat_controller::im::{
    encode_move_to_hue_and_saturation_fields, ImValue, ATTR_CURRENT_HUE, ATTR_CURRENT_SATURATION,
    ATTR_ON_OFF, CLUSTER_COLOR_CONTROL, CLUSTER_ON_OFF, CMD_MOVE_TO_HUE_AND_SATURATION,
    CMD_ON_OFF_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_TOGGLE,
};
use mat_controller::session::SecureSession;
use mat_controller::transport::UdpTransport;
use mat_controller::{case, dnssd, kvs};

fn env_u64(name: &str) -> u64 {
    let s = std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).expect("hex id"),
        None => s.parse().expect("decimal id"),
    }
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(s) => s
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number")),
        Err(_) => default,
    }
}

async fn read_bool(s: &mut SecureSession<'_>, ep: u16, cfg: &MrpConfig) -> bool {
    match s
        .read_attribute(ep, CLUSTER_ON_OFF, ATTR_ON_OFF, cfg)
        .await
        .expect("read on-off")
    {
        ImValue::Bool(b) => b,
        v => panic!("on-off not bool: {v:?}"),
    }
}

async fn read_color_u8(s: &mut SecureSession<'_>, ep: u16, attr: u32, cfg: &MrpConfig) -> u8 {
    match s
        .read_attribute(ep, CLUSTER_COLOR_CONTROL, attr, cfg)
        .await
        .expect("read colorcontrol attr")
    {
        ImValue::Uint(v) => u8::try_from(v).expect("u8 attr"),
        v => panic!("colorcontrol attr not uint: {v:?}"),
    }
}

#[tokio::test]
#[ignore = "requires chip-tool KVS + a commissioned real device (task e2e:m3)"]
async fn fabric_ride_along_onoff_and_color() {
    let dir = PathBuf::from(std::env::var("MAT_E2E_KVS_DIR").expect("MAT_E2E_KVS_DIR required"));
    let device_node_id = env_u64("MAT_E2E_NODE_ID");
    let endpoint: u16 = env_parse("MAT_E2E_ENDPOINT", 1);
    let fabric_index: u8 = env_parse("MAT_E2E_FABRIC_INDEX", 1);
    let issuer_index: u8 = env_parse("MAT_E2E_ISSUER_INDEX", 0);

    // 受け入れ 1: KVS から CA 材料 + NOC 由来の node/fabric id
    let materials = kvs::read_self_issue_materials(
        &dir.join("chip_tool_config.alpha.ini"),
        &dir.join("chip_tool_config.ini"),
        fabric_index,
        issuer_index,
    )
    .expect("read CA materials");
    eprintln!(
        "controller node id 0x{:016X}, fabric id 0x{:016X} (from f/{}/n)",
        materials.node_id, materials.fabric_id, fabric_index
    );

    // 受け入れ 2: 本番 fabric への相乗り identity を自己発行
    let creds = FabricCredentials::from_self_issued(materials).expect("self-issue NOC");

    // 受け入れ 3: 自前 mDNS 解決（MAT_E2E_PEER は障害切り分け用バイパス）
    let (peers, mrp): (Vec<std::net::SocketAddr>, MrpConfig) = match std::env::var("MAT_E2E_PEER")
    {
        Ok(p) => (vec![p.parse().expect("socket addr")], MrpConfig::default()),
        Err(_) => {
            let iface = std::env::var("MAT_E2E_IFACE")
                .expect("MAT_E2E_IFACE or MAT_E2E_PEER required");
            let scope = dnssd::iface_index(&iface).expect("iface index");
            let cfid = compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
            let node =
                dnssd::resolve_operational(scope, &cfid, device_node_id, Duration::from_secs(8))
                    .await
                    .expect("mDNS resolve (cross-check: avahi-browse -rtp _matter._tcp)");
            eprintln!(
                "resolved {} addr(s), port {}, SII {:?} ms, SAI {:?} ms",
                node.addresses.len(),
                node.port,
                node.session_idle_interval_ms,
                node.session_active_interval_ms
            );
            (node.socket_addrs(scope), node.mrp_config())
        }
    };

    // 受け入れ 4: CASE 確立（解決したアドレスを順に試す）
    let transport = UdpTransport::bind().await.unwrap();
    let mut session = None;
    for peer in &peers {
        match case::establish(&transport, *peer, &creds, device_node_id, &mrp).await {
            Ok(s) => {
                eprintln!("CASE established via {peer}");
                session = Some(s);
                break;
            }
            Err(e) => eprintln!("CASE via {peer} failed: {e}"),
        }
    }
    let mut session = session.expect("CASE establishment failed on all resolved addresses");

    // 受け入れ 5: onoff toggle 往復（元の状態に戻して終わる）
    let before = read_bool(&mut session, endpoint, &mrp).await;
    session
        .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, &mrp)
        .await
        .expect("toggle 1");
    assert_eq!(
        read_bool(&mut session, endpoint, &mrp).await,
        !before,
        "toggle must flip on-off"
    );
    session
        .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, &mrp)
        .await
        .expect("toggle 2");
    assert_eq!(
        read_bool(&mut session, endpoint, &mrp).await,
        before,
        "second toggle must restore on-off"
    );
    eprintln!("onoff toggle round-trip OK (was {before})");

    // 受け入れ 6: 色変更（ライト on で実施し、hue/sat とも元へ復元）
    if !before {
        session
            .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None, &mrp)
            .await
            .expect("on for color");
    }
    let hue0 = read_color_u8(&mut session, endpoint, ATTR_CURRENT_HUE, &mrp).await;
    let sat0 = read_color_u8(&mut session, endpoint, ATTR_CURRENT_SATURATION, &mrp).await;
    // CurrentHue は 0..=254 の円環。確実に離れた目標を選ぶ。
    let target_hue = ((u16::from(hue0) + 80) % 254) as u8;
    let fields = encode_move_to_hue_and_saturation_fields(target_hue, 200, 0);
    session
        .invoke(
            endpoint,
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(&fields),
            &mrp,
        )
        .await
        .expect("move-to-hue-and-saturation");
    // transition 0 でも装置内の属性反映に猶予を置く
    tokio::time::sleep(Duration::from_millis(500)).await;
    let hue1 = read_color_u8(&mut session, endpoint, ATTR_CURRENT_HUE, &mrp).await;
    let d = (i32::from(hue1) - i32::from(target_hue)).abs();
    let d = d.min(254 - d); // 円環距離
    assert!(d <= 8, "current-hue {hue1} not near target {target_hue}");
    eprintln!("color change OK: hue {hue0} -> {hue1} (target {target_hue})");

    // 後始末: 色と電源状態を復元
    let fields = encode_move_to_hue_and_saturation_fields(hue0, sat0, 0);
    session
        .invoke(
            endpoint,
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(&fields),
            &mrp,
        )
        .await
        .expect("restore color");
    if !before {
        session
            .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_OFF, None, &mrp)
            .await
            .expect("restore off");
    }
    eprintln!("restored original state");
}
```

- [ ] **Step 2: ハーネススクリプトを書く**

`scripts/e2e-m3.sh` を新規作成し `chmod +x`:

```bash
#!/usr/bin/env bash
# Phase 5 M3 受け入れ: jarvis 相乗り live E2E。aarch64-musl クロスビルド →
# 転送 → コントローラ実機上で実行（KVS とデバイスは実機側にあるため）。
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_NODE_ID（対象 device node id。同上）
# 任意 env: MAT_E2E_KVS_DIR（既定 ~/.config/mat）
#           MAT_E2E_IFACE（既定: リモートの default route の iface）
#           MAT_E2E_FABRIC_INDEX（既定 1）/ MAT_E2E_ENDPOINT（既定 1）
#           MAT_E2E_ISSUER_INDEX（既定 0）/ MAT_E2E_PEER（mDNS バイパス）
set -euo pipefail
cd "$(dirname "$0")/.."
: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
: "${MAT_E2E_NODE_ID:?MAT_E2E_NODE_ID (device node id) required}"

echo "== 1/3 クロスビルド (aarch64-unknown-linux-musl, rust-lld)"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo test -p mat-controller --test live_jarvis --release \
  --target aarch64-unknown-linux-musl --no-run
BIN=$(ls -t target/aarch64-unknown-linux-musl/release/deps/live_jarvis-* \
  | grep -v '\.d$' | head -1)
file "$BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $BIN"; exit 1; }
echo "binary: $BIN"

echo "== 2/3 転送 → $MAT_E2E_HOST"
# scp は ssh-agent の状態に左右されるため、確実な ssh cat 方式で送る
ssh "$MAT_E2E_HOST" 'cat > /tmp/live_jarvis && chmod +x /tmp/live_jarvis' < "$BIN"

echo "== 3/3 実機で実行"
ssh "$MAT_E2E_HOST" \
  MAT_E2E_NODE_ID="$MAT_E2E_NODE_ID" \
  MAT_E2E_FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-1}" \
  MAT_E2E_ENDPOINT="${MAT_E2E_ENDPOINT:-1}" \
  MAT_E2E_ISSUER_INDEX="${MAT_E2E_ISSUER_INDEX:-0}" \
  MAT_E2E_KVS_DIR="${MAT_E2E_KVS_DIR:-}" \
  MAT_E2E_IFACE="${MAT_E2E_IFACE:-}" \
  MAT_E2E_PEER="${MAT_E2E_PEER:-}" \
  'bash -s' <<'EOF'
set -euo pipefail
[ -n "${MAT_E2E_KVS_DIR}" ] || MAT_E2E_KVS_DIR="$HOME/.config/mat"
if [ -z "${MAT_E2E_IFACE}" ] && [ -z "${MAT_E2E_PEER}" ]; then
  MAT_E2E_IFACE=$(ip route show default | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -1)
  echo "auto-detected iface: ${MAT_E2E_IFACE}"
fi
export MAT_E2E_KVS_DIR MAT_E2E_IFACE
[ -n "${MAT_E2E_PEER}" ] && export MAT_E2E_PEER || unset MAT_E2E_PEER
exec /tmp/live_jarvis --ignored --nocapture
EOF

echo "== e2e:m3 PASS"
```

- [ ] **Step 3: Taskfile に登録**

`Taskfile.yml` の `e2e:m2` タスクの直後に追加:

```yaml
  e2e:m3:
    desc: M3 ライブ E2E（jarvis 相乗り。要 MAT_E2E_HOST / MAT_E2E_NODE_ID。実機で onoff+色変更）
    cmds:
      - bash scripts/e2e-m3.sh
```

- [ ] **Step 4: CI が壊れていないことを確認**

Run: `task check`
Expected: 全通過（ライブテストは `#[ignore]`）。加えてライブテストのコンパイルを確認:

Run: `cargo test -p mat-controller --test live_jarvis --no-run`
Expected: コンパイル成功。

- [ ] **Step 5: 実機 E2E（メインセッションで実行。subagent はここで返す）**

```bash
MAT_E2E_HOST=<controller host> MAT_E2E_NODE_ID=<device node id> task e2e:m3
```

Expected: `== e2e:m3 PASS`。stderr に controller node/fabric id（`f/<idx>/n` 由来）、
resolved addr/SII、`CASE established`、`onoff toggle round-trip OK`、`color change OK` が出る。

トラブルシュート:
- mDNS timeout → 実機で `avahi-browse -rtp _matter._tcp` に当該インスタンスが出るか
  （SRP 未登録ノードは広告ゼロ = 既知事象）。`MAT_E2E_PEER='[<ipv6>]:5540'` で
  mDNS を切って CASE 以降のみ検証可能。
- CASE timeout → node の到達性を既存経路（`mat read`）で確認。matd の warm セッション
  とは独立に張れるはずだが、詰まる場合は `sudo systemctl stop matd` して再試行。
- ACCESS_DENIED (0x7E) → デバイス ACL が controller node id を許していない。
  `f/<idx>/n` の subject と ACL の admin subject を突き合わせる。

- [ ] **Step 6: docs 反映（実機合格後）**

`ARCHITECTURE.md` の Phase 5 節、M2 の行の直後に追加:

```markdown
- M3 完了(2026-07-12): 相乗りの堅牢化（node/fabric id を fabric テーブルの
  NOC subject から取得 — KVS index 非依存）+ 自前 one-shot mDNS 解決
  （TXT SII→MRP 接続）+ colorcontrol。jarvis 実機 Nanoleaf に本番 fabric
  相乗りで CASE + onoff/色変更の E2E 合格（`task e2e:m3`）。
```

- [ ] **Step 7: コミット**

```bash
git add crates/mat-controller/tests/live_jarvis.rs scripts/e2e-m3.sh Taskfile.yml ARCHITECTURE.md
git commit -m "feat(mat-controller): M3 live E2E — production-fabric ride-along on a real device

Cross-build harness (task e2e:m3): self-issue an identity from the
controller host's chip-tool KVS, resolve the device with our own mDNS
(SII feeding MRP), establish CASE, then onoff toggle round-trip and a
colorcontrol MoveToHueAndSaturation verified via current-hue read-back,
restoring the original state. Host and node id come from env only (public
repo: no real hosts/node ids committed).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## M3 の既知の限界（M4 への引き継ぎ事項）

- リゾルバは one-shot・IPv6 のみ・TXT 待ち合わせなし（SRV+AAAA が揃った時点で返す。
  TXT が別パケットで遅れて来る責務は追わない — MRP は既定 500ms にフォールバック）。
  matd 常駐化（M4）でキャッシュ/再解決の設計を行う。
- `MrpConfig` へは SII のみ接続。CASE 確立後の active interval（SAI）切替は
  `ResolvedNode.session_active_interval_ms` に保持済みで、M4 の warm セッション
  管理で接続する。
- `iface_index` は Linux 専用（/sys 読み）。mat の対応プラットフォームは Linux のみ
  なので許容。
- chip-tool KVS フォーマットは v1.4.2.0 固定（従来方針どおり、上流更新はユニット
  テストの破壊で検知）。

## Self-Review（記録）

- spec 受け入れ 1（NOC 由来 id）→ Task 1。受け入れ 3（mDNS + SII）→ Task 2。
  受け入れ 6（色変更）→ Task 3 + Task 4。受け入れ 2/4/5/7（自己発行・CASE・onoff・
  CI 維持）→ Task 4 + 各タスクの `task check`。spec 決定 1〜3 と非ゴールに整合。
- 型整合: `KvsError::BadNoc`（Task 1 定義 → Task 1 テストのみ使用）、
  `dnssd::{resolve_operational, iface_index, ResolvedNode::{mrp_config, socket_addrs}}`
  （Task 2 定義 → Task 4 使用、シグネチャ一致確認済み）、
  `encode_move_to_hue_and_saturation_fields` / colorcontrol 定数（Task 3 定義 →
  Task 4 使用）。`case::establish` / `SecureSession::{read_attribute, invoke}` /
  `FabricCredentials::from_self_issued` は既存 API を実ソースで確認済み。
- プレースホルダなし（全 Step にコード/コマンド実体あり）。
- 公開 repo 制約: live test / スクリプトとも実ホスト・実 node id は env 必須で
  ハードコードなし。テストのダミーは fd00::/fe80:: と chip SDK フィクスチャのみ。
