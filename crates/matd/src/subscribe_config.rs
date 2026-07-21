//! `<store>/subscriptions.toml` — matd 常駐購読のクラスタ絞り込み設定。
//!
//! 無し = full wildcard（挙動不変、aliases.toml と同じ absent-file 規律）。
//! 壊れ・未知クラスタ名・空リストは `store_parse` — matd は起動を拒否する
//! （黙って wildcard に落ちると弱リンク対策が無効化されたことに気づけない
//! ため、silent fallback はしない）。`mat`（one-shot）はこのファイルを読まない。

use std::path::Path;

use mat_core::error::{ErrorKind, MatError};

pub const SUBSCRIPTIONS_FILE: &str = "subscriptions.toml";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubscriptions {
    clusters: Vec<String>,
}

/// subscriptions.toml を読む。無ければ `Ok(None)`（= full wildcard）。
/// クラスタ名は chip-tool 記法（`mat-core::ids`）、数値文字列（`"0x0006"` /
/// `"6"`）も可（ids に無いクラスタの escape hatch — generic read と同じ規律）。
/// 重複は除去（順序は初出順を保持）。
pub fn load(store_root: &Path) -> Result<Option<Vec<u32>>, MatError> {
    let path = store_root.join(SUBSCRIPTIONS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(MatError::new(
                ErrorKind::StoreParse,
                format!("subscriptions.toml unreadable: {e}"),
            ));
        }
    };
    let raw: RawSubscriptions = toml::from_str(&text)
        .map_err(|e| MatError::new(ErrorKind::StoreParse, format!("subscriptions.toml: {e}")))?;
    if raw.clusters.is_empty() {
        return Err(MatError::new(
            ErrorKind::StoreParse,
            "subscriptions.toml: clusters must not be empty (delete the file for full wildcard)",
        ));
    }
    let mut ids: Vec<u32> = Vec::new();
    for name in &raw.clusters {
        let id = mat_core::ids::resolve_cluster(name).ok_or_else(|| {
            MatError::new(
                ErrorKind::StoreParse,
                format!("subscriptions.toml: unknown cluster '{name}'"),
            )
        })?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(Some(ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, body: &str) {
        std::fs::write(dir.join(SUBSCRIPTIONS_FILE), body).unwrap();
    }

    #[test]
    fn absent_file_means_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
    }

    #[test]
    fn resolves_names_and_numerics_dedup_in_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"clusters = ["occupancysensing", "onoff", "0x0402", "6"]"#,
        );
        // "6" = 0x0006 = onoff の重複 → 除去。初出順を保持。
        assert_eq!(
            load(dir.path()).unwrap(),
            Some(vec![0x0406, 0x0006, 0x0402])
        );
    }

    #[test]
    fn unknown_cluster_name_is_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"clusters = ["nosuchcluster"]"#);
        let e = load(dir.path()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::StoreParse);
        assert!(e.detail.contains("nosuchcluster"));
    }

    #[test]
    fn empty_list_is_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "clusters = []");
        assert_eq!(load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
    }

    #[test]
    fn broken_toml_and_unknown_key_are_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "clusters = [broken");
        assert_eq!(load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
        write(dir.path(), "clusterz = [\"onoff\"]");
        assert_eq!(load(dir.path()).unwrap_err().kind, ErrorKind::StoreParse);
    }
}
