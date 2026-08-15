//! Task 12: self-commissioning end-to-end against the assembled `Device`
//! runtime. Two flavors:
//!
//! - `direct_drive_*` (not `#[ignore]`, runs in `task check`/CI): drives
//!   the full commissioning + CASE + CommissioningComplete sequence
//!   directly against a `Device` bound on an ephemeral loopback-only port,
//!   using `mat_controller::pase::establish`/`case::establish` plus the
//!   commissioner-side `mat_controller::commissioning` pub encoders and
//!   `SecureSession::invoke_for_data` — no mDNS. This is a from-scratch
//!   reimplementation of `commission_on_network`'s inner steps
//!   (`run_credential_steps` + `operational_case_and_complete`), which
//!   can't be called directly because both are private to
//!   `mat_controller::commissioning` and `commission_on_network` itself
//!   only accepts an mDNS-discoverable `CommissionTarget` — exactly what
//!   this test must avoid depending on.
//! - `fail_safe_expiry_gates_add_noc` (not `#[ignore]`): a fast
//!   (~1.2s), no-networking check that `CommissioningServer`'s fail-safe
//!   timer really does expire and re-gate commands, driven directly (no
//!   `Device`/runtime involved — this is `CommissioningServer`'s own
//!   contract, already exercised structurally by `core::commissioning`'s
//!   own tests; this one adds the missing wall-clock-expiry case).
//! - `live_mdns_self_commission` (`#[ignore]`, run via `task
//!   e2e:device:m1`): the same commissioning sequence but reached via real
//!   mDNS discovery (`mat_controller::commissioning::commission_on_network`,
//!   the exact function `mat commission`'s native backend calls) — mirrors
//!   `scripts/e2e-device-m1.sh`, which drives the *actual* `mat` binary
//!   instead. Needs a real NIC (see `discover_live.rs`'s doc comment on why
//!   this can't run in CI); interface selected by `MAT_E2E_IFACE` (default
//!   `lo`, per that same test's convention).
#![cfg(feature = "net")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::attestation::verify_device_attestation;
use mat_controller::case::{self};
use mat_controller::commissioning::{
    decode_attestation_response, decode_cert_chain_response, decode_csr_response,
    decode_noc_response, encode_add_noc, encode_add_trusted_root, encode_arm_fail_safe,
    encode_attestation_request, encode_cert_chain_request, encode_csr_request,
    parse_nocsr_elements, CommissionParams, CommissionTarget, CommissioningFabric, CERT_TYPE_DAC,
    CERT_TYPE_PAI, CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_NOC,
    CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE, CMD_ATTESTATION_REQUEST, CMD_CERT_CHAIN_REQUEST,
    CMD_COMMISSIONING_COMPLETE, CMD_CSR_REQUEST,
};
use mat_controller::exchange::MrpConfig;
use mat_controller::pase;
use mat_controller::session::SecureSession;
use mat_controller::transport::{Transport, UdpTransport};
use mat_controller::x509;

use mat_device::device::{Device, DeviceConfig};

const PASSCODE: u32 = 20202021;
const DISCRIMINATOR: u16 = 840;
const VENDOR_ID: u16 = 0xFFF1;
const PRODUCT_ID: u16 = 0x8000;
const ADMIN_NODE_ID: u64 = 112_233;
const DEVICE_NODE_ID: u64 = 1;
const ADMIN_VENDOR_ID: u16 = 0xFFF1;

