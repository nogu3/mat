//! `mat fabric init` — 初回 fabric bootstrap（M8c-3）。直経路のみ・
//! ネットワーク未接触（KVS ローカル生成だけ）。iface 解決より前に
//! dispatch される（main.rs 参照）。

use std::path::Path;

use serde_json::json;

use mat_controller::commissioning::CommissioningFabric;
use mat_core::error::{ErrorKind, MatError};
use mat_core::output;

/// 初回 fabric bootstrap: root CA + ランダム epoch IPK を生成し、chip-tool
/// INI 互換 KVS を新規作成する。store ディレクトリが無ければ作る（init は
/// bootstrap 経路 — `Store::open` の store_missing 既定とは異なる）。
pub fn run_init(
    store_path: &Path,
    fabric_id: u64,
    admin_node_id: u64,
    fabric_index: u8,
    issuer_index: u8,
) -> Result<(), MatError> {
    std::fs::create_dir_all(store_path).map_err(|e| {
        MatError::store_missing(format!(
            "cannot create store dir {}: {e}",
            store_path.display()
        ))
    })?;

    let fab = CommissioningFabric::generate(fabric_id, admin_node_id)
        .map_err(|e| MatError::new(ErrorKind::Other, format!("fabric generate: {e}")))?;

    fab.write_kvs_bootstrap(store_path, fabric_index, issuer_index)
        .map_err(|e| {
            let kind = match e {
                mat_controller::kvs::KvsError::AlreadyExists => ErrorKind::Other,
                _ => ErrorKind::StoreParse,
            };
            MatError::new(
                kind,
                format!(
                    "fabric init: {e} (store: {}; 既存 KVS の上書きはしない — 再初期化は両 ini を手動削除)",
                    store_path.display()
                ),
            )
        })?;

    tracing::info!("fabric bootstrap written (native kvs)");

    // 出力 JSON（スキーマ: timestamp 必須、key material は一切含めない）。
    let rcac =
        mat_controller::cert::MatterCert::parse(&fab.rcac_tlv).expect("just-generated rcac parses");
    let cfid = mat_controller::fabric::compressed_fabric_id(&rcac.pub_key, fab.fabric_id);
    output::emit(json!({
        "store": store_path.display().to_string(),
        "fabric_id": fab.fabric_id,
        "fabric_index": fabric_index,
        "compressed_fabric_id": format!("{:016X}", u64::from_be_bytes(cfid)),
        "admin_node_id": fab.admin_node_id,
    }));
    Ok(())
}
