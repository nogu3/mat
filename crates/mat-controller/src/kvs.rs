//! Minimal reader for chip-tool's Linux ini KVS (connectedhomeip v1.4.2.0).
//!
//! Two readers: [`read_fabric_credentials`] (a full credential set including
//! the operational key, keys `f/<index>/{r,i,n,o}` and `f/<index>/k/0` —
//! chip-tool does not persist its *own* op key, so this path serves fixtures
//! and non-chip-tool stores), and [`read_self_issue_materials`] (what
//! self-issuing our own NOC needs: the root CA key from the alpha ini; the
//! root cert, our node/fabric id, and the IPK from the main ini fabric
//! table). Format facts (verified against SDK v1.4.2.0): `[Default]`
//! section, base64 values; the keyset stores the already derived
//! *operational* group key, not the epoch key.

use std::path::Path;

use base64ct::{Base64, Encoding};

use crate::tlv::{Element, Reader, Tag, Value};

/// Fabric credentials read from chip-tool's ini KVS, still in raw form
/// (opaque certs, unparsed keys) as CASE needs them.
#[derive(Clone)]
pub struct RawFabricCredentials {
    pub rcac: Vec<u8>,
    pub icac: Option<Vec<u8>>,
    pub noc: Vec<u8>,
    pub op_public_key: [u8; 65],
    pub op_private_key: [u8; 32],
    pub ipk_operational: [u8; 16],
}

/// Manual `Debug`: this struct carries the operational private key and the
/// fabric's identity-protection key, both secret. Never derive `Debug` here
/// again — certs/keys are logged incidentally via `{:?}` (error contexts,
/// test failure output, etc.) and this repo is public.
impl std::fmt::Debug for RawFabricCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawFabricCredentials")
            .field("rcac_len", &self.rcac.len())
            .field("icac_len", &self.icac.as_ref().map(Vec::len))
            .field("noc_len", &self.noc.len())
            .field("op_public_key_len", &self.op_public_key.len())
            .field("op_private_key", &"[REDACTED]")
            .field("ipk_operational", &"[REDACTED]")
            .finish()
    }
}

/// KVS read/parse error. `Display` names the offending key and reason so an
/// AI or operator can decide recovery without opening the ini file.
#[derive(Debug)]
pub enum KvsError {
    Io(std::io::Error),
    SectionMissing,
    KeyMissing(String),
    BadBase64(String),
    BadOpKey {
        fabric_index: u8,
        reason: &'static str,
    },
    BadKeyset {
        fabric_index: u8,
        reason: &'static str,
    },
    BadNoc {
        fabric_index: u8,
        reason: &'static str,
    },
    BadCaKey(&'static str),
}

impl std::fmt::Display for KvsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KvsError::Io(e) => write!(f, "kvs: io error: {e}"),
            KvsError::SectionMissing => write!(f, "kvs: missing [Default] section"),
            KvsError::KeyMissing(k) => write!(f, "kvs key \"{k}\": missing"),
            KvsError::BadBase64(k) => write!(f, "kvs key \"{k}\": invalid base64"),
            KvsError::BadOpKey {
                fabric_index,
                reason,
            } => {
                write!(f, "kvs key \"f/{fabric_index}/o\": bad op key: {reason}")
            }
            KvsError::BadKeyset {
                fabric_index,
                reason,
            } => {
                write!(f, "kvs key \"f/{fabric_index}/k/0\": bad keyset: {reason}")
            }
            KvsError::BadNoc {
                fabric_index,
                reason,
            } => {
                write!(f, "kvs key \"f/{fabric_index}/n\": bad noc: {reason}")
            }
            KvsError::BadCaKey(reason) => write!(f, "kvs: bad CA key: {reason}"),
        }
    }
}

