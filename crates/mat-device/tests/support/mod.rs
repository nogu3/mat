//! Shared direct-drive setup for `mat-device`'s `net`-feature integration
//! tests: a loopback-only `DeviceConfig` plus a from-scratch commissioning
//! sequence (`commission_directly`) that reaches an operational, secured
//! `SecureSession` against a running `Device` without mDNS. Extracted from
//! `self_commission_live.rs` (Task M2-2's brief: reuse this setup rather
//! than duplicate it) — that file was the original, and only, owner of this
//! logic before a second test (`onoff_invoke.rs`) needed the same
//! commissioned-session starting point.
//!
//! Not itself a Cargo integration test target: `tests/support/mod.rs`
//! (a `mod.rs` inside a subdirectory of `tests/`) is invisible to Cargo's
//! test-target discovery, which only treats direct files in `tests/` (and
//! `tests/*/main.rs`) as their own test binaries. Consumers pull it in with
//! `mod support;`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::attestation::verify_device_attestation;
use mat_controller::case;
use mat_controller::commissioning::{
    decode_attestation_response, decode_cert_chain_response, decode_csr_response,
    decode_noc_response, encode_add_noc, encode_add_trusted_root, encode_arm_fail_safe,
    encode_attestation_request, encode_cert_chain_request, encode_csr_request,
    parse_nocsr_elements, CommissioningFabric, CERT_TYPE_DAC, CERT_TYPE_PAI,
    CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_NOC,
    CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE, CMD_ATTESTATION_REQUEST, CMD_CERT_CHAIN_REQUEST,
    CMD_COMMISSIONING_COMPLETE, CMD_CSR_REQUEST,
};
use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::FabricCredentials;
use mat_controller::pase;
use mat_controller::session::SecureSession;
use mat_controller::transport::{Transport, UdpTransport};
use mat_controller::x509;

use mat_device::core::bridge::DeviceKind;
use mat_device::device::{AttestationMode, DeviceConfig, VirtualDeviceConfig};

/// The endpoint the one bridged device in [`device_config`] lands on — EP0
/// is the root, EP1 the Aggregator, so the first (and here only) bridged
/// device gets EP2 (`net::endpoint_ledger::FIRST_BRIDGED_ENDPOINT`). M3
/// turned `matv` into a pure bridge, so there is no longer any On/Off
/// cluster on EP1 for these tests to drive.
///
/// `allow(dead_code)`: `support` is compiled separately into *each*
/// integration test binary, so anything only some of them use looks unused
/// from the others' point of view (`self_commission_live.rs` never touches
/// an application endpoint).
#[allow(dead_code)]
pub const BRIDGED_EP: u16 = 2;

pub const PASSCODE: u32 = 20202021;
pub const DISCRIMINATOR: u16 = 840;
pub const VENDOR_ID: u16 = 0xFFF1;
pub const PRODUCT_ID: u16 = 0x8000;
pub const DEVICE_NODE_ID: u64 = 1;
pub const ADMIN_VENDOR_ID: u16 = 0xFFF1;

pub fn fast_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: Duration::from_millis(50),
        active_interval: Duration::from_millis(50),
        max_retries: 4,
        backoff: 1.2,
        jitter: 0.0,
    }
}

/// A `Device` bound on loopback-only (`iface = "lo"`) with an ephemeral
/// port — no mDNS discovery needed by these tests, but `Device::run` still
/// spawns the advertiser (it's unconditional in `net::runtime::run`), so
/// `iface` must still resolve to a real, existing interface. `lo` always
/// does; it typically lacks an IPv6 link-local address, which is why this
/// helper is only used by the direct-drive tests (which never touch mDNS)
/// and not by a live-mDNS one.
pub fn device_config(store_dir: std::path::PathBuf) -> DeviceConfig {
    DeviceConfig {
        passcode: PASSCODE,
        discriminator: DISCRIMINATOR,
        vendor_id: VENDOR_ID,
        product_id: PRODUCT_ID,
        port: 0,
        store_dir,
        iface: "lo".to_string(),
        attestation: AttestationMode::default(),
        // The standard e2e `[[device]]` block (same id/kind/name as
        // `matv`'s own tests and `scripts/e2e-*`), landing on
        // [`BRIDGED_EP`].
        devices: vec![VirtualDeviceConfig {
            id: "e2e-light".to_string(),
            kind: DeviceKind::OnOffLight,
            name: "E2E Light".to_string(),
        }],
    }
}

