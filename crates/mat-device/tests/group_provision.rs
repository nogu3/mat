//! Task 5: closed-loop proof that `mat group provision`'s wire steps
//! (spec §11.2.7.1 KeySetWrite, §11.2.7.6 group-key-map, §1.3.6.1 AddGroup)
//! all reach the real `Device` runtime through one commissioned CASE
//! session — not just `Node::handle_im` called in isolation (the
//! `core::group_key_management`/`core::groups` unit tests) but through the
//! actual CASE `SecureSession` -> `net::runtime` -> cluster handler path a
//! real controller uses.
//!
//! Task 4 wired `session.peer_node_id()` into every `ReadCtx`/`InvokeCtx`
//! built by `net/runtime.rs` (`subject: session.peer_node_id()` at all 5
//! call sites) so that ACL enforcement judges the *real* CASE peer instead
//! of a stub. This test is the end-to-end confirmation that the whole
//! provisioning sequence still clears that enforcement gate as the
//! commissioning admin (who holds the automatic Administer ACL entry
//! `AclStore::add_case_admin` creates on AddNOC) — KeySetWrite and the
//! group-key-map write both require Manage/Administer per
//! `core::group_key_management`'s access table, and would fail with
//! `STATUS_UNSUPPORTED_ACCESS` if the subject were wrong or zeroed out.
//!
//! Reuses the same direct-drive commissioning setup as `onoff_invoke.rs`
//! (`tests/support/mod.rs`) — no mDNS, controller talks straight to the
//! device's loopback port.
#![cfg(feature = "net")]

use std::net::SocketAddr;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im;
use mat_controller::tlv::{Reader, Tag, Value};

use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config};

const ADMIN_NODE_ID: u64 = 990_011;

const KEYSET_ID: u16 = 0x01AA;
const EPOCH_KEY: [u8; 16] = [0x5A; 16];
const GROUP_ID: u16 = 0x000A;

/// Decodes an `AddGroupResponse` CommandFields struct (spec §1.3.7.2:
/// `{0: Status, 1: GroupID}`) — deliberately hand-rolled here rather than
/// reusing `mat_device::core::groups`'s private decoder, since the whole
/// point of this test is to verify the wire bytes a real controller would
/// have to parse itself.
fn decode_add_group_response(fields_tlv: &[u8]) -> (u8, u16) {
    let mut r = Reader::new(fields_tlv);
    assert!(
        matches!(r.next().unwrap().unwrap().value, Value::StructStart),
        "AddGroupResponse fields must be a struct"
    );
    let mut status = None;
    let mut group_id = None;
    loop {
        let el = r
            .next()
            .unwrap()
            .expect("AddGroupResponse struct must be closed");
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => status = Some(u8::try_from(v).unwrap()),
            (Tag::Context(1), Value::Uint(v)) => group_id = Some(u16::try_from(v).unwrap()),
            _ => {}
        }
    }
    (
        status.expect("AddGroupResponse must carry Status"),
        group_id.expect("AddGroupResponse must carry GroupID"),
    )
}

/// The 4-step `mat group provision` sequence (brief's design), run over one
/// commissioned CASE session with real UDP round-trips end to end:
///
/// 1. `KeySetWrite` (GroupKeyManagement, EP0) — Administer-gated.
/// 2. group-key-map full-replace write (GroupKeyManagement, EP0) —
///    Manage-gated.
/// 3. `AddGroup` (Groups, the bridged endpoint) — Manage-gated (Task 4
///    fix, spec §1.3.5 — see `groups.rs::invoke_privilege`).
/// 4. group-key-map read-back (fabric-filtered) — the written entry must
///    come back with the server-substituted `fabricIndex` (254) attached.
#[tokio::test]
async fn group_provision_sequence_round_trips_over_case_session() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    // Same `[::]` -> `[::1]` substitution as the other `net`-feature tests
    // (see `onoff_invoke.rs`'s comment): `local_addr()` is the wildcard
    // bind address, not a valid send destination.
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

    // 1. KeySetWrite.
    let fields = im::encode_key_set_write_fields(KEYSET_ID, &EPOCH_KEY);
    let resp = session
        .invoke_for_data(
            0,
            im::CLUSTER_GROUP_KEY_MANAGEMENT,
            im::CMD_KEY_SET_WRITE,
            Some(&fields),
            None,
            &cfg,
        )
        .await
        .expect("KeySetWrite should succeed over the commissioning admin's CASE session");
    assert_eq!(resp.status, im::STATUS_SUCCESS);

    // 2. group-key-map write (full replace — this is the device's first
    //    group-key-map entry, so the "final merged list" is just the one
    //    (group, keyset) pair).
    let tlv = im::encode_group_key_map_tlv(&[(GROUP_ID, KEYSET_ID)]);
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_GROUP_KEY_MANAGEMENT,
            im::ATTR_GROUP_KEY_MAP,
            &tlv,
            None,
            &cfg,
        )
        .await
        .expect("group-key-map write should succeed over the commissioning admin's CASE session");

    // 3. AddGroup, on the bridged endpoint (spec §1.3.6.1 — the only
    //    endpoint-scoped step of the four).
    let fields = im::encode_add_group_fields(GROUP_ID, "e2e-group");
    let resp = session
        .invoke_for_data(
            support::BRIDGED_EP,
            im::CLUSTER_GROUPS,
            im::CMD_ADD_GROUP,
            Some(&fields),
            None,
            &cfg,
        )
        .await
        .expect(
            "AddGroup should succeed at the IM level over the commissioning admin's CASE session",
        );
    assert_eq!(resp.status, im::STATUS_SUCCESS);
    let fields_tlv = resp
        .fields_tlv
        .as_deref()
        .expect("AddGroup must reply with AddGroupResponse CommandFields, not a bare status");
    let (add_group_status, returned_group_id) = decode_add_group_response(fields_tlv);
    assert_eq!(
        add_group_status,
        im::STATUS_SUCCESS,
        "AddGroupResponse.Status must be SUCCESS"
    );
    assert_eq!(returned_group_id, GROUP_ID);

    // 4. group-key-map read-back: exactly the one entry we wrote, with a
    //    server-substituted fabricIndex (254) — proof the write actually
    //    landed against this session's fabric, not just that it didn't
    //    error.
    let value = session
        .read_attribute_json(
            0,
            im::CLUSTER_GROUP_KEY_MANAGEMENT,
            im::ATTR_GROUP_KEY_MAP,
            &cfg,
        )
        .await
        .expect(
            "group-key-map read-back should succeed over the commissioning admin's CASE session",
        );
    let arr = value
        .as_array()
        .unwrap_or_else(|| panic!("group-key-map is not a list: {value}"));
    assert_eq!(
        arr.len(),
        1,
        "exactly the one group-key-map entry written in step 2: {value}"
    );
    let entry = arr[0]
        .as_object()
        .unwrap_or_else(|| panic!("group-key-map entry is not an object: {}", arr[0]));
    assert_eq!(
        entry.get("1").and_then(serde_json::Value::as_u64),
        Some(u64::from(GROUP_ID)),
        "groupId (field 1): {entry:?}"
    );
    assert_eq!(
        entry.get("2").and_then(serde_json::Value::as_u64),
        Some(u64::from(KEYSET_ID)),
        "groupKeySetID (field 2): {entry:?}"
    );
    assert_eq!(
        entry.get("254").and_then(serde_json::Value::as_u64),
        Some(1),
        "server-substituted fabricIndex (field 254) — the device's first (and only) fabric: {entry:?}"
    );

    device_task.abort();
    let _ = device_task.await;
}
