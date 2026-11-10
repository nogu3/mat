//! 上流（`mat --matd` クライアント）⇔ `matd` の unix socket 既定パス。
//!
//! `mat`（既定探索）と `matd`（`--socket` 省略時の bind）が同じ既定を指すよう、
//! 一箇所で定義する。0.27.0 で既定を systemd の `RuntimeDirectory=matd` 慣習
//! （`$XDG_RUNTIME_DIR/matd/matd.sock`）へ移行。mat 側の探索は移行期互換のため
//! 旧 flat パス（`$XDG_RUNTIME_DIR/matd.sock`）も第 2 候補として connect を試す。
//!
//! `XDG_RUNTIME_DIR` が無い（または空・相対）ときは既定を**持たない**。以前の
//! `/tmp/matd.sock` fallback は廃止した: world-writable な /tmp では matd 不在中に
//! 他ユーザーが偽 socket を先置きでき、`mat` がそこへ op を送って応答を信じて
//! しまう（matd 側の「stale 削除 → bind」も失敗 / 競合する）。既定が無いとき
//! `matd` は `--socket` を要求してエラー、`mat` の自動発見は候補 0 本（= matd
//! 不在と同じ扱いで直経路）になる。

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// `XDG_RUNTIME_DIR` を採用できる形（非空の絶対パス）なら返す。XDG Base
/// Directory 仕様どおり、相対パスは無効として無視する。
fn usable_runtime_dir(xdg_runtime_dir: Option<OsString>) -> Option<PathBuf> {
    xdg_runtime_dir
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// `matd` の既定 bind パス: `$XDG_RUNTIME_DIR/matd/matd.sock`。XDG 不在（空・
/// 相対を含む）なら `None` — 呼び手は `--socket` 明示を求める。
pub fn default_socket_path() -> Option<PathBuf> {
    default_socket_path_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// [`default_socket_path`] の env 注入版（テスト用に純関数）。
pub fn default_socket_path_from(xdg_runtime_dir: Option<OsString>) -> Option<PathBuf> {
    usable_runtime_dir(xdg_runtime_dir).map(|dir| dir.join("matd").join("matd.sock"))
}

/// `mat` の既定探索候補（順に connect を試す）: subdir 新既定 → flat 旧既定。
/// XDG 不在（空・相対を含む）なら空 — 自動発見は「matd 不在」と同じく直経路へ
/// 落ちる。stale socket は connect が失敗するので自然に次候補へ進む。
pub fn default_socket_candidates() -> Vec<PathBuf> {
    default_socket_candidates_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// [`default_socket_candidates`] の env 注入版（テスト用に純関数）。
pub fn default_socket_candidates_from(xdg_runtime_dir: Option<OsString>) -> Vec<PathBuf> {
    match usable_runtime_dir(xdg_runtime_dir) {
        Some(dir) => vec![dir.join("matd").join("matd.sock"), dir.join("matd.sock")],
        None => Vec::new(),
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
            Some(PathBuf::from("/run/user/1000/matd/matd.sock"))
        );
    }

    /// XDG 不在なら既定は無い（共有 /tmp への fallback は廃止 — 他ユーザーが
    /// 偽 socket を先置きできるため）。
    #[test]
    fn default_path_is_none_without_xdg() {
        assert_eq!(default_socket_path_from(None), None);
    }

    /// 空・相対の XDG_RUNTIME_DIR は XDG Base Directory 仕様どおり無視する
    /// （相対だと cwd 基準の bind になり、置き場所が実行場所次第になる）。
    #[test]
    fn empty_or_relative_xdg_is_treated_as_unset() {
        assert_eq!(default_socket_path_from(Some("".into())), None);
        assert_eq!(default_socket_path_from(Some("run/user".into())), None);
        assert!(default_socket_candidates_from(Some("".into())).is_empty());
        assert!(default_socket_candidates_from(Some("run/user".into())).is_empty());
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
    fn candidates_are_empty_without_xdg() {
        assert!(default_socket_candidates_from(None).is_empty());
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
