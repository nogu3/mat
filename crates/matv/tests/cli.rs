//! matv の CLI 面のテスト（M1: 単一ノード）。
//!
//! `matv` は起動時に stdout へ JSON 1 行を出し（mat の流儀: stdout=JSON、
//! ログ=stderr）、その後 `Device::run` で待ち受け続ける（Ctrl-C まで戻らない）。
//! そのため `assert_cmd::Command`（完走を待つ）ではなくプレーンな
//! `std::process::Command` を `spawn` し、stdout の 1 行目だけ読んでから
//! プロセスの生存を確認して kill する — `assert_cmd::cargo::CommandCargoExt`
//! は `std::process::Command` にも実装されているので `cargo_bin` はそのまま使える。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use assert_cmd::prelude::*;

/// stdout の 1 行目を読むタイムアウト。CI の遅いマシンでも余裕を持たせる。
const STDOUT_LINE_TIMEOUT: Duration = Duration::from_secs(10);

/// SIGINT を送ってからプロセスが終わるのを待つ上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn prints_setup_payload_and_stays_up() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("matv.toml");
    std::fs::write(
        &cfg,
        format!(
            // 末尾の `[[device]]` は M3 の標準 e2e ブロック（scripts/e2e-* と
            // mat-device の integration テストが使うものと同一）。matv は純
            // bridge なので 1 台以上の宣言が必須。
            "passcode = 20202021\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\nstore = \"{}\"\niface = \"lo\"\n\n[[device]]\nid = \"e2e-light\"\nkind = \"onoff-light\"\nname = \"E2E Light\"\n",
            dir.path().display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("matv")
        .unwrap()
        .arg("--config")
        .arg(&cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn matv");

    // stdout の1行目をタイムアウト付きで読む（別スレッド + mpsc — matv は
    // 読み終えた後も run() でブロックし続けるので read_line 自体は帰ってくる
    // が、プロセスがハングした場合に無限に待たないためのタイムアウト）。
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader.read_line(&mut line).map(|_| line);
        let _ = tx.send(result);
    });

    let line = match rx.recv_timeout(STDOUT_LINE_TIMEOUT) {
        Ok(Ok(line)) => line,
        Ok(Err(e)) => {
            let _ = child.kill();
            panic!("failed to read matv stdout: {e}");
        }
        Err(_) => {
            let _ = child.kill();
            panic!("matv did not print a stdout line within {STDOUT_LINE_TIMEOUT:?}");
        }
    };

    let json: serde_json::Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("first stdout line was not JSON ({e}): {line:?}"));
    let qr = json
        .get("qr_payload")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("missing qr_payload in {json}"));
    assert!(qr.starts_with("MT:"), "qr_payload should be MT:...: {qr}");
    assert!(json.get("manual_code").and_then(|v| v.as_str()).is_some());
    assert!(json.get("port").and_then(|v| v.as_u64()).is_some());
    assert!(json.get("store").and_then(|v| v.as_str()).is_some());

    // プロセスが生存し続けていることを確認（run() は Ctrl-C まで戻らない）。
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().unwrap().is_none(),
        "matv exited before it was killed"
    );

    child.kill().expect("kill matv");
    let _ = child.wait();
}

/// `--stdin-control`（Task 8）: stdin の JSON 1 行 = 刺激 1 件。適用できた
/// ものは stdout に JSON 1 行、適用できなかったものは stderr に mat 形式の
/// error JSON 1 行（フックは読み続ける）。
#[test]
fn stdin_control_applies_a_press_and_reports_unknown_device() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("matv.toml");
    std::fs::write(
        &cfg,
        format!(
            "passcode = 20202021\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\ngroup_port = 0\nstore = \"{}\"\niface = \"lo\"\n\n[[device]]\nid = \"btn\"\nkind = \"switch\"\nname = \"Button\"\n\n[[device]]\nid = \"door\"\nkind = \"contact-sensor\"\nname = \"Door\"\n",
            dir.path().display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("matv")
        .unwrap()
        .arg("--config")
        .arg(&cfg)
        .arg("--stdin-control")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn matv");

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let _ = tx.send(line);
                }
            }
        }
    });
    let (etx, erx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let _ = etx.send(line);
                }
            }
        }
    });

    let first = rx
        .recv_timeout(STDOUT_LINE_TIMEOUT)
        .expect("setup payload line");
    assert!(first.contains("qr_payload"), "first stdout line: {first:?}");

    use std::io::Write;
    writeln!(stdin, r#"{{"device":"btn","press":"short"}}"#).unwrap();
    let applied_line = rx.recv_timeout(STDOUT_LINE_TIMEOUT).expect("applied line");
    let applied: serde_json::Value = serde_json::from_str(applied_line.trim())
        .unwrap_or_else(|e| panic!("applied line was not JSON ({e}): {applied_line:?}"));
    assert_eq!(applied["device"], "btn");
    assert_eq!(applied["applied"], "press");
    // 短押し = InitialPress + ShortRelease の 2 イベント。
    assert_eq!(applied["event_numbers"].as_array().unwrap().len(), 2);

    writeln!(stdin, r#"{{"device":"nope","state":true}}"#).unwrap();
    let err = erx.recv_timeout(STDOUT_LINE_TIMEOUT).expect("error line");
    assert!(err.contains("\"kind\":\"not_found\""), "stderr: {err}");

    child.kill().unwrap();
    let _ = child.wait();
}

/// `--stdin-control` 中でも Ctrl-C で素直に終わること。stdin フックは
/// `tokio::io::stdin()`（blocking プールの専用スレッドで**キャンセル
/// できない** `read()` に入る）で読むので、ランタイムを暗黙 drop に任せると
/// `Runtime::Drop` がそのスレッドの完了を待ってハングする — stdin が開いた
/// ままだと EOF は永遠に来ない。`main` が `shutdown_background` で明示的に
/// 畳むことをここで釘打ちする（stdin は開いたまま SIGINT を送る）。
#[cfg(unix)]
#[test]
fn ctrl_c_exits_even_while_stdin_control_is_reading() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("matv.toml");
    std::fs::write(
        &cfg,
        format!(
            "passcode = 20202021\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\ngroup_port = 0\nstore = \"{}\"\niface = \"lo\"\n\n[[device]]\nid = \"btn\"\nkind = \"switch\"\nname = \"Button\"\n",
            dir.path().display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("matv")
        .unwrap()
        .arg("--config")
        .arg(&cfg)
        .arg("--stdin-control")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn matv");

    // stdin は開けたまま持ち続ける（EOF を送らない = フックのスレッドは
    // read() で止まったまま）。
    let _stdin = child.stdin.take().expect("piped stdin");

    // setup payload を読んでから SIGINT を送る（`tokio::select!` が
    // ctrl_c() を初回ポーリングしてハンドラを登録した後になる）。
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
    });
    let first = rx
        .recv_timeout(STDOUT_LINE_TIMEOUT)
        .expect("setup payload line");
    assert!(first.contains("qr_payload"), "first stdout line: {first:?}");
    std::thread::sleep(Duration::from_millis(200));

    // SAFETY: 自分が spawn した子プロセスへのシグナル送信のみ。
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(rc, 0, "failed to send SIGINT");

    let deadline = std::time::Instant::now() + SHUTDOWN_TIMEOUT;
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("matv did not exit within {SHUTDOWN_TIMEOUT:?} after SIGINT");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(
        status.success(),
        "matv should exit cleanly on ctrl-c, got {status:?}"
    );
}
