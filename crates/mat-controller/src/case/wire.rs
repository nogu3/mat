//! CASE wire codec — the Sigma1/2/3 framing, TBS/TBE payloads, ECDH and the
//! HKDF salt/nonce/info material, in **one** copy shared by both roles:
//! [`crate::case`] (initiator) and [`crate::case_responder`] (responder).
//!
//! Both sides used to carry their own byte-identical copies of these
//! encoders; a drift between them is an interop bug that only a live
//! handshake would catch (`case_self_handshake` is that pin). Parse errors
//! are bare `&'static str` labels so each role can wrap them in its own
//! error type (`CaseError::Sigma2Malformed` / `CaseCoreError::Decode`)
//! without this module knowing either.

use sha2::{Digest, Sha256};

use crate::tlv::{skip_container, Reader, Tag, Value, Writer};

/// AEAD nonce for TBE2 (Sigma2's `encrypted2`), spec §4.14.2.
pub(crate) const TBE2_NONCE: &[u8; 13] = b"NCASE_Sigma2N";
/// AEAD nonce for TBE3 (Sigma3's `encrypted3`), spec §4.14.2.
pub(crate) const TBE3_NONCE: &[u8; 13] = b"NCASE_Sigma3N";
/// HKDF info for S2K, spec §4.14.2.2.
pub(crate) const INFO_S2K: &[u8] = b"Sigma2";
/// HKDF info for S3K, spec §4.14.2.4.
pub(crate) const INFO_S3K: &[u8] = b"Sigma3";

/// Sigma1's decoded fields (responder side).
pub struct Sigma1 {
    pub initiator_random: [u8; 32],
    pub initiator_session_id: u16,
    pub dest_id: [u8; 32],
    pub initiator_eph_pub: [u8; 65],
}

/// Sigma2's decoded fields (initiator side).
#[derive(Debug, PartialEq, Eq)]
pub struct Sigma2 {
    pub responder_random: [u8; 32],
    pub responder_session_id: u16,
    pub responder_eph_pub: [u8; 65],
    pub encrypted2: Vec<u8>,
}

/// A decrypted TBE payload's fields (TBE2 on the initiator side, TBE3 on the
/// responder side — same shape).
pub struct Tbe {
    pub noc: Vec<u8>,
    pub icac: Option<Vec<u8>>,
    pub signature: [u8; 64],
}

/// Encodes Sigma1: `struct{1: random, 2: session_id, 3: dest_id, 4: eph_pub}`.
/// No optional fields (resumption, session params) are sent. `pub` (Task 10):
/// `mat-device`'s CASE responder unit test builds Sigma1 with this same
/// initiator-authoritative encoder rather than hand-rolling a second copy
/// (re-exported as `crate::case::encode_sigma1`, where it has always lived).
pub fn encode_sigma1(
    random: &[u8; 32],
    session_id: u16,
    dest_id: &[u8; 32],
    eph_pub: &[u8; 65],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), random);
    w.put_uint(Tag::Context(2), u64::from(session_id));
    w.put_bytes(Tag::Context(3), dest_id);
    w.put_bytes(Tag::Context(4), eph_pub);
    w.end_container();
    w.finish()
}

/// Parses Sigma1: `struct{1: random, 2: session_id, 3: dest_id, 4: eph_pub,
/// ...}` (any optional fields past tag 4 — resumption, session params — are
/// ignored; [`encode_sigma1`] never sends them). Resumption fields (tag
/// 6/7) are deliberately tolerated and ignored — full-handshake fallback per
/// spec §4.14.2; Sigma2Resume is out of M2 scope.
pub(crate) fn parse_sigma1(payload: &[u8]) -> Result<Sigma1, &'static str> {
    let mut r = Reader::new(payload);
    match r.next().map_err(|_| "sigma1 tlv")?.map(|e| e.value) {
        Some(Value::StructStart) => {}
        _ => return Err("sigma1 top-level struct"),
    }
    let mut random: Option<[u8; 32]> = None;
    let mut session_id: Option<u16> = None;
    let mut dest: Option<[u8; 32]> = None;
    let mut eph: Option<[u8; 65]> = None;
    loop {
        let el = r
            .next()
            .map_err(|_| "sigma1 tlv")?
            .ok_or("sigma1 truncated")?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => {
                random = Some(b.try_into().map_err(|_| "sigma1 random length")?);
            }
            (Tag::Context(2), Value::Uint(v)) => {
                session_id = Some(u16::try_from(v).map_err(|_| "sigma1 session id")?);
            }
            // initiatorSessionParams（tag 5 の struct）等のネストは丸ごと
            // 読み飛ばす — 中身の context tag をこのレベルの field と
            // 誤読しない（matter.js は必ず sessionParams を含める）。
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r).map_err(|_| "sigma1 tlv")?;
            }
            (Tag::Context(3), Value::Bytes(b)) => {
                dest = Some(b.try_into().map_err(|_| "sigma1 dest id length")?);
            }
            (Tag::Context(4), Value::Bytes(b)) => {
                eph = Some(b.try_into().map_err(|_| "sigma1 eph length")?);
            }
            _ => {}
        }
    }
    Ok(Sigma1 {
        initiator_random: random.ok_or("sigma1 random")?,
        initiator_session_id: session_id.ok_or("sigma1 session id")?,
        dest_id: dest.ok_or("sigma1 dest id")?,
        initiator_eph_pub: eph.ok_or("sigma1 eph")?,
    })
}

