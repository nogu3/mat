//! CASE responder — thin, `FabricEntry`-based wrapper around
//! `mat_controller::case_responder::CaseResponderCore`, the pure state
//! machine that actually implements the protocol (spec §4.14). Pure — no
//! tokio, no sockets, no files (checked by `cargo check -p mat-device
//! --no-default-features` in CI); the net-side driver
//! (`crate::net::case`, gated behind the `net` feature) owns transport /
//! `ResponderExchange` plumbing and calls [`CaseResponderCore::on_message`]
//! once per received message.
//!
//! **Why this is a wrapper, not the implementation**: `mat-device` depends
//! on `mat-controller`, never the reverse, and `mat-controller`'s own
//! `test_support::responder_task` needs the same state machine (Task 10
//! Step 6 migrates it onto `case_responder::CaseResponderCore` too, as a
//! regression guard proving both crates' CASE responder behavior stays
//! identical). So the actual protocol logic — Sigma1/2/3 framing, transcript
//! hashing, TBS/TBE codecs, fabric selection, chain verification — lives in
//! `mat_controller::case_responder` (see that module's doc comment for the
//! full rationale, mirroring how `spake2p` hosts both PASE roles in one
//! crate). This module's only job is converting `mat-device`'s own
//! [`FabricEntry`] (fabric persistence + AddNOC bookkeeping fields
//! `mat-controller` has no business knowing about) into
//! `case_responder::CaseFabric` (just the fields CASE itself needs), and
//! re-exporting [`CaseOutput`]/[`CaseCoreError`] unchanged.
use mat_controller::case_responder::CaseFabric;

pub use mat_controller::case_responder::{CaseCoreError, CaseOutput};

use crate::core::fabric_store::FabricEntry;

impl From<FabricEntry> for CaseFabric {
    fn from(e: FabricEntry) -> Self {
        CaseFabric {
            fabric_index: e.fabric_index,
            root_tlv: e.root_tlv,
            noc_tlv: e.noc_tlv,
            icac_tlv: e.icac_tlv,
            op_private_key: e.op_private_key,
            ipk_operational: e.ipk_operational,
            node_id: e.node_id,
            fabric_id: e.fabric_id,
            root_public_key: e.root_public_key,
        }
    }
}

/// `FabricEntry`-based CASE responder core. Wraps
/// `mat_controller::case_responder::CaseResponderCore` — see the module doc
/// for why the actual state machine lives there.
pub struct CaseResponderCore(mat_controller::case_responder::CaseResponderCore);

impl CaseResponderCore {
    pub fn new(fabrics: Vec<FabricEntry>, responder_session_id: u16) -> Self {
        let fabrics = fabrics.into_iter().map(CaseFabric::from).collect();
        Self(mat_controller::case_responder::CaseResponderCore::new(
            fabrics,
            responder_session_id,
        ))
    }

    /// Feeds one incoming opcode+payload, returns the response to send (or
    /// the terminal `Established` output). See
    /// `case_responder::CaseResponderCore::on_message` for the full
    /// contract (state machine, error semantics).
    pub fn on_message(&mut self, opcode: u8, payload: &[u8]) -> Result<CaseOutput, CaseCoreError> {
        self.0.on_message(opcode, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::case::{encode_sigma1, eph_pub_bytes, random_p256_secret};
    use mat_controller::cert::MatterCert;
    use mat_controller::fabric::case_destination_id;
    use mat_controller::test_support::{ICA01, IPK, NODE01_NOC, NODE01_PRIV, ROOT01_CHIP};
    use mat_controller::tlv::{Reader, Tag, Value};

    /// A `mat-device`-facing smoke test that the `FabricEntry -> CaseFabric`
    /// conversion is wired correctly end-to-end (Sigma1 -> Sigma2). Full
    /// protocol correctness (fabric selection, TBS/TBE framing, transcript
    /// hashing, Sigma3, session-key derivation) is `case_responder`'s own
    /// unit-test responsibility (mat-controller) plus this crate's
    /// `tests/case_establish.rs` integration test against the production
    /// initiator — this test only needs to prove the wrapper itself, not
    /// re-cover the state machine.
    fn fabric_entry() -> FabricEntry {
        let noc_cert = MatterCert::parse(NODE01_NOC).expect("parse node01_01 noc");
        let root_cert = MatterCert::parse(ROOT01_CHIP).expect("parse root01");
        FabricEntry {
            fabric_index: 1,
            root_tlv: ROOT01_CHIP.to_vec(),
            noc_tlv: NODE01_NOC.to_vec(),
            icac_tlv: Some(ICA01.to_vec()),
            op_private_key: NODE01_PRIV.try_into().expect("32 bytes"),
            ipk_operational: IPK,
            node_id: noc_cert.node_id().expect("node id"),
            fabric_id: noc_cert.fabric_id().expect("fabric id"),
            root_public_key: root_cert.pub_key,
            admin_subject: 0,
        }
    }

    // Sigma1/Sigma2 opcodes (spec §4.14). `case_responder::OPCODE_SIGMA1`/
    // `OPCODE_SIGMA2` are `pub(crate)` to mat-controller (the net driver
    // doesn't need them — see `net::case`'s doc comment), so this test uses
    // the literal wire values directly.
    const OPCODE_SIGMA1: u8 = 0x30;
    const OPCODE_SIGMA2: u8 = 0x31;

    #[test]
    fn wraps_fabric_entry_and_answers_sigma1_with_sigma2() {
        let entry = fabric_entry();
        let mut core = CaseResponderCore::new(vec![entry.clone()], 0xB0B1);

        let initiator_secret = random_p256_secret();
        let initiator_eph = eph_pub_bytes(&initiator_secret);
        let initiator_random = [0x42u8; 32];
        let dest_id = case_destination_id(
            &entry.ipk_operational,
            &initiator_random,
            &entry.root_public_key,
            entry.fabric_id,
            entry.node_id,
        );
        let sigma1 = encode_sigma1(&initiator_random, 0x1234, &dest_id, &initiator_eph);

        let CaseOutput::Reply(sigma2_bytes, opcode) =
            core.on_message(OPCODE_SIGMA1, &sigma1).unwrap()
        else {
            panic!("expected Reply");
        };
        assert_eq!(opcode, OPCODE_SIGMA2);

        // Sanity: the reply decodes as a well-formed Sigma2 struct with our
        // configured responder session id in tag2.
        let mut r = Reader::new(&sigma2_bytes);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut session_id = None;
        loop {
            let el = r.next().unwrap().expect("truncated sigma2");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(2), Value::Uint(v)) => session_id = Some(v as u16),
                _ => {}
            }
        }
        assert_eq!(session_id, Some(0xB0B1));
    }
}
