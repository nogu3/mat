//! `mat commission` — fabric への参加（初回 commission / multi-admin join 両対応）。
//!
//! `chip-tool pairing code <node-id> <setup-code>` をラップする。`setup_code` は
//! 印刷された QR/manual code（初回）でも、既存 admin が開いた window の発行コード
//! （join）でも一様に扱える。Root CA / 自分の NOC は chip-tool が初回 pairing 時に
//! ストア配下へ生成・永続する。
//!
//! `target`（IP/DNS）は台帳のメタとして記録する。`pairing code` はコード内の
//! discriminator から mDNS でノードを自前探索するため、chip-tool には渡さない。

use std::path::Path;

use serde_json::json;

use crate::error::{ErrorKind, MatError};
use crate::output;
use crate::parse::commission_succeeded;
use crate::runner::{classify_failure, ChipTool};
use crate::store::{NodeRecord, Store};

pub fn run(
    store_path: &Path,
    target: &str,
    setup_code: &str,
    node_id: Option<u64>,
) -> Result<(), MatError> {
    // commission はストアを bootstrap してよい経路（初回 fabric 作成を含む）。
    let mut store = Store::open_or_init(store_path)?;
    let chip = ChipTool::new(store.root());

    let node_id = node_id.unwrap_or_else(|| next_node_id(&store));

    let out = chip.run(["pairing", "code", &node_id.to_string(), setup_code])?;

    if out.success() && commission_succeeded(&out.stdout) {
        store.upsert_node(NodeRecord {
            node_id,
            address: Some(target.to_string()),
            commissioned_at: output::now_iso8601(),
        })?;
        output::emit(json!({ "node_id": node_id, "status": "success" }));
        return Ok(());
    }

    // 失敗。chip-tool の粗い exit code に頼らず出力から種別を分類し、
    // 分類できなければ commission_failed にフォールバック。
    let kind = classify_failure(&out.stdout, &out.stderr).unwrap_or(ErrorKind::CommissionFailed);
    Err(MatError::new(
        kind,
        format!("commissioning node {node_id} ({target}) failed"),
    ))
}

/// 台帳の最大 node_id + 1。空なら 1。
fn next_node_id(store: &Store) -> u64 {
    store.nodes().map(|n| n.node_id).max().map_or(1, |m| m + 1)
}
