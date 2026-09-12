mod dedicated_op_socket;

use super::*;

#[test]
fn thread_egress_explicit_failure_is_hard_error() {
    let r = thread_egress_decision(
        "eth0",
        &Some(ThreadIfaceChoice::Explicit("wpan9".into())),
        |_| Err("no such iface".into()),
    );
    assert!(r.is_err());
}

#[test]
fn thread_egress_auto_failure_degrades_to_lan_only() {
    let r = thread_egress_decision(
        "eth0",
        &Some(ThreadIfaceChoice::Auto("wpan0".into())),
        |_| Err("no such iface".into()),
    );
    assert_eq!(r.unwrap(), None);
}

#[test]
fn thread_egress_resolved_returns_scope() {
    let r = thread_egress_decision(
        "eth0",
        &Some(ThreadIfaceChoice::Auto("wpan0".into())),
        |_| Ok(7),
    );
    assert_eq!(r.unwrap(), Some(("wpan0".into(), 7)));
}

#[test]
fn thread_egress_none_is_lan_only() {
    let r = thread_egress_decision("eth0", &None, |_| unreachable!());
    assert_eq!(r.unwrap(), None);
}

/// 監査 Minor-1: 運用 iface（`cfg.iface`）と thread iface が同名なら
/// `resolve` すら呼ばず第 2 egress を張らない（`MAT_IFACE=wpan0` +
/// wpan0 自動検出の同一 iface 二重送出を回避）。`resolve` を
/// `unreachable!()` にして「呼ばれないこと」自体を固定する。
#[test]
fn thread_egress_same_as_op_iface_is_skipped_without_resolving_auto() {
    let r = thread_egress_decision(
        "wpan0",
        &Some(ThreadIfaceChoice::Auto("wpan0".into())),
        |_| unreachable!("resolve must not be called when thread iface == op iface"),
    );
    assert_eq!(r.unwrap(), None);
}

/// 同上、explicit 指定でも同じ規律（同一 iface はハードエラーにせず
/// 単に第 2 egress を張らない）。
#[test]
fn thread_egress_same_as_op_iface_is_skipped_without_resolving_explicit() {
    let r = thread_egress_decision(
        "wpan0",
        &Some(ThreadIfaceChoice::Explicit("wpan0".into())),
        |_| unreachable!("resolve must not be called when thread iface == op iface"),
    );
    assert_eq!(r.unwrap(), None);
}

#[tokio::test]
async fn generic_read_write_via_fake() {
    use crate::test_support::FakeEstablisher;
    let engine = Engine::with_parts(Box::new(FakeEstablisher::default()), None);
    let mut conn = engine.establisher.establish(5).await.unwrap();
    // fake は read_json に固定値を返す（test_support 拡張で定義）。
    let v = conn.read_json(1, 0x0008, 0x0000).await.unwrap();
    assert!(v.is_number());
    conn.write_tlv(
        1,
        0x0008,
        0x0011,
        arg_value_to_tlv(&mat_core::ids::ArgValue::UInt(128)),
        false,
    )
    .await
    .unwrap();
    let all = conn.read_cluster(1, 0x0006).await.unwrap();
    assert!(!all.is_empty());
}

#[tokio::test]
async fn build_fails_cleanly_without_kvs() {
    // KVS が無いディレクトリでは store_missing 相当のエラーで即失敗し、
    // panic しない（matd 起動時に安全フォールバックへ落とす判断材料）。
    let dir = tempfile::tempdir().unwrap();
    let cfg = NativeConfig {
        store: dir.path().to_path_buf(),
        iface: "lo".to_string(),
        thread_iface: None,
        fabric_index: 1,
        issuer_index: 0,
    };
    let err = Engine::build(&cfg).await.expect_err("no KVS present");
    assert!(
        matches!(
            err.kind,
            ErrorKind::StoreMissing | ErrorKind::StoreParse | ErrorKind::Other
        ),
        "unexpected kind: {:?}",
        err.kind
    );
}

