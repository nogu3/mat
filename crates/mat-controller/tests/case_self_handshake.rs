//! Offline CASE self-handshake (mandatory quality gate, plan Task 6).
//!
//! Drives the real CASE initiator (`case::establish`) against a test-only
//! CASE *responder* (extracted to `mat_controller::test_support`, gated
//! behind the `test-responder` feature) over loopback UDP, then performs one
//! secured IM read. This is the first *executable* coverage of the CASE
//! crypto-ordering core — transcript boundaries (S2K salted with
//! SHA256(sigma1) alone, sigma2 folded in only afterwards; S3K over
//! SHA256(sigma1||sigma2); SessionKeys over SHA256(sigma1||sigma2||sigma3)),
//! the S2K/S3K/SessionKeys HKDF derivations, TBS2/TBS3 orientation
//! (sender-eph before receiver-eph), the i2r/r2i key split, and the
//! Sigma1/2/3 + StatusReport wire framing — none of which the (device-
//! blocked) live E2E can currently exercise. See `test_support` for the
//! responder implementation and its residual-risk caveat.
//!
//! The `establish_any_*` tests below pin the Happy Eyeballs candidate race
//! (`case::establish_any`): a dead first address no longer blocks a live
//! second one, all-dead reports every peer, and a live first address wins
//! without the second responder ever seeing a Sigma1.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::case::{self, EstablishAnyError};
use mat_controller::cert::{verify_noc_chain, MatterCert};
use mat_controller::fabric::FabricCredentials;
use mat_controller::im::{self, ImValue};
use mat_controller::kvs::SelfIssueMaterials;
use mat_controller::test_support::{
    fast_cfg, responder_task, ICA01, INITIATOR_NODE_ID, IPK, NODE01_NOC, NODE01_PRIV, ROOT01_CHIP,
    ROOT01_PRIV,
};
use mat_controller::transport::{Transport, UdpTransport};
use tokio::task::JoinHandle;

/// Responder identity (node01_01 under ica01/root01) + an initiator
/// credential set on the same fabric.
struct Fixture {
    creds: FabricCredentials,
    responder_node_id: u64,
}

fn fixture() -> Fixture {
    let noc_cert = MatterCert::parse(NODE01_NOC).expect("parse node01_01 NOC");
    let responder_node_id = noc_cert.node_id().expect("node id");
    let responder_fabric_id = noc_cert.fabric_id().expect("fabric id");
    let ica_cert = MatterCert::parse(ICA01).unwrap();
    let root_cert = MatterCert::parse(ROOT01_CHIP).unwrap();
    verify_noc_chain(&noc_cert, Some(&ica_cert), &root_cert).expect("fixture chain");

    let materials = SelfIssueMaterials {
        rcac: ROOT01_CHIP.to_vec(),
        root_private_key: ROOT01_PRIV.try_into().unwrap(),
        ipk_operational: IPK,
        node_id: INITIATOR_NODE_ID,
        fabric_id: responder_fabric_id,
    };
    let creds = FabricCredentials::from_self_issued(materials).expect("self-issued creds");
    Fixture {
        creds,
        responder_node_id,
    }
}

/// Spawns a loopback CASE responder; returns its address and the task
/// (which resolves to the initiator address it observed).
async fn spawn_responder(fx: &Fixture) -> (SocketAddr, JoinHandle<SocketAddr>) {
    let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let addr = t.local_addr().unwrap();
    let op_priv: [u8; 32] = NODE01_PRIV.try_into().unwrap();
    let handle = tokio::spawn(responder_task(
        t,
        INITIATOR_NODE_ID,
        fx.responder_node_id,
        NODE01_NOC.to_vec(),
        ICA01.to_vec(),
        op_priv,
        ROOT01_CHIP.to_vec(),
    ));
    (addr, handle)
}

/// A loopback address bound but never serviced: nobody reads from the
/// socket, so datagrams sent there are accepted by the kernel and never
/// answered (unconnected UDP gets no ICMP error either), which is the
/// faithful "dead address" behaviour — the CASE attempt times out after
/// the MRP budget of `fast_cfg()`. The socket is returned alongside the
/// address and must be kept alive (bound) for as long as the address is
/// used as "dead": dropping it early would free the ephemeral port for
/// reuse, and a later `spawn_responder()` (in this or a parallel test)
/// could then be handed the same port, turning "dead" into "alive".
async fn dead_port() -> (SocketAddr, UdpTransport) {
    let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let addr = t.local_addr().unwrap();
    (addr, t)
}

