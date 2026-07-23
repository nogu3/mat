//! `mat diag mesh` の純ロジック: per-node 収集結果（cluster 53 スナップショット +
//! cluster 0x33 自己同定）から Thread メッシュのトポロジーグラフを組み立てる。
//! 副作用なし。収集（CASE/IM）は `mat` 側 `native_direct::diag_mesh_probe` の担当。

use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

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

/// `mat diag mesh` の出力全体（`timestamp` は emit 側が付与）。
#[derive(Debug, Serialize)]
pub struct MeshGraph {
    pub network: NetworkSummary,
    pub nodes: Vec<MeshNode>,
    pub edges: Vec<MeshEdge>,
}

#[derive(Debug, Serialize)]
pub struct NetworkSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<u64>,
    /// 全 probe 済みノードで観測した partition-id（複数 = メッシュ分断の兆候）。
    pub partition_ids: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leader_router_id: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MeshNode {
    /// 安定キー: `ext:<HEX16>` / `rloc:0x….` / `node:<node_id>`（同定不能 fabric ノード）。
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rloc16: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router_id: Option<u8>,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// fabric ノードのみ。未知参加者は省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<ProbeErrorOut>,
}

#[derive(Debug, Serialize)]
pub struct ProbeErrorOut {
    pub kind: ErrorKind,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct MeshEdge {
    pub a: String,
    pub b: String,
    /// a の neighbor-table の b 行（= a が受信した b の電波品質）。null = 観測なし。
    pub a_sees_b: Option<LinkMetrics>,
    pub b_sees_a: Option<LinkMetrics>,
    /// route-table 由来（LinkEstablished = true の行のみ。a 視点優先）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteMetrics>,
}

#[derive(Debug, Serialize)]
pub struct LinkMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lqi: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_rssi: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_rssi: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_error_rate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct RouteMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lqi_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lqi_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_cost: Option<u64>,
}

/// テーブル（`neighbor_table` / `route_table`）の行を object だけに絞って返す。
fn table_rows<'a>(fields: &'a Map<String, Value>, key: &str) -> Vec<&'a Map<String, Value>> {
    fields
        .get(key)
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

fn link_metrics(row: &Map<String, Value>) -> LinkMetrics {
    LinkMetrics {
        lqi: row.get("Lqi").and_then(Value::as_u64),
        avg_rssi: row.get("AverageRssi").and_then(Value::as_i64),
        last_rssi: row.get("LastRssi").and_then(Value::as_i64),
        frame_error_rate: row.get("FrameErrorRate").and_then(Value::as_u64),
        age: row.get("Age").and_then(Value::as_u64),
    }
}

fn ordered(x: String, y: String) -> (String, String) {
    if x <= y {
        (x, y)
    } else {
        (y, x)
    }
}

/// テーブル観測から集めた未知参加者の証拠。
#[derive(Default)]
struct Part {
    rloc16: Option<u16>,
    seen_as_router: bool,
    seen_as_child: bool,
    rx_on_when_idle: Option<bool>,
    route_router_id: Option<u64>,
}

