//! `mat listen` の統合テスト。fake matd（tmp の UnixListener + ストリーム応答）で
//! count / timeout / exit code / matd 落ちの契約を釘打ちする（spec テスト方針 3）。
//! 実 matd も実デバイスも不要。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread::JoinHandle;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

const ACK: &str = "{\"timestamp\":\"2026-07-20T00:00:00+09:00\",\"listening\":true}\n";
const EVENT: &str = "{\"timestamp\":\"2026-07-20T00:00:01+09:00\",\"node_id\":21,\"endpoint\":1,\"cluster\":\"occupancysensing\",\"attribute\":\"occupancy\",\"value\":1,\"priming\":false}\n";

fn mat_listen(socket: &std::path::Path, extra: &[&str]) -> Command {
    let store = TempDir::new().unwrap();
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_IFACE", "lo")
        .env("MAT_MATD_SOCKET", socket)
        .env_remove("MAT_MATD")
        .arg("--store")
        .arg(store.keep()) // TempDir は listen 終了まで生かすため keep でリーク
        .arg("listen")
        .args(extra);
    c
}

/// fake matd: listen リクエスト 1 行を読み、ack + イベント N 行を返す。
/// `hold` = 送信後も接続を開いたまま維持する秒数（timeout 系テスト用）。
fn spawn_fake_matd_stream(socket: PathBuf, events: usize, hold_ms: u64) -> JoinHandle<String> {
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut req = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut req)
            .unwrap();
        stream.write_all(ACK.as_bytes()).unwrap();
        for _ in 0..events {
            stream.write_all(EVENT.as_bytes()).unwrap();
        }
        stream.flush().unwrap();
        if hold_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
        req
    })
}

#[test]
fn listen_count_reached_exits_zero_with_events_on_stdout() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd_stream(socket.clone(), 2, 500);

    mat_listen(&socket, &["--count", "2", "--timeout-ms", "5000"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"occupancy\"").count(2));

    let req = matd.join().unwrap();
    assert!(req.contains("\"op\":\"listen\""), "request line: {req}");
}

#[test]
fn listen_filters_are_forwarded_in_request() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd_stream(socket.clone(), 1, 200);

    mat_listen(
        &socket,
        &[
            "--node",
            "21",
            "--cluster",
            "occupancysensing",
            "--count",
            "1",
        ],
    )
    .assert()
    .success();

    let req = matd.join().unwrap();
    assert!(req.contains("\"node_id\":21"), "request line: {req}");
    assert!(
        req.contains("\"cluster\":\"occupancysensing\""),
        "request line: {req}"
    );
}

#[test]
fn listen_timeout_without_events_exits_3() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // ack のみ・イベント 0・接続は維持 → mat 側の timeout で打ち切り。
    let _matd = spawn_fake_matd_stream(socket.clone(), 0, 3000);

    mat_listen(&socket, &["--timeout-ms", "300"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

#[test]
fn listen_timeout_with_partial_events_exits_zero() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // 1 件だけ流して沈黙 → count=2 未達のまま timeout → 1 件以上なので exit 0。
    let _matd = spawn_fake_matd_stream(socket.clone(), 1, 3000);

    mat_listen(&socket, &["--count", "2", "--timeout-ms", "300"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"occupancy\"").count(1));
}

#[test]
fn listen_without_matd_exits_13() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock"); // bind しない

    mat_listen(&socket, &[])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("matd_unavailable"));
}

#[test]
fn listen_stream_cut_by_matd_death_exits_13_keeping_output() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // 1 件流して即クローズ（hold 0）→ count=2 未達で EOF → exit 13、出力済みは残る。
    let _matd = spawn_fake_matd_stream(socket.clone(), 1, 0);

    mat_listen(&socket, &["--count", "2", "--timeout-ms", "5000"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("\"occupancy\"").count(1))
        .stderr(predicate::str::contains("matd_unavailable"));
}

#[test]
fn listen_with_mat_matd_disabled_exits_13() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let _listener = UnixListener::bind(&socket).unwrap(); // 居ても使わない

    mat_listen(&socket, &[])
        .env("MAT_MATD", "0")
        .assert()
        .code(13)
        .stderr(predicate::str::contains("matd_unavailable"));
}
