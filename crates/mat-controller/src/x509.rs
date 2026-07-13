//! 最小 DER リーダと X.509 証明書 / PKCS#10 CSR 解析（attestation 検証の下地）。
//!
//! Matter の DAC/PAI/PAA 証明書チェーン（Device Attestation）は Matter TLV
//! 独自形式の operational 証明書（cert.rs）と違い、標準 X.509 DER で配布される。
//! ここでは attestation 検証（Task 5）と CSR パース（Task 10）に必要な最小限だけを
//! 読む — 汎用 ASN.1/X.509 ライブラリではない。
//!
//! 入力はすべてデバイス由来（不正な形式があり得る）。長さ形式は short /
//! 0x81 / 0x82 のみ受理し、それ以外・範囲外・truncated はすべて `Err` を返す
//! （panic しない — asn1.rs の方針を踏襲）。

use crate::asn1;

// --- OID 定数（内容バイトのみ。タグ 0x06 は asn1::oid が付与する） ---

const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]; // 1.2.840.10045.2.1
const OID_PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]; // 1.2.840.10045.3.1.7
const OID_ECDSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]; // 1.2.840.10045.4.3.2
const OID_SKID: &[u8] = &[0x55, 0x1D, 0x0E]; // 2.5.29.14
const OID_AKID: &[u8] = &[0x55, 0x1D, 0x23]; // 2.5.29.35
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1D, 0x13]; // 2.5.29.19
const OID_CN: &[u8] = &[0x55, 0x04, 0x03]; // 2.5.4.3
const OID_MATTER_VID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x01]; // 1.3.6.1.4.1.37244.2.1
const OID_MATTER_PID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x02]; // 1.3.6.1.4.1.37244.2.2

/// X.509 / CSR 解析エラー。不正入力は必ず `Err`（panic しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X509Error {
    /// DER 構造そのものが壊れている（タグ不一致・長さ超過・truncated 等）。
    Der(&'static str),
    /// 署名アルゴリズム／公開鍵アルゴリズムが ECDSA-with-SHA256 / P-256 以外。
    UnsupportedAlg,
    /// 署名検証に失敗した。
    BadSignature,
    /// 公開鍵のエンコーディングが不正（65B 非圧縮点でない等）。
    BadPublicKey,
}

impl std::fmt::Display for X509Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            X509Error::Der(msg) => write!(f, "der parse error: {msg}"),
            X509Error::UnsupportedAlg => write!(f, "unsupported x509 algorithm"),
            X509Error::BadSignature => write!(f, "x509 signature verification failed"),
            X509Error::BadPublicKey => write!(f, "invalid x509 public key encoding"),
        }
    }
}

impl std::error::Error for X509Error {}

/// 解析済み X.509 証明書。`issuer`/`subject` は Name の DER 生バイト
/// （タグ+長さ込み）— チェーン構築時のバイト単位一致比較にそのまま使える。
#[derive(Debug, Clone)]
pub struct X509Cert {
    /// TBSCertificate の生 DER（tag+length 込み）。署名対象そのもの。
    pub tbs: Vec<u8>,
    /// 非圧縮 SEC1 P-256 公開鍵（0x04 || X || Y、65B）。
    pub public_key: [u8; 65],
    /// issuer Name の生 DER。
    pub issuer: Vec<u8>,
    /// subject Name の生 DER。
    pub subject: Vec<u8>,
    /// raw r||s（64B）に正規化した ECDSA 署名。
    pub signature: [u8; 64],
    /// SubjectKeyIdentifier 拡張（あれば）。
    pub skid: Option<Vec<u8>>,
    /// AuthorityKeyIdentifier 拡張の keyIdentifier（あれば）。
    pub akid: Option<Vec<u8>>,
    /// Matter VID（Matter VID OID の RDN、無ければ CN 中の `Mvid:XXXX` から）。
    pub vid: Option<u16>,
    /// Matter PID（Matter PID OID の RDN、無ければ CN 中の `Mpid:XXXX` から）。
    pub pid: Option<u16>,
}

