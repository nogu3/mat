//! Task 12, Step 4: closed-loop proof of the subscription server (spec
//! §8.10) against the real `Device` runtime — priming, a change-driven
//! report, and a keep-alive, all over one commissioned CASE session, with
//! `mat_controller`'s own initiator-side subscription API
//! (`SecureSession::{subscribe_wildcard, next_subscription_report}`) as the
//! judge. That pairing is the point: those two functions were written
//! against real Matter devices (Nature Remo / SwitchBot / Echo-adjacent
//! hubs), so passing them is evidence the device speaks the wire contract
//! a real controller expects, not just a contract this repo agrees with
//! itself on.
//!
//! Reuses the same direct-drive commissioning setup as `onoff_invoke.rs`
//! (`tests/support/mod.rs`) — no mDNS, controller talks straight to the
//! device's loopback port.
#![cfg(feature = "net")]

use std::net::SocketAddr;
use std::time::Duration;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im;

use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config};

const ADMIN_NODE_ID: u64 = 778_899;

/// How long to wait for a device-initiated report. Generous relative to
/// what the device actually promises (a 5s MaxInterval ceiling, so a
/// keep-alive every ~3s) so a slow CI box doesn't turn a working
/// subscription into a flake — a *broken* subscription still fails, it
/// just takes 10s to say so.
const REPORT_WAIT: Duration = Duration::from_secs(10);

/// subscribe → invoke → report → keep-alive, in one session:
///
/// 1. Subscribing to the OnOff cluster primes with OnOff's current value.
/// 2. Turning the light on (an invoke over the same session) makes the
///    device push a report carrying the new value — the actual point of
///    the whole task: a device-initiated exchange, on its own initiative,
///    driven by a state change it noticed itself.
/// 3. With nothing further changing, an *empty* report still arrives
///    within the interval the device promised, carrying the same
///    SubscriptionId — the keep-alive that stops a controller from
///    declaring the subscription dead.
#[tokio::test]
async fn subscribed_controller_gets_priming_a_change_report_and_a_keep_alive() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    // Same `[::]` -> `[::1]` substitution as `onoff_invoke.rs`/
    // `self_commission_live.rs`: `local_addr()` is the wildcard bind
    // address, not a valid send destination.
    let addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        device.local_addr().port(),
    );
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der"))
        .expect("device should have written its PAA DER at Device::new");

    let device_task = tokio::spawn(async move {
        let _ = device.run().await;
    });

    let fabric =
        CommissioningFabric::generate(0x2233_4455, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(addr, &paa_der, &fabric).await;
    let cfg = support::fast_cfg();

    // 1. Subscribe, narrowed to the OnOff cluster (endpoint/attribute
    //    wildcard). Priming must already carry the attribute's value.
    let (sr, priming) = session
        .subscribe_wildcard(0, 5, false, &[im::CLUSTER_ON_OFF], &cfg)
        .await
        .expect("subscribe should complete with a SubscribeResponse");
    assert!(
        sr.max_interval_s <= 5,
        "device must not answer with a MaxInterval above the requested ceiling, got {}",
        sr.max_interval_s
    );
    let priming_onoff = priming
        .iter()
        .flat_map(|m| &m.reports)
        .find(|r| r.attribute == Some(im::ATTR_ON_OFF))
        .expect("priming must report the subscribed OnOff attribute");
    assert_eq!(
        priming_onoff.data,
        Some(serde_json::json!(false)),
        "the light starts off"
    );
    for chunk in &priming {
        assert_eq!(
            chunk.subscription_id,
            Some(sr.subscription_id),
            "every priming chunk carries the SubscriptionId"
        );
    }

    // 2. Change the state through the same session — the device notices
    //    the change itself and pushes a report for it.
    session
        .invoke(1, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None, &cfg)
        .await
        .expect("On invoke should succeed");

    let rd = session
        .next_subscription_report(REPORT_WAIT, &cfg)
        .await
        .expect("the device must report the OnOff change it just made");
    assert_eq!(rd.subscription_id, Some(sr.subscription_id));
    let reported = rd
        .reports
        .iter()
        .find(|r| r.attribute == Some(im::ATTR_ON_OFF))
        .expect("the change report must carry OnOff");
    assert_eq!(reported.data, Some(serde_json::json!(true)));

    // 3. Nothing changes from here — the next report is the empty
    //    keep-alive, still tagged with this subscription.
    let rd = session
        .next_subscription_report(REPORT_WAIT, &cfg)
        .await
        .expect("a keep-alive must arrive within the promised MaxInterval");
    assert_eq!(rd.subscription_id, Some(sr.subscription_id));
    assert!(
        rd.reports.is_empty(),
        "nothing changed, so the keep-alive must carry no reports: {:?}",
        rd.reports
    );

    device_task.abort();
    let _ = device_task.await;
}
