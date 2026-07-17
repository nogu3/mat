//! chip-tool を一切 spawn しない統合テスト（M8c-3 Task3）。
//!
//! Stage 1（native 既定化, Task4）以降、`MAT_IFACE` 未設定でも native 経路に
//! 入るため、chip-tool のダミー実行体を PATH に注入する前提の「成功時の JSON
//! 内容」テストは環境依存で挙動が変わってしまう。ここに残すのは
//! **バックエンド（native / chip-tool）に
//! 到達する前に完結する** テストだけ: CLI 引数エラー（exit 2）・store エラー
//! （exit 10/11）・alias 解決（成功はバックエンド到達前で観測できる形のみ・
//! 失敗は exit 2/10）・`--matd` 非対応サブコマンド（exit 2）。
//!
//! 成功時の JSON スキーマ（read/write/invoke/describe/diag node 等）の検証は
//! `crates/mat/src/native_direct.rs` の `FakeConn` ベースのユニットテストに
//! 委譲した（chip-tool 統合テストでの重複カバレッジを持たない）。

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// テスト用の `mat` コマンド。store は与えられた dir。
///
/// `MAT_IFACE=lo` を固定する: Task4（native 既定化）で `MAT_IFACE` 未設定は
/// 自動検出に切り替わるため、先にこのテストスイート側を「明示 iface 指定」の
/// 形に揃えておく（`lo` は実在するが KVS 資材が無いので native 経路は必ず
/// warn + フォールスルーし、store/require_node チェックはこれまでどおり
/// コマンド層 or native_direct::execute() の同一ロジックで exit 10/11 を出す
/// — 詳細は native_direct.rs の `execute()` の doc コメント参照）。
/// `MAT_MATD=0` で直経路に固定する（matd 自動検出が既定のため、開発機で実
/// matd が動いていても拾わない）。
fn mat(store: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_IFACE", "lo")
        .env("MAT_MATD", "0")
        .arg("--store")
        .arg(store);
    c
}

/// node 5 が commission 済みのストアを直接構築する（chip-tool を経由しない —
/// `mat_core::store::Store` の `nodes.json` スキーマに直接書く）。
fn store_with_node5() -> TempDir {
    let store = TempDir::new().unwrap();
    std::fs::write(
        store.path().join("nodes.json"),
        r#"{"version":1,"nodes":{"5":{"node_id":5,"address":"192.0.2.10","commissioned_at":"2026-01-01T00:00:00+09:00"}}}"#,
    )
    .unwrap();
    store
}

// ── CLI 引数エラー（clap レベル、exit 2、バックエンド不到達）───────────────

#[test]
fn color_temp_requires_exactly_one_of_kelvin_or_mireds() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color-temp", "--node", "5"])
        .assert()
        .code(2);
    mat(store.path())
        .args([
            "color-temp",
            "--node",
            "5",
            "--kelvin",
            "2700",
            "--mireds",
            "370",
        ])
        .assert()
        .code(2);
}

#[test]
fn color_requires_both_hue_and_sat() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color", "--node", "5", "--hue", "330"])
        .assert()
        .code(2);
    mat(store.path())
        .args(["color", "--node", "5", "--sat", "80"])
        .assert()
        .code(2);
}

#[test]
fn color_rejects_out_of_range_values() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color", "--node", "5", "--hue", "361", "--sat", "80"])
        .assert()
        .code(2);
    mat(store.path())
        .args(["color", "--node", "5", "--hue", "330", "--sat", "101"])
        .assert()
        .code(2);
}

#[test]
fn color_spec_systems_are_mutually_exclusive() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color", "--node", "5", "--name", "red", "--rgb", "#ff0000"])
        .assert()
        .code(2);
    mat(store.path())
        .args([
            "color", "--node", "5", "--name", "red", "--hue", "0", "--sat", "100",
        ])
        .assert()
        .code(2);
    mat(store.path())
        .args([
            "color", "--node", "5", "--rgb", "#ff0000", "--hue", "0", "--sat", "100",
        ])
        .assert()
        .code(2);
    // どの系統も無し / hue・sat の片割れも exit 2（既存挙動の維持）。
    mat(store.path())
        .args(["color", "--node", "5"])
        .assert()
        .code(2);
    mat(store.path())
        .args(["color", "--node", "5", "--hue", "330"])
        .assert()
        .code(2);
}

#[test]
fn group_color_temp_requires_exactly_one_of_kelvin_or_mireds() {
    let store = store_with_node5();
    mat(store.path())
        .args(["group", "color-temp", "--group", "1"])
        .assert()
        .code(2);
    mat(store.path())
        .args([
            "group",
            "color-temp",
            "--group",
            "1",
            "--kelvin",
            "2700",
            "--mireds",
            "370",
        ])
        .assert()
        .code(2);
}

