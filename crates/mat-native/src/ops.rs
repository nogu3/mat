//! mat / matd 共有の native op ロジック（describe / diag thread）。
//!
//! `NodeConn` の上に立つ純粋ロジック層 —— バックエンド（実 CASE / test fake）
//! を問わない。値の符号化を伴わない読み取り専用 op なので `classify`（`op::NodeOpKind`
//! の write/invoke コンストラクタと違い）は常に `Some/None` — 値エラーはない。

use std::collections::HashMap;

use serde_json::{Map, Value};

use mat_controller::im::{
    encode_add_group_fields, encode_group_key_map_tlv, encode_key_set_write_fields,
    encode_key_set_write_fields_multi, ATTR_ACL, ATTR_GROUP_KEY_MAP, CLUSTER_ACCESS_CONTROL,
    CLUSTER_GROUPS, CLUSTER_GROUP_KEY_MANAGEMENT, CMD_ADD_GROUP, CMD_KEY_SET_WRITE,
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

/// JSON 配列から数値要素のみを u64 化して集める（配列でない/数値でない
/// 要素はスキップする寛容な変換）。
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
    ("leader_router_id", "leader-router-id"),
    ("mesh_local_prefix", "mesh-local-prefix"),
];

/// list-of-struct 属性: (出力キー, chip-tool 属性名)。
const TABLES: &[(&str, &str)] = &[
    ("neighbor_table", "neighbor-table"),
    ("route_table", "route-table"),
];

/// NeighborTableStruct（cluster 53）の field id → chip-tool 表記名。
/// 表記は `crates/mat-core/src/parse.rs` の `struct_list_parses_neighbor_table`
/// テスト（＝実 chip-tool の `neighbor-table` ログと同値）から確定。
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
/// テスト（＝実 chip-tool の `route-table` ログと同値）から確定。
/// **注意**: LQI 表記は NeighborTable の "Lqi" と揃わず "LQIIn"/"LQIOut"
/// （chip-tool の実際の表記ゆれ、テストのログサンプルが正）。
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

/// General Diagnostics（cluster 0x33）NetworkInterfaces。
const CLUSTER_GENERAL_DIAG: u32 = 0x0033;
const ATTR_NETWORK_INTERFACES: u32 = 0x0000;
/// NetworkInterfaceStruct の InterfaceTypeEnum: 4 = Thread。
const IFACE_TYPE_THREAD: u64 = 4;

/// cluster 0x33 NetworkInterfaces から Thread インターフェースの自己同定情報
/// （HardwareAddress = 802.15.4 ExtAddress、IPv6 一覧）を取り出す。
/// Thread IF が無い / 形が想定外なら Ok(None)（mesh 収集は自己同定なしで続行可能
/// — read 自体の失敗は Err で伝播し、呼び出し側が None に丸めるか決める）。
/// struct のキーは context tag の 10 進文字列（`tlv_element_to_json` 参照）:
/// "0"=Name, "1"=IsOperational, "4"=HardwareAddress(octstr→hex),
/// "6"=IPv6Addresses(list of octstr→hex), "7"=Type。
pub async fn thread_identity(
    conn: &mut dyn NodeConn,
    endpoint: u16,
) -> Result<Option<mat_core::mesh::Identity>, MatError> {
    let v = conn
        .read_json(endpoint, CLUSTER_GENERAL_DIAG, ATTR_NETWORK_INTERFACES)
        .await?;
    let Some(items) = v.as_array() else {
        return Ok(None);
    };
    for item in items {
        let Some(o) = item.as_object() else { continue };
        if o.get("7").and_then(Value::as_u64) != Some(IFACE_TYPE_THREAD) {
            continue;
        }
        let Some(hw) = o.get("4").and_then(Value::as_str) else {
            continue;
        };
        let ipv6 = o
            .get("6")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        return Ok(Some(mat_core::mesh::Identity {
            ext_address: hw.to_string(),
            ipv6,
        }));
    }
    Ok(None)
}

