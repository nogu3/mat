//! イベント購読の閉ループ: `switch` + `contact-sensor` を持つ Device に対し、
//! mat-controller の `subscribe`（EventRequests wildcard urgent）で購読し、刺激
//! （ボタン押下 / 開閉）が EventReport として届くことを判定する。属性購読の
//! 無退行は既存 `subscribe_loop.rs` が担当。
//!
//! `subscribe_loop.rs` と同じ direct-drive セットアップ（`tests/support/mod.rs`）
//! — mDNS 無し、コントローラはデバイスの loopback ポートへ直接話す。判定役が
//! mat-controller 自身の initiator 側 API なのも同じ理由で、実機に対して書かれた
//! コードが通ることがワイヤ契約の証拠になる。
#![cfg(feature = "net")]

use std::time::Duration;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im::{self, EventPathIn, EventReport, SubscribeSpec};

use mat_device::core::bridge::DeviceKind;
use mat_device::core::stimulus::{PressKind, Stimulus};
use mat_device::device::VirtualDeviceConfig;
use mat_device::net::stimulus::StimulusApplyError;

mod support;
use support::{commission_directly, device_config_with, spawn_device};

const ADMIN_NODE_ID: u64 = 778_900;

/// デバイス発レポートの待ち時間。刺激で起きる urgent レポートは
/// MinInterval（0 秒）で出るので 10 秒は十分に緩い — 遅い CI で「動いて
/// いる購読」が flake にならないため。壊れた購読はそれでも（10 秒かけて）
/// 落ちる。MaxInterval 上限は 60 秒にしてあるので（`spec`）、この待ちの
/// 間に keep-alive が割り込んで判定対象のレポートをずらすことはない。
const REPORT_WAIT: Duration = Duration::from_secs(10);

/// bridged endpoint は宣言順に EP2 から（EP0 = root、EP1 = Aggregator）。
const SWITCH_EP: u16 = 2;
const CONTACT_EP: u16 = 3;

fn devices() -> Vec<VirtualDeviceConfig> {
    vec![
        VirtualDeviceConfig {
            id: "btn".into(),
            kind: DeviceKind::Switch,
            name: "Button".into(),
        },
        VirtualDeviceConfig {
            id: "door".into(),
            kind: DeviceKind::ContactSensor,
            name: "Door".into(),
        },
    ]
}

/// EventReport 列を `(endpoint, cluster, event)` の並びへ — 順序も含めて
/// 一致を見たいので `Vec` のまま比較する。`Status` 側は落とす（届いた
/// データイベントの並びが判定対象）。
fn event_ids(events: &[EventReport]) -> Vec<(u16, u32, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            EventReport::Data(d) => Some((d.endpoint, d.cluster, d.event)),
            EventReport::Status { .. } => None,
        })
        .collect()
}

