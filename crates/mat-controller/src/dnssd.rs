//! Minimal one-shot mDNS/DNS-SD resolver for Matter operational services
//! (Matter spec §4.3; RFC 6762 legacy unicast queries; RFC 2782 SRV).
//!
//! Scope: resolve one `<CompressedFabricId>-<NodeId>._matter._tcp.local`
//! instance to IPv6 addresses + port + MRP intervals (TXT `SII`/`SAI`).
//! No advertising, no cache: send a legacy unicast query (source port ≠
//! 5353, so responders reply straight back to us), fold responses until
//! SRV + at least one AAAA for its target are in hand. TXT is folded when
//! it arrives in the same responses but is not waited for — MRP falls back
//! to the spec default interval without it.
//!
//! M8b adds one-shot browse (`browse_commissionable` / `browse_operational`):
//! same legacy unicast transport, but enumerating PTR answers for a whole
//! service type and folding SRV/TXT/AAAA per instance until a fixed window
//! ([`BROWSE_WINDOW`]) expires — no early return, still no cache.

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::exchange::MrpConfig;

const MDNS_GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_PORT: u16 = 5353;
const TYPE_PTR: u16 = 12;
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
                write!(
                    f,
                    "dnssd: no SRV+AAAA answer for \"{instance}\" within the deadline"
                )
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

/// Byte budget per packet for [`encode_ptr_query_with_known`] (comfortably
/// under a typical path MTU; real responders truncated at ~1428B — see the
/// module doc's known-answer-suppression note).
const KNOWN_ANSWER_PACKET_BUDGET: usize = 1400;

/// PTR クエリ + Known-Answer リストを 1..N 個のパケットに符号化する
/// (RFC 6762 §7.2)。KA が 1 パケットに収まらない場合は分割し、最後以外の
/// パケットに TC を立てる（responder は TC の間、応答を保留して継続を待つ）。
/// 2 パケット目以降は question 数 0 の KA 継続パケット。
///
/// レコードの owner name は、そのパケット内でオフセット 12 に置かれた名前
/// （パケット 1 なら question 名そのもの、継続パケットならその中の最初の
/// レコードが literal に書く service 名）への圧縮ポインタ (`0xC0 0x0C`) で
/// 表す。継続パケットの最初のレコードだけは、指す先がまだ無いので service
/// 名を literal に書く（以後のレコードはそれを指せる）。rdata（instance の
/// 完全名）は先頭ラベルを literal に書き、残り（service 名の tail）は同じ
/// オフセット 12 への圧縮ポインタで表す。
///
/// 既知 instance が 0 件のときは旧来の単発クエリ（TC 無し、1 パケット）に
/// 退化する。
fn encode_ptr_query_with_known(service: &str, known: &[(String, u32)]) -> Vec<Vec<u8>> {
    if known.is_empty() {
        return vec![encode_query(0, &[(service, TYPE_PTR)])];
    }

    let mut qname = Vec::new();
    push_name(&mut qname, service);
    let mut question = qname.clone();
    question.extend_from_slice(&TYPE_PTR.to_be_bytes());
    question.extend_from_slice(&CLASS_IN.to_be_bytes());

    // 各 KA の "tail"（type+class+ttl+rdlength+rdata）を先に組み立てる。
    // owner name（ポインタ or literal）はパケット内の位置に依存するため、
    // グループ分けの段階で別途足す。
    let suffix = format!(".{service}");
    let mut ka_tails: Vec<Vec<u8>> = Vec::new();
    for (name, ttl) in known {
        if name.len() <= suffix.len() {
            continue; // 防御的スキップ: service の下位名の形になっていない
        }
        let (label, tail) = name.split_at(name.len() - suffix.len());
        if !tail.eq_ignore_ascii_case(&suffix) || label.is_empty() || label.len() > 63 {
            continue;
        }
        let mut rec = Vec::with_capacity(2 + 2 + 4 + 2 + 1 + label.len() + 2);
        rec.extend_from_slice(&TYPE_PTR.to_be_bytes());
        rec.extend_from_slice(&CLASS_IN.to_be_bytes());
        rec.extend_from_slice(&ttl.to_be_bytes());
        let rdlen = (1 + label.len() + 2) as u16;
        rec.extend_from_slice(&rdlen.to_be_bytes());
        rec.push(label.len() as u8);
        rec.extend_from_slice(label.as_bytes());
        rec.extend_from_slice(&[0xC0, 0x0C]); // rdata: service 名 tail への圧縮ポインタ
        ka_tails.push(rec);
    }

    if ka_tails.is_empty() {
        return vec![encode_query(0, &[(service, TYPE_PTR)])];
    }

    // グループ分け: 各パケットの owner name コストは
    // - パケット 1 のレコード: 常に 2B ポインタ（question が既にオフセット 12 にある）
    // - 継続パケットの最初のレコード: qname.len()B literal（後続の指す先を作る）
    // - 継続パケットの 2 個目以降: 2B ポインタ
    let mut groups: Vec<Vec<usize>> = vec![Vec::new()];
    let mut current_size = 12 + question.len();
    for (idx, tail) in ka_tails.iter().enumerate() {
        loop {
            let gi = groups.len() - 1;
            let is_packet0 = gi == 0;
            let is_first_in_group = groups[gi].is_empty();
            let name_len = if is_packet0 || !is_first_in_group {
                2
            } else {
                qname.len()
            };
            let rec_len = name_len + tail.len();
            if groups[gi].is_empty() || current_size + rec_len <= KNOWN_ANSWER_PACKET_BUDGET {
                groups[gi].push(idx);
                current_size += rec_len;
                break;
            }
            groups.push(Vec::new());
            current_size = 12;
        }
    }

    let n = groups.len();
    groups
        .into_iter()
        .enumerate()
        .map(|(i, idxs)| {
            let mut out = Vec::new();
            out.extend_from_slice(&0u16.to_be_bytes()); // id
            let flags: u16 = if i + 1 < n { 0x0200 } else { 0 };
            out.extend_from_slice(&flags.to_be_bytes());
            let qdcount: u16 = if i == 0 { 1 } else { 0 };
            out.extend_from_slice(&qdcount.to_be_bytes());
            out.extend_from_slice(&(idxs.len() as u16).to_be_bytes());
            out.extend_from_slice(&[0, 0, 0, 0]); // ns/ar
            if i == 0 {
                out.extend_from_slice(&question);
            }
            for (j, &idx) in idxs.iter().enumerate() {
                if i == 0 || j > 0 {
                    out.extend_from_slice(&[0xC0, 0x0C]);
                } else {
                    out.extend_from_slice(&qname);
                }
                out.extend_from_slice(&ka_tails[idx]);
            }
            out
        })
        .collect()
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
    Ptr(String),
    Srv { port: u16, target: String },
    Txt(Vec<Vec<u8>>),
    Aaaa(Ipv6Addr),
    Other,
}

struct Record {
    name: String,
    rdata: RData,
    ttl: u32,
}

