//! Device Attestation の失効照合（revocation set）。
//!
//! 入力は connectedhomeip の `credentials/generate_revocation_set.py` が出す
//! JSON（DCL の revocation distribution point と各 CRL を取得し、CRL 署名者の
//! 検証まで済ませて「発行者 → 失効シリアル一覧」に畳んだもの）。mat は CRL の
//! 解析も DCL へのアクセスもしない — 渡されたファイルと DAC / PAI を突き合わせる
//! だけ（ネットワーク・状態なし）。
//!
//! ```json
//! [{ "type": "revocation_set",
//!    "issuer_subject_key_id": "<hex>",     // 失効させた CA の SKID
//!    "issuer_name": "<base64 DER Name>",   // その CA の subject
//!    "revoked_serial_numbers": ["<hex>", ...],
//!    "crl_signer_cert": "...", "crl_signer_delegator": null }]
//! ```
//!
//! `crl_signer_*` は生成スクリプトが検証に使った材料の写しで、照合には使わない。

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use base64ct::{Base64, Encoding};

use crate::x509::X509Cert;

/// これより古い（mtime 基準）ファイルは呼び出し側が warn する目安。JSON 自体に
/// 生成日時が無いので mtime で代用する。古くても照合は行う（止めない）。
pub const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// revocation set の読み込み失敗。指定されたファイルが使えないときは照合なしへ
/// 黙って落とさず、呼び出し側がハードエラーにする。
#[derive(Debug)]
pub enum RevocationError {
    Io(std::io::Error),
    /// JSON として不正、またはトップレベルが配列でない。
    Json(String),
    /// `index` 番目の要素の形が期待と違う。
    Entry {
        index: usize,
        detail: &'static str,
    },
}

impl std::fmt::Display for RevocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RevocationError::Io(e) => write!(f, "io error: {e}"),
            RevocationError::Json(e) => write!(f, "invalid json: {e}"),
            RevocationError::Entry { index, detail } => write!(f, "entry {index}: {detail}"),
        }
    }
}

impl std::error::Error for RevocationError {}

struct Entry {
    issuer_skid: Vec<u8>,
    /// 発行 CA の subject Name（生 DER、タグ込み）。
    issuer_name: Vec<u8>,
    /// 先頭ゼロを落として正規化したシリアル。
    serials: HashSet<Vec<u8>>,
}

/// 解析済みの revocation set。
pub struct RevocationSet {
    entries: Vec<Entry>,
}

impl RevocationSet {
    /// JSON バイト列から組み立てる。`type` が `revocation_set` でない要素は
    /// 将来の拡張として warn して読み飛ばす。それ以外の形の崩れは `Err`。
    pub fn from_json(bytes: &[u8]) -> Result<Self, RevocationError> {
        let root: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| RevocationError::Json(e.to_string()))?;
        let items = root
            .as_array()
            .ok_or_else(|| RevocationError::Json("top level is not an array".to_string()))?;
        let mut entries = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let bad = |detail| RevocationError::Entry { index, detail };
            let obj = item.as_object().ok_or(bad("not an object"))?;
            let kind = obj.get("type").and_then(|v| v.as_str());
            if kind != Some("revocation_set") {
                tracing::warn!(index, kind, "revocation set: unknown entry type — skipped");
                continue;
            }
            let skid = obj
                .get("issuer_subject_key_id")
                .and_then(|v| v.as_str())
                .ok_or(bad("issuer_subject_key_id missing"))?;
            let issuer_skid = decode_hex(skid).ok_or(bad("issuer_subject_key_id is not hex"))?;
            let name = obj
                .get("issuer_name")
                .and_then(|v| v.as_str())
                .ok_or(bad("issuer_name missing"))?;
            let issuer_name =
                Base64::decode_vec(name).map_err(|_| bad("issuer_name is not base64"))?;
            if issuer_name.is_empty() {
                return Err(bad("issuer_name is empty"));
            }
            let list = obj
                .get("revoked_serial_numbers")
                .and_then(|v| v.as_array())
                .ok_or(bad("revoked_serial_numbers is not an array"))?;
            let mut serials = HashSet::with_capacity(list.len());
            for serial in list {
                let raw = serial
                    .as_str()
                    .and_then(decode_hex)
                    .ok_or(bad("revoked serial is not hex"))?;
                serials.insert(normalize_serial(&raw).to_vec());
            }
            entries.push(Entry {
                issuer_skid,
                issuer_name,
                serials,
            });
        }
        Ok(Self { entries })
    }

    /// ファイルから読む。
    pub fn load(path: &Path) -> Result<Self, RevocationError> {
        let bytes = std::fs::read(path).map_err(RevocationError::Io)?;
        Self::from_json(&bytes)
    }

    /// 発行者エントリの数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `cert` が失効一覧に載っているか。発行者 Name のバイト一致 + シリアル一致で
    /// 判定し、証明書に AKID があればエントリの SKID とも一致を要求する（同名で
    /// 鍵違いの CA を取り違えない）。AKID の無い証明書は Name だけで照合する
    /// （拡張を落とした証明書が照合をすり抜けないよう、厳しい側に倒す）。
    pub fn is_revoked(&self, cert: &X509Cert) -> bool {
        let serial = normalize_serial(&cert.serial);
        self.entries.iter().any(|e| {
            e.issuer_name == cert.issuer
                && cert.akid.as_ref().is_none_or(|akid| *akid == e.issuer_skid)
                && e.serials.contains(serial)
        })
    }
}

