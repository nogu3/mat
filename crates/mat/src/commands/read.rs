//! `mat read` — 属性を読む。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。このモジュールは native 経路から呼ばれる成功 JSON の emit のみを
//! 持つ（スキーマの単一ソース）。

use serde_json::json;

use mat_core::output;

/// `read` の成功 JSON を stdout へ emit する。native 直経路（`native_direct`）
/// から呼ばれる単一ソース（スキーマ不変）。
pub(crate) fn emit_read_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value: serde_json::Value,
) {
    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "attribute": attribute,
        "value": value,
    }));
}
