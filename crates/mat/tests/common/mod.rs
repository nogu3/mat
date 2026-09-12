//! `mat` の統合テスト間で重複していたテスト用スキャフォールディング。
//! 各テストバイナリ（integration / matd_auto / listen）はこのうちの一部だけ
//! 使うので、未使用分の `dead_code` 警告は抑止する。
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use assert_cmd::Command;
use tempfile::TempDir;

pub const NODE5_LEDGER: &str = r#"{"version":1,"nodes":{"5":{"node_id":5,"address":"192.0.2.10","commissioned_at":"2026-01-01T00:00:00+09:00"}}}"#;

/// node 5 が commission 済みのストアを直接構築する（chip-tool を経由しない —
/// `mat_core::store::Store` の `nodes.json` スキーマに直接書く）。
pub fn store_with_node5() -> TempDir {
    let store = TempDir::new().unwrap();
    std::fs::write(store.path().join("nodes.json"), NODE5_LEDGER).unwrap();
    store
}

/// テスト用の `mat` コマンド。store は与えられた dir。
///
/// `MAT_IFACE=lo` を固定する: Task4（native 既定化）で `MAT_IFACE` 未設定は
/// 自動検出に切り替わるため、先にこのテストスイート側を「明示 iface 指定」の
/// 形に揃えておく（`lo` は実在するが KVS 資材が無いので native 経路は必ず
/// warn + フォールスルーし、store/require_node チェックはこれまでどおり
/// コマンド層 or native_direct::run() の同一ロジックで exit 10/11 を出す
/// （`run` は `Store::open` + `require_node` を engine 構築より前に行う）
/// — 詳細は native_direct.rs の `run()` の doc コメント参照）。
/// `MAT_MATD=0` で直経路に固定する（matd 自動検出が既定のため、開発機で実
/// matd が動いていても拾わない）。
pub fn mat(store: &Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_IFACE", "lo")
        .env("MAT_MATD", "0")
        .arg("--store")
        .arg(store);
    c
}

/// 自動検出モード（MAT_MATD 未設定）の mat。probe 先は MAT_MATD_SOCKET で tmp に
/// 固定し、開発機で実 matd が動いていても拾わないようにする。`MAT_IFACE=lo` は
/// `mat()` と同じ理由（Task4 の native既定化に向けた決定性の固定）。
pub fn mat_auto(store: &Path, socket: &Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_IFACE", "lo")
        .env("MAT_MATD_SOCKET", socket)
        .env_remove("MAT_MATD")
        .arg("--store")
        .arg(store);
    c
}

/// `mat listen` テスト用のコマンド。store は使い捨ての TempDir を用意する
/// （listen 終了までの生存が要るため `keep()` でリーク — テストプロセス終了
/// まで保持できればよく、後始末は不要）。
pub fn mat_listen(socket: &Path, extra: &[&str]) -> Command {
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

/// fake matd。接続ごとに `sessions` の 1 要素を消費: 要求 1 行を読み、`lines`
/// を順に書き、`hold_ms` だけ接続を開いたまま保持してから閉じる（EOF）。
/// 戻り値は各接続で受けた要求行（op の検証用）。
pub fn spawn_fake_matd(
    socket: PathBuf,
    sessions: Vec<(Vec<&'static str>, u64)>,
) -> JoinHandle<Vec<String>> {
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let mut reqs = Vec::new();
        for (lines, hold_ms) in sessions {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut req)
                .unwrap();
            for l in lines {
                stream.write_all(l.as_bytes()).unwrap();
            }
            stream.flush().unwrap();
            if hold_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(hold_ms));
            }
            reqs.push(req);
            // drop(stream) = EOF
        }
        reqs
    })
}
