//! mDNS プローブ。`--iface`（`MAT_IFACE`）設定時は native browse
//! （`mat-controller::dnssd`、M8b）、未設定・IO 失敗時は `avahi-browse` に
//! フォールバック。プロセス起動（avahi）と socket I/O を伴うため副作用なしの
//! `mat-core` ではなくバイナリ側に置く。`diag node --deep` と
//! `discover --probe` が共有する。

use std::ffi::OsString;
use std::process::Command as StdCommand;

use mat_core::diag::{parse_avahi_matter, MatterInstance};
use mat_core::error::{ErrorKind, MatError};

/// `_matter._tcp` インスタンスを列挙する。iface 指定時は native browse、
/// IO 失敗は warn + avahi-browse フォールバック（read-only なので二重実行の
/// 害なし）。結果 0 件は正常（フォールバックしない）。
pub fn mdns(iface: Option<&str>) -> Result<Vec<MatterInstance>, MatError> {
    if let Some(iface) = iface {
        match native(iface) {
            Ok(list) => return Ok(list),
            Err(e) => {
                tracing::warn!(
                    iface,
                    error = %e,
                    "native mDNS browse failed; falling back to avahi-browse"
                );
            }
        }
    }
    avahi()
}

/// native browse（M8b）。エラーは呼び出し側が avahi へフォールバックする。
fn native(iface: &str) -> Result<Vec<MatterInstance>, Box<dyn std::error::Error>> {
    let scope_id = mat_controller::dnssd::iface_index(iface)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let list = rt.block_on(mat_controller::dnssd::browse_operational(
        scope_id,
        mat_controller::dnssd::BROWSE_WINDOW,
    ))?;
    tracing::info!(instances = list.len(), "probe executed (native browse)");
    Ok(list.into_iter().map(to_matter_instance).collect())
}

/// browse 結果 → 既存の診断データモデルへの写し。到達性判定
/// （`mat_core::reachability::resolve`）と diag の self-fabric 照合は
/// この型を経由するため無改変で動く。
fn to_matter_instance(o: mat_controller::dnssd::OperationalInstance) -> MatterInstance {
    MatterInstance {
        compressed_fabric: o.compressed_fabric,
        node_id: o.node_id,
        addresses: o.addresses.iter().map(|a| a.to_string()).collect(),
    }
}

/// `avahi-browse -rt _matter._tcp` を実行して `_matter._tcp` インスタンスを得る。
/// バイナリは `MAT_AVAHI_BROWSE_BIN` で上書き可。
fn avahi() -> Result<Vec<MatterInstance>, MatError> {
    let bin =
        std::env::var_os("MAT_AVAHI_BROWSE_BIN").unwrap_or_else(|| OsString::from("avahi-browse"));
    let out = StdCommand::new(&bin)
        .args(["-rt", "_matter._tcp"])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MatError::child_not_found(format!("avahi-browse not found ({bin:?})"))
            } else {
                MatError::new(
                    ErrorKind::Other,
                    format!("avahi-browse spawn failed ({bin:?}): {e}"),
                )
            }
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    let stderr_text = String::from_utf8_lossy(&out.stderr);
    tracing::debug!(%text, "avahi-browse stdout");
    tracing::debug!(%stderr_text, "avahi-browse stderr");
    Ok(parse_avahi_matter(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_matter_instance_stringifies_addresses() {
        let o = mat_controller::dnssd::OperationalInstance {
            compressed_fabric: "00AABB1122CC3344".to_string(),
            node_id: 5,
            addresses: vec!["fd00::10".parse().unwrap()],
        };
        let m = to_matter_instance(o);
        assert_eq!(m.node_id, 5);
        assert_eq!(m.addresses, vec!["fd00::10".to_string()]);
    }
}
