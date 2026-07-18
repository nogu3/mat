//! `mat write` — 書き込み可能属性を設定する。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。このモジュールは native 経路から呼ばれる成功 JSON の emit のみを
//! 持つ（スキーマの単一ソース）。制御（照明 ON/OFF 等）は属性 write ではなく
//! `invoke` を使う非対称に注意。

use serde_json::json;

use mat_core::output;
use mat_core::parse::normalize_value;

/// `write` の成功 JSON を stdout へ emit する。native 直経路（`native_direct`）
/// から呼ばれる単一ソース（スキーマ不変）。
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
