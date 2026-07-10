//! Minimal reader for chip-tool's Linux ini KVS (connectedhomeip v1.4.2.0).
//!
//! Reads the five fabric credentials CASE needs. Format facts (verified
//! against SDK v1.4.2.0): `[Default]` section, base64 values, keys
//! `f/<index>/{r,i,n,o}` and `f/<index>/k/0`; the keyset stores the already
//! derived *operational* group key, not the epoch key.

use std::path::Path;

use base64ct::{Base64, Encoding};

use crate::tlv::{Element, Reader, Tag, Value};

/// Fabric credentials read from chip-tool's ini KVS, still in raw form
/// (opaque certs, unparsed keys) as CASE needs them.
#[derive(Debug, Clone)]
pub struct RawFabricCredentials {
    pub rcac: Vec<u8>,
    pub icac: Option<Vec<u8>>,
    pub noc: Vec<u8>,
    pub op_public_key: [u8; 65],
    pub op_private_key: [u8; 32],
    pub ipk_operational: [u8; 16],
}

/// KVS read/parse error. `Display` names the offending key and reason so an
/// AI or operator can decide recovery without opening the ini file.
#[derive(Debug)]
pub enum KvsError {
    Io(std::io::Error),
    SectionMissing,
    KeyMissing(String),
    BadBase64(String),
    BadOpKey(&'static str),
    BadKeyset(&'static str),
}

impl std::fmt::Display for KvsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KvsError::Io(e) => write!(f, "kvs: io error: {e}"),
            KvsError::SectionMissing => write!(f, "kvs: missing [Default] section"),
            KvsError::KeyMissing(k) => write!(f, "kvs key \"{k}\": missing"),
            KvsError::BadBase64(k) => write!(f, "kvs key \"{k}\": invalid base64"),
            KvsError::BadOpKey(reason) => {
                write!(f, "kvs key \"f/<n>/o\": bad op key: {reason}")
            }
            KvsError::BadKeyset(reason) => {
                write!(f, "kvs key \"f/<n>/k/0\": bad keyset: {reason}")
            }
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
        let (k, v) = line.split_once('=')?;
        if k.trim() == key {
            return Some(v.trim());
        }
    }
    None
}

/// Reads the next TLV element, mapping decode/EOF errors to `BadOpKey`.
fn next_opkey_el<'a>(r: &mut Reader<'a>) -> Result<Element<'a>, KvsError> {
    r.next()
        .map_err(|_| KvsError::BadOpKey("malformed tlv"))?
        .ok_or(KvsError::BadOpKey("malformed tlv"))
}

/// Parses the chip-tool `OperationalKeypair` TLV blob (version + 97-byte
/// SEC1-uncompressed-pubkey||privkey pair) into its two halves.
fn parse_opkey(blob: &[u8]) -> Result<([u8; 65], [u8; 32]), KvsError> {
    let mut r = Reader::new(blob);

    let el = next_opkey_el(&mut r)?;
    if el.value != Value::StructStart {
        return Err(KvsError::BadOpKey("malformed tlv"));
    }

    let el = next_opkey_el(&mut r)?;
    let version = match (el.tag, el.value) {
        (Tag::Context(0), Value::Uint(v)) => v,
        _ => return Err(KvsError::BadOpKey("malformed tlv")),
    };
    if version != 1 {
        return Err(KvsError::BadOpKey("unsupported version"));
    }

    let el = next_opkey_el(&mut r)?;
    let keypair = match (el.tag, el.value) {
        (Tag::Context(1), Value::Bytes(b)) => b,
        _ => return Err(KvsError::BadOpKey("malformed tlv")),
    };
    if keypair.len() != 97 {
        return Err(KvsError::BadOpKey("keypair must be 97 bytes"));
    }

    let mut pubkey = [0u8; 65];
    let mut privkey = [0u8; 32];
    pubkey.copy_from_slice(&keypair[..65]);
    privkey.copy_from_slice(&keypair[65..]);
    Ok((pubkey, privkey))
}

/// Reads the next TLV element, mapping decode/EOF errors to `BadKeyset`.
fn next_keyset_el<'a>(r: &mut Reader<'a>) -> Result<Element<'a>, KvsError> {
    r.next()
        .map_err(|_| KvsError::BadKeyset("malformed tlv"))?
        .ok_or(KvsError::BadKeyset("malformed tlv"))
}

