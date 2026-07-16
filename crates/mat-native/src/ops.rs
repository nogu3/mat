//! mat / matd 共有の native op ロジック（describe / diag thread）。
//!
//! `NodeConn` の上に立つ純粋ロジック層 —— バックエンド（実 CASE / test fake）
//! を問わない。値の符号化を伴わない読み取り専用 op なので `classify`（M8a
//! Task7 の `classify_strict` と違い）は常に `Some/None` — 値エラーはない。

use std::collections::HashMap;

use serde_json::{Map, Value};

use mat_controller::im::{
    encode_add_group_fields, encode_group_key_map_tlv, encode_key_set_write_fields, ATTR_ACL,
    ATTR_GROUP_KEY_MAP, CLUSTER_ACCESS_CONTROL, CLUSTER_GROUPS, CLUSTER_GROUP_KEY_MANAGEMENT,
    CMD_ADD_GROUP, CMD_KEY_SET_WRITE,
};
use mat_controller::tlv::{Tag, Writer};
use mat_core::acl::{entries_from_im_json, merge_group_entry, AclEntry};
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

/// 1 ノード分のデバイス側 group provision に必要な材料一式。
pub struct ProvisionNodeParams {
    pub group_id: u16,
    pub keyset_id: u16,
    pub name: String,
    /// AddGroup を実行するエンドポイント（KeySetWrite / group-key-map / ACL は
    /// 常に ep0 — Matter spec 上これらは Node-wide なクラスタのため）。
    pub endpoint: u16,
    pub epoch_key: [u8; 16],
}

/// `mat_core::group::resolve_epoch_key` が返す 32 桁 hex 文字列（16 バイト）を
/// `[u8;16]` へ。呼び出し前提は「resolve_epoch_key が返した値そのもの」（検証
/// 済み・小文字 32 桁）だが、形式が崩れていた場合は呼び出し側のバグとして
/// `ParseError` を返す（panic させない）。
pub fn epoch_key_from_hex(hex: &str) -> Result<[u8; 16], MatError> {
    if hex.len() != 32 {
        return Err(MatError::parse_error(format!(
            "epoch key must be 32 hex chars (16 bytes), got {} chars",
            hex.len()
        )));
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| MatError::parse_error(format!("invalid epoch key hex: {hex}")))?;
    }
    Ok(out)
}

/// provision の 1 ステップに失敗した際、どのステップかを detail に残す
/// （chip-tool 経路の `run_node_step` と同粒度 — `commands/group.rs` 参照）。
fn provision_step_err(e: MatError, step: &str) -> MatError {
    MatError::new(
        e.kind,
        format!("provision step '{step}' failed: {}", e.detail),
    )
}

/// group-key-map 属性（list of `GroupKeyMapStruct`）の read JSON を
/// `(groupId, groupKeySetID)` 列へ。fabricIndex（254）等の他フィールドは
/// 無視する（groupId/groupKeySetID 以外はここで再現する必要が無い）。
fn parse_group_key_map(v: &Value) -> Result<Vec<(u16, u16)>, MatError> {
    let arr = v
        .as_array()
        .ok_or_else(|| MatError::parse_error(format!("group-key-map is not an array: {v}")))?;
    arr.iter()
        .map(|item| {
            let obj = item.as_object().ok_or_else(|| {
                MatError::parse_error(format!("group-key-map entry is not an object: {item}"))
            })?;
            let group_id = obj
                .get("1")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(|| {
                    MatError::parse_error(format!(
                        "group-key-map entry missing/invalid groupId: {item}"
                    ))
                })?;
            let keyset_id = obj
                .get("2")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(|| {
                    MatError::parse_error(format!(
                        "group-key-map entry missing/invalid groupKeySetID: {item}"
                    ))
                })?;
            Ok((group_id, keyset_id))
        })
        .collect()
}

/// `AclEntry` 列を `AccessControlEntryStruct` 列の Data TLV へ（write_tlv に
/// 渡す形）。ACL write は全置換のため、呼び出し側は read-merge 済みの最終形を
/// 渡すこと。
fn encode_acl_entries_tlv(entries: &[AclEntry]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for e in entries {
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(e.privilege));
        w.put_uint(Tag::Context(2), u64::from(e.auth_mode));
        w.start_array(Tag::Context(3));
        for s in &e.subjects {
            w.put_uint(Tag::Anonymous, *s);
        }
        w.end_container();
        match &e.targets {
            None => w.put_null(Tag::Context(4)),
            Some(targets) => {
                w.start_array(Tag::Context(4));
                for t in targets {
                    w.start_struct(Tag::Anonymous);
                    match t.cluster {
                        Some(c) => w.put_uint(Tag::Context(0), u64::from(c)),
                        None => w.put_null(Tag::Context(0)),
                    }
                    match t.endpoint {
                        Some(ep) => w.put_uint(Tag::Context(1), u64::from(ep)),
                        None => w.put_null(Tag::Context(1)),
                    }
                    match t.device_type {
                        Some(d) => w.put_uint(Tag::Context(2), u64::from(d)),
                        None => w.put_null(Tag::Context(2)),
                    }
                    w.end_container();
                }
                w.end_container();
            }
        }
        w.put_uint(Tag::Context(254), u64::from(e.fabric_index));
        w.end_container();
    }
    w.end_container();
    w.finish()
}

