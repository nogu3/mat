
use super::*;

fn event_entry(number: u64) -> im::EventEntryOut {
    im::EventEntryOut::Data(im::EventReportOut {
        endpoint: 2,
        cluster: im::CLUSTER_SWITCH,
        event: im::EVENT_SWITCH_INITIAL_PRESS,
        event_number: number,
        priority: im::EventPriority::Info,
        system_timestamp_ms: 1234,
        data_tlv: None,
    })
}

/// `chunk_events` の「最後だけ more_chunks=false、どれも
/// suppress_response=false（SubscribeResponse が続く）」と、
/// イベントが無ければチャンクを 1 つも足さない（属性側が既に
/// 送っている）ことを固定する。
#[test]
fn event_chunks_flag_only_the_last_one_as_final_and_never_suppress() {
    assert!(chunk_events(&[], REPORT_CHUNK_BUDGET, 7).is_empty());

    let one = chunk_events(&[event_entry(1)], REPORT_CHUNK_BUDGET, 7);
    assert_eq!(one.len(), 1);
    let m = im::decode_report_data_message(&one[0]).expect("decodable");
    assert_eq!(m.subscription_id, Some(7));
    assert!(!m.more_chunks);
    assert!(!m.suppress_response);

    // 予算を 1 件ぶんに満たない値まで絞れば必ず分割される。
    let entries: Vec<im::EventEntryOut> = (1..=3).map(event_entry).collect();
    let split = chunk_events(&entries, 1, 7);
    assert_eq!(split.len(), 3);
    for (i, chunk) in split.iter().enumerate() {
        let m = im::decode_report_data_message(chunk).expect("decodable");
        assert_eq!(m.more_chunks, i != split.len() - 1, "chunk {i}");
        assert!(!m.suppress_response, "chunk {i}");
    }
}

/// `fit_events` の 3 分岐: 全部入る / 一部だけ入る（最長 prefix）/
/// 1 件も入らない（0 = 呼び側は属性だけ送って警告する既存挙動）。
/// dirty レポートはチャンク分割しないので、ここが唯一の歯止め。
#[test]
fn fit_events_takes_the_longest_prefix_that_fits_the_budget() {
    let all: Vec<im::EventEntryOut> = (1..=8).map(event_entry).collect();

    // 予算たっぷり: 全件。
    assert_eq!(fit_events(&[], &all, REPORT_CHUNK_BUDGET, 7), all.len());

    // 3 件ちょうどの予算 → 3 件（4 件目で溢れる）。
    let three = im::encode_report_data_full(&[], &all[..3], false, Some(7), false).len();
    assert_eq!(fit_events(&[], &all, three, 7), 3);

    // 1 件も入らない予算 → 0。
    let one = im::encode_report_data_full(&[], &all[..1], false, Some(7), false).len();
    assert_eq!(fit_events(&[], &all, one - 1, 7), 0);

    // 空の候補列はいつでも 0。
    assert_eq!(fit_events(&[], &[], REPORT_CHUNK_BUDGET, 7), 0);
}

/// 属性側の最終チャンクは、イベントが続くとき more_chunks=true に
/// なる（`trailer_follows`）— これを落とすと購読者はイベントを
/// 読まずに報告を終える。属性パスが空（イベントだけの購読）でも
/// 空の ReportData 1 つがこの形で出る。
#[test]
fn an_event_only_priming_report_still_opens_with_a_more_chunks_attribute_report() {
    let node = Node::new();
    let read_ctx = ReadCtx {
        fabric_index: 1,
        fabric_filtered: false,
        subject: Subject::node(1),
    };
    let chunks = node.read_chunks(&[], &read_ctx, REPORT_CHUNK_BUDGET, Some(7), true);
    assert_eq!(chunks.len(), 1);
    let m = im::decode_report_data_message(&chunks[0]).expect("decodable");
    assert!(m.reports.is_empty());
    assert!(m.more_chunks);
    assert!(!m.suppress_response);

    // trailer が無いとき（属性だけの購読）は従来どおり more=false。
    let chunks = node.read_chunks(&[], &read_ctx, REPORT_CHUNK_BUDGET, Some(7), false);
    let m = im::decode_report_data_message(&chunks[0]).expect("decodable");
    assert!(!m.more_chunks);
}

