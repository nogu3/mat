//! Production `mat_controller::pase::establish` against `mat-device`'s own
//! PASE responder (`net::pase::run_pase_once`) over real loopback UDP.
//! Mirror of `mat-controller`'s `pase_self_handshake.rs` (which drives the
//! initiator against a *test-only* hand-rolled responder), except here the
//! responder under test is production `mat-device` code — first end-to-end
//! proof that `core::pase::PaseResponderCore` + the `net` driver interop
//! with the real initiator: opcode/TLV framing, PAKE context hashing, and
//! (most importantly) that the session keys the device derives are the
//! *same* keys the controller derives (PASE is unauthenticated — both
//! sides end up at node id 0, so key agreement is the only thing this test
//! can observe end-to-end).
//!
//! Gated on the `net` feature: this test drives `mat_device::net::pase`
//! (tokio + real UDP sockets), which doesn't exist under
//! `--no-default-features` — without this gate `cargo test -p mat-device
//! --no-default-features` fails to compile the test binary (Task 9
//! carry-over fix).
#![cfg(feature = "net")]

use std::sync::Arc;
use std::time::Duration;

use mat_controller::exchange::MrpConfig;
use mat_controller::pase;
use mat_controller::transport::{Transport, UdpTransport};
use mat_device::net::pase::run_pase_once;

/// `MrpConfig` with fast retry intervals and no jitter — same values as
/// `mat_controller::test_support::fast_cfg` (not reused directly: pulling
/// it in would require enabling mat-controller's `test-responder` feature
/// for a single trivial struct literal).
fn fast_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: Duration::from_millis(50),
        active_interval: Duration::from_millis(50),
        max_retries: 2,
        backoff: 1.0,
        jitter: 0.0,
    }
}

#[tokio::test]
async fn mat_establish_against_mat_device_core() {
    let transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let addr = transport.local_addr().unwrap();
    let dev = tokio::spawn(run_pase_once(transport, 20202021));

    let t2 = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));
    let session = pase::establish(t2, addr, 20202021, &fast_cfg())
        .await
        .expect("PASE establish should succeed against mat-device's responder");
    // PASE is unauthenticated: both sides use node id 0.
    assert_eq!(session.peer_node_id(), 0);

    dev.await
        .expect("responder task panicked")
        .expect("responder handshake should succeed");
}