impl std::error::Error for KvsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            KvsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Returns the body of the `[Default]` section (everything after the
/// `[Default]` line up to the next line starting with `[`, or end of file).
fn default_section(text: &str) -> Option<&str> {
    let mut pos = 0usize;
    let mut in_section = false;
    let mut section_start = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if in_section {
            if trimmed.starts_with('[') {
                return Some(&text[section_start..pos]);
            }
        } else if trimmed == "[Default]" {
            in_section = true;
            section_start = pos + line.len();
        }
        pos += line.len();
    }
    if in_section {
        Some(&text[section_start..])
    } else {
        None
    }
}

/// Looks up `key` in an ini section body: each line is split on the first
/// `=`, both sides trimmed.
fn lookup<'a>(section: &'a str, key: &str) -> Option<&'a str> {
    for line in section.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key {
            return Some(v.trim());
        }
    }
    None
}

/// Looks up `key` in `section` and base64-decodes it. Missing key or an
/// empty value both map to `None` (chip-tool writes empty values for some
/// absent-but-present keys); a present, non-empty, unparseable value is a
/// hard error naming the key.
fn decode_b64(section: &str, key: &str) -> Result<Option<Vec<u8>>, KvsError> {
    match lookup(section, key) {
        None => Ok(None),
        Some("") => Ok(None),
        Some(v) => Base64::decode_vec(v)
            .map(Some)
            .map_err(|_| KvsError::BadBase64(key.to_string())),
    }
}

/// Reads the next TLV element, mapping decode/EOF errors to `BadOpKey`.
fn next_opkey_el<'a>(r: &mut Reader<'a>, fabric_index: u8) -> Result<Element<'a>, KvsError> {
    r.next()
        .map_err(|_| KvsError::BadOpKey {
            fabric_index,
            reason: "malformed tlv",
        })?
        .ok_or(KvsError::BadOpKey {
            fabric_index,
            reason: "malformed tlv",
        })
}

/// Parses the chip-tool `OperationalKeypair` TLV blob (version + 97-byte
/// SEC1-uncompressed-pubkey||privkey pair) into its two halves.
fn parse_opkey(blob: &[u8], fabric_index: u8) -> Result<([u8; 65], [u8; 32]), KvsError> {
    let mut r = Reader::new(blob);

    let el = next_opkey_el(&mut r, fabric_index)?;
    if el.value != Value::StructStart {
        return Err(KvsError::BadOpKey {
            fabric_index,
            reason: "malformed tlv",
        });
    }

    let el = next_opkey_el(&mut r, fabric_index)?;
    let version = match (el.tag, el.value) {
        (Tag::Context(0), Value::Uint(v)) => v,
        _ => {
            return Err(KvsError::BadOpKey {
                fabric_index,
                reason: "malformed tlv",
            })
        }
    };
    if version != 1 {
        return Err(KvsError::BadOpKey {
            fabric_index,
            reason: "unsupported version",
        });
    }

    let el = next_opkey_el(&mut r, fabric_index)?;
    let keypair = match (el.tag, el.value) {
        (Tag::Context(1), Value::Bytes(b)) => b,
        _ => {
            return Err(KvsError::BadOpKey {
                fabric_index,
                reason: "malformed tlv",
            })
        }
    };
    if keypair.len() != 97 {
        return Err(KvsError::BadOpKey {
            fabric_index,
            reason: "keypair must be 97 bytes",
        });
    }

    let mut pubkey = [0u8; 65];
    let mut privkey = [0u8; 32];
    pubkey.copy_from_slice(&keypair[..65]);
    privkey.copy_from_slice(&keypair[65..]);
    Ok((pubkey, privkey))
}

/// Reads the next TLV element, mapping decode/EOF errors to `BadKeyset`.
fn next_keyset_el<'a>(r: &mut Reader<'a>, fabric_index: u8) -> Result<Element<'a>, KvsError> {
    r.next()
        .map_err(|_| KvsError::BadKeyset {
            fabric_index,
            reason: "malformed tlv",
        })?
        .ok_or(KvsError::BadKeyset {
            fabric_index,
            reason: "malformed tlv",
        })
}

