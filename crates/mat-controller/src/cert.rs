//! Matter operational certificate (NOC/ICAC/RCAC) parsing and verification.
//!
//! Matter certificates are encoded as compact Matter TLV, not X.509 DER.
//! Their ECDSA signature, however, is defined over the SHA-256 hash of the
//! equivalent DER-encoded `TBSCertificate` (Matter Core Spec 1.4 Appendix B).
//! This module parses the TLV form, rebuilds the DER `TBSCertificate` byte
//! for byte, and verifies signatures / chains against it.

use crate::asn1;
use crate::tlv::{Reader, Tag, Value, Writer};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};

/// SubjectKeyIdentifier per Matter/X.509: SHA-1 of the 65-byte public key.
pub fn subject_key_id(public_key: &[u8; 65]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    Sha1::digest(public_key).into()
}

/// Matter epoch (seconds since 2000-01-01T00:00:00Z) for 2021-01-01T00:00:00Z.
pub const MATTER_EPOCH_2021: u32 = 662_688_000;
/// NOC validity period: 10 years.
pub const NOC_VALIDITY_SECS: u32 = 315_360_000;

// EKU (Matter TLV enum values): serverAuth=1, clientAuth=2. NOC uses client, server.
const EKU_CLIENT_AUTH: u64 = 2;
const EKU_SERVER_AUTH: u64 = 1;
const KEY_USAGE_DIGITAL_SIGNATURE: u16 = 0x0001;

/// Build and sign a NOC (2-cert chain: signed directly by `issuer`, no ICAC).
/// `issuer` is the RCAC (self-signed root); `issuer_private_key` its op key.
pub fn issue_noc(
    op_public_key: &[u8; 65],
    node_id: u64,
    fabric_id: u64,
    issuer: &MatterCert,
    issuer_private_key: &[u8; 32],
    serial: &[u8],
) -> Result<MatterCert, CertError> {
    // 発行者(root)の SubjectKeyId を AKID に使う
    let issuer_skid = issuer
        .extensions
        .iter()
        .find_map(|e| match e {
            CertExtension::SubjectKeyId(id) => Some(id.clone()),
            _ => None,
        })
        .ok_or(CertError::Malformed("issuer has no subject key id"))?;

    let extensions = vec![
        CertExtension::BasicConstraints {
            is_ca: false,
            path_len: None,
        },
        CertExtension::KeyUsage(KEY_USAGE_DIGITAL_SIGNATURE),
        CertExtension::ExtendedKeyUsage(vec![EKU_CLIENT_AUTH, EKU_SERVER_AUTH]),
        CertExtension::SubjectKeyId(subject_key_id(op_public_key).to_vec()),
        CertExtension::AuthorityKeyId(issuer_skid),
    ];

    let mut noc = MatterCert {
        serial: serial.to_vec(),
        issuer: issuer.subject.clone(),
        not_before: MATTER_EPOCH_2021,
        not_after: MATTER_EPOCH_2021.saturating_add(NOC_VALIDITY_SECS),
        subject: vec![
            DnAttr {
                tlv_tag: 17,
                value: DnValue::MatterId(node_id),
            },
            DnAttr {
                tlv_tag: 21,
                value: DnValue::MatterId(fabric_id),
            },
        ],
        pub_key: *op_public_key,
        extensions,
        signature: [0u8; 64], // TBS 署名で埋める
    };

    let tbs = noc.tbs_der()?;
    noc.signature = crate::crypto::sign_ecdsa_p256(issuer_private_key, &tbs)
        .map_err(|_| CertError::BadSignature)?;
    Ok(noc)
}

// --- Pre-computed OID constants (DER bytes, tag 0x06 included) ---

const OID_ECDSA_WITH_SHA256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]; // 1.2.840.10045.4.3.2
const OID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]; // 1.2.840.10045.2.1
const OID_PRIME256V1: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]; // 1.2.840.10045.3.1.7
const OID_COMMON_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03]; // 2.5.4.3
                                                                // Matter arc 1.3.6.1.4.1.37244.1.x -> 2B 06 01 04 01 82 A2 7C 01 xx
const OID_MATTER_NODE_ID: &[u8] = &[
    0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x01,
];
const OID_MATTER_FIRMWARE_SIGNING_ID: &[u8] = &[
    0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x02,
];
const OID_MATTER_ICAC_ID: &[u8] = &[
    0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x03,
];
const OID_MATTER_RCAC_ID: &[u8] = &[
    0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x04,
];
const OID_MATTER_FABRIC_ID: &[u8] = &[
    0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x05,
];
const OID_MATTER_NOC_CAT: &[u8] = &[
    0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x06,
];
const OID_EXT_BASIC_CONSTRAINTS: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x13]; // 2.5.29.19
const OID_EXT_KEY_USAGE: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x0F]; // 2.5.29.15
const OID_EXT_EXTENDED_KEY_USAGE: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x25]; // 2.5.29.37
const OID_EXT_SUBJECT_KEY_ID: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x0E]; // 2.5.29.14
const OID_EXT_AUTHORITY_KEY_ID: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x23]; // 2.5.29.35

