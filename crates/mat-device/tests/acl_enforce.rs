//! Task 5: closed-loop proof that ACL enforcement (`AclStore::check`,
//! wired into `Node::handle_read`/`handle_write`/`handle_invoke` by an
//! earlier task) judges the *real* CASE peer's operational Node ID —
//! `session.peer_node_id()`, wired into every `ReadCtx`/`InvokeCtx` built
//! by `net/runtime.rs` — rather than a stub or a zeroed-out subject.
//!
//! Scenario (brief's design memo, `Groups` now Manage-gated per Task 4's
//! fix doesn't change this): after commissioning, the admin's own
//! Administer ACL entry (`AclStore::add_case_admin`, auto-created on
//! AddNOC) is used to **replace itself** with an Operate-only entry for
//! the same subject. That replacing write goes through (it's judged
//! against the *pre*-replace Administer entry). From then on:
//!
//! - ACL read (Administer-gated, spec §11.1.5) must be rejected with
//!   `STATUS_UNSUPPORTED_ACCESS` (0x7E) — the admin no longer holds
//!   Administer.
//! - OnOff Toggle (Operate-gated, the default in
//!   `ClusterHandler::invoke_privilege`) must still succeed — the demoted
//!   entry still grants Operate.
//!
//! This only proves enforcement is live for the *real* session identity if
//! the ACL read genuinely turns from allowed to rejected around the
//! replace — a subject that was wrong (e.g. always 0, or the PASE
//! fabric-0 bypass leaking into CASE) would make the read either always
//! fail (no admin entry ever matches) or never fail (fabric 0 bypasses
//! `AclStore::check` unconditionally), not flip exactly at the replace.
//!
//! Reuses the same direct-drive commissioning setup as `onoff_invoke.rs`
//! (`tests/support/mod.rs`) — no mDNS, controller talks straight to the
//! device's loopback port.
#![cfg(feature = "net")]

use std::net::SocketAddr;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im::{self, ImError};
use mat_controller::session::SessionError;
use mat_controller::tlv::{Tag, Writer};

use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config};

const ADMIN_NODE_ID: u64 = 660_033;

/// `AccessControlEntryPrivilegeEnum` values (spec §11.1.7.1) — mirrored
/// here rather than imported since `mat_device::core::access_control`'s
/// constants are crate-private (`pub(crate)`), and `mat_core::acl` only
/// defines `PRIVILEGE_OPERATE` (the one value the group-provision path
/// needs). The device's own wire-form doc for this encoding lives at
/// `mat_device::core::access_control`'s `write_acl_entry`.
const PRIVILEGE_OPERATE: u8 = 3;
const AUTH_MODE_CASE: u8 = 2;

/// The fabric index `commission_directly` always lands on — it's the
/// device's first (and only) fabric in every one of these tests
/// (`support::commission_directly` asserts `AddNOC`'s returned
/// `fabric_index == Some(1)`).
const FABRIC_INDEX: u8 = 1;

/// One-entry `AccessControlEntryStruct` array Data TLV (spec §11.1.7.1):
/// `{1: privilege, 2: authMode, 3: subjects(array), 4: targets(null), 254:
/// fabricIndex}` — same wire shape as
/// `mat_device::core::access_control::write_acl_entry` /
/// `mat_native::ops::encode_acl_entries_tlv`, hand-rolled here (rather than
/// imported) because both are crate-private to their own crates.
fn encode_single_acl_entry_tlv(
    privilege: u8,
    auth_mode: u8,
    subject: u64,
    fabric_index: u8,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(privilege));
    w.put_uint(Tag::Context(2), u64::from(auth_mode));
    w.start_array(Tag::Context(3));
    w.put_uint(Tag::Anonymous, subject);
    w.end_container();
    w.put_null(Tag::Context(4));
    w.put_uint(Tag::Context(254), u64::from(fabric_index));
    w.end_container();
    w.end_container();
    w.finish()
}

#[tokio::test]
async fn demoting_admin_acl_entry_blocks_administer_but_not_operate() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    // Same `[::]` -> `[::1]` substitution as the other `net`-feature tests.
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

    // Baseline: the automatic post-AddNOC Administer entry (`AclStore::
    // add_case_admin`) lets the admin read the ACL attribute it holds —
    // and specifically an entry naming *this* session's subject
    // (`ADMIN_NODE_ID`), not a stub or a zeroed-out identity.
    let before = session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await
        .expect("ACL read should succeed while the admin still holds Administer");
    let before_arr = before
        .as_array()
        .unwrap_or_else(|| panic!("ACL is not a list: {before}"));
    assert_eq!(
        before_arr.len(),
        1,
        "only the automatic AddNOC admin entry exists yet: {before}"
    );
    let before_entry = before_arr[0]
        .as_object()
        .unwrap_or_else(|| panic!("ACL entry is not an object: {}", before_arr[0]));
    assert_eq!(
        before_entry.get("1").and_then(serde_json::Value::as_u64),
        Some(5), // PRIVILEGE_ADMINISTER
        "the automatic admin entry starts at Administer: {before_entry:?}"
    );
    let subjects = before_entry
        .get("3")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("ACL entry has no subjects array: {before_entry:?}"));
    assert_eq!(
        subjects.first().and_then(serde_json::Value::as_u64),
        Some(ADMIN_NODE_ID),
        "the admin entry's subject is this session's own CASE peer node id, \
         not a stub: {before_entry:?}"
    );

    // Replace the ACL with an Operate-only entry for the same subject.
    // This write itself is judged against the *pre*-replace Administer
    // entry, so it must succeed.
    let demoted_tlv = encode_single_acl_entry_tlv(
        PRIVILEGE_OPERATE,
        AUTH_MODE_CASE,
        ADMIN_NODE_ID,
        FABRIC_INDEX,
    );
    session
        .write_attribute_tlv(
            0,
            im::CLUSTER_ACCESS_CONTROL,
            im::ATTR_ACL,
            &demoted_tlv,
            None,
            &cfg,
        )
        .await
        .expect("the demoting ACL write should succeed under the pre-replace Administer entry");

    // From here on, ACL read (Administer-gated) must be rejected —
    // UNSUPPORTED_ACCESS, not a transport/decode error.
    let after = session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await;
    match after {
        Err(SessionError::Im(ImError::AttributeStatus(status))) => {
            assert_eq!(
                status,
                im::STATUS_UNSUPPORTED_ACCESS,
                "ACL read after demotion must be rejected as UNSUPPORTED_ACCESS, got status {status:#04x}"
            );
        }
        other => panic!(
            "expected ACL read to be rejected with AttributeStatus(UNSUPPORTED_ACCESS) \
             after the demotion, got: {other:?}"
        ),
    }

    // But OnOff Toggle (Operate-gated) still goes through — the demoted
    // entry still grants Operate, and enforcement is judging the *same*
    // real subject it just rejected above, not two different identities.
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
        .expect("OnOff Toggle should still succeed for the Operate-demoted admin");
    assert_eq!(resp.status, im::STATUS_SUCCESS);

    device_task.abort();
    let _ = device_task.await;
}