#[test]
fn load_fabric_credentials_maps_missing_store_to_store_missing() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = NativeConfig {
        store: dir.path().to_path_buf(),
        iface: "lo".into(),
        thread_iface: None,
        fabric_index: 1,
        issuer_index: 0,
    };
    let err = load_fabric_credentials(&cfg).unwrap_err();
    assert_eq!(err.kind, ErrorKind::StoreMissing);
    // Clone できる（rotate-ipk が確立器生成クロージャへ move する）。
    let _ = cfg.clone();
}

/// 3 経路（Engine::build / commission / probe）が同じ 1 本を通るので、
/// 文言の `mat fabric init` ヒントと kind をここで釘打ちする。
#[test]
fn load_self_issue_materials_hints_fabric_init_on_missing_store() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = NativeConfig {
        store: dir.path().to_path_buf(),
        iface: "lo".into(),
        thread_iface: None,
        fabric_index: 1,
        issuer_index: 0,
    };
    let err = load_self_issue_materials(&cfg).unwrap_err();
    assert_eq!(err.kind, ErrorKind::StoreMissing);
    assert!(
        err.detail.starts_with("native: read KVS credentials: "),
        "{}",
        err.detail
    );
    assert!(
        err.detail.ends_with(" — run `mat fabric init`"),
        "{}",
        err.detail
    );
}

#[test]
fn op_scope_id_maps_unknown_iface_to_other() {
    let cfg = NativeConfig {
        store: std::path::PathBuf::from("/nonexistent"),
        iface: "no-such-iface-at-all".into(),
        thread_iface: None,
        fabric_index: 1,
        issuer_index: 0,
    };
    let err = op_scope_id(&cfg).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(
        err.detail
            .starts_with("native: resolve iface \"no-such-iface-at-all\" index: "),
        "{}",
        err.detail
    );
}

#[tokio::test]
async fn default_establisher_rejects_subscription() {
    // Establisher trait の default 実装は購読非対応（CaseEstablisher だけが上書き）。
    struct NoSub;
    #[async_trait]
    impl Establisher for NoSub {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            Err(MatError::new(ErrorKind::Other, "unused"))
        }
    }
    // `.unwrap_err()` would require `Box<dyn SubscribeConn>: Debug`, which
    // `SubscribeConn` deliberately doesn't require (mirrors `Engine`'s
    // manual, secret-hiding `Debug` — see its impl above): match instead.
    let err = match NoSub.establish_subscription(1).await {
        Err(e) => e,
        Ok(_) => panic!("default establish_subscription must reject"),
    };
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(err.detail.contains("subscription"));
}

#[tokio::test]
async fn fake_establisher_serves_scripted_subscription() {
    use crate::test_support::{FakeEstablisher, FakeSubConn};
    let est = FakeEstablisher::default();
    let mut conn = est.establish_subscription(5).await.unwrap();
    let (info, priming, _events) = conn.subscribe(&[], &[], None).await.unwrap();
    assert_eq!(info.max_interval_s, 60);
    assert_eq!(priming.len(), 1); // default fake は onoff=true の priming 1 チャンク
                                  // scripted report が尽きたら next_report は timeout まで待って Ok(None)（無音）。
    let silent = conn
        .next_report_full(std::time::Duration::from_millis(50))
        .await
        .unwrap()
        .map(|r| r.data);
    assert!(silent.is_none());
    // 共有 live キューに積めば次の next_report が払い出す。
    est.sub_live
        .lock()
        .unwrap()
        .push_back(crate::test_support::onoff_report(1, false));
    let msg = conn
        .next_report_full(std::time::Duration::from_millis(50))
        .await
        .unwrap()
        .map(|r| r.data)
        .expect("live report");
    assert_eq!(msg.reports.len(), 1);
    let _ = FakeSubConn::default(); // 型が公開されていること
}

