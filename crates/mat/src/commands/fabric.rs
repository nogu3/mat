//! `mat fabric init` / `list` / `rotate-ipk`。`init` / `list` は直経路のみ・
//! ネットワーク未接触（KVS ローカル生成/読取だけ）で iface 解決より前に
//! dispatch される（main.rs 参照）。`rotate-ipk` はネットワークに出るため
//! iface 解決後の直経路 dispatch を通る — 状態機械の実体は
//! `mat_native::rotate_ipk`、ここは台帳でのノード解決と body の emit だけ。

use std::path::Path;

use serde_json::json;

use mat_controller::commissioning::CommissioningFabric;
use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_native::rotate_ipk::{self, RotateIpkParams, RotateMode};
use mat_native::NativeConfig;

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

/// `mat fabric list`: main KVS を走査して fabric ごとの identity を返す。
/// 鍵素材は出さない。
pub fn run_list(store_path: &Path, current_fabric_index: u8) -> Result<(), MatError> {
    use mat_controller::kvs::{self, KvsError, MAIN_INI_FILE};
    let main_ini = store_path.join(MAIN_INI_FILE);
    let map_err = |e: KvsError| match e {
        KvsError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            MatError::store_missing(format!(
                "{} not found — run `mat fabric init` to bootstrap the credential store",
                main_ini.display()
            ))
        }
        other => MatError::store_parse(format!("fabric list: {other}")),
    };
    let mut fabrics = Vec::new();
    for idx in kvs::list_fabric_indices(&main_ini).map_err(map_err)? {
        let (admin_node_id, fabric_id) = kvs::read_noc_identity(&main_ini, idx).map_err(map_err)?;
        let pubkey = kvs::read_rcac_pubkey(&main_ini, idx).map_err(map_err)?;
        let cfid = mat_controller::fabric::compressed_fabric_id(&pubkey, fabric_id);
        let ipk_epoch = match kvs::read_mat_ipk_epoch(&main_ini, idx).map_err(map_err)? {
            Some(_) => "mat",
            None => "chip-tool",
        };
        let pending = kvs::read_mat_ipk_epoch_slot(&main_ini, idx, kvs::IpkEpochSlot::Next)
            .map_err(map_err)?
            .is_some();
        fabrics.push(json!({
            "fabric_index": idx,
            "fabric_id": fabric_id,
            "admin_node_id": admin_node_id,
            "compressed_fabric_id": format!("{:016X}", u64::from_be_bytes(cfid)),
            "ipk_epoch": ipk_epoch,
            "ipk_rotation_pending": pending,
            "current": idx == current_fabric_index,
        }));
    }
    tracing::info!(count = fabrics.len(), "fabric list executed");
    output::emit(mat_core::body::fabric_list_success(
        &store_path.display().to_string(),
        fabrics,
    ));
    Ok(())
}

/// `mat fabric rotate-ipk`: 状態機械は `mat_native::rotate_ipk`、ここは台帳での
/// ノード解決（省略 = 全ノード）と body の emit だけ。部分失敗（pending /
/// catch_up_incomplete）は stdout に body を出したうえで stderr error を返す
/// （終了コードは最初に失敗したノードの kind）。
pub fn run_rotate_ipk(
    store_path: &Path,
    nodes: &[u64],
    catch_up: bool,
    abort: bool,
    native: Option<&crate::native_direct::Config<'_>>,
    op_timeout_ms: u64,
) -> Result<(), MatError> {
    let cfg = native.ok_or_else(|| {
        MatError::new(
            ErrorKind::Other,
            "rotate-ipk: native backend not configured (internal)",
        )
    })?;
    let store = mat_core::store::Store::open(store_path)?;
    let node_ids: Vec<u64> = if nodes.is_empty() {
        store.nodes().map(|n| n.node_id).collect()
    } else {
        for &id in nodes {
            store.require_node(id)?;
        }
        nodes.to_vec()
    };
    let mode = if abort {
        RotateMode::Abort
    } else if catch_up {
        RotateMode::CatchUp
    } else {
        RotateMode::Rotate
    };
    let params = RotateIpkParams {
        node_ids,
        mode,
        per_node_timeout_ms: op_timeout_ms,
    };
    let native_cfg = NativeConfig {
        store: store.root().to_path_buf(),
        iface: cfg.iface.to_string(),
        thread_iface: cfg.thread_iface.clone(),
        fabric_index: cfg.fabric_index,
        issuer_index: cfg.issuer_index,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MatError::new(ErrorKind::Other, format!("tokio runtime: {e}")))?;
    let outcome = rt
        .block_on(rotate_ipk::run(&native_cfg, &params))
        .map_err(crate::native_direct::map_engine_build_error)?;
    tracing::info!(
        status = outcome.status.as_str(),
        nodes = outcome.nodes.len(),
        "fabric rotate-ipk executed"
    );
    output::emit(outcome.body(cfg.fabric_index));
    match outcome.partial_error() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
