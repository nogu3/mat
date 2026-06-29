//! mDNS プローブ（`avahi-browse`）。プロセス起動を伴うため副作用なしの `mat-core`
//! ではなくバイナリ側に置く。`diag node --deep` と `discover --probe` が共有する。

use std::ffi::OsString;
use std::process::Command as StdCommand;

use mat_core::diag::{parse_avahi_matter, MatterInstance};
use mat_core::error::{ErrorKind, MatError};

/// `avahi-browse -rt _matter._tcp` を実行して `_matter._tcp` インスタンスを得る。
/// バイナリは `MAT_AVAHI_BROWSE_BIN` で上書き可。
pub fn mdns() -> Result<Vec<MatterInstance>, MatError> {
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
