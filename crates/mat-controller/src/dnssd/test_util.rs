//! dnssd テスト共有ヘルパ（合成 mDNS 応答、実 iface 上の multicast / unicast
//! 応答器）。`codec` / `resolve` / `browse` / `cache` の tests から使う。
#![cfg(test)]

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use super::codec::push_name;
use super::{bind_mdns_socket, MDNS_GROUP, MDNS_PORT, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT};

/// `_matterc._udp.local` の browse / known-answer テスト共通の service 名
/// （`codec` の known-answer テストと、Task 3 で `browse.rs` へ移る browse
/// テストの両方が使う — cross-submodule test 定数なのでここに置く）。
pub(super) const MC: &str = "_matterc._udp.local";

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

/// `IFF_UP|IFF_MULTICAST` な iface（lo 以外、`operstate == "up"` 優先）。
/// group.rs のテストと同じ実行時発見方式 — lo は IFF_MULTICAST を持たず
/// IPv6 マルチキャストが絶対に届かないため除外。
pub(super) fn multicast_ifaces() -> Vec<(String, u32)> {
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
    let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));
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

/// class を指定できる AAAA 単独メッセージ（cache-flush ビット検証用）。
pub(super) fn synth_aaaa_class(name: &str, ttl: u32, addr: Ipv6Addr, class: u16) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&[0, 0, 0x84, 0x00]); // id 0, QR|AA
    m.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0]); // qd 0, an 1
    push_name(&mut m, name);
    m.extend_from_slice(&TYPE_AAAA.to_be_bytes());
    m.extend_from_slice(&class.to_be_bytes());
    m.extend_from_slice(&ttl.to_be_bytes());
    m.extend_from_slice(&16u16.to_be_bytes());
    m.extend_from_slice(&addr.octets());
    m
}