fn fast_cfg() -> MrpConfig {
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
/// and not by the live one below.
fn device_config(store_dir: std::path::PathBuf) -> DeviceConfig {
    DeviceConfig {
        passcode: PASSCODE,
        discriminator: DISCRIMINATOR,
        vendor_id: VENDOR_ID,
        product_id: PRODUCT_ID,
        port: 0,
        store_dir,
        iface: "lo".to_string(),
    }
}

/// Runs the full commissioning credential-steps sequence (spec §5.5 steps
/// 3-9) directly against `addr` using only `mat_controller`'s public
/// encoders/decoders and `SecureSession::invoke_for_data` — mirroring
/// `mat_controller::commissioning::run_credential_steps` +
/// `operational_case_and_complete`, which are private and therefore not
/// callable from here. Returns the post-CASE, post-CommissioningComplete
/// `SecureSession` on success.
async fn commission_directly(
    addr: SocketAddr,
    paa_der: &[u8],
    fabric: &CommissioningFabric,
) -> SecureSession {
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

    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            Some(&encode_add_noc(
                &noc_tlv,
                &fabric.ipk_epoch,
                fabric.admin_node_id,
                ADMIN_VENDOR_ID,
            )),
            None,
            &cfg,
        )
        .await
        .expect("add noc");
    let (noc_status, fabric_index) =
        decode_noc_response(resp.fields_tlv.as_deref().unwrap()).expect("decode noc response");
    assert_eq!(noc_status, 0, "AddNOC should succeed");
    assert_eq!(fabric_index, Some(1));

    // 8. CASE on the new fabric (retry: AddNOC just completed, the runtime
    // needs no warm-up in-process but real hardware/timing might).
    let creds = fabric.admin_credentials().expect("admin credentials");
    let case_transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));
    let mut session = None;
    let mut last_err = None;
    for _ in 0..10 {
        match case::establish(
            Arc::clone(&case_transport),
            addr,
            &creds,
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

#[tokio::test]
async fn direct_drive_self_commission_reaches_operational_case() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    // `device.local_addr()` is `[::]:{port}` (the wildcard bind address the
    // brief specifies) — not itself a valid *destination* to send to.
    // Substitute the loopback address, keeping the OS-assigned ephemeral
    // port; a `[::]`-bound socket accepts traffic addressed to `[::1]` too.
    let addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        device.local_addr().port(),
    );
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der"))
        .expect("device should have written its PAA DER at Device::new");

    tokio::spawn(async move {
        let _ = device.run().await;
    });

    let fabric =
        CommissioningFabric::generate(0x1122_3344, ADMIN_NODE_ID).expect("fabric generate");
    let _session = commission_directly(addr, &paa_der, &fabric).await;

    // Persistence smoke test (brief's self-review requirement): the fabric
    // AddNOC just installed is really on disk, in the shape a fresh
    // `Device::new` over the same `store_dir` would reload — checked via
    // `FabricPersist::load` directly (the runtime doesn't expose fabrics
    // itself; this proves persistence without adding surface area to
    // `Device`).
    {
        use mat_device::core::fabric_store::FabricPersist;
        let entries = mat_device::net::store::store_in_dir(store_dir.path())
            .load()
            .expect("load persisted fabrics");
        assert_eq!(entries.len(), 1, "AddNOC should have persisted one fabric");
        assert_eq!(entries[0].node_id, DEVICE_NODE_ID);
    }

    // Restart path: a second `Device::new` over the same store_dir must
    // reload without error (this is what "operational advert re-published"
    // depends on inside `net::runtime::run` — see its restart-path doc
    // comment; a full mDNS assertion belongs in the live test below).
    let restarted = Device::new(device_config(store_dir.path().to_path_buf()));
    assert!(
        restarted.is_ok(),
        "Device::new should reload the persisted fabric store across a restart: {:?}",
        restarted.err()
    );
}

