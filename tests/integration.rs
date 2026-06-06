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
fn discover_with_missing_store_bootstraps_and_succeeds() {
    // discover は認証情報不要（commissionable 探索のみ）。store 無しでも
    // 空ストアを bootstrap して成功し、commissionable を返す。
    let store = TempDir::new().unwrap();
    let missing = store.path().join("does-not-exist");
    mat(&missing)
        .arg("discover")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"commissionable\""));
    // 空ストアが作られている。
    assert!(missing.is_dir());
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

/// node 5 を commission 済みにしたストアを用意する（Phase 1 操作系の前提）。
fn store_with_node5() -> TempDir {
    let store = TempDir::new().unwrap();
    mat(store.path())
        .args(["commission", "192.0.2.10", "MT:FAKE", "--node-id", "5"])
        .assert()
        .success();
    store
}

#[test]
fn read_parses_value() {
    let store = store_with_node5();
    mat(store.path())
        .args(["read", "5", "1", "onoff", "on-off"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""))
        .stdout(predicate::str::contains("\"attribute\":\"on-off\""))
        .stdout(predicate::str::contains("\"value\":true"))
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn write_reports_success() {
    let store = store_with_node5();
    mat(store.path())
        .args(["write", "5", "1", "levelcontrol", "on-level", "128"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"success\""))
        .stdout(predicate::str::contains("\"value\":\"128\""));
}

#[test]
fn invoke_reports_success() {
    let store = store_with_node5();
    mat(store.path())
        .args(["invoke", "5", "1", "onoff", "toggle"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\":\"toggle\""))
        .stdout(predicate::str::contains("\"status\":\"success\""));
}

#[test]
fn on_maps_to_onoff_invoke() {
    let store = store_with_node5();
    mat(store.path())
        .args(["on", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""))
        .stdout(predicate::str::contains("\"command\":\"on\""))
        .stdout(predicate::str::contains("\"status\":\"success\""));
}

#[test]
fn off_maps_to_onoff_invoke() {
    let store = store_with_node5();
    mat(store.path())
        .args(["off", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\":\"off\""));
}

#[test]
fn describe_lists_endpoints_and_clusters() {
    let store = store_with_node5();
    mat(store.path())
        .args(["describe", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"endpoints\""))
        // ep0 の server-list（29,31）と ep1（6,8）。
        .stdout(predicate::str::contains("\"endpoint\":0"))
        .stdout(predicate::str::contains("\"endpoint\":1"))
        .stdout(predicate::str::contains("29"))
        .stdout(predicate::str::contains("\"clusters\":[6,8]"));
}

#[test]
fn open_window_returns_codes() {
    let store = store_with_node5();
    mat(store.path())
        .args(["open-window", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"node_id\":5"))
        .stdout(predicate::str::contains("\"manual_code\":\"36217551492\""))
        .stdout(predicate::str::contains(
            "\"qr_payload\":\"MT:-24J0AFN00KA0648G00\"",
        ))
        .stdout(predicate::str::contains("\"expires_at\""))
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn open_window_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["open-window", "99"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn open_window_timeout_exits_3() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args(["open-window", "5"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

#[test]
fn read_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["read", "99", "1", "onoff", "on-off"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn read_missing_store_exits_10() {
    let store = TempDir::new().unwrap();
    let missing = store.path().join("nope");
    mat(&missing)
        .args(["read", "5", "1", "onoff", "on-off"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("store_missing"));
}

#[test]
fn read_timeout_exits_3() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args(["read", "5", "1", "onoff", "on-off"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

#[test]
fn invoke_reject_exits_4() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "reject")
        .args(["invoke", "5", "1", "onoff", "on"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("device_rejected"));
}