/// Encodes Sigma2: `struct{1: responder_random, 2: responder_session_id,
/// 3: responder_eph_pub, 4: encrypted2}`.
pub(crate) fn encode_sigma2(
    random: &[u8; 32],
    session_id: u16,
    eph: &[u8; 65],
    encrypted2: &[u8],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), random);
    w.put_uint(Tag::Context(2), u64::from(session_id));
    w.put_bytes(Tag::Context(3), eph);
    w.put_bytes(Tag::Context(4), encrypted2);
    w.end_container();
    w.finish()
}

/// Parses Sigma2: `struct{1: responder_random, 2: responder_session_id,
/// 3: responder_eph_pub, 4: encrypted2, [5: session params (skipped)]}`.
pub(crate) fn parse_sigma2(payload: &[u8]) -> Result<Sigma2, &'static str> {
    let mut r = Reader::new(payload);
    match r.next().map_err(|_| "tlv")?.map(|e| e.value) {
        Some(Value::StructStart) => {}
        _ => return Err("top-level struct"),
    }

    let mut responder_random: Option<[u8; 32]> = None;
    let mut responder_session_id: Option<u16> = None;
    let mut responder_eph_pub: Option<[u8; 65]> = None;
    let mut encrypted2: Option<Vec<u8>> = None;

    loop {
        let el = r.next().map_err(|_| "tlv")?.ok_or("truncated")?;
        match el.value {
            Value::ContainerEnd => break,
            Value::Bytes(b) if el.tag == Tag::Context(1) => {
                responder_random = Some(b.try_into().map_err(|_| "responder random length")?);
            }
            Value::Uint(v) if el.tag == Tag::Context(2) => {
                responder_session_id = Some(u16::try_from(v).map_err(|_| "responder session id")?);
            }
            Value::Bytes(b) if el.tag == Tag::Context(3) => {
                responder_eph_pub =
                    Some(b.try_into().map_err(|_| "responder ephemeral key length")?);
            }
            Value::Bytes(b) if el.tag == Tag::Context(4) => {
                encrypted2 = Some(b.to_vec());
            }
            Value::StructStart | Value::ArrayStart | Value::ListStart => {
                skip_container(&mut r).map_err(|_| "tlv")?;
            }
            _ => {} // unknown/unsupported scalar field: ignore
        }
    }

    let responder_session_id = responder_session_id.ok_or("responder session id")?;
    if responder_session_id == 0 {
        return Err("responder session id must be non-zero");
    }

    Ok(Sigma2 {
        responder_random: responder_random.ok_or("responder random")?,
        responder_session_id,
        responder_eph_pub: responder_eph_pub.ok_or("responder ephemeral key")?,
        encrypted2: encrypted2.ok_or("encrypted2")?,
    })
}

/// Sigma3 wire payload: `struct{1: encrypted3}`.
pub(crate) fn encode_sigma3(encrypted3: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), encrypted3);
    w.end_container();
    w.finish()
}

/// Extracts the single context-1 byte string from a Sigma3 payload
/// (`struct{1: encrypted3}`).
pub(crate) fn parse_sigma3(payload: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut r = Reader::new(payload);
    match r.next().map_err(|_| "sigma3 tlv")?.map(|e| e.value) {
        Some(Value::StructStart) => {}
        _ => return Err("sigma3 top-level struct"),
    }
    let mut enc: Option<Vec<u8>> = None;
    loop {
        let el = r
            .next()
            .map_err(|_| "sigma3 tlv")?
            .ok_or("sigma3 truncated")?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => enc = Some(b.to_vec()),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r).map_err(|_| "sigma3 tlv")?;
            }
            _ => {}
        }
    }
    enc.ok_or("sigma3 missing encrypted3")
}

