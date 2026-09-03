//! Closed-loop proof of the ACL write validation (spec §11.1.7.1 entry
//! constraints, `access_control::validate_entry`) over a real CASE
//! session: a full replace carrying a CAT subject with version 0 is
//! answered with `CONSTRAINT_ERROR` and leaves the store untouched (the
//! admin can still read the ACL — its automatic Administer entry survived),
//! and the entry shape `mat group grant` writes (Operate / Group auth mode
//! / subject = group id / no targets) is accepted as a list append.
//!
//! Same direct-drive setup as `acl_enforce.rs` (`tests/support/mod.rs`).
#![cfg(feature = "net")]

use std::net::SocketAddr;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im::{self, ImError};
use mat_controller::session::SessionError;
use mat_controller::tlv::{Tag, Writer};

use mat_device::core::access_control::cat_subject;
use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config};

const ADMIN_NODE_ID: u64 = 660_033;
const GROUP_ID: u16 = 0x0102;

/// spec §11.1.7.1 enums, mirrored as in `acl_enforce.rs`.
const PRIVILEGE_VIEW: u8 = 1;
const PRIVILEGE_OPERATE: u8 = 3;
const PRIVILEGE_ADMINISTER: u8 = 5;
const AUTH_MODE_CASE: u8 = 2;
const AUTH_MODE_GROUP: u8 = 3;
const FABRIC_INDEX: u8 = 1;

/// One `AccessControlEntryStruct` (`{1: privilege, 2: authMode, 3:
/// subjects, 4: null, 254: fabricIndex}`) into `w`.
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
    w.put_uint(Tag::Context(254), u64::from(FABRIC_INDEX));
    w.end_container();
}

/// Full-replace Data TLV: an array of entries.
fn entries_tlv(entries: &[(u8, u8, &[u64])]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (privilege, auth_mode, subjects) in entries {
        put_entry(&mut w, *privilege, *auth_mode, subjects);
    }
    w.end_container();
    w.finish()
}

#[tokio::test]
async fn invalid_acl_write_is_constraint_error_and_group_grant_shape_is_accepted() {
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

    // 1. Full replace with a valid admin entry *and* a CAT-version-0 entry:
    //    rejected as a whole, CONSTRAINT_ERROR on the attribute path.
    let bad = entries_tlv(&[
        (PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, &[ADMIN_NODE_ID]),
        (PRIVILEGE_VIEW, AUTH_MODE_CASE, &[cat_subject(0xABCD_0000)]),
    ]);
    let res = session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &bad,
            None,
            &cfg,
        )
        .await;
    match res {
        Err(SessionError::Im(ImError::AttributeStatus(status))) => assert_eq!(
            status,
            im::STATUS_CONSTRAINT_ERROR,
            "CAT version 0 subject must be CONSTRAINT_ERROR, got {status:#04x}"
        ),
        other => panic!("expected AttributeStatus(CONSTRAINT_ERROR), got {other:?}"),
    }

    // 2. The store is untouched: the automatic admin entry still grants
    //    Administer, so the ACL read goes through and shows exactly it.
    let acl = session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await
        .expect("ACL read must still succeed — the rejected write must not have replaced anything");
    assert_eq!(
        acl.as_array().map(Vec::len),
        Some(1),
        "store must be unchanged: {acl}"
    );

    // 3. `mat group grant` shape (Operate / Group / [group id] / no
    //    targets) next to the admin entry, as a full replace: accepted.
    //    (`SecureSession::write_attribute_tlv` has no list-append form, so
    //    the replace carries both entries — the same wire shape `mat group
    //    grant` ends up writing after its read-merge-write.)
    let grant = entries_tlv(&[
        (PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, &[ADMIN_NODE_ID]),
        (PRIVILEGE_OPERATE, AUTH_MODE_GROUP, &[u64::from(GROUP_ID)]),
    ]);
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &grant,
            None,
            &cfg,
        )
        .await
        .expect("the group-grant entry shape must be accepted");
    let acl = session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await
        .expect("ACL read after the grant-shaped replace");
    assert_eq!(
        acl.as_array().map(Vec::len),
        Some(2),
        "admin + group entries: {acl}"
    );

    device_task.abort();
    let _ = device_task.await;
}
