//! ダミー `chip-tool` を使った統合テスト。実 chip-tool 不要・CI で回る。
//!
//! 各テストは `--store` に tempdir を渡してストアを隔離し、`MAT_CHIP_TOOL_BIN`
//! で `tests/fixtures/fake-chip-tool.sh` を指す。

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn fake_chip_tool() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-chip-tool.sh")
}

/// fake chip-tool を使う `mat` コマンド。store は与えられた dir。
fn mat(store: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_CHIP_TOOL_BIN", fake_chip_tool())
        .arg("--store")
        .arg(store);
    c
}

#[test]
fn discover_lists_commissionable_devices() {
    let store = TempDir::new().unwrap(); // 存在する空ストア
    mat(store.path())
        .arg("discover")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"devices\""))
        .stdout(predicate::str::contains("192.0.2.10"))
        .stdout(predicate::str::contains("\"commissionable\""))
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn discover_with_missing_store_exits_10() {
    let store = TempDir::new().unwrap();
    let missing = store.path().join("does-not-exist");
    mat(&missing)
        .arg("discover")
        .assert()
        .code(10)
        .stderr(predicate::str::contains("store_missing"));
}

#[test]
fn discover_with_missing_chip_tool_exits_12() {
    let store = TempDir::new().unwrap();
    Command::cargo_bin("mat")
        .unwrap()
        .env("MAT_CHIP_TOOL_BIN", "/nonexistent/chip-tool-binary")
        .arg("--store")
        .arg(store.path())
        .arg("discover")
        .assert()
        .code(12)
        .stderr(predicate::str::contains("child_not_found"));
}

#[test]
fn commission_success_updates_store_and_shows_in_discover() {
    let store = TempDir::new().unwrap();

    // commission（ストアは自動 bootstrap される）。
    mat(store.path())
        .args([
            "commission",
            "192.0.2.10",
            "MT:FAKE-SETUP-CODE",
            "--node-id",
            "5",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"node_id\":5"))
        .stdout(predicate::str::contains("\"status\":\"success\""));

    // 台帳に乗ったので discover が commissioned として返す。
    mat(store.path())
        .arg("discover")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"commissioned\""))
        .stdout(predicate::str::contains("\"node_id\":5"));
}

#[test]
fn commission_timeout_exits_3() {
    let store = TempDir::new().unwrap();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args(["commission", "192.0.2.10", "MT:FAKE"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

#[test]
fn commission_reject_exits_4() {
    let store = TempDir::new().unwrap();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "reject")
        .args(["commission", "192.0.2.10", "MT:FAKE"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("device_rejected"));
}

#[test]
fn commission_auto_assigns_node_id() {
    let store = TempDir::new().unwrap();
    // node-id 指定なし → 空台帳なので 1 が振られる。
    mat(store.path())
        .args(["commission", "192.0.2.10", "MT:FAKE"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"node_id\":1"));
}