/// 先頭の 0x00 を落とす（DER の符号用パディングと、hex の桁合わせの差を吸収）。
fn normalize_serial(serial: &[u8]) -> &[u8] {
    let zeros = serial.iter().take_while(|b| **b == 0).count();
    &serial[zeros..]
}

/// 区切りなしの hex（大文字小文字不問、DCL 表記の `:` 区切りも許す）。空は `None`。
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = s.bytes().filter(|b| *b != b':').collect();
    if digits.is_empty() || digits.len() & 1 == 1 {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

/// `path` の最終更新からの経過時間（取れなければ `None`）。
pub fn file_age(path: &Path) -> Option<Duration> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x509::{generate_dev_attestation, parse_x509};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// `cert` を失効させる 1 エントリの JSON 値（生成スクリプトと同じ形）。
    fn entry_for(cert: &X509Cert, serials: &[String]) -> serde_json::Value {
        serde_json::json!({
            "type": "revocation_set",
            "issuer_subject_key_id": hex(cert.akid.as_deref().expect("akid")),
            "issuer_name": Base64::encode_string(&cert.issuer),
            "revoked_serial_numbers": serials,
            "crl_signer_cert": "AAAA",
            "crl_signer_delegator": null,
        })
    }

    fn set_of(entries: Vec<serde_json::Value>) -> RevocationSet {
        RevocationSet::from_json(serde_json::Value::Array(entries).to_string().as_bytes()).unwrap()
    }

    fn chain() -> (X509Cert, X509Cert) {
        let da = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        (
            parse_x509(&da.dac_der).unwrap(),
            parse_x509(&da.pai_der).unwrap(),
        )
    }

    #[test]
    fn parse_x509_exposes_the_serial() {
        let (dac, pai) = chain();
        assert!(!dac.serial.is_empty());
        assert!(!pai.serial.is_empty());
    }

    #[test]
    fn revoked_serial_under_matching_issuer_is_revoked() {
        let (dac, pai) = chain();
        let set = set_of(vec![entry_for(&dac, &[hex(&dac.serial)])]);
        assert_eq!(set.len(), 1);
        assert!(set.is_revoked(&dac));
        // PAI は別の発行者（PAA）の下なので、同じ set では失効にならない。
        assert!(!set.is_revoked(&pai));
    }

    #[test]
    fn other_serial_or_other_issuer_is_not_revoked() {
        let (dac, _) = chain();
        let other_serial = set_of(vec![entry_for(&dac, &["00DEADBEEF".to_string()])]);
        assert!(!other_serial.is_revoked(&dac));

        // 同じシリアルでも別チェーン（別の PAI）の発行分は無関係。
        let (other_dac, _) = chain();
        let other_issuer = set_of(vec![entry_for(&other_dac, &[hex(&dac.serial)])]);
        assert!(!other_issuer.is_revoked(&dac));
    }

    #[test]
    fn same_issuer_name_with_different_key_id_is_not_revoked() {
        let (dac, _) = chain();
        let mut e = entry_for(&dac, &[hex(&dac.serial)]);
        e["issuer_subject_key_id"] = serde_json::json!("00".repeat(20));
        assert!(!set_of(vec![e]).is_revoked(&dac));
    }

    #[test]
    fn serial_comparison_ignores_leading_zero_bytes_and_hex_case() {
        let (mut dac, _) = chain();
        // DER の正の INTEGER は最上位ビットが立つと 0x00 が前置される。
        dac.serial = vec![0x00, 0x8A, 0xBC];
        let set = set_of(vec![entry_for(&dac, &["8abc".to_string()])]);
        assert!(set.is_revoked(&dac));
        let padded = set_of(vec![entry_for(&dac, &["00008ABC".to_string()])]);
        assert!(padded.is_revoked(&dac));
    }

    #[test]
    fn cert_without_akid_is_matched_by_issuer_name_alone() {
        let (dac, _) = chain();
        let set = set_of(vec![entry_for(&dac, &[hex(&dac.serial)])]);
        let mut stripped = dac.clone();
        stripped.akid = None;
        assert!(set.is_revoked(&stripped));
    }

    #[test]
    fn empty_array_is_a_valid_empty_set() {
        let set = RevocationSet::from_json(b"[]").unwrap();
        assert!(set.is_empty());
        let (dac, _) = chain();
        assert!(!set.is_revoked(&dac));
    }

    #[test]
    fn unknown_entry_type_is_skipped() {
        let (dac, _) = chain();
        let set = set_of(vec![
            serde_json::json!({ "type": "something_new", "x": 1 }),
            entry_for(&dac, &[hex(&dac.serial)]),
        ]);
        assert_eq!(set.len(), 1);
        assert!(set.is_revoked(&dac));
    }

    #[test]
    fn malformed_input_is_an_error_never_an_empty_set() {
        assert!(matches!(
            RevocationSet::from_json(b"not json"),
            Err(RevocationError::Json(_))
        ));
        assert!(matches!(
            RevocationSet::from_json(b"{}"),
            Err(RevocationError::Json(_))
        ));
        let (dac, _) = chain();
        let good = entry_for(&dac, &[hex(&dac.serial)]);
        for (field, bad) in [
            ("issuer_subject_key_id", serde_json::json!("zz")),
            ("issuer_subject_key_id", serde_json::json!("")),
            ("issuer_name", serde_json::json!("***")),
            ("issuer_name", serde_json::Value::Null),
            ("revoked_serial_numbers", serde_json::json!("0102")),
            ("revoked_serial_numbers", serde_json::json!(["xyz"])),
            ("revoked_serial_numbers", serde_json::json!([""])),
        ] {
            let mut e = good.clone();
            e[field] = bad.clone();
            let json = serde_json::Value::Array(vec![good.clone(), e]).to_string();
            assert!(
                matches!(
                    RevocationSet::from_json(json.as_bytes()),
                    Err(RevocationError::Entry { index: 1, .. })
                ),
                "{field} = {bad} should be rejected"
            );
        }
    }

    #[test]
    fn load_reads_a_file_and_reports_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocation-set.json");
        std::fs::write(&path, b"[]").unwrap();
        assert!(RevocationSet::load(&path).unwrap().is_empty());
        assert!(file_age(&path).unwrap() < STALE_AFTER);
        assert!(matches!(
            RevocationSet::load(&dir.path().join("nope.json")),
            Err(RevocationError::Io(_))
        ));
        assert!(file_age(&dir.path().join("nope.json")).is_none());
    }
}
