//! Certification Declaration（CD、spec §6.3）の組み立て。
//!
//! CD は「この VID/PID の組み合わせは CSA の認証プロセスを通っている」と
//! CSA が主張する署名済みの小さな文書で、デバイスは AttestationResponse の
//! `AttestationElements` に丸ごと同梱する。コミッショナ（chip / Alexa /
//! Google など、いずれも chip SDK 由来）は
//!
//! 1. CMS 署名者の key id を取り出し、
//! 2. 自分が持つ CD 署名鍵の信頼ストアからその公開鍵を引き、
//! 3. CMS 署名を検証し、
//! 4. 中身の `vendor_id` / `product_id_array` を Basic Information クラスタの
//!    VendorID / ProductID と突き合わせる
//!
//! という順で検証する。CD が無い／署名者が引けないと commissioning は
//! attestation ステップで硬く失敗する（chip の
//! `AttestationVerificationResult::kCertificationDeclarationNoKeyId` 等）。
//!
//! ## この実装が使う署名鍵について
//!
//! 本物の CD は CSA が発行する（実デバイスの認証を経て初めて手に入る）。
//! ここで使うのは connectedhomeip リポジトリに**平文で公開されている**
//! 「Matter Test CD Signing Authority」の鍵で、chip SDK の全ビルドが
//! 既定でこの公開鍵を CD 信頼ストアに内蔵している（`gTestCdPubkeyKid` /
//! `gTestCdPubkeyBytes`。`--only-allow-trusted-cd-keys` を明示的に立てた
//! 場合のみ拒否される）。esp-matter などの開発用デバイスや chip の
//! example app が使っているのと同じ「未認証デバイスの標準的な形」であり、
//! **CSA 認証の代わりにはならない** — `x509::generate_dev_attestation` が
//! 作る PAA/PAI/DAC と同じく、開発・テスト用の attestation 素材である。
//!
//! 秘密鍵が公開されている以上これは秘密情報ではなく、リポジトリに定数と
//! して置くことに機密上の問題はない（出所:
//! `credentials/test/certification-declaration/Chip-Test-CD-Signing-Key.pem`）。

use crate::asn1;
use crate::crypto::{sign_ecdsa_p256, CryptoError};
use crate::tlv::{Tag, Writer};

/// 「Matter Test CD Signing Authority」の P-256 **秘密**鍵（raw 32B）。
///
/// **これは秘密情報ではなく、秘密情報として扱ってもいけない。**
/// connectedhomeip リポジトリに平文でコミットされている公開のテスト鍵
/// （`credentials/test/certification-declaration/Chip-Test-CD-Signing-Key.pem`）
/// で、世界中の誰でも同じ鍵で CD を署名できる。したがってこの鍵で署名された
/// CD は「CSA が認証した」ことを一切証明しない — 「chip 系コントローラが
/// 既定で受け入れる、未認証デバイスの標準的な形」でしかない。
///
/// 本番のデバイス identity には**絶対に使わないこと**。実機を CSA 認証に
/// 通すときは、CSA から発行された CD（と実 PAA/PAI/DAC）に差し替える。
///
/// `#[doc(hidden)]`: 公開 API として案内する類のものではなく、
/// [`generate_dev_certification_declaration`] の実装詳細として `pub` に
/// なっているだけ（テストと、将来 CD を自前で組みたい呼び出し側のため）。
#[doc(hidden)]
pub const TEST_CD_SIGNING_KEY: [u8; 32] = [
    0xAE, 0xF3, 0x48, 0x41, 0x16, 0xE9, 0x48, 0x1E, 0xC5, 0x7B, 0xE0, 0x47, 0x2D, 0xF4, 0x1B, 0xF4,
    0x99, 0x06, 0x4E, 0x50, 0x24, 0xAD, 0x86, 0x9E, 0xCA, 0x5E, 0x88, 0x98, 0x02, 0xD4, 0x80, 0x75,
];

/// 上記鍵の subject key identifier。CMS SignerInfo に載せる値であり、
/// コミッショナはこれで公開鍵を引く（chip の `gTestCdPubkeyKid` と同値）。
pub const TEST_CD_SIGNING_KEY_ID: [u8; 20] = [
    0x62, 0xFA, 0x82, 0x33, 0x59, 0xAC, 0xFA, 0xA9, 0x96, 0x3E, 0x1C, 0xFA, 0x14, 0x0A, 0xDD, 0xF5,
    0x04, 0xF3, 0x71, 0x60,
];

