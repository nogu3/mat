//! Closed-loop proof of AddNOC's `CaseAdminSubject` check (spec
//! §11.17.6.8.1): a CAT subject with version 0 — the shape that used to
//! install an Administer entry nobody could ever match — is answered with
//! `NOCResponse(InvalidAdminSubject = 0x0B)`, nothing is installed, and the
//! *same* PASE session can retry with a valid subject and commission all
//! the way through CASE + CommissioningComplete.
//!
//! Uses `tests/support/mod.rs`'s split commissioning helpers
//! (`pase_until_add_noc` / `add_noc` / `case_and_complete`) so the AddNOC
//! reply itself is observable.
#![cfg(feature = "net")]

use std::net::SocketAddr;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im;

use mat_device::core::access_control::cat_subject;
use mat_device::device::Device;

mod support;
use support::{add_noc, case_and_complete, device_config, pase_until_add_noc};

const ADMIN_NODE_ID: u64 = 660_033;
/// `NodeOperationalCertStatusEnum::InvalidAdminSubject` (spec §11.17.5.9).
const NOC_STATUS_INVALID_ADMIN_SUBJECT: u8 = 0x0B;

#[tokio::test]
async fn add_noc_with_cat_version_zero_is_rejected_then_retry_succeeds() {
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
    let (mut pase, noc_tlv) = pase_until_add_noc(addr, &paa_der, &fabric).await;

    // CAT(identifier 0xABCD, version 0): syntactically a CAT subject, but
    // version 0 is reserved — no NOC can ever carry it.
    let (status, fabric_index) =
        add_noc(&mut pase, &noc_tlv, &fabric, cat_subject(0xABCD_0000)).await;
    assert_eq!(status, NOC_STATUS_INVALID_ADMIN_SUBJECT);
    assert_eq!(fabric_index, None);

    // Same PASE session, same CSR/root: a node-id subject goes through.
    let (status, fabric_index) = add_noc(&mut pase, &noc_tlv, &fabric, ADMIN_NODE_ID).await;
    assert_eq!(status, 0, "retry with a valid admin subject must succeed");
    assert_eq!(fabric_index, Some(1));

    // And the fabric is fully usable: CASE + CommissioningComplete + an
    // Administer-gated read (ACL) under the automatic admin entry.
    let creds = fabric.admin_credentials().expect("admin credentials");
    let mut session = case_and_complete(addr, &creds).await;
    let cfg = support::fast_cfg();
    let acl = session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await
        .expect("ACL read under the automatic admin entry");
    assert_eq!(
        acl.as_array().map(Vec::len),
        Some(1),
        "exactly one (automatic admin) ACL entry after the retried AddNOC: {acl}"
    );

    device_task.abort();
    let _ = device_task.await;
}
