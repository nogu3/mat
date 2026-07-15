//! SPAKE2+ (P256-SHA256-HKDF-HMAC) — Matter spec §3.10 / RFC 9383。
//!
//! controller は常に prover（A 側）。verifier 素材の計算（w0/L）は
//! open-window の PAKEPasscodeVerifier 生成にだけ使う。TT (transcript) の
//! 組み立ては RFC 9383 §3.3 に従うが、TT ハッシュ後の鍵導出 (Ka/Ke split と
//! 確認鍵 KcA/KcB) は Matter spec §3.10.3 固有のレイアウトで、RFC 9383 本体
//! の KDF とは異なる点に注意。

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{AffinePoint, EncodedPoint, ProjectivePoint, Scalar};
use sha2::{Digest, Sha256};

use crate::case::random_p256_secret;

/// RFC 9383 §4 の P-256 用固定点 M（compressed SEC1, 33 bytes）。
pub const SPAKE_M: [u8; 33] = [
    0x02, 0x88, 0x6e, 0x2f, 0x97, 0xac, 0xe4, 0x6e, 0x55, 0xba, 0x9d, 0xd7, 0x24, 0x25, 0x79, 0xf2,
    0x99, 0x3b, 0x64, 0xe1, 0x6e, 0xf3, 0xdc, 0xab, 0x95, 0xaf, 0xd4, 0x97, 0x33, 0x3d, 0x8f, 0xa1,
    0x2f,
];

/// RFC 9383 §4 の P-256 用固定点 N（compressed SEC1, 33 bytes）。
pub const SPAKE_N: [u8; 33] = [
    0x03, 0xd8, 0xbb, 0xd6, 0xc6, 0x39, 0xc6, 0x29, 0x37, 0xb0, 0x4d, 0x99, 0x7f, 0x38, 0xc3, 0x77,
    0x07, 0x19, 0xc6, 0x29, 0xd7, 0x01, 0x4d, 0x49, 0xa2, 0x4b, 0x4f, 0x98, 0xba, 0xa1, 0x29, 0x2b,
    0x49,
];

/// SPAKE2+ の失敗要因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpakeError {
    /// 相手の共有ポイント（pA/pB）が SEC1 として不正、または曲線上にない。
    BadPoint,
    /// 相手の共有ポイントが単位元（無限遠点）。なりすまし防止のため拒否する
    /// (RFC 9383 §3.2: shareP/shareV must not be the identity element)。
    Identity,
}

impl std::fmt::Display for SpakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpakeError::BadPoint => {
                write!(f, "spake2+: peer share point is not a valid P-256 point")
            }
            SpakeError::Identity => {
                write!(f, "spake2+: peer share point is the identity element")
            }
        }
    }
}

impl std::error::Error for SpakeError {}

/// 40 バイト big-endian を曲線位数 n で還元して Scalar にする。
/// (Scalar 演算は mod n なので、バイトごとの畳み込みで正確に還元できる)
pub(crate) fn scalar_from_be_bytes_mod_n(bytes: &[u8]) -> Scalar {
    let b256 = Scalar::from(256u64);
    bytes.iter().fold(Scalar::ZERO, |acc, b| {
        acc * b256 + Scalar::from(u64::from(*b))
    })
}

/// spec §3.10: PBKDF2-SHA256(passcode の u32 LE, salt, iterations) 80 バイト
/// → 前半 40B が w0s、後半 40B が w1s。それぞれ mod n。
pub fn derive_w0_w1(passcode: u32, salt: &[u8], iterations: u32) -> (Scalar, Scalar) {
    let mut ws = [0u8; 80];
    pbkdf2::pbkdf2_hmac::<Sha256>(&passcode.to_le_bytes(), salt, iterations, &mut ws);
    (
        scalar_from_be_bytes_mod_n(&ws[..40]),
        scalar_from_be_bytes_mod_n(&ws[40..]),
    )
}

/// open-window（PASE 招待コード発行）用の PAKEPasscodeVerifier:
/// w0(32B BE) || L(65B uncompressed SEC1)。spec §3.10 / §5.4.2。
pub fn compute_verifier(passcode: u32, salt: &[u8], iterations: u32) -> [u8; 97] {
    let (w0, w1) = derive_w0_w1(passcode, salt, iterations);
    let l = ProjectivePoint::GENERATOR * w1;
    let mut out = [0u8; 97];
    out[..32].copy_from_slice(&w0.to_bytes());
    out[32..].copy_from_slice(&encode_point(&l));
    out
}

