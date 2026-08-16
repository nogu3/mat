//! Pure DNS resource-record encoder for the device-side mDNS advertiser
//! (spec §4.3; RFC 6762). `mat_controller::dnssd` (the querier) can only
//! build DNS *questions* — this module builds *answers*: PTR/SRV/TXT/AAAA
//! records for the commissionable (`_matterc._udp.local`) and operational
//! (`_matter._tcp.local`) service instances a device advertises.
//!
//! Kept deliberately simple, matching the task brief:
//! - Names are always emitted **uncompressed** (length-prefixed labels, one
//!   trailing `0x00`). Compression is legal to receive but never emitted —
//!   this keeps the encoder trivial to test with a self-contained parser
//!   (see the `tests` module) instead of golden bytes.
//! - Messages carry **zero questions**; matched records go in the Answer
//!   section, with SRV/TXT/AAAA as Additional when the query matched a PTR
//!   name (RFC 6762 §12 "additional record generation" in spirit, not to
//!   the letter).
//! - `unicast: bool` mirrors RFC 6762 §6.4 "Legacy Unicast Responses": when
//!   `true`, the cache-flush bit (top bit of the class field) is omitted on
//!   every record (a legacy/QU unicast reply must not tell the querier to
//!   flush its cache from a single reply — RFC 6762 §6.4). When `false`
//!   (normal multicast response / unsolicited announcement), the
//!   cache-flush bit is set on *unique* records (SRV/TXT/AAAA) but never on
//!   PTR, which is a *shared* record type (RFC 6762 §10.2).
//!
//! `parse_questions` (used by `net::mdns`'s socket loop to figure out what a
//! received query is asking for) lives here too: parsing bytes is pure, no
//! I/O, so it fits `core`'s no-I/O rule even though its only caller is in
//! `net`. It tolerates the `0xC0` compression-pointer form real resolvers
//! use in queries — this project's own encoder never emits it, but must be
//! able to read it.
//!
//! TTL convention (brief leaves the exact numbers to us, "pick consistent
//! values and document"): 120s for host-specific unique records (SRV/AAAA —
//! short-lived, since address/port can change across restarts), 4500s for
//! PTR/TXT (the common avahi/chip-tool convention for the more stable
//! service-level records).

use std::net::Ipv6Addr;

use mat_controller::dnssd::operational_instance;

const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;
/// RFC 6762 §10.2 cache-flush bit (top bit of the class field).
const CACHE_FLUSH: u16 = 0x8000;
/// RFC 6762 §5.4 QU (unicast-response requested) bit — top bit of the
/// qclass field on a question.
const QU_BIT: u16 = 0x8000;

/// TTL (seconds) for SRV/AAAA — see module doc.
const HOST_TTL: u32 = 120;
/// TTL (seconds) for PTR/TXT — see module doc.
const PTR_TXT_TTL: u32 = 4500;

/// One commissionable-service advert (spec §4.3.1, `_matterc._udp.local`).
#[derive(Debug, Clone)]
pub struct CommissionableAdvert {
    /// Service instance name — spec says this SHOULD be a random 64-bit
    /// value, formatted as 16 uppercase hex digits (no service suffix).
    pub instance: String,
    /// Target host name, *without* the trailing `.local` (this module
    /// appends it, mirroring `dnssd::hostname_from_target`'s inverse).
    pub hostname: String,
    pub discriminator: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub port: u16,
    pub addr_v6: Ipv6Addr,
}

/// One operational-service advert (spec §4.3.1, `_matter._tcp.local`).
#[derive(Debug, Clone)]
pub struct OperationalAdvert {
    pub compressed_fabric_id: [u8; 8],
    pub node_id: u64,
    pub hostname: String,
    pub port: u16,
    pub addr_v6: Ipv6Addr,
}

/// One resource record to be serialized. Kept internal — callers only ever
/// see the encoded bytes.
enum Rr {
    Ptr {
        owner: String,
        target: String,
        ttl: u32,
    },
    Srv {
        owner: String,
        target: String,
        port: u16,
        ttl: u32,
    },
    Txt {
        owner: String,
        strings: Vec<String>,
        ttl: u32,
    },
    Aaaa {
        owner: String,
        addr: Ipv6Addr,
        ttl: u32,
    },
}

impl Rr {
    /// PTR is RFC 6762's "shared" record type — cache-flush never applies
    /// to it (§10.2), even in a normal multicast response.
    fn is_shared(&self) -> bool {
        matches!(self, Rr::Ptr { .. })
    }
}

/// Appends `name` in DNS label form (RFC 1035 §3.1), uncompressed.
fn push_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.') {
        let bytes = label.as_bytes();
        debug_assert!(!bytes.is_empty() && bytes.len() <= 63, "bad dns label");
        let len = bytes.len().min(63);
        out.push(len as u8);
        out.extend_from_slice(&bytes[..len]);
    }
    out.push(0);
}