/// TBS payload signed over in Sigma2/Sigma3:
/// `struct{1: noc, [2: icac], 3: sender_eph_pub, 4: receiver_eph_pub}`
/// (sender before receiver — both roles depend on that order).
pub(crate) fn encode_tbs(
    noc: &[u8],
    icac: Option<&[u8]>,
    sender_eph: &[u8; 65],
    receiver_eph: &[u8; 65],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), noc);
    if let Some(icac) = icac {
        w.put_bytes(Tag::Context(2), icac);
    }
    w.put_bytes(Tag::Context(3), sender_eph);
    w.put_bytes(Tag::Context(4), receiver_eph);
    w.end_container();
    w.finish()
}

/// TBE plaintext: `struct{1: noc, [2: icac], 3: signature, [4: resumptionID]}`
/// (spec §4.14.2). `resumption_id` is `Some` only for TBE2 (the responder's
/// Sigma2): chip's Sigma2 parser expects TBE2's tag 4 unconditionally. The
/// initiator's TBE3 passes `None` — TBE3 has no resumption id.
pub(crate) fn encode_tbe(
    noc: &[u8],
    icac: Option<&[u8]>,
    sig: &[u8; 64],
    resumption_id: Option<&[u8; 16]>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), noc);
    if let Some(icac) = icac {
        w.put_bytes(Tag::Context(2), icac);
    }
    w.put_bytes(Tag::Context(3), sig);
    if let Some(id) = resumption_id {
        w.put_bytes(Tag::Context(4), id);
    }
    w.end_container();
    w.finish()
}

/// Parses a decrypted TBE payload: `struct{1: noc, [2: icac], 3: signature,
/// ...}` (any trailing optional fields — e.g. resumption id — are ignored).
pub(crate) fn parse_tbe(payload: &[u8]) -> Result<Tbe, &'static str> {
    let mut r = Reader::new(payload);
    match r.next().map_err(|_| "tbe tlv")?.map(|e| e.value) {
        Some(Value::StructStart) => {}
        _ => return Err("tbe top-level struct"),
    }
    let mut noc: Option<Vec<u8>> = None;
    let mut icac: Option<Vec<u8>> = None;
    let mut sig: Option<[u8; 64]> = None;
    loop {
        let el = r.next().map_err(|_| "tbe tlv")?.ok_or("tbe truncated")?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => noc = Some(b.to_vec()),
            (Tag::Context(2), Value::Bytes(b)) => icac = Some(b.to_vec()),
            (Tag::Context(3), Value::Bytes(b)) => {
                sig = Some(b.try_into().map_err(|_| "tbe signature length")?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r).map_err(|_| "tbe tlv")?;
            }
            _ => {}
        }
    }
    Ok(Tbe {
        noc: noc.ok_or("tbe noc")?,
        icac,
        signature: sig.ok_or("tbe signature")?,
    })
}

/// ECDH between our ephemeral secret and the peer's ephemeral public key.
/// `None` = peer public key is not a valid P-256 point.
pub(crate) fn ecdh(secret: &p256::SecretKey, peer_pub: &[u8; 65]) -> Option<[u8; 32]> {
    let pk = p256::PublicKey::from_sec1_bytes(peer_pub).ok()?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), pk.as_affine());
    let mut out = [0u8; 32];
    out.copy_from_slice(shared.raw_secret_bytes().as_slice());
    Some(out)
}

/// S2K salt (spec §4.14.2.2): `IPK || responderRandom || responderEphPubKey
/// || SHA256(Sigma1)`.
pub(crate) fn s2k_salt(
    ipk: &[u8; 16],
    responder_random: &[u8; 32],
    responder_eph_pub: &[u8; 65],
    sigma1_hash: &[u8; 32],
) -> Vec<u8> {
    let mut salt = Vec::with_capacity(16 + 32 + 65 + 32);
    salt.extend_from_slice(ipk);
    salt.extend_from_slice(responder_random);
    salt.extend_from_slice(responder_eph_pub);
    salt.extend_from_slice(sigma1_hash);
    salt
}

