//! `mat read` — 属性を読む。
//!
//! `chip-tool <cluster> read <attribute> <node_id> <endpoint>` をラップし、
//! `Data = ...` 行から値をパースして `mat` スキーマに正規化する。
//! read は認証情報必須経路なので store は bootstrap しない（無ければ exit 10）、
//! 未 commission node は exit 11。

use std::path::Path;

use serde_json::json;

use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::parse::parse_read_value;
use crate::runner::ChipTool;
use mat_core::normalize::classify_failure;
use mat_core::store::Store;

pub fn run(
    store_path: &Path,
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let chip = ChipTool::new(store.root());

    let out = chip.run([
        cluster.to_string(),
        "read".to_string(),
        attribute.to_string(),
        node_id.to_string(),
        endpoint.to_string(),
    ])?;

    // chip-tool の exit code は粗い。出力から失敗を分類できればそれを優先する。
    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("read {cluster}/{attribute} on node {node_id} endpoint {endpoint} failed"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!("chip-tool read exited with {:?}", out.code),
        ));
    }

    let value = parse_read_value(&out.stdout).ok_or_else(|| {
        MatError::parse_error(format!(
            "could not parse a value from chip-tool read output for {cluster}/{attribute}"
        ))
    })?;

    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "attribute": attribute,
        "value": value,
    }));
    Ok(())
}
