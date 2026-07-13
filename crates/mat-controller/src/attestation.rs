//! Device Attestation 検証（Matter Core Spec §6.2 Device Attestation, §11.17
//! CommissioningComplete 前段の Attestation ステージ）。
//!
//! commissioning 中に受け取る DAC（Device Attestation Certificate）/PAI
//! （Product Attestation Intermediate）証明書と AttestationResponse
//! （elements + signature）を検証する。検証は 2 段に分かれる:
//!
//! - **チェーン / nonce / 署名（厳格）**: DAC→PAI→PAA の証明書チェーン、
//!   attestation 署名、nonce 一致のいずれかが崩れれば `Err` を返し、
//!   呼び出し側（commissioning フロー）はただちに中断しなければならない。
//! - **CD（Certification Declaration、warn のみ）**: CMS SignedData の
//!   パース失敗・CD 署名者証明書の欠落・CD 署名検証失敗・DAC との
//!   VID/PID 不一致は、いずれも `tracing::warn!` を出すだけで検証全体は
//!   継続する。CSA の signer 鍵ローテーションやベンダーの CD 実装の
//!   癖でホームコントローラの commissioning が丸ごとブロックされる
//!   事態を避けるための意図的な仕様判断（2026-07-13 ユーザー決定）。

use std::path::Path;

use crate::tlv::{Reader, Tag, Value};
use crate::x509::{parse_x509, DerReader, X509Cert, X509Error};

/// Attestation 検証エラー。strict 系のバリアントのみがここに現れる
/// （CD 関連の不備は `verify_cd_warn` 内で warn ログに落として `()` を返す
/// ため、決してここには現れない）。
#[derive(Debug)]
pub enum AttestationError {
    /// DAC→PAI→PAA チェーンの issuer/subject 不一致・署名不一致・PAA 未発見。
    Chain(&'static str),
    /// elements 中の attestation_nonce が呼び出し側の期待値と不一致。
    Nonce,
    /// `elements ‖ attestation_challenge` に対する DAC 署名の検証失敗。
    Signature,
    /// elements（AttestationElements TLV）自体のパース失敗・必須フィールド欠落。
    Elements(&'static str),
    /// DAC/PAI の X.509 DER パース失敗。
    X509(X509Error),
    /// `load_der_dir` のファイル I/O 失敗。
    Io(std::io::Error),
}

impl std::fmt::Display for AttestationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttestationError::Chain(msg) => write!(f, "attestation chain error: {msg}"),
            AttestationError::Nonce => write!(f, "attestation nonce mismatch"),
            AttestationError::Signature => {
                write!(f, "attestation signature verification failed")
            }
            AttestationError::Elements(msg) => write!(f, "attestation elements error: {msg}"),
            AttestationError::X509(e) => write!(f, "x509 error: {e}"),
            AttestationError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for AttestationError {}

impl From<X509Error> for AttestationError {
    fn from(e: X509Error) -> Self {
        AttestationError::X509(e)
    }
}

impl From<std::io::Error> for AttestationError {
    fn from(e: std::io::Error) -> Self {
        AttestationError::Io(e)
    }
}

/// `dir` 内の `*.der`（大文字小文字無視）ファイルを全部読んで返す
/// （PAA 信頼ストア / CD signer 証明書ストアのロードに使う）。空ディレクトリは
/// 空 `Vec`。ディレクトリ自体が無い・読めない等は `Err`。
pub fn load_der_dir(dir: &Path) -> Result<Vec<Vec<u8>>, AttestationError> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_der = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("der"));
        if !is_der || !path.is_file() {
            continue;
        }
        out.push(std::fs::read(&path)?);
    }
    Ok(out)
}

