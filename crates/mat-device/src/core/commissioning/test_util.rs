//! Commissioning-server test helpers shared by the sibling modules' tests
//! (fixture server, fabric installation, direct command dispatch).
#![cfg(test)]

use mat_controller::commissioning::{
    decode_csr_response, encode_add_noc, encode_add_trusted_root, encode_arm_fail_safe,
    encode_csr_request, parse_nocsr_elements, CommissioningFabric, CLUSTER_GENERAL_COMMISSIONING,
    CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_NOC, CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE,
    CMD_CSR_REQUEST,
};
use mat_controller::im;
use mat_controller::x509::{generate_dev_attestation, parse_csr};

use crate::core::datamodel::{InvokeCtx, InvokeReply, ReadCtx};
use crate::core::fabric_store::FabricStore;
use crate::core::group_key_management::GroupKeyStore;
use crate::core::group_membership::GroupMembershipStore;

use super::CommissioningServer;

/// Fixed per-test "session" attestation challenge — in the real
/// protocol this comes from `SecureSession::attestation_challenge()`
/// and stays constant for the lifetime of one PASE/CASE session; tests
/// drive several commands against the same `InvokeCtx` value to match.
pub(super) const TEST_CHALLENGE: [u8; 16] = [42u8; 16];

pub(super) fn test_ctx() -> InvokeCtx {
    InvokeCtx {
        attestation_challenge: TEST_CHALLENGE,
        ..InvokeCtx::default()
    }
}

/// A `ReadCtx` for a session on `fabric_index`, at the (now filtered)
/// default — for the tests that only care *which* fabric is reading and
/// always pass the fabric that's actually installed, so filtered vs.
/// unfiltered makes no difference to what comes back. The
/// fabric-filtering behavior itself is covered by
/// `oc_reads_are_fabric_scoped_when_fabric_filtered`, which spells both
/// fields out at every call site.
pub(super) fn read_ctx(fabric_index: u8) -> ReadCtx {
    ReadCtx {
        fabric_index,
        ..ReadCtx::default()
    }
}

/// A `CommissioningServer` over a freshly generated dev attestation
/// chain and an in-memory (non-persisted) `FabricStore` — file-backed
/// persistence is `net`-only and tested in `net::store` instead (this
/// module must stay `cargo check --no-default-features`-clean).
pub(super) fn test_server() -> CommissioningServer {
    let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
    CommissioningServer::new(dev, FabricStore::new())
}

/// Drives ArmFailSafe → CSRRequest → AddTrustedRootCertificate → AddNOC
/// against `server`, installing one fabric (`fabric_id`/`node_id` from
/// the caller; admin subject `0xAA` and admin vendor id `0xFFF1` fixed —
/// no test here needs to vary those). Returns the `AddNOC` reply so
/// callers that care about the command's own response (like
/// `add_noc_installs_fabric`) can assert on it directly. Delegates to
/// `install_fabric_with_admin` with admin subject `0xAA`.
pub(super) fn install_fabric(
    server: &mut CommissioningServer,
    fabric_id: u64,
    node_id: u64,
) -> InvokeReply {
    install_fabric_with_admin(server, fabric_id, node_id, 0xAA).0
}

/// `install_fabric` の admin subject 可変版: ArmFailSafe → CSR →
/// AddTrustedRoot まで進めてから `case_admin_subject` で AddNOC を
/// 打ち、その reply と「同じ pending で再 AddNOC するための NOC/
/// fabric」を返す。
pub(super) fn install_fabric_with_admin(
    server: &mut CommissioningServer,
    fabric_id: u64,
    node_id: u64,
    case_admin_subject: u64,
) -> (InvokeReply, Vec<u8>, CommissioningFabric) {
    let fabric = CommissioningFabric::generate(fabric_id, 0xAA).unwrap();

    drive_invoke(
        server,
        CLUSTER_GENERAL_COMMISSIONING,
        CMD_ARM_FAIL_SAFE,
        &encode_arm_fail_safe(120, 1),
    );

    let (_, csr_resp) = expect_data(drive_invoke(
        server,
        CLUSTER_OPERATIONAL_CREDENTIALS,
        CMD_CSR_REQUEST,
        &encode_csr_request(&[3u8; 32]),
    ));
    let (elements, _sig) = decode_csr_response(&csr_resp).unwrap();
    let (csr_der, _nonce) = parse_nocsr_elements(&elements).unwrap();
    let device_pub = parse_csr(&csr_der).unwrap();
    let noc = fabric.issue_device_noc(&device_pub, node_id).unwrap();

    assert_eq!(
        drive_invoke(
            server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_TRUSTED_ROOT,
            &encode_add_trusted_root(&fabric.rcac_tlv),
        ),
        InvokeReply::Status(im::STATUS_SUCCESS)
    );

    let reply = drive_invoke(
        server,
        CLUSTER_OPERATIONAL_CREDENTIALS,
        CMD_ADD_NOC,
        &encode_add_noc(&noc, &fabric.ipk_epoch, case_admin_subject, 0xFFF1),
    );
    (reply, noc, fabric)
}

