//! 上流（`mat --matd` クライアント）⇔ `matd` の unix socket 既定パス。
//!
//! `mat`（`--matd` の値省略時）と `matd`（`--socket` 省略時）が同じ既定を指すよう、
//! 一箇所で定義する。

use std::path::PathBuf;

/// 既定の matd socket パス: `$XDG_RUNTIME_DIR/matd.sock`、無ければ `/tmp/matd.sock`。
pub fn default_socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("matd.sock")
    } else {
        PathBuf::from("/tmp/matd.sock")
    }
}
