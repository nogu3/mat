//! `mat write` — 書き込み可能属性を設定する。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。成功 JSON の形は `mat_core::body`（直経路・matd 共有の単一
//! ソース）、このモジュールは stdout への emit のみを持つ。制御（照明 ON/OFF 等）
//! は属性 write ではなく `invoke` を使う非対称に注意。

use mat_core::{body, output};

/// `write` の成功 JSON を stdout へ emit する（body は `mat_core::body` 共有）。
pub(crate) fn emit_write_success(
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    attribute: &str,
    value: &str,
) {
    output::emit(body::write_success(
        node_id, endpoint, cluster, attribute, value,
    ));
}
