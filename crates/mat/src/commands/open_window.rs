//! `mat open-window` — `mat` 所有デバイスを他 admin（Alexa / Apple / Google 等）へ
//! 共有するため commissioning window を開く。
//!
//! `chip-tool pairing open-commissioning-window <node-id> <option> <timeout>
//! <iteration> <discriminator>` をラップする。`option = 1`（ECM: Enhanced
//! Commissioning Method）で一回限りの新コードを発行させ、`manual_code`（11桁）と
//! `qr_payload`（`MT:...` 文字列）の両方を返す。
//!
//! QR 画像のレンダリングは `mat` の責務ではない（stdout には文字列のみ。描画は上層）。
//! 「複数機器を QR 1枚でまとめて共有」は Matter 仕様上できない＝それはブリッジの話で
//! `mat` 外（`casa-bridge`）。`open-window` はネイティブ機器を1台ずつ共有する。

use std::path::Path;

use serde_json::json;

use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::parse::parse_open_window;
use crate::runner::ChipTool;
use mat_core::normalize::classify_failure;
use mat_core::store::Store;

/// ECM（Enhanced Commissioning Method）= 一回限りの新コードを発行する option。
const OPTION_ECM: &str = "1";

pub fn run(
    store_path: &Path,
    node_id: u64,
    timeout: u32,
    iteration: u32,
    discriminator: u16,
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let chip = ChipTool::new(store.root());

    let out = chip.run([
        "pairing".to_string(),
        "open-commissioning-window".to_string(),
        node_id.to_string(),
        OPTION_ECM.to_string(),
        timeout.to_string(),
        iteration.to_string(),
        discriminator.to_string(),
    ])?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("open-window on node {node_id} failed"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!(
                "chip-tool open-commissioning-window exited with {:?}",
                out.code
            ),
        ));
    }

    let codes = parse_open_window(&out.stdout);
    let (manual_code, qr_payload) = match (codes.manual_code, codes.qr_payload) {
        (Some(m), Some(q)) => (m, q),
        _ => {
            return Err(MatError::parse_error(format!(
                "could not parse manual_code / qr_payload from open-commissioning-window output for node {node_id}"
            )))
        }
    };

    output::emit(json!({
        "node_id": node_id,
        "manual_code": manual_code,
        "qr_payload": qr_payload,
        "expires_at": output::expires_in(i64::from(timeout)),
    }));
    Ok(())
}
