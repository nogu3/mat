//! PASE responder state machine (verifier / B side, spec §4.13). Pure —
//! one opcode+payload in, one [`PaseOutput`] out, no tokio, no sockets, no
//! files. The net-side driver (`crate::net::pase`, gated behind the `net`
//! feature) owns transport/`ResponderExchange` plumbing and calls
//! [`PaseResponderCore::on_message`] once per received message; this module
//! never touches I/O (checked by `cargo check -p mat-device
//! --no-default-features` in CI).
//!
//! Mirrors `mat_controller::pase::establish`'s initiator flow with roles
//! swapped, and follows the same wire order proven by
//! `mat_controller::test_support::pase_responder_task` (a hand-rolled test
//! responder driven directly over a raw socket, not through this crate).
//!
//! State machine: `Idle --PBKDFParamRequest--> AwaitPake1 --Pake1-->
//! AwaitPake3 --Pake3--> Established`. Any opcode that doesn't match the
//! current state (including replays and out-of-order messages) is rejected
//! with [`PaseCoreError::UnexpectedMessage`] — the caller is expected to
//! answer with a StatusReport failure and drop the exchange; this core
//! never retries or recovers on its own.

use mat_controller::message::OPCODE_STATUS_REPORT;
use mat_controller::pase::{
    self, OPCODE_PASE_PAKE1, OPCODE_PASE_PAKE2, OPCODE_PASE_PAKE3, OPCODE_PBKDF_PARAM_REQUEST,
    OPCODE_PBKDF_PARAM_RESPONSE,
};
use mat_controller::session::SessionKeys;
use mat_controller::spake2p::{Spake2pVerifier, SpakeError};

/// HKDF info string for the post-PASE session key derivation (spec
/// §4.13.2.3). Matches `mat_controller::pase::establish`'s constant of the
/// same name.
const INFO_SESSION_KEYS: &[u8] = b"SessionKeys";

/// Verifier-side configuration: the setup passcode plus the PBKDF salt/
/// iterations this responder advertises in PBKDFParamResponse, and the
/// local (responder) session id it assigns itself (spec §4.13.1.2's
/// `responderSessionId`).
pub struct PaseVerifierConfig {
    pub passcode: u32,
    pub salt: Vec<u8>,
    pub iterations: u32,
    pub responder_session_id: u16,
}

/// One `on_message` step's result. `Reply` is a mid-handshake response
/// (PBKDFParamResponse or Pake2) the caller must send back reliably.
/// `Established` is the terminal StatusReport(success) plus the derived
/// session keys and the peer's (initiator's) session id — the caller needs
/// both to build a `SecureSession::new_device_role(...)`.
pub enum PaseOutput {
    Reply(Vec<u8>, u8),
    Established {
        reply: Vec<u8>,
        opcode: u8,
        keys: SessionKeys,
        peer_session_id: u16,
    },
}

/// Protocol violation. The caller (net driver) answers with a StatusReport
/// failure and drops the exchange — this core never retries or recovers.
#[derive(Debug)]
pub enum PaseCoreError {
    /// Wrong opcode for the current state (covers replays/out-of-order
    /// messages too — the state machine only accepts the one opcode its
    /// current state expects).
    UnexpectedMessage(u8),
    /// A `mat_controller::pase` decoder rejected the payload.
    Decode(pase::PaseError),
    /// The peer's SPAKE2+ share point (pA) was invalid (bad point or the
    /// identity element).
    Spake(SpakeError),
    /// Pake3's cA didn't match our computed confirmation — wrong passcode.
    ConfirmMismatch,
}

impl std::fmt::Display for PaseCoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaseCoreError::UnexpectedMessage(op) => {
                write!(f, "pase: unexpected opcode 0x{op:02X} for current state")
            }
            PaseCoreError::Decode(e) => write!(f, "pase: {e}"),
            PaseCoreError::Spake(e) => write!(f, "pase: {e}"),
            PaseCoreError::ConfirmMismatch => {
                write!(f, "pase: confirmation mismatch (wrong passcode?)")
            }
        }
    }
}

impl std::error::Error for PaseCoreError {}

impl From<pase::PaseError> for PaseCoreError {
    fn from(e: pase::PaseError) -> Self {
        PaseCoreError::Decode(e)
    }
}

impl From<SpakeError> for PaseCoreError {
    fn from(e: SpakeError) -> Self {
        PaseCoreError::Spake(e)
    }
}