/// Minimal `DeviceConfig` fixture for tests that need a `ServeState`
/// (`config` is only read when a `WindowRequest` reopens the window
/// with `mdns: Some(..)`, neither of which any `serve_secured`-driving
/// test below exercises — `store_dir`/`iface` are never touched by
/// those paths, so their placeholder values are never resolved).
pub(super) fn test_config() -> DeviceConfig {
    DeviceConfig {
        passcode: 20202021,
        discriminator: 3840,
        vendor_id: 0xFFF1,
        product_id: 0x8000,
        port: 5540,
        store_dir: std::path::PathBuf::new(),
        iface: String::new(),
        attestation: Default::default(),
        group_port: 0,
        devices: vec![],
    }
}

#[test]
fn classifies_pase_opcodes() {
    for op in [
        OPCODE_PBKDF_PARAM_REQUEST,
        OPCODE_PBKDF_PARAM_RESPONSE,
        OPCODE_PASE_PAKE1,
        OPCODE_PASE_PAKE2,
        OPCODE_PASE_PAKE3,
    ] {
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, op),
            UnsecuredFlow::Pase,
            "opcode 0x{op:02X} should classify as Pase"
        );
    }
}

#[test]
fn classifies_case_sigma1() {
    assert_eq!(
        classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_CASE_SIGMA1),
        UnsecuredFlow::Case
    );
}

#[test]
fn ignores_foreign_protocol_id() {
    assert_eq!(
        classify_unsecured(PROTOCOL_ID_INTERACTION_MODEL, OPCODE_PBKDF_PARAM_REQUEST),
        UnsecuredFlow::Ignore
    );
}

#[test]
fn ignores_unknown_secure_channel_opcode() {
    // e.g. OPCODE_STATUS_REPORT (0x40) or OPCODE_MRP_STANDALONE_ACK
    // (0x10) reaching the classifier as a "first" datagram — the main
    // loop actually filters standalone acks before classifying, but
    // the classifier itself should still be inert on them (defense in
    // depth / doesn't assume its caller's pre-filtering).
    assert_eq!(
        classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, 0x40),
        UnsecuredFlow::Ignore
    );
    assert_eq!(
        classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_MRP_STANDALONE_ACK),
        UnsecuredFlow::Ignore
    );
}

// ── commissioning window admission (Task 14) ────────────────────────

#[test]
fn admit_unsecured_drops_pase_when_window_closed() {
    assert_eq!(admit_unsecured(UnsecuredFlow::Pase, false), None);
}

#[test]
fn admit_unsecured_allows_pase_when_window_open() {
    assert_eq!(
        admit_unsecured(UnsecuredFlow::Pase, true),
        Some(UnsecuredFlow::Pase)
    );
}

#[test]
fn admit_unsecured_allows_case_regardless_of_window() {
    assert_eq!(
        admit_unsecured(UnsecuredFlow::Case, true),
        Some(UnsecuredFlow::Case)
    );
    assert_eq!(
        admit_unsecured(UnsecuredFlow::Case, false),
        Some(UnsecuredFlow::Case)
    );
}

#[test]
fn admit_unsecured_allows_ignore_regardless_of_window() {
    assert_eq!(
        admit_unsecured(UnsecuredFlow::Ignore, true),
        Some(UnsecuredFlow::Ignore)
    );
    assert_eq!(
        admit_unsecured(UnsecuredFlow::Ignore, false),
        Some(UnsecuredFlow::Ignore)
    );
}

// ── mDNS retry backoff (review fix round 1, item 1) ────────────────

#[tokio::test(start_paused = true)]
async fn mdns_retry_schedules_the_initial_interval_first() {
    let retry = MdnsRetry::new();
    assert_eq!(
        retry.next_attempt_at - retry.first_failure_at,
        MDNS_RETRY_INTERVAL_INITIAL
    );
}

