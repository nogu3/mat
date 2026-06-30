//! `mat diag node` の診断データモデルと純ロジック（パーサ + verdict 決定木）。
//!
//! 副作用なし。`mat` 側 `commands/diag.rs::node()` がチェックを集めて `Checks` を
//! 組み、[`derive_verdict`] で原因 `verdict` を導く。chip-tool には触れない。

use serde::Serialize;

use crate::error::ErrorKind;

/// 弱リンク判定の閾値。best LQI がこれ未満 / loss% がこれ以上なら「弱い」。
pub const LQI_WEAK: u8 = 20;
pub const LOSS_WEAK: u8 = 30;

/// mDNS に見えた `_matter._tcp` の1インスタンス（`<CFID>-<nodeid>`）。
/// `+` 行（announce）と `=` 行（resolved）は同じ (compressed_fabric, node_id) を持つため
/// マージして1件にまとめる。`addresses` は resolved ブロックの `address = [<addr>]` から
/// 抽出した IP アドレスリスト（dedup 済み）。announce のみの場合は空。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatterInstance {
    /// compressed fabric id（16桁 hex、大文字正規化）。
    pub compressed_fabric: String,
    pub node_id: u64,
    /// resolved された IP アドレス（`address = [<addr>]` 行から抽出、dedup 済み）。
    pub addresses: Vec<String>,
}