impl X509Cert {
    /// この証明書の署名が `issuer` の公開鍵で検証できるかだけを見る。
    /// issuer/subject 名の突き合わせ（チェーン構築）はここでは行わない —
    /// それは呼び出し側（attestation.rs、Task 5）の責務。
    pub fn verify_signed_by(&self, issuer: &X509Cert) -> Result<(), X509Error> {
        crate::crypto::verify_ecdsa_p256(&issuer.public_key, &self.tbs, &self.signature)
            .map_err(|_| X509Error::BadSignature)
    }
}

/// 最小 DER TLV リーダ。破損入力に対して panic せず `Err` を返す。
/// 受理する長さ形式: short（<0x80）/ 0x81（1B 長さ）/ 0x82（2B BE 長さ）のみ。
pub(crate) struct DerReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> DerReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub(crate) fn peek_tag(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    /// 次の TLV を読み `(tag, content, raw_tlv全体)` を返す。
    pub(crate) fn read(&mut self) -> Result<(u8, &'a [u8], &'a [u8]), X509Error> {
        let start = self.pos;
        let tag = *self
            .buf
            .get(self.pos)
            .ok_or(X509Error::Der("truncated tag"))?;
        self.pos += 1;
        let len_byte = *self
            .buf
            .get(self.pos)
            .ok_or(X509Error::Der("truncated length"))?;
        self.pos += 1;
        let len = if len_byte < 0x80 {
            len_byte as usize
        } else if len_byte == 0x81 {
            let b = *self
                .buf
                .get(self.pos)
                .ok_or(X509Error::Der("truncated length"))?;
            self.pos += 1;
            b as usize
        } else if len_byte == 0x82 {
            let hi = *self
                .buf
                .get(self.pos)
                .ok_or(X509Error::Der("truncated length"))?;
            let lo = *self
                .buf
                .get(self.pos + 1)
                .ok_or(X509Error::Der("truncated length"))?;
            self.pos += 2;
            (usize::from(hi) << 8) | usize::from(lo)
        } else {
            return Err(X509Error::Der("unsupported der length form"));
        };
        let content_start = self.pos;
        let content_end = content_start
            .checked_add(len)
            .ok_or(X509Error::Der("length overflow"))?;
        if content_end > self.buf.len() {
            return Err(X509Error::Der("truncated content"));
        }
        let content = &self.buf[content_start..content_end];
        let raw = &self.buf[start..content_end];
        self.pos = content_end;
        Ok((tag, content, raw))
    }

    pub(crate) fn expect(&mut self, tag: u8) -> Result<&'a [u8], X509Error> {
        let (t, content, _) = self.read()?;
        if t != tag {
            return Err(X509Error::Der("unexpected der tag"));
        }
        Ok(content)
    }
}

