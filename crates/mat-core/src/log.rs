//! ログ初期化の共有ヘルパ。
//!
//! subscriber の組み立ては各バイナリ（`mat` / `matd`）が行う — `mat-core` に
//! `tracing-subscriber` 依存を持ち込まないため、ここにはフィルタ指定の
//! 選択規則だけを純関数で置く。

/// ログフィルタ指定の候補を `MAT_LOG` → `RUST_LOG` の優先順で返す。
///
/// 空文字・空白のみは **未設定として扱う**。`EnvFilter` としてはディレクティブ
/// 0 個の有効な指定になり、既定 level に落ちずログが全 OFF になるため
/// （`systemctl --user set-environment MAT_LOG=...` で一時 debug を入れて
/// 戻すときに踏みやすい）。
///
/// 1 つに絞らず順序付きで返すのは、**パースできない指定を次の候補へ送る**ため。
/// 旧実装の `try_from_env("MAT_LOG").or_else(|_| try_from_default_env())` は
/// 「`MAT_LOG` が不正なら `RUST_LOG` を使う」挙動を持っていた（`try_from_env` は
/// 未設定でも不正でも `Err`）。呼び出し側が順に `try_new` することでそれを保つ。
pub fn log_filter_candidates(mat_log: Option<&str>, rust_log: Option<&str>) -> Vec<String> {
    [mat_log, rust_log]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// 環境変数を読んで [`log_filter_candidates`] を適用する薄いラッパ。
pub fn log_filter_candidates_from_env() -> Vec<String> {
    let mat_log = std::env::var("MAT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    log_filter_candidates(mat_log.as_deref(), rust_log.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spec_is_treated_as_unset() {
        // 空文字は EnvFilter としては「ディレクティブ 0 個」の有効な指定に
        // なり、ERROR すら出なくなる（実測）。未設定と同じ扱いにする。
        assert!(log_filter_candidates(Some(""), None).is_empty());
        assert!(log_filter_candidates(Some("   "), None).is_empty());
    }

    #[test]
    fn mat_log_comes_before_rust_log() {
        assert_eq!(
            log_filter_candidates(Some("debug"), Some("trace")),
            vec!["debug".to_string(), "trace".to_string()]
        );
    }

    #[test]
    fn empty_mat_log_falls_back_to_rust_log() {
        assert_eq!(
            log_filter_candidates(Some(""), Some("info")),
            vec!["info".to_string()]
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            log_filter_candidates(Some(" info "), None),
            vec!["info".to_string()]
        );
    }

    #[test]
    fn absent_everywhere_is_empty() {
        assert!(log_filter_candidates(None, None).is_empty());
        assert!(log_filter_candidates(None, Some("  ")).is_empty());
    }
}
