//! Closed-loop proof that ACL enforcement honors CASE-Authenticated-Tag
//! subjects (spec §6.6.2.1.2) for the *real* CASE peer — the CATs read off
//! the Sigma3 NOC by `net::case` (`SecureSession::peer_cats`) and wired
//! into every `ReadCtx`/`InvokeCtx` by `net/runtime.rs`, not a stub.
//!
//! The scenario is the Apple Home shape that used to lock the admin out:
//! AddNOC's `CaseAdminSubject` is a **CAT subject**
//! (`0xFFFF_FFFD_hhhh_vvvv`), so the automatic Administer entry
//! (`AclStore::add_case_admin`) names no node id at all — the admin's later
//! CASE sessions can only be authorized through the CATs in its NOC.
//! Before CAT support, `AclStore::check` compared node ids only, so the
//! admin's very first operational request (CommissioningComplete,
//! Administer-gated) was refused and commissioning never finished.
//!
//! Steps, each judged against the entry the previous one installed:
//!
//! 1. Commission with `CaseAdminSubject = CAT(id 0xABCD, version 2)` and
//!    an admin NOC carrying `CAT(0xABCD, version 3)` — a *newer* version
//!    than the entry asks for, so this also proves "at or above", not
//!    equality. CommissioningComplete + ACL read + OnOff Toggle succeed.
//! 2. Replace the ACL with an entry for `CAT(0xABCD, version 1)` (older
//!    than the NOC's): still authorized.
//! 3. Replace it with `CAT(0xABCD, version 4)` (newer than the NOC's): the
//!    write itself goes through (judged against step 2's entry), but from
//!    then on the admin is rejected with `UNSUPPORTED_ACCESS` — the
//!    fails-closed side of the version rule.
//!
//! Reuses the direct-drive commissioning setup of `onoff_invoke.rs` /
//! `acl_enforce.rs` (`tests/support/mod.rs`) via `commission_directly_as`,
//! which lets the test choose the `CaseAdminSubject` and the admin's CASE
//! credentials.
#![cfg(feature = "net")]

use std::net::SocketAddr;

use mat_controller::case::{eph_pub_bytes, random_p256_secret};
use mat_controller::cert::{generate_rcac, issue_noc_with_cats, verify_noc_chain};
use mat_controller::commissioning::CommissioningFabric;
use mat_controller::fabric::{compressed_fabric_id, derive_ipk_operational, FabricCredentials};
use mat_controller::im::{self, ImError};
use mat_controller::kvs::SelfIssueMaterials;
use mat_controller::session::{SecureSession, SessionError};
use mat_controller::tlv::{Tag, Writer};

use mat_device::core::access_control::cat_subject;
use mat_device::device::Device;

mod support;
use support::{commission_directly_as, device_config};

const ADMIN_NODE_ID: u64 = 660_033;
const FABRIC_ID: u64 = 0x2233_4455;
const CAT_ID: u32 = 0xABCD;

/// `AccessControlEntryPrivilegeEnum` / `AccessControlEntryAuthModeEnum`
/// values (spec §11.1.7.1), mirrored as in `acl_enforce.rs`.
const PRIVILEGE_ADMINISTER: u8 = 5;
const AUTH_MODE_CASE: u8 = 2;
const FABRIC_INDEX: u8 = 1;

/// CAT value for identifier `CAT_ID` at `version`.
fn cat(version: u32) -> u32 {
    (CAT_ID << 16) | version
}

/// A fabric whose admin's CASE NOC carries `admin_cats`: the plain
/// `CommissioningFabric::generate` can only self-issue a CAT-less admin
/// NOC, so this builds the root itself and issues the admin NOC by hand.
fn fabric_with_cat_admin(admin_cats: &[u32]) -> (CommissioningFabric, FabricCredentials) {
    let (rcac, root_private_key) = generate_rcac().expect("generate rcac");
    let mut ipk_epoch = [0u8; 16];
    getrandom::getrandom(&mut ipk_epoch).expect("os rng");
    let ipk_operational =
        derive_ipk_operational(&ipk_epoch, &compressed_fabric_id(&rcac.pub_key, FABRIC_ID));

    let op_secret = random_p256_secret();
    let op_public_key = eph_pub_bytes(&op_secret);
    let op_private_key: [u8; 32] = op_secret.to_bytes().into();
    let noc = issue_noc_with_cats(
        &op_public_key,
        ADMIN_NODE_ID,
        FABRIC_ID,
        &rcac,
        &root_private_key,
        &[0x11],
        admin_cats,
    )
    .expect("issue cat-bearing admin noc");
    verify_noc_chain(&noc, None, &rcac).expect("admin noc chains to its root");

    let fabric = CommissioningFabric::from_materials(
        SelfIssueMaterials {
            rcac: rcac.to_tlv(),
            root_private_key,
            ipk_operational,
            node_id: ADMIN_NODE_ID,
            fabric_id: FABRIC_ID,
        },
        ipk_epoch,
    );
    let creds = FabricCredentials {
        rcac_tlv: rcac.to_tlv(),
        icac_tlv: None,
        noc_tlv: noc.to_tlv(),
        op_public_key,
        op_private_key,
        ipk_operational,
        node_id: ADMIN_NODE_ID,
        fabric_id: FABRIC_ID,
        root_public_key: rcac.pub_key,
    };
    (fabric, creds)
}

