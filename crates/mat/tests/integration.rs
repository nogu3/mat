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
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE-SETUP-CODE",
            "--node",
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
fn commission_passes_paa_trust_store_when_set() {
    // 本番デバイスの attestation 検証用に、MAT_PAA_TRUST_STORE で指定した PAA
    // ディレクトリが chip-tool へ `--paa-trust-store-path` で渡ること。
    let store = TempDir::new().unwrap();
    let paa = TempDir::new().unwrap();
    let args_file = store.path().join("recorded-args.txt");

    mat(store.path())
        .env("MAT_PAA_TRUST_STORE", paa.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE-SETUP-CODE",
            "--node",
            "7",
        ])
        .assert()
        .success();

    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("--paa-trust-store-path"),
        "args did not include PAA flag: {recorded}"
    );
    assert!(
        recorded.contains(paa.path().to_str().unwrap()),
        "args did not include PAA path: {recorded}"
    );
}

#[test]
fn commission_timeout_exits_3() {
    let store = TempDir::new().unwrap();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

#[test]
fn commission_reject_exits_4() {
    let store = TempDir::new().unwrap();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "reject")
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
        ])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("device_rejected"));
}

#[test]
fn commission_auto_assigns_node_id() {
    let store = TempDir::new().unwrap();
    // node-id 指定なし → 空台帳なので 1 が振られる。
    mat(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"node_id\":1"));
}