/// Device Attestation を検証する（spec §6.2 / §11.17）。
///
/// strict 部（チェーン・attestation 署名・nonce）が 1 つでも失敗すれば
/// `Err` を返し、commissioning は中止しなければならない。CD の検証は
/// warn のみで、成功しても失敗してもこの関数の戻り値には影響しない。
///
/// 8 引数は Task 10（commissioning フロー）から固定インタフェースとして
/// 消費される契約（本ブリーフ #Interfaces）— まとめて構造体化しない。
#[allow(clippy::too_many_arguments)]
pub fn verify_device_attestation(
    dac_der: &[u8],
    pai_der: &[u8],
    paa_ders: &[Vec<u8>],
    cd_signer_ders: &[Vec<u8>],
    elements: &[u8],
    signature: &[u8; 64],
    expected_nonce: &[u8; 32],
    attestation_challenge: &[u8; 16],
) -> Result<(), AttestationError> {
    let dac = parse_x509(dac_der)?;
    let pai = parse_x509(pai_der)?;

    // --- チェーン（厳格）: DAC は PAI の署名を持つか ---
    if dac.issuer != pai.subject {
        return Err(AttestationError::Chain("dac issuer != pai subject"));
    }
    dac.verify_signed_by(&pai)
        .map_err(|_| AttestationError::Chain("dac signature"))?;

    // --- チェーン（厳格）: PAI は信頼ストア中の PAA いずれかの署名を持つか ---
    // issuer/subject のバイト一致に加え、双方に AKID/SKID があれば
    // それも一致することを要求して絞り込む（同名で鍵違いの偽 PAA を弾く）。
    let paa = paa_ders
        .iter()
        .filter_map(|der| parse_x509(der).ok())
        .find(|paa| {
            pai.issuer == paa.subject
                && match (&pai.akid, &paa.skid) {
                    (Some(akid), Some(skid)) => akid == skid,
                    _ => true,
                }
        })
        .ok_or(AttestationError::Chain("no matching PAA in trust store"))?;
    pai.verify_signed_by(&paa)
        .map_err(|_| AttestationError::Chain("pai signature"))?;

    // --- チェーン（厳格）: DAC↔PAI の Matter VID/PID 整合（spec §6.2.2.2）---
    match (dac.vid, pai.vid) {
        (Some(dac_vid), Some(pai_vid)) if dac_vid == pai_vid => {}
        _ => return Err(AttestationError::Chain("dac/pai vid mismatch")),
    }
    // PAI の PID は省略可（spec 上 optional）。あれば DAC と一致必須。
    if let Some(pai_pid) = pai.pid {
        if dac.pid != Some(pai_pid) {
            return Err(AttestationError::Chain("dac/pai pid mismatch"));
        }
    }

    // --- チェーン（厳格）: basicConstraints cA（spec §6.2.2.1）---
    // PAI/PAA は CA 証明書（cA=true）でなければならない。DAC は CA
    // 証明書であってはならない（cA=false または拡張なしのみ許容）。
    if pai.is_ca != Some(true) {
        return Err(AttestationError::Chain("pai is not a ca certificate"));
    }
    if paa.is_ca != Some(true) {
        return Err(AttestationError::Chain("paa is not a ca certificate"));
    }
    if dac.is_ca == Some(true) {
        return Err(AttestationError::Chain("dac must not be a ca certificate"));
    }

    // --- 有効期間（warn のみ）: 時計ずれ・特殊 notAfter 運用で
    // commissioning を壊さないための意図的仕様（brief #4、M6a spec 決定 3 の
    // 哲学を踏襲）。
    warn_if_out_of_validity("dac", &dac);
    warn_if_out_of_validity("pai", &pai);
    warn_if_out_of_validity("paa", &paa);

    // --- attestation 署名（厳格）: DAC 秘密鍵が elements‖challenge に署名したか ---
    let mut msg = Vec::with_capacity(elements.len() + attestation_challenge.len());
    msg.extend_from_slice(elements);
    msg.extend_from_slice(attestation_challenge);
    crate::crypto::verify_ecdsa_p256(&dac.public_key, &msg, signature)
        .map_err(|_| AttestationError::Signature)?;

    // --- elements 解析 + nonce（厳格）---
    let (cd_bytes, nonce) = parse_elements(elements)?;
    if nonce != *expected_nonce {
        return Err(AttestationError::Nonce);
    }

    // --- CD（warn のみ。2026-07-13 ユーザー決定）---
    verify_cd_warn(&cd_bytes, &dac, cd_signer_ders);

    Ok(())
}