/// `Certificate ::= SEQ { tbsCertificate SEQ, signatureAlgorithm SEQ, signature BIT STRING }`
/// を読み、TBS 生バイトと SPKI 公開鍵、issuer/subject 生バイト、
/// SKID/AKID/VID/PID 拡張を抜き出す。署名検証はしない（`verify_signed_by` で行う）。
pub fn parse_x509(der: &[u8]) -> Result<X509Cert, X509Error> {
    let mut top = DerReader::new(der);
    let cert_content = top.expect(0x30)?;
    let mut cert = DerReader::new(cert_content);

    let (tbs_tag, tbs_content, tbs_raw) = cert.read()?;
    if tbs_tag != 0x30 {
        return Err(X509Error::Der("tbsCertificate not a sequence"));
    }
    let tbs = tbs_raw.to_vec();

    let sig_alg_content = cert.expect(0x30)?;
    check_ecdsa_sha256_alg(sig_alg_content)?;

    let sig_bits = cert.expect(0x03)?;
    let signature = parse_ecdsa_signature(sig_bits)?;

    // --- TBSCertificate の中身 ---
    let mut t = DerReader::new(tbs_content);
    if t.peek_tag() == Some(0xA0) {
        t.expect(0xA0)?; // [0] version — optional、値は使わない
    }
    t.expect(0x02)?; // serial — 使わない
    t.expect(0x30)?; // TBS 内の signature(AlgorithmIdentifier) — 使わない

    let (issuer_tag, _issuer_content, issuer_raw) = t.read()?;
    if issuer_tag != 0x30 {
        return Err(X509Error::Der("issuer not a name"));
    }
    let issuer = issuer_raw.to_vec();

    t.expect(0x30)?; // validity — 使わない

    let (subject_tag, subject_content, subject_raw) = t.read()?;
    if subject_tag != 0x30 {
        return Err(X509Error::Der("subject not a name"));
    }
    let subject = subject_raw.to_vec();

    let spki_content = t.expect(0x30)?;
    let public_key = parse_spki(spki_content)?;

    let mut skid = None;
    let mut akid = None;
    if t.peek_tag() == Some(0xA3) {
        let ext_wrap = t.expect(0xA3)?;
        let mut ew = DerReader::new(ext_wrap);
        let exts_content = ew.expect(0x30)?;
        let mut list = DerReader::new(exts_content);
        while !list.is_empty() {
            let ext_seq = list.expect(0x30)?;
            let mut er = DerReader::new(ext_seq);
            let oid_bytes = er.expect(0x06)?;
            if er.peek_tag() == Some(0x01) {
                er.expect(0x01)?; // critical BOOLEAN — 使わない
            }
            let value = er.expect(0x04)?;
            if oid_bytes == OID_SKID {
                let mut vr = DerReader::new(value);
                skid = Some(vr.expect(0x04)?.to_vec());
            } else if oid_bytes == OID_AKID {
                let mut vr = DerReader::new(value);
                let seq_content = vr.expect(0x30)?;
                let mut sr = DerReader::new(seq_content);
                if sr.peek_tag() == Some(0x80) {
                    akid = Some(sr.expect(0x80)?.to_vec());
                }
            }
            // 他の拡張（BasicConstraints 等）は読み捨てる。
        }
    }

    let (vid, pid) = parse_vid_pid(subject_content)?;

    Ok(X509Cert {
        tbs,
        public_key,
        issuer,
        subject,
        signature,
        skid,
        akid,
        vid,
        pid,
    })
}

/// `CertificationRequest ::= SEQ { certificationRequestInfo SEQ, sigAlg SEQ, signature BIT STRING }`
/// （PKCS#10）を読み、自己署名を検証したうえで P-256 公開鍵を返す。
pub fn parse_csr(der: &[u8]) -> Result<[u8; 65], X509Error> {
    let mut top = DerReader::new(der);
    let content = top.expect(0x30)?;
    let mut r = DerReader::new(content);

    let (cri_tag, cri_content, cri_raw) = r.read()?;
    if cri_tag != 0x30 {
        return Err(X509Error::Der("certificationRequestInfo not a sequence"));
    }

    let sig_alg_content = r.expect(0x30)?;
    check_ecdsa_sha256_alg(sig_alg_content)?;

    let sig_bits = r.expect(0x03)?;
    let signature = parse_ecdsa_signature(sig_bits)?;

    let mut cr = DerReader::new(cri_content);
    cr.expect(0x02)?; // version — 使わない
    cr.expect(0x30)?; // subject Name — 使わない
    let spki_content = cr.expect(0x30)?;
    let public_key = parse_spki(spki_content)?;
    // attributes [0] は無視してよい（CRI 生バイト全体を署名対象として使う）。

    crate::crypto::verify_ecdsa_p256(&public_key, cri_raw, &signature)
        .map_err(|_| X509Error::BadSignature)?;

    Ok(public_key)
}