/// イベント付き購読（フェーズ B）: `subscribe` が受けた event_paths /
/// event_min を fake が記録し、priming イベントを払い出す。
#[tokio::test]
async fn fake_sub_conn_records_event_scope_and_serves_priming_events() {
    use crate::test_support::{switch_press_event, FakeEstablisher};
    let est = FakeEstablisher::default();
    *est.sub_priming_events.lock().unwrap() = vec![switch_press_event(7, 1)];
    let mut conn = est.establish_subscription(5).await.unwrap();
    let paths = vec![mat_controller::im::EventPathIn::WILDCARD_URGENT];
    let (info, priming, priming_events) =
        conn.subscribe(&[0x0006], &paths, Some(42)).await.unwrap();
    assert_eq!(info.max_interval_s, 60);
    assert_eq!(priming.len(), 1);
    assert_eq!(priming_events, vec![switch_press_event(7, 1)]);
    assert_eq!(*est.sub_clusters.lock().unwrap(), vec![0x0006]);
    assert_eq!(*est.sub_event_paths.lock().unwrap(), paths);
    assert_eq!(*est.sub_event_min.lock().unwrap(), Some(42));
}

/// `next_report_full`: 属性キューとイベントキューは独立に払い出される
/// （属性のみ / イベントのみ / 両方 の 3 形をテストが作れる）。
#[tokio::test]
async fn fake_sub_conn_next_report_full_serves_live_events() {
    use crate::test_support::{onoff_report, switch_press_event, FakeEstablisher};
    let slice = std::time::Duration::from_millis(50);
    let est = FakeEstablisher::default();
    let mut conn = est.establish_subscription(5).await.unwrap();
    conn.subscribe(&[], &[], None).await.unwrap();
    assert!(conn.next_report_full(slice).await.unwrap().is_none());

    // 属性 + イベントが同じ report に同居する形。
    est.sub_live
        .lock()
        .unwrap()
        .push_back(onoff_report(1, false));
    est.sub_live_events
        .lock()
        .unwrap()
        .push_back(vec![switch_press_event(9, 1)]);
    let r = conn
        .next_report_full(slice)
        .await
        .unwrap()
        .expect("live report");
    assert_eq!(r.data.reports.len(), 1);
    assert_eq!(r.events, vec![switch_press_event(9, 1)]);

    // イベントだけの report（属性キューは空）。
    est.sub_live_events
        .lock()
        .unwrap()
        .push_back(vec![switch_press_event(10, 1)]);
    let r = conn
        .next_report_full(slice)
        .await
        .unwrap()
        .expect("event-only report");
    assert!(r.data.reports.is_empty());
    assert_eq!(r.events.len(), 1);
}

/// fake の失敗カウンタ: 残り回数だけ失敗し、尽きたら成功する
/// （matd の再確立ラダーを回すための足場）。既定 0 = 常に成功なので
/// 既存テストの挙動は変わらない。
#[tokio::test]
async fn fake_establisher_fails_subscription_n_times_then_succeeds() {
    use crate::test_support::FakeEstablisher;
    use std::sync::atomic::Ordering;

    let est = FakeEstablisher::default();
    est.fail_subscription.store(2, Ordering::SeqCst);
    for attempt in 1..=2 {
        let err = match est.establish_subscription(5).await {
            Err(e) => e,
            Ok(_) => panic!("attempt {attempt} は失敗するはず"),
        };
        assert_eq!(err.kind, ErrorKind::Timeout, "既定 fail_kind を使う");
    }
    assert!(
        est.establish_subscription(5).await.is_ok(),
        "カウンタが尽きたら成功する"
    );
    // 失敗も試行として数える（matd 側テストが calls で試行回数を主張できる）。
    assert_eq!(est.calls.load(Ordering::SeqCst), 3);
}

/// 確立の**あと**に注入した fail_next_report が pump 側（FakeSubConn）へ効く。
/// Arc 共有でないとこの順序が表現できない。
#[tokio::test]
async fn fake_sub_conn_next_report_fails_when_injected_after_establish() {
    use crate::test_support::FakeEstablisher;
    use std::sync::atomic::Ordering;

    let est = FakeEstablisher::default();
    let mut conn = est.establish_subscription(5).await.unwrap();
    conn.subscribe(&[], &[], None).await.unwrap();

    est.fail_next_report.store(1, Ordering::SeqCst);
    let err = match conn
        .next_report_full(std::time::Duration::from_millis(50))
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("注入した 1 回は Err になるはず"),
    };
    assert_eq!(err.kind, ErrorKind::SessionFailed);
    // 尽きたら従来どおり無音 Ok(None)。
    assert!(conn
        .next_report_full(std::time::Duration::from_millis(50))
        .await
        .unwrap()
        .is_none());
}