fn decode_point(bytes: &[u8]) -> Result<ProjectivePoint, SpakeError> {
    let ep = EncodedPoint::from_bytes(bytes).map_err(|_| SpakeError::BadPoint)?;
    let ap = Option::<AffinePoint>::from(AffinePoint::from_encoded_point(&ep))
        .ok_or(SpakeError::BadPoint)?;
    let p = ProjectivePoint::from(ap);
    if p == ProjectivePoint::IDENTITY {
        return Err(SpakeError::Identity);
    }
    Ok(p)
}

fn encode_point(p: &ProjectivePoint) -> [u8; 65] {
    p.to_affine()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .expect("uncompressed SEC1 P-256 point is always 65 bytes")
}

/// TT の要素: u64 LE の長さ || バイト列（RFC 9383 §3.3）。
fn tt_elem(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// TT (transcript) を RFC 9383 §3.3 の順序で組み立てる:
/// Context, idProver, idVerifier, M, N, pA, pB, Z, V, w0。
/// prover / verifier どちらの役でも Z・V さえ計算できれば同じ TT になる —
/// 自己整合性テスト (verifier 役) から直接呼べるようフリー関数にしてある。
#[allow(clippy::too_many_arguments)]
fn build_transcript(
    context: &[u8],
    id_p: &[u8],
    id_v: &[u8],
    p_a: &[u8],
    p_b: &[u8],
    z: &ProjectivePoint,
    v: &ProjectivePoint,
    w0: &Scalar,
) -> Vec<u8> {
    let m = decode_point(&SPAKE_M).expect("SPAKE_M is a valid embedded constant");
    let n = decode_point(&SPAKE_N).expect("SPAKE_N is a valid embedded constant");
    let mut tt = Vec::new();
    tt_elem(&mut tt, context);
    tt_elem(&mut tt, id_p);
    tt_elem(&mut tt, id_v);
    tt_elem(&mut tt, &encode_point(&m));
    tt_elem(&mut tt, &encode_point(&n));
    tt_elem(&mut tt, p_a);
    tt_elem(&mut tt, p_b);
    tt_elem(&mut tt, &encode_point(z));
    tt_elem(&mut tt, &encode_point(v));
    tt_elem(&mut tt, &w0.to_bytes());
    tt
}

/// SHA256(TT) を Ka(前半16B) / Ke(後半16B) に分割する。Matter 固有の分割
/// (spec §3.10.3) — RFC 9383 本体の K_main/K_confirmP/K_confirmV とは異なる。
fn split_hash(tt: &[u8]) -> ([u8; 16], [u8; 16]) {
    let hash = Sha256::digest(tt);
    let mut k_a = [0u8; 16];
    let mut k_e = [0u8; 16];
    k_a.copy_from_slice(&hash[..16]);
    k_e.copy_from_slice(&hash[16..32]);
    (k_a, k_e)
}

/// HKDF-SHA256(salt=[], ikm=Ka, info="ConfirmationKeys") 32B
/// → KcA(前半16B) / KcB(後半16B)（spec §3.10.3）。
fn confirmation_keys(k_a: &[u8; 16]) -> ([u8; 16], [u8; 16]) {
    let hk = Hkdf::<Sha256>::new(Some(&[]), k_a);
    let mut kc = [0u8; 32];
    hk.expand(b"ConfirmationKeys", &mut kc)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    let mut kc_a = [0u8; 16];
    let mut kc_b = [0u8; 16];
    kc_a.copy_from_slice(&kc[..16]);
    kc_b.copy_from_slice(&kc[16..]);
    (kc_a, kc_b)
}

fn hmac32(key: &[u8; 16], msg: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// `case::random_p256_secret` と同じ乱数源から Scalar を得る（0 は
/// `random_p256_secret` 側で引き直し済み、ここでは Deref するだけ）。
fn random_scalar() -> Scalar {
    *random_p256_secret().to_nonzero_scalar()
}

/// SPAKE2+ 完了後の共有材料。`c_a` は自分（controller = prover）が相手に送る
/// 確認メッセージ、`expected_c_b` は相手（verifier）から届くはずの確認
/// メッセージの期待値、`k_e` は後続のセッション鍵導出に使う暗号鍵
/// (spec §3.10.3)。`k_e` は実質的にセッション鍵の種であり秘密情報 —
/// `Debug` は意図的に実装しない（このリポジトリは public）。
pub struct PakeShared {
    pub c_a: [u8; 32],
    pub expected_c_b: [u8; 32],
    pub k_e: [u8; 16],
}

/// SPAKE2+ prover（controller は常にこちら = A 側）。w0/w1/x を保持する。
/// いずれも秘密のパスコード由来の値 / 一時乱数であり、このリポジトリは
/// public なので `Debug` は絶対に derive しない
/// (`fabric::FabricCredentials` の手動 `Debug` と同じ理由)。
pub struct Spake2pProver {
    w0: Scalar,
    w1: Scalar,
    x: Scalar,
}

impl Spake2pProver {
    /// 新しい prover を作る。`x` は毎回新規の乱数（spec §3.10 手順1）。
    pub fn new(w0: Scalar, w1: Scalar) -> Self {
        Self::new_with_x(w0, w1, random_scalar())
    }

    /// `x` を固定して prover を作る。RFC 9383 のテストベクタ検証専用。
    pub(crate) fn new_with_x(w0: Scalar, w1: Scalar, x: Scalar) -> Self {
        Self { w0, w1, x }
    }

    /// pA = x*P + w0*M （RFC 9383 §3.2 の prover 側 shareP、spec §3.10 手順1）。
    pub fn p_a(&self) -> [u8; 65] {
        let m = decode_point(&SPAKE_M).expect("SPAKE_M is a valid embedded constant");
        let p_a = ProjectivePoint::GENERATOR * self.x + m * self.w0;
        encode_point(&p_a)
    }

    /// TT (transcript) を計算する内部関数。RFC 9383 ベクタ検証のため
    /// クレート内に公開する（ハッシュ前の生バイト列を直接比較したいので）。
    pub(crate) fn transcript(
        &self,
        p_b: &[u8],
        context: &[u8],
        id_p: &[u8],
        id_v: &[u8],
    ) -> Result<Vec<u8>, SpakeError> {
        let p_b_point = decode_point(p_b)?;
        let n = decode_point(&SPAKE_N).expect("SPAKE_N is a valid embedded constant");
        let t = p_b_point - n * self.w0;
        let z = t * self.x;
        let v = t * self.w1;
        Ok(build_transcript(
            context,
            id_p,
            id_v,
            &self.p_a(),
            p_b,
            &z,
            &v,
            &self.w0,
        ))
    }

    /// SPAKE2+ を完了する（spec §3.10 手順2-3）。`p_b` は相手（verifier）から
    /// 届いた shareV。戻り値の `c_a` を相手に送り、相手の確認メッセージが
    /// `expected_c_b` と一致することを確認してからセッションを確立する。
    pub fn finish(
        &self,
        p_b: &[u8],
        context: &[u8],
        id_p: &[u8],
        id_v: &[u8],
    ) -> Result<PakeShared, SpakeError> {
        let tt = self.transcript(p_b, context, id_p, id_v)?;
        let (k_a, k_e) = split_hash(&tt);
        let (kc_a, kc_b) = confirmation_keys(&k_a);
        Ok(PakeShared {
            c_a: hmac32(&kc_a, p_b),
            expected_c_b: hmac32(&kc_b, &self.p_a()),
            k_e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        // 空白除去付き hex デコード（テスト専用の素朴な実装で良い）
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // RFC 9383 Appendix C, "SPAKE2+-P256-SHA256-HKDF-SHA256-HMAC-SHA256 Test
    // Vectors" から転記（取得元: https://www.rfc-editor.org/rfc/rfc9383.txt,
    // 2026-07-13）。Context/idProver/idVerifier/w0/w1/x/shareP/shareV/TT の
    // 生ベクタ。K_main 以降（Ka/Ke split, KcA/KcB）は Matter 固有レイアウトの
    // ため RFC ベクタでは検証できない — それは `prover_and_test_verifier_agree`
    // で自己整合性として確認する。
    const CONTEXT_HEX: &str = "\
        5350414b45322b2d503235362d5348413235362d484b44462d5348413235362d484d4143\
        2d534841323536205465737420566563746f7273";

    const W0_HEX: &str = "\
        bb8e1bbcf3c48f62c08db243652ae55d3e5586053fca77102994f23ad95491b3";

    const W1_HEX: &str = "\
        7e945f34d78785b8a3ef44d0df5a1a97d6b3b460409a345ca7830387a74b1dba";

    const X_HEX: &str = "\
        d1232c8e8693d02368976c174e2088851b8365d0d79a9eee709c6a05a2fad539";

    const SHARE_P_HEX: &str = "\
        04ef3bd051bf78a2234ec0df197f7828060fe9856503579bb1733009042c15c0c1de1277\
        27f418b5966afadfdd95a6e4591d171056b333dab97a79c7193e341727";

    const SHARE_V_HEX: &str = "\
        04c0f65da0d11927bdf5d560c69e1d7d939a05b0e88291887d679fcadea75810fb5cc1ca\
        7494db39e82ff2f50665255d76173e09986ab46742c798a9a68437b048";

    const TT_HEX: &str = "\
        38000000000000005350414b45322b2d503235362d5348413235362d484b44462d534841\
        3235362d484d41432d534841323536205465737420566563746f72730600000000000000\
        636c69656e740600000000000000736572766572410000000000000004886e2f97ace46e\
        55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f5ff355163e43ce224e0b0e\
        65ff02ac8e5c7be09419c785e0ca547d55a12e2d20410000000000000004d8bbd6c639c6\
        2937b04d997f38c3770719c629d7014d49a24b4f98baa1292b4907d60aa6bfade45008a6\
        36337f5168c64d9bd36034808cd564490b1e656edbe7410000000000000004ef3bd051bf\
        78a2234ec0df197f7828060fe9856503579bb1733009042c15c0c1de127727f418b5966a\
        fadfdd95a6e4591d171056b333dab97a79c7193e341727410000000000000004c0f65da0\
        d11927bdf5d560c69e1d7d939a05b0e88291887d679fcadea75810fb5cc1ca7494db39e8\
        2ff2f50665255d76173e09986ab46742c798a9a68437b048410000000000000004bbfce7\
        dd7f277819c8da21544afb7964705569bdf12fb92aa388059408d50091a0c5f1d3127f56\
        813b5337f9e4e67e2ca633117a4fbd559946ab474356c4183941000000000000000458bf\
        27c6bca011c9ce1930e8984a797a3419797b936629a5a937cf2f11c8b9514b82b993da8a\
        46e664f23db7c01edc87faa530db01c2ee405230b18997f16b682000000000000000bb8e\
        1bbcf3c48f62c08db243652ae55d3e5586053fca77102994f23ad95491b3";

    #[test]
    fn rfc9383_p256_vector() {
        let w0 = scalar_from_be_bytes_mod_n(&h(W0_HEX));
        let w1 = scalar_from_be_bytes_mod_n(&h(W1_HEX));
        let x = scalar_from_be_bytes_mod_n(&h(X_HEX));
        let prover = Spake2pProver::new_with_x(w0, w1, x);
        assert_eq!(prover.p_a().to_vec(), h(SHARE_P_HEX));

        // TT の中身（Z/V を含む）はベクタの TT と一致するはず。
        let tt = prover
            .transcript(&h(SHARE_V_HEX), &h(CONTEXT_HEX), b"client", b"server")
            .unwrap();
        assert_eq!(tt, h(TT_HEX));
    }

    /// 自己整合: テスト内にミニマムな verifier 役を実装し、prover と突き合わせる。
    /// (Matter 固有の Ka/Ke/確認キー分割は RFC ベクタでは検証できないため、
    ///  両役を自前実装して cA/cB/Ke が一致することを確認する)
    #[test]
    fn prover_and_test_verifier_agree() {
        let (w0, w1) = derive_w0_w1(20202021, b"SPAKE2P Key Salt", 1000);
        let prover = Spake2pProver::new(w0, w1);
        let l = ProjectivePoint::GENERATOR * w1;

        // verifier 役 (デバイス側): y は乱数、pB = y*P + w0*N
        let y = random_scalar();
        let n = decode_point(&SPAKE_N).unwrap();
        let m = decode_point(&SPAKE_M).unwrap();
        let p_b_point = ProjectivePoint::GENERATOR * y + n * w0;
        let p_b = encode_point(&p_b_point);

        let ctx = b"test context";
        let shared = prover.finish(&p_b, ctx, b"", b"").unwrap();

        // verifier 側で同じ TT を組む: Z = y*(pA - w0*M), V = y*L
        let p_a_point = decode_point(&prover.p_a()).unwrap();
        let z = (p_a_point - m * w0) * y;
        let v = l * y;
        let tt = build_transcript(ctx, b"", b"", &prover.p_a(), &p_b, &z, &v, &w0);
        let (k_a, k_e) = split_hash(&tt);
        let (kc_a, kc_b) = confirmation_keys(&k_a);

        assert_eq!(shared.k_e, k_e);
        assert_eq!(shared.c_a, hmac32(&kc_a, &p_b));
        assert_eq!(shared.expected_c_b, hmac32(&kc_b, &prover.p_a()));
    }

    #[test]
    fn verifier_bytes_are_w0_l() {
        let v = compute_verifier(20202021, b"salt-0123456789abcdef", 1000);
        let (w0, w1) = derive_w0_w1(20202021, b"salt-0123456789abcdef", 1000);
        assert_eq!(&v[..32], w0.to_bytes().as_slice());
        let l = ProjectivePoint::GENERATOR * w1;
        assert_eq!(&v[32..], encode_point(&l).as_slice());
    }

    #[test]
    fn rejects_bad_peer_point() {
        let (w0, w1) = derive_w0_w1(1, b"s", 1000);
        let p = Spake2pProver::new(w0, w1);
        assert!(matches!(
            p.finish(&[0u8; 65], b"c", b"", b""),
            Err(SpakeError::BadPoint)
        ));
    }
}
