//! `mat read` — 属性を読む。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。成功 JSON の形は `mat_core::body`（直経路・matd 共有の単一
//! ソース）、このモジュールは stdout への emit のみを持つ。

use mat_core::{body, output};

/// `read` の成功 JSON を stdout へ emit する（body は `mat_core::body` 共有）。
pub(crate) fn emit_read_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value: serde_json::Value,
) {
    output::emit(body::read_success(
        node_id, endpoint, cluster, attribute, value,
    ));
}
