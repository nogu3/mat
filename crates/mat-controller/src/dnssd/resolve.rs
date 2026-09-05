//! One-shot 解決: operational（`<CFID>-<NodeId>._matter._tcp`、複数ノードを
//! 1 ソケットで demux する `resolve_operational_many`）と commissionable
//! （`_L<disc>._sub._matterc._udp` の PTR → SRV/TXT/AAAA）。SRV + target
//! 一致 AAAA が揃った時点で早期 return する（browse と違い全員は集めない）。

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::time::Instant;

use super::codec::{encode_query, parse_message, prune_aaaa, push_aaaa, txt_u32, RData};
use super::{
    bind_mdns_socket, is_link_local, operational_instance, DnssdError, ResolvedNode, MDNS_GROUP,
    MDNS_PORT, QUERY_RESEND_INTERVAL, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT,
};

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
/// advertising proxy — production pcap, 2026-08-05) answers by unicast, and a
/// unicast datagram to the shared port 5353 is delivered to only ONE bound
/// socket. With per-node sockets (the pre-1.21.0 probe) every answer lands
/// on an arbitrary socket whose per-instance filter silently discards it
/// (audit ⑩'s real mechanism). Queries for unresolved instances are resent
/// every second until `timeout`.
///
/// The outer `Err` is a socket-level I/O failure (bind/send — the whole
/// batch is unresolvable, e.g. an interface without multicast). Per-node
/// results are `Ok(ResolvedNode)` or `Err(Timeout)`, in `node_ids` order.
/// Duplicate entries in `node_ids` are tolerated but resolve one resend tick
/// (~1s) apart — each answer feeds only the first unresolved matching query.
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
                let msg = encode_query(0, &[(&q.service, TYPE_SRV), (&q.service, TYPE_TXT)]);
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
                    if let Some(q) = queries
                        .iter_mut()
                        .find(|q| q.resolved.is_none() && r.name.eq_ignore_ascii_case(&q.service))
                    {
                        prune_aaaa(&mut q.aaaa, &target);
                        q.srv = Some((port, target));
                    }
                }
                RData::Txt(strings) => {
                    if let Some(q) = queries
                        .iter_mut()
                        .find(|q| q.resolved.is_none() && r.name.eq_ignore_ascii_case(&q.service))
                    {
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
                Err(DnssdError::Timeout {
                    instance: q.service,
                }),
            ),
        })
        .collect())
}

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
    let sock = bind_mdns_socket(scope_id).map_err(DnssdError::Io)?;
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

#[cfg(test)]
mod tests {
    use super::super::test_util::{
        multicast_ifaces, spawn_multicast_announcer, spawn_unicast_responder,
        synth_commissionable_response, synth_response,
    };
    use super::*;

    /// resolve_commissionable が、マルチキャストでしか応答しない responder
    /// （実機 OTBR proxy と同型）の commissionable 広告を受信できること。
    /// resolver が ephemeral ソケット（5353 非 bind・ff02::fb 未 join）だと
    /// この応答は構造的に受信不能で必ず timeout する — ゲート2 検証3
    /// （cross-fabric commission）が実機で解決不能になった回帰のピン留め。
    #[tokio::test]
    async fn resolve_commissionable_receives_multicast_only_response() {
        let msg = synth_commissionable_response(
            "_L2989._sub._matterc._udp.local",
            "MCASTONLY-RC._matterc._udp.local",
            "mcastonly-rc.local",
            5540,
            &["D=2989"],
            "fd00::2989".parse().unwrap(),
        );
        let mut tried = Vec::new();
        for (name, idx) in multicast_ifaces() {
            let Ok(announcer) = spawn_multicast_announcer(idx, msg.clone()) else {
                tried.push(format!("{name}(idx={idx}): responder bind failed"));
                continue;
            };
            let res = resolve_commissionable(idx, 2989, Duration::from_millis(1500)).await;
            announcer.abort();
            match res {
                Ok(node) => {
                    assert_eq!(node.port, 5540);
                    assert_eq!(
                        node.addresses,
                        vec!["fd00::2989".parse::<Ipv6Addr>().unwrap()]
                    );
                    return; // 最初に届いた iface で十分 — PASS。
                }
                Err(e) => tried.push(format!("{name}(idx={idx}): {e:?}")),
            }
        }
        panic!(
            "no multicast-capable interface delivered the multicast-only \
             commissionable answer to resolve_commissionable (lo excluded — \
             it lacks IFF_MULTICAST on Linux); tried: {tried:?}"
        );
    }

    /// resolve_operational（CASE 前の targeted resolve）も、マルチキャストで
    /// しか応答しない responder（実機 OTBR proxy と同型）の広告を受信できる
    /// こと。browse / resolve_commissionable と同じ回帰（QU bit 層1、sibling
    /// 関数の適用漏れ = 0.23.1 の教訓）のピン留め — これで 3 兄弟が対称になる。
    #[tokio::test]
    async fn resolve_operational_receives_multicast_only_response() {
        let cfid: [u8; 8] = 0xAB7D_E088_02E0_CD54u64.to_be_bytes();
        let node_id: u64 = 5;
        let service = format!(
            "{}._matter._tcp.local",
            operational_instance(&cfid, node_id)
        );
        let msg = synth_response(
            &service,
            "mcastonly-op.local",
            5540,
            &["SII=5000"],
            "fd00::5".parse().unwrap(),
        );
        let mut tried = Vec::new();
        for (name, idx) in multicast_ifaces() {
            let Ok(announcer) = spawn_multicast_announcer(idx, msg.clone()) else {
                tried.push(format!("{name}(idx={idx}): responder bind failed"));
                continue;
            };
            let res = resolve_operational(idx, &cfid, node_id, Duration::from_millis(1500)).await;
            announcer.abort();
            match res {
                Ok(node) => {
                    assert_eq!(node.port, 5540);
                    assert_eq!(node.addresses, vec!["fd00::5".parse::<Ipv6Addr>().unwrap()]);
                    return; // 最初に届いた iface で十分 — PASS。
                }
                Err(e) => tried.push(format!("{name}(idx={idx}): {e:?}")),
            }
        }
        panic!(
            "no multicast-capable interface delivered the multicast-only \
             operational answer to resolve_operational (lo excluded — it lacks \
             IFF_MULTICAST on Linux); tried: {tried:?}"
        );
    }

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
                let service = format!("{}._matter._tcp.local", operational_instance(&cfid, id));
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
            let res =
                resolve_operational_many(idx, &cfid, &[5, 6, silent], Duration::from_millis(1500))
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
            "no multicast-capable interface delivered the unicast-only \
             answers to resolve_operational_many; tried: {tried:?}"
        );
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
}