/// Appends one resource record (owner name, type, class[+cache-flush], ttl,
/// rdlength, rdata) to `out`.
fn push_rr(out: &mut Vec<u8>, rr: &Rr, cache_flush: bool) {
    let (owner, ttl, rtype) = match rr {
        Rr::Ptr { owner, ttl, .. } => (owner, *ttl, TYPE_PTR),
        Rr::Srv { owner, ttl, .. } => (owner, *ttl, TYPE_SRV),
        Rr::Txt { owner, ttl, .. } => (owner, *ttl, TYPE_TXT),
        Rr::Aaaa { owner, ttl, .. } => (owner, *ttl, TYPE_AAAA),
    };
    push_name(out, owner);
    out.extend_from_slice(&rtype.to_be_bytes());
    let class = CLASS_IN | if cache_flush { CACHE_FLUSH } else { 0 };
    out.extend_from_slice(&class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());

    let mut rdata = Vec::new();
    match rr {
        Rr::Ptr { target, .. } => push_name(&mut rdata, target),
        Rr::Srv { target, port, .. } => {
            rdata.extend_from_slice(&0u16.to_be_bytes()); // priority
            rdata.extend_from_slice(&0u16.to_be_bytes()); // weight
            rdata.extend_from_slice(&port.to_be_bytes());
            push_name(&mut rdata, target);
        }
        Rr::Txt { strings, .. } => {
            for s in strings {
                let bytes = s.as_bytes();
                let len = bytes.len().min(255);
                rdata.push(len as u8);
                rdata.extend_from_slice(&bytes[..len]);
            }
        }
        Rr::Aaaa { addr, .. } => rdata.extend_from_slice(&addr.octets()),
    }
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(&rdata);
}

/// Builds one DNS response message: header (id=0, QR=1 AA=1, zero
/// questions) + `answers` in the Answer section + `additionals` in the
/// Additional section. `unicast` controls the cache-flush bit — see module
/// doc.
fn encode_message(answers: &[Rr], additionals: &[Rr], unicast: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(&0u16.to_be_bytes()); // id
    out.extend_from_slice(&0x8400u16.to_be_bytes()); // flags: QR=1 AA=1
    out.extend_from_slice(&0u16.to_be_bytes()); // qdcount
    out.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // nscount
    out.extend_from_slice(&(additionals.len() as u16).to_be_bytes());
    for rr in answers {
        push_rr(&mut out, rr, !unicast && !rr.is_shared());
    }
    for rr in additionals {
        push_rr(&mut out, rr, !unicast && !rr.is_shared());
    }
    out
}

struct CommissionableNames {
    service: String,
    long_subtype: String,
    short_subtype: String,
    instance_full: String,
    host_full: String,
}

/// Long-discriminator subtype (spec §4.3.1: `_L<discriminator>._sub.
/// _matterc._udp.local`, decimal, no zero padding) and short-discriminator
/// subtype (`_S<short>._sub...`, `short` = top 4 bits of the 12-bit
/// discriminator — same derivation as `commissioning::manual_pairing_code`
/// and `mat-native::commission`'s short-code matcher).
fn commissionable_names(ad: &CommissionableAdvert) -> CommissionableNames {
    let short = (ad.discriminator >> 8) as u8;
    CommissionableNames {
        service: "_matterc._udp.local".to_string(),
        long_subtype: format!("_L{}._sub._matterc._udp.local", ad.discriminator),
        short_subtype: format!("_S{short}._sub._matterc._udp.local"),
        instance_full: format!("{}._matterc._udp.local", ad.instance),
        host_full: format!("{}.local", ad.hostname),
    }
}

/// TXT record content the querier (`mat_controller::dnssd`) actually reads:
/// `D` (discriminator), `VP` (`<vendor>+<product>`), plus `CM`/`SII`/`SAI`
/// per the brief.
fn commissionable_txt(ad: &CommissionableAdvert) -> Vec<String> {
    vec![
        format!("D={}", ad.discriminator),
        format!("VP={}+{}", ad.vendor_id, ad.product_id),
        "CM=1".to_string(),
        "SII=300".to_string(),
        "SAI=300".to_string(),
    ]
}