/// notAfter がこの値（GeneralizedTime）なら「無期限」（spec の運用上の
/// 慣例）— warn しない。
const NO_EXPIRY_NOT_AFTER: &str = "99991231235959Z";

/// `label` 証明書（`"dac"`/`"pai"`/`"paa"`）の Validity が現在時刻を含んで
/// いるか確認し、外れていれば/解釈できなければ `tracing::warn!` して継続する
/// （brief #4: 有効期間は厳格失敗にしない）。
fn warn_if_out_of_validity(label: &'static str, cert: &X509Cert) {
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return; // システム時計がエポック以前 — 比較不能、何もしない
    };
    let now = now.as_secs() as i64;

    match cert.not_before.as_deref().map(parse_cert_time) {
        Some(Some((y, mo, d, h, mi, s))) => {
            let nb = epoch_seconds(y, mo, d, h, mi, s);
            if now < nb {
                tracing::warn!(
                    cert = label,
                    not_before = cert.not_before.as_deref(),
                    "certificate not yet valid — continuing"
                );
            }
        }
        _ => {
            tracing::warn!(
                cert = label,
                raw = ?cert.not_before,
                "certificate notBefore unparseable — continuing"
            );
        }
    }

    match cert.not_after.as_deref() {
        Some(NO_EXPIRY_NOT_AFTER) => {} // 無期限 — 警告しない
        Some(raw) => match parse_cert_time(raw) {
            Some((y, mo, d, h, mi, s)) => {
                let na = epoch_seconds(y, mo, d, h, mi, s);
                if now > na {
                    tracing::warn!(
                        cert = label,
                        not_after = raw,
                        "certificate expired — continuing"
                    );
                }
            }
            None => {
                tracing::warn!(
                    cert = label,
                    raw,
                    "certificate notAfter unparseable — continuing"
                );
            }
        },
        None => {
            tracing::warn!(
                cert = label,
                "certificate notAfter unparseable — continuing"
            );
        }
    }
}

/// 証明書 Time の生文字列（UTCTime `YYMMDDHHMMSSZ` / GeneralizedTime
/// `YYYYMMDDHHMMSSZ`）を `(year, month, day, hour, min, sec)` に分解する。
/// どちらの形式でもなければ `None`（呼び出し側で「解釈不能」として扱う）。
/// UTCTime の年は `YY < 50 → 20YY`、それ以外 `→ 19YY`（X.509 の慣例）。
fn parse_cert_time(raw: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let digits = raw.strip_suffix('Z')?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (year, rest) = if digits.len() == 12 {
        // UTCTime: YYMMDDHHMMSS
        let yy: i32 = digits[0..2].parse().ok()?;
        let year = if yy < 50 { 2000 + yy } else { 1900 + yy };
        (year, &digits[2..])
    } else if digits.len() == 14 {
        // GeneralizedTime: YYYYMMDDHHMMSS
        (digits[0..4].parse().ok()?, &digits[4..])
    } else {
        return None;
    };
    let mon: u32 = rest[0..2].parse().ok()?;
    let day: u32 = rest[2..4].parse().ok()?;
    let hour: u32 = rest[4..6].parse().ok()?;
    let min: u32 = rest[6..8].parse().ok()?;
    let sec: u32 = rest[8..10].parse().ok()?;
    if !(1..=12).contains(&mon) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    Some((year, mon, day, hour, min, sec))
}

