//! `mat describe` — ノードのエンドポイント / クラスタを introspect する。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。成功 JSON の形は `mat_core::body`（直経路・matd 共有の単一
//! ソース）、このモジュールは stdout への emit のみを持つ。クラスタは数値 ID の
//! 配列で返す（人間向けの名前解決は `mat` の責務外。`mat` は数値で完結する）。

use mat_core::{body, output};

/// `describe` の成功 JSON を stdout へ emit する（body は `mat_core::body` 共有）。
pub(crate) fn emit_describe_success(node_id: u64, endpoints: &[(u16, Vec<u64>)]) {
    output::emit(body::describe_success(node_id, endpoints));
}