#[test]
fn establisher_reload_is_unsupported_by_default() {
    use crate::test_support::FakeEstablisher;
    let engine = Engine::with_parts(Box::new(FakeEstablisher::default()), None);
    let err = engine.reload_credentials().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(
        err.detail.contains("not supported"),
        "detail={}",
        err.detail
    );
}

#[test]
fn check_identity_accepts_same_fabric_and_rejects_changes() {
    let a = fake_creds([0xCC; 16], 0x1234, 0x1B669, [0xAA; 65]);
    // 同じ identity、IPK だけ違う = OK（ローテーション後の姿）。
    let b = fake_creds([0xDD; 16], 0x1234, 0x1B669, [0xAA; 65]);
    check_identity(&a, &b).expect("ipk change alone is fine");
    // fabric_id / node_id / root 公開鍵のどれが変わっても other で拒否。
    for (label, other) in [
        (
            "fabric_id",
            fake_creds([0xCC; 16], 0x9999, 0x1B669, [0xAA; 65]),
        ),
        ("node_id", fake_creds([0xCC; 16], 0x1234, 0x77, [0xAA; 65])),
        (
            "root key",
            fake_creds([0xCC; 16], 0x1234, 0x1B669, [0xAB; 65]),
        ),
    ] {
        let err = check_identity(&a, &other).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other, "{label}");
        assert!(
            err.detail.contains("restart matd"),
            "{label}: {}",
            err.detail
        );
    }
}

/// テスト用: 証明書無しの `FabricCredentials`（identity 照合と swap だけに使う）。
fn fake_creds(
    ipk: [u8; 16],
    fabric_id: u64,
    node_id: u64,
    root_public_key: [u8; 65],
) -> FabricCredentials {
    FabricCredentials {
        rcac_tlv: Vec::new(),
        icac_tlv: None,
        noc_tlv: Vec::new(),
        op_public_key: [0u8; 65],
        op_private_key: [0u8; 32],
        ipk_operational: ipk,
        node_id,
        fabric_id,
        root_public_key,
    }
}

#[test]
fn case_establisher_swap_reports_ipk_change() {
    let est = CaseEstablisher {
        creds: std::sync::RwLock::new(Arc::new(fake_creds([0xCC; 16], 1, 2, [0xAA; 65]))),
        scope_id: 0,
        resolver: Arc::new(OneShotResolver),
        cfg: NativeConfig {
            store: std::path::PathBuf::from("/nonexistent"),
            iface: "lo".into(),
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        },
    };
    // 同じ IPK → false、違う IPK → true、その後は新しい値が見える。
    assert!(!est.swap_credentials(fake_creds([0xCC; 16], 1, 2, [0xAA; 65])));
    assert!(est.swap_credentials(fake_creds([0xDD; 16], 1, 2, [0xAA; 65])));
    assert_eq!(est.creds().ipk_operational, [0xDD; 16]);
}

/// KVS が無い store で reload すると store_missing（`load_fabric_credentials`
/// と同じ写像）。swap は起きない。
#[test]
fn case_establisher_reload_maps_missing_store_to_store_missing() {
    let dir = tempfile::tempdir().unwrap();
    let est = CaseEstablisher {
        creds: std::sync::RwLock::new(Arc::new(fake_creds([0xCC; 16], 1, 2, [0xAA; 65]))),
        scope_id: 0,
        resolver: Arc::new(OneShotResolver),
        cfg: NativeConfig {
            store: dir.path().to_path_buf(),
            iface: "lo".into(),
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        },
    };
    let err = est.reload_credentials().unwrap_err();
    assert_eq!(err.kind, ErrorKind::StoreMissing);
    assert_eq!(est.creds().ipk_operational, [0xCC; 16]);
}