/// Skips the remainder of the container currently open at relative depth 0
/// (i.e. reads elements, tracking nested container depth, until the
/// `ContainerEnd` that matches the container we're inside of). Used both to
/// skip over unknown/uninteresting subtrees and to finish consuming a
/// container after we've already extracted what we needed from its start.
fn skip_rest_of_container(r: &mut Reader) -> Result<(), KvsError> {
    let mut depth: i32 = 0;
    loop {
        let el = next_keyset_el(r)?;
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
fn parse_key_struct(r: &mut Reader) -> Result<[u8; 16], KvsError> {
    let el = next_keyset_el(r)?;
    if el.value != Value::StructStart {
        return Err(KvsError::BadKeyset("malformed tlv"));
    }

    let mut key: Option<[u8; 16]> = None;
    loop {
        let el = next_keyset_el(r)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(6), Value::Bytes(b)) => {
                if b.len() != 16 {
                    return Err(KvsError::BadKeyset("operational key must be 16 bytes"));
                }
                let mut arr = [0u8; 16];
                arr.copy_from_slice(b);
                key = Some(arr);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_rest_of_container(r)?;
            }
            _ => {}
        }
    }
    key.ok_or(KvsError::BadKeyset("missing operational key"))
}

/// Parses the chip-tool `KeySet` TLV blob, returning the operational group
/// key (`ipk_operational`) of the first entry in the key array. `keys_count`
/// (Context(2)) must be at least 1; unknown tags/containers are skipped.
fn parse_keyset(blob: &[u8]) -> Result<[u8; 16], KvsError> {
    let mut r = Reader::new(blob);

    let el = next_keyset_el(&mut r)?;
    if el.value != Value::StructStart {
        return Err(KvsError::BadKeyset("malformed tlv"));
    }

    let mut keys_count: Option<u64> = None;
    let mut ipk: Option<[u8; 16]> = None;

    loop {
        let el = next_keyset_el(&mut r)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::Uint(v)) => keys_count = Some(v),
            (Tag::Context(3), Value::ArrayStart) => {
                ipk = Some(parse_key_struct(&mut r)?);
                skip_rest_of_container(&mut r)?;
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_rest_of_container(&mut r)?;
            }
            _ => {}
        }
    }

    let keys_count = keys_count.ok_or(KvsError::BadKeyset("missing keys_count"))?;
    if keys_count < 1 {
        return Err(KvsError::BadKeyset("keys_count must be >= 1"));
    }
    ipk.ok_or(KvsError::BadKeyset("missing key entries"))
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
    let get = |key: String| -> Result<Option<Vec<u8>>, KvsError> {
        match lookup(section, &key) {
            None => Ok(None),
            Some("") => Ok(None),
            Some(v) => Base64::decode_vec(v)
                .map(Some)
                .map_err(|_| KvsError::BadBase64(key)),
        }
    };
    let must = |key: String| -> Result<Vec<u8>, KvsError> {
        get(key.clone())?.ok_or(KvsError::KeyMissing(key))
    };

    let rcac = must(format!("f/{fabric_index}/r"))?;
    let icac = get(format!("f/{fabric_index}/i"))?;
    let noc = must(format!("f/{fabric_index}/n"))?;
    let (op_public_key, op_private_key) = parse_opkey(&must(format!("f/{fabric_index}/o"))?)?;
    let ipk_operational = parse_keyset(&must(format!("f/{fabric_index}/k/0"))?)?;

    Ok(RawFabricCredentials {
        rcac,
        icac,
        noc,
        op_public_key,
        op_private_key,
        ipk_operational,
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
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0); // policy
        w.put_uint(Tag::Context(2), 1); // keys_count
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
        let mut body = String::from("[Default]\n");
        for (k, v) in entries {
            body.push_str(&format!("{} = {}\n", k, Base64::encode_string(v)));
        }
        let path = std::env::temp_dir().join(format!(
            "mat-kvs-test-{}-{}.ini",
            std::process::id(),
            entries.len()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    const PUB: [u8; 65] = [0xAA; 65];
    const PRIV: [u8; 32] = [0xBB; 32];
    const IPK: [u8; 16] = [0xCC; 16];

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
            KvsError::BadOpKey(_)
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
}