/// `(year, month, day, hour, min, sec)` をエポック秒に変換する。
/// 日数計算は Howard Hinnant の `days_from_civil`（proleptic Gregorian 暦、
/// 1970-01-01 を 0 とする）— chrono 等の外部 crate を使わずに済ませるため
/// ここだけ自前で持つ。
fn epoch_seconds(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> i64 {
    days_from_civil(year, month, day) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(min) * 60
        + i64::from(sec)
}

/// Howard Hinnant `days_from_civil`。<http://howardhinnant.github.io/date_algorithms.html>
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(if m <= 2 { y - 1 } else { y });
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (i64::from(m) + 9) % 12; // [0, 11]: 3月=0 ... 2月=11
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// `AttestationElements ::= TLV struct { 1: certification_declaration OCTET
/// STRING, 2: attestation_nonce OCTET STRING(32), 3: timestamp UINT, ... }`
/// をパースし `(certification_declaration, attestation_nonce)` を返す。
/// tag1 欠落・tag2 欠落/長さ不正・TLV そのものが壊れている場合は
/// `Elements(..)`。
fn parse_elements(elements: &[u8]) -> Result<(Vec<u8>, [u8; 32]), AttestationError> {
    let mut r = Reader::new(elements);
    let first = r
        .next()
        .map_err(|_| AttestationError::Elements("tlv parse error"))?
        .ok_or(AttestationError::Elements("empty elements"))?;
    if !matches!(first.value, Value::StructStart) {
        return Err(AttestationError::Elements("elements not a struct"));
    }

    let mut cd: Option<Vec<u8>> = None;
    let mut nonce: Option<[u8; 32]> = None;
    loop {
        let el = r
            .next()
            .map_err(|_| AttestationError::Elements("tlv parse error"))?
            .ok_or(AttestationError::Elements("truncated elements"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => cd = Some(b.to_vec()),
            (Tag::Context(2), Value::Bytes(b)) => {
                nonce = Some(
                    b.try_into()
                        .map_err(|_| AttestationError::Elements("nonce wrong length"))?,
                );
            }
            _ => {} // timestamp / firmware_information 等は素通り
        }
    }

    let cd = cd.ok_or(AttestationError::Elements("no certification declaration"))?;
    let nonce = nonce.ok_or(AttestationError::Elements("no attestation nonce"))?;
    Ok((cd, nonce))
}

// ============================================================================
// CD（Certification Declaration）— warn only（決して Err を返さない）
// ============================================================================

/// CD（CMS SignedData に包まれた Matter TLV）を検証する。**Err を返さない** —
/// パース失敗・signer 証明書欠落・署名不一致・VID/PID 不一致はすべて
/// `tracing::warn!` して継続する（2026-07-13 ユーザー決定: CSA の signer 鍵
/// ローテーションやベンダー CD の癖で commissioning がブロックされてはいけない）。
fn verify_cd_warn(cd_bytes: &[u8], dac: &X509Cert, cd_signer_ders: &[Vec<u8>]) {
    let (cd_tlv, signer_info) = match parse_cms_signed_data(cd_bytes) {
        Ok(v) => v,
        Err(reason) => {
            tracing::warn!(
                reason,
                "certification declaration (CMS) unparseable — continuing"
            );
            return;
        }
    };

    match parse_cd_vid_pid(&cd_tlv) {
        Ok((vid, pids)) => {
            if let (Some(dac_vid), Some(cd_vid)) = (dac.vid, vid) {
                if dac_vid != cd_vid {
                    tracing::warn!(
                        dac_vid,
                        cd_vid,
                        "certification declaration vendor_id mismatch vs DAC — continuing"
                    );
                }
            }
            if let Some(dac_pid) = dac.pid {
                if !pids.is_empty() && !pids.contains(&dac_pid) {
                    tracing::warn!(
                        dac_pid,
                        ?pids,
                        "certification declaration product_id_array does not contain DAC pid — continuing"
                    );
                }
            }
        }
        Err(reason) => {
            tracing::warn!(
                reason,
                "certification declaration TLV unparseable — continuing"
            );
        }
    }

    verify_cd_signature_warn(&signer_info, cd_signer_ders);
}

/// CD の CMS 署名を検証する（warn only）。
fn verify_cd_signature_warn(signer_info: &Option<CdSignerInfo>, cd_signer_ders: &[Vec<u8>]) {
    if cd_signer_ders.is_empty() {
        tracing::warn!("certification declaration signature not verified (no cd signer certs provided) — continuing");
        return;
    }
    let Some(signer_info) = signer_info else {
        tracing::warn!(
            "certification declaration signature not verified (no signerInfo in CMS) — continuing"
        );
        return;
    };
    let Ok(raw_sig) = der_ecdsa_sig_to_raw64(&signer_info.signature) else {
        tracing::warn!("certification declaration signature encoding unparseable — continuing");
        return;
    };
    let verified = cd_signer_ders.iter().any(|der| {
        parse_x509(der).is_ok_and(|cert| {
            crate::crypto::verify_ecdsa_p256(&cert.public_key, &signer_info.signed_bytes, &raw_sig)
                .is_ok()
        })
    });
    if !verified {
        tracing::warn!(
            "certification declaration signature verification failed against all provided cd signer certs — continuing"
        );
    }
}

/// CMS SignerInfo から取り出した、署名検証に必要な最小情報。
struct CdSignerInfo {
    /// 署名対象バイト列（signedAttrs があればそれを SET として再タグ付けした
    /// もの、無ければ eContent そのもの）。
    signed_bytes: Vec<u8>,
    /// DER `SEQ { r INTEGER, s INTEGER }` エンコードの ECDSA 署名。
    signature: Vec<u8>,
}

/// `ContentInfo ::= SEQ { contentType OID, content [0] EXPLICIT SignedData }`、
/// `SignedData ::= SEQ { version, digestAlgorithms SET, encapContentInfo SEQ {
/// eContentType OID, eContent [0] EXPLICIT OCTET STRING }, certificates?,
/// crls?, signerInfos SET OF SignerInfo }` から `(eContent, 最初の
/// SignerInfo)` を取り出す。壊れていれば理由文字列を返す（呼び出し側で warn
/// して継続するため、ここでは `panic` しない）。
fn parse_cms_signed_data(der: &[u8]) -> Result<(Vec<u8>, Option<CdSignerInfo>), &'static str> {
    let mut top = DerReader::new(der);
    let ci = top
        .expect(0x30)
        .map_err(|_| "content info not a sequence")?;
    let mut ci_r = DerReader::new(ci);
    ci_r.expect(0x06).map_err(|_| "missing content type oid")?;
    let wrapped = ci_r
        .expect(0xA0)
        .map_err(|_| "missing [0] content wrapper")?;
    let mut wrap_r = DerReader::new(wrapped);
    let sd = wrap_r
        .expect(0x30)
        .map_err(|_| "signed data not a sequence")?;

    let mut sd_r = DerReader::new(sd);
    sd_r.read().map_err(|_| "missing version")?; // CMSVersion
    sd_r.read().map_err(|_| "missing digestAlgorithms")?; // digestAlgorithms SET

    let eci = sd_r.expect(0x30).map_err(|_| "missing encapContentInfo")?;
    let mut eci_r = DerReader::new(eci);
    eci_r
        .expect(0x06)
        .map_err(|_| "missing econtent type oid")?;
    let econtent_wrap = eci_r
        .expect(0xA0)
        .map_err(|_| "missing [0] econtent wrapper")?;
    let mut ew_r = DerReader::new(econtent_wrap);
    let econtent = ew_r
        .expect(0x04)
        .map_err(|_| "econtent not an octet string")?;
    let cd_tlv = econtent.to_vec();

    // certificates [0] IMPLICIT / crls [1] IMPLICIT（省略可、CD では通常無い）
    while matches!(sd_r.peek_tag(), Some(0xA0) | Some(0xA1)) {
        if sd_r.read().is_err() {
            return Ok((cd_tlv, None));
        }
    }

    let signer_infos = match sd_r.expect(0x31) {
        Ok(c) => c,
        Err(_) => return Ok((cd_tlv, None)),
    };
    let mut si_r = DerReader::new(signer_infos);
    let first = match si_r.expect(0x30) {
        Ok(c) => c,
        Err(_) => return Ok((cd_tlv, None)),
    };
    let signer_info = parse_signer_info(first, &cd_tlv).ok();
    Ok((cd_tlv, signer_info))
}

/// `SignerInfo ::= SEQ { version, sid SignerIdentifier, digestAlgorithm,
/// signedAttrs [0] IMPLICIT SET OF Attribute OPTIONAL, signatureAlgorithm,
/// signature OCTET STRING, unsignedAttrs [1] IMPLICIT SET OF Attribute
/// OPTIONAL }`。`signedAttrs` があれば CMS §5.4 に従い IMPLICIT タグを
/// 外して SET（0x31）として再エンコードしたものを署名対象にする。
fn parse_signer_info(content: &[u8], econtent: &[u8]) -> Result<CdSignerInfo, &'static str> {
    let mut r = DerReader::new(content);
    r.read().map_err(|_| "signerinfo missing version")?;
    r.read().map_err(|_| "signerinfo missing sid")?; // SignerIdentifier（CHOICE）
    r.read().map_err(|_| "signerinfo missing digestAlgorithm")?;

    let mut signed_bytes = econtent.to_vec();
    if r.peek_tag() == Some(0xA0) {
        let (_, _content, raw) = r.read().map_err(|_| "bad signedAttrs")?;
        if raw.is_empty() {
            return Err("empty signedAttrs");
        }
        let mut reencoded = Vec::with_capacity(raw.len());
        reencoded.push(0x31); // [0] IMPLICIT -> SET タグに戻す
        reencoded.extend_from_slice(&raw[1..]);
        signed_bytes = reencoded;
    }

    r.read()
        .map_err(|_| "signerinfo missing signatureAlgorithm")?;
    let sig_bytes = r.expect(0x04).map_err(|_| "signerinfo missing signature")?;
    Ok(CdSignerInfo {
        signed_bytes,
        signature: sig_bytes.to_vec(),
    })
}