/// Skips the remainder of the container currently open at relative depth 0
/// (i.e. reads elements, tracking nested container depth, until the
/// `ContainerEnd` that matches the container we're inside of). Used both to
/// skip over unknown/uninteresting subtrees and to finish consuming a
/// container after we've already extracted what we needed from its start.
fn skip_rest_of_container(r: &mut Reader, fabric_index: u8) -> Result<(), KvsError> {
    let mut depth: i32 = 0;
    loop {
        let el = next_keyset_el(r, fabric_index)?;
        match el.value {
            Value::StructStart | Value::ArrayStart | Value::ListStart => depth += 1,
            Value::ContainerEnd => {
                if depth == 0 {
                    return Ok(());
                }
                depth -= 1;
            }
            _ => {}
        }
    }
}

/// Parses one `GroupKey` struct (start_time / hash / key bytes) from within
/// the keyset's key array, returning the 16-byte operational key.
fn parse_key_struct(r: &mut Reader, fabric_index: u8) -> Result<[u8; 16], KvsError> {
    let el = next_keyset_el(r, fabric_index)?;
    if el.value != Value::StructStart {
        return Err(KvsError::BadKeyset {
            fabric_index,
            reason: "malformed tlv",
        });
    }

    let mut key: Option<[u8; 16]> = None;
    loop {
        let el = next_keyset_el(r, fabric_index)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(6), Value::Bytes(b)) => {
                if b.len() != 16 {
                    return Err(KvsError::BadKeyset {
                        fabric_index,
                        reason: "operational key must be 16 bytes",
                    });
                }
                let mut arr = [0u8; 16];
                arr.copy_from_slice(b);
                key = Some(arr);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_rest_of_container(r, fabric_index)?;
            }
            _ => {}
        }
    }
    key.ok_or(KvsError::BadKeyset {
        fabric_index,
        reason: "missing operational key",
    })
}

/// Parses the chip-tool `KeySet` TLV blob, returning the operational group
/// key (`ipk_operational`) of the first entry in the key array. `keys_count`
/// (Context(2)) must be at least 1; unknown tags/containers are skipped.
fn parse_keyset(blob: &[u8], fabric_index: u8) -> Result<[u8; 16], KvsError> {
    let mut r = Reader::new(blob);

    let el = next_keyset_el(&mut r, fabric_index)?;
    if el.value != Value::StructStart {
        return Err(KvsError::BadKeyset {
            fabric_index,
            reason: "malformed tlv",
        });
    }

    let mut keys_count: Option<u64> = None;
    let mut ipk: Option<[u8; 16]> = None;

    loop {
        let el = next_keyset_el(&mut r, fabric_index)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::Uint(v)) => keys_count = Some(v),
            (Tag::Context(3), Value::ArrayStart) => {
                ipk = Some(parse_key_struct(&mut r, fabric_index)?);
                skip_rest_of_container(&mut r, fabric_index)?;
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_rest_of_container(&mut r, fabric_index)?;
            }
            _ => {}
        }
    }

    let keys_count = keys_count.ok_or(KvsError::BadKeyset {
        fabric_index,
        reason: "missing keys_count",
    })?;
    if keys_count < 1 {
        return Err(KvsError::BadKeyset {
            fabric_index,
            reason: "keys_count must be >= 1",
        });
    }
    ipk.ok_or(KvsError::BadKeyset {
        fabric_index,
        reason: "missing key entries",
    })
}

/// Reads the five fabric credentials chip-tool's CASE implementation needs
/// (RCAC, optional ICAC, NOC, operational keypair, operational group key)
/// out of its Linux ini KVS file, for the given `fabric_index`.
pub fn read_fabric_credentials(
    path: &Path,
    fabric_index: u8,
) -> Result<RawFabricCredentials, KvsError> {
    let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
    let section = default_section(&text).ok_or(KvsError::SectionMissing)?;
    let get = |key: String| -> Result<Option<Vec<u8>>, KvsError> { decode_b64(section, &key) };
    let must = |key: String| -> Result<Vec<u8>, KvsError> {
        get(key.clone())?.ok_or(KvsError::KeyMissing(key))
    };

    let rcac = must(format!("f/{fabric_index}/r"))?;
    let icac = get(format!("f/{fabric_index}/i"))?;
    let noc = must(format!("f/{fabric_index}/n"))?;
    let (op_public_key, op_private_key) =
        parse_opkey(&must(format!("f/{fabric_index}/o"))?, fabric_index)?;
    let ipk_operational = parse_keyset(&must(format!("f/{fabric_index}/k/0"))?, fabric_index)?;

    Ok(RawFabricCredentials {
        rcac,
        icac,
        noc,
        op_public_key,
        op_private_key,
        ipk_operational,
    })
}

