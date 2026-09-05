//! Closed-loop proof that matv receives groupcast: provision a group over
//! CASE (KeySetWrite / group-key-map / AddGroup / Group ACL entry), then
//! send a real group-session datagram — built with the controller's own
//! `mat_controller::group::build_group_datagram` from the same epoch key —
//! straight to the device's group socket, and read the OnOff attribute back
//! over CASE. Also pins replay rejection, ACL enforcement on the group
//! subject, and that keys + membership survive a device restart.
//!
//! The datagram is sent as plain unicast UDP to `Device::group_local_addr()`
//! (the header still says `Destination::Group`): loopback has no
//! IFF_MULTICAST, so the multicast leg is covered by `task e2e:device:m3`
//! on a real interface instead.
#![cfg(feature = "net")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::case;
use mat_controller::cert::MatterCert;
use mat_controller::commissioning::CommissioningFabric;
use mat_controller::fabric::{
    compressed_fabric_id, derive_group_session_id, derive_ipk_operational,
};
use mat_controller::group::build_group_datagram;
use mat_controller::im;
use mat_controller::kvs::GroupCredentials;
use mat_controller::session::SecureSession;
use mat_controller::tlv::{Tag, Writer};
use mat_controller::transport::{Transport, UdpTransport};

use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config, DEVICE_NODE_ID};

const ADMIN_NODE_ID: u64 = 660_033;
const FABRIC_ID: u64 = 0x2233_4455;
const KEYSET_ID: u16 = 42;
const GROUP_ID: u16 = 0x000A;
const EPOCH_KEY: [u8; 16] = [0x5A; 16];
const PRIVILEGE_OPERATE: u8 = 3;
const PRIVILEGE_ADMINISTER: u8 = 5;
const AUTH_MODE_CASE: u8 = 2;
const AUTH_MODE_GROUP: u8 = 3;

fn put_entry(w: &mut Writer, privilege: u8, auth_mode: u8, subjects: &[u64]) {
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(privilege));
    w.put_uint(Tag::Context(2), u64::from(auth_mode));
    w.start_array(Tag::Context(3));
    for s in subjects {
        w.put_uint(Tag::Anonymous, *s);
    }
    w.end_container();
    w.put_null(Tag::Context(4));
    w.put_uint(Tag::Context(254), 1);
    w.end_container();
}

fn acl_tlv(with_group: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    put_entry(
        &mut w,
        PRIVILEGE_ADMINISTER,
        AUTH_MODE_CASE,
        &[ADMIN_NODE_ID],
    );
    if with_group {
        put_entry(
            &mut w,
            PRIVILEGE_OPERATE,
            AUTH_MODE_GROUP,
            &[u64::from(GROUP_ID)],
        );
    }
    w.end_container();
    w.finish()
}

fn loopback(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        addr.port(),
    )
}

fn group_creds(fabric: &CommissioningFabric) -> GroupCredentials {
    let rcac = MatterCert::parse(&fabric.rcac_tlv).expect("rcac parses");
    let operational = derive_ipk_operational(
        &EPOCH_KEY,
        &compressed_fabric_id(&rcac.pub_key, fabric.fabric_id),
    );
    GroupCredentials {
        session_id: derive_group_session_id(&operational),
        encryption_key: operational,
    }
}

async fn send_group_toggle(creds: &GroupCredentials, to: SocketAddr, counter: u32) {
    let dg = build_group_datagram(
        creds,
        ADMIN_NODE_ID,
        counter,
        0x1234,
        GROUP_ID,
        im::CLUSTER_ON_OFF,
        im::CMD_ON_OFF_TOGGLE,
        None,
    )
    .unwrap();
    let sock = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
    sock.send_to(&dg, to).await.unwrap();
}

/// chip SDK の送信形（security flags P ビット + header 難読化）で toggle を送る。
async fn send_private_group_toggle(creds: &GroupCredentials, to: SocketAddr, counter: u32) {
    use mat_controller::message::{
        Destination, MessageHeader, ProtocolHeader, PROTOCOL_ID_INTERACTION_MODEL,
    };
    let header = MessageHeader {
        session_id: creds.session_id,
        security_flags: 0x01 | mat_device::core::group_privacy::PRIVACY_FLAG,
        message_counter: counter,
        source_node_id: Some(ADMIN_NODE_ID),
        destination: Destination::Group(GROUP_ID),
    };
    let proto = ProtocolHeader {
        initiator: true,
        needs_ack: false,
        acked_counter: None,
        opcode: im::OPCODE_INVOKE_REQUEST,
        exchange_id: 0x1235,
        protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
        vendor_id: None,
    };
    let payload = im::encode_group_invoke_request(im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE, None);
    let mut dg = mat_controller::crypto::seal_message(
        &creds.encryption_key,
        &header,
        &proto,
        &payload,
        ADMIN_NODE_ID,
    )
    .unwrap();
    assert!(mat_device::core::group_privacy::obfuscate_header(
        &mut dg,
        &creds.encryption_key
    ));
    let sock = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
    sock.send_to(&dg, to).await.unwrap();
}

async fn read_onoff(session: &mut SecureSession) -> bool {
    let cfg = support::fast_cfg();
    session
        .read_attribute_json(
            support::BRIDGED_EP,
            im::CLUSTER_ON_OFF,
            im::ATTR_ON_OFF,
            &cfg,
        )
        .await
        .expect("on-off read")
        .as_bool()
        .expect("on-off is a bool")
}

