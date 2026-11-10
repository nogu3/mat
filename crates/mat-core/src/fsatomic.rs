//! Atomic なファイル置換（tmp + fsync + rename）。
//!
//! `std::fs::write` は O_TRUNC 上書きのため、電源断・クラッシュのタイミングで
//! ファイル全体を失う。`mat-controller` の group counter persist と同じ規律を
//! 台帳（nodes.json）と aliases.toml に適用するための共有ヘルパ。
//!
//! 書いたファイルは常に owner-only（0600）、[`create_private_dir_all`] が
//! 新規に作るディレクトリは 0700。credential store の中身を他ユーザーに
//! 読ませない（tmp+rename で毎回置換されるので手動 chmod は残らない）。

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// 書き込むファイルの mode（owner rw のみ）。
const PRIVATE_FILE_MODE: u32 = 0o600;
/// 新規作成するディレクトリの mode（owner rwx のみ）。
const PRIVATE_DIR_MODE: u32 = 0o700;

/// `path` と同一ディレクトリの `.tmp` へ書き込み → fsync → rename で置換する。
/// 途中で落ちても既存ファイルは無傷（tmp が残るだけ）。
///
/// tmp は残骸（前回クラッシュの 0644 等）を消してから `create_new`
/// （O_EXCL — symlink / hard link 残骸を辿らない）+ mode 0600 で作り直し、
/// fd に 0600 を明示設定する（umask に依らず置換後の本体が必ず 0600）。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
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
    std::fs::rename(&tmp, path)
}

/// `path` を（途中の祖先も含め）0700 で作る。既に存在するディレクトリの
/// mode には触らない — 利用者が意図して設定したもの、または Docker の
/// マウントボリュームでありうるため。
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(PRIVATE_DIR_MODE);
    builder.create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_content_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes.json");
        write_atomic(&path, b"{\"v\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"v\":1}");
        assert!(!dir.path().join("nodes.tmp").exists());
    }

    #[test]
    fn replaces_existing_content_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliases.toml");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!dir.path().join("aliases.tmp").exists());
    }

    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes.json");
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn narrows_previously_world_readable_target() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliases.toml");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn ignores_stale_wide_tmp() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes.json");
        let tmp = dir.path().join("nodes.tmp");
        std::fs::write(&tmp, "stale leftover that is longer than the new body").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert_eq!(mode_of(&path), 0o600);
        assert!(!tmp.exists());
    }

    #[test]
    fn create_private_dir_all_makes_0700_tree() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("a").join("mat");
        create_private_dir_all(&store).unwrap();
        assert_eq!(mode_of(&store), 0o700);
        assert_eq!(mode_of(&dir.path().join("a")), 0o700);
    }

    #[test]
    fn create_private_dir_all_leaves_existing_dir_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("mat");
        std::fs::create_dir(&store).unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o750)).unwrap();
        create_private_dir_all(&store).unwrap();
        assert_eq!(mode_of(&store), 0o750);
    }
}