/// Parsed Matter operational certificate (NOC / ICAC / RCAC).
#[derive(Debug, Clone)]
pub struct MatterCert {
    pub serial: Vec<u8>,
    pub issuer: Vec<DnAttr>,
    pub not_before: u32,
    pub not_after: u32,
    pub subject: Vec<DnAttr>,
    pub pub_key: [u8; 65],
    /// TLV appearance order preserved — required to rebuild the DER TBS.
    pub extensions: Vec<CertExtension>,
    pub signature: [u8; 64],
}

#[derive(Debug, Clone, PartialEq)]
pub struct DnAttr {
    pub tlv_tag: u8,
    pub value: DnValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DnValue {
    MatterId(u64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CertExtension {
    BasicConstraints { is_ca: bool, path_len: Option<u8> },
    KeyUsage(u16),
    ExtendedKeyUsage(Vec<u64>),
    SubjectKeyId(Vec<u8>),
    AuthorityKeyId(Vec<u8>),
}

/// Certificate parse / verification error. No panics on malformed input.
#[derive(Debug)]
pub enum CertError {
    Tlv(crate::tlv::TlvError),
    Malformed(&'static str),
    UnsupportedAlgorithm,
    UnsupportedDnAttr(u8),
    BadSignature,
    BadPublicKey,
}

impl std::fmt::Display for CertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertError::Tlv(e) => write!(f, "certificate tlv error: {e}"),
            CertError::Malformed(what) => {
                write!(f, "malformed certificate: missing/invalid {what}")
            }
            CertError::UnsupportedAlgorithm => write!(f, "unsupported certificate algorithm"),
            CertError::UnsupportedDnAttr(tag) => {
                write!(
                    f,
                    "unsupported distinguished-name attribute tag 0x{tag:02X}"
                )
            }
            CertError::BadSignature => write!(f, "certificate signature verification failed"),
            CertError::BadPublicKey => write!(f, "invalid public key encoding"),
        }
    }
}

impl std::error::Error for CertError {}

impl From<crate::tlv::TlvError> for CertError {
    fn from(e: crate::tlv::TlvError) -> Self {
        CertError::Tlv(e)
    }
}

/// Matter operational certs are ~400-600 bytes (SDK caps them at 600);
/// anything larger is hostile or corrupt, and oversized fields would
/// otherwise reach asn1::tlv's length assert during TBS rebuilding.
const MAX_CERT_TLV_LEN: usize = 1024;

impl MatterCert {
    /// Parse a Matter TLV certificate (anonymous struct, context tags 1..11).
    pub fn parse(tlv_bytes: &[u8]) -> Result<MatterCert, CertError> {
        if tlv_bytes.len() > MAX_CERT_TLV_LEN {
            return Err(CertError::Malformed("certificate too large"));
        }

        let mut r = Reader::new(tlv_bytes);

        match r.next()?.map(|el| el.value) {
            Some(Value::StructStart) => {}
            _ => return Err(CertError::Malformed("top-level struct")),
        }

        let mut serial: Option<Vec<u8>> = None;
        let mut sig_algo_seen = false;
        let mut issuer: Option<Vec<DnAttr>> = None;
        let mut not_before: Option<u32> = None;
        let mut not_after: Option<u32> = None;
        let mut subject: Option<Vec<DnAttr>> = None;
        let mut pubkey_algo_seen = false;
        let mut curve_seen = false;
        let mut pub_key: Option<[u8; 65]> = None;
        let mut extensions: Vec<CertExtension> = Vec::new();
        let mut signature: Option<[u8; 64]> = None;

        loop {
            let el = r
                .next()?
                .ok_or(CertError::Malformed("truncated certificate"))?;
            if let Value::ContainerEnd = el.value {
                break;
            }
            let Tag::Context(t) = el.tag else {
                return Err(CertError::Malformed("expected context-tagged field"));
            };
            match (t, el.value) {
                (1, Value::Bytes(b)) => serial = Some(b.to_vec()),
                (2, Value::Uint(1)) => sig_algo_seen = true,
                (2, Value::Uint(_)) => return Err(CertError::UnsupportedAlgorithm),
                (3, Value::ListStart) => issuer = Some(parse_dn(&mut r)?),
                (4, Value::Uint(v)) => {
                    not_before =
                        Some(u32::try_from(v).map_err(|_| CertError::Malformed("not-before"))?)
                }
                (5, Value::Uint(v)) => {
                    not_after =
                        Some(u32::try_from(v).map_err(|_| CertError::Malformed("not-after"))?)
                }
                (6, Value::ListStart) => subject = Some(parse_dn(&mut r)?),
                (7, Value::Uint(1)) => pubkey_algo_seen = true,
                (7, Value::Uint(_)) => return Err(CertError::UnsupportedAlgorithm),
                (8, Value::Uint(1)) => curve_seen = true,
                (8, Value::Uint(_)) => return Err(CertError::UnsupportedAlgorithm),
                (9, Value::Bytes(b)) => {
                    pub_key = Some(
                        b.try_into()
                            .map_err(|_| CertError::Malformed("public key length"))?,
                    )
                }
                (10, Value::ListStart) => extensions = parse_extensions(&mut r)?,
                (11, Value::Bytes(b)) => {
                    signature = Some(
                        b.try_into()
                            .map_err(|_| CertError::Malformed("signature length"))?,
                    )
                }
                _ => return Err(CertError::Malformed("unexpected certificate field")),
            }
        }

        if !sig_algo_seen {
            return Err(CertError::Malformed("signature algorithm"));
        }
        if !pubkey_algo_seen {
            return Err(CertError::Malformed("public key algorithm"));
        }
        if !curve_seen {
            return Err(CertError::Malformed("elliptic curve"));
        }

        Ok(MatterCert {
            serial: serial.ok_or(CertError::Malformed("serial"))?,
            issuer: issuer.ok_or(CertError::Malformed("issuer"))?,
            not_before: not_before.ok_or(CertError::Malformed("not-before"))?,
            not_after: not_after.ok_or(CertError::Malformed("not-after"))?,
            subject: subject.ok_or(CertError::Malformed("subject"))?,
            pub_key: pub_key.ok_or(CertError::Malformed("public key"))?,
            extensions,
            signature: signature.ok_or(CertError::Malformed("signature"))?,
        })
    }

