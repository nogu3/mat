//! `mat discover` — commissionable / commissioned ノードを探索する。
//!
//! commissionable は `chip-tool discover commissionables` の mDNS 探索結果、
//! commissioned は `mat` の台帳（KVS）から読む。両者を1つの `devices` 配列にまとめる。

use std::path::Path;

use serde_json::json;

use crate::error::MatError;
use crate::output;
use crate::parse::parse_commissionables;
use crate::runner::ChipTool;
use crate::store::Store;

pub fn run(store_path: &Path) -> Result<(), MatError> {
    let store = Store::open(store_path)?; // 無ければ store_missing(exit 10)
    let chip = ChipTool::new(store.root());

    // commissionable 探索。chip-tool は探索を時間で打ち切るため非 0 終了もあり得る。
    // ここでは exit code で失敗扱いにせず、得られた行をパースする（child_not_found
    // = exit 12 だけは run() がエラーで返す）。
    let out = chip.run(["discover", "commissionables"])?;
    let commissionable = parse_commissionables(&out.stdout);

    let mut devices = Vec::new();
    for d in &commissionable {
        let mut v = serde_json::to_value(d).map_err(|e| {
            MatError::parse_error(format!("cannot serialize discovered device: {e}"))
        })?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("state".into(), json!("commissionable"));
        }
        devices.push(v);
    }
    for n in store.nodes() {
        devices.push(json!({
            "state": "commissioned",
            "node_id": n.node_id,
            "address": n.address,
            "commissioned_at": n.commissioned_at,
        }));
    }

    output::emit(json!({ "devices": devices }));
    Ok(())
}