/// Smallest possible record: 1-byte root name + type(2) + class(2) +
/// ttl(4) + rdlength(2) with empty rdata.
const MIN_RECORD_LEN: usize = 11;
/// Cap on folded AAAA candidates while the SRV target is still unknown —
/// a flooder must not grow memory; the real address always fits once the
/// SRV answer arrives and non-matching entries are pruned.
const MAX_AAAA: usize = 16;

/// Capacity to pre-reserve for `claimed` records in a `msg_len`-byte
/// message: never more than could physically fit (header counts are
/// attacker-controlled; a forged 3×65535 must not reserve megabytes).
fn record_capacity(claimed: usize, msg_len: usize) -> usize {
    claimed.min(msg_len.saturating_sub(12) / MIN_RECORD_LEN)
}

/// Folds one AAAA record into the candidate list, bounding growth:
/// once the SRV target is known only matching names are kept; before
/// that, candidates are capped at [`MAX_AAAA`] and deduplicated.
fn push_aaaa(
    aaaa: &mut Vec<(String, Ipv6Addr)>,
    srv_target: Option<&str>,
    name: String,
    addr: Ipv6Addr,
) {
    if let Some(target) = srv_target {
        if !name.eq_ignore_ascii_case(target) {
            return;
        }
    }
    if aaaa.len() >= MAX_AAAA {
        return;
    }
    if aaaa
        .iter()
        .any(|(n, a)| *a == addr && n.eq_ignore_ascii_case(&name))
    {
        return;
    }
    aaaa.push((name, addr));
}

/// Drops candidates that do not belong to the SRV target (called once the
/// target becomes known, so flooded slots free up for the real address).
fn prune_aaaa(aaaa: &mut Vec<(String, Ipv6Addr)>, target: &str) {
    aaaa.retain(|(n, _)| n.eq_ignore_ascii_case(target));
}

fn be16(buf: &[u8], pos: usize) -> Result<u16, DnssdError> {
    let b = buf
        .get(pos..pos + 2)
        .ok_or(DnssdError::Malformed("truncated"))?;
    Ok(u16::from_be_bytes(b.try_into().expect("2 bytes")))
}

fn be32(buf: &[u8], pos: usize) -> Result<u32, DnssdError> {
    let b = buf
        .get(pos..pos + 4)
        .ok_or(DnssdError::Malformed("truncated"))?;
    Ok(u32::from_be_bytes(b.try_into().expect("4 bytes")))
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
    let mut records = Vec::with_capacity(record_capacity(total, buf.len()));
    for _ in 0..total {
        let (name, p) = read_name(buf, pos)?;
        let rtype = be16(buf, p)?;
        let ttl = be32(buf, p + 4)?;
        let rdlen = usize::from(be16(buf, p + 8)?);
        let rdata_pos = p + 10;
        let rdata = buf
            .get(rdata_pos..rdata_pos + rdlen)
            .ok_or(DnssdError::Malformed("rdata past end"))?;
        let rdata = match rtype {
            TYPE_PTR => {
                // rdata はそれ自体が（圧縮され得る）1 個のドメイン名
                // (RFC 1035 §3.3.12)。メッセージ全体基準の絶対オフセットで
                // 読む。不正な圧縮ポインタなど、名前の読み込みに失敗しても
                // このデータグラム全体を捨てない（同一応答に有効な
                // SRV/TXT/AAAA が同梱されている場合、それらを失わないように
                // するため）。本番 resolve_operational パスは parse_message
                // 失敗でデータグラム全体を破棄するので、PTR だけの読み込み失敗が
                // 全体を巻き込まないことが重要。
                match read_name(buf, rdata_pos) {
                    Ok((name, _)) => RData::Ptr(name),
                    Err(_) => RData::Other,
                }
            }
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
        records.push(Record { name, rdata, ttl });
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
                    prune_aaaa(&mut aaaa, &target);
                    srv = Some((port, target));
                }
                RData::Txt(strings) if r.name.eq_ignore_ascii_case(&service) => {
                    txt = Some(strings);
                }
                RData::Aaaa(addr) => {
                    let target = srv.as_ref().map(|(_, t)| t.as_str());
                    push_aaaa(&mut aaaa, target, r.name, addr);
                }
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

/// Long-discriminator サブタイプ名（spec §4.3.1: `_L<discriminator>._sub.
/// _matterc._udp.local`、discriminator は 12bit を 10 進数表記、ゼロ埋めなし）。
fn long_discriminator_subtype(long_discriminator: u16) -> String {
    format!("_L{long_discriminator}._sub._matterc._udp.local")
}

/// SRV/TXT が判明し、SRV target のアドレスが 1 つ以上揃った時点で
/// `ResolvedNode` を組み立てる。TXT `D=` が `long_discriminator` と一致し
/// ない場合は（サブタイプで絞れていても、コミッショニング中の別デバイスの
/// 流れ弾を弾くため）拒否する。`commissionable_from_response`（単発応答）と
/// `resolve_commissionable`（複数応答にまたがる畳み込み）の両方から使う共通
/// ロジック。
fn build_commissionable(
    long_discriminator: u16,
    port: u16,
    target: &str,
    txt: &[Vec<u8>],
    aaaa: &[(String, Ipv6Addr)],
) -> Option<ResolvedNode> {
    if txt_u32(txt, "D") != Some(u32::from(long_discriminator)) {
        return None;
    }
    let mut addresses: Vec<Ipv6Addr> = Vec::new();
    for (name, addr) in aaaa {
        if name.eq_ignore_ascii_case(target) && !addresses.contains(addr) {
            addresses.push(*addr);
        }
    }
    if addresses.is_empty() {
        return None;
    }
    // 非 link-local 優先（同じクラス内では応答順を安定に保つ）。
    addresses.sort_by_key(is_link_local);
    Some(ResolvedNode {
        port,
        addresses,
        session_idle_interval_ms: txt_u32(txt, "SII"),
        session_active_interval_ms: txt_u32(txt, "SAI"),
    })
}

/// 1 個の DNS メッセージ単体から commissionable node を抽出する（PTR→
/// instance→SRV/TXT/AAAA が同一応答の additional に同梱された、行儀の良い
/// responder の通常ケース）。`resolve_commissionable` がまずこの高速経路を
/// 試し、ダメなら複数応答にまたがる畳み込みにフォールバックする。
fn commissionable_from_response(bytes: &[u8], long_discriminator: u16) -> Option<ResolvedNode> {
    let subtype = long_discriminator_subtype(long_discriminator);
    let records = parse_message(bytes).ok()?;
    let instance = records.iter().find_map(|r| match &r.rdata {
        RData::Ptr(name) if r.name.eq_ignore_ascii_case(&subtype) => Some(name.clone()),
        _ => None,
    })?;
    let (port, target) = records.iter().find_map(|r| match &r.rdata {
        RData::Srv { port, target } if r.name.eq_ignore_ascii_case(&instance) => {
            Some((*port, target.clone()))
        }
        _ => None,
    })?;
    let txt = records.iter().find_map(|r| match &r.rdata {
        RData::Txt(strings) if r.name.eq_ignore_ascii_case(&instance) => Some(strings.clone()),
        _ => None,
    })?;
    let mut aaaa: Vec<(String, Ipv6Addr)> = Vec::new();
    for r in &records {
        if let RData::Aaaa(addr) = &r.rdata {
            push_aaaa(&mut aaaa, Some(target.as_str()), r.name.clone(), *addr);
        }
    }
    build_commissionable(long_discriminator, port, &target, &txt, &aaaa)
}

/// One-shot legacy unicast mDNS browse for the commissionable node
/// advertising `long_discriminator` under `_matterc._udp` (spec §4.3.1).
/// Queries the long-discriminator service subtype PTR
/// (`_L<discriminator>._sub._matterc._udp.local`), then folds the PTR
/// answer's instance name against SRV/TXT/AAAA the same way
/// `resolve_operational` folds an operational instance's records — resent
/// every second until `timeout`. TXT `D=` is checked against
/// `long_discriminator` before a candidate is accepted: the subtype narrows
/// the browse, but a stray response from another commissioning-mode device
/// must not be mistaken for the intended one.
pub async fn resolve_commissionable(
    scope_id: u32,
    long_discriminator: u16,
    timeout: Duration,
) -> Result<ResolvedNode, DnssdError> {
    let subtype = long_discriminator_subtype(long_discriminator);
    let sock = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
        .await
        .map_err(DnssdError::Io)?;
    let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));

    let mut instance: Option<String> = None;
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
            let q = encode_query(0, &[(&subtype, TYPE_PTR)]);
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
        // 単発の完結応答（PTR+SRV+TXT+AAAA が全部同梱）はここで即決する。
        if let Some(node) = commissionable_from_response(&buf[..n], long_discriminator) {
            return Ok(node);
        }
        // そうでなければ、複数応答にまたがる断片を resolve_operational と
        // 同じ要領で畳み込む（AAAA が 2 段目クエリの別便で来る場合など）。
        // 他の応答者のデータグラムが壊れていても解決全体を中断しない。
        let Ok(records) = parse_message(&buf[..n]) else {
            continue;
        };
        for r in records {
            match r.rdata {
                RData::Ptr(name) if r.name.eq_ignore_ascii_case(&subtype) => {
                    instance = Some(name);
                }
                RData::Srv { port, target }
                    if instance
                        .as_deref()
                        .is_some_and(|i| r.name.eq_ignore_ascii_case(i)) =>
                {
                    prune_aaaa(&mut aaaa, &target);
                    srv = Some((port, target));
                }
                RData::Txt(strings)
                    if instance
                        .as_deref()
                        .is_some_and(|i| r.name.eq_ignore_ascii_case(i)) =>
                {
                    txt = Some(strings);
                }
                RData::Aaaa(addr) => {
                    let target = srv.as_ref().map(|(_, t)| t.as_str());
                    push_aaaa(&mut aaaa, target, r.name, addr);
                }
                _ => {}
            }
        }
        if let (Some((port, target)), Some(strings)) = (&srv, &txt) {
            if let Some(node) =
                build_commissionable(long_discriminator, *port, target, strings, &aaaa)
            {
                return Ok(node);
            }
            if !aaaa_queried {
                let q = encode_query(0, &[(target.as_str(), TYPE_AAAA)]);
                sock.send_to(&q, dest).await.map_err(DnssdError::Io)?;
                aaaa_queried = true;
            }
        }
    }
    Err(DnssdError::Timeout {
        instance: instance.unwrap_or(subtype),
    })
}

