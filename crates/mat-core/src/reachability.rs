//! commissioned ノードの台帳エントリを、ライブ mDNS インスタンス一覧に照合して
//! 到達性を判定する純ロジック。副作用なし（プロセス起動はバイナリ側 `probe` が担う）。
//!
//! 照合は `node_id` で行う（台帳 node_id は自 fabric が採番した値）。同一 node_id が
//! 別 fabric で広告されると偽陽性の可能性があるが、当面はベストエフォート。fabric
//! 厳密判別には compressed-fabric-id（CASE を要する重い経路）が必要なため避ける。

use crate::diag::MatterInstance;

/// 1 ノードの照合結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeReachability {
    /// ライブ mDNS に当該 node_id の広告が見つかったか。
    pub reachable: bool,
    /// 解決できたライブアドレス（見つからなければ None）。台帳アドレスが一致
    /// インスタンスの addresses に含まれればそれを、無ければ先頭アドレスを返す。
    pub live_address: Option<String>,
}

/// `node_id` でライブインスタンスに照合する。
pub fn resolve(
    node_id: u64,
    ledger_address: Option<&str>,
    instances: &[MatterInstance],
) -> NodeReachability {
    let matched: Vec<&MatterInstance> = instances.iter().filter(|i| i.node_id == node_id).collect();
    if matched.is_empty() {
        return NodeReachability {
            reachable: false,
            live_address: None,
        };
    }
    // 台帳アドレスが一致インスタンスの addresses に含まれればそれを優先（安定性）。
    if let Some(addr) = ledger_address {
        if matched
            .iter()
            .any(|i| i.addresses.iter().any(|a| a == addr))
        {
            return NodeReachability {
                reachable: true,
                live_address: Some(addr.to_string()),
            };
        }
    }
    // 含まれなければ最初の非空アドレスの先頭を採る（announce のみなら None）。
    let live = matched
        .iter()
        .flat_map(|i| i.addresses.iter())
        .next()
        .cloned();
    NodeReachability {
        reachable: true,
        live_address: live,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(node_id: u64, addrs: &[&str]) -> MatterInstance {
        MatterInstance {
            compressed_fabric: "00AABB1122CC3344".to_string(),
            node_id,
            addresses: addrs.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn matched_returns_reachable_with_first_live_address() {
        let instances = [inst(5, &["192.0.2.99"])];
        let r = resolve(5, Some("192.0.2.10"), &instances);
        assert!(r.reachable);
        assert_eq!(r.live_address, Some("192.0.2.99".to_string()));
    }

    #[test]
    fn matched_prefers_ledger_address_when_present() {
        let instances = [inst(5, &["192.0.2.99", "192.0.2.10"])];
        let r = resolve(5, Some("192.0.2.10"), &instances);
        assert!(r.reachable);
        assert_eq!(r.live_address, Some("192.0.2.10".to_string()));
    }

    #[test]
    fn not_matched_returns_unreachable() {
        let instances = [inst(255, &["192.0.2.50"])];
        let r = resolve(5, Some("192.0.2.10"), &instances);
        assert!(!r.reachable);
        assert_eq!(r.live_address, None);
    }

    #[test]
    fn matched_announce_only_is_reachable_without_address() {
        let instances = [inst(5, &[])];
        let r = resolve(5, None, &instances);
        assert!(r.reachable);
        assert_eq!(r.live_address, None);
    }
}
