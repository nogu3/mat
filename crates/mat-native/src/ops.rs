//! mat / matd 共有の native op ロジック（describe / diag thread）。
//!
//! `NodeConn` の上に立つ純粋ロジック層 —— バックエンド（実 CASE / test fake）
//! を問わない。値の符号化を伴わない読み取り専用 op なので `classify`（M8a
//! Task7 の `classify_strict` と違い）は常に `Some/None` — 値エラーはない。

use std::collections::HashMap;

use serde_json::{Map, Value};

use mat_core::error::{ErrorKind, MatError};
use mat_core::ids::resolve_attribute;

use crate::NodeConn;

const CLUSTER_DESCRIPTOR: u32 = 0x001D;
const ATTR_PARTS_LIST: u32 = 0x0003;
const ATTR_SERVER_LIST: u32 = 0x0001;
const CLUSTER_THREAD_DIAG: u32 = 0x0035;

/// descriptor 歩き: ep0 の parts-list → 各 ep の server-list。
/// 返り値: (endpoint, cluster id 群) の列（ep0 先頭、重複なし）。
pub async fn describe(conn: &mut dyn NodeConn) -> Result<Vec<(u16, Vec<u64>)>, MatError> {
    let parts = conn
        .read_json(0, CLUSTER_DESCRIPTOR, ATTR_PARTS_LIST)
        .await?;
    let mut endpoints: Vec<u16> = vec![0];
    for p in parse_id_list_json(&parts) {
        if let Ok(ep) = u16::try_from(p) {
            if !endpoints.contains(&ep) {
                endpoints.push(ep);
            }
        }
    }

    let mut out = Vec::with_capacity(endpoints.len());
    for ep in endpoints {
        let servers = conn
            .read_json(ep, CLUSTER_DESCRIPTOR, ATTR_SERVER_LIST)
            .await?;
        out.push((ep, parse_id_list_json(&servers)));
    }
    Ok(out)
}

/// JSON 配列から数値要素のみを u64 化して集める（chip-tool 経路の
/// `parse_id_list` と同じ寛容さ: 配列でない/数値でない要素はスキップ）。
fn parse_id_list_json(v: &Value) -> Vec<u64> {
    match v.as_array() {
        Some(items) => items.iter().filter_map(Value::as_u64).collect(),
        None => Vec::new(),
    }
}

/// Thread Network Diagnostics スナップショット。`fields` は出力キー→値
/// （読めなかったものは `null`）、`unavailable` は (chip-tool 属性名, kind) —
/// wildcard read は per-attribute の失敗を出さない（デバイスが持っている
/// 属性だけ返す）ため、native 経路では通常空（スキーマ上「あれば出す」）。
#[derive(Debug)]
pub struct ThreadSnapshot {
    pub fields: Map<String, Value>,
    pub unavailable: Vec<(String, ErrorKind)>,
}

/// スカラー属性: (出力キー, chip-tool 属性名)。属性 ID はハードコードせず
/// `mat_core::ids::resolve_attribute` で引く。
const SCALARS: &[(&str, &str)] = &[
    ("routing_role", "routing-role"),
    ("network_name", "network-name"),
    ("extended_pan_id", "extended-pan-id"),
    ("pan_id", "pan-id"),
    ("partition_id", "partition-id"),
    ("channel", "channel"),
];

/// list-of-struct 属性: (出力キー, chip-tool 属性名)。
const TABLES: &[(&str, &str)] = &[
    ("neighbor_table", "neighbor-table"),
    ("route_table", "route-table"),
];

/// NeighborTableStruct（cluster 53）の field id → chip-tool 表記名。
/// 表記は `crates/mat-core/src/parse.rs` の `struct_list_parses_neighbor_table`
/// テスト（＝ fake-chip-tool フィクスチャ `neighbor-table` と同値）から確定。
/// field id は Matter spec cluster 53 NeighborTableStruct の定義順。
const NEIGHBOR_TABLE_FIELDS: &[(u8, &str)] = &[
    (0, "ExtAddress"),
    (1, "Age"),
    (2, "Rloc16"),
    (3, "LinkFrameCounter"),
    (4, "MleFrameCounter"),
    (5, "Lqi"),
    (6, "AverageRssi"),
    (7, "LastRssi"),
    (8, "FrameErrorRate"),
    (9, "MessageErrorRate"),
    (10, "RxOnWhenIdle"),
    (11, "FullThreadDevice"),
    (12, "FullNetworkData"),
    (13, "IsChild"),
];

