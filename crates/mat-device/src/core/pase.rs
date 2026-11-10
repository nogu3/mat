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
use mat_controller::pase::derive_session_keys;
use mat_controller::pase::{
    self, OPCODE_PASE_PAKE1, OPCODE_PASE_PAKE2, OPCODE_PASE_PAKE3, OPCODE_PBKDF_PARAM_REQUEST,
    OPCODE_PBKDF_PARAM_RESPONSE,
};
use mat_controller::session::SessionKeys;
use mat_controller::spake2p::{Spake2pVerifier, SpakeError};

/// PASE の secret 供給源。`Passcode` は起動時窓（QR の passcode から
/// w0/w1 を導出）、`VerifierMaterial` は ECM 窓（OpenCommissioningWindow が
/// 渡す w0‖L をそのまま使う — こちらは w1 を知らない）。
#[derive(Clone)]
pub enum PaseSecret {
    Passcode(u32),
    VerifierMaterial([u8; 97]),
}

/// Verifier-side configuration: the PASE secret (passcode or verifier
/// material) plus the PBKDF salt/iterations this responder advertises in
/// PBKDFParamResponse, and the local (responder) session id it assigns
/// itself (spec §4.13.1.2's `responderSessionId`).
pub struct PaseVerifierConfig {
    pub secret: PaseSecret,
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

    /// Whether this handshake has put a passcode guess to the test without
    /// it being confirmed — i.e. Pake2 has been produced (state
    /// `AwaitPake3`) but Pake3 never verified. This is the definition of a
    /// *failed PASE attempt* the commissioning window's brute-force cap
    /// ([`PaseAttemptLimiter`]) counts, whatever way the handshake then
    /// ended: a wrong cA ([`PaseCoreError::ConfirmMismatch`]), a
    /// StatusReport/other opcode instead of Pake3, or silence (the net
    /// driver's receive timeout).
    ///
    /// Why Pake2 is the line: this responder sends cB in Pake2 *before* it
    /// sees cA, and the peer's pA already commits to one passcode guess,
    /// so the moment Pake2 leaves, the peer can check its guess against cB
    /// locally and walk away — an attacker never needs to send Pake3.
    /// Counting only wrong-cA failures would therefore not bound guesses
    /// at all. Conversely, nothing before Pake2 reveals anything about the
    /// passcode (PBKDFParamResponse carries only public salt/iterations; a
    /// Pake1 with an invalid pA fails before any cB is computed), so those
    /// failures — malformed/out-of-order datagrams, invalid points, a peer
    /// that never sends Pake1 — are not counted: they test no guess, and
    /// counting them would let any LAN host close the window with 20
    /// garbage packets without running SPAKE2+ at all. A peer that does
    /// send a well-formed Pake1 and walks away *is* counted even if it was
    /// never really guessing — it is indistinguishable from one that was.
    pub fn has_unconfirmed_guess(&self) -> bool {
        matches!(self.state, State::AwaitPake3 { .. })
    }

