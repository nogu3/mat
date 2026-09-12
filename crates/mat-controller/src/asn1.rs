//! Minimal DER writer — just enough to rebuild the TBSCertificate of a
//! Matter operational certificate for signature verification (cert.rs).
//! Not a general ASN.1 library; no parsing.

/// Encode a TLV with DER length encoding (short form <128, or 0x81/0x82 long form).
pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 4);
    out.push(tag);
    let len = content.len();
    if len < 128 {
        out.push(len as u8);
    } else if len < 256 {
        out.push(0x81);
        out.push(len as u8);
    } else {
        // 証明書 TBS は 64KiB を超えない
        assert!(len <= usize::from(u16::MAX), "der content too large");
        out.push(0x82);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    }
    out.extend_from_slice(content);
    out
}

/// Encode a SEQUENCE (tag 0x30).
pub fn seq(children: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &children.concat())
}

/// Encode a SET OF (tag 0x31).
pub fn set_of(children: &[&[u8]]) -> Vec<u8> {
    tlv(0x31, &children.concat())
}

/// Encode an INTEGER (tag 0x02).
pub fn integer(content: &[u8]) -> Vec<u8> {
    tlv(0x02, content)
}

/// Encode an OBJECT IDENTIFIER (tag 0x06). `content` is the OID's encoded
/// arcs (no tag/length), e.g. `[0x2A, 0x86, 0x48, ...]`.
pub fn oid(content: &[u8]) -> Vec<u8> {
    tlv(0x06, content)
}

/// Encode a BOOLEAN (tag 0x01), value 0xFF for true, 0x00 for false.
pub fn boolean(v: bool) -> Vec<u8> {
    tlv(0x01, &[if v { 0xFF } else { 0x00 }])
}

/// Encode a BIT STRING (tag 0x03), with unused_bits count prefix.
pub fn bit_string(unused_bits: u8, bytes: &[u8]) -> Vec<u8> {
    let mut content = vec![unused_bits];
    content.extend_from_slice(bytes);
    tlv(0x03, &content)
}

/// Encode an OCTET STRING (tag 0x04).
pub fn octet_string(bytes: &[u8]) -> Vec<u8> {
    tlv(0x04, bytes)
}

/// Encode a UTF8String (tag 0x0C).
pub fn utf8_string(s: &str) -> Vec<u8> {
    tlv(0x0C, s.as_bytes())
}

/// Encode a PrintableString (tag 0x13).
pub fn printable_string(s: &str) -> Vec<u8> {
    tlv(0x13, s.as_bytes())
}

/// Encode a UTCTime (tag 0x17).
pub fn utc_time(s: &str) -> Vec<u8> {
    tlv(0x17, s.as_bytes())
}

/// Encode a GeneralizedTime (tag 0x18).
pub fn generalized_time(s: &str) -> Vec<u8> {
    tlv(0x18, s.as_bytes())
}

/// Encode a context-constructed tag (0xA0 | n).
pub fn context_constructed(n: u8, content: &[u8]) -> Vec<u8> {
    tlv(0xA0 | n, content)
}

/// Encode a context-primitive tag (0x80 | n).
pub fn context_primitive(n: u8, content: &[u8]) -> Vec<u8> {
    tlv(0x80 | n, content)
}

/// 符号無し big-endian バイト列を最小長 DER INTEGER にする（先頭ゼロを削り、
/// 最上位ビットが立っていれば 0x00 を付け直す）。
pub fn uint_integer(bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes;
    while b.len() > 1 && b[0] == 0 {
        b = &b[1..];
    }
    if b.first().is_some_and(|f| f & 0x80 != 0) {
        let mut v = Vec::with_capacity(b.len() + 1);
        v.push(0);
        v.extend_from_slice(b);
        integer(&v)
    } else {
        integer(b)
    }
}

/// raw `r ‖ s`（64B）の ECDSA-P256 署名を DER `SEQUENCE { INTEGER r,
/// INTEGER s }` にする（X.509 証明書・CMS SignerInfo の署名フィールド用）。
pub fn ecdsa_signature(sig: &[u8; 64]) -> Vec<u8> {
    seq(&[&uint_integer(&sig[..32]), &uint_integer(&sig[32..])])
}