// ── browse（M8b: discover native 化）───────────────────────────────────

/// browse の収集ウィンドウ。resolve と違い「全員から集める」ため早期 return
/// せず、この時間で打ち切る。
pub const BROWSE_WINDOW: Duration = Duration::from_secs(3);
/// browse が追跡する instance 数の上限（偽装 flood でメモリを伸ばさない —
/// MAX_AAAA と同思想）。実機の複数 fabric レジストリは 32 を上回る
/// （2026-07 実機観測: 29+ instance が単一 TC 切り捨て応答に収まらず、かつ
/// 古い fabric の残留 entry も含め 32 を超過）ため 128 に拡張。
/// 128 × 約 100B は依然として無視できるフラッド上限。
const MAX_INSTANCES: usize = 128;
/// browse 中の AAAA 候補プール上限（instance 横断で共有）。
const MAX_BROWSE_AAAA: usize = 64;
/// フォローアップクエリ 1 メッセージあたりの質問数上限（MTU 超え回避）。
const MAX_QUESTIONS_PER_MSG: usize = 8;

/// `_matterc._udp` で見つかった commissionable 1 台分（TXT パース済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionableInstance {
    /// SRV target から末尾 `.local` を除いた形。
    pub hostname: Option<String>,
    pub port: Option<u16>,
    /// 非 link-local 優先でソート、dedup 済み。
    pub addresses: Vec<Ipv6Addr>,
    /// TXT `D`（long discriminator）。
    pub discriminator: Option<u32>,
    /// TXT `VP`（`<vendor>+<product>`、product は省略され得る）。
    pub vendor_id: Option<u32>,
    pub product_id: Option<u32>,
}

/// `_matter._tcp` で見つかった operational 1 台分。SRV/AAAA が期限内に揃わなく
/// ても PTR が見えた instance は返す（announce のみ = addresses 空 — 到達性
/// 判定側の「広告あり・アドレス未解決」セマンティクスを保存するため）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationalInstance {
    /// 16 桁大文字 hex。
    pub compressed_fabric: String,
    pub node_id: u64,
    pub addresses: Vec<Ipv6Addr>,
}

/// finish() が返す、サービス種別に依存しない 1 instance 分の素材。
struct FoldedInstance {
    /// instance の完全名（先頭ラベルが instance 名）。
    name: String,
    port: Option<u16>,
    target: Option<String>,
    txt: Vec<Vec<u8>>,
    /// SRV target に一致した AAAA（非 link-local 優先ソート、dedup 済み）。
    addresses: Vec<Ipv6Addr>,
}

#[derive(Default)]
struct InstanceFold {
    srv: Option<(u16, String)>,
    txt: Option<Vec<Vec<u8>>>,
    /// この instance を紹介した PTR レコードの TTL（重複 PTR は最新を残す）。
    /// Known-Answer suppression の再送クエリに載せる値。
    ttl: u32,
}

/// browse の畳み込み状態。データグラム単位で [`fold`](Self::fold) に食わせ、
/// window 満了後に [`finish`](Self::finish) で取り出す。
struct BrowseFold {
    /// 例 "_matterc._udp.local"（大文字小文字無視で照合）。
    service: String,
    /// key = instance 完全名。到着順・dedup・MAX_INSTANCES で打ち止め。
    instances: Vec<(String, InstanceFold)>,
    /// hostname → アドレスのプール（instance 横断で共有し、finish 時に
    /// SRV target 名で引く）。
    aaaa: Vec<(String, Ipv6Addr)>,
}