/// DER `SEQ { r INTEGER, s INTEGER }` を raw r‖s（32B 左ゼロ詰め x2 = 64B）に
/// 正規化する（x509.rs の同名ロジックの CMS 版 — signature フィールドは
/// BIT STRING ではなく OCTET STRING の中身がそのまま DER SEQ）。
fn der_ecdsa_sig_to_raw64(der: &[u8]) -> Result<[u8; 64], &'static str> {
    let mut r = DerReader::new(der);
    let seq = r.expect(0x30).map_err(|_| "signature not a der sequence")?;
    let mut inner = DerReader::new(seq);
    let r_bytes = inner.expect(0x02).map_err(|_| "signature missing r")?;
    let s_bytes = inner.expect(0x02).map_err(|_| "signature missing s")?;
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&int_to_32(r_bytes)?);
    out[32..].copy_from_slice(&int_to_32(s_bytes)?);
    Ok(out)
}

/// DER INTEGER の中身（符号バイト付き・可変長）を 32B 左ゼロ詰め固定長にする。
fn int_to_32(b: &[u8]) -> Result<[u8; 32], &'static str> {
    let b = if b.len() > 1 && b[0] == 0 { &b[1..] } else { b };
    if b.is_empty() || b.len() > 32 {
        return Err("integer out of range");
    }
    let mut out = [0u8; 32];
    out[32 - b.len()..].copy_from_slice(b);
    Ok(out)
}