/// Answers one question for the commissionable service, if `q_name`
/// matches something this advert can answer:
/// - base service or either subtype PTR → PTR (answer) + SRV/TXT/AAAA
///   (additional)
/// - the instance's own name → SRV/TXT (answer) + AAAA (additional)
/// - the hostname → AAAA (answer)
///
/// `None` if `q_name` doesn't match any of the above (not for us).
pub fn encode_commissionable_response(
    q_name: &str,
    ad: &CommissionableAdvert,
    unicast: bool,
) -> Option<Vec<u8>> {
    let names = commissionable_names(ad);
    let srv = Rr::Srv {
        owner: names.instance_full.clone(),
        target: names.host_full.clone(),
        port: ad.port,
        ttl: HOST_TTL,
    };
    let txt = Rr::Txt {
        owner: names.instance_full.clone(),
        strings: commissionable_txt(ad),
        ttl: PTR_TXT_TTL,
    };
    let aaaa = Rr::Aaaa {
        owner: names.host_full.clone(),
        addr: ad.addr_v6,
        ttl: HOST_TTL,
    };

    if q_name.eq_ignore_ascii_case(&names.service)
        || q_name.eq_ignore_ascii_case(&names.long_subtype)
        || q_name.eq_ignore_ascii_case(&names.short_subtype)
    {
        let ptr = Rr::Ptr {
            owner: q_name.to_string(),
            target: names.instance_full.clone(),
            ttl: PTR_TXT_TTL,
        };
        return Some(encode_message(&[ptr], &[srv, txt, aaaa], unicast));
    }
    if q_name.eq_ignore_ascii_case(&names.instance_full) {
        return Some(encode_message(&[srv, txt], &[aaaa], unicast));
    }
    if q_name.eq_ignore_ascii_case(&names.host_full) {
        return Some(encode_message(&[aaaa], &[], unicast));
    }
    None
}

struct OperationalNames {
    service: String,
    instance_full: String,
    host_full: String,
}

fn operational_names(ad: &OperationalAdvert) -> OperationalNames {
    let instance = operational_instance(&ad.compressed_fabric_id, ad.node_id);
    OperationalNames {
        service: "_matter._tcp.local".to_string(),
        instance_full: format!("{instance}._matter._tcp.local"),
        host_full: format!("{}.local", ad.hostname),
    }
}

/// Operational TXT: `SII`/`SAI` only — the querier's `resolve_operational`
/// only reads those two keys (no `D`/`VP`/`CM`, which are commissioning-only
/// concepts).
fn operational_txt() -> Vec<String> {
    vec!["SII=300".to_string(), "SAI=300".to_string()]
}

/// Same shape as [`encode_commissionable_response`], for the operational
/// service (no subtypes — spec §4.3.1 has none for `_matter._tcp`).
pub fn encode_operational_response(
    q_name: &str,
    ad: &OperationalAdvert,
    unicast: bool,
) -> Option<Vec<u8>> {
    let names = operational_names(ad);
    let srv = Rr::Srv {
        owner: names.instance_full.clone(),
        target: names.host_full.clone(),
        port: ad.port,
        ttl: HOST_TTL,
    };
    let txt = Rr::Txt {
        owner: names.instance_full.clone(),
        strings: operational_txt(),
        ttl: PTR_TXT_TTL,
    };
    let aaaa = Rr::Aaaa {
        owner: names.host_full.clone(),
        addr: ad.addr_v6,
        ttl: HOST_TTL,
    };

    if q_name.eq_ignore_ascii_case(&names.service) {
        let ptr = Rr::Ptr {
            owner: q_name.to_string(),
            target: names.instance_full.clone(),
            ttl: PTR_TXT_TTL,
        };
        return Some(encode_message(&[ptr], &[srv, txt, aaaa], unicast));
    }
    if q_name.eq_ignore_ascii_case(&names.instance_full) {
        return Some(encode_message(&[srv, txt], &[aaaa], unicast));
    }
    if q_name.eq_ignore_ascii_case(&names.host_full) {
        return Some(encode_message(&[aaaa], &[], unicast));
    }
    None
}

/// Builds one unsolicited multicast announcement (RFC 6762 §8.3) containing
/// every record for `commissionable` (if advertising) and each entry of
/// `operational`, all in the Answer section (nothing withheld to
/// Additional — an announcement isn't answering a specific question).
/// Cache-flush is set on unique records exactly as a normal (non-legacy)
/// multicast response would (`unicast = false`). The caller (`net::mdns`)
/// is expected to send this twice, ~1s apart, per §8.3 — this function
/// only builds the bytes.
pub fn encode_unsolicited_announcement(
    commissionable: Option<&CommissionableAdvert>,
    operational: &[OperationalAdvert],
) -> Vec<u8> {
    encode_announcement_or_goodbye(commissionable, operational, None)
}

