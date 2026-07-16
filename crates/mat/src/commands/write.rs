//! `mat write` — 書き込み可能属性を設定する。
//!
//! `chip-tool <cluster> write <attribute> <value> <node_id> <endpoint>` をラップ。
//! 制御（照明 ON/OFF 等）は属性 write ではなく `invoke` を使う非対称に注意。

use std::path::Path;

use serde_json::json;

use crate::runner::ChipTool;
use mat_core::error::{ErrorKind, MatError};
use mat_core::normalize::classify_failure;
use mat_core::output;
use mat_core::parse::{normalize_value, operation_succeeded};
use mat_core::store::Store;

pub fn run(
    store_path: &Path,
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value: &str,
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let chip = ChipTool::new(store.root());

    let out = chip.run([
        cluster.to_string(),
        "write".to_string(),
        attribute.to_string(),
        value.to_string(),
        node_id.to_string(),
        endpoint.to_string(),
    ])?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("write {cluster}/{attribute} on node {node_id} endpoint {endpoint} failed"),
        ));
    }
    if !out.success() || !operation_succeeded(&out.stdout) {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!("write {cluster}/{attribute} on node {node_id} did not report success"),
        ));
    }

    emit_write_success(node_id, endpoint, cluster, attribute, value);
    Ok(())
}

/// `write` の成功 JSON を stdout へ emit する。chip-tool 経路と native 直経路
/// （`native_direct`）の両方から呼ばれる単一ソース（スキーマ不変）。
pub(crate) fn emit_write_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value: &str,
) {
    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "attribute": attribute,
        // read と型を揃える（`"100"` ではなく `100`）。mat は属性の型を持たないため、
        // CLI 入力文字列を read と同じ normalize_value で型推定する。
        "value": normalize_value(value),
        "status": "success",
    }));
}
