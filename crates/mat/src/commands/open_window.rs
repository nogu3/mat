//! `mat open-window` — `mat` 所有デバイスを他 admin（Alexa / Apple / Google 等）へ
//! 共有するため commissioning window を開く。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。このモジュールは native 経路から呼ばれる成功 JSON の emit のみを
//! 持つ（スキーマの単一ソース）。ECM（Enhanced Commissioning Method）で一回限りの
//! 新コードを発行させ、`manual_code`（11桁）と `qr_payload`（`MT:...` 文字列）の
//! 両方を返す。
//!
//! QR 画像のレンダリングは `mat` の責務ではない（stdout には文字列のみ）。
//! 「複数機器を QR 1枚でまとめて共有」は Matter 仕様上できない＝それはブリッジ
//! （別プロジェクト）の話で `mat` 外。`open-window` はネイティブ機器を1台ずつ共有する。

use serde_json::json;

use mat_core::output;

/// `open-window` の成功 JSON を stdout へ emit する。native 直経路
/// （`native_direct`）から呼ばれる単一ソース（スキーマ不変）。
pub(crate) fn emit_open_window_success(
    node_id: u64,
    manual_code: &str,
    qr_payload: &str,
    timeout: u32,
) {
    output::emit(json!({
        "node_id": node_id,
        "manual_code": manual_code,
        "qr_payload": qr_payload,
        "expires_at": output::expires_in(i64::from(timeout)),
    }));
}
