//! controller 側 group state の native KVS 書込（M8c-2）。
//!
//! chip-tool `groupsettings` 4 コマンド相当を `mat-controller::group_settings`
//! に委譲する薄いラッパー。mat 直経路（native_direct）と matd（server::
//! group_provision）の両方が使う。エラーは mat の ErrorKind へ写像する
//! （フォールバック判断は呼び出し側 — ctx 未構成のみフォールバック対象。
//! ここから返るエラーは全て hard error）。

use std::path::PathBuf;

use mat_controller::group_settings::{GroupProvisionWrite, GroupSettingsError};
use mat_core::error::{ErrorKind, MatError};

/// KVS 書込に必要な資材。`Engine::build` が KVS 読出し時に組み立てる。
pub struct GroupSettingsCtx {
    pub main_ini: PathBuf,
    pub fabric_index: u8,
    pub cfid: [u8; 8],
}

pub fn write_group_provision(
    ctx: &GroupSettingsCtx,
    group_id: u16,
    keyset_id: u16,
    name: &str,
    epoch_key: &[u8; 16],
    rebind: bool,
) -> Result<(), MatError> {
    mat_controller::group_settings::write_group_provision(
        &ctx.main_ini,
        ctx.fabric_index,
        &ctx.cfid,
        &GroupProvisionWrite {
            group_id,
            keyset_id,
            name,
            epoch_key: *epoch_key,
            rebind,
        },
    )
    .map_err(map_gs_err)?;
    tracing::info!(
        group_id,
        keyset_id,
        "group provision controller state written (native kvs)"
    );
    Ok(())
}

/// GroupSettingsError → ErrorKind。全て hard error（ワイヤ未接触だが KVS は
/// 触った可能性があるため chip-tool を重ねない）。kind は Other に寄せ、
/// detail で復旧手段を示す（chip-tool 経路の分類とは厳密一致しない —
/// native op の写像表と同じ扱い）。
fn map_gs_err(e: GroupSettingsError) -> MatError {
    let detail = match &e {
        GroupSettingsError::DuplicateBind { group_id, keyset_id } => format!(
            "keyset {keyset_id} is already bound to group {group_id} in the controller kvs; use --rebind"
        ),
        GroupSettingsError::Kvs(mat_controller::kvs::KvsError::Locked) => {
            "controller kvs is locked by another process (concurrent provision?)".to_string()
        }
        other => format!("controller kvs group write failed: {other}"),
    };
    MatError::new(ErrorKind::Other, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &tempfile::TempDir) -> GroupSettingsCtx {
        let p = dir.path().join("chip_tool_config.ini");
        std::fs::write(&p, "[Default]\n").unwrap();
        GroupSettingsCtx {
            main_ini: p,
            fabric_index: 2,
            cfid: [7u8; 8],
        }
    }

    #[test]
    fn write_group_provision_writes_kvs_and_maps_duplicate_to_other() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        write_group_provision(&c, 99, 99, "e2e", &[0x42; 16], false).unwrap();
        // 読み側で解決できる（controller 層の round-trip は Task 3 で証明済み、
        // ここは配線の確認だけ）。
        assert!(mat_controller::kvs::read_group_credentials(&c.main_ini, 2, 99).is_ok());
        // 二重 bind は Other + "--rebind" 誘導の detail。
        let err = write_group_provision(&c, 99, 99, "e2e", &[0x42; 16], false).unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::Other);
        assert!(err.detail.contains("--rebind"), "{}", err.detail);
        // rebind なら通る。
        write_group_provision(&c, 99, 99, "e2e", &[0x42; 16], true).unwrap();
    }

    #[test]
    fn locked_kvs_is_hard_error_not_fallback_shaped() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        let _held = mat_controller::kvs::KvsTxn::open(&c.main_ini).unwrap();
        let err = write_group_provision(&c, 99, 99, "e2e", &[0x42; 16], false).unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::Other);
        assert!(err.detail.contains("locked"), "{}", err.detail);
    }
}
