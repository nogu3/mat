//! `mat invoke` — コマンドを実行する。
//!
//! `chip-tool <cluster> <command> [args...] <node_id> <endpoint>` をラップ。
//! chip-tool は宛先 node_id / endpoint を**末尾**に取る。コマンド引数はその前。
//! `mat on` / `mat off` は OnOff クラスタの On/Off コマンドを **invoke** に
//! マップしたショートカット（属性 write ではない）で、ここを再利用する。

use std::path::Path;

use serde_json::json;

use crate::runner::ChipTool;
use mat_core::error::{ErrorKind, MatError};
use mat_core::normalize::classify_failure;
use mat_core::output;
use mat_core::parse::operation_succeeded;
use mat_core::store::Store;

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

    // chip-tool は `<cluster> <command> [command-args...] <node_id> <endpoint>` の順で
    // 宛先を末尾に取る。コマンド引数を node_id/endpoint の前に置かないと、引数が宛先
    // として誤読され（node_id=0 等）応答が来ず timeout する。
    let mut argv = vec![cluster.to_string(), command.to_string()];
    argv.extend(args.iter().cloned());
    argv.push(node_id.to_string());
    argv.push(endpoint.to_string());

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