/// `certification_type`: 開発・テスト用（spec §6.3.1 CertificationType）。
const CERTIFICATION_TYPE_DEVELOPMENT_AND_TEST: u8 = 0;
/// `format_version`: chip は 1 以外を `kAttestationElementsMalformed` で弾く。
const FORMAT_VERSION: u8 = 1;
/// `certificate_id` は spec 上ちょうど 19 文字。当初は「認証を受けていない
/// 開発用デバイス」と読み取れる自作の固定値（`MATDEV0000000000-00`）を
/// 置いていたが、M2 Echo attestation 実験（Task 10 追加ラウンド）で
/// Alexa ペアリング実績のある `matter.js` の CD generator
/// （`packages/protocol/src/certificate/kinds/CertificationDeclaration.ts`
/// `CertificationDeclaration.generate()`、commit
/// `793e431d932552f63273ed1a0684fdc065ba066d` 時点）と突き合わせたところ
/// **一致しない唯一のフィールドの一つ**だったため、matter.js/chip の全
/// テスト CD が使う正典値 `"CSA00000SWC00000-00"` に差し替えた —
/// コミッショナ（Echo 含む）側がテスト CD をこの identity で認識してい
/// る可能性があるため合わせる。
const CERTIFICATE_ID: &str = "CSA00000SWC00000-00";
/// CD の `device_type_id`（タグ 3）。spec 上は任意の値を置けるが、
/// `matter.js`（上記 `generate()`）は VID/PID に関わらず常に `22` を
/// 固定で書いている — CD のこのフィールドはコミッショナ側の
/// VID/PID 突き合わせには使われず（Basic Information と照合されるのは
/// vendor_id/product_id のみ）、Echo 含む実績のあるコミッショナが見て
/// いる値がこれなので合わせておく。以前はデバイスの実際の device type
/// （呼び出し側の `x509::generate_dev_attestation` 経由）を渡していたが、
/// CD の中身としては無意味だったため引数ごと削除した。
const DEVICE_TYPE_ID_IN_CD: u32 = 22;

// CMS で使う OID（tag/len を除いた中身のバイト列）。
const OID_PKCS7_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x01];
const OID_PKCS7_SIGNED_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_ECDSA_WITH_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];

/// CD の中身（spec §6.3.1 の `cd-struct` TLV）を組む。
///
/// タグは昇順に並べる — chip の `DecodeCertificationElements` は
/// `reader.Next(ContextTag(..))` を順番に呼ぶ実装で、順序が違うと
/// `kCertificationDeclarationInvalidFormat` になる。
///
/// `dac_origin_*` と `authorized_paa_list`（タグ 9〜11）は optional で、
/// 出すとコミッショナ側の追加の突き合わせ（DAC/PAI の VID/PID 一致、
/// PAA SKID の許可リスト）が有効になる。ここでは出さない — DAC と CD を
/// 同じ VID/PID で自分が発行しているので突き合わせる相手がいない。
pub fn encode_certification_elements(vendor_id: u16, product_id: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(FORMAT_VERSION));
    w.put_uint(Tag::Context(1), u64::from(vendor_id));
    w.start_array(Tag::Context(2));
    w.put_uint(Tag::Anonymous, u64::from(product_id));
    w.end_container();
    w.put_uint(Tag::Context(3), u64::from(DEVICE_TYPE_ID_IN_CD));
    w.put_str(Tag::Context(4), CERTIFICATE_ID);
    w.put_uint(Tag::Context(5), 0); // security_level（コミッショナは解釈しない）
    w.put_uint(Tag::Context(6), 0); // security_information（同上）
    w.put_uint(Tag::Context(7), 1); // version_number（同上）
    w.put_uint(
        Tag::Context(8),
        u64::from(CERTIFICATION_TYPE_DEVELOPMENT_AND_TEST),
    );
    w.end_container();
    w.finish()
}