/// ping6 統計。
#[derive(Debug, Clone, PartialEq)]
pub struct Ping6Stats {
    pub loss_pct: u8,
    pub rtt_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IpCheck {
    pub ok: bool,
    pub loss_pct: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    pub method: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MdnsCheck {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertised_self_fabric: Option<bool>,
    pub advertised_any_fabric: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationalCheck {
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ErrorKind>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadCheck {
    pub neighbor_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_lqi: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_role: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Checks {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<IpCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdns: Option<MdnsCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operational: Option<OperationalCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadCheck>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Ok,
    IpUnreachable,
    LinkStarved,
    FabricMissing,
    NotAdvertised,
    Unresolvable,
    SessionFailed,
    DeviceRejected,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub verdict: VerdictKind,
    pub summary: String,
    pub recommendation: String,
}

/// `ping6` の統計サマリ行をパースする。`loss%` 行が無ければ `None`（未実行/失敗）。
pub fn parse_ping6(stdout: &str) -> Option<Ping6Stats> {
    let mut loss_pct: Option<u8> = None;
    let mut rtt_ms: Option<f64> = None;
    for line in stdout.lines() {
        if let Some(idx) = line.find("% packet loss") {
            let head = &line[..idx];
            let num = head.rsplit([' ', ',']).find(|t| !t.is_empty());
            if let Some(v) = num.and_then(|t| t.trim().parse::<f64>().ok()) {
                loss_pct = Some(v.round() as u8);
            }
        }
        if (line.contains("rtt ") || line.contains("round-trip")) && line.contains('=') {
            if let Some(rest) = line.split('=').nth(1) {
                // 例: " 46.773/56.351/61.236/6.773 ms" → avg は2番目。
                if let Some(avg) = rest.trim().split('/').nth(1) {
                    rtt_ms = avg.trim().parse::<f64>().ok();
                }
            }
        }
    }
    loss_pct.map(|loss_pct| Ping6Stats { loss_pct, rtt_ms })
}

/// `avahi-browse -rt _matter._tcp` 出力から `<CFID>-<nodeid>` インスタンスを抽出。
/// 人間形式（空白区切り）と `-p` 形式（`;` 区切り）の両方に対応。
///
/// ステートフルに処理し、`announce`（`+`）行と `resolve`（`=`）行を同一 (CFID, node_id) で
/// マージして1件にまとめる。`address = [<addr>]` 行は直前のインスタンスに付ける（dedup）。
pub fn parse_avahi_matter(stdout: &str) -> Vec<MatterInstance> {
    let mut out: Vec<MatterInstance> = Vec::new();
    let mut current_idx: Option<usize> = None;

    for line in stdout.lines() {
        // address = [...] 行: 現在インスタンスに付与。
        let trimmed = line.trim();
        if trimmed.starts_with("address = [") && trimmed.ends_with(']') {
            if let Some(idx) = current_idx {
                let addr = &trimmed["address = [".len()..trimmed.len() - 1];
                if !addr.is_empty() && !out[idx].addresses.contains(&addr.to_string()) {
                    out[idx].addresses.push(addr.to_string());
                }
            }
            continue;
        }

        if !line.contains("_matter._tcp") {
            continue;
        }

        // `<CFID>-<nodeid>` トークンを探す（空白・`;` で区切り）。
        for tok in line.split(|c: char| c.is_whitespace() || c == ';') {
            if let Some((fab, node)) = tok.split_once('-') {
                let fab_ok = fab.len() == 16 && fab.bytes().all(|b| b.is_ascii_hexdigit());
                let node_ok = !node.is_empty() && node.bytes().all(|b| b.is_ascii_hexdigit());
                if fab_ok && node_ok {
                    if let Ok(node_id) = u64::from_str_radix(node, 16) {
                        let cfid = fab.to_ascii_uppercase();
                        // 同じ (CFID, node_id) が既にあればそこを current にしてマージ。
                        if let Some(pos) = out
                            .iter()
                            .position(|i| i.compressed_fabric == cfid && i.node_id == node_id)
                        {
                            current_idx = Some(pos);
                        } else {
                            out.push(MatterInstance {
                                compressed_fabric: cfid,
                                node_id,
                                addresses: vec![],
                            });
                            current_idx = Some(out.len() - 1);
                        }
                        break; // トークン見つかったのでこの行の残りは不要。
                    }
                }
            }
        }
    }
    out
}

/// chip-tool ログの `Compressed FabricId 0x<hex>` から自 fabric の compressed id を抽出。
pub fn parse_compressed_fabric_id(stderr: &str) -> Option<String> {
    let marker = "Compressed FabricId 0x";
    let start = stderr.find(marker)? + marker.len();
    let hex: String = stderr[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    (hex.len() >= 8).then(|| hex.to_ascii_uppercase())
}

/// chip-tool の operational discovery ログ（`[DIS]` 行など）から、対象 `node_id` 向けに
/// 解決されたインスタンス名 `<CFID>-<NodeId>` を探して自 fabric の compressed id を返す。
///
/// stderr 全体を走査し、空白 / `;` / `,` 区切りの各 token の先頭（`.` より前）が
/// `<16hex>-<16hex>` 形で、後半（NodeId）が `node_id` に一致するものの前半（CFID、
/// 大文字正規化）を返す。複数あれば最初の一致。無ければ `None`。
/// 第1候補として使う理由: operational read 自体が必ず通る解決経路のログで、
/// fabric init の `Compressed FabricId` 行より出やすい。
pub fn parse_operational_instance_cfid(stderr: &str, node_id: u64) -> Option<String> {
    for line in stderr.lines() {
        for tok in line.split(|c: char| c.is_whitespace() || c == ';' || c == ',') {
            let head = tok.split('.').next().unwrap_or(tok);
            if let Some((fab, node)) = head.split_once('-') {
                let fab_ok = fab.len() == 16 && fab.bytes().all(|b| b.is_ascii_hexdigit());
                let node_ok = node.len() == 16 && node.bytes().all(|b| b.is_ascii_hexdigit());
                if fab_ok && node_ok {
                    if let Ok(n) = u64::from_str_radix(node, 16) {
                        if n == node_id {
                            return Some(fab.to_ascii_uppercase());
                        }
                    }
                }
            }
        }
    }
    None
}

/// thread 診断 or ip loss から「弱リンク」か判定。
fn weak_link(checks: &Checks) -> bool {
    let thread_weak = checks
        .thread
        .as_ref()
        .is_some_and(|t| t.neighbor_count <= 1 || t.best_lqi.is_some_and(|l| l < LQI_WEAK));
    let ip_weak = checks.ip.as_ref().is_some_and(|i| i.loss_pct >= LOSS_WEAK);
    thread_weak || ip_weak
}

fn verdict(kind: VerdictKind, summary: &str, rec: &str) -> Verdict {
    Verdict {
        verdict: kind,
        summary: summary.to_string(),
        recommendation: rec.to_string(),
    }
}

/// チェック結果から最尤の原因 `verdict` ＋ summary ＋ recommendation を導く（純関数）。
pub fn derive_verdict(checks: &Checks) -> Verdict {
    // 解決できた = 制御可能のはず。
    if checks.operational.as_ref().is_some_and(|o| o.resolved) {
        return verdict(
            VerdictKind::Ok,
            "Operational discovery succeeded; the node should be controllable.",
            "No action needed.",
        );
    }

    // IP 不達（--deep 時のみ判定可能）。
    if let Some(ip) = &checks.ip {
        if !ip.ok {
            let summary = format!(
                "The node does not respond to ping ({}% packet loss); it is off the network at the IP layer.",
                ip.loss_pct
            );
            return verdict(
                VerdictKind::IpUnreachable,
                &summary,
                "Check power, the Thread Border Router, and network routing.",
            );
        }
    }

    // mDNS 広告の有無で判定（--deep 時のみ mdns が埋まる）。
    if let Some(mdns) = &checks.mdns {
        // 自 fabric を広告していない（false）か、CFID 不明（None）の時に分岐。
        if mdns.advertised_self_fabric != Some(true) {
            if mdns.advertised_any_fabric {
                if mdns.advertised_self_fabric == Some(false) {
                    return verdict(
                        VerdictKind::FabricMissing,
                        "Device advertises Matter under other fabrics but not ours; our fabric was likely removed.",
                        "Re-commission via multi-admin share from a controller that still has the device.",
                    );
                }
                // any 広告ありだが自 fabric 不明 → 解決失敗の一般原因へ委ねる。
            } else if weak_link(checks) {
                let loss_pct = checks.ip.as_ref().map(|ip| ip.loss_pct);
                let best_lqi = checks.thread.as_ref().and_then(|t| t.best_lqi);
                let detail = match (loss_pct, best_lqi) {
                    (Some(loss), Some(lqi)) => format!("loss {}%, best LQI {}", loss, lqi),
                    (Some(loss), None) => format!("loss {}%", loss),
                    (None, Some(lqi)) => format!("best LQI {}", lqi),
                    (None, None) => String::new(),
                };
                let summary = if detail.is_empty() {
                    "IP reachable but not advertising Matter on any fabric; weak Thread link — SRP registration likely incomplete.".to_string()
                } else {
                    format!("IP reachable but not advertising Matter on any fabric; weak Thread link ({detail}) — SRP registration likely incomplete.")
                };
                return verdict(
                    VerdictKind::LinkStarved,
                    &summary,
                    "Improve the Thread link (move the device near a router) or wait; do NOT factory reset — the fabric is intact.",
                );
            } else {
                return verdict(
                    VerdictKind::NotAdvertised,
                    "Not advertising Matter on any fabric, but no strong weak-link evidence.",
                    "Re-run with --deep after a power cycle; verify the Thread link.",
                );
            }
        }
    }

    // ここまで来たら operational の失敗種別で分類。
    match checks.operational.as_ref().and_then(|o| o.kind) {
        Some(ErrorKind::SessionFailed) => verdict(
            VerdictKind::SessionFailed,
            "Resolved but CASE session establishment failed.",
            "Retry; check operational credentials (CASE) state.",
        ),
        Some(ErrorKind::Timeout) | Some(ErrorKind::Unreachable) => verdict(
            VerdictKind::Unresolvable,
            "Operational discovery / resolution timed out (mDNS may be present but not resolvable now).",
            "Retry; transient mDNS/resolution failure. Use --deep to distinguish link_starved vs fabric_missing.",
        ),
        Some(ErrorKind::DeviceRejected) => verdict(
            VerdictKind::DeviceRejected,
            "CASE established but the command was rejected by the device.",
            "Check endpoint / cluster / ACL.",
        ),
        _ => verdict(
            VerdictKind::Unknown,
            "Could not classify the failure; inspect the checks.",
            "Inspect the `checks` object and chip-tool stderr.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_types_construct() {
        let c = Checks::default();
        assert!(c.operational.is_none());
        // Meaningful: a fully resolved node always yields verdict Ok.
        let c_ok = Checks {
            operational: Some(OperationalCheck {
                resolved: true,
                kind: None,
            }),
            ..Default::default()
        };
        assert_eq!(derive_verdict(&c_ok).verdict, VerdictKind::Ok);
    }

    #[test]
    fn ping6_zero_loss_with_rtt() {
        let s = "PING x(x) 56 data bytes\n\
                 3 packets transmitted, 3 received, 0% packet loss, time 2003ms\n\
                 rtt min/avg/max/mdev = 46.773/56.351/61.236/6.773 ms\n";
        let p = parse_ping6(s).unwrap();
        assert_eq!(p.loss_pct, 0);
        assert_eq!(p.rtt_ms, Some(56.351));
    }

    #[test]
    fn ping6_total_loss_no_rtt() {
        let s = "3 packets transmitted, 0 received, 100% packet loss, time 2002ms\n";
        let p = parse_ping6(s).unwrap();
        assert_eq!(p.loss_pct, 100);
        assert_eq!(p.rtt_ms, None);
    }

    #[test]
    fn ping6_unparseable_is_none() {
        assert!(parse_ping6("ping: command not found\n").is_none());
    }

    #[test]
    fn avahi_extracts_instances_human_format() {
        let s = "+   eth0 IPv6 00AABB1122CC3344-0000000000000005   _matter._tcp   local\n\
                 =   eth0 IPv6 00AABB1122CC3344-0000000000000005   _matter._tcp   local\n\
                 +   eth0 IPv6 0011223344556677-000000000000004F   _matter._tcp   local\n";
        let v = parse_avahi_matter(s);
        assert_eq!(v.len(), 2); // dedup の =/+ 重複は1件
        assert_eq!(v[0].compressed_fabric, "00AABB1122CC3344");
        assert_eq!(v[0].node_id, 5);
        assert_eq!(v[0].addresses, Vec::<String>::new());
        assert_eq!(v[1].node_id, 0x4F);
        assert_eq!(v[1].addresses, Vec::<String>::new());
    }

    #[test]
    fn avahi_handles_parseable_semicolons() {
        let s = "+;eth0;IPv6;00AABB1122CC3344-0000000000000005;_matter._tcp;local\n";
        let v = parse_avahi_matter(s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].node_id, 5);
        assert_eq!(v[0].addresses, Vec::<String>::new());
    }

    #[test]
    fn avahi_empty_or_noise_is_empty() {
        assert!(parse_avahi_matter("").is_empty());
        assert!(parse_avahi_matter("avahi-browse: command not found\n").is_empty());
    }

    #[test]
    fn avahi_resolved_block_attaches_address() {
        // announce (+) と resolve (=) が同一インスタンスにマージされ、
        // address = [...] 行のアドレスが付く。
        let s = "+   eth0 IPv6 00AABB1122CC3344-0000000000000005   _matter._tcp   local\n\
                 =   eth0 IPv6 00AABB1122CC3344-0000000000000005   _matter._tcp   local\n\
                    hostname = [dummy.local]\n\
                    address = [fd00::1]\n\
                    port = [5540]\n";
        let v = parse_avahi_matter(s);
        assert_eq!(
            v.len(),
            1,
            "announce + resolve should merge into one instance"
        );
        assert_eq!(v[0].compressed_fabric, "00AABB1122CC3344");
        assert_eq!(v[0].node_id, 5);
        assert_eq!(v[0].addresses, vec!["fd00::1".to_string()]);
    }

    #[test]
    fn avahi_resolved_block_dedup_addresses() {
        // 同じアドレスが複数の resolve ブロックに現れても dedup される。
        let s = "=   eth0 IPv6 00AABB1122CC3344-0000000000000005   _matter._tcp   local\n\
                    address = [fd00::1]\n\
                 =   eth0 IPv6 00AABB1122CC3344-0000000000000005   _matter._tcp   local\n\
                    address = [fd00::1]\n";
        let v = parse_avahi_matter(s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].addresses, vec!["fd00::1".to_string()]);
    }

    #[test]
    fn cfid_extracted_from_chip_log() {
        let s = "[FP] Fabric index 0x1 ... Compressed FabricId 0x00AABB1122CC3344, FabricId 0x1";
        assert_eq!(
            parse_compressed_fabric_id(s).as_deref(),
            Some("00AABB1122CC3344")
        );
    }

    #[test]
    fn cfid_absent_is_none() {
        assert!(parse_compressed_fabric_id("no fabric here").is_none());
    }

    fn op(resolved: bool, kind: Option<ErrorKind>) -> OperationalCheck {
        OperationalCheck { resolved, kind }
    }

    #[test]
    fn verdict_ok_when_resolved() {
        let c = Checks {
            operational: Some(op(true, None)),
            ..Default::default()
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::Ok);
    }

    #[test]
    fn verdict_ip_unreachable() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck {
                ok: false,
                loss_pct: 100,
                rtt_ms: None,
                method: "ping6",
            }),
            ..Default::default()
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::IpUnreachable);
    }

    #[test]
    fn verdict_link_starved_when_not_advertised_and_weak() {
        // 今回の実機ケース: ip 生存(loss 50%)・自/any 広告なし・op timeout。
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck {
                ok: true,
                loss_pct: 50,
                rtt_ms: Some(168.0),
                method: "ping6",
            }),
            mdns: Some(MdnsCheck {
                advertised_self_fabric: Some(false),
                advertised_any_fabric: false,
            }),
            thread: None,
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::LinkStarved);
    }

