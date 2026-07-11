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
}
