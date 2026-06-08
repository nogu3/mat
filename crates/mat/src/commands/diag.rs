//! `mat diag thread` — Thread Network Diagnostics (cluster 53) のスナップショット。
//!
//! メッシュの健全性分析用に、1 ノードの Thread 診断属性をワンショットで集約する
//! （複数回 `chip-tool` を呼ぶ。`describe` と同じ作法）。返す JSON で「近くに何台
//! いて電波がどれだけ強いか（neighbor-table の LQI/RSSI）」「中継しているか
//! （routing-role）」「メッシュが分断していないか（partition-id）」を読み取れる。
//!
//! 複数ノードを束ねた「メッシュ地図」化は上位層の責務。ここは 1 ノードの生診断を
//! `mat` スキーマへ正規化するだけ（クラスタ名/enum の名前解決はしない＝数値のまま）。

use std::path::Path;

use serde_json::{json, Value};

use crate::runner::ChipTool;
use mat_core::error::{ErrorKind, MatError};
use mat_core::normalize::classify_failure;
use mat_core::output;
use mat_core::parse::{parse_read_value, parse_struct_list};
use mat_core::store::Store;

/// Thread Network Diagnostics の chip-tool クラスタ名。
const CLUSTER: &str = "threadnetworkdiagnostics";

pub fn thread(store_path: &Path, node_id: u64, endpoint: u16) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let chip = ChipTool::new(store.root());

    // スカラ属性: パース不能なら null（デバイスが当該属性を持たない場合に備える）。
    // 到達不能/timeout 等の本物の失敗は read_attr が MatError で伝播する。
    let routing_role = parse_read_value(&read_attr(&chip, node_id, endpoint, "routing-role")?);
    let partition_id = parse_read_value(&read_attr(&chip, node_id, endpoint, "partition-id")?);
    let channel = parse_read_value(&read_attr(&chip, node_id, endpoint, "channel")?);
    let network_name = parse_read_value(&read_attr(&chip, node_id, endpoint, "network-name")?);
    let rloc16 = parse_read_value(&read_attr(&chip, node_id, endpoint, "rloc16")?);

    // list-of-struct: 隣接（LQI/RSSI）と経路（cost/hop）。メッシュ分析の本命。
    let neighbor_table = parse_struct_list(&read_attr(&chip, node_id, endpoint, "neighbor-table")?);
    let route_table = parse_struct_list(&read_attr(&chip, node_id, endpoint, "route-table")?);

    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "thread": {
            "routing_role": routing_role.unwrap_or(Value::Null),
            "partition_id": partition_id.unwrap_or(Value::Null),
            "channel": channel.unwrap_or(Value::Null),
            "network_name": network_name.unwrap_or(Value::Null),
            "rloc16": rloc16.unwrap_or(Value::Null),
            "neighbor_table": neighbor_table,
            "route_table": route_table,
        },
    }));
    Ok(())
}

/// `chip-tool threadnetworkdiagnostics read <attr> <node> <ep>` を実行し stdout を返す。
/// 失敗分類が立てばそれを、立たず非 0 終了なら child_failed を返す（`read` と同形）。
fn read_attr(chip: &ChipTool, node_id: u64, endpoint: u16, attr: &str) -> Result<String, MatError> {
    let out = chip.run([
        CLUSTER.to_string(),
        "read".to_string(),
        attr.to_string(),
        node_id.to_string(),
        endpoint.to_string(),
    ])?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("diag thread: reading {attr} on node {node_id} endpoint {endpoint} failed"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!(
                "diag thread: chip-tool read {attr} exited with {:?}",
                out.code
            ),
        ));
    }
    Ok(out.stdout)
}