impl BrowseFold {
    fn new(service: &str) -> Self {
        BrowseFold {
            service: service.to_string(),
            instances: Vec::new(),
            aaaa: Vec::new(),
        }
    }

    /// 1 データグラム分を畳み込む。PTR を先に全部拾ってから SRV/TXT/AAAA を
    /// 処理する 2 パス（同一データグラム内のレコード順に依存しない）。
    fn fold(&mut self, records: &[Record]) {
        for r in records {
            if let RData::Ptr(inst) = &r.rdata {
                if !r.name.eq_ignore_ascii_case(&self.service) {
                    continue;
                }
                if let Some((_, f)) = self
                    .instances
                    .iter_mut()
                    .find(|(n, _)| n.eq_ignore_ascii_case(inst))
                {
                    // 重複 PTR: TTL は最新のものを残す。
                    f.ttl = r.ttl;
                } else if self.instances.len() < MAX_INSTANCES {
                    let f = InstanceFold {
                        ttl: r.ttl,
                        ..InstanceFold::default()
                    };
                    self.instances.push((inst.clone(), f));
                }
            }
        }
        for r in records {
            match &r.rdata {
                RData::Srv { port, target } => {
                    if let Some((_, f)) = self
                        .instances
                        .iter_mut()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&r.name))
                    {
                        f.srv = Some((*port, target.clone()));
                    }
                }
                RData::Txt(strings) => {
                    if let Some((_, f)) = self
                        .instances
                        .iter_mut()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&r.name))
                    {
                        f.txt = Some(strings.clone());
                    }
                }
                RData::Aaaa(addr)
                    if self.aaaa.len() < MAX_BROWSE_AAAA
                        && !self
                            .aaaa
                            .iter()
                            .any(|(n, a)| a == addr && n.eq_ignore_ascii_case(&r.name)) =>
                {
                    self.aaaa.push((r.name.clone(), *addr));
                }
                _ => {}
            }
        }
    }

    /// まだ足りない素材へのフォローアップ質問 (name, qtype)。
    fn pending_questions(&self) -> Vec<(String, u16)> {
        let mut out = Vec::new();
        for (name, f) in &self.instances {
            if f.srv.is_none() {
                out.push((name.clone(), TYPE_SRV));
            }
            if f.txt.is_none() {
                out.push((name.clone(), TYPE_TXT));
            }
            if let Some((_, target)) = &f.srv {
                if !self
                    .aaaa
                    .iter()
                    .any(|(n, _)| n.eq_ignore_ascii_case(target))
                {
                    out.push((target.clone(), TYPE_AAAA));
                }
            }
        }
        out
    }

    /// Known-Answer suppression（RFC 6762 §7.1）用に、既知 instance の
    /// (完全名, TTL) を返す。再送クエリの answer セクションに載せると
    /// responder は載っていない残りだけを返すため、単一データグラムに
    /// 収まらない大きなレジストリ（実機で 29 PTR + TC 切り捨てを実証）でも
    /// 再送のたびに続きが取れる。
    fn known_answers(&self) -> Vec<(String, u32)> {
        self.instances
            .iter()
            .map(|(name, f)| (name.clone(), f.ttl))
            .collect()
    }

    fn finish(self) -> Vec<FoldedInstance> {
        let pool = self.aaaa;
        self.instances
            .into_iter()
            .map(|(name, f)| {
                let (port, target) = match f.srv {
                    Some((p, t)) => (Some(p), Some(t)),
                    None => (None, None),
                };
                let mut addresses: Vec<Ipv6Addr> = Vec::new();
                if let Some(t) = &target {
                    for (n, a) in &pool {
                        if n.eq_ignore_ascii_case(t) && !addresses.contains(a) {
                            addresses.push(*a);
                        }
                    }
                    addresses.sort_by_key(is_link_local);
                }
                FoldedInstance {
                    name,
                    port,
                    target,
                    txt: f.txt.unwrap_or_default(),
                    addresses,
                }
            })
            .collect()
    }
}

/// One-shot legacy unicast mDNS browse: `service`（例 "_matterc._udp.local"）
/// の PTR を列挙し、instance ごとに SRV/TXT/AAAA を畳み込む。resolve_* と
/// 違い早期 return せず `window` 満了まで収集する（全員から集めるため、
/// 実行時間 = window で固定）。クエリは 1 秒間隔で再送。
async fn browse(
    scope_id: u32,
    service: &str,
    window: Duration,
) -> Result<Vec<FoldedInstance>, DnssdError> {
    let sock = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
        .await
        .map_err(DnssdError::Io)?;
    let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));
    let mut fold = BrowseFold::new(service);
    let deadline = Instant::now() + window;
    let mut next_send = Instant::now();
    // browse 応答は resolve より大きくなり得る（複数 instance の additional
    // 同梱）ため、受信バッファは mDNS の実質上限まで取る。
    let mut buf = vec![0u8; 9000];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_send {
            for q in encode_ptr_query_with_known(service, &fold.known_answers()) {
                sock.send_to(&q, dest).await.map_err(DnssdError::Io)?;
            }
            let pending = fold.pending_questions();
            for chunk in pending.chunks(MAX_QUESTIONS_PER_MSG) {
                let qs: Vec<(&str, u16)> = chunk.iter().map(|(n, t)| (n.as_str(), *t)).collect();
                let q = encode_query(0, &qs);
                sock.send_to(&q, dest).await.map_err(DnssdError::Io)?;
            }
            next_send = now + QUERY_RESEND_INTERVAL;
        }
        let wait = deadline.min(next_send).saturating_duration_since(now);
        let Ok(recv) = tokio::time::timeout(wait, sock.recv_from(&mut buf)).await else {
            continue;
        };
        let (n, _) = recv.map_err(DnssdError::Io)?;
        // 他人の壊れたデータグラムで browse を中断しない。
        let Ok(records) = parse_message(&buf[..n]) else {
            continue;
        };
        fold.fold(&records);
    }
    Ok(fold.finish())
}

/// `_matterc._udp` の全 commissionable を列挙する（spec §4.3.1）。
/// 0 件は正常（周囲に commissioning モードのデバイスが無い）。
pub async fn browse_commissionable(
    scope_id: u32,
    window: Duration,
) -> Result<Vec<CommissionableInstance>, DnssdError> {
    Ok(browse(scope_id, "_matterc._udp.local", window)
        .await?
        .iter()
        .filter_map(commissionable_from_fold)
        .collect())
}

/// `_matter._tcp` の全 operational instance を列挙する（spec §4.3）。
/// announce のみ（SRV/AAAA 未解決）の instance も addresses 空で含める。
pub async fn browse_operational(
    scope_id: u32,
    window: Duration,
) -> Result<Vec<OperationalInstance>, DnssdError> {
    Ok(browse(scope_id, "_matter._tcp.local", window)
        .await?
        .iter()
        .filter_map(operational_from_fold)
        .collect())
}