/// Runs the full commissioning credential-steps sequence (spec §5.5 steps
/// 3-9) directly against `addr` using only `mat_controller`'s public
/// encoders/decoders and `SecureSession::invoke_for_data` — mirroring
/// `mat_controller::commissioning::run_credential_steps` +
/// `operational_case_and_complete`, which are private and therefore not
/// callable from here. Returns the post-CASE, post-CommissioningComplete
/// `SecureSession` on success.
///
/// `allow(dead_code)`: see `BRIDGED_EP` — `acl_cat_subject.rs` only calls
/// `commission_directly_as`.
#[allow(dead_code)]
pub async fn commission_directly(
    addr: SocketAddr,
    paa_der: &[u8],
    fabric: &CommissioningFabric,
) -> SecureSession {
    let creds = fabric.admin_credentials().expect("admin credentials");
    commission_directly_as(addr, paa_der, fabric, fabric.admin_node_id, &creds).await
}

/// [`commission_directly_as`] の前半: PASE → ArmFailSafe → attestation
/// （`verify_device_attestation` で strict 検証）→ CSR → NOC 発行 →
/// AddTrustedRootCertificate。AddNOC の直前で止め、PASE セッションと
/// 発行済みデバイス NOC を返す — AddNOC の応答そのものを検査したい
/// テスト（`add_noc_invalid_admin_subject.rs`）のための切り出し。
///
/// `allow(dead_code)`: see `BRIDGED_EP` — only the AddNOC-rejection test
/// calls this directly.
#[allow(dead_code)]
pub async fn pase_until_add_noc(
    addr: SocketAddr,
    paa_der: &[u8],
    fabric: &CommissioningFabric,
) -> (SecureSession, Vec<u8>) {
    let cfg = fast_cfg();
    let transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));

    // 2. PASE.
    let mut pase = pase::establish(Arc::clone(&transport), addr, PASSCODE, &cfg)
        .await
        .expect("pase establish");
    let challenge = pase.attestation_challenge();

    // 3. ArmFailSafe.
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 1)),
            None,
            &cfg,
        )
        .await
        .expect("arm fail safe");
    assert_eq!(resp.status, 0);

    // 5. Attestation (strict — verified below with `verify_device_attestation`,
    // same as the real commissioner).
    let nonce = [9u8; 32];
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ATTESTATION_REQUEST,
            Some(&encode_attestation_request(&nonce)),
            None,
            &cfg,
        )
        .await
        .expect("attestation request");
    let (elements, att_sig) = decode_attestation_response(resp.fields_tlv.as_deref().unwrap())
        .expect("decode attestation");

    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            Some(&encode_cert_chain_request(CERT_TYPE_DAC)),
            None,
            &cfg,
        )
        .await
        .expect("dac cert chain request");
    let dac = decode_cert_chain_response(resp.fields_tlv.as_deref().unwrap()).expect("decode dac");

    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            Some(&encode_cert_chain_request(CERT_TYPE_PAI)),
            None,
            &cfg,
        )
        .await
        .expect("pai cert chain request");
    let pai = decode_cert_chain_response(resp.fields_tlv.as_deref().unwrap()).expect("decode pai");

    verify_device_attestation(
        &dac,
        &pai,
        std::slice::from_ref(&paa_der.to_vec()),
        &[],
        &elements,
        &att_sig,
        &nonce,
        &challenge,
    )
    .expect("device attestation should verify against the device's own PAA");

    // 6. CSR -> NOC issuance.
    let csr_nonce = [10u8; 32];
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            Some(&encode_csr_request(&csr_nonce)),
            None,
            &cfg,
        )
        .await
        .expect("csr request");
    let (nocsr_elements, _sig) =
        decode_csr_response(resp.fields_tlv.as_deref().unwrap()).expect("decode csr response");
    let (csr_der, returned_nonce) =
        parse_nocsr_elements(&nocsr_elements).expect("parse nocsr elements");
    assert_eq!(returned_nonce, csr_nonce);
    let device_pub = x509::parse_csr(&csr_der).expect("parse csr");
    let noc_tlv = fabric
        .issue_device_noc(&device_pub, DEVICE_NODE_ID)
        .expect("issue device noc");

    // 7. AddTrustedRootCertificate -> AddNOC.
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_TRUSTED_ROOT,
            Some(&encode_add_trusted_root(&fabric.rcac_tlv)),
            None,
            &cfg,
        )
        .await
        .expect("add trusted root");
    assert_eq!(resp.status, 0);

    (pase, noc_tlv)
}