/// diag node の thread シグナル: `ThreadSnapshot` から `ThreadCheck`
/// （隣接数 / 最良 LQI / routing role）を導出する。`neighbor_table` が
/// 読めていない（wildcard に含まれず Null、または属性自体が cluster に無い）
/// 場合は Err（chip-tool 経路の「neighbor-table が読めなければ thread check
/// 不可」と同義）。
pub fn thread_check_from_snapshot(
    snap: &ThreadSnapshot,
) -> Result<mat_core::diag::ThreadCheck, MatError> {
    let rows = snap
        .fields
        .get("neighbor_table")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            MatError::new(
                ErrorKind::Other,
                "neighbor-table not reported by node (unsupported cluster or read failed)",
            )
        })?;
    let best_lqi = rows
        .iter()
        .filter_map(|r| r.get("Lqi").and_then(Value::as_u64))
        .map(|v| v as u8)
        .max();
    let routing_role = snap.fields.get("routing_role").and_then(Value::as_i64);
    Ok(mat_core::diag::ThreadCheck {
        neighbor_count: rows.len(),
        best_lqi,
        routing_role,
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
#[derive(Clone)]
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
pub(crate) fn encode_acl_entries_tlv(entries: &[AclEntry]) -> Vec<u8> {
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

/// IPK の KeySet id（spec §11.2.6.2）。
pub const IPK_KEYSET_ID: u16 = 0;

/// IPK keyset（keyset 0）へ `KeySetWrite` を 1 回打つ — `fabric rotate-ipk` の
/// 配布 / catch-up の 1 ステップ。`epochs` は (epoch_key, start_time) 1〜3 本、
/// start_time は単調増加かつ非 0（spec §11.2.8.1）。ep0、timed 無し。失敗は
/// detail に `key-set-write (ipk): ` を前置。
pub async fn write_ipk_keyset(
    conn: &mut dyn NodeConn,
    epochs: &[([u8; 16], u64)],
) -> Result<(), MatError> {
    let fields = encode_key_set_write_fields_multi(IPK_KEYSET_ID, epochs);
    conn.invoke(
        0,
        CLUSTER_GROUP_KEY_MANAGEMENT,
        CMD_KEY_SET_WRITE,
        Some(fields),
        false,
    )
    .await
    .map_err(|e| MatError::new(e.kind, format!("key-set-write (ipk): {}", e.detail)))
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

/// Groups cluster RemoveGroup（spec §1.3.7.4）/ GroupKeyManagement KeySetRemove
/// （§11.2.8.3）。`mat_controller::im` には足さずここで局所定義する。
pub const CMD_REMOVE_GROUP: u32 = 0x03;
pub const CMD_KEY_SET_REMOVE: u32 = 0x03;
/// RemoveGroupResponse.status の NOT_FOUND（グループ未登録 — 冪等に続行）。
const STATUS_NOT_FOUND: u8 = 0x8B;

/// 1 ノード分の group 撤収の入力。
#[derive(Clone)]
pub struct RemoveGroupNodeParams {
    pub group_id: u16,
    /// RemoveGroup を送るエンドポイント（ACL / group-key-map / KeySetRemove は常に ep0）。
    pub endpoint: u16,
}

/// 1 ノード分の撤収結果（各ステップで実際に書いたか）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveGroupNodeReport {
    pub acl_removed: bool,
    pub group_removed: bool,
    pub keymap_removed: bool,
    pub keyset_removed: bool,
}

/// 撤収の 1 ステップに失敗した際、どのステップかを detail に残す
/// （`provision_step_err` と同粒度）。
fn remove_step_err(e: MatError, step: &str) -> MatError {
    MatError::new(e.kind, format!("remove step '{step}' failed: {}", e.detail))
}

/// provision の逆順: ACL の Group エントリ除去（read-merge-write、read 失敗時は
/// 絶対に write しない）→ RemoveGroup（NOT_FOUND は冪等に続行）→ group-key-map
/// から当該 group の行を除去 → その keyset を参照する行が残らなければ KeySetRemove。
pub async fn remove_group_node(
    conn: &mut dyn NodeConn,
    p: &RemoveGroupNodeParams,
) -> Result<RemoveGroupNodeReport, MatError> {
    // 1) ACL（全置換 write なので read できなければ絶対に write しない —
    //    管理者エントリを失うとデバイスが管理不能になる）。
    let current = conn
        .read_json(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL)
        .await
        .map_err(|e| remove_step_err(e, "acl read"))?;
    let entries = entries_from_im_json(&current).map_err(|e| remove_step_err(e, "acl read"))?;
    let acl_removed = match mat_core::acl::without_group_entry(&entries, p.group_id) {
        None => false,
        Some(kept) => {
            conn.write_tlv(
                0,
                CLUSTER_ACCESS_CONTROL,
                ATTR_ACL,
                encode_acl_entries_tlv(&kept),
                false,
            )
            .await
            .map_err(|e| remove_step_err(e, "acl write"))?;
            true
        }
    };

    // 2) RemoveGroup（データ応答 {0: status, 1: groupID}）
    let resp = conn
        .invoke_for_data(
            p.endpoint,
            CLUSTER_GROUPS,
            CMD_REMOVE_GROUP,
            Some(encode_remove_group_fields(p.group_id)),
            false,
        )
        .await
        .map_err(|e| remove_step_err(e, "groups remove-group"))?;
    let status =
        decode_response_status(&resp).map_err(|e| remove_step_err(e, "groups remove-group"))?;
    let group_removed = match status {
        0 => true,
        STATUS_NOT_FOUND => false,
        s => {
            return Err(MatError::new(
                ErrorKind::DeviceRejected,
                format!(
                    "remove step 'groups remove-group' failed: RemoveGroupResponse status {s:#04x}"
                ),
            ))
        }
    };

    // 3) group-key-map read-merge-write
    let current = conn
        .read_json(0, CLUSTER_GROUP_KEY_MANAGEMENT, ATTR_GROUP_KEY_MAP)
        .await
        .map_err(|e| remove_step_err(e, "group-key-map read"))?;
    let entries =
        parse_group_key_map(&current).map_err(|e| remove_step_err(e, "group-key-map read"))?;
    let removed_keysets: Vec<u16> = entries
        .iter()
        .filter(|(g, _)| *g == p.group_id)
        .map(|(_, k)| *k)
        .collect();
    let kept: Vec<(u16, u16)> = entries
        .iter()
        .copied()
        .filter(|(g, _)| *g != p.group_id)
        .collect();
    let keymap_removed = !removed_keysets.is_empty();
    if keymap_removed {
        conn.write_tlv(
            0,
            CLUSTER_GROUP_KEY_MANAGEMENT,
            ATTR_GROUP_KEY_MAP,
            encode_group_key_map_tlv(&kept),
            false,
        )
        .await
        .map_err(|e| remove_step_err(e, "group-key-map write"))?;
    }

    // 4) KeySetRemove（そのデバイスの map に参照が残っていない keyset だけ）
    let mut keyset_removed = false;
    for ks in removed_keysets {
        if kept.iter().any(|(_, k)| *k == ks) {
            continue;
        }
        conn.invoke(
            0,
            CLUSTER_GROUP_KEY_MANAGEMENT,
            CMD_KEY_SET_REMOVE,
            Some(encode_key_set_remove_fields(ks)),
            false,
        )
        .await
        .map_err(|e| remove_step_err(e, "key-set-remove"))?;
        keyset_removed = true;
    }
    Ok(RemoveGroupNodeReport {
        acl_removed,
        group_removed,
        keymap_removed,
        keyset_removed,
    })
}

/// RemoveGroup `{0: groupID}`。
fn encode_remove_group_fields(group_id: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(group_id));
    w.end_container();
    w.finish()
}

/// KeySetRemove `{0: groupKeySetID}`。
fn encode_key_set_remove_fields(keyset_id: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(keyset_id));
    w.end_container();
    w.finish()
}

/// `{0: status, ...}` 形の応答から status（ctx0 の uint）だけを読む。外側 struct
/// 直下の ctx0 のみを見る — 途中の nested container（struct / array / list）は
/// 丸ごと飛ばし、その中の ctx0 や ContainerEnd を誤読しない。
fn decode_response_status(fields: &[u8]) -> Result<u8, MatError> {
    use mat_controller::tlv::{skip_container, Reader, TlvError, Value as Tlv};
    let bad = |what: &str| MatError::parse_error(format!("RemoveGroupResponse: {what}"));
    let tlv_err = |e: TlvError| match e {
        TlvError::Truncated => bad("truncated"),
        _ => bad("tlv decode error"),
    };
    let mut r = Reader::new(fields);
    match r.next().map_err(tlv_err)? {
        Some(el) if el.value == Tlv::StructStart => {}
        _ => return Err(bad("missing struct")),
    }
    loop {
        let el = r.next().map_err(tlv_err)?.ok_or_else(|| bad("truncated"))?;
        match (el.tag, el.value) {
            (_, Tlv::ContainerEnd) => return Err(bad("missing status")),
            (Tag::Context(0), Tlv::Uint(v)) => {
                return u8::try_from(v).map_err(|_| bad("status out of range"))
            }
            (_, Tlv::StructStart | Tlv::ArrayStart | Tlv::ListStart) => {
                skip_container(&mut r).map_err(tlv_err)?
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeConn;

    #[test]
    fn decode_response_status_flat_and_nested() {
        use mat_controller::tlv::Tag;
        // 平坦な RemoveGroupResponse `{0: status, 1: groupID}`。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0x8B); // NOT_FOUND
        w.put_uint(Tag::Context(1), 7);
        w.end_container();
        assert_eq!(decode_response_status(&w.finish()).unwrap(), 0x8B);

        // status より前に nested container（struct 内に ctx0、array）が来ても、
        // その中の ctx0 を status と誤読せず、内側の ContainerEnd で
        // 「missing status」に早落ちしない。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(2));
        w.put_uint(Tag::Context(0), 0x55); // ← これは status ではない
        w.end_container();
        w.start_array(Tag::Context(3));
        w.put_uint(Tag::Anonymous, 1);
        w.end_container();
        w.put_uint(Tag::Context(0), 0);
        w.end_container();
        assert_eq!(decode_response_status(&w.finish()).unwrap(), 0);

        // nested container だけで status が無い → missing status（truncated ではない）。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(2));
        w.put_uint(Tag::Context(0), 0x55);
        w.end_container();
        w.end_container();
        let err = decode_response_status(&w.finish()).unwrap_err();
        assert!(err.detail.contains("missing status"), "{}", err.detail);

        // nested container が途中で切れている → truncated。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(2));
        w.put_uint(Tag::Context(0), 0x55);
        w.end_container();
        w.end_container();
        let bytes = w.finish();
        let err = decode_response_status(&bytes[..bytes.len() - 2]).unwrap_err();
        assert!(err.detail.contains("truncated"), "{}", err.detail);
    }

    #[tokio::test]
    async fn thread_identity_picks_thread_interface() {
        // NetworkInterfaces: eth (type=2) と Thread (type=4)。Thread を選ぶ。
        let mut conn = FakeConn::scripted().with_read(
            0,
            0x0033,
            0x0000,
            serde_json::json!([
                {"0": "eth0", "1": true, "4": "aabbccddeeff", "6": [], "7": 2},
                {"0": "wpan0", "1": true, "4": "0011223344556677",
                 "6": ["fd00112233445566000000fffe001400",
                        "fe800000000000000011223344556677"],
                 "7": 4}
            ]),
        );
        let id = thread_identity(&mut conn, 0).await.unwrap().unwrap();
        assert_eq!(id.ext_address, "0011223344556677");
        assert_eq!(id.ipv6.len(), 2);
    }

    #[tokio::test]
    async fn thread_identity_none_without_thread_interface() {
        let mut conn = FakeConn::scripted().with_read(
            0,
            0x0033,
            0x0000,
            serde_json::json!([
                {"0": "eth0", "1": true, "4": "aabbccddeeff", "6": [], "7": 2}
            ]),
        );
        assert!(thread_identity(&mut conn, 0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn thread_identity_none_on_non_array() {
        let mut conn = FakeConn::scripted().with_read(0, 0x0033, 0x0000, serde_json::json!(null));
        assert!(thread_identity(&mut conn, 0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn diag_thread_includes_leader_and_ml_prefix_scalars() {
        // 既存 diag_thread_maps_names_and_partial_results と同じ組み方で、
        // leader-router-id(0x000d) と mesh-local-prefix(0x0005) が fields に載ること。
        let mut conn = FakeConn::scripted().with_cluster(
            0,
            0x0035,
            vec![
                (0x0001, serde_json::json!(5)),
                (0x000d, serde_json::json!(8)),
                (0x0005, serde_json::json!("fd00112233445566")),
            ],
        );
        let snap = diag_thread(&mut conn, 0).await.unwrap();
        assert_eq!(snap.fields["leader_router_id"], serde_json::json!(8));
        assert_eq!(
            snap.fields["mesh_local_prefix"],
            serde_json::json!("fd00112233445566")
        );
    }

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
    async fn thread_check_from_snapshot_computes_max_lqi_and_routing_role() {
        // diag_thread → thread_check_from_snapshot の連結。neighbor-table 2行
        // （field id "5" = Lqi）から best_lqi = max、routing-role はそのまま数値化。
        let mut conn = FakeConn::scripted().with_cluster(
            1,
            0x0035,
            vec![
                (0x0001, serde_json::json!(2)), // routing-role
                (
                    0x0007,
                    serde_json::json!([
                        {"0": 42, "5": 120},
                        {"0": 43, "5": 200},
                    ]),
                ), // neighbor-table, 2 rows
            ],
        );
        let snap = diag_thread(&mut conn, 1).await.unwrap();
        let check = thread_check_from_snapshot(&snap).unwrap();
        assert_eq!(check.neighbor_count, 2);
        assert_eq!(check.best_lqi, Some(200));
        assert_eq!(check.routing_role, Some(2));
    }

    #[tokio::test]
    async fn thread_check_from_snapshot_errs_when_neighbor_table_missing() {
        // neighbor-table 属性がデバイスから返らない（wildcard に含まれない）
        // ケース = null。chip-tool 経路の「読めなければ thread check 不可」と同義。
        let mut conn = FakeConn::scripted().with_cluster(
            1,
            0x0035,
            vec![(0x0001, serde_json::json!(2))], // routing-role のみ
        );
        let snap = diag_thread(&mut conn, 1).await.unwrap();
        let err = thread_check_from_snapshot(&snap).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
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
            async fn invoke_for_data(
                &mut self,
                _endpoint: u16,
                _cluster: u32,
                _command: u32,
                _fields: Option<Vec<u8>>,
                _timed: bool,
            ) -> Result<Vec<u8>, MatError> {
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
        let expected_tlv = encode_group_key_map_tlv(&[(10, 60)]);
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
        let expected_tlv = encode_group_key_map_tlv(&[(11, 61), (10, 60)]);
        assert_eq!(
            writes[0].3, expected_tlv,
            "group-key-map must preserve (11, 61) and add (10, 60)"
        );
    }

    // M8b Task12: group 撤収（remove_group_node）のデバイス側ステップ。

    /// ACL read JSON（IM 形: `{1: privilege, 2: authMode, 3: subjects,
    /// 4: targets, 254: fabricIndex}`）— 管理者エントリ + 指定 group の
    /// Group エントリ。
    fn acl_json_with_group(group_id: u16) -> Value {
        serde_json::json!([
            {"1": 5, "2": 2, "3": [112233], "4": null, "254": 1},
            {"1": 3, "2": 3, "3": [group_id], "4": null, "254": 1}
        ])
    }

    /// ACL read JSON — 管理者エントリのみ（Group エントリ無し）。
    fn acl_json_without_group() -> Value {
        serde_json::json!([{"1": 5, "2": 2, "3": [112233], "4": null, "254": 1}])
    }

    fn remove_group_response_tlv(status: u8, group_id: u16) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(status));
        w.put_uint(Tag::Context(1), u64::from(group_id));
        w.end_container();
        w.finish()
    }

    /// group-key-map の read JSON（IM 形: フィールドは数字キー "1"=groupId,
    /// "2"=groupKeySetID）。
    fn gkm_json(rows: &[(u16, u16)]) -> Value {
        Value::Array(
            rows.iter()
                .map(|(g, k)| serde_json::json!({ "1": g, "2": k, "254": 1 }))
                .collect(),
        )
    }

    #[tokio::test]
    async fn remove_group_node_runs_four_steps_and_removes_unreferenced_keyset() {
        let acl = acl_json_with_group(1);
        let mut conn = FakeConn::scripted()
            .with_read(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, acl)
            .with_read(
                0,
                CLUSTER_GROUP_KEY_MANAGEMENT,
                ATTR_GROUP_KEY_MAP,
                gkm_json(&[(1, 42)]),
            )
            .with_invoke_response(
                2,
                CLUSTER_GROUPS,
                CMD_REMOVE_GROUP,
                remove_group_response_tlv(0, 1),
            );
        let rep = remove_group_node(
            &mut conn,
            &RemoveGroupNodeParams {
                group_id: 1,
                endpoint: 2,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rep,
            RemoveGroupNodeReport {
                acl_removed: true,
                group_removed: true,
                keymap_removed: true,
                keyset_removed: true
            }
        );
        assert_eq!(
            conn.calls(),
            &[
                format!("write_tlv(0,{CLUSTER_ACCESS_CONTROL:#06X},{ATTR_ACL:#06X})"),
                format!("invoke_for_data(2,{CLUSTER_GROUPS:#06X},{CMD_REMOVE_GROUP:#06X})"),
                format!(
                    "write_tlv(0,{CLUSTER_GROUP_KEY_MANAGEMENT:#06X},{ATTR_GROUP_KEY_MAP:#06X})"
                ),
                format!("invoke(0,{CLUSTER_GROUP_KEY_MANAGEMENT:#06X},{CMD_KEY_SET_REMOVE:#06X})"),
            ]
        );
    }

    #[tokio::test]
    async fn remove_group_node_keeps_keyset_still_referenced_and_tolerates_not_found() {
        let acl = acl_json_without_group();
        let mut conn = FakeConn::scripted()
            .with_read(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, acl)
            .with_read(
                0,
                CLUSTER_GROUP_KEY_MANAGEMENT,
                ATTR_GROUP_KEY_MAP,
                gkm_json(&[(1, 42), (2, 42)]),
            )
            .with_invoke_response(
                2,
                CLUSTER_GROUPS,
                CMD_REMOVE_GROUP,
                remove_group_response_tlv(0x8B, 1),
            );
        let rep = remove_group_node(
            &mut conn,
            &RemoveGroupNodeParams {
                group_id: 1,
                endpoint: 2,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rep,
            RemoveGroupNodeReport {
                acl_removed: false,
                group_removed: false,
                keymap_removed: true,
                keyset_removed: false
            }
        );
        // ACL write 無し（Group エントリが元々無い）/ KeySetRemove 無し
        // （keyset 42 は group 2 からまだ参照されている）。command id が
        // RemoveGroup と同値（0x03）なので部分一致ではなく完全一致で見る。
        assert_eq!(
            conn.calls(),
            &[
                format!("invoke_for_data(2,{CLUSTER_GROUPS:#06X},{CMD_REMOVE_GROUP:#06X})"),
                format!(
                    "write_tlv(0,{CLUSTER_GROUP_KEY_MANAGEMENT:#06X},{ATTR_GROUP_KEY_MAP:#06X})"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn remove_group_node_never_writes_acl_when_read_fails() {
        let mut conn = FakeConn::scripted().with_read(
            0,
            CLUSTER_ACCESS_CONTROL,
            ATTR_ACL,
            serde_json::json!("not-a-list"),
        );
        let err = remove_group_node(
            &mut conn,
            &RemoveGroupNodeParams {
                group_id: 1,
                endpoint: 2,
            },
        )
        .await
        .unwrap_err();
        assert!(err.detail.contains("acl read"), "{}", err.detail);
        assert!(conn.calls().is_empty(), "read 失敗で一切 write しない");
    }

    #[tokio::test]
    async fn write_ipk_keyset_invokes_key_set_write_on_ep0_with_keyset_0() {
        let mut conn = FakeConn::scripted();
        write_ipk_keyset(&mut conn, &[([0x0C; 16], 1), ([0x0E; 16], 2)])
            .await
            .unwrap();
        assert_eq!(conn.calls(), &["invoke(0,0x003F,0x0000)".to_string()]);
    }

    #[tokio::test]
    async fn write_ipk_keyset_prefixes_error_with_step_name() {
        let mut conn = FakeConn {
            fail_first_send: true,
            fail_kind: ErrorKind::DeviceRejected,
            ..FakeConn::scripted()
        };
        let err = write_ipk_keyset(&mut conn, &[([0x0C; 16], 1)])
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        assert!(
            err.detail.starts_with("key-set-write (ipk): "),
            "{}",
            err.detail
        );
    }
}