/// TXT から文字列値（key は大文字小文字無視）を取り出す。
fn txt_str<'a>(strings: &'a [Vec<u8>], key: &str) -> Option<&'a str> {
    for s in strings {
        let Ok(s) = std::str::from_utf8(s) else {
            continue;
        };
        let Some((k, v)) = s.split_once('=') else {
            continue;
        };
        if k.eq_ignore_ascii_case(key) {
            return Some(v);
        }
    }
    None
}

/// TXT `VP`（`<vendor>+<product>`、product 省略可、10 進）を分解する。
fn split_vp(vp: &str) -> (Option<u32>, Option<u32>) {
    match vp.split_once('+') {
        Some((v, p)) => (v.parse().ok(), p.parse().ok()),
        None => (vp.parse().ok(), None),
    }
}

/// SRV target（例 "HOST01.local"）→ hostname（末尾 ".local" を除去）。
fn hostname_from_target(target: &str) -> String {
    target.strip_suffix(".local").unwrap_or(target).to_string()
}

/// instance 完全名の先頭ラベル `<CFID 16hex>-<NodeId 16hex>` をパースする。
/// 形式外は None（他プロトコル / 他サービスの流れ弾）。
fn parse_operational_label(name: &str) -> Option<(String, u64)> {
    let label = name.split('.').next()?;
    let (cfid, node) = label.split_once('-')?;
    if cfid.len() != 16 || node.len() != 16 {
        return None;
    }
    if !cfid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let node_id = u64::from_str_radix(node, 16).ok()?;
    Some((cfid.to_ascii_uppercase(), node_id))
}

/// 畳み込んだ素材 → commissionable。素材ゼロ（PTR しか見えず SRV/TXT/AAAA が
/// 期限内に揃わなかった）は None（chip-tool 経路の空エントリ skip と同じ扱い）。
fn commissionable_from_fold(f: &FoldedInstance) -> Option<CommissionableInstance> {
    let discriminator = txt_u32(&f.txt, "D");
    let (vendor_id, product_id) = txt_str(&f.txt, "VP").map(split_vp).unwrap_or((None, None));
    let c = CommissionableInstance {
        hostname: f.target.as_deref().map(hostname_from_target),
        port: f.port,
        addresses: f.addresses.clone(),
        discriminator,
        vendor_id,
        product_id,
    };
    if c.hostname.is_none()
        && c.port.is_none()
        && c.addresses.is_empty()
        && c.discriminator.is_none()
        && c.vendor_id.is_none()
        && c.product_id.is_none()
    {
        return None;
    }
    Some(c)
}