/// [`commission_directly_as`] の AddNOC 1 発分: `(NOCResponse status,
/// fabric_index)` を返す。成功判定は呼び側（拒否を期待するテストもある）。
///
/// `allow(dead_code)`: see `BRIDGED_EP` — only the AddNOC-rejection test
/// calls this directly.
#[allow(dead_code)]
pub async fn add_noc(
    pase: &mut SecureSession,
    noc_tlv: &[u8],
    fabric: &CommissioningFabric,
    case_admin_subject: u64,
) -> (u8, Option<u8>) {
    let cfg = fast_cfg();
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            Some(&encode_add_noc(
                noc_tlv,
                &fabric.ipk_epoch,
                case_admin_subject,
                ADMIN_VENDOR_ID,
            )),
            None,
            &cfg,
        )
        .await
        .expect("add noc");
    decode_noc_response(resp.fields_tlv.as_deref().unwrap()).expect("decode noc response")
}

/// [`commission_directly_as`] の後半: 新 fabric への CASE（リトライ付き）
/// + CommissioningComplete。
///
/// `allow(dead_code)`: see `BRIDGED_EP` — only the AddNOC-rejection test
/// calls this directly.
#[allow(dead_code)]
pub async fn case_and_complete(addr: SocketAddr, creds: &FabricCredentials) -> SecureSession {
    let cfg = fast_cfg();

    // 8. CASE on the new fabric (retry: AddNOC just completed, the runtime
    // needs no warm-up in-process but real hardware/timing might).
    let case_transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));
    let mut session = None;
    let mut last_err = None;
    for _ in 0..10 {
        match case::establish(
            Arc::clone(&case_transport),
            addr,
            creds,
            DEVICE_NODE_ID,
            &cfg,
        )
        .await
        {
            Ok(s) => {
                session = Some(s);
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    let mut session = session.unwrap_or_else(|| panic!("CASE establish failed: {last_err:?}"));

    // 9. CommissioningComplete.
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            None,
            None,
            &cfg,
        )
        .await
        .expect("commissioning complete");
    assert_eq!(resp.status, 0);

    session
}

/// [`commission_directly`] with the admin identity spelled out: the
/// `CaseAdminSubject` AddNOC installs as the automatic Administer ACL
/// entry's subject (a node id, or a CAT subject as Apple Home sends), and
/// the operational credentials the post-AddNOC CASE presents (whose NOC
/// may carry CATs). `commission_directly` is the node-id-only special
/// case: `fabric.admin_node_id` + `fabric.admin_credentials()`.
///
/// `allow(dead_code)`: see `BRIDGED_EP` — only the CAT-subject test calls
/// this directly.
#[allow(dead_code)]
pub async fn commission_directly_as(
    addr: SocketAddr,
    paa_der: &[u8],
    fabric: &CommissioningFabric,
    case_admin_subject: u64,
    creds: &FabricCredentials,
) -> SecureSession {
    let (mut pase, noc_tlv) = pase_until_add_noc(addr, paa_der, fabric).await;
    let (noc_status, fabric_index) = add_noc(&mut pase, &noc_tlv, fabric, case_admin_subject).await;
    assert_eq!(noc_status, 0, "AddNOC should succeed");
    assert_eq!(fabric_index, Some(1));

    case_and_complete(addr, creds).await
}
