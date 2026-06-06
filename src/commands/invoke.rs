//! `mat invoke` — コマンドを実行する。
//!
//! `chip-tool <cluster> <command> <node_id> <endpoint> [args...]` をラップ。
//! `mat on` / `mat off` は OnOff クラスタの On/Off コマンドを **invoke** に
//! マップしたショートカット（属性 write ではない）で、ここを再利用する。

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
    command: &str,
    args: &[String],
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let chip = ChipTool::new(store.root());

    let mut argv = vec![cluster.to_string(), command.to_string()];
    argv.push(node_id.to_string());
    argv.push(endpoint.to_string());
    argv.extend(args.iter().cloned());

    let out = chip.run(argv)?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("invoke {cluster}/{command} on node {node_id} endpoint {endpoint} failed"),
        ));
    }
    if !out.success() || !operation_succeeded(&out.stdout) {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!("invoke {cluster}/{command} on node {node_id} did not report success"),
        ));
    }

    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "command": command,
        "status": "success",
    }));
    Ok(())
}

/// `mat on` / `mat off` の実体。OnOff クラスタの On/Off コマンドを invoke する。
pub fn run_onoff(store_path: &Path, node_id: u64, endpoint: u16, on: bool) -> Result<(), MatError> {
    let command = if on { "on" } else { "off" };
    run(store_path, node_id, endpoint, "onoff", command, &[])
}