/// Fail-safe expiry (brief's self-review requirement): arm with a very
/// short expiry, wait past it, and confirm a gated command that would
/// otherwise succeed is rejected with `STATUS_FAILSAFE_REQUIRED`. Drives
/// `CommissioningServer` directly (no `Device`/socket needed — this is
/// purely `core::commissioning`'s own timer contract).
#[tokio::test]
async fn fail_safe_expiry_gates_add_noc() {
    use mat_controller::commissioning::decode_commissioning_status_response;
    use mat_controller::im;
    use mat_device::core::commissioning::CommissioningServer;
    use mat_device::core::datamodel::{InvokeCtx, InvokeReply};
    use mat_device::core::fabric_store::FabricStore;

    let dev = x509::generate_dev_attestation(VENDOR_ID, PRODUCT_ID).expect("dev attestation");
    let server = CommissioningServer::new(dev, FabricStore::new());
    let (mut gc, mut oc) = {
        let (a, b) = server.into_cluster_handlers();
        (a, b)
    };
    let mut ctx = InvokeCtx::default();

    // ArmFailSafe(1s) — short enough to expire within the test's own
    // budget without slowing `task check` down noticeably.
    let arm_reply = gc.invoke(CMD_ARM_FAIL_SAFE, &encode_arm_fail_safe(1, 1), &mut ctx);
    let InvokeReply::Data { fields_tlv, .. } = arm_reply else {
        panic!("expected data reply from ArmFailSafe");
    };
    let (code, _) = decode_commissioning_status_response(&fields_tlv).unwrap();
    assert_eq!(code, 0, "ArmFailSafe should succeed");

    tokio::time::sleep(Duration::from_millis(1200)).await;

    let reply = oc.invoke(CMD_CSR_REQUEST, &encode_csr_request(&[1u8; 32]), &mut ctx);
    assert_eq!(
        reply,
        InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED),
        "CSRRequest after fail-safe expiry must be rejected, not silently served"
    );
}

/// Live full-mDNS variant (mirrors `scripts/e2e-device-m1.sh`, which drives
/// the real `mat` binary instead of `mat_controller::commissioning`
/// directly). `#[ignore]`d for the same reason as `discover_live.rs`:
/// reliable multicast delivery needs a real NIC.
///
/// Env: `MAT_E2E_IFACE` selects the interface (defaults to `lo`, useful
/// for a quick local smoke run where multicast loopback happens to work —
/// same convention as `discover_live.rs`).
#[tokio::test]
#[ignore = "要: 実 NIC。task e2e:device:m1 で実行"]
async fn live_mdns_self_commission() {
    let iface = std::env::var("MAT_E2E_IFACE").unwrap_or_else(|_| "lo".to_string());
    let scope_id =
        mat_controller::dnssd::iface_index(&iface).expect("interface index (set MAT_E2E_IFACE?)");

    let store_dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = device_config(store_dir.path().to_path_buf());
    cfg.iface = iface;
    let device = Device::new(cfg).expect("device new");
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der")).unwrap();
    let paa_dir = store_dir.path().join("paa");

    tokio::spawn(async move {
        let _ = device.run().await;
    });
    // Let the mDNS advertiser come up before browsing for it.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let fabric =
        CommissioningFabric::generate(0x9988_7766, ADMIN_NODE_ID).expect("fabric generate");
    let transport = Arc::new(UdpTransport::bind().await.expect("bind"));
    let dev = commission_on_network_helper(transport, &fabric, scope_id, &paa_dir).await;

    assert_eq!(dev.node_id, DEVICE_NODE_ID);
    assert_eq!(dev.fabric_index, Some(1));
    let _ = paa_der; // kept for parity with the direct-drive test's shape
}

async fn commission_on_network_helper(
    transport: Arc<UdpTransport>,
    fabric: &CommissioningFabric,
    scope_id: u32,
    paa_dir: &std::path::Path,
) -> mat_controller::commissioning::CommissionedDevice {
    mat_controller::commissioning::commission_on_network(
        transport,
        fabric,
        CommissionParams {
            passcode: PASSCODE,
            target: CommissionTarget::Discriminator(DISCRIMINATOR),
            device_node_id: DEVICE_NODE_ID,
            paa_dir: Some(paa_dir),
            cd_signer_dir: None,
            scope_id,
        },
    )
    .await
    .expect("commission_on_network should succeed against the running Device")
}