/// `AlgorithmIdentifier ::= SEQ { OID, ... }` が ecdsa-with-SHA256 か確認する。
fn check_ecdsa_sha256_alg(content: &[u8]) -> Result<(), X509Error> {
    let mut r = DerReader::new(content);
    let oid_bytes = r.expect(0x06)?;
    if oid_bytes != OID_ECDSA_SHA256 {
        return Err(X509Error::UnsupportedAlg);
    }
    Ok(())
}

/// `SubjectPublicKeyInfo ::= SEQ { SEQ { OID ecPublicKey, OID prime256v1 }, BIT STRING key }`
/// から 65B 非圧縮 P-256 公開鍵を取り出す。
fn parse_spki(content: &[u8]) -> Result<[u8; 65], X509Error> {
    let mut r = DerReader::new(content);
    let alg_seq = r.expect(0x30)?;
    let mut a = DerReader::new(alg_seq);
    let alg_oid = a.expect(0x06)?;
    if alg_oid != OID_EC_PUBLIC_KEY {
        return Err(X509Error::UnsupportedAlg);
    }
    let curve_oid = a.expect(0x06)?;
    if curve_oid != OID_PRIME256V1 {
        return Err(X509Error::UnsupportedAlg);
    }
    let bits = r.expect(0x03)?;
    let (unused, key_bytes) = bits.split_first().ok_or(X509Error::BadPublicKey)?;
    if *unused != 0 {
        return Err(X509Error::BadPublicKey);
    }
    key_bytes.try_into().map_err(|_| X509Error::BadPublicKey)
}

/// 署名 BIT STRING（unused-bits byte + DER `SEQ { r INT, s INT }`）を
/// raw r||s（32B 左ゼロ詰め x2 = 64B）に正規化する。
fn parse_ecdsa_signature(bits: &[u8]) -> Result<[u8; 64], X509Error> {
    let (unused, seq_bytes) = bits
        .split_first()
        .ok_or(X509Error::Der("empty signature bit string"))?;
    if *unused != 0 {
        return Err(X509Error::Der("unexpected unused bits in signature"));
    }
    let mut r = DerReader::new(seq_bytes);
    let seq_content = r.expect(0x30)?;
    let mut inner = DerReader::new(seq_content);
    let r_bytes = inner.expect(0x02)?;
    let s_bytes = inner.expect(0x02)?;
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&int_to_32(r_bytes)?);
    out[32..].copy_from_slice(&int_to_32(s_bytes)?);
    Ok(out)
}

/// DER INTEGER の中身（符号バイト付き・可変長）を 32B 左ゼロ詰め固定長にする。
fn int_to_32(b: &[u8]) -> Result<[u8; 32], X509Error> {
    let b = if b.len() > 1 && b[0] == 0 { &b[1..] } else { b };
    if b.is_empty() || b.len() > 32 {
        return Err(X509Error::Der("integer out of range"));
    }
    let mut out = [0u8; 32];
    out[32 - b.len()..].copy_from_slice(b);
    Ok(out)
}

/// subject Name（`SEQ of SET of SEQ { OID, value }`の中身）から Matter VID/PID
/// を抜き出す。専用 OID の RDN が無ければ CN 文字列中の `Mvid:XXXX` /
/// `Mpid:XXXX` を探す。
fn parse_vid_pid(name_content: &[u8]) -> Result<(Option<u16>, Option<u16>), X509Error> {
    let mut vid = None;
    let mut pid = None;
    let mut cn: Option<String> = None;

    let mut r = DerReader::new(name_content);
    while !r.is_empty() {
        let set_content = r.expect(0x31)?;
        let mut sr = DerReader::new(set_content);
        while !sr.is_empty() {
            let atv = sr.expect(0x30)?;
            let mut ar = DerReader::new(atv);
            let oid_bytes = ar.expect(0x06)?;
            let (val_tag, val_content, _) = ar.read()?;
            if val_tag != 0x0C && val_tag != 0x13 {
                continue; // UTF8String/PrintableString 以外の DN 値型は無視
            }
            let Ok(s) = std::str::from_utf8(val_content) else {
                continue;
            };
            if oid_bytes == OID_MATTER_VID {
                vid = u16::from_str_radix(s, 16).ok();
            } else if oid_bytes == OID_MATTER_PID {
                pid = u16::from_str_radix(s, 16).ok();
            } else if oid_bytes == OID_CN {
                cn = Some(s.to_string());
            }
        }
    }

    if let Some(cn) = &cn {
        if vid.is_none() {
            vid = extract_hex_tag(cn, "Mvid:");
        }
        if pid.is_none() {
            pid = extract_hex_tag(cn, "Mpid:");
        }
    }

    Ok((vid, pid))
}