    fn handle_pbkdf_param_request(&mut self, payload: &[u8]) -> Result<PaseOutput, PaseCoreError> {
        let req = pase::decode_pbkdf_param_request(payload)?;
        let mut responder_random = [0u8; 32];
        getrandom::fill(&mut responder_random).expect("os rng");
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
        let verifier = match &self.config.secret {
            PaseSecret::Passcode(passcode) => {
                Spake2pVerifier::from_passcode(*passcode, &self.config.salt, self.config.iterations)
            }
            PaseSecret::VerifierMaterial(material) => {
                Spake2pVerifier::from_verifier_material(material).map_err(PaseCoreError::Spake)?
            }
        };
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
        // cA is a MAC keyed by the SPAKE2+ shared secret: compare in constant time.
        if !mat_controller::crypto::ct_eq(&c_a, &expected_c_a) {
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

/// Failed PASE attempts a single commissioning window tolerates before it
/// must close. The Matter Core Specification requires a device to close
/// its commissioning window once 20 PASE attempts have failed: the setup
/// passcode is only ~27 bits, so this cap — not the passcode's entropy
/// alone — is the online brute-force defence.
pub const PASE_MAX_FAILED_ATTEMPTS: u8 = 20;

/// What [`PaseAttemptLimiter::record_failure`] decided about one failed
/// handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaseFailureVerdict {
    /// The handshake failed before testing a passcode guess (see
    /// [`PaseResponderCore::has_unconfirmed_guess`]) — not counted.
    NotCounted,
    /// Counted; the window stays open. `failures` is the new total.
    Counted { failures: u8 },
    /// Counted, and the total has reached [`PASE_MAX_FAILED_ATTEMPTS`]: the
    /// caller must close the commissioning window now.
    LimitReached,
}

/// Per-commissioning-window failed-PASE-attempt counter. Pure bookkeeping:
/// the runtime owns one per open window (a fresh one each time a window
/// opens, so the count resets per window) and acts on the verdict.
///
/// A *successful* PASE does not reset the count: the cap bounds how many
/// guesses one window admits, and a success (the passcode holder getting
/// in) does not make further guesses against the same, still-valid
/// passcode any less dangerous — if commissioning then stalls and the
/// window stays open, an attacker must not get a fresh budget. A legitimate
/// commissioner never comes near the cap (it knows the passcode), so this
/// costs it nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaseAttemptLimiter {
    failures: u8,
}

impl PaseAttemptLimiter {
    /// Failed attempts counted so far in this window.
    pub fn failures(&self) -> u8 {
        self.failures
    }

