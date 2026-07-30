//! Atomic なファイル置換（tmp + fsync + rename）。
//!
//! `std::fs::write` は O_TRUNC 上書きのため、電源断・クラッシュのタイミングで
//! ファイル全体を失う。`mat-controller` の group counter persist と同じ規律を
//! 台帳（nodes.json）と aliases.toml に適用するための共有ヘルパ。

use std::io::{self, Write};
use std::path::Path;

/// `path` と同一ディレクトリの `.tmp` へ書き込み → fsync → rename で置換する。
/// 途中で落ちても既存ファイルは無傷（tmp が残るだけ）。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
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
}