/// `s` 中の `prefix` 直後 4 桁の 16 進数を u16 として取り出す（無ければ None）。
fn extract_hex_tag(s: &str, prefix: &str) -> Option<u16> {
    let idx = s.find(prefix)?;
    let start = idx + prefix.len();
    let hex = s.get(start..start + 4)?;
    u16::from_str_radix(hex, 16).ok()
}

/// Task 5（attestation.rs）のテストが DAC→PAI→PAA フィクスチャチェーンを
/// 組み立てるのに再利用するテスト用証明書合成ヘルパ。`#[cfg(test)]` ではなく
/// 常時コンパイルされる `pub(crate)`（クレート内の他モジュールのテストから
/// 呼べるようにするため）。本番の署名パス（parse_x509 / verify_signed_by /
/// parse_csr）はここに一切依存しない。
#[allow(dead_code)]
pub(crate) mod test_support {
    use super::{
        asn1, OID_AKID, OID_BASIC_CONSTRAINTS, OID_CN, OID_ECDSA_SHA256, OID_EC_PUBLIC_KEY,
        OID_MATTER_PID, OID_MATTER_VID, OID_PRIME256V1, OID_SKID,
    };

    /// 最小の自己/他者署名 X.509 証明書を合成する（DER バイト列）。
    /// `subject`/`issuer` は CN 文字列のバイト列。CA 証明書には
    /// BasicConstraints(CA=true) を、全証明書に SKID（subject 鍵の）と
    /// AKID（signer 鍵の SKID と同値）を付ける — チェーン照合を
    /// issuer/subject バイト一致・akid/skid 一致の両方でテストできるように。
    pub(crate) fn make_test_cert(
        subject: &[u8],
        issuer: &[u8],
        subject_key: &p256::SecretKey,
        signer_key: &p256::SecretKey,
        is_ca: bool,
        vid_pid: Option<(u16, u16)>,
    ) -> Vec<u8> {
        use p256::elliptic_curve::sec1::ToEncodedPoint;

        let subject_pub: [u8; 65] = subject_key
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .expect("uncompressed p256 point is 65 bytes");
        let signer_pub: [u8; 65] = signer_key
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .expect("uncompressed p256 point is 65 bytes");
        let subject_skid = crate::cert::subject_key_id(&subject_pub);
        let signer_skid = crate::cert::subject_key_id(&signer_pub);

        let version = asn1::context_constructed(0, &asn1::integer(&[2])); // v3
        let serial = asn1::integer(&[0x01]);
        let sig_alg = asn1::seq(&[&asn1::oid(OID_ECDSA_SHA256)]);
        // issuer Name は「発行元 CA の実際の subject バイト列」と一致していな
        // ければならない（X.509 のチェーン照合はバイト一致が前提 —
        // attestation.rs Task 5 の DAC.issuer == PAI.subject 判定）。この
        // フィクスチャの 3 階層（PAA: vid_pid なし → PAI/DAC: 同じ vid_pid）
        // では、leaf 証明書（is_ca=false、= DAC 相当）の発行元だけが
        // vid_pid 入りの subject を持つので、その場合だけ vid_pid を issuer
        // Name にも埋め込む。CA 証明書（PAA/PAI 自身、is_ca=true）の発行元は
        // 常に vid_pid なし（ルート PAA はベンダー非依存）とみなす。
        let issuer_name = build_name(issuer, if is_ca { None } else { vid_pid });
        let validity = asn1::seq(&[
            &asn1::utc_time("260101000000Z"),
            &asn1::utc_time("300101000000Z"),
        ]);
        let subject_name = build_name(subject, vid_pid);
        let spki = asn1::seq(&[
            &asn1::seq(&[&asn1::oid(OID_EC_PUBLIC_KEY), &asn1::oid(OID_PRIME256V1)]),
            &asn1::bit_string(0, &subject_pub),
        ]);

        let mut ext_items: Vec<Vec<u8>> = Vec::new();
        if is_ca {
            ext_items.push(basic_constraints_ext());
        }
        ext_items.push(skid_ext(&subject_skid));
        ext_items.push(akid_ext(&signer_skid));
        let ext_refs: Vec<&[u8]> = ext_items.iter().map(Vec::as_slice).collect();
        let extensions = asn1::context_constructed(3, &asn1::seq(&ext_refs));

        let tbs = asn1::seq(&[
            &version,
            &serial,
            &sig_alg,
            &issuer_name,
            &validity,
            &subject_name,
            &spki,
            &extensions,
        ]);

        let signer_priv: [u8; 32] = signer_key.to_bytes().into();
        let raw_sig = crate::crypto::sign_ecdsa_p256(&signer_priv, &tbs).expect("sign tbs");
        let sig_bits = asn1::bit_string(0, &raw_sig_to_der(&raw_sig));

        asn1::seq(&[&tbs, &sig_alg, &sig_bits])
    }