    /// Rebuild the DER-encoded `TBSCertificate` this certificate's signature
    /// was computed over.
    pub fn tbs_der(&self) -> Result<Vec<u8>, CertError> {
        let issuer = dn_name(&self.issuer)?;
        let subject = dn_name(&self.subject)?;
        let not_before = asn1_time(self.not_before);
        let not_after = asn1_time(self.not_after);

        let mut ext_ders = Vec::with_capacity(self.extensions.len());
        for ext in &self.extensions {
            ext_ders.push(extension_der(ext)?);
        }
        let ext_refs: Vec<&[u8]> = ext_ders.iter().map(Vec::as_slice).collect();
        let extensions_seq = asn1::seq(&ext_refs);

        let spki = asn1::seq(&[
            &asn1::seq(&[OID_EC_PUBLIC_KEY, OID_PRIME256V1]),
            &asn1::bit_string(0, &self.pub_key),
        ]);

        Ok(asn1::seq(&[
            &asn1::context_constructed(0, &asn1::integer(&[2])), // version v3
            &asn1::integer(&self.serial),
            &asn1::seq(&[OID_ECDSA_WITH_SHA256]),
            &issuer,
            &asn1::seq(&[&not_before, &not_after]),
            &subject,
            &spki,
            &asn1::context_constructed(3, &extensions_seq),
        ]))
    }

    /// Verify this certificate's signature was produced by `issuer_public_key`.
    pub fn verify_signed_by(&self, issuer_public_key: &[u8; 65]) -> Result<(), CertError> {
        let key = VerifyingKey::from_sec1_bytes(issuer_public_key)
            .map_err(|_| CertError::BadPublicKey)?;
        let sig = Signature::from_slice(&self.signature).map_err(|_| CertError::BadSignature)?;
        key.verify(&self.tbs_der()?, &sig)
            .map_err(|_| CertError::BadSignature)
    }

    /// Look up a Matter-id-valued subject DN attribute by TLV tag
    /// (17=node, 19=icac, 20=rcac, 21=fabric).
    pub fn subject_matter_id(&self, tlv_tag: u8) -> Option<u64> {
        self.subject.iter().find_map(|attr| {
            if attr.tlv_tag == tlv_tag {
                match attr.value {
                    DnValue::MatterId(id) => Some(id),
                    DnValue::Text(_) => None,
                }
            } else {
                None
            }
        })
    }

    pub fn node_id(&self) -> Option<u64> {
        self.subject_matter_id(17)
    }

    pub fn fabric_id(&self) -> Option<u64> {
        self.subject_matter_id(21)
    }

