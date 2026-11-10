//! matd の socket / lock ファイルの権限。socket の権限が唯一のアクセス制御
//! （接続できれば write / invoke / shutdown / reload まで全 op が通る）なので、
//! umask に依らず 0600 であることを釘打ちする。
//!
//! umask はプロセス全体の状態なので、他のテストと混ざらないよう専用の
//! テストバイナリに分け、テストも 1 本にしている。

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use mat_core::error::MatError;
use matd::server::NativeState;

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn socket_and_lock_file_are_0600_even_with_permissive_umask() {
    // 最も緩い umask（0）でも 0600 になること。
    rustix::process::umask(rustix::fs::Mode::empty());

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("matd.sock");

    // lock ファイル（<socket>.lock）。
    let _lock = matd::lock::acquire(&socket).unwrap();
    assert_eq!(mode(&matd::lock::lock_path(&socket)), 0o600);

    // serve が bind した socket。ping が通る = accept ループに入った後
    // （chmod は bind 直後・accept 前に済んでいる）なので、その時点の mode を見る。
    let store = tempfile::tempdir().unwrap();
    let (tx, _rx) = tokio::sync::broadcast::channel(4);
    let daemon = std::sync::Arc::new(matd::server::DaemonInfo {
        version: "test",
        started: std::time::Instant::now(),
        iface: "lo".into(),
        fabric_index: 1,
        reloads: matd::server::ReloadStats::default(),
    });
    let sock2 = socket.clone();
    let handle = tokio::spawn(async move {
        let _ = matd::server::serve(
            &sock2,
            store.path().to_path_buf(),
            std::sync::Arc::new(NativeState::Unavailable(MatError::store_missing("test"))),
            tx,
            std::sync::Arc::new(matd::subscription::SubHealth::new(None)),
            daemon,
        )
        .await;
    });

    let mut stream = None;
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(&socket).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stream = stream.expect("could not connect to matd socket");
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(b"{\"op\":\"ping\"}\n").await.unwrap();
    let resp = BufReader::new(read_half)
        .lines()
        .next_line()
        .await
        .unwrap()
        .unwrap();
    assert!(resp.contains("pong"), "got: {resp}");

    assert_eq!(mode(&socket), 0o600);

    handle.abort();
}