/// 1 ノード分のデバイス側 provision: KeySetWrite → group-key-map
/// read-merge-write → AddGroup → ACL read-merge-write。失敗はどのステップかを
/// detail に含めて即 Err（chip-tool 経路の `run_node_step` と同粒度）。
///
/// 宛先エンドポイント: KeySetWrite / group-key-map / ACL は ep0
/// （GroupKeyManagement・AccessControl は Node-wide、AddGroup のみ
/// `p.endpoint` — chip-tool 経路の argv と同じ、`commands/group.rs` 参照）。
pub async fn provision_node(
    conn: &mut dyn NodeConn,
    p: &ProvisionNodeParams,
) -> Result<(), MatError> {
    // KeySetWrite（timed 不要 — resolve_command(0x003F, "key-set-write") の
    // timed フラグは false）。
    let fields = encode_key_set_write_fields(p.keyset_id, &p.epoch_key);
    conn.invoke(
        0,
        CLUSTER_GROUP_KEY_MANAGEMENT,
        CMD_KEY_SET_WRITE,
        Some(fields),
        false,
    )
    .await
    .map_err(|e| provision_step_err(e, "key-set-write"))?;

    // group-key-map: 全置換 write なので read-merge-write（chip-tool 経路の
    // 単一要素 write は実は他 group のマッピングを消していた可能性がある —
    // native ではここで改善する）。
    let current = conn
        .read_json(0, CLUSTER_GROUP_KEY_MANAGEMENT, ATTR_GROUP_KEY_MAP)
        .await
        .map_err(|e| provision_step_err(e, "group-key-map read"))?;
    let mut entries =
        parse_group_key_map(&current).map_err(|e| provision_step_err(e, "group-key-map read"))?;
    match entries.iter_mut().find(|(g, _)| *g == p.group_id) {
        Some(slot) => slot.1 = p.keyset_id,
        None => entries.push((p.group_id, p.keyset_id)),
    }
    let tlv = encode_group_key_map_tlv(&entries);
    conn.write_tlv(
        0,
        CLUSTER_GROUP_KEY_MANAGEMENT,
        ATTR_GROUP_KEY_MAP,
        tlv,
        false,
    )
    .await
    .map_err(|e| provision_step_err(e, "group-key-map write"))?;

    // AddGroup（指定エンドポイント、timed 不要）。
    let fields = encode_add_group_fields(p.group_id, &p.name);
    conn.invoke(
        p.endpoint,
        CLUSTER_GROUPS,
        CMD_ADD_GROUP,
        Some(fields),
        false,
    )
    .await
    .map_err(|e| provision_step_err(e, "groups add-group"))?;

    // ACL: groupcast は authMode=Group で届くため、Group エントリが無いと
    // デバイスが黙って捨てる（commissioning が作るのは CASE 管理者エントリだけ）。
    ensure_group_acl(conn, p.group_id).await?;
    Ok(())
}

