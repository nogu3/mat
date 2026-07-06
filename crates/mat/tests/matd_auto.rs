//! matd 自動発見の統合テスト。fake matd（tmp の UnixListener）と fake chip-tool で
//! 経路選択（自動 / MAT_MATD=0 / stale socket / 非対応 op）を検証する。実 matd 不要。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn fake_chip_tool() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-chip-tool.sh")
}

/// 自動検出モード（MAT_MATD 未設定）の mat。probe 先は MAT_MATD_SOCKET で tmp に
/// 固定し、開発機で実 matd が動いていても拾わないようにする。
fn mat_auto(store: &Path, socket: &Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_CHIP_TOOL_BIN", fake_chip_tool())
        .env("MAT_MATD_SOCKET", socket)
        .env_remove("MAT_MATD")
        .arg("--store")
        .arg(store);
    c
}

/// 直経路（MAT_MATD=0）の mat。ストア準備用。
fn mat_direct(store: &Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_CHIP_TOOL_BIN", fake_chip_tool())
        .env("MAT_MATD", "0")
        .arg("--store")
        .arg(store);
    c
}

/// fake chip-tool 直経路で node 5 を commission 済みにしたストア。
fn store_with_node5() -> TempDir {
    let store = TempDir::new().unwrap();
    mat_direct(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
            "--node",
            "5",
        ])
        .assert()
        .success();
    store
}

/// fake matd: 1 接続を受け、1 行読んでマーカー入り応答を 1 行返して終了する。
/// join の戻り値は受信したリクエスト行（op の検証用）。
fn spawn_fake_matd(socket: PathBuf) -> JoinHandle<String> {
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut req = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut req)
            .unwrap();
        stream
            .write_all(b"{\"via\":\"fake-matd\",\"value\":true}\n")
            .unwrap();
        req
    })
}

#[test]
fn auto_routes_to_live_matd() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone());

    mat_auto(store.path(), &socket)
        .args([
            "read",
            "--node",
            "5",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // fake matd に read op が届いている（= matd 経路で実行された）。
    let req = matd.join().unwrap();
    assert!(req.contains("\"op\":\"read\""), "request line: {req}");
}

#[test]
fn auto_routes_color_temp_with_converted_mireds() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone());

    mat_auto(store.path(), &socket)
        .args(["color-temp", "--node", "5", "--kelvin", "2700"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // 換算（2700K → 370 mireds）は mat 側で済んだ状態で matd に届く。
    let req = matd.join().unwrap();
    assert!(req.contains("\"op\":\"color_temp\""), "request line: {req}");
    assert!(req.contains("\"mireds\":370"), "request line: {req}");
    assert!(req.contains("\"kelvin\":2700"), "request line: {req}");
}

#[test]
fn auto_routes_color_with_converted_values() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone());

    mat_auto(store.path(), &socket)
        .args(["color", "--node", "5", "--hue", "330", "--sat", "80"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // 換算（330° → 233、80% → 203）は mat 側で済んだ状態で matd に届く。
    let req = matd.join().unwrap();
    assert!(req.contains("\"op\":\"color\""), "request line: {req}");
    assert!(req.contains("\"hue_raw\":233"), "request line: {req}");
    assert!(
        req.contains("\"saturation_raw\":203"),
        "request line: {req}"
    );
}

#[test]
fn auto_falls_back_when_socket_missing() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock"); // bind しない = 存在しないパス

    mat_auto(store.path(), &socket)
        .args([
            "read",
            "--node",
            "5",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""))
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn auto_falls_back_on_stale_socket() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // bind 後すぐ drop: ファイルは残るが誰も listen していない（ECONNREFUSED）。
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());

    mat_auto(store.path(), &socket)
        .args([
            "read",
            "--node",
            "5",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""));
}

#[test]
fn mat_matd_zero_forces_direct_even_with_live_matd() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let _listener = UnixListener::bind(&socket).unwrap(); // 生きているが使われないはず

    mat_auto(store.path(), &socket)
        .env("MAT_MATD", "0") // env_remove の後に上書き
        .args([
            "read",
            "--node",
            "5",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"onoff\""))
        .stdout(predicate::str::contains("fake-matd").not());
}

#[test]
fn auto_keeps_unsupported_ops_on_direct_path() {
    let store = TempDir::new().unwrap(); // discover は空ストアで動く
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // 生きた listener。自動モードでも discover は probe されず直経路のはず
    // （probe されると backlog に接続だけ成功し応答待ちでハングしてテストが失敗する）。
    let _listener = UnixListener::bind(&socket).unwrap();

    mat_auto(store.path(), &socket)
        .arg("discover")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"devices\""));
}
