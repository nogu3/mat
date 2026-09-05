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

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::exchange::MrpConfig;

mod cache;
mod codec;
mod resolve;
#[cfg(test)]
mod test_util;
pub use cache::*;
pub use resolve::*;

use codec::{
    encode_ptr_query_with_known, encode_query, parse_message, txt_str, txt_u32, RData, Record,
};

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

/// Binds the one-shot mDNS query socket used by the resolvers below.
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

/// finish() が返す、サービス種別に依存しない 1 instance 分の素材。
/// instance の完全名は commissionable では不要（operational 側の label 解析
/// —`parse_operational_label`— が M8b で撤去され、この構造体の唯一の消費者
/// だった）ため持たない。
struct FoldedInstance {
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
            .filter(|(_, f)| f.ttl != 0) // TTL 0 は goodbye — KA に載せない（RFC 6762）。
            .map(|(name, f)| (name.clone(), f.ttl))
            .collect()
    }

    fn finish(self) -> Vec<FoldedInstance> {
        let pool = self.aaaa;
        self.instances
            .into_iter()
            .map(|(_name, f)| {
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
    let sock = bind_mdns_socket(scope_id).map_err(DnssdError::Io)?;
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

#[cfg(test)]
mod tests {
    use super::codec::push_name;
    use super::test_util::{
        multicast_ifaces, spawn_multicast_announcer, synth_commissionable_response, MC,
    };
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

    /// browse（discover の commissionable 列挙）も同じくマルチキャストのみの
    /// 広告を受信できること。resolve_commissionable と同じ回帰のピン留め。
    #[tokio::test]
    async fn browse_receives_multicast_only_announcement() {
        let msg = synth_commissionable_response(
            "_matterc._udp.local",
            "MCASTONLY-BR._matterc._udp.local",
            "mcastonly-br.local",
            5541,
            &["D=2990"],
            "fd00::2990".parse().unwrap(),
        );
        let mut tried = Vec::new();
        for (name, idx) in multicast_ifaces() {
            let Ok(announcer) = spawn_multicast_announcer(idx, msg.clone()) else {
                tried.push(format!("{name}(idx={idx}): responder bind failed"));
                continue;
            };
            let res = browse_commissionable(idx, Duration::from_millis(1200)).await;
            announcer.abort();
            match res {
                Ok(list) if list.iter().any(|c| c.discriminator == Some(2990)) => return,
                Ok(list) => tried.push(format!(
                    "{name}(idx={idx}): announcement not seen ({} unrelated instances)",
                    list.len()
                )),
                Err(e) => tried.push(format!("{name}(idx={idx}): {e:?}")),
            }
        }
        panic!(
            "no multicast-capable interface delivered the multicast-only \
             commissionable announcement to browse (lo excluded — it lacks \
             IFF_MULTICAST on Linux); tried: {tried:?}"
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
            port: None,
            target: None,
            txt: vec![],
            addresses: vec![],
        };
        assert!(commissionable_from_fold(&empty).is_none());
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
}
