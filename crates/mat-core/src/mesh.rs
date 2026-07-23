//! `mat diag mesh` の純ロジック: per-node 収集結果（cluster 53 スナップショット +
//! cluster 0x33 自己同定）から Thread メッシュのトポロジーグラフを組み立てる。
//! 副作用なし。収集（CASE/IM）は `mat` 側 `native_direct::diag_mesh_probe` の担当。

use serde_json::{Map, Value};

use crate::error::ErrorKind;

/// per-node 収集の入力 1 件。
#[derive(Debug)]
pub struct NodeInput {
    pub node_id: u64,
    /// aliases.toml の node alias 逆引き結果。
    pub alias: Option<String>,
    pub probe: Result<ProbeData, ProbeFailure>,
}

/// probe 成功時のデータ。
#[derive(Debug)]
pub struct ProbeData {
    /// `ops::diag_thread` の fields（`neighbor_table` / `route_table` /
    /// `routing_role` / `partition_id` / `leader_router_id` /
    /// `mesh_local_prefix` / `network_name` / `channel`）。
    pub thread: Map<String, Value>,
    /// cluster 0x33 由来の自己同定（読めなければ None — エッジは他ノード視点のみ）。
    pub identity: Option<Identity>,
}

/// cluster 0x33 NetworkInterfaces の Thread インターフェース情報。
#[derive(Debug, Clone)]
pub struct Identity {
    /// HardwareAddress（hex 文字列、大文字小文字不問 — 正準化はこちらで行う）。
    pub ext_address: String,
    /// IPv6Addresses（各 32 桁 hex）。
    pub ipv6: Vec<String>,
}

/// probe 失敗時の記録（JSON の `probe_error` へ）。
#[derive(Debug)]
pub struct ProbeFailure {
    pub kind: ErrorKind,
    pub detail: String,
}

/// テーブル行の ExtAddress（u64）→ 正準 16 桁大文字 hex。
pub fn ext_hex_from_u64(v: u64) -> String {
    format!("{v:016X}")
}

/// hex 文字列を正準形（大文字 16 桁）へ。16 桁 hex でなければ None。
pub fn canon_ext_hex(s: &str) -> Option<String> {
    (s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_uppercase())
}

/// mesh-local-prefix（hex、先頭 8B を prefix とみなす）と IPv6 一覧から自 RLOC16 を
/// 導出。RLOC = `<prefix 8B> 00 00 00 ff fe 00 <rloc16 2B>`。
pub fn derive_rloc16(mesh_local_prefix_hex: &str, ipv6_hex: &[String]) -> Option<u16> {
    let p = mesh_local_prefix_hex.to_ascii_lowercase();
    if p.len() < 16 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let prefix = &p[..16];
    for a in ipv6_hex {
        let a = a.to_ascii_lowercase();
        if a.len() == 32 && a.starts_with(prefix) && &a[16..28] == "000000fffe00" {
            return u16::from_str_radix(&a[28..32], 16).ok();
        }
    }
    None
}

/// cluster 53 RoutingRoleEnum → 出力 role 文字列。
pub fn role_from_routing_role(v: i64) -> &'static str {
    match v {
        6 => "leader",
        5 => "router",
        4 => "reed",
        3 => "child",
        2 => "sed",
        _ => "unknown",
    }
}

/// RLOC16 から RouterId。router アドレス（下位 10bit = 0）のみ Some。
#[allow(dead_code)] // Task 3 (build_graph) で使用
fn router_id_of(rloc16: u16) -> Option<u8> {
    ((rloc16 & 0x03FF) == 0).then_some((rloc16 >> 10) as u8)
}

/// "0x1400" 形式。
#[allow(dead_code)] // Task 3 (build_graph) で使用
fn rloc16_str(r: u16) -> String {
    format!("{r:#06x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_hex_from_u64_is_upper_16() {
        assert_eq!(ext_hex_from_u64(0x0011223344556677), "0011223344556677");
        assert_eq!(ext_hex_from_u64(0xAABBCCDDEEFF0011), "AABBCCDDEEFF0011");
    }

    #[test]
    fn canon_ext_hex_normalizes_or_rejects() {
        assert_eq!(
            canon_ext_hex("aabbccddeeff0011").as_deref(),
            Some("AABBCCDDEEFF0011")
        );
        assert_eq!(canon_ext_hex("zzbbccddeeff0011"), None);
        assert_eq!(canon_ext_hex("aabb"), None);
    }

    #[test]
    fn derive_rloc16_finds_rloc_address() {
        // prefix fd00112233445566 + 000000fffe00 + 1400
        let addrs = vec![
            "fe800000000000000011223344556677".to_string(), // link-local
            "fd001122334455660000000000abcdef".to_string(), // ML-EID（fffe00 なし）
            "fd00112233445566000000fffe001400".to_string(), // RLOC
        ];
        assert_eq!(derive_rloc16("fd00112233445566", &addrs), Some(0x1400));
    }

    #[test]
    fn derive_rloc16_none_without_match() {
        let addrs = vec!["fe800000000000000011223344556677".to_string()];
        assert_eq!(derive_rloc16("fd00112233445566", &addrs), None);
        // prefix が 16 桁 hex 未満なら常に None
        assert_eq!(derive_rloc16("fd00", &addrs), None);
    }

    #[test]
    fn derive_rloc16_tolerates_long_prefix_encoding() {
        // 一部デバイスが prefix を長い octstr で返しても先頭 8B を prefix とみなす
        let addrs = vec!["fd00112233445566000000fffe002c00".to_string()];
        assert_eq!(derive_rloc16("fd0011223344556600", &addrs), Some(0x2c00));
    }

    #[test]
    fn role_mapping_matches_cluster53_enum() {
        assert_eq!(role_from_routing_role(6), "leader");
        assert_eq!(role_from_routing_role(5), "router");
        assert_eq!(role_from_routing_role(4), "reed");
        assert_eq!(role_from_routing_role(3), "child");
        assert_eq!(role_from_routing_role(2), "sed");
        assert_eq!(role_from_routing_role(0), "unknown");
    }

    #[test]
    fn router_id_only_for_router_addresses() {
        assert_eq!(router_id_of(0x1400), Some(5)); // 0x1400 >> 10 = 5
        assert_eq!(router_id_of(0x1401), None); // child index 付きは router ではない
    }
}
