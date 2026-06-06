//! `mat write` — 書き込み可能属性を設定する。
//!
//! `chip-tool <cluster> write <attribute> <value> <node_id> <endpoint>` をラップ。
//! 制御（照明 ON/OFF 等）は属性 write ではなく `invoke` を使う非対称に注意。

use std::path::Path;

use serde_json::json;

use crate::error::{ErrorKind, MatError};
use crate::output;
use crate::parse::operation_succeeded;
use crate::runner::{classify_failure, ChipTool};
use crate::store::Store;

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

    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "attribute": attribute,
        "value": value,
        "status": "success",
    }));
    Ok(())
}