/// 畳み込んだ素材 → operational。announce のみ（addresses 空）でも返す。
fn operational_from_fold(f: &FoldedInstance) -> Option<OperationalInstance> {
    let (compressed_fabric, node_id) = parse_operational_label(&f.name)?;
    Some(OperationalInstance {
        compressed_fabric,
        node_id,
        addresses: f.addresses.clone(),
    })
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

    /// commissionable browse 用の合成応答: PTR(subtype→instance) +
    /// SRV(instance→port/target) + TXT(instance) + AAAA(target への圧縮名)
    /// を 1 メッセージに詰める。`synth_response` の SRV/TXT/AAAA 部分に PTR
    /// を足した形。
    fn synth_commissionable_response(
        subtype: &str,
        instance: &str,
        target: &str,
        port: u16,
        txt: &[&str],
        addr: Ipv6Addr,
    ) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&[0, 0, 0x84, 0x00]); // id 0, QR|AA
        m.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0]); // qd 0, an 4, ns/ar 0
                                                        // --- PTR ---
        push_name(&mut m, subtype);
        m.extend_from_slice(&TYPE_PTR.to_be_bytes());
        m.extend_from_slice(&[0, 1, 0, 0, 0, 120]); // IN（PTR は cache-flush 立てないのが通例）, ttl
        let mut rdata = Vec::new();
        push_name(&mut rdata, instance);
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&rdata);
        // --- SRV ---
        push_name(&mut m, instance);
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
        push_name(&mut m, instance);
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
    fn extracts_commissionable_from_ptr_srv_txt_aaaa() {
        let addr: Ipv6Addr = "fd00::1".parse().unwrap();
        let msg = synth_commissionable_response(
            "_L3840._sub._matterc._udp.local",
            "ABCD1234._matterc._udp.local",
            "dev.local",
            5540,
            &["D=3840", "SII=5000"],
            addr,
        );
        let node = commissionable_from_response(&msg, 3840).expect("should resolve");
        assert_eq!(node.port, 5540);
        assert_eq!(node.addresses, vec![addr]);
        assert_eq!(node.session_idle_interval_ms, Some(5000));
    }

    #[test]
    fn rejects_mismatched_discriminator() {
        let addr: Ipv6Addr = "fd00::1".parse().unwrap();
        let msg = synth_commissionable_response(
            "_L3840._sub._matterc._udp.local",
            "ABCD1234._matterc._udp.local",
            "dev.local",
            5540,
            &["D=1234", "SII=5000"], // subtype は 3840 で絞れているが TXT D は不一致
            addr,
        );
        assert_eq!(commissionable_from_response(&msg, 3840), None);
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
    fn record_capacity_clamps_forged_counts() {
        // 12B ヘッダだけで an/ns/ar=65535×3 を偽装しても、メッセージ長から
        // 物理的に入り得ない分は事前確保しない（フラッド耐性）
        assert_eq!(record_capacity(196_605, 12), 0);
        // 1500B のデータグラムなら最大でも (1500-12)/11 レコード
        assert!(record_capacity(196_605, 1500) <= (1500 - 12) / 11);
        // 正直なカウントはそのまま
        assert_eq!(record_capacity(3, 1500), 3);
    }

    #[test]
    fn aaaa_fold_caps_growth_before_srv_is_known() {
        // SRV target 判明前のフラッド: 異名 AAAA を大量に受けても cap 止まり
        let mut aaaa: Vec<(String, Ipv6Addr)> = Vec::new();
        for i in 0..10_000u32 {
            let addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, i as u16);
            push_aaaa(&mut aaaa, None, format!("h{i}.local"), addr);
        }
        assert_eq!(aaaa.len(), MAX_AAAA);
    }

    #[test]
    fn aaaa_fold_dedupes() {
        let mut aaaa: Vec<(String, Ipv6Addr)> = Vec::new();
        let addr: Ipv6Addr = "fd00::1".parse().unwrap();
        push_aaaa(&mut aaaa, None, "dev.local".into(), addr);
        push_aaaa(&mut aaaa, None, "DEV.local".into(), addr); // 名前は大文字小文字非依存
        assert_eq!(aaaa.len(), 1);
    }

    #[test]
    fn aaaa_fold_filters_on_srv_target_once_known() {
        // SRV target 判明後: 不一致 AAAA は保持しない
        let mut aaaa: Vec<(String, Ipv6Addr)> = Vec::new();
        for i in 0..10_000u32 {
            let addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 1, i as u16);
            push_aaaa(&mut aaaa, Some("dev.local"), format!("evil{i}.local"), addr);
        }
        assert!(aaaa.is_empty());
        // 一致（大文字小文字非依存）は入る
        let real: Ipv6Addr = "fd00::42".parse().unwrap();
        push_aaaa(&mut aaaa, Some("dev.local"), "DEV.LOCAL".into(), real);
        assert_eq!(aaaa, vec![("DEV.LOCAL".to_string(), real)]);
    }

    #[test]
    fn aaaa_prune_frees_flooded_slots_for_the_real_target() {
        // cap がフラッドで埋まったあとに SRV が判明しても、prune で
        // 本物の AAAA が入る余地が戻る
        let mut aaaa: Vec<(String, Ipv6Addr)> = Vec::new();
        for i in 0..MAX_AAAA as u16 {
            let addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 2, i);
            push_aaaa(&mut aaaa, None, format!("junk{i}.local"), addr);
        }
        assert_eq!(aaaa.len(), MAX_AAAA);
        prune_aaaa(&mut aaaa, "dev.local");
        assert!(aaaa.is_empty());
        let real: Ipv6Addr = "fd00::99".parse().unwrap();
        push_aaaa(&mut aaaa, Some("dev.local"), "dev.local".into(), real);
        assert_eq!(aaaa.len(), 1);
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
        assert_eq!(
            node.mrp_config().initial_interval,
            Duration::from_millis(5000)
        );
        node.session_idle_interval_ms = None;
        assert_eq!(
            node.mrp_config().initial_interval,
            Duration::from_millis(500)
        );
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
        let SocketAddr::V6(a0) = addrs[0] else {
            panic!()
        };
        assert_eq!(*a0.ip(), ula);
        assert_eq!(a0.scope_id(), 0);
        assert_eq!(a0.port(), 5540);
        let SocketAddr::V6(a1) = addrs[1] else {
            panic!()
        };
        assert_eq!(*a1.ip(), ll);
        assert_eq!(a1.scope_id(), 7);
    }

    #[test]
    fn malformed_ptr_does_not_abort_datagram_parsing() {
        // 不正な圧縮ポインタを持つ PTR レコードと、有効な SRV/TXT/AAAA が
        // 同梱されたデータグラム。PTR の読み込み失敗が全体を巻き込まないことを確認。
        let addr: Ipv6Addr = "fd00::1".parse().unwrap();
        let service = "0000000000000001-0000000000000002._matter._tcp.local";
        let target = "dev.local";

        // 正常な SRV+TXT+AAAA を合成
        let mut m = Vec::new();
        m.extend_from_slice(&[0, 0, 0x84, 0x00]); // id 0, QR|AA
        m.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0]); // qd 0, an 4 (SRV+TXT+AAAA+PTR)

        // --- SRV (有効) ---
        push_name(&mut m, service);
        m.extend_from_slice(&TYPE_SRV.to_be_bytes());
        m.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 120]); // cache-flush|IN, ttl
        let mut srv_rdata = vec![0, 0, 0, 0]; // priority, weight
        srv_rdata.extend_from_slice(&5540u16.to_be_bytes());
        let mut tname = Vec::new();
        push_name(&mut tname, target);
        srv_rdata.extend_from_slice(&tname);
        m.extend_from_slice(&(srv_rdata.len() as u16).to_be_bytes());
        let target_off = m.len() + 6;
        m.extend_from_slice(&srv_rdata);

        // --- TXT (有効) ---
        push_name(&mut m, service);
        m.extend_from_slice(&TYPE_TXT.to_be_bytes());
        m.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 120]);
        let txt_str = "SII=5000";
        let mut txt_rdata = Vec::new();
        txt_rdata.push(txt_str.len() as u8);
        txt_rdata.extend_from_slice(txt_str.as_bytes());
        m.extend_from_slice(&(txt_rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&txt_rdata);

        // --- AAAA (有効な圧縮名) ---
        m.extend_from_slice(&[0xC0 | (target_off >> 8) as u8, (target_off & 0xFF) as u8]);
        m.extend_from_slice(&TYPE_AAAA.to_be_bytes());
        m.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 120]);
        m.extend_from_slice(&16u16.to_be_bytes());
        m.extend_from_slice(&addr.octets());

        // --- PTR (不正な圧縮ポインタ: 範囲外を指す) ---
        let ptr_name = "_L1234._sub._matterc._udp.local";
        push_name(&mut m, ptr_name);
        m.extend_from_slice(&TYPE_PTR.to_be_bytes());
        m.extend_from_slice(&[0, 1, 0, 0, 0, 120]); // IN, ttl
                                                    // 不正な圧縮ポインタ: バッファ外を指す (0xC0FF = offset 255 + 256 = 511)
        m.extend_from_slice(&2u16.to_be_bytes()); // rdlen = 2
        m.extend_from_slice(&[0xC0, 0xFF]); // out-of-range pointer

        // parse_message が成功し、PTR は Other として、
        // SRV/TXT/AAAA は正常に抽出されることを確認
        let records = parse_message(&m).expect("should parse despite malformed PTR");

        // レコード数は 4 (SRV, TXT, AAAA, PTR/Other)
        assert_eq!(records.len(), 4);

        // SRV を検証
        let srv = records
            .iter()
            .find(|r| matches!(r.rdata, RData::Srv { .. }))
            .expect("should have SRV");
        assert_eq!(srv.name, service);
        if let RData::Srv { port, ref target } = srv.rdata {
            assert_eq!(port, 5540);
            assert_eq!(target, "dev.local");
        } else {
            panic!("not srv");
        }

        // TXT を検証
        let txt = records
            .iter()
            .find(|r| matches!(r.rdata, RData::Txt(_)))
            .expect("should have TXT");
        assert_eq!(txt.name, service);
        if let RData::Txt(ref strings) = txt.rdata {
            assert_eq!(txt_u32(strings, "SII"), Some(5000));
        } else {
            panic!("not txt");
        }

        // AAAA を検証
        let aaaa = records
            .iter()
            .find(|r| matches!(r.rdata, RData::Aaaa(_)))
            .expect("should have AAAA");
        assert_eq!(aaaa.name, "dev.local");
        if let RData::Aaaa(got) = aaaa.rdata {
            assert_eq!(got, addr);
        } else {
            panic!("not aaaa");
        }

        // PTR は Other として保存される（名前は読めたが、読み込みに失敗）
        let ptr = records
            .iter()
            .find(|r| r.name == ptr_name)
            .expect("should have PTR record");
        assert!(matches!(ptr.rdata, RData::Other));
    }

    /// browse 用の合成応答: PTR(service→instance) + SRV/TXT/AAAA を 1 メッセージに
    /// 詰める（additional 同梱の行儀良い responder 相当）。`records` で個別に
    /// 抜き差しできるよう、載せるレコード種を引数で選ぶ。
    #[allow(clippy::too_many_arguments)]
    fn synth_browse_response(
        service: &str,
        instance: &str,
        with_srv: Option<(u16, &str)>,
        with_txt: Option<&[&str]>,
        with_aaaa: Option<(&str, Ipv6Addr)>,
    ) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(&0u16.to_be_bytes()); // id
        msg.extend_from_slice(&0x8400u16.to_be_bytes()); // QR|AA
        msg.extend_from_slice(&0u16.to_be_bytes()); // qd
        let mut count: u16 = 1; // PTR
        if with_srv.is_some() {
            count += 1;
        }
        if with_txt.is_some() {
            count += 1;
        }
        if with_aaaa.is_some() {
            count += 1;
        }
        msg.extend_from_slice(&count.to_be_bytes()); // an
        msg.extend_from_slice(&[0, 0, 0, 0]); // ns/ar
                                              // PTR: service -> instance
        push_name(&mut msg, service);
        msg.extend_from_slice(&TYPE_PTR.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());
        msg.extend_from_slice(&[0, 0, 0, 120]);
        let mut ptr_rdata = Vec::new();
        push_name(&mut ptr_rdata, instance);
        msg.extend_from_slice(&(ptr_rdata.len() as u16).to_be_bytes());
        msg.extend_from_slice(&ptr_rdata);
        if let Some((port, target)) = with_srv {
            push_name(&mut msg, instance);
            msg.extend_from_slice(&TYPE_SRV.to_be_bytes());
            msg.extend_from_slice(&CLASS_IN.to_be_bytes());
            msg.extend_from_slice(&[0, 0, 0, 120]);
            let mut rdata = vec![0, 0, 0, 0]; // priority/weight
            rdata.extend_from_slice(&port.to_be_bytes());
            push_name(&mut rdata, target);
            msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            msg.extend_from_slice(&rdata);
        }
        if let Some(strings) = with_txt {
            push_name(&mut msg, instance);
            msg.extend_from_slice(&TYPE_TXT.to_be_bytes());
            msg.extend_from_slice(&CLASS_IN.to_be_bytes());
            msg.extend_from_slice(&[0, 0, 0, 120]);
            let mut rdata = Vec::new();
            for s in strings {
                rdata.push(s.len() as u8);
                rdata.extend_from_slice(s.as_bytes());
            }
            msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            msg.extend_from_slice(&rdata);
        }
        if let Some((host, addr)) = with_aaaa {
            push_name(&mut msg, host);
            msg.extend_from_slice(&TYPE_AAAA.to_be_bytes());
            msg.extend_from_slice(&CLASS_IN.to_be_bytes());
            msg.extend_from_slice(&[0, 0, 0, 120]);
            msg.extend_from_slice(&16u16.to_be_bytes());
            msg.extend_from_slice(&addr.octets());
        }
        msg
    }

    const MC: &str = "_matterc._udp.local";
    const MO: &str = "_matter._tcp.local";

    #[test]
    fn browse_fold_collects_two_instances_from_bundled_responses() {
        let a1: Ipv6Addr = "fd00::1".parse().unwrap();
        let a2: Ipv6Addr = "fd00::2".parse().unwrap();
        let d1 = synth_browse_response(
            MC,
            &format!("INST1.{MC}"),
            Some((5540, "h1.local")),
            Some(&["D=3840", "VP=65521+32768"]),
            Some(("h1.local", a1)),
        );
        let d2 = synth_browse_response(
            MC,
            &format!("INST2.{MC}"),
            Some((5541, "h2.local")),
            Some(&["D=100"]),
            Some(("h2.local", a2)),
        );
        let mut fold = BrowseFold::new(MC);
        fold.fold(&parse_message(&d1).unwrap());
        fold.fold(&parse_message(&d2).unwrap());
        let out = fold.finish();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].port, Some(5540));
        assert_eq!(out[0].addresses, vec![a1]);
        assert_eq!(out[1].port, Some(5541));
        assert_eq!(out[1].addresses, vec![a2]);
    }

    #[test]
    fn browse_fold_is_order_independent_within_a_datagram() {
        // SRV/TXT/AAAA が PTR より前に並んでいても畳み込める（fold は 2 パス）。
        // synth は PTR を先頭に置くので、parse 結果を並べ替えて食わせる。
        let a1: Ipv6Addr = "fd00::1".parse().unwrap();
        let d = synth_browse_response(
            MC,
            &format!("INST1.{MC}"),
            Some((5540, "h1.local")),
            Some(&["D=1"]),
            Some(("h1.local", a1)),
        );
        let mut records = parse_message(&d).unwrap();
        records.reverse(); // PTR が最後
        let mut fold = BrowseFold::new(MC);
        fold.fold(&records);
        let out = fold.finish();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].port, Some(5540));
        assert_eq!(out[0].addresses, vec![a1]);
    }

    #[test]
    fn browse_fold_dedupes_instances_and_caps_growth() {
        let mut fold = BrowseFold::new(MC);
        let d = synth_browse_response(MC, &format!("INST1.{MC}"), None, None, None);
        fold.fold(&parse_message(&d).unwrap());
        fold.fold(&parse_message(&d).unwrap()); // 同じ PTR を 2 回
        assert_eq!(fold.instances.len(), 1);
        for i in 0..(MAX_INSTANCES + 5) {
            let d = synth_browse_response(MC, &format!("X{i}.{MC}"), None, None, None);
            fold.fold(&parse_message(&d).unwrap());
        }
        assert_eq!(fold.instances.len(), MAX_INSTANCES);
    }

    #[test]
    fn browse_fold_ignores_records_for_other_services() {
        // 同じ網に有線 LAN プリンタ等がいても混ざらない。
        let mut fold = BrowseFold::new(MC);
        let d = synth_browse_response(
            "_ipp._tcp.local",
            "printer._ipp._tcp.local",
            Some((631, "printer.local")),
            None,
            None,
        );
        fold.fold(&parse_message(&d).unwrap());
        assert!(fold.instances.is_empty());
    }

    #[test]
    fn browse_finish_sorts_link_local_after_global_through_fold() {
        // AAAA を link-local → global の順で食わせても、finish() は
        // 非 link-local 優先で返す（--probe の live_address は先頭を使う）。
        let ll: Ipv6Addr = "fe80::10".parse().unwrap();
        let global: Ipv6Addr = "fd00::10".parse().unwrap();
        let mut fold = BrowseFold::new(MC);
        let d1 = synth_browse_response(
            MC,
            &format!("INST1.{MC}"),
            Some((5540, "h1.local")),
            Some(&["D=1"]),
            Some(("h1.local", ll)),
        );
        let d2 = synth_browse_response(
            MC,
            &format!("INST1.{MC}"),
            Some((5540, "h1.local")),
            Some(&["D=1"]),
            Some(("h1.local", global)),
        );
        fold.fold(&parse_message(&d1).unwrap());
        fold.fold(&parse_message(&d2).unwrap());
        let out = fold.finish();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].addresses, vec![global, ll]);
    }

    #[test]
    fn browse_pending_questions_lists_missing_srv_txt_aaaa() {
        let mut fold = BrowseFold::new(MC);
        // PTR のみ → SRV と TXT を要求。
        let d = synth_browse_response(MC, &format!("INST1.{MC}"), None, None, None);
        fold.fold(&parse_message(&d).unwrap());
        let q = fold.pending_questions();
        assert!(q.contains(&(format!("INST1.{MC}"), TYPE_SRV)));
        assert!(q.contains(&(format!("INST1.{MC}"), TYPE_TXT)));
        // SRV が来たら target の AAAA を要求（プールにまだ無い）。
        let d = synth_browse_response(
            MC,
            &format!("INST1.{MC}"),
            Some((5540, "h1.local")),
            Some(&["D=1"]),
            None,
        );
        fold.fold(&parse_message(&d).unwrap());
        let q = fold.pending_questions();
        assert!(q.contains(&("h1.local".to_string(), TYPE_AAAA)));
        assert!(!q.iter().any(|(_, t)| *t == TYPE_SRV));
    }

    #[test]
    fn commissionable_from_fold_parses_txt_hostname_and_sorts_addresses() {
        let global: Ipv6Addr = "fd00::10".parse().unwrap();
        let ll: Ipv6Addr = "fe80::10".parse().unwrap();
        let f = FoldedInstance {
            name: format!("INST1.{MC}"),
            port: Some(5540),
            target: Some("HOST01.local".to_string()),
            txt: vec![b"D=3840".to_vec(), b"VP=65521+32768".to_vec()],
            addresses: vec![global, ll],
        };
        let c = commissionable_from_fold(&f).unwrap();
        assert_eq!(c.hostname.as_deref(), Some("HOST01"));
        assert_eq!(c.port, Some(5540));
        assert_eq!(c.discriminator, Some(3840));
        assert_eq!(c.vendor_id, Some(65521));
        assert_eq!(c.product_id, Some(32768));
        assert_eq!(c.addresses, vec![global, ll]);
    }

    #[test]
    fn commissionable_from_fold_accepts_vendor_only_vp_and_skips_empty() {
        let f = FoldedInstance {
            name: format!("INST1.{MC}"),
            port: None,
            target: None,
            txt: vec![b"VP=65521".to_vec()],
            addresses: vec![],
        };
        let c = commissionable_from_fold(&f).unwrap();
        assert_eq!(c.vendor_id, Some(65521));
        assert_eq!(c.product_id, None);
        // 素材ゼロ（PTR しか見えなかった instance）は出さない。
        let empty = FoldedInstance {
            name: format!("INST2.{MC}"),
            port: None,
            target: None,
            txt: vec![],
            addresses: vec![],
        };
        assert!(commissionable_from_fold(&empty).is_none());
    }

    #[test]
    fn operational_from_fold_parses_label_and_keeps_announce_only() {
        let f = FoldedInstance {
            name: format!("00AABB1122CC3344-000000000000000B.{MO}"),
            port: Some(5540),
            target: None,
            txt: vec![],
            addresses: vec![],
        };
        let o = operational_from_fold(&f).unwrap();
        assert_eq!(o.compressed_fabric, "00AABB1122CC3344");
        assert_eq!(o.node_id, 0x0B);
        assert!(o.addresses.is_empty()); // announce のみ → 空で返す（skip しない）
    }

    #[test]
    fn operational_from_fold_rejects_malformed_labels() {
        for bad in [
            format!("shortname.{MO}"),
            format!("GGGGBB1122CC3344-000000000000000B.{MO}"), // 非 hex
            format!("00AABB1122CC3344.{MO}"),                  // '-' 無し
            format!("00AABB1122CC3344-0B.{MO}"),               // 桁不足
        ] {
            let f = FoldedInstance {
                name: bad,
                port: None,
                target: None,
                txt: vec![],
                addresses: vec![],
            };
            assert!(operational_from_fold(&f).is_none());
        }
    }

    #[test]
    fn record_ttl_is_parsed() {
        // synth_browse_response uses TTL 120 (bytes [0,0,0,120]) — fold の
        // known_answers() 経由で PTR レコードの ttl が取り出せることを確認。
        let d = synth_browse_response(MC, &format!("INST1.{MC}"), None, None, None);
        let mut fold = BrowseFold::new(MC);
        fold.fold(&parse_message(&d).unwrap());
        assert_eq!(fold.known_answers(), vec![(format!("INST1.{MC}"), 120)]);
    }

    #[test]
    fn known_answer_query_degenerates_without_known() {
        let pkts = encode_ptr_query_with_known(MC, &[]);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0], encode_query(0, &[(MC, TYPE_PTR)]));
    }

    #[test]
    fn known_answer_query_roundtrips_through_parser() {
        // KA 2 件入りクエリを自前 parse_message で読み戻し、answer の PTR が
        // 完全名で復元される（圧縮ポインタの検証）。
        let known = vec![
            (format!("INST1.{MC}"), 120u32),
            (format!("INST2.{MC}"), 99u32),
        ];
        let pkts = encode_ptr_query_with_known(MC, &known);
        assert_eq!(pkts.len(), 1);
        let records = parse_message(&pkts[0]).unwrap();
        let ptrs: Vec<_> = records
            .iter()
            .filter_map(|r| match &r.rdata {
                RData::Ptr(n) if r.name.eq_ignore_ascii_case(MC) => Some((n.clone(), r.ttl)),
                _ => None,
            })
            .collect();
        assert_eq!(ptrs.len(), 2);
        assert_eq!(ptrs[0], (format!("INST1.{MC}"), 120));
        assert_eq!(ptrs[1], (format!("INST2.{MC}"), 99));
    }

    #[test]
    fn known_answer_query_splits_and_sets_tc() {
        // 1400B を超える KA（長いラベルで水増し）が複数パケットに割れ、
        // 最後以外に TC が立ち、全 KA が失われず分配される。
        let known: Vec<(String, u32)> = (0..60)
            .map(|i| (format!("INSTANCE-{i:04}-{}.{MC}", "X".repeat(20)), 120))
            .collect();
        let pkts = encode_ptr_query_with_known(MC, &known);
        assert!(pkts.len() >= 2);
        for p in &pkts {
            assert!(p.len() <= 1400);
        }
        for p in &pkts[..pkts.len() - 1] {
            assert_eq!(
                u16::from_be_bytes([p[2], p[3]]) & 0x0200,
                0x0200,
                "TC on non-last"
            );
        }
        let last = pkts.last().unwrap();
        assert_eq!(u16::from_be_bytes([last[2], last[3]]) & 0x0200, 0);
        let total: usize = pkts
            .iter()
            .map(|p| {
                parse_message(p)
                    .unwrap()
                    .iter()
                    .filter(|r| matches!(r.rdata, RData::Ptr(_)))
                    .count()
            })
            .sum();
        assert_eq!(total, 60);
    }
}