/// A `CommissioningServer` with one fabric already installed
/// (fabric_id=0x1122, node_id=0x5001, admin_vendor_id=0xFFF1) — shared
/// setup for the GC/OC attribute-read tests below, which only care
/// about the resulting fabric state, not the `AddNOC` command's own
/// reply (`add_noc_installs_fabric` covers that separately via
/// `install_fabric` directly).
pub(super) fn commissioned_server() -> CommissioningServer {
    let mut server = test_server();
    install_fabric(&mut server, 0x1122, 0x5001);
    server
}

/// Drives one command directly against `server`'s shared state
/// (bypassing `Node`/IM wire framing — `wired_into_node_dispatches_
/// both_clusters` below covers that separately). Unlike the brief's
/// illustrative 3-arg sketch, this takes `cluster` explicitly: several
/// `CMD_*` request ids collide numerically across the two clusters
/// (e.g. `CMD_ARM_FAIL_SAFE == CMD_ATTESTATION_REQUEST == 0x00`), so
/// inferring the cluster from the command id alone would be ambiguous.
pub(super) fn drive_invoke(
    server: &mut CommissioningServer,
    cluster: u32,
    command: u32,
    fields: &[u8],
) -> InvokeReply {
    server.invoke_command(cluster, command, fields, &test_ctx())
}

pub(super) fn expect_data(reply: InvokeReply) -> (u32, Vec<u8>) {
    match reply {
        InvokeReply::Data {
            response_command,
            fields_tlv,
        } => (response_command, fields_tlv),
        InvokeReply::Status(status) => {
            panic!("expected data reply, got status 0x{status:02X}")
        }
        InvokeReply::ClusterStatus {
            status,
            cluster_status,
        } => {
            panic!(
                "expected data reply, got cluster status 0x{status:02X} (cluster-specific: 0x{cluster_status:02X})"
            )
        }
    }
}

/// rollback / RemoveFabric の purge 対象 3 store を配線した server。
pub(super) fn server_with_stores() -> (
    CommissioningServer,
    crate::core::access_control::AclStore,
    GroupKeyStore,
    GroupMembershipStore,
) {
    let mut server = test_server();
    let acl = crate::core::access_control::AclStore::new();
    let gk = GroupKeyStore::new();
    let membership = GroupMembershipStore::new();
    server.set_acl_store(acl.clone());
    server.set_group_key_store(gk.clone());
    server.set_group_membership_store(membership.clone());
    (server, acl, gk, membership)
}

/// fabric 1 の admin (subject 0xAA) が ACL 上 Administer を持つか —
/// AddNOC の自動 admin エントリの有無を表す。
pub(super) fn admin_allowed(acl: &crate::core::access_control::AclStore) -> bool {
    acl.check(
        1,
        crate::core::access_control::Subject::node(0xAA),
        crate::core::access_control::PRIVILEGE_ADMINISTER,
        0,
        im::CLUSTER_ACCESS_CONTROL,
    )
}

/// install 直後に 3 store へ fabric 1 の状態を仕込む（admin ACL は AddNOC が
/// 自動で入れる）。
pub(super) fn seed_fabric_state(gk: &GroupKeyStore, membership: &GroupMembershipStore) {
    gk.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();
    gk.replace_fabric_map(1, vec![(0x000A, 7)]);
    membership.add(1, 0x000A, 2).unwrap();
}

pub(super) fn assert_fabric_state_purged(
    acl: &crate::core::access_control::AclStore,
    gk: &GroupKeyStore,
    membership: &GroupMembershipStore,
) {
    assert!(!admin_allowed(acl), "ACL admin entry must be purged");
    assert!(!gk.keyset_exists(1, 7));
    assert!(gk.map_entries_for(1).is_empty());
    assert!(
        membership.groups_by_fabric().is_empty(),
        "membership must be purged"
    );
}
