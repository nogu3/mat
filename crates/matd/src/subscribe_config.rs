//! `<store>/subscriptions.toml` — matd 常駐購読のクラスタ絞り込み設定
//! （属性 = `clusters`）と、イベント購読の範囲設定（`events`）。
//!
//! 無し = full wildcard（挙動不変、aliases.toml と同じ absent-file 規律）。
//! 壊れ・未知クラスタ名・空の `clusters` は `store_parse` — matd は起動を拒否
//! する（黙って wildcard に落ちると弱リンク対策が無効化されたことに気づけない
//! ため、silent fallback はしない）。`mat`（one-shot）はこのファイルを読まない。
//!
//! `events` は `clusters`（属性の絞り込み）とは独立："events" キー無し =
//! `EventScope::Wildcard`（全クラスタのイベントを urgent 購読）、
//! `events = []` は Phase A 以前の配線（EventRequests 無し）に戻す
//! `EventScope::Off`、`events = ["switch", ...]` は列挙したクラスタのみの
//! `EventScope::Clusters`。`clusters` キーが無くても `events` だけの config
//! を受理する — 空リストエラーは `clusters` キーが**存在してかつ空**のとき
//! だけ発生する。

use std::path::Path;

use mat_controller::im::EventPathIn;
use mat_core::error::{ErrorKind, MatError};

pub const SUBSCRIPTIONS_FILE: &str = "subscriptions.toml";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubscriptions {
    clusters: Option<Vec<String>>,
    events: Option<Vec<String>>,
}

/// `subscriptions.toml` から読み込んだ設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeConfig {
    /// 属性の絞り込み（`clusters` キー）。`None` = full wildcard（キー無し）。
    pub clusters: Option<Vec<u32>>,
    /// イベント購読の範囲（`events` キー）。
    pub events: EventScope,
}

/// イベント購読の範囲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScope {
    /// `events` キー無し — 全クラスタのイベントを urgent 購読する。
    Wildcard,
    /// `events = [...]` — 列挙したクラスタのイベントのみ urgent 購読する。
    Clusters(Vec<u32>),
    /// `events = []` — イベント購読を張らない（Phase A 以前の配線）。
    Off,
}

impl EventScope {
    /// この範囲に対応する `EventPathIn` の並び（spec §6.3: 常に `urgent: true`）。
    pub fn to_paths(&self) -> Vec<EventPathIn> {
        match self {
            EventScope::Wildcard => vec![EventPathIn::WILDCARD_URGENT],
            EventScope::Clusters(ids) => ids
                .iter()
                .map(|&cluster| EventPathIn {
                    endpoint: None,
                    cluster: Some(cluster),
                    event: None,
                    urgent: true,
                })
                .collect(),
            EventScope::Off => Vec::new(),
        }
    }
}

/// クラスタ名（chip-tool 記法）または数値文字列（`"0x0006"` / `"6"`）の並びを
/// クラスタ ID に解決する。`clusters` / `events` 共通の解決ロジック。
/// 重複は除去（順序は初出順を保持）。未知名は `store_parse`。
fn resolve_cluster_names(names: &[String]) -> Result<Vec<u32>, MatError> {
    let mut ids: Vec<u32> = Vec::new();
    for name in names {
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
    Ok(ids)
}

/// subscriptions.toml を読む。無ければ `Ok(None)`（= full wildcard、`events`
/// も含めて挙動不変）。
pub fn load(store_root: &Path) -> Result<Option<SubscribeConfig>, MatError> {
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

    let clusters = match raw.clusters {
        None => None,
        Some(names) if names.is_empty() => {
            return Err(MatError::new(
                ErrorKind::StoreParse,
                "subscriptions.toml: clusters must not be empty (delete the file for full wildcard)",
            ));
        }
        Some(names) => Some(resolve_cluster_names(&names)?),
    };

    let events = match raw.events {
        None => EventScope::Wildcard,
        Some(names) if names.is_empty() => EventScope::Off,
        Some(names) => EventScope::Clusters(resolve_cluster_names(&names)?),
    };

    Ok(Some(SubscribeConfig { clusters, events }))
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
        let cfg = load(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.clusters, Some(vec![0x0406, 0x0006, 0x0402]));
        assert_eq!(cfg.events, EventScope::Wildcard);
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
    fn empty_clusters_list_is_store_parse() {
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

    #[test]
    fn events_key_absent_is_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"clusters = ["onoff"]"#);
        let cfg = load(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.events, EventScope::Wildcard);
    }

    #[test]
    fn events_names_and_numerics_resolve_to_clusters() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"events = ["switch", "0x45"]"#);
        let cfg = load(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.events, EventScope::Clusters(vec![0x3B, 0x45]));
        // clusters キー無し → 属性の絞り込みは wildcard のまま。
        assert_eq!(cfg.clusters, None);
    }

    #[test]
    fn empty_events_list_is_off() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "events = []");
        let cfg = load(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.events, EventScope::Off);
        assert_eq!(cfg.clusters, None);
    }

    #[test]
    fn unknown_event_cluster_name_is_store_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"events = ["nosuchcluster"]"#);
        let e = load(dir.path()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::StoreParse);
        assert!(e.detail.contains("nosuchcluster"));
    }

    #[test]
    fn clusters_and_events_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"
clusters = ["onoff"]
events = ["switch", "booleanstate"]
"#,
        );
        let cfg = load(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.clusters, Some(vec![0x0006]));
        assert_eq!(cfg.events, EventScope::Clusters(vec![0x3B, 0x45]));
    }

    #[test]
    fn to_paths_wildcard_is_single_urgent_wildcard_path() {
        assert_eq!(
            EventScope::Wildcard.to_paths(),
            vec![EventPathIn::WILDCARD_URGENT]
        );
    }

    #[test]
    fn to_paths_clusters_is_one_urgent_path_per_cluster() {
        assert_eq!(
            EventScope::Clusters(vec![0x3B, 0x45]).to_paths(),
            vec![
                EventPathIn {
                    endpoint: None,
                    cluster: Some(0x3B),
                    event: None,
                    urgent: true,
                },
                EventPathIn {
                    endpoint: None,
                    cluster: Some(0x45),
                    event: None,
                    urgent: true,
                },
            ]
        );
    }

    #[test]
    fn to_paths_off_is_empty() {
        assert_eq!(EventScope::Off.to_paths(), Vec::<EventPathIn>::new());
    }
}