#[test]
fn group_color_spec_systems_are_mutually_exclusive() {
    let store = store_with_node5();
    mat(store.path())
        .args([
            "group", "color", "--group", "1", "--name", "red", "--hue", "0", "--sat", "100",
        ])
        .assert()
        .code(2);
    mat(store.path())
        .args(["group", "color", "--group", "1"])
        .assert()
        .code(2);
}

// ── resolve.rs レベルのエラー（kind=other→exit2、または store_parse→exit10、
//    いずれもバックエンド不到達） ────────────────────────────────────────────

#[test]
fn color_invalid_rgb_exits_2() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color", "--node", "5", "--rgb", "zzz"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"other\""));
}

#[test]
fn color_unknown_name_exits_2_and_broken_colors_exits_10() {
    let store = store_with_node5();
    // 未知の色名は CLI 引数相当のエラー（kind=other, exit 2）。
    mat(store.path())
        .args(["color", "--node", "5", "--name", "sakura"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"other\""));
    // 壊れた [colors]（RGB パース不能）は store_parse（exit 10）。
    std::fs::write(
        store.path().join("aliases.toml"),
        "[colors]\nbad = \"zzz\"\n",
    )
    .unwrap();
    mat(store.path())
        .args(["color", "--node", "5", "--name", "red"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("store_parse"));
}

// ── store エラー（Store::open / require_node、バックエンド不到達）──────────
//
// native 直経路（`native_direct::execute()`）は Store::open / require_node を
// Engine 構築より前に行う（chip-tool 経路と同一の順序・エラー）ので、
// `MAT_IFACE=lo` 固定でもこれらのテストは変わらず backend 不到達で完結する。

#[test]
fn read_missing_store_exits_10() {
    let store = TempDir::new().unwrap();
    let missing = store.path().join("nope");
    mat(&missing)
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
        .code(10)
        .stderr(predicate::str::contains("store_missing"));
}

#[test]
fn read_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
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
fn color_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color", "--node", "99", "--hue", "330", "--sat", "80"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn color_temp_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color-temp", "--node", "99", "--kelvin", "2700"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn open_window_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["open-window", "--node", "99"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn diag_node_unknown_node_exits_11() {
    // `commands::diag::node` は native IM probe より前に Store::require_node
    // する（`crates/mat/src/commands/diag.rs` 参照）ので、native/chip-tool の
    // どちらにも到達しない。
    let store = store_with_node5();
    mat(store.path())
        .args(["diag", "node", "--node", "99"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn group_provision_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["group", "provision", "--group", "1", "--nodes", "99"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn group_grant_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["group", "grant", "--group", "1", "--nodes", "99"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn group_provision_rejects_bad_epoch_key() {
    // require_node(5) はここでは通る（台帳にある）。epoch key の検証は
    // controller state 書込（KVS/chip-tool）より前に走るので、この失敗は
    // バックエンドに一切触れない（`provision_controller_state` 冒頭で
    // `resolve_epoch_key` を呼ぶ — `crates/mat/src/commands/group.rs` 参照）。
    let store = store_with_node5();
    mat(store.path())
        .args([
            "group",
            "provision",
            "--group",
            "1",
            "--nodes",
            "5",
            "--epoch-key",
            "dead",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("\"kind\":\"other\""));
}

// ── `--matd` 明示 + 非対応サブコマンド（matd プロトコル未到達、exit 2）───────

#[test]
fn group_grant_with_forced_matd_exits_2() {
    // grant は直経路のみ（matd プロトコルに op を追加しない）。`to_op()` が
    // 未対応と判定した時点で弾かれるので、指定したソケットには一切接続しない
    // （`crates/mat/src/matd_client.rs` の `dispatch()` 参照）。
    let store = store_with_node5();
    mat(store.path())
        .args([
            "--matd",
            "/nonexistent/matd.sock",
            "group",
            "grant",
            "--group",
            "1",
            "--nodes",
            "5",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"other\""));
}

// ── discover ─────────────────────────────────────────────────────────────
//
// `discover`（commissionable browse・`--probe` の mDNS targeted resolve 共に）
// はこのテスト環境（サンドボックス）では `lo` 上の multicast 送信が
// `Network is unreachable` として即エラーになり、native 側が chip-tool へ
// フォールバックしてしまう（実機の「0 件で正常終了」や「IFF_MULTICAST 無しで
// フォールバックのみ avahi 側に出る」という前提が成り立たない）。discover の
// 成功系はもともと chip-tool のダミー実行体依存だった（撤去対象）ため、ここでは
// 追わない — mDNS browse/probe 自体のロジックは `mat-controller::dnssd` /
// `crates/mat/src/probe.rs` 側の責務。

// ---- alias 解決（aliases.toml） ----
//
// alias 解決自体（`resolve::resolve_command`）は main.rs でバックエンド経路
// 選択より前に完結する。解決の失敗は kind=other（exit 2）または
// store_parse（exit 10）。解決の成功は、後続の Store::require_node が未
// commission で弾く場面（exit 11、node_not_commissioned の detail に解決後の
// 数値 node_id が載る）で観測する — read/write 等を実際に成功させるには
// バックエンドが要るため、ここでは検証しない（成功時の JSON は
// native_direct.rs のユニットテスト側の責務）。

#[test]
fn node_and_endpoint_alias_resolve_before_backend_then_node_not_commissioned() {
    // node 5 は台帳に無い（aliases.toml だけを置く）。alias が数値 5 / 2 へ
    // 正しく解決された上で Store::require_node(5) が未 commission として弾く
    // （node/endpoint いずれかの alias 解決が失敗していれば exit 2 になり、
    // このテストは exit 11 を観測できない）。
    let store = TempDir::new().unwrap();
    std::fs::write(
        store.path().join("aliases.toml"),
        "[nodes]\nliving-light = 5\n\n[endpoints.living-light]\nnight = 2\n",
    )
    .unwrap();
    mat(store.path())
        .args(["on", "--node", "living-light", "--endpoint", "night"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains(
            "\"kind\":\"node_not_commissioned\"",
        ));
}

#[test]
fn unknown_alias_exits_2() {
    let store = store_with_node5();
    std::fs::write(
        store.path().join("aliases.toml"),
        "[nodes]\nliving-light = 5\n",
    )
    .unwrap();
    mat(store.path())
        .args(["describe", "--node", "bogus"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"other\""));
}

#[test]
fn alias_without_aliases_file_exits_2() {
    let store = store_with_node5(); // aliases.toml 無し
    mat(store.path())
        .args(["describe", "--node", "living-light"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"other\""));
}

#[test]
fn corrupt_aliases_file_exits_10() {
    let store = store_with_node5();
    std::fs::write(store.path().join("aliases.toml"), "not = = toml").unwrap();
    mat(store.path())
        .args(["describe", "--node", "5"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("store_parse"));
}

#[test]
fn all_digit_alias_name_in_file_exits_10() {
    let store = store_with_node5();
    std::fs::write(store.path().join("aliases.toml"), "[nodes]\n42 = 5\n").unwrap();
    mat(store.path())
        .args(["describe", "--node", "5"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("store_parse"));
}

#[test]
fn commission_with_duplicate_alias_exits_2_before_running() {
    let store = store_with_node5();
    std::fs::write(
        store.path().join("aliases.toml"),
        "[nodes]\nliving-light = 5\n",
    )
    .unwrap();
    mat(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
            "--alias",
            "living-light",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"other\""));
}

#[test]
fn commission_with_all_digit_alias_exits_2() {
    let store = TempDir::new().unwrap();
    mat(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
            "--alias",
            "42",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"other\""));
}

// ── native 既定化（M8c-3 Task4）: MAT_IFACE 未設定は autodetect に入る ──────

/// `mat()` ヘルパーは `MAT_IFACE=lo` を固定するため、このテストだけ env を
/// 外して autodetect を発火させる。候補数は環境依存（CI/開発機で 0 個も
/// 複数個もあり得る）なので、成功可否ではなく「autodetect が実際に走った
/// 証拠が stderr に出ること」を assert する: マーカー
/// `iface auto-selected (native default)`（候補一意 → native 経路へ進み、
/// 別の理由で失敗するケース）か、autodetect 自身のエラー（候補 0 /
/// 複数、kind `other`）のどちらか。単に `"error"` を含むだけでは
/// chip-tool 不在（exit 12）でも通ってしまい無意味なため、どちらの経路
/// にも共通する証拠として `iface` という語の出現を要求する。
#[test]
fn no_iface_env_reaches_autodetect_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("mat").unwrap();
    cmd.env_remove("MAT_IFACE")
        .env("MAT_MATD", "0")
        .arg("--store")
        .arg(dir.path())
        .args([
            "read",
            "--node",
            "1",
            "--cluster",
            "onoff",
            "--attribute",
            "on-off",
        ]);
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("\"error\""),
        "structured error expected: {stderr}"
    );
    // autodetect が実際に走った証拠: 成功時の info marker か、autodetect
    // 自身の失敗（候補 0/複数）のどちらか。chip-tool 不在の
    // child_not_found（native 未経由）ではこの語は出ない。
    assert!(
        stderr.contains("iface auto-selected (native default)")
            || stderr.contains("iface autodetect"),
        "expected evidence that iface autodetect ran: {stderr}"
    );
}
