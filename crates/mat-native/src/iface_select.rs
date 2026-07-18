//! iface 自動検出（M8c-3 native 既定化）。
//!
//! `MAT_IFACE` / `MAT_MATD_IFACE` 未設定時に Matter 用の iface を選ぶ。
//! 候補条件: operstate up（carrier 有 — 未使用 docker0 等を除外）・
//! MULTICAST・非 loopback・非 POINTOPOINT（tailscale0 / tun 系を除外 —
//! multicast egress で経路解決に勝ってしまう罠の回避）・IPv6 link-local
//! アドレス保有。候補ちょうど 1 つなら採用、0 または複数はハードエラー
//! （曖昧なまま選ぶと group 送信がサイレント不達 + カウンタ汚染になる
//! 前科があるため、決定的に選ばず利用者に `MAT_IFACE` 指定を求める）。
//! 毎回実行時に検出し状態は持たない（設計ルール 4）。

use mat_core::error::{ErrorKind, MatError};

const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_POINTOPOINT: u32 = 0x10;
const IFF_MULTICAST: u32 = 0x1000;

#[derive(Debug, Clone)]
pub struct IfaceInfo {
    pub name: String,
    pub flags: u32,
    pub operstate_up: bool,
    pub has_ipv6_ll: bool,
}

#[derive(Debug)]
pub enum SelectError {
    NoCandidate,
    Ambiguous(Vec<String>),
}

fn eligible(i: &IfaceInfo) -> bool {
    i.operstate_up
        && i.has_ipv6_ll
        && i.flags & IFF_UP != 0
        && i.flags & IFF_MULTICAST != 0
        && i.flags & IFF_LOOPBACK == 0
        && i.flags & IFF_POINTOPOINT == 0
}

/// 候補選別の純関数（表駆動テスト対象）。`infos` の列挙順を保つ。
pub fn select(infos: &[IfaceInfo]) -> Result<String, SelectError> {
    let mut names: Vec<String> = infos
        .iter()
        .filter(|i| eligible(i))
        .map(|i| i.name.clone())
        .collect();
    match names.len() {
        0 => Err(SelectError::NoCandidate),
        1 => Ok(names.remove(0)),
        _ => Err(SelectError::Ambiguous(names)),
    }
}

/// `/sys/class/net` + `/proc/net/if_inet6` を走査して候補を集め、`select` する。
/// 失敗はすべて kind `other`（新 kind は設けない — spec 設計 4）。
pub fn autodetect() -> Result<String, MatError> {
    let infos = scan().map_err(|e| {
        MatError::new(
            ErrorKind::Other,
            format!("iface autodetect: scan failed: {e}"),
        )
    })?;
    select(&infos).map_err(|e| match e {
        SelectError::NoCandidate => MatError::new(
            ErrorKind::Other,
            "iface autodetect: no usable interface (need up/multicast/non-p2p with IPv6 link-local); set MAT_IFACE".to_string(),
        ),
        SelectError::Ambiguous(names) => MatError::new(
            ErrorKind::Other,
            format!("iface autodetect: ambiguous candidates [{}]; set MAT_IFACE", names.join(", ")),
        ),
    })
}

fn scan() -> std::io::Result<Vec<IfaceInfo>> {
    // IPv6 link-local を持つ iface 名の集合: /proc/net/if_inet6 の各行は
    // "<addr32hex> <ifindex> <prefixlen> <scope> <flags> <name>"。scope 0x20 = link-local。
    let mut ll_names = std::collections::HashSet::new();
    for line in std::fs::read_to_string("/proc/net/if_inet6")?.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 6 && cols[3] == "20" {
            ll_names.insert(cols[5].to_string());
        }
    }
    let mut infos = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir("/sys/class/net")?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name); // 決定的な列挙順
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let base = entry.path();
        let flags = std::fs::read_to_string(base.join("flags"))
            .ok()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        let operstate_up = std::fs::read_to_string(base.join("operstate"))
            .map(|s| s.trim() == "up")
            .unwrap_or(false);
        infos.push(IfaceInfo {
            has_ipv6_ll: ll_names.contains(&name),
            name,
            flags,
            operstate_up,
        });
    }
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ifi(name: &str, flags: u32, up: bool, ll: bool) -> IfaceInfo {
        IfaceInfo {
            name: name.into(),
            flags,
            operstate_up: up,
            has_ipv6_ll: ll,
        }
    }

    // flags: IFF_UP=0x1, IFF_LOOPBACK=0x8, IFF_POINTOPOINT=0x10, IFF_MULTICAST=0x1000
    const ETH: u32 = 0x1 | 0x1000; // up|multicast
    const LO: u32 = 0x1 | 0x8 | 0x1000;
    const TS: u32 = 0x1 | 0x10 | 0x1000; // tailscale0: up|pointopoint|multicast

    #[test]
    fn selects_single_ethernet() {
        let infos = [ifi("eth0", ETH, true, true), ifi("lo", LO, true, true)];
        assert_eq!(select(&infos).unwrap(), "eth0");
    }

    #[test]
    fn excludes_pointopoint_tailscale() {
        let infos = [
            ifi("eth0", ETH, true, true),
            ifi("tailscale0", TS, true, true),
        ];
        assert_eq!(select(&infos).unwrap(), "eth0");
    }

    #[test]
    fn excludes_down_carrier_and_missing_ll() {
        // docker0 は up フラグはあるが operstate down / veth は link-local 無しを模す
        let infos = [
            ifi("eth0", ETH, true, true),
            ifi("docker0", ETH, false, true),
            ifi("veth1", ETH, true, false),
        ];
        assert_eq!(select(&infos).unwrap(), "eth0");
    }

    #[test]
    fn zero_candidates_is_error() {
        let infos = [ifi("lo", LO, true, true)];
        assert!(matches!(select(&infos), Err(SelectError::NoCandidate)));
    }

    #[test]
    fn multiple_candidates_is_ambiguous_and_lists_names() {
        let infos = [ifi("eth0", ETH, true, true), ifi("wlan0", ETH, true, true)];
        match select(&infos) {
            Err(SelectError::Ambiguous(names)) => assert_eq!(names, vec!["eth0", "wlan0"]),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }
}