/// 実 KVS のフィクスチャ: 新しい fabric を bootstrap した store（chip-tool
/// INI 互換の alpha + main）と、それを指す `NativeConfig`。TempDir は
/// 呼び手が生かし続ける（drop でストアごと消える）。
fn bootstrapped_store(fabric_id: u64, node_id: u64) -> (tempfile::TempDir, NativeConfig) {
    let dir = tempfile::tempdir().unwrap();
    let fab =
        mat_controller::commissioning::CommissioningFabric::generate(fabric_id, node_id).unwrap();
    fab.write_kvs_bootstrap(dir.path(), 1, 0).unwrap();
    let cfg = NativeConfig {
        store: dir.path().to_path_buf(),
        iface: "lo".into(),
        thread_iface: None,
        fabric_index: 1,
        issuer_index: 0,
    };
    (dir, cfg)
}

/// `case_establisher` と同じ形の確立器を、iface 解決を挟まずに組む
/// （テストに実 iface は要らない — reload は KVS しか触らない）。
fn establisher_over_store(creds_cfg: &NativeConfig, reload_cfg: &NativeConfig) -> CaseEstablisher {
    let creds = load_fabric_credentials(creds_cfg).expect("bootstrapped store loads");
    CaseEstablisher {
        creds: std::sync::RwLock::new(Arc::new(creds)),
        scope_id: 0,
        resolver: Arc::new(OneShotResolver),
        cfg: reload_cfg.clone(),
    }
}

/// (a) ストアが変わっていなければ reload は `Ok(false)`、IPK も据え置き。
#[test]
fn case_establisher_reload_over_real_kvs_is_false_when_unchanged() {
    let (_dir, cfg) = bootstrapped_store(0x1234, 112233);
    let est = establisher_over_store(&cfg, &cfg);
    let before = est.creds().ipk_operational;
    assert!(
        !est.reload_credentials()
            .expect("reload over an intact store"),
        "unchanged store must report ipk unchanged"
    );
    assert_eq!(est.creds().ipk_operational, before);
}

/// (b) `f/<idx>/k/0` を別 epoch で書き換える（rotate-ipk の commit と同じ
/// 経路）と reload は `Ok(true)`、新 epoch の運用鍵が入る。
#[test]
fn case_establisher_reload_over_real_kvs_picks_up_a_rotated_ipk() {
    let (_dir, cfg) = bootstrapped_store(0x1234, 112233);
    let est = establisher_over_store(&cfg, &cfg);
    let before = est.creds();
    let cfid = compressed_fabric_id(&before.root_public_key, before.fabric_id);
    let main_ini = cfg.store.join(mat_controller::kvs::MAIN_INI_FILE);
    let cur = mat_controller::kvs::read_mat_ipk_epoch(&main_ini, cfg.fabric_index)
        .unwrap()
        .expect("bootstrap persists the current epoch");
    let next = [0x5A; 16];
    assert_ne!(cur, next, "the fixture epoch must differ from the new one");
    mat_controller::group_settings::begin_ipk_rotation(&main_ini, cfg.fabric_index, &next).unwrap();
    mat_controller::group_settings::commit_ipk_rotation(
        &main_ini,
        cfg.fabric_index,
        &cfid,
        &cur,
        &next,
    )
    .unwrap();

    assert!(
        est.reload_credentials().expect("reload after a commit"),
        "a rotated IPK must report changed"
    );
    assert_eq!(
        est.creds().ipk_operational,
        mat_controller::fabric::derive_ipk_operational(&next, &cfid),
        "the new epoch's operational key must be installed"
    );
}

/// (c) 別 fabric の store を指した reload は `other` で拒否され、資格情報は
/// 一切差し替わらない（restart 案内）。
#[test]
fn case_establisher_reload_over_real_kvs_rejects_a_different_fabric() {
    let (_dir_a, cfg_a) = bootstrapped_store(0x1234, 112233);
    let (_dir_b, cfg_b) = bootstrapped_store(0x9999, 112233);
    // 起動時の資格情報は A、reload が読むのは B（= 取り違えた store）。
    let est = establisher_over_store(&cfg_a, &cfg_b);
    let before = est.creds().ipk_operational;

    let err = est.reload_credentials().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(err.detail.contains("restart matd"), "detail={}", err.detail);
    assert_eq!(est.creds().ipk_operational, before, "no swap on rejection");
}
