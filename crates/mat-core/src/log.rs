//! ログ初期化の共有ヘルパ。
//!
//! subscriber の組み立ては各バイナリ（`mat` / `matd`）が行う — `mat-core` に
//! `tracing-subscriber` 依存を持ち込まないため、ここにはフィルタ指定の
//! 選択規則だけを純関数で置く。

/// `MAT_LOG` / `RUST_LOG` からログフィルタ指定を選ぶ（`MAT_LOG` 優先）。
///
/// 空文字・空白のみは **未設定として扱う**。`EnvFilter` としてはディレクティブ
/// 0 個の有効な指定になり、既定 level に落ちずログが全 OFF になるため
/// （`systemctl --user set-environment MAT_LOG=...` で一時 debug を入れて
/// 戻すときに踏みやすい）。
pub fn log_filter_spec(mat_log: Option<&str>, rust_log: Option<&str>) -> Option<String> {
    let pick = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    pick(mat_log).or_else(|| pick(rust_log))
}

/// 環境変数を読んで [`log_filter_spec`] を適用する薄いラッパ。
pub fn log_filter_spec_from_env() -> Option<String> {
    let mat_log = std::env::var("MAT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    log_filter_spec(mat_log.as_deref(), rust_log.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spec_is_treated_as_unset() {
        // 空文字は EnvFilter としては「ディレクティブ 0 個」の有効な指定に
        // なり、ERROR すら出なくなる（実測）。未設定と同じ扱いにする。
        assert_eq!(log_filter_spec(Some(""), None), None);
        assert_eq!(log_filter_spec(Some("   "), None), None);
    }

    #[test]
    fn mat_log_wins_over_rust_log() {
        assert_eq!(
            log_filter_spec(Some("debug"), Some("trace")),
            Some("debug".to_string())
        );
    }

    #[test]
    fn empty_mat_log_falls_back_to_rust_log() {
        assert_eq!(
            log_filter_spec(Some(""), Some("info")),
            Some("info".to_string())
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            log_filter_spec(Some(" info "), None),
            Some("info".to_string())
        );
    }

    #[test]
    fn absent_everywhere_is_none() {
        assert_eq!(log_filter_spec(None, None), None);
        assert_eq!(log_filter_spec(None, Some("  ")), None);
    }
}
