//! flock 排他 + tmp/rename 原子置換。`KvsTxn`（chip-tool INI）と
//! `group::PersistedGroupCounter`（group data counter）が同じ規律を共有する。
//!
//! store には root CA 秘密鍵（alpha ini）と IPK（main ini）が平文で入るので、
//! 本体・tmp・lock sidecar はすべて owner-only（0600）で作る。tmp+rename で
//! 毎回置換されるため、手動 `chmod 600` は次の書込で消える — 権限はここで
//! 毎回付け直す。

use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// store 内ファイルの mode（owner rw のみ）。
const PRIVATE_FILE_MODE: u32 = 0o600;

/// `path` の隣の sidecar `<path>.lock` を advisory flock（NonBlocking
/// exclusive）する。本体は tmp+rename で置換されるので本体 fd への flock は
/// rename 後に無効化される — 安定した別ファイルに取り、戻り値の `File` を
/// 持っている間だけロックが生きる（Drop で OS が解放）。競合は
/// `io::ErrorKind::WouldBlock`。
pub(crate) fn take_lock(path: &Path) -> io::Result<std::fs::File> {
    use rustix::fs::{flock, FlockOperation};
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .open(PathBuf::from(lock_path))?;
    flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        if e == rustix::io::Errno::WOULDBLOCK {
            io::Error::new(io::ErrorKind::WouldBlock, "locked by another process")
        } else {
            io::Error::other(e)
        }
    })?;
    Ok(lock)
}

/// `<path>.tmp`（ファイル名末尾に付加 — `with_extension` の stem 衝突
/// （`a.ini` と `a.counter` が同じ `a.tmp` を取り合う）を避ける）へ書き、
/// `sync_all` してから `rename` で置換する。クラッシュしても途中書きの
/// 本体は残らない。
///
/// tmp は残骸（前回クラッシュの 0644 等）を消してから `create_new`
/// （O_EXCL — symlink / hard link 残骸を辿らない）+ mode 0600 で作り直し、
/// fd に 0600 を明示設定する（umask に依らず rename 後の本体が必ず 0600）。
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(&tmp)?;
    f.set_permissions(std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn tmp_of(p: &Path) -> PathBuf {
        let mut t = p.as_os_str().to_owned();
        t.push(".tmp");
        PathBuf::from(t)
    }

    #[test]
    fn atomic_replace_creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chip_tool_config.alpha.ini");
        atomic_replace(&path, b"[Default]\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[Default]\n");
        assert_eq!(mode_of(&path), 0o600);
        assert!(!tmp_of(&path).exists());
    }

    #[test]
    fn atomic_replace_narrows_previously_world_readable_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chip_tool_config.ini");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_replace(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn atomic_replace_ignores_stale_wide_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chip_tool_config.ini");
        let tmp = tmp_of(&path);
        std::fs::write(&tmp, "stale leftover that is longer than the new body").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_replace(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(mode_of(&path), 0o600);
        assert!(!tmp.exists());
    }

    #[test]
    fn take_lock_creates_owner_only_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chip_tool_config.ini");
        let _lock = take_lock(&path).unwrap();
        assert_eq!(
            mode_of(&dir.path().join("chip_tool_config.ini.lock")),
            0o600
        );
    }
}