#[tokio::test(start_paused = true)]
async fn mdns_retry_keeps_the_short_interval_before_the_threshold() {
    let mut retry = MdnsRetry::new();
    // Advance to just before the backoff threshold and fail again —
    // still short-interval territory.
    tokio::time::advance(MDNS_RETRY_BACKOFF_THRESHOLD - Duration::from_secs(1)).await;
    retry.schedule_next();
    assert_eq!(
        retry.next_attempt_at - Instant::now(),
        MDNS_RETRY_INTERVAL_INITIAL
    );
}

#[tokio::test(start_paused = true)]
async fn mdns_retry_switches_to_the_long_interval_past_the_threshold() {
    let mut retry = MdnsRetry::new();
    tokio::time::advance(MDNS_RETRY_BACKOFF_THRESHOLD + Duration::from_secs(1)).await;
    retry.schedule_next();
    assert_eq!(
        retry.next_attempt_at - Instant::now(),
        MDNS_RETRY_INTERVAL_LONG
    );
}

#[tokio::test(start_paused = true)]
async fn mdns_retry_deadline_never_resolves_when_no_retry_pending() {
    let none: Option<MdnsRetry> = None;
    // If this ever resolved, the test would hang until its own harness
    // timeout — so this is really "doesn't hang" plus a bounded race
    // against a short sleep to make that assertion concrete.
    tokio::select! {
        () = mdns_retry_deadline(&none) => panic!("deadline resolved with no retry pending"),
        () = tokio::time::sleep(Duration::from_secs(3600)) => {}
    }
}

// ── commissioning window deadline (Task 14) ─────────────────────────

#[tokio::test(start_paused = true)]
async fn commissioning_window_deadline_never_resolves_when_closed() {
    // Same "doesn't hang" technique as the mDNS retry/fail-safe tests
    // above: if this ever resolved, the closed-window branch would
    // spuriously fire and start dropping PASE that should have been
    // fine.
    tokio::select! {
        () = commissioning_window_deadline(&CommissioningWindow::Closed) => {
            panic!("deadline resolved for a closed window")
        }
        () = tokio::time::sleep(Duration::from_secs(3600)) => {}
    }
}

#[tokio::test(start_paused = true)]
async fn commissioning_window_deadline_resolves_once_the_open_window_lapses() {
    let window = CommissioningWindow::Open {
        until: Instant::now() + COMMISSIONING_WINDOW_DURATION,
    };
    tokio::select! {
        () = commissioning_window_deadline(&window) => {}
        () = tokio::time::sleep(COMMISSIONING_WINDOW_DURATION + Duration::from_secs(1)) => {
            panic!("commissioning_window_deadline never resolved for an open window")
        }
    }
}

#[test]
fn commissioning_window_is_open_reports_correctly() {
    assert!(CommissioningWindow::Open {
        until: Instant::now() + COMMISSIONING_WINDOW_DURATION
    }
    .is_open());
    assert!(CommissioningWindow::EnhancedOpen {
        until: Instant::now() + Duration::from_secs(60),
        request: WindowRequest {
            verifier: [0x11; 97],
            discriminator: 100,
            iterations: 1000,
            salt: vec![1, 2, 3],
            timeout_s: 60,
        },
    }
    .is_open());
    assert!(!CommissioningWindow::Closed.is_open());
}

// ── ECM window (Task 4) ──────────────────────────────────────────────

/// dispatch 後に WindowRequest が stage されていれば、runtime の窓が
/// EnhancedOpen になり ECM 用 PASE 設定が得られる（純粋ロジック部分の
/// 単体テスト — apply_window_request を関数に切り出してテストする）。
#[test]
fn apply_window_request_transitions_to_enhanced_open() {
    let req = WindowRequest {
        verifier: [0x42; 97],
        discriminator: 0x0ABC,
        iterations: 1000,
        salt: vec![0x5A; 16],
        timeout_s: 300,
    };
    let window = apply_window_request(req.clone());
    // Destructured from a clone, not `window` itself, so `window` stays
    // usable below (`CommissioningWindow` can't be `Copy` — it carries
    // `WindowRequest`'s `Vec<u8>` salt).
    let CommissioningWindow::EnhancedOpen { until, request } = window.clone() else {
        panic!("expected EnhancedOpen");
    };
    assert_eq!(request.discriminator, 0x0ABC);
    assert!(until > Instant::now());
    // ECM 中の PASE 設定が verifier 素材になること
    let config = pase_config_for_window(
        &window, /*boot passcode*/ 20202021, /*boot salt*/ &[0u8; 32], 0x1234,
    );
    assert!(matches!(config.secret, PaseSecret::VerifierMaterial(m) if m == [0x42; 97]));
    assert_eq!(config.iterations, 1000);
    assert_eq!(config.salt, vec![0x5A; 16]);
}