    /// Records one failed handshake. `guess_tested` is the failed core's
    /// [`PaseResponderCore::has_unconfirmed_guess`]; only those count.
    /// Saturates at [`PASE_MAX_FAILED_ATTEMPTS`] and keeps answering
    /// `LimitReached` from there on.
    pub fn record_failure(&mut self, guess_tested: bool) -> PaseFailureVerdict {
        if !guess_tested {
            return PaseFailureVerdict::NotCounted;
        }
        self.failures = self
            .failures
            .saturating_add(1)
            .min(PASE_MAX_FAILED_ATTEMPTS);
        if self.failures >= PASE_MAX_FAILED_ATTEMPTS {
            PaseFailureVerdict::LimitReached
        } else {
            PaseFailureVerdict::Counted {
                failures: self.failures,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::spake2p::{self, Spake2pProver};

    fn cfg() -> PaseVerifierConfig {
        PaseVerifierConfig {
            secret: PaseSecret::Passcode(20202021),
            salt: b"SPAKE2P Key Salt".to_vec(),
            iterations: 1000,
            responder_session_id: 0xB0B1,
        }
    }

    /// Drives a full handshake (PBKDFParamRequest -> Pake1 -> Pake3) against
    /// a responder built from `config`, using a `Spake2pProver` initiator
    /// that knows `passcode` — no I/O, no `ResponderExchange`. Works for a
    /// `PaseSecret::Passcode(passcode)` responder as well as a
    /// `PaseSecret::VerifierMaterial` responder built from
    /// `compute_verifier(passcode, ..)` with the same salt/iterations,
    /// since both land on the same w0. Also asserts the derived session
    /// keys agree with an independent prover-side HKDF computation
    /// (key-derivation direction check). Returns the terminal
    /// `PaseOutput::Established` so callers can assert on its fields.
    fn drive_full_handshake(config: PaseVerifierConfig, passcode: u32) -> PaseOutput {
        let expected_responder_session_id = config.responder_session_id;
        let expected_salt = config.salt.clone();
        let expected_iterations = config.iterations;
        let mut core = PaseResponderCore::new(config);

        // --- PBKDFParamRequest -> PBKDFParamResponse ---
        let req = pase::encode_pbkdf_param_request(&[9u8; 32], 0x0011);
        let PaseOutput::Reply(resp_bytes, op) =
            core.on_message(OPCODE_PBKDF_PARAM_REQUEST, &req).unwrap()
        else {
            panic!("expected Reply")
        };
        assert_eq!(op, OPCODE_PBKDF_PARAM_RESPONSE);

        let resp = pase::decode_pbkdf_param_response(&resp_bytes).unwrap();
        assert_eq!(resp.responder_session_id, expected_responder_session_id);
        assert_eq!(resp.iterations, expected_iterations);
        assert_eq!(resp.salt, expected_salt);

        // --- prover side (drives the handshake exactly as a real
        // initiator would, using the same public codecs/crypto) ---
        let context = pase::pake_context(&req, &resp_bytes);
        let (w0, w1) = spake2p::derive_w0_w1(passcode, &resp.salt, resp.iterations);
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
        let output = core.on_message(OPCODE_PASE_PAKE3, &pake3).unwrap();
        let PaseOutput::Established {
            ref reply,
            opcode,
            ref keys,
            peer_session_id,
        } = output
        else {
            panic!("expected Established")
        };
        assert_eq!(opcode, OPCODE_STATUS_REPORT);
        assert_eq!(reply, &[0u8; 8]);
        assert_eq!(peer_session_id, 0x0011);

        // --- session keys must match an independent prover-side derivation ---
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&[]), &shared.k_e);
        let mut okm = [0u8; 48];
        hk.expand(b"SessionKeys", &mut okm).unwrap();
        assert_eq!(keys.i2r, okm[..16]);
        assert_eq!(keys.r2i, okm[16..32]);
        assert_eq!(keys.attestation_challenge, okm[32..]);

        output
    }

    /// Full handshake with a `PaseSecret::Passcode` responder (startup
    /// commissioning window).
    #[test]
    fn pase_core_full_handshake() {
        drive_full_handshake(cfg(), 20202021);
    }

    /// ECM（OpenCommissioningWindow）: passcode ではなく verifier 素材
    /// （w0‖L, 97B）で responder を構成してもハンドシェイクが成立する。
    /// verifier は initiator と同じ passcode/salt/iterations から
    /// `compute_verifier` で作る（実運用では OCW が 97B を直接渡してくる）。
    #[test]
    fn ecm_verifier_material_handshake_succeeds() {
        use mat_controller::spake2p::compute_verifier;
        let salt = [0x5A; 16];
        let iterations = 1000;
        let material = compute_verifier(31415926, &salt, iterations);
        let config = PaseVerifierConfig {
            secret: PaseSecret::VerifierMaterial(material),
            salt: salt.to_vec(),
            iterations,
            responder_session_id: 0xB0B2,
        };
        drive_full_handshake(config, 31415926);
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

    // ── failed-attempt accounting (commissioning-window brute-force cap) ──

    /// Drives `core` through PBKDFParamRequest + Pake1 with a prover that
    /// knows `passcode`, returning the Pake3 that prover would send.
    fn drive_to_pake3(core: &mut PaseResponderCore, passcode: u32) -> Vec<u8> {
        let req = pase::encode_pbkdf_param_request(&[1u8; 32], 0x22);
        let PaseOutput::Reply(resp_bytes, _) =
            core.on_message(OPCODE_PBKDF_PARAM_REQUEST, &req).unwrap()
        else {
            panic!("expected Reply")
        };
        let resp = pase::decode_pbkdf_param_response(&resp_bytes).unwrap();
        let context = pase::pake_context(&req, &resp_bytes);
        let (w0, w1) = spake2p::derive_w0_w1(passcode, &resp.salt, resp.iterations);
        let prover = Spake2pProver::new(w0, w1);
        let pake1 = pase::encode_pake1(&prover.p_a());
        let PaseOutput::Reply(pake2_bytes, _) = core.on_message(OPCODE_PASE_PAKE1, &pake1).unwrap()
        else {
            panic!("expected Reply")
        };
        let (p_b, _) = pase::decode_pake2(&pake2_bytes).unwrap();
        let shared = prover.finish(&p_b, &context, b"", b"").unwrap();
        pase::encode_pake3(&shared.c_a)
    }

    /// Before Pake2 goes out no passcode guess has been tested: a fresh
    /// core, one that only answered PBKDFParamRequest, and one that
    /// rejected a garbage Pake1 all report `false`.
    #[test]
    fn no_unconfirmed_guess_before_pake2_is_sent() {
        let core = PaseResponderCore::new(cfg());
        assert!(!core.has_unconfirmed_guess());

        let mut core = PaseResponderCore::new(cfg());
        let req = pase::encode_pbkdf_param_request(&[1u8; 32], 0x22);
        core.on_message(OPCODE_PBKDF_PARAM_REQUEST, &req).unwrap();
        assert!(!core.has_unconfirmed_guess());
        // A Pake1 whose pA is not a valid point fails before Pake2 exists.
        let bad = pase::encode_pake1(&[0u8; 65]);
        assert!(core.on_message(OPCODE_PASE_PAKE1, &bad).is_err());
        assert!(!core.has_unconfirmed_guess());

        // Garbage as the very first message.
        let mut core = PaseResponderCore::new(cfg());
        assert!(core.on_message(OPCODE_PASE_PAKE3, &[0xFF]).is_err());
        assert!(!core.has_unconfirmed_guess());
    }

    /// Once Pake2 (carrying cB) has been produced the peer can check its
    /// guess, so the attempt counts whether it then sends a wrong cA, a
    /// StatusReport, or nothing at all.
    #[test]
    fn unconfirmed_guess_after_pake2_until_confirmed() {
        // Wrong passcode: Pake3 mismatch leaves the guess unconfirmed.
        let mut core = PaseResponderCore::new(cfg());
        let pake3 = drive_to_pake3(&mut core, 999_999);
        assert!(core.has_unconfirmed_guess(), "abandoned after Pake2");
        assert!(matches!(
            core.on_message(OPCODE_PASE_PAKE3, &pake3),
            Err(PaseCoreError::ConfirmMismatch)
        ));
        assert!(core.has_unconfirmed_guess(), "wrong cA");

        // Peer aborts with a StatusReport instead of Pake3 (what
        // `mat_controller::pase::establish` does on a cB mismatch).
        let mut core = PaseResponderCore::new(cfg());
        drive_to_pake3(&mut core, 999_999);
        assert!(core.on_message(OPCODE_STATUS_REPORT, &[0u8; 8]).is_err());
        assert!(core.has_unconfirmed_guess());

        // Right passcode: Established is not an unconfirmed guess.
        let mut core = PaseResponderCore::new(cfg());
        let pake3 = drive_to_pake3(&mut core, 20202021);
        assert!(matches!(
            core.on_message(OPCODE_PASE_PAKE3, &pake3).unwrap(),
            PaseOutput::Established { .. }
        ));
        assert!(!core.has_unconfirmed_guess());
    }

    #[test]
    fn attempt_limiter_ignores_attempts_without_a_tested_guess() {
        let mut limiter = PaseAttemptLimiter::default();
        for _ in 0..100 {
            assert_eq!(
                limiter.record_failure(false),
                PaseFailureVerdict::NotCounted
            );
        }
        assert_eq!(limiter.failures(), 0);
    }

    #[test]
    fn attempt_limiter_closes_on_the_twentieth_counted_failure() {
        assert_eq!(PASE_MAX_FAILED_ATTEMPTS, 20);
        let mut limiter = PaseAttemptLimiter::default();
        for n in 1..PASE_MAX_FAILED_ATTEMPTS {
            assert_eq!(
                limiter.record_failure(true),
                PaseFailureVerdict::Counted { failures: n }
            );
            // Uncounted noise in between changes nothing.
            assert_eq!(
                limiter.record_failure(false),
                PaseFailureVerdict::NotCounted
            );
        }
        assert_eq!(limiter.failures(), PASE_MAX_FAILED_ATTEMPTS - 1);
        assert_eq!(
            limiter.record_failure(true),
            PaseFailureVerdict::LimitReached
        );
        assert_eq!(limiter.failures(), PASE_MAX_FAILED_ATTEMPTS);
        // Saturates (the runtime has closed the window by now; a late
        // caller still gets `LimitReached`, never a wrapped counter).
        assert_eq!(
            limiter.record_failure(true),
            PaseFailureVerdict::LimitReached
        );
        assert_eq!(limiter.failures(), PASE_MAX_FAILED_ATTEMPTS);
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