/// RouteTableStruct（cluster 53）の field id → chip-tool 表記名。
/// 表記は `crates/mat-core/src/parse.rs` の `struct_list_realworld_log_format`
/// テスト（＝ fake-chip-tool フィクスチャ `route-table` と同値）から確定。
/// **注意**: LQI 表記は NeighborTable の "Lqi" と揃わず "LQIIn"/"LQIOut"
/// （chip-tool の実際の表記ゆれ、フィクスチャが正）。
const ROUTE_TABLE_FIELDS: &[(u8, &str)] = &[
    (0, "ExtAddress"),
    (1, "Rloc16"),
    (2, "RouterId"),
    (3, "NextHop"),
    (4, "PathCost"),
    (5, "LQIIn"),
    (6, "LQIOut"),
    (7, "Age"),
    (8, "Allocated"),
    (9, "LinkEstablished"),
];

/// Thread 診断スナップショット（cluster 0x0035 の wildcard read 1発 + 整形）。
/// 部分結果ポリシーは chip-tool 経路と同じ: 読めた属性のみ、失敗は unavailable。
/// `read_cluster` 自体が失敗（不達等）なら Err をそのまま伝播する。
pub async fn diag_thread(
    conn: &mut dyn NodeConn,
    endpoint: u16,
) -> Result<ThreadSnapshot, MatError> {
    let rows = conn.read_cluster(endpoint, CLUSTER_THREAD_DIAG).await?;
    let by_attr: HashMap<u32, Value> = rows.into_iter().collect();

    let mut fields = Map::new();
    for (out_key, attr_name) in SCALARS {
        let attr_id = resolve_attribute(CLUSTER_THREAD_DIAG, attr_name).map(|a| a.id);
        let v = attr_id
            .and_then(|id| by_attr.get(&id))
            .cloned()
            .unwrap_or(Value::Null);
        fields.insert((*out_key).to_string(), v);
    }
    for (out_key, attr_name) in TABLES {
        let attr_id = resolve_attribute(CLUSTER_THREAD_DIAG, attr_name).map(|a| a.id);
        let rename_table = table_fields_for(attr_name);
        let v = attr_id
            .and_then(|id| by_attr.get(&id))
            .cloned()
            .map(|v| rename_struct_array(v, rename_table))
            .unwrap_or(Value::Null);
        fields.insert((*out_key).to_string(), v);
    }

    // wildcard read は per-attribute の失敗を出さない: read_cluster 自体が
    // 成功した以上、native 経路の unavailable は常に空（呼び出し側でも
    // `!unavailable.is_empty()` ガード済みなのでスキーマは互換）。
    Ok(ThreadSnapshot {
        fields,
        unavailable: Vec::new(),
    })
}

fn table_fields_for(attr_name: &str) -> &'static [(u8, &'static str)] {
    if attr_name == "neighbor-table" {
        NEIGHBOR_TABLE_FIELDS
    } else {
        ROUTE_TABLE_FIELDS
    }
}

/// list-of-struct の各要素で、field id（context tag の10進文字列キー）を
/// chip-tool 表記名へ改名する。table に無い field id は元のキーのまま残す
/// （前方互換 — 未知フィールドを黙って落とさない）。
fn rename_struct_array(v: Value, table: &[(u8, &str)]) -> Value {
    match v {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| rename_struct_fields(item, table))
                .collect(),
        ),
        other => other,
    }
}