/// Builds one "goodbye" packet (RFC 6762 §8.3/§10.1: a departing responder
/// announces its records one last time with TTL=0, telling peers to purge
/// them from cache immediately rather than waiting out the normal TTL).
/// Same record set, same order, as [`encode_unsolicited_announcement`] for
/// the same adverts — every TTL is forced to 0 instead of each record
/// type's normal value. The caller (`net::mdns`) sends this *before*
/// removing an advert (clearing commissionable, or `remove_operational`),
/// per the same "send twice, ~1s apart" §8.3 cadence as a normal
/// announcement.
pub fn encode_goodbye(
    commissionable: Option<&CommissionableAdvert>,
    operational: &[OperationalAdvert],
) -> Vec<u8> {
    encode_announcement_or_goodbye(commissionable, operational, Some(0))
}

/// Shared record-set builder for [`encode_unsolicited_announcement`] and
/// [`encode_goodbye`] — identical shape either way, differing only in
/// whether every record's TTL is forced to `ttl_override` (goodbye,
/// always `Some(0)`) or left at its normal per-record-type value
/// (announcement, `None`).
fn encode_announcement_or_goodbye(
    commissionable: Option<&CommissionableAdvert>,
    operational: &[OperationalAdvert],
    ttl_override: Option<u32>,
) -> Vec<u8> {
    let ttl = |normal: u32| ttl_override.unwrap_or(normal);
    let mut answers = Vec::new();
    if let Some(ad) = commissionable {
        let names = commissionable_names(ad);
        answers.push(Rr::Ptr {
            owner: names.service.clone(),
            target: names.instance_full.clone(),
            ttl: ttl(PTR_TXT_TTL),
        });
        answers.push(Rr::Ptr {
            owner: names.long_subtype.clone(),
            target: names.instance_full.clone(),
            ttl: ttl(PTR_TXT_TTL),
        });
        answers.push(Rr::Ptr {
            owner: names.short_subtype.clone(),
            target: names.instance_full.clone(),
            ttl: ttl(PTR_TXT_TTL),
        });
        answers.push(Rr::Srv {
            owner: names.instance_full.clone(),
            target: names.host_full.clone(),
            port: ad.port,
            ttl: ttl(HOST_TTL),
        });
        answers.push(Rr::Txt {
            owner: names.instance_full.clone(),
            strings: commissionable_txt(ad),
            ttl: ttl(PTR_TXT_TTL),
        });
        answers.push(Rr::Aaaa {
            owner: names.host_full.clone(),
            addr: ad.addr_v6,
            ttl: ttl(HOST_TTL),
        });
    }
    for ad in operational {
        let names = operational_names(ad);
        answers.push(Rr::Ptr {
            owner: names.service.clone(),
            target: names.instance_full.clone(),
            ttl: ttl(PTR_TXT_TTL),
        });
        answers.push(Rr::Srv {
            owner: names.instance_full.clone(),
            target: names.host_full.clone(),
            port: ad.port,
            ttl: ttl(HOST_TTL),
        });
        answers.push(Rr::Txt {
            owner: names.instance_full.clone(),
            strings: operational_txt(),
            ttl: ttl(PTR_TXT_TTL),
        });
        answers.push(Rr::Aaaa {
            owner: names.host_full.clone(),
            addr: ad.addr_v6,
            ttl: ttl(HOST_TTL),
        });
    }
    encode_message(&answers, &[], false)
}

// ── question parsing (for `net::mdns`'s incoming-query handling) ──────────