/// このクレートが DER に書く／読む OID の内容バイト（タグ `0x06` と長さは
/// 含まない — [`oid`] が付与する）。cert.rs（Matter TLV → DER TBS 再構築）、
/// x509.rs（DAC/PAI/PAA・CSR 解析と test fixture 合成）、cd.rs / attestation.rs
/// （CMS SignedData）が共有する唯一の表。
pub mod oids {
    /// 1.2.840.10045.2.1 id-ecPublicKey
    pub const EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
    /// 1.2.840.10045.3.1.7 prime256v1
    pub const PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
    /// 1.2.840.10045.4.3.2 ecdsa-with-SHA256
    pub const ECDSA_WITH_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
    /// 2.16.840.1.101.3.4.2.1 sha256
    pub const SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
    /// 2.5.4.3 commonName
    pub const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
    // Matter arc 1.3.6.1.4.1.37244.1.x -> 2B 06 01 04 01 82 A2 7C 01 xx
    /// 1.3.6.1.4.1.37244.1.1 matter-node-id
    pub const MATTER_NODE_ID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x01];
    /// 1.3.6.1.4.1.37244.1.2 matter-firmware-signing-id
    pub const MATTER_FIRMWARE_SIGNING_ID: &[u8] =
        &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x02];
    /// 1.3.6.1.4.1.37244.1.3 matter-icac-id
    pub const MATTER_ICAC_ID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x03];
    /// 1.3.6.1.4.1.37244.1.4 matter-rcac-id
    pub const MATTER_RCAC_ID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x04];
    /// 1.3.6.1.4.1.37244.1.5 matter-fabric-id
    pub const MATTER_FABRIC_ID: &[u8] =
        &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x05];
    /// 1.3.6.1.4.1.37244.1.6 matter-noc-cat
    pub const MATTER_NOC_CAT: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x06];
    /// 1.3.6.1.4.1.37244.2.1 matter-vid（DAC/PAI subject）
    pub const MATTER_VID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x01];
    /// 1.3.6.1.4.1.37244.2.2 matter-pid（DAC/PAI subject）
    pub const MATTER_PID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x02];
    /// 2.5.29.19 basicConstraints
    pub const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1D, 0x13];
    /// 2.5.29.15 keyUsage
    pub const KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x0F];
    /// 2.5.29.37 extKeyUsage
    pub const EXTENDED_KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x25];
    /// 2.5.29.14 subjectKeyIdentifier
    pub const SUBJECT_KEY_ID: &[u8] = &[0x55, 0x1D, 0x0E];
    /// 2.5.29.35 authorityKeyIdentifier
    pub const AUTHORITY_KEY_ID: &[u8] = &[0x55, 0x1D, 0x23];
    /// 1.3.6.1.5.5.7.3 id-kp（末尾 1 バイトの purpose を足して EKU OID になる）
    pub const ID_KP_PREFIX: &[u8] = &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03];
    /// 1.2.840.113549.1.7.1 pkcs7-data
    pub const PKCS7_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x01];
    /// 1.2.840.113549.1.7.2 pkcs7-signedData
    pub const PKCS7_SIGNED_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
    /// 1.2.840.113549.1.9.4 messageDigest（CMS signedAttrs）
    pub const CMS_MESSAGE_DIGEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_short_and_long_lengths() {
        assert_eq!(tlv(0x04, &[0xAB]), vec![0x04, 0x01, 0xAB]);
        assert_eq!(tlv(0x04, &[]), vec![0x04, 0x00]);
        let long = vec![0x00; 200]; // 128..256 → 0x81 プレフィクス
        let enc = tlv(0x04, &long);
        assert_eq!(&enc[..3], &[0x04, 0x81, 200]);
        assert_eq!(enc.len(), 3 + 200);
        let longer = vec![0x00; 300]; // 256.. → 0x82 + u16 BE
        let enc = tlv(0x04, &longer);
        assert_eq!(&enc[..4], &[0x04, 0x82, 0x01, 0x2C]);
    }

    #[test]
    fn encodes_primitives() {
        assert_eq!(integer(&[0x02]), vec![0x02, 0x01, 0x02]);
        assert_eq!(boolean(true), vec![0x01, 0x01, 0xFF]);
        assert_eq!(boolean(false), vec![0x01, 0x01, 0x00]);
        assert_eq!(bit_string(7, &[0x80]), vec![0x03, 0x02, 0x07, 0x80]);
        assert_eq!(octet_string(&[1, 2]), vec![0x04, 0x02, 0x01, 0x02]);
        assert_eq!(utf8_string("AB"), vec![0x0C, 0x02, 0x41, 0x42]);
        assert_eq!(printable_string("A"), vec![0x13, 0x01, 0x41]);
        assert_eq!(oid(&[0x55, 0x1D, 0x0E]), vec![0x06, 0x03, 0x55, 0x1D, 0x0E]);
        assert_eq!(
            utc_time("260101000000Z"),
            [vec![0x17, 0x0D], b"260101000000Z".to_vec()].concat()
        );
    }

    #[test]
    fn encodes_containers() {
        assert_eq!(
            seq(&[&integer(&[0x01]), &boolean(true)]),
            vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x01, 0x01, 0xFF]
        );
        assert_eq!(
            set_of(&[&integer(&[0x01])]),
            vec![0x31, 0x03, 0x02, 0x01, 0x01]
        );
        assert_eq!(
            context_constructed(0, &integer(&[0x02])),
            vec![0xA0, 0x03, 0x02, 0x01, 0x02]
        );
        assert_eq!(context_primitive(0, &[0xAA]), vec![0x80, 0x01, 0xAA]);
    }
}
