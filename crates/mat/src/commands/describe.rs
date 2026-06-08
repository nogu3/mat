//! `mat describe` — ノードのエンドポイント / クラスタを introspect する。
//!
//! Descriptor クラスタを使う: エンドポイント 0 の `parts-list` で子エンドポイントを
//! 列挙し（0 自身も含める）、各エンドポイントの `server-list` でクラスタ ID を読む。
//! 複数回 chip-tool を呼ぶため遅いが、ワンショットで完結する（`mat` の方針どおり）。
//!
//! LLM が「何を叩けるか」を知るための AI-native の肝。クラスタは数値 ID の配列で返す
//! （人間向けの名前解決は `casa` の責務、`mat` は数値で完結する）。

use std::path::Path;

use serde_json::json;

use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::parse::parse_id_list;
use crate::runner::ChipTool;
use mat_core::normalize::classify_failure;
use mat_core::store::Store;

pub fn run(store_path: &Path, node_id: u64) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let chip = ChipTool::new(store.root());

    // エンドポイント 0 の parts-list で子エンドポイントを列挙。0 自身を先頭に足す。
    let parts = descriptor_list(&chip, node_id, 0, "parts-list")?;
    let mut endpoints: Vec<u16> = vec![0];
    for p in parts {
        if let Ok(ep) = u16::try_from(p) {
            if !endpoints.contains(&ep) {
                endpoints.push(ep);
            }
        }
    }

    let mut out_endpoints = Vec::new();
    for ep in endpoints {
        let clusters = descriptor_list(&chip, node_id, ep, "server-list")?;
        out_endpoints.push(json!({ "endpoint": ep, "clusters": clusters }));
    }

    output::emit(json!({
        "node_id": node_id,
        "endpoints": out_endpoints,
    }));
    Ok(())
}

/// `chip-tool descriptor read <list> <node> <ep>` を実行し ID リストを返す。
fn descriptor_list(
    chip: &ChipTool,
    node_id: u64,
    endpoint: u16,
    list: &str,
) -> Result<Vec<u64>, MatError> {
    let out = chip.run([
        "descriptor".to_string(),
        "read".to_string(),
        list.to_string(),
        node_id.to_string(),
        endpoint.to_string(),
    ])?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("describe: reading {list} on node {node_id} endpoint {endpoint} failed"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!("describe: chip-tool read {list} exited with {:?}", out.code),
        ));
    }
    Ok(parse_id_list(&out.stdout))
}
