//! 単一インスタンスガード。`<socket>.lock` に排他 advisory ロック（flock）を取り、
//! 二重起動を防ぐ。ロックは open file description に紐づき、プロセス終了（kill/crash
//! 含む）で OS が自動解放するため stale 状態が残らない。取得した `File` を保持する
//! 限りロックは有効。

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use rustix::fs::{flock, FlockOperation};

use mat_core::error::{ErrorKind, MatError};

/// ロックファイルパス（socket パス + `.lock`）。
pub fn lock_path(socket_path: &Path) -> PathBuf {
    let mut p = socket_path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// 排他ロックを取得する。既に別 matd が保持していれば `Err`（`ErrorKind::Other`）。
/// 返す `File` はプロセス生存中保持すること（Drop でロック解放）。
pub fn acquire(socket_path: &Path) -> Result<File, MatError> {
    let path = lock_path(socket_path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("failed to open matd lock file {}: {e}", path.display()),
            )
        })?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        // ロック競合 = 別の matd が稼働中。
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Err(MatError::new(
            ErrorKind::Other,
            format!("matd already running (lock held at {})", path.display()),
        )),
        Err(e) => Err(MatError::new(
            ErrorKind::Other,
            format!("failed to lock matd lock file {}: {e}", path.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn second_acquire_fails_then_succeeds_after_release() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("matd.sock");

        let first = acquire(&sock).expect("first acquire should succeed");
        // 保持中は 2 度目が失敗（別 open file description なので同一プロセスでも競合）。
        let err = acquire(&sock).expect_err("second acquire must fail while held");
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(
            err.detail.contains("already running"),
            "detail should say already running, got: {}",
            err.detail
        );

        // 解放すれば再取得できる。
        drop(first);
        let _again = acquire(&sock).expect("acquire after release should succeed");
    }

    #[test]
    fn lock_path_appends_suffix() {
        assert_eq!(
            lock_path(Path::new("/run/mat/matd.sock")),
            PathBuf::from("/run/mat/matd.sock.lock")
        );
    }
}