/// CD TLV `struct { 1: format_version, 2: vendor_id UINT, 3:
/// product_id_array ARRAY OF UINT, ... }` から `(vendor_id,
/// product_id_array)` を抜き出す（brief の簡略構造に合わせ、他フィールドは
/// 読み捨てる）。壊れていれば理由文字列を返す。
fn parse_cd_vid_pid(cd_tlv: &[u8]) -> Result<(Option<u16>, Vec<u16>), &'static str> {
    let mut r = Reader::new(cd_tlv);
    let first = r
        .next()
        .map_err(|_| "cd tlv parse error")?
        .ok_or("empty cd tlv")?;
    if !matches!(first.value, Value::StructStart) {
        return Err("cd tlv not a struct");
    }

    let mut vid = None;
    let mut pids = Vec::new();
    loop {
        let el = r
            .next()
            .map_err(|_| "cd tlv parse error")?
            .ok_or("truncated cd tlv")?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => vid = u16::try_from(v).ok(),
            (Tag::Context(2), Value::ArrayStart) => loop {
                let e2 = r
                    .next()
                    .map_err(|_| "cd tlv parse error")?
                    .ok_or("truncated cd tlv product_id_array")?;
                match e2.value {
                    Value::ContainerEnd => break,
                    Value::Uint(v) => {
                        if let Ok(v) = u16::try_from(v) {
                            pids.push(v);
                        }
                    }
                    _ => {}
                }
            },
            _ => {}
        }
    }
    Ok((vid, pids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::random_p256_secret;
    use crate::crypto::sign_ecdsa_p256;
    use crate::tlv::{Tag, Writer};
    use crate::x509::test_support::make_test_cert;

    struct Fixture {
        dac: Vec<u8>,
        pai: Vec<u8>,
        paa: Vec<u8>,
        dac_key: p256::SecretKey,
    }

    fn chain() -> Fixture {
        let paa_key = random_p256_secret();
        let pai_key = random_p256_secret();
        let dac_key = random_p256_secret();
        let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, true, None);
        let pai = make_test_cert(
            b"pai",
            b"paa",
            &pai_key,
            &paa_key,
            true,
            Some((0xFFF1, 0x8001)),
        );
        let dac = make_test_cert(
            b"dac",
            b"pai",
            &dac_key,
            &pai_key,
            false,
            Some((0xFFF1, 0x8001)),
        );
        Fixture {
            dac,
            pai,
            paa,
            dac_key,
        }
    }

    fn elements(nonce: &[u8; 32]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), b"fake-cd"); // CD は warn 経路なので偽物で良い
        w.put_bytes(Tag::Context(2), nonce);
        w.put_uint(Tag::Context(3), 0); // timestamp
        w.end_container();
        w.finish()
    }

    fn sign(fix: &Fixture, elements: &[u8], challenge: &[u8; 16]) -> [u8; 64] {
        let mut msg = elements.to_vec();
        msg.extend_from_slice(challenge);
        let priv_bytes: [u8; 32] = fix.dac_key.to_bytes().into();
        sign_ecdsa_p256(&priv_bytes, &msg).unwrap()
    }

    #[test]
    fn accepts_valid_attestation() {
        let fix = chain();
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let sig = sign(&fix, &el, &challenge);
        verify_device_attestation(
            &fix.dac,
            &fix.pai,
            std::slice::from_ref(&fix.paa),
            &[],
            &el,
            &sig,
            &nonce,
            &challenge,
        )
        .unwrap();
    }

    #[test]
    fn rejects_unknown_paa() {
        let fix = chain();
        let other = chain(); // 別の根
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let sig = sign(&fix, &el, &challenge);
        let err = verify_device_attestation(
            &fix.dac,
            &fix.pai,
            std::slice::from_ref(&other.paa),
            &[],
            &el,
            &sig,
            &nonce,
            &challenge,
        )
        .unwrap_err();
        assert!(matches!(err, AttestationError::Chain(_)));
    }

    #[test]
    fn rejects_wrong_nonce() {
        let fix = chain();
        let challenge = [6u8; 16];
        let el = elements(&[5u8; 32]);
        let sig = sign(&fix, &el, &challenge);
        let err = verify_device_attestation(
            &fix.dac,
            &fix.pai,
            std::slice::from_ref(&fix.paa),
            &[],
            &el,
            &sig,
            &[9u8; 32],
            &challenge,
        )
        .unwrap_err();
        assert!(matches!(err, AttestationError::Nonce));
    }

    #[test]
    fn rejects_tampered_signature() {
        let fix = chain();
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let mut sig = sign(&fix, &el, &challenge);
        sig[0] ^= 0xFF;
        let err = verify_device_attestation(
            &fix.dac,
            &fix.pai,
            std::slice::from_ref(&fix.paa),
            &[],
            &el,
            &sig,
            &nonce,
            &challenge,
        )
        .unwrap_err();
        assert!(matches!(err, AttestationError::Signature));
    }

    // --- Task 4.5: VID/PID 整合・cA・有効期間 ---

    #[test]
    fn rejects_dac_pai_vid_mismatch() {
        let paa_key = random_p256_secret();
        let pai_key = random_p256_secret();
        let dac_key = random_p256_secret();
        let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, true, None);
        // PAI は VID 0xFFF2、DAC は VID 0xFFF1 — 不一致。
        let pai = make_test_cert(
            b"pai",
            b"paa",
            &pai_key,
            &paa_key,
            true,
            Some((0xFFF2, 0x8001)),
        );
        let dac = make_test_cert(
            b"dac",
            b"pai",
            &dac_key,
            &pai_key,
            false,
            Some((0xFFF1, 0x8001)),
        );
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let priv_bytes: [u8; 32] = dac_key.to_bytes().into();
        let mut msg = el.clone();
        msg.extend_from_slice(&challenge);
        let sig = sign_ecdsa_p256(&priv_bytes, &msg).unwrap();
        let err = verify_device_attestation(
            &dac,
            &pai,
            std::slice::from_ref(&paa),
            &[],
            &el,
            &sig,
            &nonce,
            &challenge,
        )
        .unwrap_err();
        assert!(matches!(err, AttestationError::Chain(_)));
    }

    #[test]
    fn rejects_pai_without_ca_flag() {
        let paa_key = random_p256_secret();
        let fake_pai_key = random_p256_secret();
        let dac_key = random_p256_secret();
        let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, true, None);
        // "PAI" 位置に is_ca=false（basicConstraints 拡張なし = cA は
        // None、Some(true) ではない）の証明書を置く — DAC の位置にあるべき
        // 種類の証明書を PAI として渡すのと同義（brief: 「DAC を PAI の
        // 位置に渡す」ことで cA=false 拒否を確認）。
        let fake_pai = make_test_cert(
            b"pai",
            b"paa",
            &fake_pai_key,
            &paa_key,
            false,
            Some((0xFFF1, 0x8001)),
        );
        let dac = make_test_cert(
            b"dac",
            b"pai",
            &dac_key,
            &fake_pai_key,
            false,
            Some((0xFFF1, 0x8001)),
        );
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let priv_bytes: [u8; 32] = dac_key.to_bytes().into();
        let mut msg = el.clone();
        msg.extend_from_slice(&challenge);
        let sig = sign_ecdsa_p256(&priv_bytes, &msg).unwrap();
        let err = verify_device_attestation(
            &dac,
            &fake_pai,
            std::slice::from_ref(&paa),
            &[],
            &el,
            &sig,
            &nonce,
            &challenge,
        )
        .unwrap_err();
        assert!(matches!(err, AttestationError::Chain(_)));
    }

    #[test]
    fn epoch_seconds_matches_known_utc_values() {
        // openssl x509 -text で確認した Root01 fixture の Validity
        // （x509.rs の sdk_fixtures_expose_is_ca_and_validity テスト）を
        // 独立に検算した既知値（date -u -d ... +%s 相当）。
        assert_eq!(epoch_seconds(2020, 10, 15, 14, 23, 43), 1_602_771_823);
        assert_eq!(epoch_seconds(2040, 10, 15, 14, 23, 42), 2_233_923_822);
        assert_eq!(epoch_seconds(1970, 1, 1, 0, 0, 0), 0);
    }

    #[test]
    fn parse_cert_time_handles_utc_and_generalized() {
        assert_eq!(
            parse_cert_time("201015142343Z"),
            Some((2020, 10, 15, 14, 23, 43))
        );
        assert_eq!(
            parse_cert_time("20201015142343Z"),
            Some((2020, 10, 15, 14, 23, 43))
        );
        // UTCTime の慣例: YY < 50 -> 20YY、それ以外 -> 19YY。
        assert_eq!(
            parse_cert_time("990101000000Z"),
            Some((1999, 1, 1, 0, 0, 0))
        );
        assert_eq!(
            parse_cert_time(NO_EXPIRY_NOT_AFTER),
            Some((9999, 12, 31, 23, 59, 59))
        );
    }

    #[test]
    fn parse_cert_time_rejects_garbage() {
        assert_eq!(parse_cert_time("not-a-time"), None);
        assert_eq!(parse_cert_time("2020101514234"), None); // Z 無し
        assert_eq!(parse_cert_time("209915142343Z"), None); // 月 99
    }
}