    #[test]
    fn verdict_link_starved_via_weak_thread() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck {
                ok: true,
                loss_pct: 0,
                rtt_ms: Some(50.0),
                method: "ping6",
            }),
            mdns: Some(MdnsCheck {
                advertised_self_fabric: Some(false),
                advertised_any_fabric: false,
            }),
            thread: Some(ThreadCheck {
                neighbor_count: 1,
                best_lqi: Some(3),
                routing_role: Some(2),
            }),
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::LinkStarved);
    }

    #[test]
    fn verdict_fabric_missing() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck {
                ok: true,
                loss_pct: 0,
                rtt_ms: Some(50.0),
                method: "ping6",
            }),
            mdns: Some(MdnsCheck {
                advertised_self_fabric: Some(false),
                advertised_any_fabric: true,
            }),
            thread: None,
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::FabricMissing);
    }

    #[test]
    fn verdict_not_advertised_without_weak_evidence() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ip: Some(IpCheck {
                ok: true,
                loss_pct: 0,
                rtt_ms: Some(20.0),
                method: "ping6",
            }),
            mdns: Some(MdnsCheck {
                advertised_self_fabric: Some(false),
                advertised_any_fabric: false,
            }),
            thread: None,
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::NotAdvertised);
    }

    #[test]
    fn verdict_unresolvable_when_mdns_unknown_timeout() {
        // --deep 無し: ip/mdns は None。op timeout → unresolvable。
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Timeout))),
            ..Default::default()
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::Unresolvable);
    }

    #[test]
    fn verdict_session_failed() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::SessionFailed))),
            ..Default::default()
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::SessionFailed);
    }

    #[test]
    fn verdict_device_rejected() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::DeviceRejected))),
            ..Default::default()
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::DeviceRejected);
    }

    #[test]
    fn verdict_unknown_fallback() {
        let c = Checks {
            operational: Some(op(false, Some(ErrorKind::Other))),
            ..Default::default()
        };
        assert_eq!(derive_verdict(&c).verdict, VerdictKind::Unknown);
    }

    #[test]
    fn operational_instance_cfid_matches_node() {
        let stderr = "[DIS] OperationalSessionSetup[1:0000000000000005]: resolved instance \
                      00AABB1122CC3344-0000000000000005._matter._tcp.local.\n";
        assert_eq!(
            parse_operational_instance_cfid(stderr, 5),
            Some("00AABB1122CC3344".to_string())
        );
    }

    #[test]
    fn operational_instance_cfid_lowercase_is_normalized() {
        let stderr = "00aabb1122cc3344-0000000000000005._matter._tcp\n";
        assert_eq!(
            parse_operational_instance_cfid(stderr, 5),
            Some("00AABB1122CC3344".to_string())
        );
    }

    #[test]
    fn operational_instance_cfid_ignores_other_node() {
        let stderr = "[DIS] ... 00AABB1122CC3344-0000000000000009._matter._tcp ...\n";
        assert_eq!(parse_operational_instance_cfid(stderr, 5), None);
    }

    #[test]
    fn operational_instance_cfid_absent_returns_none() {
        // fabricIndex:nodeid（コロン区切り、16桁hexでない左辺）は誤マッチしない。
        let stderr = "[DIS] OperationalSessionSetup[1:0000000000000005]: looking up\n";
        assert_eq!(parse_operational_instance_cfid(stderr, 5), None);
    }
}
