//! Production `mat_controller::case::establish` against `mat-device`'s own
//! CASE responder (`net::case::run_case_once`) over real loopback UDP.
//! Mirror of `mat-controller`'s `case_self_handshake.rs` (which drives the
//! initiator against a *test-only* hand-rolled responder), except here the
//! responder under test is production `mat-device` code — first end-to-end
//! proof that `core::case::CaseResponderCore` + the `net` driver interop
//! with the real initiator: fabric selection via `case_destination_id`, the
//! TBS2/TBS3 orientation and S2K/S3K/SessionKeys transcript boundaries,
//! peer NOC chain verification, and the secured post-CASE channel (proven
//! here by one secured IM read/report round trip, same as
//! `case_self_handshake.rs`).
//!
//! Gated on the `net` feature: this test drives `mat_device::net::case`
//! (tokio + real UDP sockets), which doesn't exist under
//! `--no-default-features`.
#![cfg(feature = "net")]

use std::sync::Arc;
use std::time::Duration;

use mat_controller::case;
use mat_controller::commissioning::CommissioningFabric;
use mat_controller::exchange::MrpConfig;
use mat_controller::im::{self, ImValue};
use mat_controller::tlv::{Tag, Writer};
use mat_controller::transport::{Transport, UdpTransport};
use mat_device::core::fabric_store::FabricEntry;
use mat_device::net::case::run_case_once;

const ADMIN_NODE_ID: u64 = 0x1;
const FABRIC_ID: u64 = 0xFAB10;
const DEVICE_NODE_ID: u64 = 0x5001;
const RESPONDER_SESSION_ID: u16 = 0xC0C1;

/// `MrpConfig` with fast retry intervals and no jitter — same values as
/// `mat_controller::test_support::fast_cfg` / `mat-device`'s
/// `pase_establish.rs` (not reused directly: pulling in `test_support`
/// would require enabling mat-controller's `test-responder` feature for a
/// single trivial struct literal).
fn fast_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: Duration::from_millis(50),
        active_interval: Duration::from_millis(50),
        max_retries: 2,
        backoff: 1.0,
        jitter: 0.0,
    }
}

/// ReportData for onoff `OnOff` = false, `SuppressResponse` = true (so the
/// initiator's `read_attribute` won't send a closing StatusResponse).
/// Structurally the production counterpart of
/// `mat_controller::test_support::report_data_false_suppressed`, built with
/// `im::encode_report_data` (the doc comment on that function names the test
/// helper as the fixture it was modeled on).
fn report_data_false_suppressed() -> Vec<u8> {
    let value_tlv = {
        let mut w = Writer::new();
        w.put_bool(Tag::Anonymous, false);
        w.finish()
    };
    im::encode_report_data(
        &[im::AttrReportOut {
            endpoint: 1,
            cluster: im::CLUSTER_ON_OFF,
            attribute: im::ATTR_ON_OFF,
            data_version: 1,
            value_tlv,
        }],
        true, // SuppressResponse
    )
}

#[tokio::test]
async fn case_establishes_against_mat_device_core_and_reads() {
    // Admin/controller side: a throwaway fabric this test both commissions
    // devices onto (`issue_device_noc`) and initiates CASE as
    // (`admin_credentials`) — same helper `mat_native`'s real commissioning
    // flow uses.
    let fabric = CommissioningFabric::generate(FABRIC_ID, ADMIN_NODE_ID).expect("generate fabric");
    let admin_creds = fabric.admin_credentials().expect("admin credentials");

    // Device operational identity: fresh key pair + a NOC issued by the same
    // root, so it and the admin share a fabric (a CASE requirement).
    let device_secret = case::random_p256_secret();
    let device_pub = case::eph_pub_bytes(&device_secret);
    let device_priv: [u8; 32] = device_secret.to_bytes().into();
    let device_noc_tlv = fabric
        .issue_device_noc(&device_pub, DEVICE_NODE_ID)
        .expect("issue device noc");

    let fabric_entry = FabricEntry {
        fabric_index: 1,
        root_tlv: fabric.rcac_tlv.clone(),
        noc_tlv: device_noc_tlv,
        icac_tlv: None, // two-tier chain, NOC signed directly by root
        op_private_key: device_priv,
        // Same IPK the admin side derived from the fabric's epoch IPK
        // (`admin_credentials` did that derivation already) — CASE requires
        // both sides to share the operational IPK.
        ipk_operational: admin_creds.ipk_operational,
        node_id: DEVICE_NODE_ID,
        fabric_id: fabric.fabric_id,
        root_public_key: admin_creds.root_public_key,
        admin_subject: ADMIN_NODE_ID,
    };

    // Responder (mat-device core, over the net driver) socket first, so we
    // can hand its address to the initiator.
    let responder_transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let responder_addr = responder_transport.local_addr().unwrap();

    let dev = tokio::spawn(async move {
        let (mut session, fabric_index) = run_case_once(
            responder_transport,
            vec![fabric_entry],
            RESPONDER_SESSION_ID,
        )
        .await?;
        // Serve one secured IM ReadRequest, mirroring
        // `test_support::responder_task`'s tail — but through the
        // production `SecureSession` device-role helpers instead of hand
        // rolling the seal/open framing a second time.
        let request = session.recv_request(Duration::from_secs(5)).await?;
        assert_eq!(request.proto.protocol_id, im::PROTOCOL_ID_IM);
        assert_eq!(request.proto.opcode, im::OPCODE_READ_REQUEST);
        session
            .reply_reliable(
                &request,
                im::PROTOCOL_ID_IM,
                im::OPCODE_REPORT_DATA,
                &report_data_false_suppressed(),
                &fast_cfg(),
            )
            .await?;
        Ok::<u8, Box<dyn std::error::Error + Send + Sync>>(fabric_index)
    });

    // Initiator: production CASE, driven directly (not through
    // `mat_native`/`commissioning::operational_case_and_complete` — both
    // are already gated behind CommissioningComplete/discovery machinery
    // this test doesn't need; `case::establish` is already `pub`, no
    // visibility change required).
    let initiator_udp = Arc::new(
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap(),
    );
    let initiator_transport = Arc::new(Transport::Udp(Arc::clone(&initiator_udp)));

    let cfg = fast_cfg();
    let mut session = case::establish(
        Arc::clone(&initiator_transport),
        responder_addr,
        &admin_creds,
        DEVICE_NODE_ID,
        &cfg,
    )
    .await
    .expect("CASE establish should succeed against mat-device's responder");

    let value = session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("secured read should succeed");
    assert_eq!(value, ImValue::Bool(false));

    let fabric_index = dev
        .await
        .expect("responder task panicked")
        .expect("responder handshake/read should succeed");
    assert_eq!(fabric_index, 1);
}
