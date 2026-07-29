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

use std::sync::Arc;

use mat_controller::case;
use mat_controller::cert::{verify_noc_chain, MatterCert};
use mat_controller::fabric::FabricCredentials;
use mat_controller::im::{self, ImValue};
use mat_controller::kvs::SelfIssueMaterials;
use mat_controller::test_support::{
    fast_cfg, responder_task, ICA01, INITIATOR_NODE_ID, IPK, NODE01_NOC, NODE01_PRIV, ROOT01_CHIP,
    ROOT01_PRIV,
};
use mat_controller::transport::{Transport, UdpTransport};

#[tokio::test]
async fn case_establishes_and_reads_over_loopback() {
    // Responder identity: node01_01 (+ ica01 + root01). Parse its node/fabric
    // id so the initiator's self-issued NOC shares the same fabric.
    let noc_cert = MatterCert::parse(NODE01_NOC).expect("parse node01_01 NOC");
    let responder_node_id = noc_cert.node_id().expect("node id");
    let responder_fabric_id = noc_cert.fabric_id().expect("fabric id");
    // Sanity: node01_01 chains to root01 through ica01 (so the initiator, which
    // trusts root01, will accept it in Sigma2).
    let ica_cert = MatterCert::parse(ICA01).unwrap();
    let root_cert = MatterCert::parse(ROOT01_CHIP).unwrap();
    verify_noc_chain(&noc_cert, Some(&ica_cert), &root_cert).expect("fixture chain");

    // Responder socket first, so we can hand its address to the initiator.
    let responder_transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let responder_addr = responder_transport.local_addr().unwrap();

    let op_priv: [u8; 32] = NODE01_PRIV.try_into().unwrap();
    let responder = tokio::spawn(responder_task(
        responder_transport,
        INITIATOR_NODE_ID,
        responder_node_id,
        NODE01_NOC.to_vec(),
        ICA01.to_vec(),
        op_priv,
        ROOT01_CHIP.to_vec(),
    ));

    // Initiator: fresh self-issued NOC under root01, same IPK and fabric id.
    let materials = SelfIssueMaterials {
        rcac: ROOT01_CHIP.to_vec(),
        root_private_key: ROOT01_PRIV.try_into().unwrap(),
        ipk_operational: IPK,
        node_id: INITIATOR_NODE_ID,
        fabric_id: responder_fabric_id,
    };
    let creds = FabricCredentials::from_self_issued(materials).expect("self-issued creds");

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
        &creds,
        responder_node_id,
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