    /// 最小の自己署名 PKCS#10 CSR を合成する（DER バイト列）。
    pub(crate) fn make_test_csr(key: &p256::SecretKey) -> Vec<u8> {
        use p256::elliptic_curve::sec1::ToEncodedPoint;

        let pub_bytes: [u8; 65] = key
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .expect("uncompressed p256 point is 65 bytes");

        let version = asn1::integer(&[0x00]);
        let subject_name = build_name(b"csr", None);
        let spki = asn1::seq(&[
            &asn1::seq(&[&asn1::oid(OID_EC_PUBLIC_KEY), &asn1::oid(OID_PRIME256V1)]),
            &asn1::bit_string(0, &pub_bytes),
        ]);
        let attributes = asn1::context_constructed(0, &[]); // [0] IMPLICIT SET OF Attribute（空）
        let cri = asn1::seq(&[&version, &subject_name, &spki, &attributes]);

        let sig_alg = asn1::seq(&[&asn1::oid(OID_ECDSA_SHA256)]);
        let priv_bytes: [u8; 32] = key.to_bytes().into();
        let raw_sig = crate::crypto::sign_ecdsa_p256(&priv_bytes, &cri).expect("sign cri");
        let sig_bits = asn1::bit_string(0, &raw_sig_to_der(&raw_sig));

        asn1::seq(&[&cri, &sig_alg, &sig_bits])
    }

    /// `SEQ of SET of SEQ { OID CN, UTF8String }`（+ vid_pid 指定時は Matter
    /// VID/PID OID の RDN を追加）。
    fn build_name(cn_bytes: &[u8], vid_pid: Option<(u16, u16)>) -> Vec<u8> {
        let cn_str = std::str::from_utf8(cn_bytes).expect("test cn is utf8");
        let mut rdns: Vec<Vec<u8>> = vec![asn1::set_of(&[&asn1::seq(&[
            &asn1::oid(OID_CN),
            &asn1::utf8_string(cn_str),
        ])])];
        if let Some((vid, pid)) = vid_pid {
            rdns.push(asn1::set_of(&[&asn1::seq(&[
                &asn1::oid(OID_MATTER_VID),
                &asn1::utf8_string(&format!("{vid:04X}")),
            ])]));
            rdns.push(asn1::set_of(&[&asn1::seq(&[
                &asn1::oid(OID_MATTER_PID),
                &asn1::utf8_string(&format!("{pid:04X}")),
            ])]));
        }
        let refs: Vec<&[u8]> = rdns.iter().map(Vec::as_slice).collect();
        asn1::seq(&refs)
    }