    /// Encode this certificate back to Matter TLV — inverse of `parse`.
    /// Field/extension order follows the parsed struct (which preserves TLV order).
    pub fn to_tlv(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &self.serial);
        w.put_uint(Tag::Context(2), 1); // ecdsa-with-sha256
        write_dn(&mut w, 3, &self.issuer);
        w.put_uint(Tag::Context(4), u64::from(self.not_before));
        w.put_uint(Tag::Context(5), u64::from(self.not_after));
        write_dn(&mut w, 6, &self.subject);
        w.put_uint(Tag::Context(7), 1); // ec public key
        w.put_uint(Tag::Context(8), 1); // prime256v1
        w.put_bytes(Tag::Context(9), &self.pub_key);
        w.start_list(Tag::Context(10));
        for ext in &self.extensions {
            write_extension(&mut w, ext);
        }
        w.end_container(); // extensions list
        w.put_bytes(Tag::Context(11), &self.signature);
        w.end_container(); // top struct
        w.finish()
    }
}

fn write_dn(w: &mut Writer, ctx: u8, attrs: &[DnAttr]) {
    w.start_list(Tag::Context(ctx));
    for a in attrs {
        match &a.value {
            DnValue::MatterId(id) => w.put_uint(Tag::Context(a.tlv_tag), *id),
            DnValue::Text(s) => w.put_str(Tag::Context(a.tlv_tag), s),
        }
    }
    w.end_container();
}

fn write_extension(w: &mut Writer, ext: &CertExtension) {
    match ext {
        CertExtension::BasicConstraints { is_ca, path_len } => {
            w.start_struct(Tag::Context(1));
            w.put_bool(Tag::Context(1), *is_ca);
            if let Some(pl) = path_len {
                w.put_uint(Tag::Context(2), u64::from(*pl));
            }
            w.end_container();
        }
        CertExtension::KeyUsage(bits) => w.put_uint(Tag::Context(2), u64::from(*bits)),
        CertExtension::ExtendedKeyUsage(purposes) => {
            w.start_array(Tag::Context(3));
            for p in purposes {
                w.put_uint(Tag::Anonymous, *p);
            }
            w.end_container();
        }
        CertExtension::SubjectKeyId(id) => w.put_bytes(Tag::Context(4), id),
        CertExtension::AuthorityKeyId(id) => w.put_bytes(Tag::Context(5), id),
    }
}

/// Verify a NOC's signature chain up to `rcac`, optionally through `icac`,
/// and cross-check issuer/subject DN linkage and fabric-id consistency.
pub fn verify_noc_chain(
    noc: &MatterCert,
    icac: Option<&MatterCert>,
    rcac: &MatterCert,
) -> Result<(), CertError> {
    rcac.verify_signed_by(&rcac.pub_key)?; // self-signed root
    let signer = match icac {
        Some(ica) => {
            ica.verify_signed_by(&rcac.pub_key)?;
            if ica.issuer != rcac.subject {
                return Err(CertError::Malformed("icac issuer != rcac subject"));
            }
            ica
        }
        None => rcac,
    };
    noc.verify_signed_by(&signer.pub_key)?;
    if noc.issuer != signer.subject {
        return Err(CertError::Malformed("noc issuer != signer subject"));
    }
    if noc.node_id().is_none() || noc.fabric_id().is_none() {
        return Err(CertError::Malformed("noc missing node/fabric id"));
    }
    if let Some(ica) = icac {
        if let (Some(a), Some(b)) = (ica.fabric_id(), noc.fabric_id()) {
            if a != b {
                return Err(CertError::Malformed("fabric id mismatch in chain"));
            }
        }
    }
    Ok(())
}

/// True if `matter_epoch_now` falls within `cert`'s `[not_before, not_after]`
/// validity window (`not_after == 0` means no expiry). `verify_noc_chain`
/// itself does not check validity — the time source (wall clock vs. some
/// other reference) is a decision for the CASE call site / M4, not this
/// module — so callers that need a validity check call this separately.
pub fn cert_time_valid(cert: &MatterCert, matter_epoch_now: u32) -> bool {
    matter_epoch_now >= cert.not_before
        && (cert.not_after == 0 || matter_epoch_now <= cert.not_after)
}

/// Parse a DN (issuer/subject) TLV list until its `ContainerEnd`.
fn parse_dn(r: &mut Reader<'_>) -> Result<Vec<DnAttr>, CertError> {
    let mut attrs = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(CertError::Malformed("truncated distinguished name"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::Uint(v) => {
                let Tag::Context(t) = el.tag else {
                    return Err(CertError::Malformed("dn attribute tag"));
                };
                attrs.push(DnAttr {
                    tlv_tag: t,
                    value: DnValue::MatterId(v),
                });
            }
            Value::Utf8(s) => {
                let Tag::Context(t) = el.tag else {
                    return Err(CertError::Malformed("dn attribute tag"));
                };
                attrs.push(DnAttr {
                    tlv_tag: t,
                    value: DnValue::Text(s.to_string()),
                });
            }
            _ => return Err(CertError::Malformed("dn value")),
        }
    }
    Ok(attrs)
}