/// S3K salt (spec §4.14.2.4): `IPK || SHA256(Sigma1 || Sigma2)`.
pub(crate) fn s3k_salt(ipk: &[u8; 16], sigma12_hash: &[u8; 32]) -> Vec<u8> {
    let mut salt = Vec::with_capacity(16 + 32);
    salt.extend_from_slice(ipk);
    salt.extend_from_slice(sigma12_hash);
    salt
}

/// SHA-256 of one contiguous buffer (CASE transcript hashes).
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn sigma1_roundtrip_and_ignores_nested_session_params() {
        let bytes = encode_sigma1(&[0x42; 32], 0x1234, &[0x24; 32], &[0x04; 65]);
        let s1 = parse_sigma1(&bytes).unwrap();
        assert_eq!(s1.initiator_random, [0x42; 32]);
        assert_eq!(s1.initiator_session_id, 0x1234);
        assert_eq!(s1.dest_id, [0x24; 32]);
        assert_eq!(s1.initiator_eph_pub, [0x04; 65]);
        // matter.js style: initiatorSessionParams (tag 5) must not clobber tag 2.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[0x42; 32]);
        w.put_uint(Tag::Context(2), 0x1234);
        w.put_bytes(Tag::Context(3), &[0x24; 32]);
        w.put_bytes(Tag::Context(4), &[0x04; 65]);
        w.start_struct(Tag::Context(5));
        w.put_uint(Tag::Context(2), 300);
        w.end_container();
        w.end_container();
        assert_eq!(
            parse_sigma1(&w.finish()).unwrap().initiator_session_id,
            0x1234
        );
    }

    #[test]
    fn sigma2_roundtrip_rejects_zero_session_id() {
        let bytes = encode_sigma2(&[0x11; 32], 0x1234, &[0x22; 65], b"encrypted-blob");
        let s2 = parse_sigma2(&bytes).unwrap();
        assert_eq!(s2.responder_session_id, 0x1234);
        assert_eq!(s2.encrypted2, b"encrypted-blob");
        let zero = encode_sigma2(&[0x11; 32], 0, &[0x22; 65], b"x");
        assert_eq!(
            parse_sigma2(&zero),
            Err("responder session id must be non-zero")
        );
    }

    #[test]
    fn sigma3_roundtrip() {
        assert_eq!(parse_sigma3(&encode_sigma3(b"enc3")).unwrap(), b"enc3");
        assert_eq!(
            parse_sigma3(&[0x15, 0x18]),
            Err("sigma3 missing encrypted3")
        );
    }

    #[test]
    fn tbe_roundtrip_with_and_without_resumption_id() {
        let t = parse_tbe(&encode_tbe(b"noc", Some(b"icac"), &[0x77; 64], None)).unwrap();
        assert_eq!(
            (t.noc.as_slice(), t.icac.as_deref(), t.signature),
            (&b"noc"[..], Some(&b"icac"[..]), [0x77; 64])
        );
        let with = encode_tbe(b"noc", None, &[0x77; 64], Some(&[0x88; 16]));
        let t = parse_tbe(&with).unwrap();
        assert_eq!(t.icac, None);
        // tag 4 is present on the wire
        let mut r = Reader::new(&with);
        let mut saw = false;
        while let Some(el) = r.next().unwrap() {
            if el.tag == Tag::Context(4) {
                assert_eq!(el.value, Value::Bytes(&[0x88; 16]));
                saw = true;
            }
        }
        assert!(saw);
    }

    #[test]
    fn tbs_puts_sender_before_receiver() {
        let b = encode_tbs(b"noc", None, &[0xAA; 65], &[0xBB; 65]);
        let mut r = Reader::new(&b);
        r.next().unwrap(); // struct
        r.next().unwrap(); // noc
        let e = r.next().unwrap().unwrap();
        assert_eq!(
            (e.tag, e.value),
            (Tag::Context(3), Value::Bytes(&[0xAA; 65]))
        );
        let e = r.next().unwrap().unwrap();
        assert_eq!(
            (e.tag, e.value),
            (Tag::Context(4), Value::Bytes(&[0xBB; 65]))
        );
    }

    #[test]
    fn ecdh_rejects_off_curve_point() {
        let sk = crate::case::random_p256_secret();
        assert!(ecdh(&sk, &[0x04; 65]).is_none());
        let peer = crate::case::random_p256_secret();
        let a = ecdh(&sk, &crate::case::eph_pub_bytes(&peer)).unwrap();
        let b = ecdh(&peer, &crate::case::eph_pub_bytes(&sk)).unwrap();
        assert_eq!(a, b);
    }
}