/// Data carried between steps. All fields are `Copy` (fixed-size arrays /
/// u16), so the enum itself is `Copy` — `on_message` can read `self.state`
/// by value before overwriting it.
#[derive(Clone, Copy)]
enum State {
    Idle,
    AwaitPake1 {
        /// PAKE context hash (spec §4.13.1.2), computed once from the raw
        /// PBKDFParamRequest/Response wire bytes.
        context: [u8; 32],
        /// The initiator's `PBKDFParamRequest.initiatorSessionId` — carried
        /// through to `Established` so the caller can build a session.
        peer_session_id: u16,
    },
    AwaitPake3 {
        expected_c_a: [u8; 32],
        k_e: [u8; 16],
        peer_session_id: u16,
    },
    Established,
}

/// Pure PASE responder state machine (verifier/B side). Feed it one
/// opcode+payload at a time via [`on_message`](Self::on_message); it never
/// touches I/O.
pub struct PaseResponderCore {
    config: PaseVerifierConfig,
    state: State,
}

impl PaseResponderCore {
    pub fn new(config: PaseVerifierConfig) -> Self {
        Self {
            config,
            state: State::Idle,
        }
    }

    /// Feeds one incoming opcode+payload, returns the response to send (or
    /// the terminal `Established` output). `Err` on any protocol
    /// violation — the state is left unchanged (this core never retries).
    pub fn on_message(&mut self, opcode: u8, payload: &[u8]) -> Result<PaseOutput, PaseCoreError> {
        match self.state {
            State::Idle => {
                if opcode != OPCODE_PBKDF_PARAM_REQUEST {
                    return Err(PaseCoreError::UnexpectedMessage(opcode));
                }
                self.handle_pbkdf_param_request(payload)
            }
            State::AwaitPake1 {
                context,
                peer_session_id,
            } => {
                if opcode != OPCODE_PASE_PAKE1 {
                    return Err(PaseCoreError::UnexpectedMessage(opcode));
                }
                self.handle_pake1(payload, context, peer_session_id)
            }
            State::AwaitPake3 {
                expected_c_a,
                k_e,
                peer_session_id,
            } => {
                if opcode != OPCODE_PASE_PAKE3 {
                    return Err(PaseCoreError::UnexpectedMessage(opcode));
                }
                self.handle_pake3(payload, expected_c_a, k_e, peer_session_id)
            }
            State::Established => Err(PaseCoreError::UnexpectedMessage(opcode)),
        }
    }

    fn handle_pbkdf_param_request(&mut self, payload: &[u8]) -> Result<PaseOutput, PaseCoreError> {
        let req = pase::decode_pbkdf_param_request(payload)?;
        let mut responder_random = [0u8; 32];
        getrandom::getrandom(&mut responder_random).expect("os rng");
        let resp_bytes = pase::encode_pbkdf_param_response(
            &req.initiator_random,
            &responder_random,
            self.config.responder_session_id,
            self.config.iterations,
            &self.config.salt,
        );
        let context = pase::pake_context(payload, &resp_bytes);
        self.state = State::AwaitPake1 {
            context,
            peer_session_id: req.initiator_session_id,
        };
        Ok(PaseOutput::Reply(resp_bytes, OPCODE_PBKDF_PARAM_RESPONSE))
    }

    fn handle_pake1(
        &mut self,
        payload: &[u8],
        context: [u8; 32],
        peer_session_id: u16,
    ) -> Result<PaseOutput, PaseCoreError> {
        let p_a = pase::decode_pake1(payload)?;
        let verifier = Spake2pVerifier::from_passcode(
            self.config.passcode,
            &self.config.salt,
            self.config.iterations,
        );
        let p_b = verifier.p_b();
        let shared = verifier.finish(&p_a, &context, b"", b"")?;
        let reply = pase::encode_pake2(&p_b, &shared.c_b);
        self.state = State::AwaitPake3 {
            expected_c_a: shared.expected_c_a,
            k_e: shared.k_e,
            peer_session_id,
        };
        Ok(PaseOutput::Reply(reply, OPCODE_PASE_PAKE2))
    }

    fn handle_pake3(
        &mut self,
        payload: &[u8],
        expected_c_a: [u8; 32],
        k_e: [u8; 16],
        peer_session_id: u16,
    ) -> Result<PaseOutput, PaseCoreError> {
        let c_a = pase::decode_pake3(payload)?;
        if c_a != expected_c_a {
            return Err(PaseCoreError::ConfirmMismatch);
        }
        let keys = derive_session_keys(&k_e);
        self.state = State::Established;
        Ok(PaseOutput::Established {
            reply: vec![0u8; 8], // StatusReport success: general=0, protocol_id=0, code=0
            opcode: OPCODE_STATUS_REPORT,
            keys,
            peer_session_id,
        })
    }
}