/// Parse the extensions TLV list until its `ContainerEnd`, preserving order.
fn parse_extensions(r: &mut Reader<'_>) -> Result<Vec<CertExtension>, CertError> {
    let mut exts = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(CertError::Malformed("truncated extensions"))?;
        if let Value::ContainerEnd = el.value {
            break;
        }
        let Tag::Context(t) = el.tag else {
            return Err(CertError::Malformed("extension tag"));
        };
        match (t, el.value) {
            (1, Value::StructStart) => {
                let mut is_ca: Option<bool> = None;
                let mut path_len: Option<u8> = None;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(CertError::Malformed("truncated basic-constraints"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::Bool(b) if e2.tag == Tag::Context(1) => is_ca = Some(b),
                        Value::Uint(v) if e2.tag == Tag::Context(2) => {
                            path_len = Some(
                                u8::try_from(v).map_err(|_| CertError::Malformed("path-len"))?,
                            );
                        }
                        _ => return Err(CertError::Malformed("basic-constraints field")),
                    }
                }
                exts.push(CertExtension::BasicConstraints {
                    is_ca: is_ca.ok_or(CertError::Malformed("is-ca"))?,
                    path_len,
                });
            }
            (2, Value::Uint(v)) => {
                let v = u16::try_from(v).map_err(|_| CertError::Malformed("key-usage"))?;
                exts.push(CertExtension::KeyUsage(v));
            }
            (3, Value::ArrayStart) => {
                let mut vals = Vec::new();
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(CertError::Malformed("truncated extended-key-usage"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::Uint(v) => vals.push(v),
                        _ => return Err(CertError::Malformed("extended-key-usage value")),
                    }
                }
                exts.push(CertExtension::ExtendedKeyUsage(vals));
            }
            (4, Value::Bytes(b)) => exts.push(CertExtension::SubjectKeyId(b.to_vec())),
            (5, Value::Bytes(b)) => exts.push(CertExtension::AuthorityKeyId(b.to_vec())),
            (6, _) => return Err(CertError::Malformed("future-extension unsupported")),
            _ => return Err(CertError::Malformed("extension field")),
        }
    }
    Ok(exts)
}

/// DER `Name` (issuer/subject) from Matter DN attributes:
/// `SEQUENCE of SET OF SEQUENCE { OID, value }`.
fn dn_name(attrs: &[DnAttr]) -> Result<Vec<u8>, CertError> {
    let mut rdns: Vec<Vec<u8>> = Vec::with_capacity(attrs.len());
    for attr in attrs {
        let base_tag = attr.tlv_tag & 0x7F;
        let atv = match (base_tag, &attr.value) {
            (17, DnValue::MatterId(id)) => asn1::seq(&[OID_MATTER_NODE_ID, &hex16(*id)]),
            (18, DnValue::MatterId(id)) => {
                asn1::seq(&[OID_MATTER_FIRMWARE_SIGNING_ID, &hex16(*id)])
            }
            (19, DnValue::MatterId(id)) => asn1::seq(&[OID_MATTER_ICAC_ID, &hex16(*id)]),
            (20, DnValue::MatterId(id)) => asn1::seq(&[OID_MATTER_RCAC_ID, &hex16(*id)]),
            (21, DnValue::MatterId(id)) => asn1::seq(&[OID_MATTER_FABRIC_ID, &hex16(*id)]),
            (22, DnValue::MatterId(id)) => asn1::seq(&[OID_MATTER_NOC_CAT, &hex8(*id)]),
            (1, DnValue::Text(s)) => {
                let value = if attr.tlv_tag & 0x80 != 0 {
                    asn1::printable_string(s)
                } else {
                    asn1::utf8_string(s)
                };
                asn1::seq(&[OID_COMMON_NAME, &value])
            }
            _ => return Err(CertError::UnsupportedDnAttr(attr.tlv_tag)),
        };
        rdns.push(asn1::set_of(&[&atv]));
    }
    let refs: Vec<&[u8]> = rdns.iter().map(Vec::as_slice).collect();
    Ok(asn1::seq(&refs))
}

/// Matter 64-bit id -> uppercase 16-hex-digit UTF8String DER value.
fn hex16(id: u64) -> Vec<u8> {
    asn1::utf8_string(&format!("{id:016X}"))
}