/// CA materials chip-tool persists, needed to self-issue a NOC without going
/// through chip-tool. `root_private_key` comes from the *alpha* KVS (the CA's
/// own key pair); `rcac` (root cert, Matter-TLV form — its parsed public key is
/// the root public key), `ipk_operational`, and `node_id`/`fabric_id` (both
/// from the subject of chip-tool's own operational NOC at `f/<idx>/n` — the
/// identity the device ACLs actually admit; the KVS index is just a table
/// slot) come from the *main* KVS.
#[derive(Clone)]
pub struct SelfIssueMaterials {
    pub rcac: Vec<u8>,
    pub root_private_key: [u8; 32],
    pub ipk_operational: [u8; 16],
    pub node_id: u64,
    pub fabric_id: u64,
}

/// Manual `Debug`: carries the root CA's private key and the fabric's
/// identity-protection key, both secret. See `RawFabricCredentials`'s `Debug`
/// impl for the same rationale.
impl std::fmt::Debug for SelfIssueMaterials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelfIssueMaterials")
            .field("rcac_len", &self.rcac.len())
            .field("root_private_key", &"[REDACTED]")
            .field("ipk_operational", &"[REDACTED]")
            .field("node_id", &self.node_id)
            .field("fabric_id", &self.fabric_id)
            .finish()
    }
}

