//! ログ初期化の共有ヘルパ。
//!
//! フィルタ指定の選択規則は純関数（`log_filter_candidates`）、subscriber の
//! 組み立ては `init_stderr`（`mat` / `matd` 共有。2026-09-12 の監査で
//! 3 バイナリの逐語コピーを一本化 — `tracing-subscriber` 依存はここに集約、
//! feature `log-init` の下でのみビルドされる。ライブラリ消費者は既定で
//! 非依存）。SIGPIPE の既定化 [`reset_sigpipe`] も同じ理由でここに置く。

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

/// 診断ログを stderr に出す subscriber を初期化する。レベルは `MAT_LOG`
/// （無ければ `RUST_LOG`）で制御、どちらも無い / パース不能なら
/// `default_filter`（`mat` は `"warn"`、`matd` は `"info"`）。空文字は未設定
/// 扱い、パースできない指定は次の候補へ送る（[`log_filter_candidates`]）。
/// `ansi` は呼び手が決める（`mat` は tty かつ `NO_COLOR` 未設定のときだけ、
/// `matd` は journald に ANSI を書かないよう常に false）。stdout は JSON 専用
/// なので絶対に汚さない。プロセスで 1 回だけ呼ぶこと（2 回目は panic —
/// `tracing_subscriber::fmt().init()` の性質）。
///
/// feature `log-init` が必要（バイナリ側で有効化。ライブラリ消費者には
/// `tracing-subscriber` 依存を強いない）。
#[cfg(feature = "log-init")]
pub fn init_stderr(default_filter: &str, ansi: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = log_filter_candidates_from_env()
        .into_iter()
        .find_map(|s| EnvFilter::try_new(&s).ok())
        .unwrap_or_else(|| EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .with_writer(std::io::stderr)
        .init();
}

/// SIGPIPE を既定動作（プロセス終了）に戻す。Rust の runtime は SIGPIPE を
/// 無視して起動するので、`mat ... | head -1` / `matd status | jq` のように
/// stdout のパイプ先が先に閉じると `println!` が EPIPE で panic し、stderr に
/// "failed printing to stdout: Broken pipe" を吐いて exit 101 になる。通常の
/// CLI と同じく黙って SIGPIPE で終わらせる（stderr のエラー JSON には影響
/// しない — パイプ先が閉じているのは stdout だけ）。unix 以外は no-op。
/// プロセス起動直後・スレッド生成前に 1 回呼ぶ。
pub fn reset_sigpipe() {
    #[cfg(unix)]
    // SAFETY: SIG_DFL の設定はプロセス起動直後・スレッド生成前の 1 回だけで、
    // 副作用は「EPIPE の代わりに SIGPIPE で終了する」に限られる。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
