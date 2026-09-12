//! Closed-loop proof of the ACL write validation (spec §11.1.7.1 entry
//! constraints, `access_control::validate_entry`) over a real CASE
//! session: a full replace carrying a CAT subject with version 0 is
//! answered with `CONSTRAINT_ERROR` and leaves the store untouched (the
//! admin can still read the ACL — its automatic Administer entry survived),
//! and the entry shape `mat group grant` writes (Operate / Group auth mode
//! / subject = group id / no targets) is accepted as part of a full
//! replace next to the admin entry.
//!
//! Same direct-drive setup as `acl_enforce.rs` (`tests/support/mod.rs`).
#![cfg(feature = "net")]

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im::{self, ImError};
use mat_controller::session::SessionError;

use mat_device::core::access_control::{
    cat_subject, AUTH_MODE_CASE, AUTH_MODE_GROUP, PRIVILEGE_ADMINISTER, PRIVILEGE_OPERATE,
    PRIVILEGE_VIEW,
};

mod support;
use support::{acl_entries_tlv, commission_directly, device_config, spawn_device};

const ADMIN_NODE_ID: u64 = 660_033;
const GROUP_ID: u16 = 0x0102;
const FABRIC_INDEX: u8 = 1;

#[tokio::test]
async fn invalid_acl_write_is_constraint_error_and_group_grant_shape_is_accepted() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let dev = spawn_device(device_config(store_dir.path().to_path_buf()));

    let fabric =
        CommissioningFabric::generate(0x2233_4455, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(dev.addr, &dev.paa_der, &fabric).await;
    let cfg = support::fast_cfg();

    // 1. Full replace with a valid admin entry *and* a CAT-version-0 entry:
    //    rejected as a whole, CONSTRAINT_ERROR on the attribute path.
    let bad = acl_entries_tlv(
        &[
            (PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, &[ADMIN_NODE_ID]),
            (PRIVILEGE_VIEW, AUTH_MODE_CASE, &[cat_subject(0xABCD_0000)]),
        ],
        FABRIC_INDEX,
    );
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
    let grant = acl_entries_tlv(
        &[
            (PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, &[ADMIN_NODE_ID]),
            (PRIVILEGE_OPERATE, AUTH_MODE_GROUP, &[u64::from(GROUP_ID)]),
        ],
        FABRIC_INDEX,
    );
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

    dev.task.abort();
    let _ = dev.task.await;
}