/// per-node 収集結果からグラフを組み立てる（純関数）。
///
/// - 参加者は ExtAddress（正準 16 桁大文字 hex）キーで台帳化。fabric ノードの
///   自己同定（identity）とテーブル行の両方から集める。
/// - エッジは無向 1 本に双方向実測を併記。自己同定できないノードの行は
///   張れない（他ノード視点のみ成立）。
pub fn build_graph(inputs: &[NodeInput], thread_labels: &BTreeMap<String, String>) -> MeshGraph {
    // 1. network サマリ（最初に読めた値を採用）+ mesh-local-prefix。
    let mut name = None;
    let mut channel = None;
    let mut leader_router_id = None;
    let mut ml_prefix: Option<String> = None;
    let mut partition_ids: Vec<u64> = Vec::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        let f = &p.thread;
        if name.is_none() {
            name = f
                .get("network_name")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if channel.is_none() {
            channel = f.get("channel").and_then(Value::as_u64);
        }
        if leader_router_id.is_none() {
            leader_router_id = f.get("leader_router_id").and_then(Value::as_u64);
        }
        if ml_prefix.is_none() {
            ml_prefix = f
                .get("mesh_local_prefix")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if let Some(pid) = f.get("partition_id").and_then(Value::as_u64) {
            if !partition_ids.contains(&pid) {
                partition_ids.push(pid);
            }
        }
    }
    partition_ids.sort_unstable();

    // 2. fabric ノードの自己同定（node_id → 正準 ext hex / rloc16）。
    let mut self_ext: BTreeMap<u64, String> = BTreeMap::new();
    let mut self_rloc: BTreeMap<u64, u16> = BTreeMap::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        let Some(id) = &p.identity else { continue };
        let Some(ext) = canon_ext_hex(&id.ext_address) else {
            continue;
        };
        self_ext.insert(inp.node_id, ext);
        if let Some(pref) = &ml_prefix {
            if let Some(r) = derive_rloc16(pref, &id.ipv6) {
                self_rloc.insert(inp.node_id, r);
            }
        }
    }

    // 3. 参加者台帳（ext hex → 証拠）。
    let mut parts: BTreeMap<String, Part> = BTreeMap::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        for row in table_rows(&p.thread, "neighbor_table") {
            let Some(ext) = row
                .get("ExtAddress")
                .and_then(Value::as_u64)
                .map(ext_hex_from_u64)
            else {
                continue;
            };
            let part = parts.entry(ext).or_default();
            if part.rloc16.is_none() {
                part.rloc16 = row
                    .get("Rloc16")
                    .and_then(Value::as_u64)
                    .and_then(|v| u16::try_from(v).ok());
            }
            if row.get("IsChild").and_then(Value::as_bool) == Some(true) {
                part.seen_as_child = true;
            }
            if let Some(rx) = row.get("RxOnWhenIdle").and_then(Value::as_bool) {
                part.rx_on_when_idle = Some(rx);
            }
        }
        for row in table_rows(&p.thread, "route_table") {
            let Some(ext) = row
                .get("ExtAddress")
                .and_then(Value::as_u64)
                .map(ext_hex_from_u64)
            else {
                continue;
            };
            let part = parts.entry(ext).or_default();
            if part.rloc16.is_none() {
                part.rloc16 = row
                    .get("Rloc16")
                    .and_then(Value::as_u64)
                    .and_then(|v| u16::try_from(v).ok());
            }
            part.seen_as_router = true;
            if part.route_router_id.is_none() {
                part.route_router_id = row.get("RouterId").and_then(Value::as_u64);
            }
        }
    }

    // 4. エッジ集約（無向、キーは辞書順ペア）。
    #[derive(Default)]
    struct EdgeAcc {
        a_sees_b: Option<LinkMetrics>,
        b_sees_a: Option<LinkMetrics>,
        route_a: Option<RouteMetrics>,
        route_b: Option<RouteMetrics>,
    }
    let mut edges: BTreeMap<(String, String), EdgeAcc> = BTreeMap::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        let Some(my_ext) = self_ext.get(&inp.node_id) else {
            continue;
        };
        for row in table_rows(&p.thread, "neighbor_table") {
            let Some(other) = row
                .get("ExtAddress")
                .and_then(Value::as_u64)
                .map(ext_hex_from_u64)
            else {
                continue;
            };
            if other == *my_ext {
                continue;
            }
            let key = ordered(my_ext.clone(), other);
            let mine_is_a = key.0 == *my_ext;
            let acc = edges.entry(key).or_default();
            let m = link_metrics(row);
            if mine_is_a {
                acc.a_sees_b = Some(m);
            } else {
                acc.b_sees_a = Some(m);
            }
        }
        for row in table_rows(&p.thread, "route_table") {
            if row.get("LinkEstablished").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let Some(other) = row
                .get("ExtAddress")
                .and_then(Value::as_u64)
                .map(ext_hex_from_u64)
            else {
                continue;
            };
            if other == *my_ext {
                continue;
            }
            let key = ordered(my_ext.clone(), other);
            let mine_is_a = key.0 == *my_ext;
            let acc = edges.entry(key).or_default();
            let r = RouteMetrics {
                lqi_in: row.get("LQIIn").and_then(Value::as_u64),
                lqi_out: row.get("LQIOut").and_then(Value::as_u64),
                path_cost: row.get("PathCost").and_then(Value::as_u64),
            };
            if mine_is_a {
                acc.route_a = Some(r);
            } else {
                acc.route_b = Some(r);
            }
        }
    }

    // 5. ノード出力: fabric ノード（入力順）→ 未知参加者（ext 昇順）。
    let mut nodes: Vec<MeshNode> = Vec::new();
    let mut consumed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for inp in inputs {
        let ext = self_ext.get(&inp.node_id).cloned();
        if let Some(e) = &ext {
            consumed.insert(e.clone());
        }
        let rloc16 = self_rloc.get(&inp.node_id).copied().or_else(|| {
            ext.as_ref()
                .and_then(|e| parts.get(e))
                .and_then(|p| p.rloc16)
        });
        let role = match &inp.probe {
            Ok(p) => p
                .thread
                .get("routing_role")
                .and_then(Value::as_i64)
                .map(role_from_routing_role)
                .unwrap_or("unknown"),
            Err(_) => "unknown",
        };
        let id = match (&ext, rloc16) {
            (Some(e), _) => format!("ext:{e}"),
            (None, Some(r)) => format!("rloc:{}", rloc16_str(r)),
            (None, None) => format!("node:{}", inp.node_id),
        };
        nodes.push(MeshNode {
            id,
            ext_address: ext.clone(),
            rloc16: rloc16.map(rloc16_str),
            router_id: rloc16.and_then(router_id_of),
            role: role.to_string(),
            node_id: Some(inp.node_id),
            alias: inp.alias.clone(),
            label: ext.as_ref().and_then(|e| thread_labels.get(e)).cloned(),
            probed: Some(inp.probe.is_ok()),
            probe_error: inp.probe.as_ref().err().map(|f| ProbeErrorOut {
                kind: f.kind,
                detail: f.detail.clone(),
            }),
        });
    }
    for (ext, part) in &parts {
        if consumed.contains(ext) {
            continue;
        }
        let router_id: Option<u8> = part
            .route_router_id
            .and_then(|v| u8::try_from(v).ok())
            .or_else(|| part.rloc16.and_then(router_id_of));
        let is_leader = leader_router_id.is_some() && router_id.map(u64::from) == leader_router_id;
        let role = if part.seen_as_router || router_id.is_some() {
            if is_leader {
                "leader"
            } else {
                "router"
            }
        } else if part.seen_as_child {
            if part.rx_on_when_idle == Some(false) {
                "sed"
            } else {
                "child"
            }
        } else {
            "unknown"
        };
        nodes.push(MeshNode {
            id: format!("ext:{ext}"),
            ext_address: Some(ext.clone()),
            rloc16: part.rloc16.map(rloc16_str),
            router_id: if role == "sed" || role == "child" {
                None
            } else {
                router_id
            },
            role: role.to_string(),
            node_id: None,
            alias: None,
            label: thread_labels.get(ext).cloned(),
            probed: None,
            probe_error: None,
        });
    }

    // 6. エッジ出力（キー昇順、route は a 視点優先）。
    let edges = edges
        .into_iter()
        .map(|((a, b), acc)| MeshEdge {
            a: format!("ext:{a}"),
            b: format!("ext:{b}"),
            a_sees_b: acc.a_sees_b,
            b_sees_a: acc.b_sees_a,
            route: acc.route_a.or(acc.route_b),
        })
        .collect();

    MeshGraph {
        network: NetworkSummary {
            name,
            channel,
            partition_ids,
            leader_router_id,
        },
        nodes,
        edges,
    }
}

