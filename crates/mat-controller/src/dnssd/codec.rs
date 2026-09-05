//! DNS ワイヤコーデック（RFC 1035 / RFC 6762）: 質問の符号化（QU ビット、
//! Known-Answer 分割）と応答の復号（名前圧縮、SRV/TXT/AAAA/PTR の `Record`）、
//! AAAA 候補プールの上限付き fold、TXT の `key=value` 取り出し。
//! ソケットは持たない純粋関数群 — `resolve` / `browse` / `cache` が共有する。

use std::net::Ipv6Addr;

use super::{DnssdError, CLASS_IN, QU_CLASS_IN, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT};

/// Appends `name` in DNS label form (RFC 1035 §3.1). Our names are fixed
/// service/host names, so an oversized label is a caller bug.
pub(super) fn push_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.') {
        debug_assert!(!label.is_empty() && label.len() <= 63, "bad dns label");
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

/// One DNS query message (standard query, class IN) with the given
/// (name, qtype) questions. mDNS conventionally uses id 0.
pub(super) fn encode_query(id: u16, questions: &[(&str, u16)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0, 0]); // flags
    out.extend_from_slice(&(questions.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // an/ns/ar counts
    for (name, qtype) in questions {
        push_name(&mut out, name);
        out.extend_from_slice(&qtype.to_be_bytes());
        out.extend_from_slice(&QU_CLASS_IN.to_be_bytes());
    }
    out
}

/// Byte budget per packet for [`encode_ptr_query_with_known`] (comfortably
/// under a typical path MTU; real responders truncated at ~1428B — see the
/// `browse`'s module doc's known-answer-suppression note).
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
pub(super) fn encode_ptr_query_with_known(service: &str, known: &[(String, u32)]) -> Vec<Vec<u8>> {
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

pub(super) enum RData {
    Ptr(String),
    Srv { port: u16, target: String },
    Txt(Vec<Vec<u8>>),
    Aaaa(Ipv6Addr),
    Other,
}

pub(super) struct Record {
    pub(super) name: String,
    pub(super) rdata: RData,
    pub(super) ttl: u32,
    /// RFC 6762 §10.2 の cache-flush ビット（class フィールド最上位）。class
    /// 自体の検証は従来通りしない（mDNS は IN-only）。
    pub(super) cache_flush: bool,
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
pub(super) fn push_aaaa(
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
pub(super) fn prune_aaaa(aaaa: &mut Vec<(String, Ipv6Addr)>, target: &str) {
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
/// Record classes are not validated (mDNS is IN-only); only the RFC 6762
/// cache-flush bit (top bit of the class field) is surfaced on each record.
pub(super) fn parse_message(buf: &[u8]) -> Result<Vec<Record>, DnssdError> {
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
        let cache_flush = be16(buf, p + 2)? & 0x8000 != 0;
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
        records.push(Record {
            name,
            rdata,
            ttl,
            cache_flush,
        });
        pos = rdata_pos + rdlen;
    }
    Ok(records)
}

/// Extracts a decimal `key=value` (case-insensitive key) from TXT strings.
pub(super) fn txt_u32(strings: &[Vec<u8>], key: &str) -> Option<u32> {
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

/// TXT から文字列値（key は大文字小文字無視）を取り出す。
pub(super) fn txt_str<'a>(strings: &'a [Vec<u8>], key: &str) -> Option<&'a str> {
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

#[cfg(test)]
mod tests {
    use super::super::test_util::{synth_aaaa_class, synth_response, MC};
    use super::super::CLASS_IN;
    use super::*;

    #[test]
    fn encodes_srv_query() {
        let q = encode_query(0, &[("a.local", TYPE_SRV)]);
        assert_eq!(
            q,
            [
                0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, // header: id 0, 1 question
                1, b'a', 5, b'l', b'o', b'c', b'a', b'l', 0, // qname a.local
                0, 33, 0x80, 1, // SRV, QU|IN (unicast-response bit set)
            ]
        );
    }

    /// 全 question で QU（unicast-response）ビットが立つこと（qclass 最上位
    /// 0x8000）。これが無いと、応答者（実機 OTBR mDNS proxy）が QM クエリに
    /// マルチキャストで返し、ephemeral ソケットの一発 resolver が受信できず
    /// timeout する回帰を招く（2026-07-19 実機 tcpdump で確定）。
    #[test]
    fn every_question_sets_qu_unicast_response_bit() {
        let q = encode_query(0, &[("a.local", TYPE_SRV), ("a.local", TYPE_TXT)]);
        // qclass は各 question の末尾 2 バイト。名前 "a.local" は 9 バイト
        // (1+1 + 1+5 + 1)、それに qtype(2)+qclass(2) が続く。
        // 先頭 12(ヘッダ) の後: [name9][SRV 2][qclass 2][name9][TXT 2][qclass 2]
        let first_qclass = u16::from_be_bytes([q[12 + 9 + 2], q[12 + 9 + 3]]);
        let second_qclass = u16::from_be_bytes([q[12 + 9 + 4 + 9 + 2], q[12 + 9 + 4 + 9 + 3]]);
        assert_eq!(
            first_qclass & 0x8000,
            0x8000,
            "SRV question must set QU bit"
        );
        assert_eq!(
            second_qclass & 0x8000,
            0x8000,
            "TXT question must set QU bit"
        );
        // 下位 15 ビットは通常の IN クラスのまま。
        assert_eq!(first_qclass & 0x7fff, CLASS_IN);
        assert_eq!(second_qclass & 0x7fff, CLASS_IN);
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

    /// RFC 6762 §10.2 の cache-flush ビット（class 最上位）を Record に保持する。
    /// class 自体は従来通り検証しない。
    #[test]
    fn parse_message_reads_cache_flush_bit() {
        let addr: Ipv6Addr = "fd00::1".parse().unwrap();
        let with =
            parse_message(&synth_aaaa_class("h.local", 120, addr, 0x8000 | CLASS_IN)).unwrap();
        assert!(with[0].cache_flush);
        let without = parse_message(&synth_aaaa_class("h.local", 120, addr, CLASS_IN)).unwrap();
        assert!(!without[0].cache_flush);
    }
}
