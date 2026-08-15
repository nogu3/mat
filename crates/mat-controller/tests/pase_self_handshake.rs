//! Offline PASE self-handshake (audit Tier 5).
//!
//! Drives the real PASE initiator (`pase::establish`) against a test-only
//! SPAKE2+ *verifier* responder (`test_support::pase_responder_task`) over
//! loopback UDP, then performs one secured IM read. First executable
//! coverage of the PASE success path: Ke derivation, the SessionKeys
//! i2r/r2i split, and both confirmation directions (cA/cB) — previously
//! only encode/decode shapes and failure paths were tested.

use std::sync::Arc;

use mat_controller::im::{self, ImValue};
use mat_controller::pase;
use mat_controller::test_support::{fast_cfg, pase_responder_task};
use mat_controller::transport::{Transport, UdpTransport};

#[tokio::test]
async fn pase_establishes_and_reads_over_loopback() {
    let responder_transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let responder_addr = responder_transport.local_addr().unwrap();
    let responder = tokio::spawn(pase_responder_task(responder_transport, 20202021));

    let initiator_udp = Arc::new(
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap(),
    );
    let initiator_local = initiator_udp.local_addr().unwrap();
    let transport = Arc::new(Transport::Udp(Arc::clone(&initiator_udp)));

    let cfg = fast_cfg();
    let mut session = pase::establish(Arc::clone(&transport), responder_addr, 20202021, &cfg)
        .await
        .expect("PASE establish should succeed over loopback");

    let value = session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("secured read should succeed");
    assert_eq!(value, ImValue::Bool(false));

    let observed = responder.await.expect("responder task panicked");
    assert_eq!(
        observed, initiator_local,
        "responder saw the initiator's socket"
    );
}
