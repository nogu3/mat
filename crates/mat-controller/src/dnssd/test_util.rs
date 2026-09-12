//! dnssd テスト共有ヘルパ（合成 mDNS 応答、実 iface 上の multicast / unicast
//! 応答器）。`codec` / `resolve` / `browse` / `cache` の tests から使う。
#![cfg(test)]

use std::net::Ipv6Addr;
use std::time::Duration;

use super::codec::push_name;
use super::{bind_mdns_socket, CLASS_IN, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT};

/// `_matterc._udp.local` の browse / known-answer テスト共通の service 名
/// （`codec` の known-answer テストと `browse` の browse テストの両方が使う —
/// cross-submodule test 定数なのでここに置く）。
pub(super) const MC: &str = "_matterc._udp.local";

/// 合成 mDNS 応答（QR|AA、id 0、question 無し）のビルダ。`synth_*` 各関数と
/// browse / cache の合成ヘルパはすべてこれで組む。
pub(crate) struct MsgBuilder {
    buf: Vec<u8>,
    an: u16,
    /// 直前の `srv` が書いた target 名のメッセージ内オフセット
    /// （`aaaa_ptr_srv_target` が圧縮ポインタで指す）。
    srv_target_off: Option<usize>,
}

impl MsgBuilder {
    pub(crate) fn new() -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0, 0, 0x84, 0x00]); // id 0, QR|AA
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // qd 0, an (後で埋める), ns/ar 0
        MsgBuilder {
            buf,
            an: 0,
            srv_target_off: None,
        }
    }

    fn header(&mut self, rtype: u16, class: u16, ttl: u32) {
        self.buf.extend_from_slice(&rtype.to_be_bytes());
        self.buf.extend_from_slice(&class.to_be_bytes());
        self.buf.extend_from_slice(&ttl.to_be_bytes());
        self.an += 1;
    }

    fn rdata(&mut self, rdata: &[u8]) {
        self.buf
            .extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(rdata);
    }

    /// PTR（class IN — PTR は cache-flush を立てないのが通例）。
    pub(crate) fn ptr(mut self, name: &str, instance: &str) -> Self {
        push_name(&mut self.buf, name);
        self.header(TYPE_PTR, CLASS_IN, 120);
        let mut rdata = Vec::new();
        push_name(&mut rdata, instance);
        self.rdata(&rdata);
        self
    }

    /// SRV（cache-flush|IN）。target 名の位置を記憶する。
    pub(crate) fn srv(mut self, name: &str, port: u16, target: &str) -> Self {
        push_name(&mut self.buf, name);
        self.header(TYPE_SRV, FLUSH_IN, 120);
        let mut rdata = vec![0, 0, 0, 0]; // priority, weight
        rdata.extend_from_slice(&port.to_be_bytes());
        push_name(&mut rdata, target);
        self.srv_target_off = Some(self.buf.len() + 2 + 6); // rdlength(2) + prio/weight/port(6)
        self.rdata(&rdata);
        self
    }

    /// TXT（cache-flush|IN）。
    pub(crate) fn txt(mut self, name: &str, strings: &[&str]) -> Self {
        push_name(&mut self.buf, name);
        self.header(TYPE_TXT, FLUSH_IN, 120);
        let mut rdata = Vec::new();
        for s in strings {
            rdata.push(s.len() as u8);
            rdata.extend_from_slice(s.as_bytes());
        }
        self.rdata(&rdata);
        self
    }

    /// AAAA（cache-flush|IN、TTL 120）。
    pub(crate) fn aaaa(self, name: &str, addr: Ipv6Addr) -> Self {
        self.aaaa_class(name, 120, addr, FLUSH_IN)
    }

    /// class / TTL 指定の AAAA（cache-flush ビット検証用）。
    pub(crate) fn aaaa_class(mut self, name: &str, ttl: u32, addr: Ipv6Addr, class: u16) -> Self {
        push_name(&mut self.buf, name);
        self.header(TYPE_AAAA, class, ttl);
        self.rdata(&addr.octets());
        self
    }

    /// 名前を直前の `srv` の target への圧縮ポインタで書く AAAA（実 mDNS
    /// 応答の形 — `parse_message` の名前圧縮解決を踏ませる）。
    pub(crate) fn aaaa_ptr_srv_target(mut self, addr: Ipv6Addr) -> Self {
        let off = self
            .srv_target_off
            .expect("srv() must precede aaaa_ptr_srv_target()");
        self.buf
            .extend_from_slice(&[0xC0 | (off >> 8) as u8, (off & 0xFF) as u8]);
        self.header(TYPE_AAAA, FLUSH_IN, 120);
        self.rdata(&addr.octets());
        self
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.buf[6..8].copy_from_slice(&self.an.to_be_bytes());
        self.buf
    }
}

