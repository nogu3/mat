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

use std::net::SocketAddr;
use std::time::Duration;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im::{self, EventPathIn, EventReport, SubscribeSpec};

use mat_device::core::bridge::DeviceKind;
use mat_device::core::stimulus::{PressKind, Stimulus};
use mat_device::device::{Device, VirtualDeviceConfig};
use mat_device::net::stimulus::StimulusApplyError;

mod support;
use support::{commission_directly, device_config_with};

const ADMIN_NODE_ID: u64 = 778_900;

/// デバイス発レポートの待ち時間。デバイスが約束する間隔（MaxInterval 上限
/// 5 秒）に対して十分に緩く取る — 遅い CI で「動いている購読」が flake に
/// ならないため。壊れた購読はそれでも（10 秒かけて）落ちる。
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
    let device = Device::new(device_config_with(
        store_dir.path().to_path_buf(),
        devices(),
    ))
    .expect("device new");
    // `subscribe_loop.rs` と同じ `[::]` -> `[::1]` 置換: `local_addr()` は
    // ワイルドカードの bind アドレスで、送信先としては使えない。
    let addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        device.local_addr().port(),
    );
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der"))
        .expect("device should have written its PAA DER at Device::new");
    let handle = device.stimulus_handle();

    let device_task = tokio::spawn(async move {
        let _ = device.run().await;
    });

    let fabric =
        CommissioningFabric::generate(0x2233_4466, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(addr, &paa_der, &fabric).await;
    let cfg = support::fast_cfg();

    // 1. イベント wildcard(urgent) + 属性は booleanstate だけ。priming に
    //    イベントは無い（まだ何も起きていない新品のデバイス）。
    let spec = SubscribeSpec {
        min_interval_floor_s: 0,
        max_interval_ceiling_s: 5,
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
    let out = handle
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
    handle
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
        handle.apply("nope", Stimulus::SetState(true)).await,
        Err(StimulusApplyError::UnknownDevice(_))
    ));
    assert!(matches!(
        handle.apply("btn", Stimulus::SetState(true)).await,
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

    device_task.abort();
    let _ = device_task.await;
}
