//! Minimal one-shot mDNS/DNS-SD resolver for Matter operational services
//! (Matter spec §4.3; RFC 6762; RFC 2782 SRV).
//!
//! Scope: resolve one `<CompressedFabricId>-<NodeId>._matter._tcp.local`
//! instance to IPv6 addresses + port + MRP intervals (TXT `SII`/`SAI`).
//! No advertising, no cache: bind `[::]:5353`, join `ff02::fb`, and query with
//! the QU (unicast-response) bit set — then fold both the unicast replies and
//! the multicast answers a responder may send to the group instead (see
//! [`bind_mdns_socket`] and [`QU_CLASS_IN`]). Fold responses until
//! SRV + at least one AAAA for its target are in hand. TXT is folded when
//! it arrives in the same responses but is not waited for — MRP falls back
//! to the spec default interval without it.
//!
//! The resident cache `matd` uses (`OperationalCache`) lives in the `cache`
//! submodule; the one-shot resolvers here never touch it.
//!
//! M8b adds one-shot browse (`browse_commissionable`): same legacy unicast
//! transport, but enumerating PTR answers for a whole service type and
//! folding SRV/TXT/AAAA per instance until a fixed window ([`BROWSE_WINDOW`])
//! expires — no early return, still no cache. Commissionable discovery is
//! inherently enumeration (unknown targets), so browse stays there.
//! Operational *reachability* (the mDNS probe) is NOT enumeration-based:
//! real-mesh wire evidence (2026-07-17) showed advertising proxies simply
//! not answering `_matter._tcp` PTR enumeration for some registered
//! instances (even after known-answer suppression), while targeted resolve
//! of those same instances (`resolve_operational`, the path CASE already
//! uses) succeeds. So the probe now runs concurrent `resolve_operational`
//! calls against the ledger's known (CompressedFabricId, NodeId) pairs
//! instead of browsing `_matter._tcp` and matching results — see
//! `crates/mat/src/probe.rs`. `browse_operational` was removed accordingly.
//!
//! Layout: `codec` (wire encode/decode, no sockets) · `resolve` (one-shot
//! operational / commissionable resolve with early return) · `browse`
//! (fixed-window `_matterc._udp` enumeration) · `cache` (`matd`'s resident
//! `OperationalCache`). This file keeps the shared constants, `DnssdError`,
//! `ResolvedNode`, and the 5353-bound query socket (`bind_mdns_socket`).

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::exchange::MrpConfig;

mod browse;
mod cache;
mod codec;
mod resolve;
#[cfg(test)]
mod test_util;
pub use browse::*;
pub use cache::*;
pub use resolve::*;

const MDNS_GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_PORT: u16 = 5353;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 0x0001;
/// QU (unicast-response) bit — top bit of the qclass field (RFC 6762 §5.4).
/// Set on every question we send: this resolver is a one-shot querier bound to
/// an ephemeral port (not 5353) and never joins the ff02::fb multicast group,
/// so it can only receive *unicast* replies. Without QU, real responders (an
/// OTBR mDNS advertising proxy, observed on the wire 2026-07-19) answer a QM
/// query via multicast to ff02::fb, which our ephemeral socket never sees —
/// the resolve then times out even though avahi/chip-tool (which join the
/// group and set QU) resolve the same instance. QU explicitly requests the
/// unicast reply this design already assumes.
const QU_CLASS_IN: u16 = 0x8000 | CLASS_IN;
/// Matter spec §4.12.8: SESSION_IDLE_INTERVAL default and ceiling (ms).
const MRP_DEFAULT_IDLE_MS: u32 = 500;
/// Matter 既定の SESSION_ACTIVE_INTERVAL (spec 4.12.8)。TXT に SAI が無い
/// デバイス向けのフォールバック。
const MRP_DEFAULT_ACTIVE_MS: u32 = 300;
const MRP_MAX_INTERVAL_MS: u32 = 3_600_000;
const QUERY_RESEND_INTERVAL: Duration = Duration::from_secs(1);
/// CASE 前 targeted resolve（[`resolve_operational`]）の共通窓。establish
/// （mat-native）と probe（mat の `discover --probe` / `diag node --deep`）が
/// 共有し、「CASE が届く範囲 = probe が reachable と言う範囲」を定義上
/// 一致させる。監査⑩: probe 独自の 3s 窓が establish の 8s と乖離し、
/// Thread メッシュ + advertising proxy 経由で resolve に 3〜8 秒かかる
/// 健全ノードを `reachable:false` と誤報していた。
pub const OPERATIONAL_RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);

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