/// Matter NOC CAT (32-bit) -> uppercase 8-hex-digit UTF8String DER value.
fn hex8(id: u64) -> Vec<u8> {
    asn1::utf8_string(&format!("{id:08X}"))
}

/// DER `Extension` for a single parsed extension, preserving criticality
/// per RFC 5280 conventions used by chip-cert.
fn extension_der(ext: &CertExtension) -> Result<Vec<u8>, CertError> {
    Ok(match ext {
        CertExtension::BasicConstraints { is_ca, path_len } => {
            let mut inner: Vec<Vec<u8>> = Vec::new();
            if *is_ca {
                inner.push(asn1::boolean(true));
            }
            if let Some(len) = path_len {
                inner.push(asn1::integer(&[*len]));
            }
            let inner_refs: Vec<&[u8]> = inner.iter().map(Vec::as_slice).collect();
            let value = asn1::seq(&inner_refs);
            asn1::seq(&[
                OID_EXT_BASIC_CONSTRAINTS,
                &asn1::boolean(true),
                &asn1::octet_string(&value),
            ])
        }
        CertExtension::KeyUsage(bits) => {
            let (unused, bytes) = key_usage_bits(*bits);
            let value = asn1::bit_string(unused, &bytes);
            asn1::seq(&[
                OID_EXT_KEY_USAGE,
                &asn1::boolean(true),
                &asn1::octet_string(&value),
            ])
        }
        CertExtension::ExtendedKeyUsage(vals) => {
            let mut oids = Vec::with_capacity(vals.len());
            for v in vals {
                oids.push(eku_oid(*v)?);
            }
            let oid_refs: Vec<&[u8]> = oids.iter().map(Vec::as_slice).collect();
            let value = asn1::seq(&oid_refs);
            asn1::seq(&[
                OID_EXT_EXTENDED_KEY_USAGE,
                &asn1::boolean(true),
                &asn1::octet_string(&value),
            ])
        }
        CertExtension::SubjectKeyId(id) => {
            let value = asn1::octet_string(id);
            asn1::seq(&[OID_EXT_SUBJECT_KEY_ID, &asn1::octet_string(&value)])
        }
        CertExtension::AuthorityKeyId(id) => {
            let value = asn1::seq(&[&asn1::context_primitive(0, id)]);
            asn1::seq(&[OID_EXT_AUTHORITY_KEY_ID, &asn1::octet_string(&value)])
        }
    })
}

/// RFC 5280 KeyUsage named-bit encoding: TLV uint bit `i` (LSB =
/// digitalSignature = bit 0) -> DER BIT STRING bit `i` at
/// `bytes[i/8] |= 0x80 >> (i%8)`, trailing zero octets trimmed.
/// Supports up to bit 8 (decipherOnly), i.e. at most 2 octets.
fn key_usage_bits(bits: u16) -> (u8, Vec<u8>) {
    let mut bytes = [0u8; 2];
    let mut highest: Option<u32> = None;
    for i in 0..16u32 {
        if bits & (1 << i) != 0 {
            bytes[(i / 8) as usize] |= 0x80 >> (i % 8);
            highest = Some(i);
        }
    }
    match highest {
        None => (0, Vec::new()),
        Some(h) => {
            let num_bytes = (h / 8) as usize + 1;
            let unused = 7 - (h % 8) as u8;
            (unused, bytes[..num_bytes].to_vec())
        }
    }
}

/// ExtendedKeyUsage TLV value -> OID DER bytes.
/// 1=serverAuth, 2=clientAuth, 3=codeSigning, 4=emailProtection,
/// 5=timeStamping, 6=OCSPSigning (1.3.6.1.5.5.7.3.x).
fn eku_oid(v: u64) -> Result<Vec<u8>, CertError> {
    let x: u8 = match v {
        1 => 0x01,
        2 => 0x02,
        3 => 0x03,
        4 => 0x04,
        5 => 0x08,
        6 => 0x09,
        _ => return Err(CertError::Malformed("unsupported extended-key-usage value")),
    };
    Ok(vec![
        0x06, 0x08, 0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, x,
    ])
}

