//! `mat unpair` — デバイスの RemoveFabric（自 fabric）+ 台帳 / alias 削除。
//!
//! 直経路のみ（台帳 `nodes.json` の書き手は mat だけ — 設計ルール 4）。
//! デバイス側は `NodeOpKind::RemoveFabric`（mat-native::op の 1 arm）、
//! 台帳側は `Store::remove_node` / `AliasBook::remove_node`。`--force` は
//! デバイス側の失敗を出力 JSON の `device.error` に畳んで台帳だけ消す。

use std::path::Path;

use serde_json::{json, Value};

use mat_core::alias::AliasBook;
use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::store::Store;
use mat_native::op::{NodeOp, NodeOpKind};

use crate::device_op::DeviceOp;
use crate::native_direct;

pub fn run(
    store_path: &Path,
    node_id: u64,
    force: bool,
    native: Option<&native_direct::Config<'_>>,
    op_timeout_ms: u64,
) -> Result<(), MatError> {
    let mut store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let cfg = native.ok_or_else(|| {
        MatError::new(
            ErrorKind::Other,
            "unpair: native backend not configured (internal)",
        )
    })?;

    let op = DeviceOp::Node(NodeOp {
        node_id,
        kind: NodeOpKind::RemoveFabric,
    });
    let device = match native_direct::execute(&op, store_path, cfg, op_timeout_ms) {
        Ok(body) => body,
        Err(e) if force && is_device_side_failure(e.kind) => {
            tracing::warn!(
                node_id,
                kind = ?e.kind,
                detail = %e.detail,
                "unpair --force: device-side RemoveFabric failed; removing ledger entry anyway"
            );
            device_failure_body(&e)
        }
        Err(e) => return Err(e),
    };

    let ledger_removed = store.remove_node(node_id)?;
    let mut book = AliasBook::load(store.root())?;
    let aliases_removed = book.remove_node(node_id, store.root())?;
    tracing::info!(node_id, ledger_removed, "unpair executed");
    output::emit(json!({
        "node_id": node_id,
        "aliases_removed": aliases_removed,
        "device": device,
        "ledger": { "removed": ledger_removed },
    }));
    Ok(())
}

/// `--force` が畳んでよい失敗か（spec §1: `--force` の対象はデバイス側の失敗だけ）。
///
/// 畳むのはトランスポート / IM の結果、つまり exit 3-6 に対応する 4 種のみ。
/// `store_missing` / iface 自動検出の `other` / `parse_error` などローカル側の
/// 失敗は「デバイスが応答しない」証拠が何も無いので、`--force` でも台帳を消さず
/// そのまま伝播させる。
fn is_device_side_failure(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Unreachable
            | ErrorKind::Timeout
            | ErrorKind::DeviceRejected
            | ErrorKind::SessionFailed
    )
}

/// `--force` でデバイス側が失敗したときの `device` オブジェクト。
fn device_failure_body(e: &MatError) -> Value {
    json!({ "removed": false, "error": e.to_json()["error"].clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_folds_only_device_side_failures() {
        for kind in [
            ErrorKind::Unreachable,
            ErrorKind::Timeout,
            ErrorKind::DeviceRejected,
            ErrorKind::SessionFailed,
        ] {
            assert!(is_device_side_failure(kind), "{kind:?} should be foldable");
        }
        // ローカル / 設定側の失敗は台帳を消してよい根拠にならない。
        for kind in [
            ErrorKind::StoreMissing,
            ErrorKind::StoreParse,
            ErrorKind::NodeNotCommissioned,
            ErrorKind::ParseError,
            ErrorKind::Other,
            ErrorKind::MatdUnavailable,
            ErrorKind::CommissionFailed,
        ] {
            assert!(
                !is_device_side_failure(kind),
                "{kind:?} must propagate even with --force"
            );
        }
    }

    #[test]
    fn device_failure_body_carries_kind_and_detail() {
        let e = MatError::new(ErrorKind::Unreachable, "Node 15 is unreachable");
        assert_eq!(
            device_failure_body(&e),
            json!({ "removed": false, "error": { "kind": "unreachable", "detail": "Node 15 is unreachable" } })
        );
    }
}