/// node 5 を commission 済みにしたストアを用意する（Phase 1 操作系の前提）。
fn store_with_node5() -> TempDir {
    let store = TempDir::new().unwrap();
    mat(store.path())
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

#[test]
fn read_parses_value() {
    let store = store_with_node5();
    mat(store.path())
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
        .stdout(predicate::str::contains("\"attribute\":\"on-off\""))
        .stdout(predicate::str::contains("\"value\":true"))
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn diag_thread_returns_mesh_snapshot() {
    let store = store_with_node5();
    mat(store.path())
        .args(["diag", "thread", "--node", "5"])
        .assert()
        .success()
        // スナップショット骨格と既定 endpoint 0。
        .stdout(predicate::str::contains("\"thread\""))
        .stdout(predicate::str::contains("\"endpoint\":0"))
        // スカラ（routing-role の enum は数値のまま）。どの Thread 網かは extended-pan-id。
        .stdout(predicate::str::contains("\"routing_role\":5"))
        // 文字列の長さ注釈 `(14 chars)` は剥がれ、引用符も含まない。
        .stdout(predicate::str::contains(
            "\"network_name\":\"ha-thread-6562\"",
        ))
        .stdout(predicate::str::contains("\"pan_id\":25954"))
        // neighbor-table の struct-list が配列で出る。キーは chip-tool 表記のまま。
        .stdout(predicate::str::contains("\"neighbor_table\""))
        .stdout(predicate::str::contains("\"AverageRssi\":-95"))
        .stdout(predicate::str::contains("\"Lqi\":3"))
        .stdout(predicate::str::contains("\"route_table\""))
        .stdout(predicate::str::contains("\"PathCost\":1"))
        // 全属性成功時は unavailable を出さない。
        .stdout(predicate::str::contains("\"unavailable\"").not())
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn diag_thread_partial_records_unavailable() {
    // 間欠不通の機器を模し neighbor-table だけ失敗させる。残りは返しつつ、失敗属性は
    // unavailable に記録、未取得テーブルは null（空配列 `[]` = 真にゼロ、とは区別）。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_THREAD_FAIL_ATTR", "neighbor-table")
        .args(["diag", "thread", "--node", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"routing_role\":5"))
        .stdout(predicate::str::contains("\"neighbor_table\":null"))
        .stdout(predicate::str::contains("\"unavailable\""))
        .stdout(predicate::str::contains("\"attribute\":\"neighbor-table\""))
        .stdout(predicate::str::contains("\"kind\":\"device_rejected\""));
}

#[test]
fn diag_thread_fully_unreachable_exits_3() {
    // 全属性が timeout（完全不達）なら部分結果を諦め、timeout を伝播する（exit 3）。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args(["diag", "thread", "--node", "5"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

#[test]
fn write_reports_success() {
    let store = store_with_node5();
    mat(store.path())
        .args([
            "write",
            "--node",
            "5",
            "--cluster",
            "levelcontrol",
            "--attribute",
            "on-level",
            "--value",
            "128",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"success\""))
        // write の value は read と型を揃える（文字列 "128" ではなく整数 128）。
        .stdout(predicate::str::contains("\"value\":128"));
}

#[test]
fn invoke_reports_success() {
    let store = store_with_node5();
    mat(store.path())
        .args([
            "invoke",
            "--node",
            "5",
            "--cluster",
            "onoff",
            "--command",
            "toggle",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\":\"toggle\""))
        .stdout(predicate::str::contains("\"status\":\"success\""));
}

#[test]
fn on_maps_to_onoff_invoke() {
    let store = store_with_node5();
    mat(store.path())
        .args(["on", "--node", "5"])
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
        .args(["off", "--node", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\":\"off\""));
}

#[test]
fn describe_lists_endpoints_and_clusters() {
    let store = store_with_node5();
    mat(store.path())
        .args(["describe", "--node", "5"])
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
        .args(["open-window", "--node", "5"])
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
        .args(["open-window", "--node", "99"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn open_window_timeout_exits_3() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args(["open-window", "--node", "5"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
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
fn read_timeout_exits_3() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
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
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

#[test]
fn invoke_reject_exits_4() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "reject")
        .args([
            "invoke",
            "--node",
            "5",
            "--cluster",
            "onoff",
            "--command",
            "on",
        ])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("device_rejected"));
}

// ── Phase 3: groupcast ──────────────────────────────────────────────────────

#[test]
fn group_provision_succeeds() {
    let store = store_with_node5();
    mat(store.path())
        .args([
            "group",
            "provision",
            "--group",
            "1",
            "--nodes",
            "5",
            "--name",
            "living",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"group_id\":1"))
        .stdout(predicate::str::contains("\"keyset_id\":42"))
        .stdout(predicate::str::contains("\"name\":\"living\""))
        .stdout(predicate::str::contains("\"status\":\"provisioned\""))
        .stdout(predicate::str::contains("\"nodes\":[5]"))
        .stdout(predicate::str::contains("\"timestamp\""));
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
fn group_provision_rejects_bad_epoch_key() {
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
        .stderr(predicate::str::contains("epoch-key"));
}

#[test]
fn group_provision_last_chip_call_is_add_group() {
    // 引数ファイルは各 chip-tool 呼び出しで上書きされるため、最後の呼び出し
    // （node 5 への groups add-group）が記録される。group 名と endpoint を確認。
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args([
            "group",
            "provision",
            "--group",
            "7",
            "--nodes",
            "5",
            "--name",
            "kitchen",
            "--endpoint",
            "2",
        ])
        .assert()
        .success();
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("groups add-group 7 kitchen 5 2"),
        "last chip-tool call was not the expected add-group: {recorded}"
    );
}

#[test]
fn group_invoke_reports_sent() {
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args([
            "group",
            "invoke",
            "--group",
            "1",
            "--cluster",
            "onoff",
            "--command",
            "on",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"group_id\":1"))
        .stdout(predicate::str::contains("\"command\":\"on\""))
        .stdout(predicate::str::contains("\"status\":\"sent\""))
        .stdout(predicate::str::contains("unacknowledged"));
    // group multicast 宛先（0xffffffffffff0001）が末尾近くに渡ること。
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("onoff on 0xffffffffffff0001 1"),
        "group node-id was not passed as the destination: {recorded}"
    );
}

#[test]
fn group_invoke_timeout_exits_3() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args([
            "group",
            "invoke",
            "--group",
            "1",
            "--cluster",
            "onoff",
            "--command",
            "on",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

// ── diag node ──────────────────────────────────────────────────────────────

#[test]
fn diag_node_success_verdict_ok() {
    let store = store_with_node5();
    mat(store.path())
        .args(["diag", "node", "--node", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"verdict\":\"ok\""))
        .stdout(predicate::str::contains("\"checks\""))
        .stdout(predicate::str::contains("\"timestamp\""));
}

#[test]
fn diag_node_timeout_is_unresolvable_exit0() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .args(["diag", "node", "--node", "5"])
        .assert()
        .success() // 診断は落ちない
        .stdout(predicate::str::contains("\"verdict\":\"unresolvable\""));
}

#[test]
fn diag_node_reject_is_device_rejected_exit0() {
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "reject")
        .args(["diag", "node", "--node", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"verdict\":\"device_rejected\""));
}

fn fake_ping6() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-ping6.sh")
}
fn fake_avahi() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-avahi-browse.sh")
}

#[test]
fn diag_node_deep_link_starved() {
    // operational timeout + ip 生存(50%ロス) + mDNS に node5 広告なし → link_starved。
    // self_cfid = 00AABB1122CC3344 (fake-chip-tool CFID)。
    // avahi デフォルト出力: node 0xFF under 0011223344556677（アドレス付きでない）。
    // → advertised_any_fabric=false → weak_link(loss 50%) → link_starved。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .env("MAT_PING6_BIN", fake_ping6())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .args(["diag", "node", "--node", "5", "--deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"verdict\":\"link_starved\""))
        .stdout(predicate::str::contains("\"ip\""))
        .stdout(predicate::str::contains("\"loss_pct\":50"));
}

#[test]
fn diag_node_deep_ip_unreachable() {
    // operational timeout + ping 100% loss → ip_unreachable。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .env("MAT_PING6_BIN", fake_ping6())
        .env("FAKE_PING_LOSS", "100")
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .args(["diag", "node", "--node", "5", "--deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"verdict\":\"ip_unreachable\""));
}

#[test]
fn diag_node_deep_fabric_missing() {
    // operational timeout + ip ok (50%ロス) + avahi: 192.0.2.10 が他 fabric 下に広告
    // (FAKE_AVAHI_FABRIC=0011223344556677 != fake-chip-tool CFID 00AABB1122CC3344)
    // → advertised_any_fabric=true, advertised_self_fabric=Some(false) → fabric_missing。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .env("MAT_PING6_BIN", fake_ping6())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .env("FAKE_AVAHI_ADDR", "192.0.2.10")
        .env("FAKE_AVAHI_FABRIC", "0011223344556677")
        .args(["diag", "node", "--node", "5", "--deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"verdict\":\"fabric_missing\""));
}

#[test]
fn diag_node_deep_missing_probe_binary() {
    // ping6 バイナリが存在しない → unavailable に tool_missing、verdict は出る、exit 0。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_CHIP_MODE", "timeout")
        .env("MAT_PING6_BIN", "/nonexistent/ping6")
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .args(["diag", "node", "--node", "5", "--deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"tool_missing\""))
        .stdout(predicate::str::contains("\"verdict\""));
}

// ── discover --probe ────────────────────────────────────────────────────────

#[test]
fn discover_probe_reports_reachable_with_live_address() {
    // node 5 を commission 済み（台帳 address = 192.0.2.10）。avahi が node 5 を
    // 別アドレス 192.0.2.99 で広告 → reachable:true、address はライブ値に更新。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .env("FAKE_AVAHI_ADDR", "192.0.2.99")
        .args(["discover", "--probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"state\":\"commissioned\""))
        .stdout(predicate::str::contains("\"reachable\":true"))
        .stdout(predicate::str::contains("\"address\":\"192.0.2.99\""));
}

#[test]
fn discover_probe_reports_unreachable_and_stale() {
    // avahi に node 5 の広告なし（既定出力は node FF のみ）→ reachable:false、
    // stale:true、address は台帳の据え置き値 192.0.2.10。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .args(["discover", "--probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"reachable\":false"))
        .stdout(predicate::str::contains("\"stale\":true"))
        .stdout(predicate::str::contains("\"address\":\"192.0.2.10\""));
}

#[test]
fn discover_without_probe_omits_reachable() {
    // --probe 無しは従来出力（reachable/stale を付与しない）。後方互換。
    let store = store_with_node5();
    mat(store.path())
        .arg("discover")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"state\":\"commissioned\""))
        .stdout(predicate::str::contains("\"reachable\"").not())
        .stdout(predicate::str::contains("\"stale\"").not());
}

#[test]
fn discover_probe_with_missing_avahi_reports_reachable_null() {
    // avahi-browse バイナリ不在 → プローブ不能。reachable:null、stdout は純 JSON、
    // discover 全体は成功（commissionable 探索は別経路で有効なため）。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_AVAHI_BROWSE_BIN", "/nonexistent/avahi-browse-binary")
        .args(["discover", "--probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"reachable\":null"));
}