    fn basic_constraints_ext() -> Vec<u8> {
        let value = asn1::seq(&[&asn1::boolean(true)]);
        asn1::seq(&[
            &asn1::oid(OID_BASIC_CONSTRAINTS),
            &asn1::boolean(true),
            &asn1::octet_string(&value),
        ])
    }

    fn skid_ext(id: &[u8]) -> Vec<u8> {
        let inner = asn1::octet_string(id);
        asn1::seq(&[&asn1::oid(OID_SKID), &asn1::octet_string(&inner)])
    }

    fn akid_ext(id: &[u8]) -> Vec<u8> {
        let key_id = asn1::context_primitive(0, id);
        let value_seq = asn1::seq(&[&key_id]);
        asn1::seq(&[&asn1::oid(OID_AKID), &asn1::octet_string(&value_seq)])
    }

    /// raw r||s（64B）を DER `SEQ { INTEGER r, INTEGER s }` に変換する
    /// （make_test_cert/make_test_csr が署名を埋め込む際に使う）。
    fn raw_sig_to_der(sig: &[u8; 64]) -> Vec<u8> {
        asn1::seq(&[&der_uint(&sig[..32]), &der_uint(&sig[32..])])
    }

    /// 符号無し big-endian バイト列 -> 最小長 DER INTEGER 中身
    /// （先頭ゼロを削り、最上位ビットが立っていれば 0x00 を再度付与）。
    fn der_uint(bytes: &[u8]) -> Vec<u8> {
        let mut b = bytes;
        while b.len() > 1 && b[0] == 0 {
            b = &b[1..];
        }
        if b[0] & 0x80 != 0 {
            let mut v = Vec::with_capacity(b.len() + 1);
            v.push(0);
            v.extend_from_slice(b);
            asn1::integer(&v)
        } else {
            asn1::integer(b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{make_test_cert, make_test_csr};
    use super::*;
    use crate::case::random_p256_secret;

    #[test]
    fn parses_and_verifies_self_signed() {
        let key = random_p256_secret();
        let der = make_test_cert(b"root", b"root", &key, &key, true, None);
        let cert = parse_x509(&der).unwrap();
        assert_eq!(cert.issuer, cert.subject);
        cert.verify_signed_by(&cert).unwrap();
    }

    #[test]
    fn verifies_two_level_chain_and_rejects_wrong_issuer() {
        let root = random_p256_secret();
        let leaf = random_p256_secret();
        let other = random_p256_secret();
        let root_der = make_test_cert(b"root", b"root", &root, &root, true, None);
        let leaf_der = make_test_cert(
            b"leaf",
            b"root",
            &leaf,
            &root,
            false,
            Some((0xFFF1, 0x8001)),
        );
        let root_c = parse_x509(&root_der).unwrap();
        let leaf_c = parse_x509(&leaf_der).unwrap();
        leaf_c.verify_signed_by(&root_c).unwrap();
        assert_eq!(leaf_c.vid, Some(0xFFF1));
        assert_eq!(leaf_c.pid, Some(0x8001));
        let other_der = make_test_cert(b"other", b"other", &other, &other, true, None);
        let other_c = parse_x509(&other_der).unwrap();
        assert!(leaf_c.verify_signed_by(&other_c).is_err());
    }

    #[test]
    fn parses_csr_and_rejects_tampered() {
        let key = random_p256_secret();
        let csr = make_test_csr(&key);
        let pk = parse_csr(&csr).unwrap();
        assert_eq!(pk.len(), 65);
        let mut bad = csr.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        assert!(parse_csr(&bad).is_err());
    }

    #[test]
    fn rejects_truncated_der() {
        assert!(parse_x509(&[0x30, 0x05, 0x01]).is_err());
    }
}
