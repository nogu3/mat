//! flock 排他 + tmp/rename 原子置換。`KvsTxn`（chip-tool INI）と
//! `group::PersistedGroupCounter`（group data counter）が同じ規律を共有する。

use std::io;
use std::path::{Path, PathBuf};

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
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
