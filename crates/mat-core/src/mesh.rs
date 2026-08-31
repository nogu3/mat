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
    /// cluster 0x33 由来の自己同定（読めなければ None — 自視点エッジは
    /// `node:<node_id>` を頂点として張られる）。
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

/// mesh-local-prefix の hex を 8B prefix の 16 桁 hex（小文字）へ正規化。
/// 実機観測: 8B 素形（16 桁）と長さ前置形（0x40=64bit 長 + 8B = 18 桁、NL68 系）の
/// 両方が存在する。それ以外の形は None。
fn canon_ml_prefix(hex: &str) -> Option<String> {
    let h = hex.to_ascii_lowercase();
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        16 => Some(h),
        18 if h.starts_with("40") => Some(h[2..].to_string()),
        _ => None,
    }
}

/// mesh-local-prefix（hex、8B 素形 or 長さ前置形。`canon_ml_prefix` で正規化）と
/// IPv6 一覧から自 RLOC16 を導出。RLOC = `<prefix 8B> 00 00 00 ff fe 00 <rloc16 2B>`。
pub fn derive_rloc16(mesh_local_prefix_hex: &str, ipv6_hex: &[String]) -> Option<u16> {
    let prefix = canon_ml_prefix(mesh_local_prefix_hex)?;
    for a in ipv6_hex {
        let a = a.to_ascii_lowercase();
        if a.len() == 32 && a.starts_with(&prefix) && &a[16..28] == "000000fffe00" {
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
    /// 安定キー: `ext:<HEX16>`、または `node:<node_id>`（同定不能 fabric ノード）。
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
    /// 自己同定が cluster 0x33 以外の経路で決まったときの注記。
    /// `"route-table"` = 自分の route-table 自己行が実 ExtAddress を持っていた、
    /// `"rloc16"` = 自 RLOC16 と他ノード観測行（実 ext + Rloc16）の一意一致。
    /// 0x33 の HardwareAddress で普通に自己同定できたノードには付かない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identified_by: Option<String>,
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

/// テーブル行の ExtAddress を正準 hex で。0（ゴミ行 / ESP32 系の自己行）は None。
fn row_ext_hex(row: &Map<String, Value>) -> Option<String> {
    row.get("ExtAddress")
        .and_then(Value::as_u64)
        .filter(|v| *v != 0)
        .map(ext_hex_from_u64)
}

/// テーブル行の Rloc16（u16 に収まらない値は None）。
fn row_rloc16(row: &Map<String, Value>) -> Option<u16> {
    row.get("Rloc16")
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
}

/// route-table の「経路なし」行シグネチャ: NextHop=63(invalid) + PathCost=0。
/// 実機観測 (2026-09-01) では router/leader の自己行がこの形で載る。OpenThread
/// は経路喪失ルーター行も同じ形にする（SetNextHopToInvalid）ため、単体では
/// 自己行と断定できない — どちらであっても実在の隣接観測ではないので、
/// 参加者台帳・エッジ・rloc 観測台帳の入力からは除外する。
fn is_routeless_row(row: &Map<String, Value>) -> bool {
    row.get("NextHop").and_then(Value::as_u64) == Some(63)
        && row.get("PathCost").and_then(Value::as_u64) == Some(0)
}

/// 自己行候補と経路喪失行を分ける Age 上限。自己行は「今の自分」なので実機
/// 観測では 0〜1 秒（3 ベンダー確認）。経路喪失行は最後に聞こえてから時間が
/// 経っているのが普通なので、大きい Age の 63/0 行は自己行とみなさない。
const SELF_ROW_MAX_AGE: u64 = 8;

/// 同一値を 2 ノード以上が主張したら、物理的にあり得ない衝突として全員落とす
/// （どの主張も信用できないため、片方だけ残す判断はしない）。
fn retain_unique_values<V: Ord + Clone>(m: &mut BTreeMap<u64, V>) {
    let mut counts: BTreeMap<V, u32> = BTreeMap::new();
    for v in m.values() {
        *counts.entry(v.clone()).or_insert(0) += 1;
    }
    m.retain(|_, v| counts[v] < 2);
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

/// fabric ノードのグラフ頂点 id。自己同定できれば ext:、できなければ node:。
fn fabric_vertex_id(node_id: u64, self_ext: &BTreeMap<u64, String>) -> String {
    match self_ext.get(&node_id) {
        Some(e) => format!("ext:{e}"),
        None => format!("node:{node_id}"),
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
///   自己同定（identity）とテーブル行の両方から集める。同一 ext を複数 fabric
///   ノードが自己同定した場合（実機 NL68 系 FW バグ）は物理的にあり得ない
///   衝突として全員無効化する。
/// - エッジは無向 1 本に双方向実測を併記。自視点は自己同定できれば `ext:`、
///   できなければ `node:<node_id>` を頂点として張る（同定不能でも観測は有効）。
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
                .and_then(canon_ml_prefix);
        }
        if let Some(pid) = f.get("partition_id").and_then(Value::as_u64) {
            if !partition_ids.contains(&pid) {
                partition_ids.push(pid);
            }
        }
    }
    partition_ids.sort_unstable();

    // 2. fabric ノードの自己同定（node_id → 正準 ext hex / rloc16）。
    // RLOC16 の IPv6 由来導出は ExtAddress 正準化の成否と独立に行う
    // （issue #13: ext が偽でも IPv6Addresses は実デバイス固有なので、
    // rloc 相関による救済の入力になる）。
    let mut self_ext: BTreeMap<u64, String> = BTreeMap::new();
    let mut self_rloc: BTreeMap<u64, u16> = BTreeMap::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        let Some(id) = &p.identity else { continue };
        if let Some(ext) = canon_ext_hex(&id.ext_address) {
            self_ext.insert(inp.node_id, ext);
        }
        // 自ノードの mesh_local_prefix を優先し、無ければ network サマリの
        // フォールバックを使う（両方 canon_ml_prefix 経由で正規化済み）。
        let prefix = p
            .thread
            .get("mesh_local_prefix")
            .and_then(Value::as_str)
            .and_then(canon_ml_prefix)
            .or_else(|| ml_prefix.clone());
        if let Some(pref) = &prefix {
            if let Some(r) = derive_rloc16(pref, &id.ipv6) {
                self_rloc.insert(inp.node_id, r);
            }
        }
    }

    // 実機 NL68 系で全台同一の工場 MAC（HardwareAddress）を申告する FW バグを
    // 観測: 同一 ext を 2 台以上の fabric ノードが自己同定した場合、物理的に
    // あり得ない衝突なので該当ノード全員の self_ext を無効化する。self_rloc は
    // 残す: IPv6Addresses はバグ FW でも実デバイス固有で、後段の rloc 相関
    // 救済の入力になる。同一 rloc16 の複数導出も同様に全員無効化する
    // （rloc16 はパーティション内で一意）。
    retain_unique_values(&mut self_ext);
    retain_unique_values(&mut self_rloc);

    // 2b. 自己同定できなかった probed ノードの救済（issue #13）。
    // 実機観測 (2026-09-01): router / leader の route-table には自分自身の行が
    // 入り、NextHop=63(invalid) + PathCost=0 + Age≈0 の形で載る（3 ベンダー
    // 確認。63/0 は経路喪失ルーター行と共通のため Age 上限で切り分ける）。
    // ExtAddress はベンダー依存で実値（そのまま自己同定に使える）か 0
    // （Rloc16 だけ載る）。Rloc16 しか取れないノードは、全 probed ノードの
    // テーブル観測行（実 ext + Rloc16）との一意一致で ext を確定する。
    // 証拠の強い順に確定し（自己行の実 ext → rloc16 相関）、複数 ext 候補・
    // 確定済み ext との衝突・複数ノードの同一 ext 解決・パーティション分断中の
    // rloc 相関、はすべて棄却する（推測での統合はしない — neighbor 集合
    // 類似度などのヒューリスティクスは実メッシュで BR と誤マージすることを
    // 確認済みのため不採用）。
    let mut ident_by: BTreeMap<u64, &'static str> = BTreeMap::new();
    {
        // 自己行候補の走査: 救済対象ノードの実 ext（direct_*）と自 rloc16
        // （rescue_rloc、自己行または IPv6 由来）を集める。
        let mut direct_ext: BTreeMap<u64, String> = BTreeMap::new();
        let mut direct_rloc: BTreeMap<u64, u16> = BTreeMap::new();
        let mut rescue_rloc: BTreeMap<u64, u16> = BTreeMap::new();
        for inp in inputs {
            let Ok(p) = &inp.probe else { continue };
            if self_ext.contains_key(&inp.node_id) {
                continue;
            }
            let role = p.thread.get("routing_role").and_then(Value::as_i64);
            if matches!(role, Some(5 | 6)) {
                let cands: Vec<&Map<String, Value>> = table_rows(&p.thread, "route_table")
                    .into_iter()
                    .filter(|r| {
                        is_routeless_row(r)
                            && r.get("Age").and_then(Value::as_u64).unwrap_or(0) <= SELF_ROW_MAX_AGE
                    })
                    .collect();
                // 自己行は 1 行しかあり得ない。複数一致は形が想定外として不使用。
                if let [row] = cands.as_slice() {
                    if let Some(ext) = row_ext_hex(row) {
                        direct_ext.insert(inp.node_id, ext);
                        if let Some(r) = row_rloc16(row) {
                            direct_rloc.insert(inp.node_id, r);
                        }
                        continue;
                    }
                    if let Some(r) = row_rloc16(row) {
                        rescue_rloc.insert(inp.node_id, r);
                        continue;
                    }
                }
            }
            // 自己行が使えなければ IPv6 由来の自 rloc16 で相関を試みる。
            if let Some(r) = self_rloc.get(&inp.node_id) {
                rescue_rloc.insert(inp.node_id, *r);
            }
        }

        // フェーズ 1: 自己行の実 ExtAddress（最強の証拠）を先に確定する。
        // 同一 ext の複数主張 / 確定済み ext との衝突は棄却。
        retain_unique_values(&mut direct_ext);
        direct_ext.retain(|_, e| !self_ext.values().any(|c| c == e));
        for (node, ext) in direct_ext {
            if let Some(r) = direct_rloc.get(&node) {
                self_rloc.insert(node, *r);
            }
            self_ext.insert(node, ext);
            ident_by.insert(node, "route-table");
        }

        // フェーズ 2: rloc16 相関。RLOC16 の一意性はパーティション内でしか
        // 成立しないため、分断中（partition_id 複数観測）はスキップ。
        if !rescue_rloc.is_empty() && partition_ids.len() <= 1 {
            // 観測台帳: 全 probed ノードのテーブル行から rloc16 → 実 ext 集合。
            // 63/0 行（自己行 or 経路喪失行）は第三者観測ではないので入れない。
            let mut rloc_obs: BTreeMap<u16, std::collections::BTreeSet<String>> = BTreeMap::new();
            for inp in inputs {
                let Ok(p) = &inp.probe else { continue };
                for key in ["neighbor_table", "route_table"] {
                    for row in table_rows(&p.thread, key) {
                        if is_routeless_row(row) {
                            continue;
                        }
                        let (Some(ext), Some(r)) = (row_ext_hex(row), row_rloc16(row)) else {
                            continue;
                        };
                        rloc_obs.entry(r).or_default().insert(ext);
                    }
                }
            }
            // フェーズ 1 の確定分も含めて claimed（stale な相関が自己行で
            // 確定した ext を巻き添えにしないよう、相関側だけを棄却する）。
            let claimed: std::collections::BTreeSet<String> = self_ext.values().cloned().collect();
            let mut correlated: BTreeMap<u64, String> = BTreeMap::new();
            for (node, r) in &rescue_rloc {
                let Some(exts) = rloc_obs.get(r) else {
                    continue;
                };
                // 厳格一意: その rloc16 の観測 ext がちょうど 1 つで、かつ
                // 確定済みノードの ext でないこと。
                if exts.len() != 1 {
                    continue;
                }
                let ext = exts.first().expect("len checked");
                if claimed.contains(ext) {
                    continue;
                }
                correlated.insert(*node, ext.clone());
            }
            // 複数ノードが同一 ext へ解決したら全員棄却。
            retain_unique_values(&mut correlated);
            for (node, ext) in correlated {
                self_rloc.insert(node, rescue_rloc[&node]);
                self_ext.insert(node, ext);
                ident_by.insert(node, "rloc16");
            }
        }
    }
    // 救済が成立しなかったノードの rloc16 は出力しない: 同じ電波実体の孤児
    // ext 頂点が同じ rloc を持ち得るため、二重表示は下流の rloc16 結合を壊す
    // （従来どおり node: 頂点は rloc16 を持たない）。救済で持ち込まれた rloc が
    // 既存の主張と衝突した場合も表示だけ取り下げる（ext 同定自体は維持）。
    self_rloc.retain(|n, _| self_ext.contains_key(n));
    retain_unique_values(&mut self_rloc);

    // 3. 参加者台帳（ext hex → 証拠）。
    let mut parts: BTreeMap<String, Part> = BTreeMap::new();
    for inp in inputs {
        let Ok(p) = &inp.probe else { continue };
        for row in table_rows(&p.thread, "neighbor_table") {
            // 実機で ExtAddress=0 のゴミ行を観測することがあるため除外。
            let Some(ext) = row_ext_hex(row) else {
                continue;
            };
            let part = parts.entry(ext).or_default();
            if part.rloc16.is_none() {
                part.rloc16 = row_rloc16(row);
            }
            if row.get("IsChild").and_then(Value::as_bool) == Some(true) {
                part.seen_as_child = true;
            }
            if let Some(rx) = row.get("RxOnWhenIdle").and_then(Value::as_bool) {
                part.rx_on_when_idle = Some(rx);
            }
        }
        for row in table_rows(&p.thread, "route_table") {
            // 63/0 行（自己行 or 経路喪失行）は実在の観測ではないので参加者に
            // しない — 自分の自己行から自分の幻影 ext: 頂点が生えるのを防ぐ。
            // ExtAddress=0 のゴミ行も除外。
            if is_routeless_row(row) {
                continue;
            }
            let Some(ext) = row_ext_hex(row) else {
                continue;
            };
            let part = parts.entry(ext).or_default();
            if part.rloc16.is_none() {
                part.rloc16 = row_rloc16(row);
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
        // 自己同定できていれば ext:、できなくても node:<id> で自視点のエッジを張る
        // （2026-07-23 実機 E2E 対応: 以前は自己同定不能ノードはエッジを持てな
        // かったが、テーブル行自体は有効な観測なので node: 頂点に付け替える）。
        let my_id = fabric_vertex_id(inp.node_id, &self_ext);
        let my_ext = self_ext.get(&inp.node_id);
        for row in table_rows(&p.thread, "neighbor_table") {
            // 実機で ExtAddress=0 のゴミ行を観測することがあるため除外。
            let Some(other_ext) = row_ext_hex(row) else {
                continue;
            };
            // 自己同定済みなら自分自身を指す行を弾く（同定不能時は自己参照を
            // 判定できないが、行は常に ext ベースなので自己ループは生じない）。
            if my_ext == Some(&other_ext) {
                continue;
            }
            let other_id = format!("ext:{other_ext}");
            let key = ordered(my_id.clone(), other_id);
            let mine_is_a = key.0 == my_id;
            let acc = edges.entry(key).or_default();
            let m = link_metrics(row);
            if mine_is_a {
                acc.a_sees_b = Some(m);
            } else {
                acc.b_sees_a = Some(m);
            }
        }
        for row in table_rows(&p.thread, "route_table") {
            // 63/0 行は LinkEstablished=false でもあるため下の条件で落ちるが、
            // 意図を明示して除外しておく（自己行 or 経路喪失行）。
            if is_routeless_row(row)
                || row.get("LinkEstablished").and_then(Value::as_bool) != Some(true)
            {
                continue;
            }
            // 実機で ExtAddress=0 のゴミ route 行を観測することがあるため除外。
            let Some(other_ext) = row_ext_hex(row) else {
                continue;
            };
            if my_ext == Some(&other_ext) {
                continue;
            }
            let other_id = format!("ext:{other_ext}");
            let key = ordered(my_id.clone(), other_id);
            let mine_is_a = key.0 == my_id;
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
        // ext が確定しなければ常に node:<id>（rloc16 が導出できていても、
        // 頂点キーとしては再アタッチで変わる rloc より node_id が安定）。
        let id = fabric_vertex_id(inp.node_id, &self_ext);
        nodes.push(MeshNode {
            id,
            ext_address: ext.clone(),
            rloc16: rloc16.map(rloc16_str),
            router_id: rloc16.and_then(router_id_of),
            role: role.to_string(),
            node_id: Some(inp.node_id),
            alias: inp.alias.clone(),
            label: ext.as_ref().and_then(|e| thread_labels.get(e)).cloned(),
            identified_by: ident_by.get(&inp.node_id).map(|s| s.to_string()),
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
            identified_by: None,
            probed: None,
            probe_error: None,
        });
    }

    // 6. エッジ出力（キー昇順、route は a 視点優先）。キーは既に完全な id
    // 文字列（`ext:…` / `node:…`）なので再プレフィックスしない。
    let edges = edges
        .into_iter()
        .map(|((a, b), acc)| MeshEdge {
            a,
            b,
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
    fn derive_rloc16_rejects_18_hex_not_length_prefixed() {
        // 18 桁 hex でも先頭が "40"（長さ前置バイト）でなければ不正形として None。
        let addrs = vec!["fd00112233445566000000fffe002c00".to_string()];
        assert_eq!(derive_rloc16("fd0011223344556600", &addrs), None);
    }

    #[test]
    fn derive_rloc16_tolerates_length_prefixed_encoding() {
        // 実機観測: 一部デバイスが mesh-local-prefix を「長さ前置 octstr」
        // （0x40 = 64bit 長 + 8B prefix = 18 桁 hex、NL68 系）で返す。
        let addrs = vec!["fd00112233445566000000fffe002c00".to_string()];
        assert_eq!(derive_rloc16("40fd00112233445566", &addrs), Some(0x2c00));
    }

    #[test]
    fn canon_ml_prefix_16_hex_passthrough_lowercased() {
        assert_eq!(
            canon_ml_prefix("FD00112233445566").as_deref(),
            Some("fd00112233445566")
        );
        assert_eq!(
            canon_ml_prefix("fd00112233445566").as_deref(),
            Some("fd00112233445566")
        );
    }

    #[test]
    fn canon_ml_prefix_18_hex_length_prefixed_strips_to_trailing_16() {
        assert_eq!(
            canon_ml_prefix("40fd00112233445566").as_deref(),
            Some("fd00112233445566")
        );
        // 大文字混在でも同様。
        assert_eq!(
            canon_ml_prefix("40FD00112233445566").as_deref(),
            Some("fd00112233445566")
        );
    }

    #[test]
    fn canon_ml_prefix_18_hex_not_starting_40_is_none() {
        assert_eq!(canon_ml_prefix("fd0011223344556600"), None);
    }

    #[test]
    fn canon_ml_prefix_non_hex_is_none() {
        assert_eq!(canon_ml_prefix("zzfd00112233445566"), None);
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

    /// 実機 NL68 系: 工場出荷 FW バグで全台が同一の HardwareAddress（ext）を
    /// cluster 0x33 で申告する（IPv6Addresses は空）。両者の self-ext は
    /// 物理的にあり得ない衝突として無効化され、各々 `node:<id>` として自視点
    /// エッジを持つ。
    fn dup_claim_input(node_id: u64, dup_ext: &str, real_neighbor_ext: u64) -> NodeInput {
        let mut thread = Map::new();
        thread.insert("network_name".into(), json!("TestNet"));
        thread.insert("channel".into(), json!(25));
        thread.insert("routing_role".into(), json!(5));
        thread.insert(
            "neighbor_table".into(),
            json!([
                {"ExtAddress": real_neighbor_ext, "Rloc16": 0x2000, "Lqi": 150,
                 "AverageRssi": -55, "LastRssi": -54, "FrameErrorRate": 1, "Age": 5,
                 "RxOnWhenIdle": true, "IsChild": false}
            ]),
        );
        thread.insert("route_table".into(), json!([]));
        NodeInput {
            node_id,
            alias: None,
            probe: Ok(ProbeData {
                thread,
                identity: Some(Identity {
                    ext_address: dup_ext.to_string(),
                    ipv6: vec![],
                }),
            }),
        }
    }

    #[test]
    fn duplicate_self_ext_claims_are_invalidated() {
        let n42 = dup_claim_input(42, "4CFA012000FC0115", 0xAAAAAAAAAAAAAAAAu64);
        let n7 = dup_claim_input(7, "4cfa012000fc0115", 0xBBBBBBBBBBBBBBBBu64);
        let g = build_graph(&[n42, n7], &BTreeMap::new());

        // 重複申告された ext:4CFA012000FC0115 の頂点は存在しない。
        assert!(!g.nodes.iter().any(|n| n.id == "ext:4CFA012000FC0115"));

        let node42 = g.nodes.iter().find(|n| n.node_id == Some(42)).unwrap();
        assert_eq!(node42.id, "node:42");
        assert!(node42.ext_address.is_none());
        let node7 = g.nodes.iter().find(|n| n.node_id == Some(7)).unwrap();
        assert_eq!(node7.id, "node:7");
        assert!(node7.ext_address.is_none());

        // それぞれ自視点で node:<id> → 実在ルータへのエッジが張られる。
        let e42 = g
            .edges
            .iter()
            .find(|e| e.a == "ext:AAAAAAAAAAAAAAAA" && e.b == "node:42")
            .unwrap();
        assert_eq!(e42.b_sees_a.as_ref().unwrap().lqi, Some(150));
        assert!(e42.a_sees_b.is_none());
        let e7 = g
            .edges
            .iter()
            .find(|e| e.a == "ext:BBBBBBBBBBBBBBBB" && e.b == "node:7")
            .unwrap();
        assert_eq!(e7.b_sees_a.as_ref().unwrap().lqi, Some(150));
        assert!(e7.a_sees_b.is_none());
    }

    /// 自己同定なし（identity None）の router 入力を組む fixture。
    /// route_table / neighbor_table は呼び出し側が丸ごと渡す。
    fn router_input_no_identity(
        node_id: u64,
        routing_role: i64,
        neighbor_table: Value,
        route_table: Value,
    ) -> NodeInput {
        let mut thread = Map::new();
        thread.insert("network_name".into(), json!("TestNet"));
        thread.insert("channel".into(), json!(25));
        thread.insert("routing_role".into(), json!(routing_role));
        thread.insert("neighbor_table".into(), neighbor_table);
        thread.insert("route_table".into(), route_table);
        NodeInput {
            node_id,
            alias: None,
            probe: Ok(ProbeData {
                thread,
                identity: None,
            }),
        }
    }

    /// route-table 自己行（実機観測: NextHop=63(invalid) + PathCost=0。
    /// ExtAddress はベンダー依存で 0 か実 ext）。
    fn self_row(ext: u64, rloc16: u16) -> Value {
        json!({"ExtAddress": ext, "Rloc16": rloc16, "RouterId": rloc16 >> 10,
               "NextHop": 63, "PathCost": 0, "LQIIn": 0, "LQIOut": 0,
               "Allocated": true, "LinkEstablished": false, "Age": 0})
    }

    /// 実機シナリオ（issue #13）: cluster 0x33 が読めない router が、自分の
    /// route-table 自己行（ExtAddress=0）から自 RLOC16 を取り、他ノードの
    /// 観測行（実 ext + Rloc16）との一意一致で ext を確定 → 頂点がマージされる。
    #[test]
    fn route_table_self_row_rloc_correlates_to_observed_ext() {
        // node1: identity なし。自己行 rloc 0x1400 + node2(0x2000) への通常行。
        let n1 = router_input_no_identity(
            1,
            5,
            json!([
                {"ExtAddress": 0x8899AABBCCDDEEFFu64, "Rloc16": 0x2000, "Lqi": 150,
                 "AverageRssi": -55, "LastRssi": -54, "FrameErrorRate": 1, "Age": 5,
                 "RxOnWhenIdle": true, "IsChild": false}
            ]),
            json!([self_row(0, 0x1400)]),
        );
        // node2: 自己同定済み。node1 の実 ext を rloc 0x1400 として観測。
        let n2 = fabric_input(
            2,
            None,
            "8899AABBCCDDEEFF",
            "fd00112233445566000000fffe002000",
            vec![(
                "neighbor_table",
                json!([
                    {"ExtAddress": 0x1122334455667788u64, "Rloc16": 0x1400, "Lqi": 140,
                     "AverageRssi": -60, "LastRssi": -58, "FrameErrorRate": 2, "Age": 3,
                     "RxOnWhenIdle": true, "IsChild": false}
                ]),
            )],
        );
        let g = build_graph(&[n1, n2], &BTreeMap::new());

        // node1 は ext:1122... へマージされ、node:1 も孤児 ext 頂点も存在しない。
        assert_eq!(g.nodes.len(), 2);
        let n1o = g.nodes.iter().find(|n| n.node_id == Some(1)).unwrap();
        assert_eq!(n1o.id, "ext:1122334455667788");
        assert_eq!(n1o.ext_address.as_deref(), Some("1122334455667788"));
        assert_eq!(n1o.rloc16.as_deref(), Some("0x1400"));
        assert_eq!(n1o.router_id, Some(5));
        assert_eq!(n1o.identified_by.as_deref(), Some("rloc16"));
        // 0x33 で自己同定したノードには identified_by は付かない。
        let n2o = g.nodes.iter().find(|n| n.node_id == Some(2)).unwrap();
        assert_eq!(n2o.identified_by, None);

        // エッジは 1 本に統合され、双方向実測を持つ（node:1 アンカーは無い）。
        assert_eq!(g.edges.len(), 1);
        let e = &g.edges[0];
        assert_eq!(e.a, "ext:1122334455667788");
        assert_eq!(e.b, "ext:8899AABBCCDDEEFF");
        assert_eq!(e.a_sees_b.as_ref().unwrap().lqi, Some(150));
        assert_eq!(e.b_sees_a.as_ref().unwrap().lqi, Some(140));
    }

    /// 自己行の ExtAddress が実値を持つベンダー（実機観測: leader 個体）は
    /// 観測相関なしで直接自己同定できる。
    #[test]
    fn route_table_self_row_with_nonzero_ext_identifies_directly() {
        let n1 = router_input_no_identity(
            1,
            6,
            json!([]),
            json!([self_row(0x1122334455667788, 0x1400)]),
        );
        let g = build_graph(&[n1], &BTreeMap::new());
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "ext:1122334455667788");
        assert_eq!(g.nodes[0].rloc16.as_deref(), Some("0x1400"));
        assert_eq!(g.nodes[0].identified_by.as_deref(), Some("route-table"));
    }

    /// issue #13 の本丸: FW バグの同一 MAC 申告で自己同定が全滅した router
    /// 同士でも、各自の自己行 + 相互観測で両者とも実 ext へマージされる。
    #[test]
    fn dup_mac_pair_with_self_rows_merges_into_observed_exts() {
        let mk = |node_id: u64, my_rloc: u16, other_ext: u64, other_rloc: u16| {
            let mut thread = Map::new();
            thread.insert("network_name".into(), json!("TestNet"));
            thread.insert("channel".into(), json!(25));
            thread.insert("routing_role".into(), json!(5));
            thread.insert(
                "neighbor_table".into(),
                json!([
                    {"ExtAddress": other_ext, "Rloc16": other_rloc, "Lqi": 150,
                     "AverageRssi": -55, "LastRssi": -54, "FrameErrorRate": 1, "Age": 5,
                     "RxOnWhenIdle": true, "IsChild": false}
                ]),
            );
            thread.insert("route_table".into(), json!([self_row(0, my_rloc)]));
            NodeInput {
                node_id,
                alias: None,
                probe: Ok(ProbeData {
                    thread,
                    identity: Some(Identity {
                        ext_address: "4CFA012000FC0115".into(), // 全台同一の工場 MAC
                        ipv6: vec![],
                    }),
                }),
            }
        };
        // node42 実体 = ext AAAA…(0x1400)、node7 実体 = ext BBBB…(0x2000)。
        let n42 = mk(42, 0x1400, 0xBBBBBBBBBBBBBBBBu64, 0x2000);
        let n7 = mk(7, 0x2000, 0xAAAAAAAAAAAAAAAAu64, 0x1400);
        let g = build_graph(&[n42, n7], &BTreeMap::new());

        assert_eq!(g.nodes.len(), 2);
        let n42o = g.nodes.iter().find(|n| n.node_id == Some(42)).unwrap();
        assert_eq!(n42o.id, "ext:AAAAAAAAAAAAAAAA");
        assert_eq!(n42o.identified_by.as_deref(), Some("rloc16"));
        let n7o = g.nodes.iter().find(|n| n.node_id == Some(7)).unwrap();
        assert_eq!(n7o.id, "ext:BBBBBBBBBBBBBBBB");
        // 偽 MAC の頂点は生えない。相互観測は 1 本の両方向エッジに畳まれる。
        assert!(!g.nodes.iter().any(|n| n.id == "ext:4CFA012000FC0115"));
        assert_eq!(g.edges.len(), 1);
        let e = &g.edges[0];
        assert_eq!(e.a, "ext:AAAAAAAAAAAAAAAA");
        assert_eq!(e.b, "ext:BBBBBBBBBBBBBBBB");
        assert!(e.a_sees_b.is_some() && e.b_sees_a.is_some());
    }

    /// 同一 rloc16 に複数の実 ext が観測されている（stale テーブル）なら
    /// 推測せずマージしない。
    #[test]
    fn ambiguous_rloc_observations_block_merge() {
        let n1 = router_input_no_identity(1, 5, json!([]), json!([self_row(0, 0x1400)]));
        let n2 = fabric_input(
            2,
            None,
            "8899AABBCCDDEEFF",
            "fd00112233445566000000fffe002000",
            vec![(
                "neighbor_table",
                json!([
                    {"ExtAddress": 0x1122334455667788u64, "Rloc16": 0x1400, "Lqi": 140,
                     "AverageRssi": -60, "LastRssi": -58, "FrameErrorRate": 2, "Age": 3,
                     "RxOnWhenIdle": true, "IsChild": false},
                    {"ExtAddress": 0x99AA99AA99AA99AAu64, "Rloc16": 0x1400, "Lqi": 100,
                     "AverageRssi": -70, "LastRssi": -70, "FrameErrorRate": 9, "Age": 200,
                     "RxOnWhenIdle": true, "IsChild": false}
                ]),
            )],
        );
        let g = build_graph(&[n1, n2], &BTreeMap::new());
        let n1o = g.nodes.iter().find(|n| n.node_id == Some(1)).unwrap();
        assert_eq!(n1o.id, "node:1");
        assert_eq!(n1o.identified_by, None);
        // 救済不成立の node: 頂点は rloc16 を出さない（孤児 ext 側と同じ rloc を
        // 二重表示して下流の rloc16 結合を壊さないため）。
        assert_eq!(n1o.rloc16, None);
    }

    /// 自己行 rloc が既に自己同定済みノードの ext に解決される場合
    /// （stale / 矛盾）はマージしない。
    #[test]
    fn self_row_rloc_resolving_to_identified_node_is_rejected() {
        // node1 の自己行が 0x2000 を主張するが、0x2000 の実 ext は node2 自身。
        let n1 = router_input_no_identity(
            1,
            5,
            json!([
                {"ExtAddress": 0x8899AABBCCDDEEFFu64, "Rloc16": 0x2000, "Lqi": 150,
                 "AverageRssi": -55, "LastRssi": -54, "FrameErrorRate": 1, "Age": 5,
                 "RxOnWhenIdle": true, "IsChild": false}
            ]),
            json!([self_row(0, 0x2000)]),
        );
        let n2 = fabric_input(
            2,
            None,
            "8899AABBCCDDEEFF",
            "fd00112233445566000000fffe002000",
            vec![],
        );
        let g = build_graph(&[n1, n2], &BTreeMap::new());
        let n1o = g.nodes.iter().find(|n| n.node_id == Some(1)).unwrap();
        assert_eq!(n1o.id, "node:1");
        assert_eq!(n1o.identified_by, None);
    }

    /// 自己行候補（NextHop=63 + PathCost=0）が複数あれば形が想定外として
    /// 自己同定に使わない。
    #[test]
    fn multiple_self_row_candidates_block_identification() {
        let n1 = router_input_no_identity(
            1,
            5,
            json!([]),
            json!([self_row(0, 0x1400), self_row(0, 0x2000)]),
        );
        let g = build_graph(&[n1], &BTreeMap::new());
        assert_eq!(g.nodes[0].id, "node:1");
        assert_eq!(g.nodes[0].identified_by, None);
    }

    /// 自己行は router のテーブルにしか無い（実機観測: reed には無く、
    /// ExtAddress=0 の他ルーター行はある）。router / leader 以外の
    /// routing_role では自己行探索をしない。
    #[test]
    fn non_router_roles_do_not_use_route_table_self_rows() {
        let n1 = router_input_no_identity(
            1,
            4, // reed
            json!([]),
            json!([self_row(0x1122334455667788, 0x1400)]),
        );
        let g = build_graph(&[n1], &BTreeMap::new());
        // 自己行（NextHop=63 + PathCost=0）は参加者台帳にも入れない: 自分の
        // 自己行から自分の幻影 ext: 頂点が生えたら本末転倒（issue #13 の再発）。
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "node:1");
        assert_eq!(g.nodes[0].identified_by, None);
    }

    /// 証拠クラスの優先順位: 自己行の実 ExtAddress（最強）で確定した救済は、
    /// 他ノードの stale rloc 相関が同じ ext へ衝突しても巻き添えで落ちない
    /// （相関側だけが棄却される）。
    #[test]
    fn direct_self_row_rescue_survives_stale_rloc_collision() {
        // node1: 自己行に実 ext AAAA…。
        let n1 = router_input_no_identity(
            1,
            5,
            json!([]),
            json!([self_row(0xAAAAAAAAAAAAAAAAu64, 0x1400)]),
        );
        // node2: 自己行は ext=0 で rloc 0x2000 を主張（stale）。
        let n2 = router_input_no_identity(2, 5, json!([]), json!([self_row(0, 0x2000)]));
        // node3: 自己同定済み。stale な観測で AAAA… を rloc 0x2000 として記録。
        let n3 = fabric_input(
            3,
            None,
            "8899AABBCCDDEEFF",
            "fd00112233445566000000fffe003000",
            vec![(
                "neighbor_table",
                json!([
                    {"ExtAddress": 0xAAAAAAAAAAAAAAAAu64, "Rloc16": 0x2000, "Lqi": 100,
                     "AverageRssi": -70, "LastRssi": -70, "FrameErrorRate": 9, "Age": 250,
                     "RxOnWhenIdle": true, "IsChild": false}
                ]),
            )],
        );
        let g = build_graph(&[n1, n2, n3], &BTreeMap::new());
        let n1o = g.nodes.iter().find(|n| n.node_id == Some(1)).unwrap();
        assert_eq!(n1o.id, "ext:AAAAAAAAAAAAAAAA");
        assert_eq!(n1o.identified_by.as_deref(), Some("route-table"));
        let n2o = g.nodes.iter().find(|n| n.node_id == Some(2)).unwrap();
        assert_eq!(n2o.id, "node:2");
        assert_eq!(n2o.identified_by, None);
    }

    /// partition が複数観測されている（メッシュ分断中）間は rloc16 相関を
    /// 丸ごとスキップする: RLOC16 の一意性はパーティション内でしか成立しない。
    /// 自己行の実 ExtAddress による直接同定はパーティションと無関係なので生きる。
    #[test]
    fn rloc_correlation_skipped_when_mesh_partitioned() {
        let mut thread = Map::new();
        thread.insert("network_name".into(), json!("TestNet"));
        thread.insert("channel".into(), json!(25));
        thread.insert("partition_id".into(), json!(999_999));
        thread.insert("routing_role".into(), json!(5));
        thread.insert("neighbor_table".into(), json!([]));
        thread.insert("route_table".into(), json!([self_row(0, 0x1400)]));
        let n1 = NodeInput {
            node_id: 1,
            alias: None,
            probe: Ok(ProbeData {
                thread,
                identity: None,
            }),
        };
        // 別パーティションの node2 が rloc 0x1400 の ext を観測している。
        let n2 = fabric_input(
            2,
            None,
            "8899AABBCCDDEEFF",
            "fd00112233445566000000fffe002000",
            vec![(
                "neighbor_table",
                json!([
                    {"ExtAddress": 0x1122334455667788u64, "Rloc16": 0x1400, "Lqi": 140,
                     "AverageRssi": -60, "LastRssi": -58, "FrameErrorRate": 2, "Age": 3,
                     "RxOnWhenIdle": true, "IsChild": false}
                ]),
            )],
        );
        let g = build_graph(&[n1, n2], &BTreeMap::new());
        assert_eq!(g.network.partition_ids.len(), 2);
        let n1o = g.nodes.iter().find(|n| n.node_id == Some(1)).unwrap();
        assert_eq!(n1o.id, "node:1");
        assert_eq!(n1o.identified_by, None);
    }

    /// OpenThread は経路喪失ルーター行も NextHop=63 + PathCost=0 にする
    /// （SetNextHopToInvalid）。自己行は常に「今の自分」なので Age はほぼ 0 —
    /// Age が大きい 63/0 行は自己行とみなさず、参加者台帳にも入れない。
    #[test]
    fn stale_unreachable_row_is_not_mistaken_for_self_row() {
        let row = json!({"ExtAddress": 0xCCCCCCCCCCCCCCCCu64, "Rloc16": 0x3000,
                         "RouterId": 12, "NextHop": 63, "PathCost": 0,
                         "LQIIn": 0, "LQIOut": 0, "Allocated": true,
                         "LinkEstablished": false, "Age": 200});
        let n1 = router_input_no_identity(1, 5, json!([]), json!([row]));
        let g = build_graph(&[n1], &BTreeMap::new());
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "node:1");
        assert_eq!(g.nodes[0].identified_by, None);
    }

    /// issue #13 ヒント 2 の是正: RLOC16 の IPv6 由来導出は ExtAddress
    /// 正準化の成否と独立に行い、ext が偽（非 hex）でも rloc 相関で
    /// マージできる。
    #[test]
    fn ipv6_rloc_derivation_survives_invalid_ext_claim() {
        let mut thread = Map::new();
        thread.insert("network_name".into(), json!("TestNet"));
        thread.insert("channel".into(), json!(25));
        thread.insert("routing_role".into(), json!(3)); // child: 自己行なしでも通る
        thread.insert("mesh_local_prefix".into(), json!("fd00112233445566"));
        thread.insert("neighbor_table".into(), json!([]));
        thread.insert("route_table".into(), json!([]));
        let n1 = NodeInput {
            node_id: 1,
            alias: None,
            probe: Ok(ProbeData {
                thread,
                identity: Some(Identity {
                    ext_address: "not-hex!".into(),
                    ipv6: vec!["fd00112233445566000000fffe001401".to_string()],
                }),
            }),
        };
        let n2 = fabric_input(
            2,
            None,
            "8899AABBCCDDEEFF",
            "fd00112233445566000000fffe002000",
            vec![(
                "neighbor_table",
                json!([
                    {"ExtAddress": 0x1122334455667788u64, "Rloc16": 0x1401, "Lqi": 120,
                     "AverageRssi": -62, "LastRssi": -61, "FrameErrorRate": 3, "Age": 2,
                     "RxOnWhenIdle": false, "IsChild": true}
                ]),
            )],
        );
        let g = build_graph(&[n1, n2], &BTreeMap::new());
        let n1o = g.nodes.iter().find(|n| n.node_id == Some(1)).unwrap();
        assert_eq!(n1o.id, "ext:1122334455667788");
        assert_eq!(n1o.rloc16.as_deref(), Some("0x1401"));
        assert_eq!(n1o.identified_by.as_deref(), Some("rloc16"));
    }

    #[test]
    fn zero_ext_address_rows_are_ignored() {
        let n1 = fabric_input(
            1,
            None,
            "0011223344556677",
            "fd00112233445566000000fffe001400",
            vec![(
                "route_table",
                json!([
                    {"ExtAddress": 0u64, "Rloc16": 0x2000, "RouterId": 8,
                     "PathCost": 1, "LQIIn": 3, "LQIOut": 3, "Allocated": true,
                     "LinkEstablished": true}
                ]),
            )],
        );
        let g = build_graph(&[n1], &BTreeMap::new());
        // ExtAddress=0 のゴミ行はノードにもエッジにもならない。
        assert!(g.edges.is_empty());
        assert_eq!(g.nodes.len(), 1);
        assert!(!g.nodes.iter().any(|n| n.id == "ext:0000000000000000"));
    }

    #[test]
    fn identityless_probe_success_anchors_edges_at_node_id() {
        // 2026-07-23 実機 E2E 対応の仕様変更: cluster 0x33 が読めず自己同定
        // できなくても、cluster 53 のテーブル行自体は有効な観測なので
        // `node:<id>` を自視点としてエッジを張る（以前は他ノード視点のみ）。
        let mut thread = Map::new();
        thread.insert("network_name".into(), json!("TestNet"));
        thread.insert("channel".into(), json!(25));
        thread.insert("routing_role".into(), json!(5));
        thread.insert(
            "neighbor_table".into(),
            json!([
                {"ExtAddress": 0xAABBCCDDEEFF0011u64, "Rloc16": 0x2000, "Lqi": 180,
                 "AverageRssi": -52, "LastRssi": -50, "FrameErrorRate": 0, "Age": 4,
                 "RxOnWhenIdle": true, "IsChild": false}
            ]),
        );
        thread.insert("route_table".into(), json!([]));
        let n = NodeInput {
            node_id: 9,
            alias: None,
            probe: Ok(ProbeData {
                thread,
                identity: None,
            }),
        };
        let g = build_graph(&[n], &BTreeMap::new());
        let fabric_node = g.nodes.iter().find(|n| n.node_id == Some(9)).unwrap();
        assert_eq!(fabric_node.id, "node:9");
        assert_eq!(g.edges.len(), 1);
        let e = &g.edges[0];
        assert_eq!(e.a, "ext:AABBCCDDEEFF0011");
        assert_eq!(e.b, "node:9");
        assert_eq!(e.b_sees_a.as_ref().unwrap().lqi, Some(180));
        assert!(e.a_sees_b.is_none());
    }
}