/// RLOC16 から RouterId。router アドレス（下位 10bit = 0）のみ Some。
fn router_id_of(rloc16: u16) -> Option<u8> {
    ((rloc16 & 0x03FF) == 0).then_some((rloc16 >> 10) as u8)
}

/// "0x1400" 形式。
fn rloc16_str(r: u16) -> String {
    format!("{r:#06x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// fabric ノードの probe 成功入力を組む fixture。
    fn fabric_input(
        node_id: u64,
        alias: Option<&str>,
        ext: &str,
        rloc_addr: &str,
        thread_extra: Vec<(&str, Value)>,
    ) -> NodeInput {
        let mut thread = Map::new();
        thread.insert("network_name".into(), json!("TestNet"));
        thread.insert("channel".into(), json!(25));
        thread.insert("partition_id".into(), json!(123456));
        thread.insert("leader_router_id".into(), json!(8));
        thread.insert("mesh_local_prefix".into(), json!("fd00112233445566"));
        thread.insert("routing_role".into(), json!(5));
        thread.insert("neighbor_table".into(), json!([]));
        thread.insert("route_table".into(), json!([]));
        for (k, v) in thread_extra {
            thread.insert(k.into(), v);
        }
        NodeInput {
            node_id,
            alias: alias.map(str::to_string),
            probe: Ok(ProbeData {
                thread,
                identity: Some(Identity {
                    ext_address: ext.to_string(),
                    ipv6: vec![rloc_addr.to_string()],
                }),
            }),
        }
    }

    #[test]
    fn build_graph_two_fabric_nodes_and_unknown_br() {
        // node42 (ext 0011..., rloc 0x1400) が node7 (ext 8899..., rloc 0x0c01=child)
        // と BR (ext AABB..., rloc 0x2000, route-table 経由) を見る。
        let n16 = fabric_input(
            42,
            Some("hall_motion"),
            "0011223344556677",
            "fd00112233445566000000fffe001400",
            vec![
                (
                    "neighbor_table",
                    json!([
                        {"ExtAddress": 0x8899AABBCCDDEEFFu64, "Rloc16": 0x0c01, "Lqi": 140,
                         "AverageRssi": -60, "LastRssi": -58, "FrameErrorRate": 2, "Age": 12,
                         "RxOnWhenIdle": false, "IsChild": true},
                        {"ExtAddress": 0xAABBCCDDEEFF0011u64, "Rloc16": 0x2000, "Lqi": 200,
                         "AverageRssi": -50, "LastRssi": -49, "FrameErrorRate": 0, "Age": 3,
                         "RxOnWhenIdle": true, "IsChild": false}
                    ]),
                ),
                (
                    "route_table",
                    json!([
                        {"ExtAddress": 0xAABBCCDDEEFF0011u64, "Rloc16": 0x2000, "RouterId": 8,
                         "PathCost": 1, "LQIIn": 3, "LQIOut": 3, "Allocated": true,
                         "LinkEstablished": true}
                    ]),
                ),
            ],
        );
        let n5 = fabric_input(
            7,
            None,
            "8899AABBCCDDEEFF",
            "fd00112233445566000000fffe000c01",
            vec![
                ("routing_role", json!(3)),
                (
                    "neighbor_table",
                    json!([
                        {"ExtAddress": 0x0011223344556677u64, "Rloc16": 0x1400, "Lqi": 130,
                         "AverageRssi": -65, "LastRssi": -64, "FrameErrorRate": 5, "Age": 8,
                         "RxOnWhenIdle": true, "IsChild": false}
                    ]),
                ),
            ],
        );
        let labels = BTreeMap::from([("AABBCCDDEEFF0011".to_string(), "otbr-br".to_string())]);
        let g = build_graph(&[n16, n5], &labels);

        // network サマリ
        assert_eq!(g.network.name.as_deref(), Some("TestNet"));
        assert_eq!(g.network.channel, Some(25));
        assert_eq!(g.network.partition_ids, vec![123456]);
        assert_eq!(g.network.leader_router_id, Some(8));

        // ノード: fabric 2 + unknown BR 1
        assert_eq!(g.nodes.len(), 3);
        let n16o = g.nodes.iter().find(|n| n.node_id == Some(42)).unwrap();
        assert_eq!(n16o.id, "ext:0011223344556677");
        assert_eq!(n16o.rloc16.as_deref(), Some("0x1400"));
        assert_eq!(n16o.router_id, Some(5));
        assert_eq!(n16o.role, "router");
        assert_eq!(n16o.alias.as_deref(), Some("hall_motion"));
        assert_eq!(n16o.probed, Some(true));
        let br = g.nodes.iter().find(|n| n.node_id.is_none()).unwrap();
        assert_eq!(br.id, "ext:AABBCCDDEEFF0011");
        assert_eq!(br.label.as_deref(), Some("otbr-br"));
        // RouterId 8 = leader_router_id → leader マーク
        assert_eq!(br.role, "leader");
        assert_eq!(br.probed, None);

        // エッジ: n16–n5（双方向）と n16–BR（片方向 + route）
        assert_eq!(g.edges.len(), 2);
        let e_n16_n5 = g
            .edges
            .iter()
            .find(|e| e.a == "ext:0011223344556677" && e.b == "ext:8899AABBCCDDEEFF")
            .unwrap();
        // a=n16 の neighbor 行（b=n5 を測った値）が a_sees_b
        assert_eq!(e_n16_n5.a_sees_b.as_ref().unwrap().lqi, Some(140));
        assert_eq!(e_n16_n5.b_sees_a.as_ref().unwrap().avg_rssi, Some(-65));
        let e_n16_br = g
            .edges
            .iter()
            .find(|e| e.b == "ext:AABBCCDDEEFF0011")
            .unwrap();
        assert_eq!(e_n16_br.a_sees_b.as_ref().unwrap().lqi, Some(200));
        assert!(e_n16_br.b_sees_a.is_none());
        assert_eq!(e_n16_br.route.as_ref().unwrap().path_cost, Some(1));
    }

    #[test]
    fn build_graph_probe_failure_yields_node_fallback_id() {
        let bad = NodeInput {
            node_id: 7,
            alias: None,
            probe: Err(ProbeFailure {
                kind: ErrorKind::Unreachable,
                detail: "Node 7 is unreachable".into(),
            }),
        };
        let g = build_graph(&[bad], &BTreeMap::new());
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "node:7");
        assert_eq!(g.nodes[0].probed, Some(false));
        assert_eq!(g.nodes[0].role, "unknown");
        let pe = g.nodes[0].probe_error.as_ref().unwrap();
        assert_eq!(pe.kind, ErrorKind::Unreachable);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn build_graph_route_ignores_unestablished_links() {
        let n1 = fabric_input(
            1,
            None,
            "0011223344556677",
            "fd00112233445566000000fffe001400",
            vec![(
                "route_table",
                json!([
                    {"ExtAddress": 0xAABBCCDDEEFF0011u64, "Rloc16": 0x2000, "RouterId": 8,
                     "PathCost": 2, "LQIIn": 0, "LQIOut": 0, "Allocated": true,
                     "LinkEstablished": false}
                ]),
            )],
        );
        let g = build_graph(&[n1], &BTreeMap::new());
        // 直リンク未確立 → エッジなし。ただし参加者としてはノード化される。
        assert!(g.edges.is_empty());
        assert_eq!(g.nodes.len(), 2);
        let br = g.nodes.iter().find(|n| n.node_id.is_none()).unwrap();
        assert_eq!(br.role, "leader"); // RouterId 8 = leader_router_id
    }

    #[test]
    fn build_graph_sed_detection_from_neighbor_row() {
        let n1 = fabric_input(
            1,
            None,
            "0011223344556677",
            "fd00112233445566000000fffe001400",
            vec![(
                "neighbor_table",
                json!([
                    {"ExtAddress": 0x1122334455667788u64, "Rloc16": 0x1401, "Lqi": 100,
                     "AverageRssi": -70, "LastRssi": -70, "FrameErrorRate": 10, "Age": 1,
                     "RxOnWhenIdle": false, "IsChild": true}
                ]),
            )],
        );
        let g = build_graph(&[n1], &BTreeMap::new());
        let sed = g.nodes.iter().find(|n| n.node_id.is_none()).unwrap();
        assert_eq!(sed.role, "sed");
        assert_eq!(sed.rloc16.as_deref(), Some("0x1401"));
        assert_eq!(sed.router_id, None);
    }
}