/// spec §4.13.2.3: HKDF-SHA256(salt=[], ikm=Ke(16B), info="SessionKeys")
/// 48B -> i2r(16B) || r2i(16B) || attestation_challenge(16B). Replicated
/// locally rather than reused — `mat_controller::pase::establish`'s copy of
/// this expand call is private to that function, and
/// `mat_controller::test_support::hkdf48` is behind the test-only
/// `test-responder` feature — a production driver can't depend on either.
fn derive_session_keys(k_e: &[u8; 16]) -> SessionKeys {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&[]), k_e);
    let mut okm = [0u8; 48];
    hk.expand(INFO_SESSION_KEYS, &mut okm)
        .expect("48 bytes is a valid HKDF-SHA256 output length");
    SessionKeys {
        i2r: okm[..16].try_into().expect("16"),
        r2i: okm[16..32].try_into().expect("16"),
        attestation_challenge: okm[32..].try_into().expect("16"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::spake2p::{self, Spake2pProver};

    fn cfg() -> PaseVerifierConfig {
        PaseVerifierConfig {
            passcode: 20202021,
            salt: b"SPAKE2P Key Salt".to_vec(),
            iterations: 1000,
            responder_session_id: 0xB0B1,
        }
    }

    /// Full handshake driven purely through `mat_controller::pase`'s public
    /// codecs and a `Spake2pProver` — no I/O, no `ResponderExchange`. Also
    /// asserts the derived session keys agree with an independent
    /// prover-side HKDF computation (key-derivation direction check).
    #[test]
    fn pase_core_full_handshake() {
        let mut core = PaseResponderCore::new(cfg());

        // --- PBKDFParamRequest -> PBKDFParamResponse ---
        let req = pase::encode_pbkdf_param_request(&[9u8; 32], 0x0011);
        let PaseOutput::Reply(resp_bytes, op) =
            core.on_message(OPCODE_PBKDF_PARAM_REQUEST, &req).unwrap()
        else {
            panic!("expected Reply")
        };
        assert_eq!(op, OPCODE_PBKDF_PARAM_RESPONSE);

        let resp = pase::decode_pbkdf_param_response(&resp_bytes).unwrap();
        assert_eq!(resp.responder_session_id, 0xB0B1);
        assert_eq!(resp.iterations, 1000);
        assert_eq!(resp.salt, b"SPAKE2P Key Salt");

        // --- prover side (drives the handshake exactly as a real
        // initiator would, using the same public codecs/crypto) ---
        let context = pase::pake_context(&req, &resp_bytes);
        let (w0, w1) = spake2p::derive_w0_w1(20202021, &resp.salt, resp.iterations);
        let prover = Spake2pProver::new(w0, w1);

        // --- Pake1 -> Pake2 ---
        let pake1 = pase::encode_pake1(&prover.p_a());
        let PaseOutput::Reply(pake2_bytes, op) =
            core.on_message(OPCODE_PASE_PAKE1, &pake1).unwrap()
        else {
            panic!("expected Reply")
        };
        assert_eq!(op, OPCODE_PASE_PAKE2);
        let (p_b, c_b) = pase::decode_pake2(&pake2_bytes).unwrap();

        let shared = prover.finish(&p_b, &context, b"", b"").unwrap();
        assert_eq!(
            shared.expected_c_b, c_b,
            "responder cB doesn't match prover's expectation"
        );

        // --- Pake3 -> Established ---
        let pake3 = pase::encode_pake3(&shared.c_a);
        let PaseOutput::Established {
            reply,
            opcode,
            keys,
            peer_session_id,
        } = core.on_message(OPCODE_PASE_PAKE3, &pake3).unwrap()
        else {
            panic!("expected Established")
        };
        assert_eq!(opcode, OPCODE_STATUS_REPORT);
        assert_eq!(reply, [0u8; 8]);
        assert_eq!(peer_session_id, 0x0011);

        // --- session keys must match an independent prover-side derivation ---
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&[]), &shared.k_e);
        let mut okm = [0u8; 48];
        hk.expand(b"SessionKeys", &mut okm).unwrap();
        assert_eq!(keys.i2r, okm[..16]);
        assert_eq!(keys.r2i, okm[16..32]);
        assert_eq!(keys.attestation_challenge, okm[32..]);
    }

    #[test]
    fn rejects_pake1_before_pbkdf_param_request() {
        let mut core = PaseResponderCore::new(cfg());
        let pake1 = pase::encode_pake1(&[0u8; 65]);
        assert!(matches!(
            core.on_message(OPCODE_PASE_PAKE1, &pake1),
            Err(PaseCoreError::UnexpectedMessage(op)) if op == OPCODE_PASE_PAKE1
        ));
    }

    #[test]
    fn rejects_replayed_pbkdf_param_request_after_pake1_state() {
        let mut core = PaseResponderCore::new(cfg());
        let req = pase::encode_pbkdf_param_request(&[1u8; 32], 0x22);
        core.on_message(OPCODE_PBKDF_PARAM_REQUEST, &req).unwrap();
        // Now in AwaitPake1 — a second PBKDFParamRequest (replay/reorder) is rejected.
        assert!(matches!(
            core.on_message(OPCODE_PBKDF_PARAM_REQUEST, &req),
            Err(PaseCoreError::UnexpectedMessage(op)) if op == OPCODE_PBKDF_PARAM_REQUEST
        ));
    }

    #[test]
    fn rejects_confirm_mismatch_on_wrong_passcode() {
        let mut core = PaseResponderCore::new(cfg());
        let req = pase::encode_pbkdf_param_request(&[1u8; 32], 0x22);
        let PaseOutput::Reply(resp_bytes, _) =
            core.on_message(OPCODE_PBKDF_PARAM_REQUEST, &req).unwrap()
        else {
            panic!("expected Reply")
        };
        let resp = pase::decode_pbkdf_param_response(&resp_bytes).unwrap();
        let context = pase::pake_context(&req, &resp_bytes);

        // Wrong passcode on the prover side -> cA won't match.
        let (w0, w1) = spake2p::derive_w0_w1(999_999, &resp.salt, resp.iterations);
        let prover = Spake2pProver::new(w0, w1);
        let pake1 = pase::encode_pake1(&prover.p_a());
        let PaseOutput::Reply(pake2_bytes, _) = core.on_message(OPCODE_PASE_PAKE1, &pake1).unwrap()
        else {
            panic!("expected Reply")
        };
        let (p_b, _c_b) = pase::decode_pake2(&pake2_bytes).unwrap();
        let shared = prover.finish(&p_b, &context, b"", b"").unwrap();
        let pake3 = pase::encode_pake3(&shared.c_a);
        assert!(matches!(
            core.on_message(OPCODE_PASE_PAKE3, &pake3),
            Err(PaseCoreError::ConfirmMismatch)
        ));
    }

    #[test]
    fn rejects_messages_after_established() {
        let mut core = PaseResponderCore::new(cfg());
        let req = pase::encode_pbkdf_param_request(&[1u8; 32], 0x22);
        let PaseOutput::Reply(resp_bytes, _) =
            core.on_message(OPCODE_PBKDF_PARAM_REQUEST, &req).unwrap()
        else {
            panic!("expected Reply")
        };
        let resp = pase::decode_pbkdf_param_response(&resp_bytes).unwrap();
        let context = pase::pake_context(&req, &resp_bytes);
        let (w0, w1) = spake2p::derive_w0_w1(20202021, &resp.salt, resp.iterations);
        let prover = Spake2pProver::new(w0, w1);
        let pake1 = pase::encode_pake1(&prover.p_a());
        let PaseOutput::Reply(pake2_bytes, _) = core.on_message(OPCODE_PASE_PAKE1, &pake1).unwrap()
        else {
            panic!("expected Reply")
        };
        let (p_b, _) = pase::decode_pake2(&pake2_bytes).unwrap();
        let shared = prover.finish(&p_b, &context, b"", b"").unwrap();
        let pake3 = pase::encode_pake3(&shared.c_a);
        assert!(matches!(
            core.on_message(OPCODE_PASE_PAKE3, &pake3).unwrap(),
            PaseOutput::Established { .. }
        ));
        // Any further message on this exchange is rejected.
        assert!(matches!(
            core.on_message(OPCODE_PASE_PAKE3, &pake3),
            Err(PaseCoreError::UnexpectedMessage(op)) if op == OPCODE_PASE_PAKE3
        ));
    }
}
