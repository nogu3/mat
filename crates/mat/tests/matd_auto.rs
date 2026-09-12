//! matd 自動発見の統合テスト。fake matd（tmp の UnixListener）で経路選択
//! （自動 / MAT_MATD=0 / stale socket / 非対応 op）を検証する。実 matd も
//! chip-tool も不要（M8c-3 Task3: chip-tool ダミー実行体への依存を撤去）。
//!
//! matd 経由で完結する経路（`auto_routes_*`）は `matd_client::dispatch_auto`
//! が op 交換直後に応答を返すため、そもそも native/chip-tool 直経路に
//! 到達しない。直経路へフォールバックする経路（`auto_falls_back_*` /
//! `mat_matd_zero_forces_direct_even_with_live_matd`）は、未 commission な
//! node（99）への `read` を使い、「直経路に落ちて `Store::require_node` まで
//! 到達した」ことを exit 11 で確認する（backend 成功までは要らない —
//! backend 成功系の JSON は `native_direct.rs` のユニットテスト側の責務）。

use std::os::unix::net::UnixListener;

use predicates::prelude::*;
use tempfile::TempDir;

mod common;
use common::{mat_auto, spawn_fake_matd, store_with_node5};

/// fake matd の応答本文（`spawn_fake_matd` の 1 セッション分の行）。
const FAKE_RESP: &str = "{\"via\":\"fake-matd\",\"value\":true}\n";

#[test]
fn auto_routes_to_live_matd() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone(), vec![(vec![FAKE_RESP], 0)]);

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

    // fake matd に read op が届いている（= matd 経由で実行された。native/
    // chip-tool 直経路には触れていない）。
    let req = &matd.join().unwrap()[0];
    assert!(req.contains("\"op\":\"read\""), "request line: {req}");
}

#[test]
fn auto_routes_color_temp_with_converted_mireds() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone(), vec![(vec![FAKE_RESP], 0)]);

    mat_auto(store.path(), &socket)
        .args(["color-temp", "--node", "5", "--kelvin", "2700"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // 換算（2700K → 370 mireds）は mat 側で済んだ状態で matd に届く。
    let req = &matd.join().unwrap()[0];
    assert!(req.contains("\"op\":\"color_temp\""), "request line: {req}");
    assert!(req.contains("\"mireds\":370"), "request line: {req}");
    assert!(req.contains("\"kelvin\":2700"), "request line: {req}");
}

#[test]
fn auto_routes_level_with_converted_raw_value() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone(), vec![(vec![FAKE_RESP], 0)]);

    mat_auto(store.path(), &socket)
        .args(["level", "--node", "5", "--percent", "50"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // 換算（50% → 127）は mat 側で済んだ状態で matd に届く。
    let req = &matd.join().unwrap()[0];
    assert!(req.contains("\"op\":\"level\""), "request line: {req}");
    assert!(req.contains("\"level\":127"), "request line: {req}");
    assert!(req.contains("\"percent\":50"), "request line: {req}");
}

#[test]
fn auto_routes_color_with_converted_values() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone(), vec![(vec![FAKE_RESP], 0)]);

    mat_auto(store.path(), &socket)
        .args(["color", "--node", "5", "--hue", "330", "--sat", "80"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // 換算（330° → 233、80% → 203）は mat 側で済んだ状態で matd に届く。
    let req = &matd.join().unwrap()[0];
    assert!(req.contains("\"op\":\"color\""), "request line: {req}");
    assert!(req.contains("\"hue_raw\":233"), "request line: {req}");
    assert!(
        req.contains("\"saturation_raw\":203"),
        "request line: {req}"
    );
}

#[test]
fn auto_falls_back_when_socket_missing() {
    // socket を bind しない = 接続失敗 → 直経路へフォールバック。node 99 は
    // 台帳に無いので、直経路（native 直 or 旧 chip-tool 経路のどちらでも
    // 共通）の require_node で exit 11 になる — chip-tool 成功応答は要らない。
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");

    mat_auto(store.path(), &socket)
        .args([
            "read",
            "--node",
            "99",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
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
            "99",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
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
            "99",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"))
        .stdout(predicate::str::contains("fake-matd").not());
}

#[test]
fn auto_keeps_unsupported_ops_on_direct_path() {
    // open-window は matd 非対応 op（discover / commission / open-window /
    // diag と同じ扱い、`crates/mat/src/matd_client.rs` の `to_op()` 参照）。
    // 生きた listener を用意しても probe されないはず（probe されると backlog
    // に接続だけ成功し応答待ちでハングしてテストが失敗する）。node 99 は
    // 台帳に無いので、直経路に落ちたことは require_node の exit 11 で確認する
    // （バックエンド成功可否には依存しない — 環境の mDNS/multicast 可否で
    // 結果が揺れないようにするため、discover ではなくこちらを使う）。
    let store = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let _listener = UnixListener::bind(&socket).unwrap();

    mat_auto(store.path(), &socket)
        .args(["open-window", "--node", "99"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}