/// One-entry Administer/CASE ACL array for `subject` (same wire shape as
/// `acl_enforce.rs`'s `encode_single_acl_entry_tlv`).
fn admin_entry_for(subject: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(PRIVILEGE_ADMINISTER));
    w.put_uint(Tag::Context(2), u64::from(AUTH_MODE_CASE));
    w.start_array(Tag::Context(3));
    w.put_uint(Tag::Anonymous, subject);
    w.end_container();
    w.put_null(Tag::Context(4));
    w.put_uint(Tag::Context(254), u64::from(FABRIC_INDEX));
    w.end_container();
    w.end_container();
    w.finish()
}

/// The single subject of the single ACL entry the device currently holds.
async fn read_sole_acl_subject(session: &mut SecureSession) -> u64 {
    let acl = session
        .read_attribute_json(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &support::fast_cfg(),
        )
        .await
        .expect("ACL read should succeed while the admin is authorized");
    let entries = acl
        .as_array()
        .unwrap_or_else(|| panic!("ACL is not a list: {acl}"));
    assert_eq!(entries.len(), 1, "exactly one ACL entry expected: {acl}");
    let subjects = entries[0]
        .get("3")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("ACL entry has no subjects array: {}", entries[0]));
    assert_eq!(subjects.len(), 1, "exactly one subject expected: {acl}");
    subjects[0]
        .as_u64()
        .unwrap_or_else(|| panic!("subject is not a u64: {acl}"))
}

#[tokio::test]
async fn cat_case_admin_subject_authorizes_the_admin_by_its_nocs_cat() {
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

    // Step 1: CaseAdminSubject = CAT v2, admin NOC carries CAT v3.
    let (fabric, creds) = fabric_with_cat_admin(&[cat(3)]);
    let mut session =
        commission_directly_as(addr, &paa_der, &fabric, cat_subject(cat(2)), &creds).await;
    let cfg = support::fast_cfg();

    assert_eq!(
        read_sole_acl_subject(&mut session).await,
        cat_subject(cat(2)),
        "the automatic admin entry names the CAT subject AddNOC was given, not a node id"
    );
    let resp = session
        .invoke_for_data(
            support::BRIDGED_EP,
            im::CLUSTER_ON_OFF,
            im::CMD_ON_OFF_TOGGLE,
            None,
            None,
            &cfg,
        )
        .await
        .expect("OnOff Toggle should succeed for the CAT-authorized admin");
    assert_eq!(resp.status, im::STATUS_SUCCESS);

    // Step 2: an entry asking for an *older* version than the NOC carries
    // still matches.
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &admin_entry_for(cat_subject(cat(1))),
            None,
            &cfg,
        )
        .await
        .expect("ACL replace (CAT v1) should succeed under the CAT v2 entry");
    assert_eq!(
        read_sole_acl_subject(&mut session).await,
        cat_subject(cat(1))
    );

    // Step 3: an entry asking for a *newer* version than the NOC carries
    // locks the admin out from the next request on.
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &admin_entry_for(cat_subject(cat(4))),
            None,
            &cfg,
        )
        .await
        .expect("ACL replace (CAT v4) should succeed under the CAT v1 entry");
    match session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await
    {
        Err(SessionError::Im(ImError::AttributeStatus(status))) => assert_eq!(
            status,
            im::STATUS_UNSUPPORTED_ACCESS,
            "ACL read under a CAT entry newer than the NOC's must be UNSUPPORTED_ACCESS, got {status:#04x}"
        ),
        other => panic!(
            "expected ACL read to be rejected with AttributeStatus(UNSUPPORTED_ACCESS) \
             once the entry's CAT version exceeds the NOC's, got: {other:?}"
        ),
    }

    device_task.abort();
    let _ = device_task.await;
}