/// Reads the CA materials chip-tool persists (root cert + root CA key, from
/// `alpha_ini`) plus the IPK and the node/fabric id (from the fabric table's
/// own NOC) needed to self-issue a NOC for `fabric_index`, using CA `issuer_index`.
pub fn read_self_issue_materials(
    alpha_ini: &Path,
    main_ini: &Path,
    fabric_index: u8,
    issuer_index: u8,
) -> Result<SelfIssueMaterials, KvsError> {
    // --- alpha ini: root CA key pair ---
    let alpha_text = std::fs::read_to_string(alpha_ini).map_err(KvsError::Io)?;
    let alpha_sec = default_section(&alpha_text).ok_or(KvsError::SectionMissing)?;
    let cakey_key = format!("ExampleOpCredsCAKey{issuer_index}");
    let ca_key = decode_b64(alpha_sec, &cakey_key)?.ok_or(KvsError::KeyMissing(cakey_key))?;
    if ca_key.len() != 97 {
        return Err(KvsError::BadCaKey(
            "root ca key must be 97 raw bytes (pub65||priv32)",
        ));
    }
    // Only the private half is needed; the root public key is taken from the
    // parsed RCAC (single source of truth for `case_destination_id`).
    let root_private_key: [u8; 32] = ca_key[65..].try_into().expect("32");

    // --- main ini: root cert (TLV), IPK, node id ---
    let main_text = std::fs::read_to_string(main_ini).map_err(KvsError::Io)?;
    let main_sec = default_section(&main_text).ok_or(KvsError::SectionMissing)?;
    // The root cert in operational Matter-TLV form lives in the *main* KVS
    // fabric table (`f/<idx>/r`). alpha's `ExampleCARootCert<issuer>` is stored
    // as X.509 DER, which our Matter-TLV cert parser does not accept — read the
    // TLV form here instead. Both encode the same root key (verified: the
    // 65-byte pubkey from `ExampleOpCredsCAKey<issuer>` appears in `f/<idx>/r`).
    let rcac_key = format!("f/{fabric_index}/r");
    let rcac = decode_b64(main_sec, &rcac_key)?.ok_or(KvsError::KeyMissing(rcac_key))?;
    let ipk_operational = parse_keyset(
        &decode_b64(main_sec, &format!("f/{fabric_index}/k/0"))?
            .ok_or_else(|| KvsError::KeyMissing(format!("f/{fabric_index}/k/0")))?,
        fabric_index,
    )?;

    // node id / fabric id come from the subject of chip-tool's own
    // operational NOC in the fabric table (`f/<idx>/n`, Matter-TLV): the
    // device ACLs admit exactly the identity in that cert, and its subject
    // carries the *operational* fabric id — the KVS index is just a table
    // slot and differs from the fabric id on any non-alpha fabric.
    let noc_key = format!("f/{fabric_index}/n");
    let noc_tlv = decode_b64(main_sec, &noc_key)?.ok_or(KvsError::KeyMissing(noc_key))?;
    let noc = crate::cert::MatterCert::parse(&noc_tlv).map_err(|_| KvsError::BadNoc {
        fabric_index,
        reason: "unparseable matter-tlv certificate",
    })?;
    let node_id = noc.node_id().ok_or(KvsError::BadNoc {
        fabric_index,
        reason: "subject missing node id (tag 17)",
    })?;
    let fabric_id = noc.fabric_id().ok_or(KvsError::BadNoc {
        fabric_index,
        reason: "subject missing fabric id (tag 21)",
    })?;

    Ok(SelfIssueMaterials {
        rcac,
        root_private_key,
        ipk_operational,
        node_id,
        fabric_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Tag, Writer};
    use base64ct::{Base64, Encoding};

    fn opkey_blob(pubkey: &[u8; 65], privkey: &[u8; 32]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 1); // version
        let mut kp = Vec::with_capacity(97);
        kp.extend_from_slice(pubkey);
        kp.extend_from_slice(privkey);
        w.put_bytes(Tag::Context(1), &kp);
        w.end_container();
        w.finish()
    }

    fn keyset_blob(key: &[u8; 16]) -> Vec<u8> {
        keyset_blob_with_count(key, 1)
    }

    fn keyset_blob_with_count(key: &[u8; 16], keys_count: u64) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0); // policy
        w.put_uint(Tag::Context(2), keys_count);
        w.start_array(Tag::Context(3));
        for i in 0..3u8 {
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), 0); // start_time
            w.put_uint(Tag::Context(5), 0x1234); // hash
            w.put_bytes(Tag::Context(6), if i == 0 { key } else { &[0u8; 16] });
            w.end_container();
        }
        w.end_container();
        w.put_uint(Tag::Context(7), 0xFFFF); // next keyset id (リンクリスト、無視される)
        w.end_container();
        w.finish()
    }

    fn write_ini(entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut body = String::from("[Default]\n");
        for (k, v) in entries {
            body.push_str(&format!("{} = {}\n", k, Base64::encode_string(v)));
        }
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mat-kvs-test-{}-{}-{}.ini",
            std::process::id(),
            entries.len(),
            seq
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Like `write_ini`, but with a caller-chosen filename tag instead of a
    /// counter, so two related fixtures (e.g. alpha + main ini) in the same
    /// test are easy to tell apart in a failure message.
    fn write_named_ini(tag: &str, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let mut body = String::from("[Default]\n");
        for (k, v) in entries {
            body.push_str(&format!("{} = {}\n", k, Base64::encode_string(v)));
        }
        let path =
            std::env::temp_dir().join(format!("mat-kvs-test-{}-{tag}.ini", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    const PUB: [u8; 65] = [0xAA; 65];
    const PRIV: [u8; 32] = [0xBB; 32];
    const IPK: [u8; 16] = [0xCC; 16];

    /// node01_01 フィクスチャ（chip SDK テスト証明書）とその subject の実 id。
    /// 期待値はパーサ経由で取るが、cert パース自体は cert.rs 側でフィクスチャ
    /// 検証済みなので、ここでは「kvs がその値を配線しているか」だけを見る。
    fn noc_fixture() -> (&'static [u8], u64, u64) {
        let bytes: &[u8] = include_bytes!("../tests/fixtures/node01_01_chip.bin");
        let cert = crate::cert::MatterCert::parse(bytes).unwrap();
        (bytes, cert.node_id().unwrap(), cert.fabric_id().unwrap())
    }

    #[test]
    fn reads_all_five_items() {
        let op = opkey_blob(&PUB, &PRIV);
        let ks = keyset_blob(&IPK);
        let path = write_ini(&[
            ("f/1/r", b"rcac-bytes"),
            ("f/1/i", b"icac-bytes"),
            ("f/1/n", b"noc-bytes"),
            ("f/1/o", &op),
            ("f/1/k/0", &ks),
        ]);
        let c = read_fabric_credentials(&path, 1).unwrap();
        assert_eq!(c.rcac, b"rcac-bytes");
        assert_eq!(c.icac.as_deref(), Some(b"icac-bytes".as_slice()));
        assert_eq!(c.noc, b"noc-bytes");
        assert_eq!(c.op_public_key, PUB);
        assert_eq!(c.op_private_key, PRIV);
        assert_eq!(c.ipk_operational, IPK);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn lookup_skips_lines_without_equals_sign() {
        let op = opkey_blob(&PUB, &PRIV);
        let ks = keyset_blob(&IPK);
        let path = write_ini(&[
            ("f/1/r", b"rcac-bytes"),
            ("f/1/n", b"noc-bytes"),
            ("f/1/o", &op),
            ("f/1/k/0", &ks),
        ]);
        // Inject a blank line and a comment-ish line without '=' between the
        // [Default] header and the target keys, simulating real chip-tool
        // ini quirks that must not abort the section scan.
        let text = std::fs::read_to_string(&path).unwrap();
        let text = text.replacen(
            "[Default]\n",
            "[Default]\n\n; a comment without an equals sign\n",
            1,
        );
        std::fs::write(&path, text).unwrap();

        let c = read_fabric_credentials(&path, 1).unwrap();
        assert_eq!(c.rcac, b"rcac-bytes");
        assert_eq!(c.noc, b"noc-bytes");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn missing_icac_is_none_but_missing_noc_is_error() {
        let op = opkey_blob(&PUB, &PRIV);
        let ks = keyset_blob(&IPK);
        let path = write_ini(&[
            ("f/1/r", b"rcac-bytes"),
            ("f/1/n", b"noc-bytes"),
            ("f/1/o", &op),
            ("f/1/k/0", &ks),
        ]);
        let c = read_fabric_credentials(&path, 1).unwrap();
        assert_eq!(c.icac, None);
        std::fs::remove_file(&path).ok();

        let path = write_ini(&[("f/1/r", b"rcac-bytes")]);
        let err = read_fabric_credentials(&path, 1).unwrap_err();
        assert!(matches!(err, KvsError::KeyMissing(k) if k == "f/1/n"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_bad_opkey_version_and_bad_base64() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 2); // 未知バージョン
        w.put_bytes(Tag::Context(1), &[0u8; 97]);
        w.end_container();
        let bad_op = w.finish();
        let ks = keyset_blob(&IPK);
        let path = write_ini(&[
            ("f/1/r", b"r"),
            ("f/1/n", b"n"),
            ("f/1/o", &bad_op),
            ("f/1/k/0", &ks),
        ]);
        assert!(matches!(
            read_fabric_credentials(&path, 1).unwrap_err(),
            KvsError::BadOpKey {
                fabric_index: 1,
                ..
            }
        ));
        std::fs::remove_file(&path).ok();

        let path = std::env::temp_dir().join(format!("mat-kvs-badb64-{}.ini", std::process::id()));
        std::fs::write(&path, "[Default]\nf/1/r = !!notbase64!!\n").unwrap();
        assert!(matches!(
            read_fabric_credentials(&path, 1).unwrap_err(),
            KvsError::BadBase64(_)
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_keyset_with_zero_keys_count() {
        let op = opkey_blob(&PUB, &PRIV);
        let ks = keyset_blob_with_count(&IPK, 0);
        let path = write_ini(&[
            ("f/1/r", b"r"),
            ("f/1/n", b"n"),
            ("f/1/o", &op),
            ("f/1/k/0", &ks),
        ]);
        let err = read_fabric_credentials(&path, 1).unwrap_err();
        assert!(matches!(
            err,
            KvsError::BadKeyset {
                fabric_index: 1,
                reason: "keys_count must be >= 1"
            }
        ));
        assert!(
            err.to_string().contains("f/1/k/0"),
            "error message should name the failing key: {err}"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn reads_self_issue_materials() {
        // root 鍵は生 97B（TLV ラップ無し）
        let mut root_key = Vec::with_capacity(97);
        root_key.extend_from_slice(&[0xAA; 65]); // pub
        root_key.extend_from_slice(&[0xBB; 32]); // priv
        let ks = keyset_blob(&[0xCC; 16]);
        let (noc, node_id, fabric_id) = noc_fixture();

        let alpha = write_named_ini("alpha", &[("ExampleOpCredsCAKey0", &root_key)]);
        let main = write_named_ini(
            "main",
            &[
                // root cert (TLV form) と自 NOC は fabric table に入っている
                ("f/1/r", b"rcac-tlv-bytes"),
                ("f/1/n", noc),
                ("f/1/k/0", &ks),
            ],
        );

        let m = read_self_issue_materials(&alpha, &main, 1, 0).unwrap();
        assert_eq!(m.rcac, b"rcac-tlv-bytes");
        assert_eq!(m.root_private_key, [0xBB; 32]);
        assert_eq!(m.ipk_operational, [0xCC; 16]);
        assert_eq!(m.node_id, node_id);
        assert_eq!(m.fabric_id, fabric_id);
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }

    #[test]
    fn ids_come_from_noc_subject_not_table_index() {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let ks = keyset_blob(&[0xCC; 16]);
        let (noc, node_id, fabric_id) = noc_fixture();
        // fabric テーブルの index 9 に置く — subject の id は 9 ではない
        let alpha = write_named_ini("alpha-idx", &[("ExampleOpCredsCAKey0", &root_key)]);
        let main = write_named_ini(
            "main-idx",
            &[("f/9/r", b"r"), ("f/9/n", noc), ("f/9/k/0", &ks)],
        );
        let m = read_self_issue_materials(&alpha, &main, 9, 0).unwrap();
        assert_ne!(
            fabric_id, 9,
            "fixture の fabric id が index と偶然一致すると本テストは無意味"
        );
        assert_eq!(m.fabric_id, fabric_id);
        assert_eq!(m.node_id, node_id);
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }

    #[test]
    fn missing_noc_is_key_missing() {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let ks = keyset_blob(&[0xCC; 16]);
        let alpha = write_named_ini("alpha-non", &[("ExampleOpCredsCAKey0", &root_key)]);
        let main = write_named_ini("main-non", &[("f/1/r", b"r"), ("f/1/k/0", &ks)]);
        let err = read_self_issue_materials(&alpha, &main, 1, 0).unwrap_err();
        assert!(matches!(err, KvsError::KeyMissing(k) if k == "f/1/n"));
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }

    #[test]
    fn garbage_noc_is_bad_noc_naming_the_key() {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let ks = keyset_blob(&[0xCC; 16]);
        let alpha = write_named_ini("alpha-bad", &[("ExampleOpCredsCAKey0", &root_key)]);
        let main = write_named_ini(
            "main-bad",
            &[
                ("f/1/r", b"r"),
                ("f/1/n", b"not a matter cert"),
                ("f/1/k/0", &ks),
            ],
        );
        let err = read_self_issue_materials(&alpha, &main, 1, 0).unwrap_err();
        assert!(matches!(
            err,
            KvsError::BadNoc {
                fabric_index: 1,
                ..
            }
        ));
        assert!(
            err.to_string().contains("f/1/n"),
            "エラーは実キー名を名指しすること: {err}"
        );
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }
}