fn rename_struct_fields(v: Value, table: &[(u8, &str)]) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, val) in map {
                let renamed = k
                    .parse::<u8>()
                    .ok()
                    .and_then(|id| table.iter().find(|(fid, _)| *fid == id))
                    .map(|(_, name)| (*name).to_string())
                    .unwrap_or(k);
                out.insert(renamed, val);
            }
            Value::Object(out)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeConn;

    #[tokio::test]
    async fn describe_walks_parts_and_server_lists() {
        // ep0 parts-list = [1], ep0 server-list = [29, 31], ep1 server-list = [6, 8]
        let mut conn = FakeConn::scripted()
            .with_read(0, 0x001D, 0x0003, serde_json::json!([1]))
            .with_read(0, 0x001D, 0x0001, serde_json::json!([29, 31]))
            .with_read(1, 0x001D, 0x0001, serde_json::json!([6, 8]));
        let eps = describe(&mut conn).await.unwrap();
        assert_eq!(eps, vec![(0, vec![29, 31]), (1, vec![6, 8])]);
    }

    #[tokio::test]
    async fn diag_thread_maps_names_and_partial_results() {
        // wildcard read が routing-role(1=数値), neighbor-table(structの配列) を返し、
        // network-name 等は欠けている → fields は読めた分 + null、unavailable は無し
        // （wildcard は「無い属性」を返さないだけで per-attr エラーが出ない点が
        //  chip-tool 経路と違う。全滅時のみ Err — テスト2本目で確認）。
        let mut conn = FakeConn::scripted().with_cluster(
            1,
            0x0035,
            vec![
                (0x0001, serde_json::json!(3)),                     // routing-role
                (0x0007, serde_json::json!([{"0": 42, "7": -60}])), // neighbor-table
            ],
        );
        let snap = diag_thread(&mut conn, 1).await.unwrap();
        assert_eq!(snap.fields["routing_role"], serde_json::json!(3));
        // struct キーがフィールド名へ改名されていること（chip-tool ログ互換名）。
        let nt = snap.fields["neighbor_table"].as_array().unwrap();
        assert!(
            nt[0].get("ExtAddress").is_some() || nt[0].get("Age").is_some(),
            "field-id keys must be renamed: {nt:?}"
        );
        // 返らなかった属性は null。
        assert_eq!(snap.fields["network_name"], serde_json::Value::Null);
        assert!(snap.unavailable.is_empty());
    }

    #[tokio::test]
    async fn diag_thread_propagates_err_when_read_cluster_fails() {
        // read_cluster 自体が失敗（不達等）した場合は Err をそのまま伝播する
        // （chip-tool 経路の「全滅時は最初の失敗 kind を伝播」と同義）。
        struct FailingConn;
        #[async_trait::async_trait]
        impl NodeConn for FailingConn {
            async fn read_onoff(&mut self, _endpoint: u16) -> Result<bool, MatError> {
                unimplemented!()
            }
            async fn invoke(
                &mut self,
                _endpoint: u16,
                _cluster: u32,
                _command: u32,
                _fields: Option<Vec<u8>>,
                _timed: bool,
            ) -> Result<(), MatError> {
                unimplemented!()
            }
            async fn read_json(
                &mut self,
                _endpoint: u16,
                _cluster: u32,
                _attribute: u32,
            ) -> Result<Value, MatError> {
                unimplemented!()
            }
            async fn read_cluster(
                &mut self,
                _endpoint: u16,
                _cluster: u32,
            ) -> Result<Vec<(u32, Value)>, MatError> {
                Err(MatError::new(ErrorKind::Unreachable, "fake unreachable"))
            }
            async fn write_tlv(
                &mut self,
                _endpoint: u16,
                _cluster: u32,
                _attribute: u32,
                _data_tlv: Vec<u8>,
                _timed: bool,
            ) -> Result<(), MatError> {
                unimplemented!()
            }
            async fn open_window(
                &mut self,
                _timeout_s: u16,
                _discriminator: u16,
                _iterations: u32,
            ) -> Result<(String, String), MatError> {
                unimplemented!()
            }
        }
        let mut conn = FailingConn;
        let err = diag_thread(&mut conn, 1).await.expect_err("must propagate");
        assert_eq!(err.kind, ErrorKind::Unreachable);
    }
}
