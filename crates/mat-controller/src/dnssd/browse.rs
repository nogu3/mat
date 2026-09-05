//! One-shot browse（M8b: `discover` の native 化）: `_matterc._udp` の PTR を
//! 列挙し、instance ごとに SRV/TXT/AAAA を固定窓（[`BROWSE_WINDOW`]）まで
//! 畳み込む。early return しない。Known-Answer 抑制で TC 切り捨て応答を回避
//! （実機 2026-07 の 29+ instance 観測）。operational の到達性判定は browse
//! ではなく `resolve_operational` の targeted resolve（mod.rs の doc 参照）。

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::time::Instant;

use super::codec::{
    encode_ptr_query_with_known, encode_query, parse_message, txt_str, txt_u32, RData, Record,
};
use super::{
    bind_mdns_socket, is_link_local, DnssdError, MDNS_GROUP, MDNS_PORT, QUERY_RESEND_INTERVAL,
    TYPE_AAAA, TYPE_SRV, TYPE_TXT,
};

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
    use super::super::codec::push_name;
    use super::super::test_util::{
        multicast_ifaces, spawn_multicast_announcer, synth_commissionable_response, MC,
    };
    use super::super::{CLASS_IN, TYPE_PTR};
    use super::*;

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
