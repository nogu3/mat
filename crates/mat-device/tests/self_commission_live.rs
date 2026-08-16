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

use mat_controller::case::{self};
use mat_controller::commissioning::{
    encode_arm_fail_safe, encode_csr_request, CommissionParams, CommissionTarget,
    CommissioningFabric, CMD_ARM_FAIL_SAFE, CMD_CSR_REQUEST,
};
use mat_controller::transport::{Transport, UdpTransport};
use mat_controller::x509;

use mat_device::device::Device;

mod support;
use support::{
    commission_directly, device_config, fast_cfg, DEVICE_NODE_ID, DISCRIMINATOR, PASSCODE,
    PRODUCT_ID, VENDOR_ID,
};

const ADMIN_NODE_ID: u64 = 112_233;

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

    let device_task = tokio::spawn(async move {
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

    // Restart path (full acceptance-criterion check, not just the reload
    // smoke test above): the spec's acceptance includes "CASE still answers
    // after a restart" (再起動後も CASE 応答が生きている) — proving that
    // needs actually killing the first `Device::run`, standing up a *new*
    // `Device` over the same `store_dir`, and driving a fresh CASE
    // establishment plus a secured read against it, not just confirming
    // `Device::new` doesn't error.
    device_task.abort();
    let _ = device_task.await;

    let restarted = Device::new(device_config(store_dir.path().to_path_buf()))
        .expect("Device::new should reload the persisted fabric store across a restart");
    let restarted_addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        restarted.local_addr().port(),
    );
    tokio::spawn(async move {
        let _ = restarted.run().await;
    });

    let cfg = fast_cfg();
    let creds = fabric.admin_credentials().expect("admin credentials");
    let case_transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));
    let mut session = None;
    let mut last_err = None;
    for _ in 0..10 {
        match case::establish(
            Arc::clone(&case_transport),
            restarted_addr,
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
    let mut session =
        session.unwrap_or_else(|| panic!("CASE re-establish after restart failed: {last_err:?}"));

    // A secured read after the post-restart CASE must actually work — not
    // just the handshake. VendorID mirrors what this restarted `Device` was
    // configured with (see `device_config`), so this also re-confirms the
    // BasicInformation VendorID wiring survives a restart.
    let value = session
        .read_attribute(
            0,
            mat_controller::im::CLUSTER_BASIC_INFORMATION,
            mat_controller::im::ATTR_VENDOR_ID,
            &cfg,
        )
        .await
        .expect("secured read after restart CASE should succeed");
    assert_eq!(
        value,
        mat_controller::im::ImValue::Uint(u64::from(VENDOR_ID))
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

/// Task 14, brief Step 3: proves the commissioning window actually closes
/// on `CommissioningComplete` — a fresh PASE attempt against a device that
/// just finished commissioning must get **silence**, not a `StatusReport`
/// (申し送り 7 項's DoS-hardening posture — see `admit_unsecured`'s doc
/// comment in `net::runtime`).
///
/// Two independent proofs (fix round 1, review item 2: a bare `Err` from
/// `establish` isn't proof of *silence* — a future regression to answering
/// with an explicit `StatusReport` refusal would also return `Err` and slip
/// through unnoticed):
/// - `mat_controller::pase::establish` must fail with exactly
///   `PaseError::Exchange(ExchangeError::Timeout)` — the "nothing answered
///   within the MRP retry budget" variant specifically, not e.g.
///   `PaseError::StatusReport` (which would mean the device *did* answer,
///   just with a refusal) or a decode error.
/// - A single, unretried `PBKDFParamRequest` sent raw (bypassing
///   `send_reliable`'s own retry loop entirely) followed by a bounded
///   `recv_from` on that same socket, asserted to itself time out — proving
///   literally zero bytes come back, not even a bare MRP ack, within a
///   fixed short window. This is independent of whatever `establish`'s
///   retry/backoff internals do, and is the most direct statement of
///   "silently dropped" the brief asks for.
#[tokio::test]
async fn pase_after_commissioning_complete_is_silently_dropped() {
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
        CommissioningFabric::generate(0x3344_5566, ADMIN_NODE_ID).expect("fabric generate");
    // Reaches CommissioningComplete — the window should close right there,
    // well before this device's 15-minute boot-time window would have
    // elapsed on its own.
    let _session = commission_directly(addr, &paa_der, &fabric).await;

    // Proof 1: `establish` fails with exactly the "nothing answered" variant.
    {
        use mat_controller::exchange::ExchangeError;
        use mat_controller::pase::PaseError;

        let pase_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind().await.unwrap(),
        )));
        let result =
            mat_controller::pase::establish(pase_transport, addr, PASSCODE, &fast_cfg()).await;
        // Matched by hand (not `SecureSession: Debug`-derived `{result:?}`,
        // which doesn't exist) so a regression still prints something
        // actionable: `Ok` means the window failed to stay closed at all;
        // any other `Err` variant means the device *did* answer, just not
        // with silence.
        match result {
            Err(PaseError::Exchange(ExchangeError::Timeout)) => {}
            Ok(_) => panic!("PASE against a closed-window device unexpectedly succeeded"),
            Err(e) => {
                panic!("expected a bare MRP-retry-budget timeout (no response at all), got: {e}")
            }
        }
    }

    // Proof 2: a single, unretried PBKDFParamRequest sent raw gets nothing
    // back at all within a bounded wait — not an ack, not a StatusReport.
    {
        use mat_controller::message::{Destination, MessageHeader, ProtocolHeader};
        use mat_controller::pase::{encode_pbkdf_param_request, OPCODE_PBKDF_PARAM_REQUEST};
        use mat_controller::transport::MAX_DATAGRAM;

        let raw = UdpTransport::bind().await.unwrap();
        let mut initiator_random = [0u8; 32];
        getrandom::getrandom(&mut initiator_random).expect("os rng");
        let header = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter: 1,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: OPCODE_PBKDF_PARAM_REQUEST,
            exchange_id: 0x77,
            protocol_id: mat_controller::message::PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        let mut datagram = Vec::new();
        header.encode(&mut datagram);
        proto.encode(&mut datagram);
        datagram.extend_from_slice(&encode_pbkdf_param_request(&initiator_random, 0xBEEF));

        raw.send_to(&datagram, addr).await.unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let recv = tokio::time::timeout(Duration::from_millis(300), raw.recv_from(&mut buf)).await;
        assert!(
            recv.is_err(),
            "expected total silence (bounded recv timeout) for a raw PASE datagram against a closed window, got {recv:?}"
        );
    }

    device_task.abort();
    let _ = device_task.await;
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
