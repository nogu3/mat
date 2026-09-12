//! `mat listen` の統合テスト。fake matd（tmp の UnixListener + ストリーム応答）で
//! count / timeout / exit code / matd 落ちの契約を釘打ちする（spec テスト方針 3）。
//! 実 matd も実デバイスも不要。

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread::JoinHandle;

use predicates::prelude::*;
use tempfile::TempDir;

mod common;
use common::{mat_listen, spawn_fake_matd};

const ACK: &str = "{\"timestamp\":\"2026-07-20T00:00:00+09:00\",\"listening\":true}\n";
const EVENT: &str = "{\"timestamp\":\"2026-07-20T00:00:01+09:00\",\"node_id\":21,\"endpoint\":1,\"cluster\":\"occupancysensing\",\"attribute\":\"occupancy\",\"value\":1,\"priming\":false}\n";

/// ack + イベント N 行 を 1 接続で流す fake matd（`hold_ms` = 送信後の保持時間）。
fn spawn_fake_matd_stream(socket: PathBuf, events: usize, hold_ms: u64) -> JoinHandle<Vec<String>> {
    spawn_fake_matd_sessions(socket, vec![(events, hold_ms)])
}

/// 接続ごとに (イベント数, hold_ms) を順に消費する fake matd。
fn spawn_fake_matd_sessions(
    socket: PathBuf,
    sessions: Vec<(usize, u64)>,
) -> JoinHandle<Vec<String>> {
    let sessions = sessions
        .into_iter()
        .map(|(events, hold_ms)| {
            let mut lines = vec![ACK];
            lines.extend(std::iter::repeat_n(EVENT, events));
            (lines, hold_ms)
        })
        .collect();
    spawn_fake_matd(socket, sessions)
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

    let req = &matd.join().unwrap()[0];
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

    let req = &matd.join().unwrap()[0];
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
fn listen_count_zero_streams_all_events_until_timeout() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // count 0 = 無期限: 3 件全部流れても exit 0 せず、timeout で打ち切り
    // （1 件以上受信済みなので exit 0）。
    let _matd = spawn_fake_matd_stream(socket.clone(), 3, 3000);

    mat_listen(&socket, &["--count", "0", "--timeout-ms", "300"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"occupancy\"").count(3));
}

#[test]
fn listen_count_zero_timeout_zero_streams_until_stream_end() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // count 0 + timeout 0 = 完全無期限: count 到達での exit 0 は発生せず、
    // ストリームが閉じるまで受け続ける（EOF = matd 落ち扱い exit 13）。
    let _matd = spawn_fake_matd_stream(socket.clone(), 3, 0);

    mat_listen(&socket, &["--count", "0", "--timeout-ms", "0"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("\"occupancy\"").count(3));
}

#[test]
fn listen_count_zero_timeout_without_events_exits_3() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // count 0 でも timeout>0 なら従来どおり打ち切り、0 件なら exit 3。
    let _matd = spawn_fake_matd_stream(socket.clone(), 0, 3000);

    mat_listen(&socket, &["--count", "0", "--timeout-ms", "300"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

/// `--attribute` と `--event` の同時指定は clap レベルで拒否される（バックエンド
/// 不到達、exit 2 — `color-temp --kelvin --mireds` 等と同じ規律）。
#[test]
fn listen_attribute_and_event_together_exits_2() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock"); // bind しない（到達しないはず）

    mat_listen(
        &socket,
        &["--attribute", "occupancy", "--event", "initial-press"],
    )
    .assert()
    .code(2);
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

#[test]
fn listen_reconnect_survives_matd_restart_and_accumulates_count() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // 1 件流して切る → 再接続後に 1 件流して保持 → count 2 到達で exit 0。
    let _matd = spawn_fake_matd_sessions(socket.clone(), vec![(1, 0), (1, 3000)]);

    mat_listen(
        &socket,
        &["--reconnect", "--count", "2", "--timeout-ms", "10000"],
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("\"occupancy\"").count(2))
    .stderr(predicate::str::contains("reconnecting"));
}

#[test]
fn listen_reconnect_waits_for_matd_to_appear() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let socket_for_thread = socket.clone();
    // matd は 1.5 秒遅れて現れる。
    let starter = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        spawn_fake_matd_sessions(socket_for_thread, vec![(1, 2000)])
            .join()
            .unwrap();
    });

    mat_listen(
        &socket,
        &["--reconnect", "--count", "1", "--timeout-ms", "10000"],
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("\"occupancy\"").count(1));
    starter.join().unwrap();
}

#[test]
fn listen_reconnect_timeout_without_events_exits_3() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock"); // 誰も bind しない
    mat_listen(&socket, &["--reconnect", "--timeout-ms", "700"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}