/// 窓 variant ごとの mDNS 広告パラメータ（CM 値と discriminator）。
#[test]
fn commissionable_advert_params_reflect_window_kind() {
    let boot = CommissioningWindow::Open {
        until: Instant::now() + Duration::from_secs(60),
    };
    assert_eq!(advert_params_for_window(&boot, 3210), Some((3210, 1)));
    let req = WindowRequest {
        verifier: [0x42; 97],
        discriminator: 0x0ABC,
        iterations: 1000,
        salt: vec![0x5A; 16],
        timeout_s: 300,
    };
    let ecm = CommissioningWindow::EnhancedOpen {
        until: Instant::now() + Duration::from_secs(300),
        request: req,
    };
    assert_eq!(advert_params_for_window(&ecm, 3210), Some((0x0ABC, 2)));
    assert_eq!(
        advert_params_for_window(&CommissioningWindow::Closed, 3210),
        None
    );
}

fn test_window_request() -> WindowRequest {
    WindowRequest {
        verifier: [0x11; 97],
        discriminator: 100,
        iterations: 1000,
        salt: vec![1, 2, 3],
        timeout_s: 60,
    }
}

/// staged Some + admin_open true → Apply(request) — the common case: a
/// fresh `OpenCommissioningWindow` that wasn't immediately revoked.
#[test]
fn admin_window_action_applies_staged_request_when_admin_open() {
    let req = test_window_request();
    let window = CommissioningWindow::Open {
        until: Instant::now() + COMMISSIONING_WINDOW_DURATION,
    };
    match admin_window_action(Some(req.clone()), true, &window) {
        AdminWindowAction::Apply(applied) => {
            assert_eq!(applied.discriminator, req.discriminator);
            assert_eq!(applied.timeout_s, req.timeout_s);
        }
        other => panic!("expected Apply, got {other:?}"),
    }
}

/// staged Some + admin_open false (a same-dispatch Revoke raced ahead of
/// the Open) → the stale request must not be applied. With `window`
/// already `Closed` (never `EnhancedOpen`), there is also nothing left
/// to reconcile — `None`, not `Close`.
#[test]
fn admin_window_action_drops_stale_request_without_closing_an_already_closed_window() {
    let req = test_window_request();
    let action = admin_window_action(Some(req), false, &CommissioningWindow::Closed);
    assert!(
        matches!(action, AdminWindowAction::None),
        "expected None, got {action:?}"
    );
}

/// staged None + admin_open false + window `EnhancedOpen` → `Close`: a
/// `RevokeCommissioning` (this dispatch or an earlier one) closed the
/// admin window out from under an already-open ECM window.
#[test]
fn admin_window_action_closes_an_enhanced_open_window_once_admin_window_is_revoked() {
    let window = CommissioningWindow::EnhancedOpen {
        until: Instant::now() + Duration::from_secs(60),
        request: test_window_request(),
    };
    let action = admin_window_action(None, false, &window);
    assert!(
        matches!(action, AdminWindowAction::Close),
        "expected Close, got {action:?}"
    );
}

/// staged None + admin_open true + window `EnhancedOpen` → `None`
/// (steady state: an ECM window that's still open and nothing new
/// staged this iteration — no side effect should fire every single
/// dispatch while a commissioner is just using the window it opened).
#[test]
fn admin_window_action_is_steady_state_none_for_a_still_open_enhanced_window() {
    let window = CommissioningWindow::EnhancedOpen {
        until: Instant::now() + Duration::from_secs(60),
        request: test_window_request(),
    };
    let action = admin_window_action(None, true, &window);
    assert!(
        matches!(action, AdminWindowAction::None),
        "expected None, got {action:?}"
    );
}