/// Reads a possibly-compressed DNS name starting at `pos` (RFC 1035 §4.1.4).
/// Returns the dotted name and the offset just past the name *at its
/// original location* (i.e. past the 2-byte pointer, not into the target).
/// Hop-bounded to reject compression loops. A private duplicate of
/// `mat_controller::dnssd`'s (private, un-exported) reader — see the module
/// doc for why this lives here instead of reusing that one.
fn read_name(buf: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut next = None;
    let mut hops = 0u8;
    loop {
        let &len = buf.get(pos)?;
        if len == 0 {
            return Some((out, next.unwrap_or(pos + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let &lo = buf.get(pos + 1)?;
            if next.is_none() {
                next = Some(pos + 2);
            }
            pos = usize::from(len & 0x3F) << 8 | usize::from(lo);
            hops += 1;
            if hops > 32 {
                return None;
            }
            continue;
        }
        if len & 0xC0 != 0 {
            return None; // reserved label type
        }
        let label = buf.get(pos + 1..pos + 1 + usize::from(len))?;
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&String::from_utf8_lossy(label));
        pos += 1 + usize::from(len);
    }
}

/// Parses the question section of a DNS message into `(name, qtype, qu)`
/// triples — `qu` is the top bit of the qclass field (RFC 6762 §5.4).
/// Tolerant: a short/malformed message yields as many questions as were
/// readable before parsing gave up (possibly zero), never an error — a
/// garbled or hostile datagram must not abort the advertiser's receive
/// loop.
pub fn parse_questions(buf: &[u8]) -> Vec<(String, u16, bool)> {
    if buf.len() < 12 {
        return Vec::new();
    }
    let qd = u16::from_be_bytes([buf[4], buf[5]]);
    let mut pos = 12usize;
    let mut out = Vec::new();
    for _ in 0..qd {
        let Some((name, p)) = read_name(buf, pos) else {
            break;
        };
        let (Some(qtype_bytes), Some(qclass_bytes)) = (buf.get(p..p + 2), buf.get(p + 2..p + 4))
        else {
            break;
        };
        let qtype = u16::from_be_bytes(qtype_bytes.try_into().expect("2 bytes"));
        let qclass = u16::from_be_bytes(qclass_bytes.try_into().expect("2 bytes"));
        out.push((name, qtype, qclass & QU_BIT != 0));
        pos = p + 4;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commissionable() -> CommissionableAdvert {
        CommissionableAdvert {
            instance: "1234567890ABCDEF".to_string(),
            hostname: "DEV01".to_string(),
            discriminator: 840, // short = 840 >> 8 = 3
            vendor_id: 65521,
            product_id: 32768,
            port: 5540,
            addr_v6: "fe80::1".parse().unwrap(),
        }
    }

    fn operational() -> OperationalAdvert {
        OperationalAdvert {
            compressed_fabric_id: [0x87, 0xE1, 0xB0, 0x04, 0xE2, 0x35, 0xA1, 0x30],
            node_id: 0xCD55_44AA_7B13_EF14,
            hostname: "DEV01".to_string(),
            port: 5540,
            addr_v6: "fe80::2".parse().unwrap(),
        }
    }

    // ── self-contained response parser (test-only) ─────────────────────

    struct ParsedRr {
        name: String,
        rtype: u16,
        cache_flush: bool,
        ttl: u32,
        rdata: Vec<u8>,
    }

    /// Parses `count` records starting at `*pos`, advancing `*pos` past
    /// them. Owner names may use compression (we reuse `read_name`), but
    /// our own encoder never emits it — this just means the test parser
    /// isn't accidentally weaker than a real one.
    fn parse_records(buf: &[u8], count: u16, pos: &mut usize) -> Vec<ParsedRr> {
        let mut out = Vec::new();
        for _ in 0..count {
            let (name, p) = read_name(buf, *pos).expect("record name");
            let rtype = u16::from_be_bytes(buf[p..p + 2].try_into().unwrap());
            let class = u16::from_be_bytes(buf[p + 2..p + 4].try_into().unwrap());
            let ttl = u32::from_be_bytes(buf[p + 4..p + 8].try_into().unwrap());
            let rdlen = u16::from_be_bytes(buf[p + 8..p + 10].try_into().unwrap()) as usize;
            let rdata = buf[p + 10..p + 10 + rdlen].to_vec();
            out.push(ParsedRr {
                name,
                rtype,
                cache_flush: class & CACHE_FLUSH != 0,
                ttl,
                rdata,
            });
            *pos = p + 10 + rdlen;
        }
        out
    }

    /// Splits a full response message into (answers, additionals), first
    /// asserting the header shape our encoder always produces (zero
    /// questions, zero authority records).
    fn parse_response(buf: &[u8]) -> (Vec<ParsedRr>, Vec<ParsedRr>) {
        assert!(buf.len() >= 12, "short message");
        let qd = u16::from_be_bytes([buf[4], buf[5]]);
        let an = u16::from_be_bytes([buf[6], buf[7]]);
        let ns = u16::from_be_bytes([buf[8], buf[9]]);
        let ar = u16::from_be_bytes([buf[10], buf[11]]);
        assert_eq!(qd, 0, "encoder always emits zero questions");
        assert_eq!(ns, 0, "encoder never emits authority records");
        let mut pos = 12usize;
        let answers = parse_records(buf, an, &mut pos);
        let additionals = parse_records(buf, ar, &mut pos);
        assert_eq!(pos, buf.len(), "trailing bytes after last record");
        (answers, additionals)
    }

    /// Decodes a run of length-prefixed labels that is known (by
    /// construction — our encoder never compresses) to be self-contained
    /// within `bytes`, with no absolute-offset pointers.
    fn decode_uncompressed_name(bytes: &[u8]) -> String {
        let mut out = String::new();
        let mut pos = 0usize;
        loop {
            let len = bytes[pos] as usize;
            if len == 0 {
                break;
            }
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(std::str::from_utf8(&bytes[pos + 1..pos + 1 + len]).unwrap());
            pos += 1 + len;
        }
        out
    }

    fn decode_srv(rdata: &[u8]) -> (u16, String) {
        let port = u16::from_be_bytes([rdata[4], rdata[5]]);
        (port, decode_uncompressed_name(&rdata[6..]))
    }

    fn decode_txt(rdata: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < rdata.len() {
            let n = rdata[i] as usize;
            out.push(
                std::str::from_utf8(&rdata[i + 1..i + 1 + n])
                    .unwrap()
                    .to_string(),
            );
            i += 1 + n;
        }
        out
    }

    fn decode_aaaa(rdata: &[u8]) -> Ipv6Addr {
        Ipv6Addr::from(<[u8; 16]>::try_from(rdata).unwrap())
    }

    // ── commissionable ──────────────────────────────────────────────────

    #[test]
    fn commissionable_base_ptr_query_answers_ptr_srv_txt_aaaa() {
        let ad = commissionable();
        let bytes = encode_commissionable_response("_matterc._udp.local", &ad, false)
            .expect("base service PTR should match");
        let (answers, additionals) = parse_response(&bytes);

        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].rtype, TYPE_PTR);
        assert_eq!(answers[0].name, "_matterc._udp.local");
        assert!(!answers[0].cache_flush, "PTR is shared, never cache-flush");
        assert_eq!(answers[0].ttl, PTR_TXT_TTL);
        let ptr_target = decode_uncompressed_name(&answers[0].rdata);
        assert_eq!(ptr_target, "1234567890ABCDEF._matterc._udp.local");

        assert_eq!(additionals.len(), 3);
        let srv = &additionals[0];
        assert_eq!(srv.rtype, TYPE_SRV);
        assert_eq!(srv.name, "1234567890ABCDEF._matterc._udp.local");
        assert!(
            srv.cache_flush,
            "SRV is unique, cache-flush set in a normal response"
        );
        assert_eq!(srv.ttl, HOST_TTL);
        let (port, target) = decode_srv(&srv.rdata);
        assert_eq!(port, 5540);
        assert_eq!(target, "DEV01.local");

        let txt = &additionals[1];
        assert_eq!(txt.rtype, TYPE_TXT);
        assert_eq!(txt.name, "1234567890ABCDEF._matterc._udp.local");
        assert!(txt.cache_flush);
        assert_eq!(txt.ttl, PTR_TXT_TTL);
        assert_eq!(
            decode_txt(&txt.rdata),
            vec!["D=840", "VP=65521+32768", "CM=1", "SII=300", "SAI=300"]
        );

        let aaaa = &additionals[2];
        assert_eq!(aaaa.rtype, TYPE_AAAA);
        assert_eq!(aaaa.name, "DEV01.local");
        assert!(aaaa.cache_flush);
        assert_eq!(aaaa.ttl, HOST_TTL);
        assert_eq!(
            decode_aaaa(&aaaa.rdata),
            "fe80::1".parse::<Ipv6Addr>().unwrap()
        );
    }

    #[test]
    fn commissionable_long_discriminator_subtype_matches() {
        let ad = commissionable();
        let bytes = encode_commissionable_response("_L840._sub._matterc._udp.local", &ad, false)
            .expect("long-discriminator subtype PTR should match");
        let (answers, _) = parse_response(&bytes);
        assert_eq!(answers[0].name, "_L840._sub._matterc._udp.local");
        assert_eq!(
            decode_uncompressed_name(&answers[0].rdata),
            "1234567890ABCDEF._matterc._udp.local"
        );
    }

    #[test]
    fn commissionable_short_discriminator_subtype_matches() {
        let ad = commissionable(); // discriminator 840 -> short 3
        let bytes = encode_commissionable_response("_S3._sub._matterc._udp.local", &ad, false)
            .expect("short-discriminator subtype PTR should match");
        let (answers, _) = parse_response(&bytes);
        assert_eq!(answers[0].name, "_S3._sub._matterc._udp.local");
    }

    #[test]
    fn commissionable_wrong_short_discriminator_does_not_match() {
        let ad = commissionable(); // short is 3, not 7
        assert!(
            encode_commissionable_response("_S7._sub._matterc._udp.local", &ad, false).is_none()
        );
    }

    #[test]
    fn commissionable_instance_query_answers_srv_txt_then_aaaa_additional() {
        let ad = commissionable();
        let bytes =
            encode_commissionable_response("1234567890ABCDEF._matterc._udp.local", &ad, false)
                .unwrap();
        let (answers, additionals) = parse_response(&bytes);
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].rtype, TYPE_SRV);
        assert_eq!(answers[1].rtype, TYPE_TXT);
        assert_eq!(additionals.len(), 1);
        assert_eq!(additionals[0].rtype, TYPE_AAAA);
    }

    #[test]
    fn commissionable_hostname_query_answers_aaaa_only() {
        let ad = commissionable();
        let bytes = encode_commissionable_response("DEV01.local", &ad, false).unwrap();
        let (answers, additionals) = parse_response(&bytes);
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].rtype, TYPE_AAAA);
        assert!(additionals.is_empty());
    }

    #[test]
    fn commissionable_unrelated_name_returns_none() {
        let ad = commissionable();
        assert!(encode_commissionable_response("_other._udp.local", &ad, false).is_none());
    }

    #[test]
    fn commissionable_unicast_response_omits_cache_flush_everywhere() {
        let ad = commissionable();
        let bytes = encode_commissionable_response("_matterc._udp.local", &ad, true).unwrap();
        let (answers, additionals) = parse_response(&bytes);
        assert!(!answers[0].cache_flush);
        assert!(additionals.iter().all(|r| !r.cache_flush));
    }

    // ── operational ─────────────────────────────────────────────────────

    #[test]
    fn operational_base_ptr_query_answers_ptr_srv_txt_aaaa() {
        let ad = operational();
        let bytes = encode_operational_response("_matter._tcp.local", &ad, false).unwrap();
        let (answers, additionals) = parse_response(&bytes);

        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].rtype, TYPE_PTR);
        let expected_instance = "87E1B004E235A130-CD5544AA7B13EF14._matter._tcp.local";
        assert_eq!(
            decode_uncompressed_name(&answers[0].rdata),
            expected_instance
        );

        assert_eq!(additionals.len(), 3);
        assert_eq!(additionals[0].rtype, TYPE_SRV);
        assert_eq!(additionals[0].name, expected_instance);
        let (port, target) = decode_srv(&additionals[0].rdata);
        assert_eq!(port, 5540);
        assert_eq!(target, "DEV01.local");

        assert_eq!(additionals[1].rtype, TYPE_TXT);
        assert_eq!(
            decode_txt(&additionals[1].rdata),
            vec!["SII=300", "SAI=300"]
        );

        assert_eq!(additionals[2].rtype, TYPE_AAAA);
        assert_eq!(
            decode_aaaa(&additionals[2].rdata),
            "fe80::2".parse::<Ipv6Addr>().unwrap()
        );
    }

    #[test]
    fn operational_instance_query_answers_srv_txt_then_aaaa_additional() {
        let ad = operational();
        let bytes = encode_operational_response(
            "87E1B004E235A130-CD5544AA7B13EF14._matter._tcp.local",
            &ad,
            false,
        )
        .unwrap();
        let (answers, additionals) = parse_response(&bytes);
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].rtype, TYPE_SRV);
        assert_eq!(answers[1].rtype, TYPE_TXT);
        assert_eq!(additionals.len(), 1);
        assert_eq!(additionals[0].rtype, TYPE_AAAA);
    }

    #[test]
    fn operational_hostname_query_answers_aaaa_only() {
        let ad = operational();
        let bytes = encode_operational_response("DEV01.local", &ad, false).unwrap();
        let (answers, _) = parse_response(&bytes);
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].rtype, TYPE_AAAA);
    }

    #[test]
    fn operational_unrelated_name_returns_none() {
        let ad = operational();
        assert!(encode_operational_response("_matterc._udp.local", &ad, false).is_none());
    }

    // ── unsolicited announcement ────────────────────────────────────────

    #[test]
    fn unsolicited_announcement_bundles_commissionable_and_operational() {
        let c = commissionable();
        let o = operational();
        let bytes = encode_unsolicited_announcement(Some(&c), std::slice::from_ref(&o));
        let (answers, additionals) = parse_response(&bytes);
        assert!(additionals.is_empty());
        // commissionable: PTR(base) + PTR(long) + PTR(short) + SRV + TXT + AAAA
        // operational:    PTR(base) + SRV + TXT + AAAA
        assert_eq!(answers.len(), 10);
        let types: Vec<u16> = answers.iter().map(|r| r.rtype).collect();
        assert_eq!(
            types,
            vec![
                TYPE_PTR, TYPE_PTR, TYPE_PTR, TYPE_SRV, TYPE_TXT, TYPE_AAAA, TYPE_PTR, TYPE_SRV,
                TYPE_TXT, TYPE_AAAA,
            ]
        );
        assert_eq!(answers[0].name, "_matterc._udp.local");
        assert_eq!(answers[1].name, "_L840._sub._matterc._udp.local");
        assert_eq!(answers[2].name, "_S3._sub._matterc._udp.local");
        assert_eq!(answers[6].name, "_matter._tcp.local");
        // unique records get cache-flush in an announcement (not "unicast").
        assert!(answers[3].cache_flush); // commissionable SRV
        assert!(!answers[0].cache_flush); // PTR never does
    }

    #[test]
    fn unsolicited_announcement_with_no_commissionable_only_advertises_operational() {
        let o = operational();
        let bytes = encode_unsolicited_announcement(None, std::slice::from_ref(&o));
        let (answers, _) = parse_response(&bytes);
        assert_eq!(answers.len(), 4);
        assert_eq!(answers[0].name, "_matter._tcp.local");
    }

    // ── goodbye (RFC 6762 §8.3: departure, TTL=0) ───────────────────────

    #[test]
    fn goodbye_bundles_the_same_records_as_announcement_but_all_ttl_zero() {
        let c = commissionable();
        let o = operational();
        let announce = encode_unsolicited_announcement(Some(&c), std::slice::from_ref(&o));
        let goodbye = encode_goodbye(Some(&c), std::slice::from_ref(&o));

        let (announce_answers, _) = parse_response(&announce);
        let (goodbye_answers, goodbye_additionals) = parse_response(&goodbye);

        assert!(goodbye_additionals.is_empty());
        // Same record set/order/types/names as the announcement — only TTL
        // (and, incidentally, cache-flush bit shape) differs.
        assert_eq!(goodbye_answers.len(), announce_answers.len());
        for (a, g) in announce_answers.iter().zip(goodbye_answers.iter()) {
            assert_eq!(a.rtype, g.rtype);
            assert_eq!(a.name, g.name);
            assert_eq!(a.rdata, g.rdata);
            assert_eq!(g.ttl, 0, "goodbye record must have TTL=0");
        }
        // Sanity: the announcement itself did *not* already use TTL=0
        // everywhere (otherwise this test wouldn't distinguish anything).
        assert!(announce_answers.iter().any(|r| r.ttl != 0));
    }

    #[test]
    fn goodbye_with_no_commissionable_only_advertises_operational_at_ttl_zero() {
        let o = operational();
        let bytes = encode_goodbye(None, std::slice::from_ref(&o));
        let (answers, _) = parse_response(&bytes);
        assert_eq!(answers.len(), 4);
        assert_eq!(answers[0].name, "_matter._tcp.local");
        assert!(answers.iter().all(|r| r.ttl == 0));
    }

    // ── question parsing ────────────────────────────────────────────────

    /// Encodes a minimal query message (id=0, qdcount=questions.len(),
    /// zero answer/authority/additional) for `parse_questions` to consume
    /// — a local reimplementation of `dnssd::encode_query`'s shape (that
    /// function is private to `mat_controller::dnssd`).
    fn encode_test_query(questions: &[(&str, u16, u16)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&(questions.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        for (name, qtype, qclass) in questions {
            push_name(&mut out, name);
            out.extend_from_slice(&qtype.to_be_bytes());
            out.extend_from_slice(&qclass.to_be_bytes());
        }
        out
    }

    #[test]
    fn parse_questions_reads_name_type_and_qu_bit() {
        let msg = encode_test_query(&[
            ("_matterc._udp.local", TYPE_PTR, CLASS_IN | QU_BIT),
            ("DEV01.local", TYPE_AAAA, CLASS_IN),
        ]);
        let qs = parse_questions(&msg);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0], ("_matterc._udp.local".to_string(), TYPE_PTR, true));
        assert_eq!(qs[1], ("DEV01.local".to_string(), TYPE_AAAA, false));
    }

    #[test]
    fn parse_questions_follows_compression_pointer() {
        // Question 1: "_matterc._udp.local" written literally at offset 12.
        // Question 2: SRV for the same name, but the name is a pointer back
        // to offset 12 — proving the decompressor is exercised on the way
        // in, even though this module's own encoder never emits pointers.
        let mut msg = Vec::new();
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&2u16.to_be_bytes()); // qdcount
        msg.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        push_name(&mut msg, "_matterc._udp.local"); // offset 12
        msg.extend_from_slice(&TYPE_PTR.to_be_bytes());
        msg.extend_from_slice(&(CLASS_IN | QU_BIT).to_be_bytes());
        msg.extend_from_slice(&[0xC0, 0x0C]); // pointer to offset 12
        msg.extend_from_slice(&TYPE_SRV.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());

        let qs = parse_questions(&msg);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].0, "_matterc._udp.local");
        assert_eq!(qs[1].0, "_matterc._udp.local");
        assert_eq!(qs[1].1, TYPE_SRV);
        assert!(!qs[1].2);
    }

    #[test]
    fn parse_questions_on_short_buffer_returns_empty_not_panic() {
        assert_eq!(parse_questions(&[1, 2, 3]), Vec::new());
    }

    #[test]
    fn parse_questions_on_truncated_question_stops_early_not_panic() {
        // Header claims 1 question but the buffer ends mid-name.
        let mut msg = Vec::new();
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        msg.push(5); // claims a 5-byte label, but nothing follows
        assert_eq!(parse_questions(&msg), Vec::new());
    }
}
