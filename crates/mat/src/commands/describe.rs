//! `mat describe` — ノードのエンドポイント / クラスタを introspect する。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。このモジュールは native 経路から呼ばれる成功 JSON の emit のみを
//! 持つ（スキーマの単一ソース）。クラスタは数値 ID の配列で返す（人間向けの名前
//! 解決は `mat` の責務外。`mat` は数値で完結する）。

use serde_json::json;

use mat_core::output;

/// `describe` の成功 JSON を stdout へ emit する。native 直経路（`native_direct`）
/// から呼ばれる単一ソース（スキーマ不変）。
pub(crate) fn emit_describe_success(node_id: u64, endpoints: &[(u16, Vec<u64>)]) {
    let out_endpoints: Vec<serde_json::Value> = endpoints
        .iter()
        .map(|(ep, clusters)| json!({ "endpoint": ep, "clusters": clusters }))
        .collect();
    output::emit(json!({
        "node_id": node_id,
        "endpoints": out_endpoints,
    }));
}