const FLUSH_IN: u16 = 0x8000 | CLASS_IN;

/// SRV + TXT + AAAA を 1 メッセージに合成。AAAA のレコード名は SRV rdata
/// 内の target 名への圧縮ポインタで書き、クラスには cache-flush bit を
/// 立てて実 mDNS 応答の形に寄せる。
pub(super) fn synth_response(
    service: &str,
    target: &str,
    port: u16,
    txt: &[&str],
    addr: Ipv6Addr,
) -> Vec<u8> {
    MsgBuilder::new()
        .srv(service, port, target)
        .txt(service, txt)
        .aaaa_ptr_srv_target(addr)
        .finish()
}

/// `IFF_UP|IFF_MULTICAST` な iface（lo 以外、`operstate == "up"` 優先）。
/// group.rs のテストもこれを使う — lo は IFF_MULTICAST を持たず IPv6
/// マルチキャストが絶対に届かないため除外。
pub(crate) fn multicast_ifaces() -> Vec<(String, u32)> {
    const IFF_UP: u32 = 0x1;
    const IFF_MULTICAST: u32 = 0x1000;
    let mut up_first = Vec::new();
    let mut rest = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        let base = entry.path();
        let flags = std::fs::read_to_string(base.join("flags"))
            .ok()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        if flags & IFF_UP == 0 || flags & IFF_MULTICAST == 0 {
            continue;
        }
        let Some(index) = std::fs::read_to_string(base.join("ifindex"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        else {
            continue;
        };
        let operstate = std::fs::read_to_string(base.join("operstate")).unwrap_or_default();
        if operstate.trim() == "up" {
            up_first.push((name, index));
        } else {
            rest.push((name, index));
        }
    }
    up_first.sort_by_key(|(_, idx)| *idx);
    rest.sort_by_key(|(_, idx)| *idx);
    up_first.extend(rest);
    up_first
}

/// OTBR mDNS advertising proxy 型 responder の模擬: QU（unicast-response）
/// ビットを無視し、応答/広告を **ff02::fb へのマルチキャストでのみ** 出す
/// （2026-07-19 実機 tcpdump で確定した挙動）。クエリ検出はせず周期
/// announce する — 問うのは「マルチキャスト応答を受信できるか」だけ。
pub(super) fn spawn_multicast_announcer(
    scope_id: u32,
    msg: Vec<u8>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let sock = bind_mdns_socket(scope_id)?;
    let dest = super::mdns_dest(scope_id);
    Ok(tokio::spawn(async move {
        loop {
            let _ = sock.send_to(&msg, dest).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }))
}

/// avahi（SRP advertising proxy）型 responder の模擬: クエリを受信し、
/// **問い合わせ元アドレスへの unicast でのみ**応答する（QU 準拠。
/// 2026-08-05 実機 pcap で確定した挙動）。served に無い instance には
/// 応答しない。unicast は同一ポート多重 bind の 1 ソケットにしか配達
/// されないため、並行 resolve がソケットを共有しない限り他ノード宛の
/// 答えを黙殺する — という本番機序をそのまま再現する。
pub(super) fn spawn_unicast_responder(
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

/// commissionable browse 用の合成応答: PTR(subtype→instance) +
/// SRV(instance→port/target) + TXT(instance) + AAAA(target への圧縮名)
/// を 1 メッセージに詰める。`synth_response` の SRV/TXT/AAAA 部分に PTR
/// を足した形。
pub(super) fn synth_commissionable_response(
    subtype: &str,
    instance: &str,
    target: &str,
    port: u16,
    txt: &[&str],
    addr: Ipv6Addr,
) -> Vec<u8> {
    MsgBuilder::new()
        .ptr(subtype, instance)
        .srv(instance, port, target)
        .txt(instance, txt)
        .aaaa_ptr_srv_target(addr)
        .finish()
}

/// class を指定できる AAAA 単独メッセージ（cache-flush ビット検証用）。
pub(super) fn synth_aaaa_class(name: &str, ttl: u32, addr: Ipv6Addr, class: u16) -> Vec<u8> {
    MsgBuilder::new()
        .aaaa_class(name, ttl, addr, class)
        .finish()
}
