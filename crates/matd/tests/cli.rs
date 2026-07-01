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
