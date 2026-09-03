//! Closed-loop proof of the subscribe acceptance rule (spec §8.10, chip
//! `ParseAttributePaths`): a wildcard SubscribeRequest that expands to no
//! attribute the session may read is answered with
//! `StatusResponse(INVALID_ACTION)` instead of an empty subscription — and
//! the rejection leaves the session's *existing* subscription alive.
//!
//! Scenario: after commissioning, demote the admin's automatic ACL entry
//! to Operate (same move as `acl_enforce.rs`), subscribe to OnOff
//! (Operate-readable — succeeds), then subscribe to the AccessControl
//! cluster (Administer-gated, so the wildcard expands to nothing readable
//! — rejected). Finally turn the light on and expect the *first*
//! subscription's report, proving the rejected request did not tear it
//! down.
#![cfg(feature = "net")]

use std::net::SocketAddr;
use std::time::Duration;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im;
use mat_controller::session::SecureSession;
use mat_controller::tlv::{Tag, Writer};

use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config};

const ADMIN_NODE_ID: u64 = 660_033;
const PRIVILEGE_OPERATE: u8 = 3;
const AUTH_MODE_CASE: u8 = 2;
const FABRIC_INDEX: u8 = 1;
/// Generous: the change report is due at MinIntervalFloor = 0.
const REPORT_WAIT: Duration = Duration::from_secs(10);

fn operate_entry_tlv() -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(PRIVILEGE_OPERATE));
    w.put_uint(Tag::Context(2), u64::from(AUTH_MODE_CASE));
    w.start_array(Tag::Context(3));
    w.put_uint(Tag::Anonymous, ADMIN_NODE_ID);
    w.end_container();
    w.put_null(Tag::Context(4));
    w.put_uint(Tag::Context(254), u64::from(FABRIC_INDEX));
    w.end_container();
    w.end_container();
    w.finish()
}

#[tokio::test]
async fn unreadable_wildcard_subscribe_is_invalid_action_and_keeps_the_existing_subscription() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
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

    // Demote to Operate (judged against the pre-replace Administer entry).
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &operate_entry_tlv(),
            None,
            &cfg,
        )
        .await
        .expect("demoting ACL write");

    // 1. OnOff wildcard subscription: Operate may read it — accepted.
    let (sr, _priming) = session
        .subscribe_wildcard(0, 5, false, &[im::CLUSTER_ON_OFF], &cfg)
        .await
        .expect("OnOff subscription must be accepted for an Operate subject");

    // 2. A wildcard path whose only expansion is Administer-gated:
    //    endpoint=wildcard, cluster=AccessControl, attribute=ACL. Operate
    //    may not read it anywhere, so the request has no readable path.
    //    `SecureSession::subscribe_wildcard` can only express cluster-only
    //    paths, so the SubscribeRequest is hand-rolled here (same layout
    //    as `im::encode_subscribe_request`: `{0: KeepSubscriptions, 1:
    //    MinIntervalFloor, 2: MaxIntervalCeiling, 3: AttributeRequests
    //    [AttributePathIB{3: cluster, 4: attribute}], 7: IsFabricFiltered,
    //    255: rev}`) and driven with the session's raw exchange API.
    let req = {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.put_uint(Tag::Context(1), 0);
        w.put_uint(Tag::Context(2), 5);
        w.start_array(Tag::Context(3));
        w.start_list(Tag::Anonymous);
        w.put_uint(Tag::Context(3), u64::from(im::CLUSTER_ACCESS_CONTROL));
        w.put_uint(Tag::Context(4), u64::from(im::ATTR_ACL));
        w.end_container();
        w.end_container();
        w.put_bool(Tag::Context(7), true);
        w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
        w.end_container();
        w.finish()
    };
    let exchange_id = SecureSession::new_exchange_id();
    let piggybacked = session
        .send_reliable(
            exchange_id,
            im::PROTOCOL_ID_IM,
            im::OPCODE_SUBSCRIBE_REQUEST,
            &req,
            &cfg,
        )
        .await
        .expect("SubscribeRequest send");
    let reply = match piggybacked {
        Some(m) => m,
        None => session
            .recv(exchange_id, Duration::from_secs(5))
            .await
            .expect("a reply to the SubscribeRequest"),
    };
    assert_eq!(
        reply.proto.opcode,
        im::OPCODE_STATUS_RESPONSE,
        "an unreadable wildcard subscribe must be answered with a StatusResponse, got opcode {:#04x}",
        reply.proto.opcode
    );
    let status = im::decode_status_response(&reply.payload).expect("decode StatusResponse");
    assert_eq!(status, im::STATUS_INVALID_ACTION, "got {status:#04x}");

    // 3. The first subscription survived the rejection: a change report
    //    still arrives, tagged with its SubscriptionId.
    session
        .invoke(
            support::BRIDGED_EP,
            im::CLUSTER_ON_OFF,
            im::CMD_ON_OFF_ON,
            None,
            &cfg,
        )
        .await
        .expect("On invoke should succeed");
    let rd = session
        .next_subscription_report(REPORT_WAIT, &cfg)
        .await
        .expect("the OnOff subscription must still deliver after the rejected request");
    assert_eq!(rd.subscription_id, Some(sr.subscription_id));
    assert!(
        rd.reports
            .iter()
            .any(|r| r.attribute == Some(im::ATTR_ON_OFF)),
        "change report must carry OnOff: {:?}",
        rd.reports
    );

    device_task.abort();
    let _ = device_task.await;
}