/// CD の中身を CMS SignedData（RFC 5652 のサブセット）で包んで署名する。
/// chip の `Credentials::CMS_Sign` が出すのとバイト単位で同じ形:
///
/// ```text
/// SEQUENCE {
///   OBJECT IDENTIFIER id-signedData
///   [0] EXPLICIT SEQUENCE {
///     INTEGER 3                                    -- CMSVersion v3
///     SET { SEQUENCE { OBJECT IDENTIFIER sha256 }} -- digestAlgorithms
///     SEQUENCE {                                   -- encapContentInfo
///       OBJECT IDENTIFIER id-data
///       [0] EXPLICIT OCTET STRING cd_content
///     }
///     SET { SEQUENCE {                             -- signerInfos
///       INTEGER 3
///       [0] IMPLICIT OCTET STRING signer_key_id
///       SEQUENCE { OBJECT IDENTIFIER sha256 }
///       SEQUENCE { OBJECT IDENTIFIER ecdsa-with-SHA256 }
///       OCTET STRING der_ecdsa_signature
///     }}
///   }
/// }
/// ```
///
/// 署名対象は `cd_content` の生バイト列そのもの（signedAttrs は使わない）。
pub fn cms_sign(
    cd_content: &[u8],
    signer_key_id: &[u8],
    signer_private_key: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    let raw_sig = sign_ecdsa_p256(signer_private_key, cd_content)?;

    let digest_algorithms = asn1::set_of(&[&asn1::seq(&[&asn1::oid(OID_SHA256)])]);
    let encap_content_info = asn1::seq(&[
        &asn1::oid(OID_PKCS7_DATA),
        &asn1::context_constructed(0, &asn1::octet_string(cd_content)),
    ]);
    let signer_infos = asn1::set_of(&[&asn1::seq(&[
        &asn1::integer(&[3]),
        &asn1::context_primitive(0, signer_key_id),
        &asn1::seq(&[&asn1::oid(OID_SHA256)]),
        &asn1::seq(&[&asn1::oid(OID_ECDSA_WITH_SHA256)]),
        &asn1::octet_string(&asn1::ecdsa_signature(&raw_sig)),
    ])]);
    let signed_data = asn1::seq(&[
        &asn1::integer(&[3]),
        &digest_algorithms,
        &encap_content_info,
        &signer_infos,
    ]);

    Ok(asn1::seq(&[
        &asn1::oid(OID_PKCS7_SIGNED_DATA),
        &asn1::context_constructed(0, &signed_data),
    ]))
}