// ── RemoveFabric session-drop decision (Task 6) ─────────────────────
//
// `remove_fabric_drops_session` is the one-line decision
// `serve_secured_message`'s RemoveFabric block acts on; a socket-
// harness test proving the whole PASE/CASE→AddNOC→RemoveFabric→session-
// torn-down flow end to end would be disproportionate here (it would
// mostly re-exercise `net::case`/`net::pase`, already covered
// elsewhere) — this unit-tests the decision itself, same rationale as
// the `admin_window_action` tests above.

#[test]
fn remove_fabric_drops_session_when_removed_index_matches_session() {
    assert!(remove_fabric_drops_session(1, 1));
}

#[test]
fn remove_fabric_does_not_drop_session_when_removed_index_differs() {
    assert!(!remove_fabric_drops_session(2, 1));
}

/// A PASE session's `fabric_index` is `0` (no fabric yet) — `0` can
/// never be a real `FabricIndex` (spec §2.5.1, 1-based), so it must
/// never match even a (hypothetically malformed) `removed_fabric_index
/// == 0`.
#[test]
fn remove_fabric_never_drops_a_pase_session() {
    assert!(!remove_fabric_drops_session(0, 0));
}

// ── fail-safe expiry deadline (Task 8) ──────────────────────────────
//
// Brief-scoped: only the `fail_safe_deadline()` Some/None → resolves/
// never-resolves mapping. Whether `expire_fail_safe()` then actually
// rolls back the right fabric is `core::commissioning`'s own test
// territory (Task 7); whether the runtime's `select!` branch drives the
// real mDNS goodbye end-to-end is Task 9's real-hardware gate.

fn fail_safe_test_server() -> CommissioningServer {
    let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
    CommissioningServer::new(dev, crate::core::fabric_store::FabricStore::new())
}

#[tokio::test(start_paused = true)]
async fn fail_safe_expiry_deadline_never_resolves_when_not_armed() {
    let comm_server = fail_safe_test_server();
    assert!(comm_server.fail_safe_deadline().is_none());
    // Same "doesn't hang" technique as
    // `mdns_retry_deadline_never_resolves_when_no_retry_pending`.
    tokio::select! {
        () = fail_safe_expiry_deadline(&comm_server) => {
            panic!("deadline resolved with no fail-safe armed")
        }
        () = tokio::time::sleep(Duration::from_secs(3600)) => {}
    }
}

// Not `start_paused`, unlike the mDNS retry tests above: those poll a
// `tokio::time::Instant` deadline, which a paused runtime's virtual
// clock auto-advances through freely. `fail_safe_deadline()` returns a
// `std::time::Instant` (`core::commissioning` stays runtime-agnostic on
// purpose) — real wall-clock time, which a paused *tokio* clock does
// not advance. `ArmFailSafe`'s `ExpiryLengthSeconds` also bottoms out
// at whole seconds, so this test just eats one real second rather than
// fighting the two clocks.
#[tokio::test]
async fn fail_safe_expiry_deadline_resolves_once_the_armed_window_passes() {
    use mat_controller::commissioning::{encode_arm_fail_safe, CMD_ARM_FAIL_SAFE};

    let comm_server = fail_safe_test_server();
    let (mut gc, _oc, _ac) = comm_server.into_cluster_handlers();
    let mut ctx = InvokeCtx::default();
    gc.invoke(CMD_ARM_FAIL_SAFE, &encode_arm_fail_safe(1, 1), &mut ctx);
    assert!(
        comm_server.fail_safe_deadline().is_some(),
        "ArmFailSafe should have opened a window"
    );

    // Bounded by a much longer sleep so a regression (never resolving)
    // fails the test instead of hanging — mirrors the mDNS retry tests'
    // technique of racing an unambiguous outcome.
    tokio::select! {
        () = fail_safe_expiry_deadline(&comm_server) => {}
        () = tokio::time::sleep(Duration::from_secs(30)) => {
            panic!("fail_safe_expiry_deadline never resolved for an armed window")
        }
    }
    // The window has now lapsed — `fail_safe_deadline` reads back
    // `None` (`FailSafeState::deadline`'s doc: `None` once passed).
    assert!(comm_server.fail_safe_deadline().is_none());
}
