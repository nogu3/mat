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