/// Polls until the OnOff attribute equals `want` (the datagram is applied
/// asynchronously by the device loop) — or fails after ~2 s.
async fn expect_onoff(session: &mut SecureSession, want: bool, why: &str) {
    for _ in 0..20 {
        if read_onoff(session).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{why}: on-off did not become {want}");
}

async fn provision_group(session: &mut SecureSession) {
    let cfg = support::fast_cfg();
    let resp = session
        .invoke_for_data(
            0,
            im::CLUSTER_GROUP_KEY_MANAGEMENT,
            im::CMD_KEY_SET_WRITE,
            Some(&im::encode_key_set_write_fields(KEYSET_ID, &EPOCH_KEY)),
            None,
            &cfg,
        )
        .await
        .expect("KeySetWrite");
    assert_eq!(resp.status, im::STATUS_SUCCESS);
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_GROUP_KEY_MANAGEMENT,
            im::ATTR_GROUP_KEY_MAP,
            &im::encode_group_key_map_tlv(&[(GROUP_ID, KEYSET_ID)]),
            None,
            &cfg,
        )
        .await
        .expect("group-key-map write");
    let resp = session
        .invoke_for_data(
            support::BRIDGED_EP,
            im::CLUSTER_GROUPS,
            im::CMD_ADD_GROUP,
            Some(&im::encode_add_group_fields(GROUP_ID, "grp")),
            None,
            &cfg,
        )
        .await
        .expect("AddGroup");
    assert_eq!(resp.status, im::STATUS_SUCCESS);
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &acl_tlv(true),
            None,
            &cfg,
        )
        .await
        .expect("ACL write with the group entry");
}

#[tokio::test]
async fn groupcast_toggle_is_applied_replay_rejected_acl_enforced_and_state_persists() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    let addr = loopback(device.local_addr());
    let group_addr = loopback(device.group_local_addr().expect("group socket bound"));
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der")).expect("paa.der");
    let device_task = tokio::spawn(async move {
        let _ = device.run().await;
    });

    let fabric = CommissioningFabric::generate(FABRIC_ID, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(addr, &paa_der, &fabric).await;
    provision_group(&mut session).await;
    let creds = group_creds(&fabric);

    // 1. Toggle via groupcast: off -> on.
    assert!(!read_onoff(&mut session).await);
    send_group_toggle(&creds, group_addr, 100).await;
    expect_onoff(&mut session, true, "first group toggle").await;

    // 2. Replay (same counter): no second toggle.
    send_group_toggle(&creds, group_addr, 100).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        read_onoff(&mut session).await,
        "a replayed datagram must not toggle again"
    );

    // 3. Next counter toggles again: on -> off.
    send_group_toggle(&creds, group_addr, 101).await;
    expect_onoff(&mut session, false, "second group toggle").await;

    // 3b. P ビット付き（chip SDK の送信形）も適用される: off -> on -> off。
    send_private_group_toggle(&creds, group_addr, 150).await;
    expect_onoff(&mut session, true, "privacy-flagged group toggle").await;
    send_private_group_toggle(&creds, group_addr, 151).await;
    expect_onoff(&mut session, false, "second privacy-flagged group toggle").await;

    // 4. Without the Group ACL entry the datagram is dropped.
    let cfg = support::fast_cfg();
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &acl_tlv(false),
            None,
            &cfg,
        )
        .await
        .expect("ACL write without the group entry");
    send_group_toggle(&creds, group_addr, 152).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !read_onoff(&mut session).await,
        "no Group ACL entry: the groupcast must not be applied"
    );
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &acl_tlv(true),
            None,
            &cfg,
        )
        .await
        .expect("ACL restore");

    // 5. Restart the device on the same store: keys, map and membership come
    //    back from disk and a fresh groupcast is applied.
    device_task.abort();
    let _ = device_task.await;
    let device =
        Device::new(device_config(store_dir.path().to_path_buf())).expect("device restart");
    let addr = loopback(device.local_addr());
    let group_addr = loopback(device.group_local_addr().expect("group socket bound"));
    let device_task = tokio::spawn(async move {
        let _ = device.run().await;
    });

    let admin = fabric.admin_credentials().expect("admin credentials");
    let transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));
    let mut session = None;
    for _ in 0..10 {
        match case::establish(Arc::clone(&transport), addr, &admin, DEVICE_NODE_ID, &cfg).await {
            Ok(s) => {
                session = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    let mut session = session.expect("CASE after restart");
    let table = session
        .read_attribute_json(
            0,
            im::CLUSTER_GROUP_KEY_MANAGEMENT,
            im::ATTR_GROUP_TABLE,
            &cfg,
        )
        .await
        .expect("GroupTable read");
    assert_eq!(
        table.as_array().map(Vec::len),
        Some(1),
        "GroupTable after restart: {table}"
    );
    let map = session
        .read_attribute_json(
            0,
            im::CLUSTER_GROUP_KEY_MANAGEMENT,
            im::ATTR_GROUP_KEY_MAP,
            &cfg,
        )
        .await
        .expect("GroupKeyMap read");
    assert_eq!(
        map.as_array().map(Vec::len),
        Some(1),
        "GroupKeyMap after restart: {map}"
    );
    assert!(
        !read_onoff(&mut session).await,
        "OnOff state itself is not persisted"
    );
    send_group_toggle(&creds, group_addr, 1).await; // fresh replay table after restart
    expect_onoff(&mut session, true, "group toggle after restart").await;

    device_task.abort();
    let _ = device_task.await;
}