/// Matter epoch (seconds since 2000-01-01T00:00:00Z) -> DER time value.
/// Epoch 0 is the "no well-defined expiration" sentinel
/// (chip `ChipEpochToASN1Time` parity): GeneralizedTime "99991231235959Z".
fn asn1_time(epoch: u32) -> Vec<u8> {
    if epoch == 0 {
        return asn1::generalized_time("99991231235959Z");
    }
    let unix = i64::from(epoch) + 946_684_800;
    let days = unix.div_euclid(86400);
    let secs_of_day = unix.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    if (1950..=2049).contains(&year) {
        let yy = (year % 100) as u32;
        asn1::utc_time(&format!(
            "{yy:02}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z"
        ))
    } else {
        asn1::generalized_time(&format!(
            "{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z"
        ))
    }
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_CHIP: &[u8] = include_bytes!("../tests/fixtures/root01_chip.bin");
    const ROOT_DER: &[u8] = include_bytes!("../tests/fixtures/root01_der.bin");
    const ROOT_PUB: &[u8] = include_bytes!("../tests/fixtures/root01_pubkey.bin");
    const ICA_CHIP: &[u8] = include_bytes!("../tests/fixtures/ica01_chip.bin");
    const ICA_DER: &[u8] = include_bytes!("../tests/fixtures/ica01_der.bin");
    const ICA_PUB: &[u8] = include_bytes!("../tests/fixtures/ica01_pubkey.bin");
    const NODE_CHIP: &[u8] = include_bytes!("../tests/fixtures/node01_01_chip.bin");
    const NODE_DER: &[u8] = include_bytes!("../tests/fixtures/node01_01_der.bin");

    /// DER 証明書 = SEQUENCE { TBSCertificate, sigAlg, sigValue }。
    /// 先頭要素 (TBSCertificate) のバイト列を切り出す。
    fn tbs_of(der: &[u8]) -> &[u8] {
        assert_eq!(der[0], 0x30);
        let (hdr, _) = der_len(&der[1..]);
        let inner = &der[1 + hdr..];
        assert_eq!(inner[0], 0x30);
        let (h2, l2) = der_len(&inner[1..]);
        &inner[..1 + h2 + l2]
    }

    fn der_len(buf: &[u8]) -> (usize, usize) {
        if buf[0] < 0x80 {
            (1, buf[0] as usize)
        } else {
            let n = (buf[0] & 0x7F) as usize;
            let mut len = 0usize;
            for b in &buf[1..1 + n] {
                len = len << 8 | *b as usize;
            }
            (1 + n, len)
        }
    }

    #[test]
    fn parses_node_cert_ids() {
        let node = MatterCert::parse(NODE_CHIP).unwrap();
        assert!(node.node_id().is_some());
        assert!(node.fabric_id().is_some());
        let root = MatterCert::parse(ROOT_CHIP).unwrap();
        assert_eq!(root.pub_key.as_slice(), ROOT_PUB);
        assert!(root.subject_matter_id(20).is_some()); // rcac id
    }

    #[test]
    fn rebuilt_tbs_matches_der_vectors_byte_for_byte() {
        for (chip, der) in [
            (ROOT_CHIP, ROOT_DER),
            (ICA_CHIP, ICA_DER),
            (NODE_CHIP, NODE_DER),
        ] {
            let cert = MatterCert::parse(chip).unwrap();
            assert_eq!(cert.tbs_der().unwrap().as_slice(), tbs_of(der));
        }
    }

    #[test]
    fn verifies_signatures_and_chain() {
        let root = MatterCert::parse(ROOT_CHIP).unwrap();
        let ica = MatterCert::parse(ICA_CHIP).unwrap();
        let node = MatterCert::parse(NODE_CHIP).unwrap();
        root.verify_signed_by(&root.pub_key.clone()).unwrap(); // self-signed
        ica.verify_signed_by(&root.pub_key.clone()).unwrap();
        node.verify_signed_by(&ICA_PUB.try_into().unwrap()).unwrap();
        verify_noc_chain(&node, Some(&ica), &root).unwrap();
        // ICAC 抜きだと NOC の署名が root で検証できず失敗する
        assert!(verify_noc_chain(&node, None, &root).is_err());
    }

    #[test]
    fn rejects_tampered_cert() {
        let mut sig_flipped = MatterCert::parse(NODE_CHIP).unwrap();
        sig_flipped.signature[0] ^= 0x01;
        assert!(matches!(
            sig_flipped.verify_signed_by(&ICA_PUB.try_into().unwrap()),
            Err(CertError::BadSignature)
        ));
        let mut subj_changed = MatterCert::parse(NODE_CHIP).unwrap();
        if let Some(DnAttr {
            value: DnValue::MatterId(id),
            ..
        }) = subj_changed.subject.iter_mut().find(|a| a.tlv_tag == 17)
        {
            *id ^= 1;
        }
        assert!(matches!(
            subj_changed.verify_signed_by(&ICA_PUB.try_into().unwrap()),
            Err(CertError::BadSignature)
        ));
    }

    #[test]
    fn rejects_malformed_tlv() {
        assert!(MatterCert::parse(&[0x15, 0x18]).is_err()); // 空 struct
        assert!(MatterCert::parse(b"junk").is_err());
    }

    #[test]
    fn rejects_oversized_cert() {
        let oversized = vec![0x15; 2000];
        let result = MatterCert::parse(&oversized);
        assert!(matches!(result, Err(CertError::Malformed(_))));
    }

    #[test]
    fn subject_key_id_is_sha1_of_pubkey() {
        use sha1::{Digest, Sha1};
        let node = MatterCert::parse(NODE_CHIP).unwrap();
        let expected: [u8; 20] = Sha1::digest(node.pub_key).into();
        assert_eq!(subject_key_id(&node.pub_key), expected);
    }

    #[test]
    fn to_tlv_roundtrips_all_fixtures() {
        // パース → 再エンコード → 元の TLV バイトと完全一致（エンコーダ正しさの決定的アンカー）
        for chip in [ROOT_CHIP, ICA_CHIP, NODE_CHIP] {
            let cert = MatterCert::parse(chip).unwrap();
            assert_eq!(cert.to_tlv().as_slice(), chip, "re-encode must byte-match");
        }
    }

    #[test]
    fn cert_time_valid_checks_notbefore_notafter_window() {
        let root = MatterCert::parse(ROOT_CHIP).unwrap();
        let root_priv: [u8; 32] = include_bytes!("../tests/fixtures/root01_privkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        let op_pub: [u8; 65] = include_bytes!("../tests/fixtures/node01_01_pubkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        let noc = issue_noc(&op_pub, 0x1B669, 1, &root, &root_priv, &[0x05]).unwrap();

        // 10-year validity window starting 2021-01-01: valid throughout, i.e.
        // both endpoints and a point strictly inside are valid.
        assert!(cert_time_valid(&noc, MATTER_EPOCH_2021));
        assert!(cert_time_valid(&noc, noc.not_after));
        assert!(cert_time_valid(
            &noc,
            MATTER_EPOCH_2021 + NOC_VALIDITY_SECS / 2 // roughly 2026
        ));
        // Before not-before, and past not-after (roughly 2032), are invalid.
        assert!(!cert_time_valid(&noc, MATTER_EPOCH_2021 - 1));
        assert!(!cert_time_valid(&noc, noc.not_after + 1));
    }

    #[test]
    fn issue_noc_produces_chain_valid_cert() {
        let root = MatterCert::parse(ROOT_CHIP).unwrap();
        let root_priv: [u8; 32] = include_bytes!("../tests/fixtures/root01_privkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        // 我々の operational 鍵ペア（テストではフィクスチャの node 鍵を流用）
        let op_pub: [u8; 65] = include_bytes!("../tests/fixtures/node01_01_pubkey.bin")
            .as_slice()
            .try_into()
            .unwrap();

        let noc = issue_noc(
            &op_pub,
            0x1B669,
            1,
            &root,
            &root_priv,
            &[0x01, 0x02, 0x03, 0x04],
        )
        .unwrap();

        // subject に node/fabric id が入っている
        assert_eq!(noc.node_id(), Some(0x1B669));
        assert_eq!(noc.fabric_id(), Some(1));
        // pub key は渡したもの
        assert_eq!(noc.pub_key, op_pub);
        // root で署名検証が通る（DER TBS への署名が正しい）
        noc.verify_signed_by(&root.pub_key).unwrap();
        // ICAC 無しのチェーン検証が通る（root 直署名）
        verify_noc_chain(&noc, None, &root).unwrap();
        // NOC 拡張: BasicConstraints(false) / KeyUsage(digitalSignature) / EKU(client,server) / SKID / AKID
        assert!(noc.extensions.iter().any(|e| matches!(
            e,
            CertExtension::BasicConstraints {
                is_ca: false,
                path_len: None
            }
        )));
        assert!(noc
            .extensions
            .iter()
            .any(|e| matches!(e, CertExtension::KeyUsage(0x0001))));
        // SKID = SHA1(op_pub), AKID = root の SKID
        let root_skid = root.extensions.iter().find_map(|e| match e {
            CertExtension::SubjectKeyId(id) => Some(id.clone()),
            _ => None,
        });
        assert!(noc.extensions.iter().any(|e| matches!(
            e, CertExtension::SubjectKeyId(id) if id.as_slice() == subject_key_id(&op_pub)
        )));
        assert!(noc.extensions.iter().any(|e| matches!(
            e, CertExtension::AuthorityKeyId(id) if root_skid.as_deref() == Some(id.as_slice())
        )));
        // TLV に書き出して再パースしても等価
        let reparsed = MatterCert::parse(&noc.to_tlv()).unwrap();
        assert_eq!(reparsed.node_id(), Some(0x1B669));
        reparsed.verify_signed_by(&root.pub_key).unwrap();
    }
}
