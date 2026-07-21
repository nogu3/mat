//! 上流（`mat --matd` クライアント）⇔ `matd` の unix socket 既定パス。
//!
//! `mat`（既定探索）と `matd`（`--socket` 省略時の bind）が同じ既定を指すよう、
//! 一箇所で定義する。0.27.0 で既定を systemd の `RuntimeDirectory=matd` 慣習
//! （`$XDG_RUNTIME_DIR/matd/matd.sock`）へ移行。mat 側の探索は移行期互換のため
//! 旧 flat パス（`$XDG_RUNTIME_DIR/matd.sock`）も第 2 候補として connect を試す。

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// `matd` の既定 bind パス: `$XDG_RUNTIME_DIR/matd/matd.sock`、XDG 不在なら
/// `/tmp/matd.sock`（/tmp 直下に固定名 dir を作ると他ユーザーの dir squatting
/// 面が増えるだけなので flat のまま）。
pub fn default_socket_path() -> PathBuf {
    default_socket_path_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// [`default_socket_path`] の env 注入版（テスト用に純関数）。
pub fn default_socket_path_from(xdg_runtime_dir: Option<OsString>) -> PathBuf {
    match xdg_runtime_dir {
        Some(dir) => PathBuf::from(dir).join("matd").join("matd.sock"),
        None => PathBuf::from("/tmp/matd.sock"),
    }
}

/// `mat` の既定探索候補（順に connect を試す）: subdir 新既定 → flat 旧既定。
/// XDG 不在なら `/tmp/matd.sock` の 1 本。stale socket は connect が失敗する
/// ので自然に次候補へ進む。
pub fn default_socket_candidates() -> Vec<PathBuf> {
    default_socket_candidates_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// [`default_socket_candidates`] の env 注入版（テスト用に純関数）。
pub fn default_socket_candidates_from(xdg_runtime_dir: Option<OsString>) -> Vec<PathBuf> {
    match xdg_runtime_dir {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            vec![dir.join("matd").join("matd.sock"), dir.join("matd.sock")]
        }
        None => vec![PathBuf::from("/tmp/matd.sock")],
    }
}

/// socket の親ディレクトリを 0700 で作成する（存在すれば no-op）。matd が
/// 既定パスで bind する前に呼ぶ（明示 `--socket` の親不在は従来どおり bind
/// エラーに任せるので呼ばない）。`recursive` なので途中の祖先 dir（例: 存在しない
/// `XDG_RUNTIME_DIR` 自体）も同じ 0700 で作られる。
pub fn ensure_socket_dir(socket: &Path) -> io::Result<()> {
    let Some(dir) = socket.parent() else {
        return Ok(());
    };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_path_is_subdir_under_xdg() {
        assert_eq!(
            default_socket_path_from(Some("/run/user/1000".into())),
            PathBuf::from("/run/user/1000/matd/matd.sock")
        );
    }

    #[test]
    fn default_path_falls_back_to_flat_tmp_without_xdg() {
        assert_eq!(
            default_socket_path_from(None),
            PathBuf::from("/tmp/matd.sock")
        );
    }

    #[test]
    fn candidates_are_subdir_then_flat_under_xdg() {
        assert_eq!(
            default_socket_candidates_from(Some("/run/user/1000".into())),
            vec![
                PathBuf::from("/run/user/1000/matd/matd.sock"),
                PathBuf::from("/run/user/1000/matd.sock"),
            ]
        );
    }

    #[test]
    fn candidates_are_single_tmp_without_xdg() {
        assert_eq!(
            default_socket_candidates_from(None),
            vec![PathBuf::from("/tmp/matd.sock")]
        );
    }

    #[test]
    fn ensure_socket_dir_creates_0700_parent_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("matd").join("matd.sock");

        ensure_socket_dir(&sock).unwrap();
        let meta = std::fs::metadata(sock.parent().unwrap()).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);

        // 既存 dir でも成功する（冪等）。
        ensure_socket_dir(&sock).unwrap();
    }
}