/// 開発用デバイスの CD を 1 本作る: `vendor_id`/`product_id`の CD を組み、
/// 公開テスト鍵（[`TEST_CD_SIGNING_KEY`]）で CMS 署名する。
/// **CSA 認証済みの CD ではない** — モジュール doc 参照。
///
/// 以前は `device_type` も引数に取っていたが、CD に載る `device_type_id`
/// は常に固定値 [`DEVICE_TYPE_ID_IN_CD`]（matter.js の正典値）になった
/// ため削除した（`CERTIFICATE_ID`/`DEVICE_TYPE_ID_IN_CD` の doc 参照）。
pub fn generate_dev_certification_declaration(
    vendor_id: u16,
    product_id: u16,
) -> Result<Vec<u8>, CryptoError> {
    let content = encode_certification_elements(vendor_id, product_id);
    cms_sign(&content, &TEST_CD_SIGNING_KEY_ID, &TEST_CD_SIGNING_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::verify_ecdsa_p256;
    use crate::tlv::{Reader, Value};

    /// chip の `gTestCdPubkeyBytes`。定数の秘密鍵がその公開鍵と対になって
    /// いること（＝コミッショナが引く公開鍵で本当に検証できること）を
    /// 固定する。
    const TEST_CD_PUBKEY: [u8; 65] = [
        0x04, 0x3C, 0x39, 0x89, 0x22, 0x45, 0x2B, 0x55, 0xCA, 0xF3, 0x89, 0xC2, 0x5B, 0xD1, 0xBC,
        0xA4, 0x65, 0x69, 0x52, 0xCC, 0xB9, 0x0E, 0x88, 0x69, 0x24, 0x9A, 0xD8, 0x47, 0x46, 0x53,
        0x01, 0x4C, 0xBF, 0x95, 0xD6, 0x87, 0x96, 0x5E, 0x03, 0x6B, 0x52, 0x1C, 0x51, 0x03, 0x7E,
        0x6B, 0x8C, 0xED, 0xEF, 0xCA, 0x1E, 0xB4, 0x40, 0x46, 0x69, 0x4F, 0xA0, 0x88, 0x82, 0xEE,
        0xD6, 0x51, 0x9D, 0xEC, 0xBA,
    ];

    /// DER の 1 要素を `(tag, content)` に割る（テスト用の最小パーサ）。
    fn der_split(buf: &[u8]) -> (u8, &[u8]) {
        let tag = buf[0];
        let first = buf[1];
        let (len, off) = if first & 0x80 == 0 {
            (usize::from(first), 2)
        } else {
            let n = usize::from(first & 0x7F);
            let mut v = 0usize;
            for b in &buf[2..2 + n] {
                v = (v << 8) | usize::from(*b);
            }
            (v, 2 + n)
        };
        (tag, &buf[off..off + len])
    }

    #[test]
    fn certification_elements_are_in_ascending_tag_order() {
        let el = encode_certification_elements(0xFFF1, 0x8000);
        let mut r = Reader::new(&el);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        let mut seen: Vec<u8> = Vec::new();
        let mut depth = 0usize;
        while let Some(el) = r.next().unwrap() {
            match el.value {
                Value::ArrayStart | Value::StructStart | Value::ListStart => {
                    if let Tag::Context(t) = el.tag {
                        if depth == 0 {
                            seen.push(t);
                        }
                    }
                    depth += 1;
                }
                Value::ContainerEnd => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {
                    if let Tag::Context(t) = el.tag {
                        if depth == 0 {
                            seen.push(t);
                        }
                    }
                }
            }
        }
        assert_eq!(
            seen,
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8],
            "chip の DecodeCertificationElements はタグを昇順に読む"
        );
    }

    #[test]
    fn certificate_id_is_exactly_nineteen_chars() {
        assert_eq!(CERTIFICATE_ID.chars().count(), 19);
    }

    /// CMS 封筒の構造を chip の `CMS_ExtractKeyId` / `CMS_Verify` と同じ
    /// 順でたどり、(a) 署名者 key id が取り出せること、(b) 中身が元の CD
    /// であること、(c) 署名がテスト鍵の公開鍵で検証できることを確かめる。
    #[test]
    fn cms_envelope_matches_what_chip_parses() {
        let content = encode_certification_elements(0xFFF1, 0x8000);
        let cms = generate_dev_certification_declaration(0xFFF1, 0x8000).unwrap();

        // SEQUENCE { OID signedData, [0] { SignedData } }
        let (tag, outer) = der_split(&cms);
        assert_eq!(tag, 0x30);
        let (tag, oid) = der_split(outer);
        assert_eq!((tag, oid), (0x06, OID_PKCS7_SIGNED_DATA));
        let (tag, explicit) = der_split(&outer[2 + oid.len()..]);
        assert_eq!(tag, 0xA0);
        let (tag, signed_data) = der_split(explicit);
        assert_eq!(tag, 0x30);

        // version INTEGER 3
        let (tag, version) = der_split(signed_data);
        assert_eq!((tag, version), (0x02, &[3u8][..]));
        let mut rest = &signed_data[2 + version.len()..];

        // digestAlgorithms SET { SEQUENCE { sha256 } }
        let (tag, digest_algs) = der_split(rest);
        assert_eq!(tag, 0x31);
        rest = &rest[2 + digest_algs.len()..];

        // encapContentInfo SEQUENCE { OID id-data, [0] { OCTET STRING } }
        let (tag, encap) = der_split(rest);
        assert_eq!(tag, 0x30);
        rest = &rest[2 + encap.len()..];
        let (tag, oid) = der_split(encap);
        assert_eq!((tag, oid), (0x06, OID_PKCS7_DATA));
        let (tag, econtent_explicit) = der_split(&encap[2 + oid.len()..]);
        assert_eq!(tag, 0xA0);
        let (tag, econtent) = der_split(econtent_explicit);
        assert_eq!(tag, 0x04);
        assert_eq!(econtent, content, "eContent は CD の生バイト列そのもの");

        // signerInfos SET { SEQUENCE { 3, [0] keyid, sha256, ecdsa, sig } }
        let (tag, signer_infos) = der_split(rest);
        assert_eq!(tag, 0x31);
        let (tag, si) = der_split(signer_infos);
        assert_eq!(tag, 0x30);
        let (_, si_version) = der_split(si);
        let si_rest = &si[2 + si_version.len()..];
        let (tag, key_id) = der_split(si_rest);
        assert_eq!(tag, 0x80, "signer key id は [0] IMPLICIT OCTET STRING");
        assert_eq!(key_id, TEST_CD_SIGNING_KEY_ID);

        // 署名は eContent（= CD 本体）に対する ECDSA-P256/SHA-256。
        let mut si_tail = &si_rest[2 + key_id.len()..];
        for _ in 0..2 {
            let (_, alg) = der_split(si_tail);
            si_tail = &si_tail[2 + alg.len()..];
        }
        let (tag, sig_der) = der_split(si_tail);
        assert_eq!(tag, 0x04);
        let (tag, sig_seq) = der_split(sig_der);
        assert_eq!(tag, 0x30);
        let (_, r) = der_split(sig_seq);
        let (_, s) = der_split(&sig_seq[2 + r.len()..]);
        let mut raw = [0u8; 64];
        raw[32 - r.len().min(32)..32].copy_from_slice(&r[r.len().saturating_sub(32)..]);
        raw[64 - s.len().min(32)..].copy_from_slice(&s[s.len().saturating_sub(32)..]);
        verify_ecdsa_p256(&TEST_CD_PUBKEY, &content, &raw)
            .expect("CD signature must verify under the well-known test CD public key");
    }
}
