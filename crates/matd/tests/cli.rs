//! matd の CLI 面のテスト（chip-tool 不要な経路のみ）。

use assert_cmd::Command;
use predicates::prelude::*;

/// stop 先の matd が居なければ「not running」エラーで exit 1。chip-tool は不要。
#[test]
fn stop_without_running_daemon_errors() {
    let sock = std::env::temp_dir().join(format!("matd-cli-nostop-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);

    Command::cargo_bin("matd")
        .unwrap()
        .args(["stop", "--socket", sock.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not running"));
}

/// status 先の matd が居なければ「not running」エラーで exit 1。
#[test]
fn status_without_running_daemon_errors() {
    let sock = std::env::temp_dir().join(format!("matd-cli-nostatus-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);

    Command::cargo_bin("matd")
        .unwrap()
        .args(["status", "--socket", sock.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not running"));
}

/// `XDG_RUNTIME_DIR` が無く `--socket` も無いときは既定 socket を持たない
/// （共有 /tmp への fallback は廃止）。serve も admin op も、`--socket` か
/// `XDG_RUNTIME_DIR` を求める構造化エラー（kind=other, exit 1）で止まる。
#[test]
fn no_default_socket_without_xdg_runtime_dir() {
    for args in [&[][..], &["stop"][..], &["status"][..], &["reload"][..]] {
        Command::cargo_bin("matd")
            .unwrap()
            .env_remove("XDG_RUNTIME_DIR")
            .args(args)
            .timeout(std::time::Duration::from_secs(20))
            .assert()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("\"kind\":\"other\""))
            .stderr(predicate::str::contains("--socket"))
            .stderr(predicate::str::contains("XDG_RUNTIME_DIR"));
    }
}