/// ACL の read-merge-write（provision の最終ステップ / `mat group grant` の
/// 本体）。戻り値: write した = true / 既に Group エントリがあり skip = false
/// （冪等）。
///
/// ACL の attribute write は全置換なので、write は必ず「read できたリスト +
/// 追記」のみ。read が失敗・解釈不能なら絶対に write しない（管理者エントリを
/// 失うとデバイスが管理不能になるため — `mat_core::acl` モジュール冒頭のコメント
/// と同じ方針）。
pub async fn ensure_group_acl(conn: &mut dyn NodeConn, group_id: u16) -> Result<bool, MatError> {
    let current = conn
        .read_json(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL)
        .await
        .map_err(|e| provision_step_err(e, "acl read"))?;
    let entries = entries_from_im_json(&current).map_err(|e| provision_step_err(e, "acl read"))?;
    let Some(merged) = merge_group_entry(&entries, group_id) else {
        return Ok(false); // 既に Group エントリがある。write 不要（冪等）。
    };
    let tlv = encode_acl_entries_tlv(&merged);
    conn.write_tlv(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, tlv, false)
        .await
        .map_err(|e| provision_step_err(e, "acl write"))?;
    Ok(true)
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

    // M8a Task9: group provision / grant のデバイス側ステップ。

    #[tokio::test]
    async fn provision_node_runs_steps_in_order() {
        let mut conn = FakeConn::scripted()
            .with_read(0, 0x003F, 0x0000, serde_json::json!([])) // group-key-map read
            .with_read(
                0,
                0x001F,
                0x0000,
                serde_json::json!([ // acl read（管理者のみ）
                    {"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]),
            );
        let p = ProvisionNodeParams {
            group_id: 10,
            keyset_id: 60,
            name: "grp10".into(),
            endpoint: 1,
            epoch_key: [0xAB; 16],
        };
        provision_node(&mut conn, &p).await.unwrap();
        let calls = conn.calls();
        // KeySetWrite invoke → group-key-map read → write → AddGroup invoke →
        // acl read → acl write の順。
        assert!(calls[0].starts_with("invoke(0,0x003F"), "{calls:?}"); // ep0 宛
        assert!(calls.iter().any(|c| c.starts_with("write_tlv(0,0x003F")));
        assert!(calls.iter().any(|c| c.starts_with("invoke(1,0x0004")));
        assert!(
            calls.last().unwrap().starts_with("write_tlv(0,0x001F"),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn ensure_group_acl_is_idempotent_when_entry_exists() {
        let mut conn = FakeConn::scripted().with_read(
            0,
            0x001F,
            0x0000,
            serde_json::json!([
                {"1": 5, "2": 2, "3": [1], "4": null, "254": 2},
                {"1": 3, "2": 3, "3": [10], "4": null, "254": 2}  // 既に Group エントリ
            ]),
        );
        let wrote = ensure_group_acl(&mut conn, 10).await.unwrap();
        assert!(!wrote);
        assert!(
            !conn.calls().iter().any(|c| c.starts_with("write_tlv")),
            "must not write when the Group entry already exists"
        );
    }

    #[tokio::test]
    async fn provision_node_replaces_existing_mapping_for_same_group() {
        // 既存 map に groupId=10→keyset 50 がある状態で keyset 60 を provision:
        // 書かれた map は 10→60 の1件（置換、重複しない）。
        let mut conn = FakeConn::scripted()
            .with_read(
                0,
                0x003F,
                0x0000,
                serde_json::json!([{"1": 10, "2": 50}]), // 既存 10→50
            )
            .with_read(
                0,
                0x001F,
                0x0000,
                serde_json::json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]), // 管理者のみ
            );
        let p = ProvisionNodeParams {
            group_id: 10,
            keyset_id: 60,
            name: "grp10".into(),
            endpoint: 1,
            epoch_key: [0xAB; 16],
        };
        provision_node(&mut conn, &p).await.unwrap();

        // group-key-map の write_tlv を検証: (10, 60) のみ（置換）
        let writes: Vec<_> = conn
            .written_tlv()
            .iter()
            .filter(|(ep, cl, attr, _)| *ep == 0 && *cl == 0x003F && *attr == 0x0000)
            .collect();
        assert_eq!(writes.len(), 1, "must write group-key-map exactly once");
        let expected_tlv = encode_group_key_map_tlv(&vec![(10, 60)]);
        assert_eq!(
            writes[0].3, expected_tlv,
            "group-key-map must contain only (10, 60) after replacement"
        );
    }

    #[tokio::test]
    async fn provision_node_preserves_other_groups_mappings() {
        // 既存 map に groupId=11→keyset 61 がある状態で groupId=10/keyset 60 を provision:
        // 書かれた map は {11→61, 10→60} の2件（他グループ温存）。
        let mut conn = FakeConn::scripted()
            .with_read(
                0,
                0x003F,
                0x0000,
                serde_json::json!([{"1": 11, "2": 61}]), // 既存 11→61
            )
            .with_read(
                0,
                0x001F,
                0x0000,
                serde_json::json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]), // 管理者のみ
            );
        let p = ProvisionNodeParams {
            group_id: 10,
            keyset_id: 60,
            name: "grp10".into(),
            endpoint: 1,
            epoch_key: [0xAB; 16],
        };
        provision_node(&mut conn, &p).await.unwrap();

        // group-key-map の write_tlv を検証: (11, 61) と (10, 60) の両方
        let writes: Vec<_> = conn
            .written_tlv()
            .iter()
            .filter(|(ep, cl, attr, _)| *ep == 0 && *cl == 0x003F && *attr == 0x0000)
            .collect();
        assert_eq!(writes.len(), 1, "must write group-key-map exactly once");
        // 期待値は両エントリ（順序は後で書いた 10,60 がリスト末尾）
        let expected_tlv = encode_group_key_map_tlv(&vec![(11, 61), (10, 60)]);
        assert_eq!(
            writes[0].3, expected_tlv,
            "group-key-map must preserve (11, 61) and add (10, 60)"
        );
    }
}