#[tokio::test]
async fn case_establishes_and_reads_over_loopback() {
    let fx = fixture();
    let (responder_addr, responder) = spawn_responder(&fx).await;

    let initiator_udp = Arc::new(
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap(),
    );
    let initiator_local = initiator_udp.local_addr().unwrap();
    let initiator_transport = Arc::new(Transport::Udp(Arc::clone(&initiator_udp)));

    let cfg = fast_cfg();
    let mut session = case::establish(
        Arc::clone(&initiator_transport),
        responder_addr,
        &fx.creds,
        fx.responder_node_id,
        &cfg,
    )
    .await
    .expect("CASE establish should succeed over loopback");

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

const TEST_STAGGER: Duration = Duration::from_millis(50);

#[tokio::test]
async fn establish_any_dead_first_address_falls_through_to_live_second() {
    let fx = fixture();
    let (dead, _dead_guard) = dead_port().await;
    let (live, responder) = spawn_responder(&fx).await;
    let cfg = fast_cfg();

    let est = case::establish_any(
        &[dead, live],
        &fx.creds,
        fx.responder_node_id,
        &cfg,
        TEST_STAGGER,
    )
    .await
    .expect("live second address must win");
    assert_eq!(est.peer, live, "winner is the live candidate");
    let local = est.local.expect("winner reports its local socket");

    let mut session = est.session;
    let value = session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("secured read on the winning session");
    assert_eq!(value, ImValue::Bool(false));

    let observed = responder.await.expect("responder task panicked");
    assert_eq!(
        observed.port(),
        local.port(),
        "responder saw the winning attempt's socket (wildcard-bound, so compare ports)"
    );
}

#[tokio::test]
async fn establish_any_all_dead_reports_every_peer() {
    let fx = fixture();
    let (dead1, _dead_guard) = dead_port().await;
    let (dead2, _dead_guard2) = dead_port().await;
    let cfg = fast_cfg();

    let err = case::establish_any(
        &[dead1, dead2],
        &fx.creds,
        fx.responder_node_id,
        &cfg,
        TEST_STAGGER,
    )
    .await
    .err()
    .expect("all-dead must fail");
    match err {
        EstablishAnyError::AllFailed(list) => {
            let peers: Vec<SocketAddr> = list.iter().map(|(p, _)| *p).collect();
            assert_eq!(peers, vec![dead1, dead2], "every peer, in candidate order");
            let text = format!("{}", EstablishAnyError::AllFailed(list));
            assert!(
                text.starts_with("CASE failed on all 2 address(es): "),
                "{text}"
            );
            assert!(text.contains(&dead1.to_string()) && text.contains(&dead2.to_string()));
        }
        other => panic!("expected AllFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn establish_any_no_candidates_is_no_addresses() {
    let fx = fixture();
    let cfg = fast_cfg();
    let err = case::establish_any(&[], &fx.creds, fx.responder_node_id, &cfg, TEST_STAGGER)
        .await
        .err()
        .expect("empty candidates must fail");
    assert!(matches!(err, EstablishAnyError::NoAddresses), "{err:?}");
    assert_eq!(format!("{err}"), "no addresses");
}

#[tokio::test]
async fn establish_any_live_first_wins_without_touching_second() {
    let fx = fixture();
    let (live1, responder1) = spawn_responder(&fx).await;
    let (live2, responder2) = spawn_responder(&fx).await;
    let cfg = fast_cfg();

    let est = case::establish_any(
        &[live1, live2],
        &fx.creds,
        fx.responder_node_id,
        &cfg,
        // Longer than a loopback handshake so the second attempt never starts.
        Duration::from_secs(5),
    )
    .await
    .expect("first live address wins");
    assert_eq!(est.peer, live1);

    let mut session = est.session;
    session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("read on winner");
    responder1.await.expect("responder 1 completes");

    // Responder 2 only resolves once it has served a handshake; it must
    // still be waiting.
    let untouched = tokio::time::timeout(Duration::from_millis(200), responder2).await;
    assert!(
        untouched.is_err(),
        "second responder must not have seen a Sigma1"
    );
}