/// Binds the one-shot mDNS query socket used by the `resolve` / `browse` /
/// `cache` submodules (and `test_util`'s responders).
///
/// It binds `[::]:5353` (the mDNS port) with address/port reuse and joins the
/// `ff02::fb` group on the query interface. This is the crucial difference
/// from a naive ephemeral-port querier: a real responder — an OTBR mDNS
/// advertising proxy for Thread nodes, captured on the wire 2026-07-19 —
/// answers our targeted query by **multicasting** the SRV/AAAA to `ff02::fb`
/// even when we set the QU (unicast-response) bit, which RFC 6762 §5.4
/// permits. A socket bound to an ephemeral port and not joined to the group
/// never receives those answers, so the resolve times out even though avahi /
/// chip-tool (which bind 5353 and join the group) resolve the same instance.
/// `SO_REUSEADDR` (not `SO_REUSEPORT`) lets us coexist with a system mDNS
/// daemon (e.g. avahi) that already holds 5353: with `SO_REUSEADDR` a multicast
/// datagram is delivered to *every* socket bound to the port that joined the
/// group, so we and avahi both get a copy. `SO_REUSEPORT` must NOT be used
/// here — it puts the sockets in a load-balancing group that hashes *each*
/// incoming datagram (multicast included) to a single member, so the
/// responder's multicast answer lands on avahi at random and our resolve flakes
/// (observed intermittently across all nodes, 2026-07-19). Unicast delivery
/// to the shared port reaches only ONE bound socket (the most recently
/// bound), and a real responder — avahi acting as the SRP advertising
/// proxy, captured on the wire 2026-08-05 — honors QU and answers by
/// unicast. Concurrent resolvers must therefore share one socket
/// ([`resolve_operational_many`]); per-node sockets silently lose answers
/// to whichever socket bound last (audit ⑩'s real mechanism).
///
/// Outgoing multicast is pinned to the interface — `sin6_scope_id` alone can
/// leak a datagram out a VPN interface (see `transport.rs`) — with hop limit
/// 255 (RFC 6762 §11 requires it; the OS default of 1 is also off-spec).
/// Still one-shot: the caller drops the socket when the resolve returns, so no
/// state is held between runs (design rule 4).
fn bind_mdns_socket(scope_id: u32) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.set_only_v6(true)?;
    sock.set_nonblocking(true)?;
    let bind = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, MDNS_PORT, 0, 0);
    sock.bind(&SocketAddr::V6(bind).into())?;
    sock.join_multicast_v6(&MDNS_GROUP, scope_id)?;
    sock.set_multicast_if_v6(scope_id)?;
    sock.set_multicast_hops_v6(255)?;
    UdpSocket::from_std(sock.into())
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
    /// The *active* interval (SAI) is carried alongside: retransmits while
    /// the peer is provably active (recent rx) use it instead of SII
    /// (spec 4.12.8) — Thread devices advertise SII=5000ms and using that
    /// mid-exchange loses races against the peer's response timeouts.
    pub fn mrp_config(&self) -> MrpConfig {
        let idle_ms = self
            .session_idle_interval_ms
            .unwrap_or(MRP_DEFAULT_IDLE_MS)
            .clamp(1, MRP_MAX_INTERVAL_MS);
        let active_ms = self
            .session_active_interval_ms
            .unwrap_or(MRP_DEFAULT_ACTIVE_MS)
            .clamp(1, MRP_MAX_INTERVAL_MS);
        MrpConfig {
            initial_interval: Duration::from_millis(u64::from(idle_ms)),
            active_interval: Duration::from_millis(u64::from(active_ms)),
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

    /// active_interval は TXT の SAI から取り、無ければ Matter 既定 300ms。
    /// SII=5000 の Thread デバイスに対して active 中の再送が 5 秒張り付く
    /// 実機バグ（購読 priming がデバイス側タイムアウトに負けて 0x80 死）の釘。
    #[test]
    fn mrp_config_uses_sai_for_active_interval() {
        let mut node = ResolvedNode {
            port: 5540,
            addresses: vec![],
            session_idle_interval_ms: Some(5000),
            session_active_interval_ms: Some(300),
        };
        assert_eq!(
            node.mrp_config().active_interval,
            Duration::from_millis(300)
        );
        node.session_active_interval_ms = None;
        assert_eq!(
            node.mrp_config().active_interval,
            Duration::from_millis(300) // Matter 既定 SESSION_ACTIVE_INTERVAL
        );
        node.session_active_interval_ms = Some(999_999_999);
        assert_eq!(
            node.mrp_config().active_interval,
            Duration::from_millis(3_600_000)
        );
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
