//! `mat group list`（`remove` の CLI 合成は device_op / native_direct 経由）。
//! ローカル KVS 読み取りのみ — `fabric init` と同じく iface 解決より前に
//! dispatch される（main.rs 参照）。

use std::path::Path;

use mat_controller::group_settings::{read_groups, GroupSettingsError};
use mat_controller::kvs::{KvsError, MAIN_INI_FILE};
use mat_core::body;
use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::store::Store;

pub fn run_list(store_path: &Path, fabric_index: u8) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    let main_ini = store.root().join(MAIN_INI_FILE);
    let table = read_groups(&main_ini, fabric_index).map_err(|e| map_gs_read_err(e, &main_ini))?;
    let groups: Vec<(u16, &str, Option<u16>)> = table
        .groups
        .iter()
        .map(|g| (g.group_id, g.name.as_str(), g.keyset_id))
        .collect();
    let keysets: Vec<(u16, &[u16])> = table
        .keysets
        .iter()
        .map(|k| (k.keyset_id, k.bound_groups.as_slice()))
        .collect();
    tracing::info!(fabric_index, groups = groups.len(), "group list executed");
    output::emit(body::group_list_success(fabric_index, &groups, &keysets));
    Ok(())
}

/// 読み出しエラーの写像: INI 不在 = store_missing（fabric init 案内）、
/// チェーン破損 = store_parse、flock 競合 / その他 = other。
fn map_gs_read_err(e: GroupSettingsError, main_ini: &Path) -> MatError {
    match &e {
        GroupSettingsError::Kvs(KvsError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
            MatError::store_missing(format!(
                "{} not found — run `mat fabric init` to bootstrap the credential store",
                main_ini.display()
            ))
        }
        GroupSettingsError::Corrupt { .. } => MatError::store_parse(e.to_string()),
        GroupSettingsError::Kvs(KvsError::Locked) => MatError::new(
            ErrorKind::Other,
            "controller kvs is locked by another process (concurrent provision?)",
        ),
        _ => MatError::new(ErrorKind::Other, e.to_string()),
    }
}