/// 刺激 → イベント購読の閉ループ、1 セッションで:
///
/// 1. イベント wildcard(urgent) + 属性は BooleanState だけの購読。
/// 2. ボタン短押し → InitialPress + ShortRelease が EventNumber 昇順で届く。
/// 3. 開閉 → 同一 ReportData に属性変化とイベントが同居する。
/// 4. 刺激のエラー経路（未知デバイス / そのクラスタが受けない刺激）。
/// 5. EventMin 付き再購読で priming イベントが絞られる。
/// 6. イベント path を絞った購読（クラスタ絞り込み）。
#[tokio::test]
async fn button_press_and_contact_change_arrive_as_events() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let dev = spawn_device(device_config_with(
        store_dir.path().to_path_buf(),
        devices(),
    ));

    let fabric =
        CommissioningFabric::generate(0x2233_4466, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(dev.addr, &dev.paa_der, &fabric).await;
    let cfg = support::fast_cfg();

    // 1. イベント wildcard(urgent) + 属性は booleanstate だけ。priming に
    //    イベントは無い（まだ何も起きていない新品のデバイス）。
    // MaxInterval は上限いっぱい（60 秒 = デバイス側 `MAX_MAX_INTERVAL_S`）:
    // このテストは keep-alive を待たないので、判定対象のレポートの間に
    // 無関係な空レポートが割り込む余地を消しておく。刺激由来のレポートは
    // urgent = MinInterval(0) レジームなので待ち時間には影響しない。
    let spec = SubscribeSpec {
        min_interval_floor_s: 0,
        max_interval_ceiling_s: 60,
        keep_subscriptions: false,
        clusters: vec![im::CLUSTER_BOOLEAN_STATE],
        event_paths: vec![EventPathIn::WILDCARD_URGENT],
        event_min: None,
    };
    let o = session.subscribe(&spec, &cfg).await.expect("subscribe");
    assert!(
        o.priming_events.is_empty(),
        "fresh device has no events: {:?}",
        o.priming_events
    );
    assert!(
        o.priming
            .iter()
            .flat_map(|m| &m.reports)
            .any(|r| r.attribute == Some(im::ATTR_BS_STATE_VALUE)),
        "priming must carry the subscribed BooleanState attribute: {:?}",
        o.priming
    );

    // 2. 短押し → InitialPress + ShortRelease が番号昇順で届く。
    let out = dev
        .stimulus
        .apply("btn", Stimulus::Press(PressKind::Short))
        .await
        .expect("press applied");
    assert_eq!(out.event_numbers.len(), 2);
    let rep = session
        .next_subscription_report_full(REPORT_WAIT, &cfg)
        .await
        .expect("press report");
    assert_eq!(rep.data.subscription_id, Some(o.response.subscription_id));
    assert_eq!(
        event_ids(&rep.events),
        vec![
            (
                SWITCH_EP,
                im::CLUSTER_SWITCH,
                im::EVENT_SWITCH_INITIAL_PRESS
            ),
            (
                SWITCH_EP,
                im::CLUSTER_SWITCH,
                im::EVENT_SWITCH_SHORT_RELEASE
            ),
        ]
    );
    let nums: Vec<u64> = rep
        .events
        .iter()
        .filter_map(|e| match e {
            EventReport::Data(d) => Some(d.event_number),
            EventReport::Status { .. } => None,
        })
        .collect();
    assert_eq!(nums, out.event_numbers);
    assert!(
        rep.data.reports.is_empty(),
        "a press changes no subscribed attribute: {:?}",
        rep.data.reports
    );

    // 3. 開閉 → 同一 report に StateValue 属性と StateChange イベント。
    dev.stimulus
        .apply("door", Stimulus::SetState(true))
        .await
        .expect("state applied");
    let rep = session
        .next_subscription_report_full(REPORT_WAIT, &cfg)
        .await
        .expect("contact report");
    assert!(
        rep.data
            .reports
            .iter()
            .any(|r| r.endpoint == Some(CONTACT_EP)
                && r.attribute == Some(im::ATTR_BS_STATE_VALUE)
                && r.data == Some(serde_json::json!(true))),
        "the contact change must be reported as an attribute too: {:?}",
        rep.data.reports
    );
    assert_eq!(
        event_ids(&rep.events),
        vec![(
            CONTACT_EP,
            im::CLUSTER_BOOLEAN_STATE,
            im::EVENT_BS_STATE_CHANGE
        )]
    );
    let last = match &rep.events[0] {
        EventReport::Data(d) => {
            assert_eq!(d.data, Some(serde_json::json!({"0": true})));
            d.event_number
        }
        other => unreachable!("expected a data event, got {other:?}"),
    };

    // 4. 刺激エラー: 名前が引けない（チャネル層）／このクラスタは受けない
    //    （core 側の判断）。
    assert!(matches!(
        dev.stimulus.apply("nope", Stimulus::SetState(true)).await,
        Err(StimulusApplyError::UnknownDevice(_))
    ));
    assert!(matches!(
        dev.stimulus.apply("btn", Stimulus::SetState(true)).await,
        Err(StimulusApplyError::Node(_))
    ));

    // 5. 再購読: event_min = last+1 → priming events 空。event_min 無し →
    //    ログ全量 3 件（InitialPress / ShortRelease / StateChange）。
    let o2 = session
        .subscribe(
            &SubscribeSpec {
                event_min: Some(last + 1),
                ..spec.clone()
            },
            &cfg,
        )
        .await
        .expect("resubscribe");
    assert!(
        o2.priming_events.is_empty(),
        "EventMin past the log must prime with no events: {:?}",
        o2.priming_events
    );
    let o3 = session
        .subscribe(
            &SubscribeSpec {
                event_min: None,
                ..spec.clone()
            },
            &cfg,
        )
        .await
        .expect("resubscribe all");
    assert_eq!(o3.priming_events.len(), 3);
    assert_eq!(
        event_ids(&o3.priming_events)[2],
        (
            CONTACT_EP,
            im::CLUSTER_BOOLEAN_STATE,
            im::EVENT_BS_STATE_CHANGE
        )
    );

    // 6. イベント path を Switch クラスタへ絞った購読も受理され、絞り込みが
    //    効く（押下の 2 件だけで、StateChange は落ちる）。`clusters: vec![]`
    //    は AttributeRequests が full wildcard になる仕様
    //    （`encode_subscribe_request_full`）なので、これは「属性 path が空」の
    //    検証ではなく「イベント絞り込み購読」の検証。
    let o4 = session
        .subscribe(
            &SubscribeSpec {
                clusters: vec![],
                event_paths: vec![EventPathIn {
                    cluster: Some(im::CLUSTER_SWITCH),
                    ..EventPathIn::WILDCARD_URGENT
                }],
                event_min: Some(0),
                ..spec.clone()
            },
            &cfg,
        )
        .await
        .expect("events-only subscribe");
    assert_eq!(o4.priming_events.len(), 2);

    // 7. 1 レポートに載らない量（Multi(3) 押下 3 回 = 27 件）でも、欠番なく
    //    連続したレポートに分かれて届く（`send_subscription_report` の
    //    予算内キャップ + 積み残しがあるうちは urgent を維持する規則）。
    //
    //    刺激の注入は**別タスク**でないといけない: デバイスのループは
    //    レポートの StatusResponse を待っている間 stimulus チャネルを
    //    読まないので、「apply を 3 回直列に await してから受信」だと
    //    apply#2 とレポート#1 が互いを待って詰まる（5 秒後にデバイスが
    //    購読を落とす）。実運用の刺激は物理イベントで、受信側の都合と
    //    同期しない — その形をテストでも守る。
    let presser = tokio::spawn(async move {
        for _ in 0..3 {
            dev.stimulus
                .apply("btn", Stimulus::Press(PressKind::Multi(3)))
                .await
                .expect("multi press applied");
        }
    });
    let mut numbers: Vec<u64> = Vec::new();
    let mut reports = 0usize;
    while numbers.len() < 27 {
        let rep = session
            .next_subscription_report_full(REPORT_WAIT, &cfg)
            .await
            .expect("multi press report");
        reports += 1;
        numbers.extend(rep.events.iter().filter_map(|e| match e {
            EventReport::Data(d) => Some(d.event_number),
            EventReport::Status { .. } => None,
        }));
    }
    assert_eq!(numbers.len(), 27, "余計なイベントは来ない: {numbers:?}");
    assert!(
        reports >= 2,
        "27 件は 1 レポートの予算に収まらないはず（キャップが効いていない）: {reports}"
    );
    assert!(
        numbers.windows(2).all(|w| w[1] == w[0] + 1),
        "EventNumber は昇順・欠番なし: {numbers:?}"
    );
    presser.await.expect("presser task");

    dev.task.abort();
    let _ = dev.task.await;
}
