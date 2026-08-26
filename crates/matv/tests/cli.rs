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
