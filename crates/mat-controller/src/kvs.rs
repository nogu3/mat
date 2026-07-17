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
    GroupNotFound {
        fabric_index: u8,
        group_id: u16,
    },
    BadCounter(&'static str),
    Locked,
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
            KvsError::GroupNotFound {
                fabric_index,
                group_id,
            } => {
                write!(
                    f,
                    "kvs: group {group_id} not found in fabric {fabric_index}'s GroupKeyMap"
                )
            }
            KvsError::BadCounter(reason) => write!(f, "kvs key \"g/gdc\": {reason}"),
            KvsError::Locked => write!(f, "kvs: locked by another process"),
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
/// the keyset's key array, returning the 16-byte operational key and its
/// 16-bit hash (the group session id, aka GKH), if present.
///
/// The hash (Context(5)) is `Option`: the IPK read path
/// ([`parse_keyset`]/[`read_fabric_credentials`], used by M4 CASE) never
/// needed it and the pre-M5 parser tolerated its absence — restore that
/// tolerance here. Only the group-send path
/// ([`parse_keyset_first_entry`]/[`read_group_credentials`]) actually needs
/// the hash (it's the group session id used on the wire), so it is the one
/// that turns a missing hash into an error.
fn parse_key_struct(r: &mut Reader, fabric_index: u8) -> Result<([u8; 16], Option<u16>), KvsError> {
    let el = next_keyset_el(r, fabric_index)?;
    if el.value != Value::StructStart {
        return Err(KvsError::BadKeyset {
            fabric_index,
            reason: "malformed tlv",
        });
    }

    let mut key: Option<[u8; 16]> = None;
    let mut hash: Option<u16> = None;
    loop {
        let el = next_keyset_el(r, fabric_index)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(5), Value::Uint(v)) => {
                hash = Some(u16::try_from(v).map_err(|_| KvsError::BadKeyset {
                    fabric_index,
                    reason: "hash out of range",
                })?);
            }
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
    let key = key.ok_or(KvsError::BadKeyset {
        fabric_index,
        reason: "missing operational key",
    })?;
    Ok((key, hash))
}

/// Parses the chip-tool `KeySet` TLV blob, returning the operational key and
/// (if present) hash of the first entry in the key array. `keys_count`
/// (Context(2)) must be at least 1; unknown tags/containers are skipped. The
/// hash is `Option` — see [`parse_key_struct`] for why; callers that need the
/// hash (group send) must check for `None` themselves, callers that don't
/// (IPK read, via [`parse_keyset`]) ignore it entirely.
fn parse_keyset_first_entry(
    blob: &[u8],
    fabric_index: u8,
) -> Result<([u8; 16], Option<u16>), KvsError> {
    let mut r = Reader::new(blob);

    let el = next_keyset_el(&mut r, fabric_index)?;
    if el.value != Value::StructStart {
        return Err(KvsError::BadKeyset {
            fabric_index,
            reason: "malformed tlv",
        });
    }

    let mut keys_count: Option<u64> = None;
    let mut entry: Option<([u8; 16], Option<u16>)> = None;

    loop {
        let el = next_keyset_el(&mut r, fabric_index)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::Uint(v)) => keys_count = Some(v),
            (Tag::Context(3), Value::ArrayStart) => {
                entry = Some(parse_key_struct(&mut r, fabric_index)?);
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
    entry.ok_or(KvsError::BadKeyset {
        fabric_index,
        reason: "missing key entries",
    })
}

/// Parses the chip-tool `KeySet` TLV blob, returning the operational group
/// key (`ipk_operational`) of the first entry in the key array. Thin wrapper
/// over [`parse_keyset_first_entry`] that discards the hash *without
/// requiring its presence* — the IPK read path never needed the hash, and
/// the pre-M5 parser tolerated a blob without it; keep that tolerance here
/// even though the group-send path (which does need the hash) now enforces
/// it in [`read_group_credentials`].
fn parse_keyset(blob: &[u8], fabric_index: u8) -> Result<[u8; 16], KvsError> {
    parse_keyset_first_entry(blob, fabric_index).map(|(key, _hash)| key)
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

/// Group send credentials from the GroupKeyMap + keyset blob: the group
/// session id (the keyset's GKH) and the operational encryption key.
#[derive(Clone)]
pub struct GroupCredentials {
    pub session_id: u16,
    pub encryption_key: [u8; 16],
}

/// Manual `Debug`: carries the operational group encryption key, a secret.
/// See `RawFabricCredentials`'s `Debug` impl for the same rationale.
impl std::fmt::Debug for GroupCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupCredentials")
            .field("session_id", &self.session_id)
            .field("encryption_key", &"[REDACTED]")
            .finish()
    }
}

/// KeyMapData blob (`f/<idx>/gk/<n>`): struct{ ctx1: group_id, ctx2:
/// keyset_id, ctx3: next }. Verified against a live v1.4.2.0 store.
/// Malformed entries yield `None` so the scan can skip them.
fn parse_keymap_entry(blob: &[u8]) -> Option<(u16, u16)> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let (mut group, mut keyset) = (None, None);
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => group = u16::try_from(v).ok(),
            (Tag::Context(2), Value::Uint(v)) => keyset = u16::try_from(v).ok(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_rest_of_container(&mut r, 0).ok()?;
            }
            _ => {}
        }
    }
    Some((group?, keyset?))
}

/// Reads the send credentials for `group_id`: scans the GroupKeyMap
/// (`f/<idx>/gk/1..=ff`, sparse after removals, so no early stop) for the
/// keyset id, then takes the first key entry's hash + operational key from
/// the keyset blob.
pub fn read_group_credentials(
    path: &Path,
    fabric_index: u8,
    group_id: u16,
) -> Result<GroupCredentials, KvsError> {
    let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
    let section = default_section(&text).ok_or(KvsError::SectionMissing)?;
    let mut keyset_id = None;
    for n in 1u32..=0xff {
        let Some(blob) = decode_b64(section, &format!("f/{fabric_index}/gk/{n:x}"))? else {
            continue;
        };
        if let Some((gid, ksid)) = parse_keymap_entry(&blob) {
            if gid == group_id {
                keyset_id = Some(ksid);
                break;
            }
        }
    }
    let keyset_id = keyset_id.ok_or(KvsError::GroupNotFound {
        fabric_index,
        group_id,
    })?;
    let key = format!("f/{fabric_index}/k/{keyset_id:x}");
    let blob = decode_b64(section, &key)?.ok_or(KvsError::KeyMissing(key))?;
    // parse_keyset と同じ枠組みで最初の key entry の (key, hash) を取る。ただし
    // group 送信は hash（= 群 session id、ワイヤに乗る値）が必須 — IPK 読み出し
    // と違い None を許容しない。
    let (encryption_key, session_id) = parse_keyset_first_entry(&blob, fabric_index)?;
    let session_id = session_id.ok_or(KvsError::BadKeyset {
        fabric_index,
        reason: "missing key hash",
    })?;
    Ok(GroupCredentials {
        session_id,
        encryption_key,
    })
}

/// Reads chip-tool's persisted Global Group Data Counter (`g/gdc`, u32 LE).
pub fn read_group_data_counter(path: &Path) -> Result<Option<u32>, KvsError> {
    let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
    let section = default_section(&text).ok_or(KvsError::SectionMissing)?;
    match decode_b64(section, "g/gdc")? {
        None => Ok(None),
        Some(b) => {
            let arr: [u8; 4] = b
                .as_slice()
                .try_into()
                .map_err(|_| KvsError::BadCounter("g/gdc must be 4 bytes"))?;
            Ok(Some(u32::from_le_bytes(arr)))
        }
    }
}

/// chip-tool INI KVS への書込トランザクション（M8c-2）。
///
/// open で sidecar `<ini>.lock` に advisory flock（NonBlocking exclusive、
/// `group.rs` の counter と同流儀 — 本体は tmp+rename で置換されるので本体
/// fd への flock は無効化される）を取り、ファイル全行をメモリへ読む。
/// set/remove は [Default] セクション内の行だけを操作し（既存行は in-place
/// 置換、新規は末尾追記、書式は chip-tool inipp と同じ `key=value`）、他の
/// 行は byte 単位で保全する。commit が tmp+fsync+rename の原子置換。
/// ロックは Drop まで保持（commit を呼ばなければ何も書かれない）。
pub struct KvsTxn {
    path: std::path::PathBuf,
    lines: Vec<String>,
    /// [Default] セクション内の行範囲（`lines` の添字、[start, end)）。
    default_start: usize,
    default_end: usize,
    _lock: std::fs::File,
}

impl KvsTxn {
    pub fn open(path: &Path) -> Result<Self, KvsError> {
        use rustix::fs::{flock, FlockOperation};
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(std::path::PathBuf::from(lock_path))
            .map_err(KvsError::Io)?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                KvsError::Locked
            } else {
                KvsError::Io(std::io::Error::other(e))
            }
        })?;
        let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        // [Default] セクション境界を行単位で確定。
        let mut default_start = None;
        let mut default_end = lines.len();
        for (i, line) in lines.iter().enumerate() {
            match default_start {
                None => {
                    if line.trim() == "[Default]" {
                        default_start = Some(i + 1);
                    }
                }
                Some(_) => {
                    if line.trim_start().starts_with('[') {
                        default_end = i;
                        break;
                    }
                }
            }
        }
        let default_start = default_start.ok_or(KvsError::SectionMissing)?;
        Ok(Self {
            path: path.to_path_buf(),
            lines,
            default_start,
            default_end,
            _lock: lock,
        })
    }

    /// [Default] 内で key の行を探す（先頭 `=` で分割し両側 trim — 読み側
    /// `lookup` と同じ寛容さ）。
    fn find(&self, key: &str) -> Option<usize> {
        (self.default_start..self.default_end).find(|&i| {
            self.lines[i]
                .split_once('=')
                .is_some_and(|(k, _)| k.trim() == key)
        })
    }

    /// key の値を base64 デコードして返す。無い・空は None（読み側
    /// `decode_b64` と同じ扱い）。
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvsError> {
        use base64ct::{Base64, Encoding};
        match self.find(key) {
            None => Ok(None),
            Some(i) => {
                let v = self.lines[i]
                    .split_once('=')
                    .expect("find matched")
                    .1
                    .trim();
                if v.is_empty() {
                    return Ok(None);
                }
                Base64::decode_vec(v)
                    .map(Some)
                    .map_err(|_| KvsError::BadBase64(key.to_string()))
            }
        }
    }

    pub fn set(&mut self, key: &str, value: &[u8]) {
        use base64ct::{Base64, Encoding};
        let line = format!("{key}={}", Base64::encode_string(value));
        match self.find(key) {
            Some(i) => self.lines[i] = line,
            None => {
                self.lines.insert(self.default_end, line);
                self.default_end += 1;
            }
        }
    }

    pub fn remove(&mut self, key: &str) {
        if let Some(i) = self.find(key) {
            self.lines.remove(i);
            self.default_end -= 1;
        }
    }

    /// tmp + fsync + rename の原子置換（`group.rs` counter の persist と同流儀）。
    pub fn commit(self) -> Result<(), KvsError> {
        use std::io::Write;
        let tmp = self.path.with_extension("ini.tmp");
        let mut f = std::fs::File::create(&tmp).map_err(KvsError::Io)?;
        let mut body = self.lines.join("\n");
        body.push('\n');
        f.write_all(body.as_bytes()).map_err(KvsError::Io)?;
        f.sync_all().map_err(KvsError::Io)?;
        std::fs::rename(&tmp, &self.path).map_err(KvsError::Io)?;
        Ok(())
    }
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

    fn keymap_blob(group_id: u16, keyset_id: u16, next: u8) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(group_id));
        w.put_uint(Tag::Context(2), u64::from(keyset_id));
        w.put_uint(Tag::Context(3), u64::from(next));
        w.end_container();
        w.finish()
    }

    fn keyset_blob_with_hash(key: &[u8; 16], hash: u16) -> Vec<u8> {
        // keyset_blob_with_count と同構造だが最初のエントリの ctx5 に hash を焼く
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0);
        w.put_uint(Tag::Context(2), 1);
        w.start_array(Tag::Context(3));
        for i in 0..3u8 {
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), u64::from(i == 0));
            w.put_uint(Tag::Context(5), if i == 0 { u64::from(hash) } else { 0 });
            w.put_bytes(Tag::Context(6), if i == 0 { key } else { &[0u8; 16] });
            w.end_container();
        }
        w.end_container();
        w.put_uint(Tag::Context(7), 0);
        w.end_container();
        w.finish()
    }

    /// keyset_blob_with_hash と同構造だが、最初のエントリの ctx5（hash）を丸ごと
    /// 書かない — 実機で観測された「hash 無し」keyset blob（M1〜M4 で許容して
    /// いた形）を再現する。
    fn keyset_blob_no_hash(key: &[u8; 16]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0);
        w.put_uint(Tag::Context(2), 1);
        w.start_array(Tag::Context(3));
        for i in 0..3u8 {
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), u64::from(i == 0));
            // ctx5 (hash) は意図的に省略。
            w.put_bytes(Tag::Context(6), if i == 0 { key } else { &[0u8; 16] });
            w.end_container();
        }
        w.end_container();
        w.put_uint(Tag::Context(7), 0);
        w.end_container();
        w.finish()
    }

    const GROUP_KEY: [u8; 16] = [0xDD; 16];

    #[test]
    fn keyset_without_hash_tolerated_by_ipk_path_but_rejected_by_group_path() {
        // 最終レビュー指摘: parse_key_struct を M5 で ctx5(hash) 必須に締めて
        // しまうと、IPK 読み出し（M1〜M4 実機で検証済みの容認的パース）が
        // hash 無し blob で壊れる。IPK 経路は成功し、group 経路だけ拒否する
        // ことを確認する。
        let ks_no_hash = keyset_blob_no_hash(&GROUP_KEY);

        // IPK 読み出し（read_fabric_credentials 経由）: hash 無しでも成功。
        let op = opkey_blob(&PUB, &PRIV);
        let path = write_ini(&[
            ("f/1/r", b"r"),
            ("f/1/n", b"n"),
            ("f/1/o", &op),
            ("f/1/k/0", &ks_no_hash),
        ]);
        let c = read_fabric_credentials(&path, 1).unwrap();
        assert_eq!(c.ipk_operational, GROUP_KEY);
        std::fs::remove_file(&path).ok();

        // group 読み出し（read_group_credentials 経由）: hash が無いと拒否する。
        let path2 = write_ini(&[
            ("f/2/gk/1", &keymap_blob(10, 0x3c, 0)[..]),
            ("f/2/k/3c", &ks_no_hash[..]),
        ]);
        let err = read_group_credentials(&path2, 2, 10).unwrap_err();
        assert!(
            matches!(
                err,
                KvsError::BadKeyset {
                    fabric_index: 2,
                    reason: "missing key hash"
                }
            ),
            "unexpected error: {err}"
        );
        std::fs::remove_file(&path2).ok();
    }

    #[test]
    fn reads_group_credentials_scanning_past_builtin_entries() {
        // 実機と同形: chip-tool 組み込みサンプル (0x101→0x1a1) が先に居て、
        // 本命 (group 10 → keyset 0x3c) が gk/4 に居る。
        let path = write_ini(&[
            ("f/2/gk/1", &keymap_blob(0x101, 0x1a1, 2)[..]),
            ("f/2/gk/4", &keymap_blob(10, 0x3c, 0)[..]),
            ("f/2/k/1a1", &keyset_blob_with_hash(&[0xEE; 16], 0x1111)[..]),
            ("f/2/k/3c", &keyset_blob_with_hash(&GROUP_KEY, 0x855f)[..]),
        ]);
        let c = read_group_credentials(&path, 2, 10).unwrap();
        assert_eq!(c.session_id, 0x855f);
        assert_eq!(c.encryption_key, GROUP_KEY);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn group_not_in_keymap_is_group_not_found() {
        let path = write_ini(&[("f/2/gk/1", &keymap_blob(0x101, 0x1a1, 0)[..])]);
        assert!(matches!(
            read_group_credentials(&path, 2, 10),
            Err(KvsError::GroupNotFound {
                fabric_index: 2,
                group_id: 10
            })
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn keymap_hit_without_keyset_blob_is_key_missing() {
        let path = write_ini(&[("f/2/gk/1", &keymap_blob(10, 0x3c, 0)[..])]);
        assert!(matches!(
            read_group_credentials(&path, 2, 10),
            Err(KvsError::KeyMissing(k)) if k == "f/2/k/3c"
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn malformed_keymap_entry_is_skipped() {
        // gk/1 が壊れていても gk/2 の本命は見つかる（容認的走査）。
        let path = write_ini(&[
            ("f/2/gk/1", &[0xFF, 0x00][..]),
            ("f/2/gk/2", &keymap_blob(10, 0x3c, 0)[..]),
            ("f/2/k/3c", &keyset_blob_with_hash(&GROUP_KEY, 0x855f)[..]),
        ]);
        assert_eq!(
            read_group_credentials(&path, 2, 10).unwrap().session_id,
            0x855f
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn reads_group_data_counter_u32_le() {
        let path = write_ini(&[("g/gdc", &175851168u32.to_le_bytes()[..])]);
        assert_eq!(read_group_data_counter(&path).unwrap(), Some(175851168));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn missing_gdc_is_none_and_bad_length_is_error() {
        let none = write_ini(&[("f/2/n", &[0u8][..])]);
        assert_eq!(read_group_data_counter(&none).unwrap(), None);
        std::fs::remove_file(&none).ok();
        let bad = write_ini(&[("g/gdc", &[1u8, 2, 3][..])]);
        assert!(matches!(
            read_group_data_counter(&bad),
            Err(KvsError::BadCounter(_))
        ));
        std::fs::remove_file(bad).ok();
    }

    // ---- M8c-2: KvsTxn ----

    fn tmp_ini(lines: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chip_tool_config.ini");
        std::fs::write(&p, lines).unwrap();
        (dir, p)
    }

    #[test]
    fn kvs_txn_set_get_roundtrip_and_preserves_unrelated_lines() {
        let (_d, p) = tmp_ini("[Default]\ng/gdc=AQAAAA==\n");
        let mut txn = KvsTxn::open(&p).unwrap();
        assert_eq!(txn.get("nope").unwrap(), None);
        txn.set("f/2/g", &[0x15, 0x18]);
        assert_eq!(txn.get("f/2/g").unwrap().unwrap(), vec![0x15, 0x18]);
        txn.commit().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        // 無関係キーは保全、新キーは [Default] 内に chip-tool inipp 形式（key=value）で追記。
        assert!(text.contains("g/gdc=AQAAAA=="), "{text}");
        assert!(text.contains("f/2/g=FRg="), "{text}");
        // 再読込でも読める（自作 reader との整合）。
        let txn2 = KvsTxn::open(&p).unwrap();
        assert_eq!(txn2.get("f/2/g").unwrap().unwrap(), vec![0x15, 0x18]);
    }

    #[test]
    fn kvs_txn_set_replaces_existing_line_in_place() {
        let (_d, p) = tmp_ini("[Default]\nf/2/g=AAAA\nother=x\n");
        let mut txn = KvsTxn::open(&p).unwrap();
        txn.set("f/2/g", &[1]);
        txn.commit().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("f/2/g=").count(), 1, "{text}");
        assert!(text.contains("other=x"));
    }

    #[test]
    fn kvs_txn_remove_deletes_line() {
        let (_d, p) = tmp_ini("[Default]\nf/2/gk/1=AAAA\nkeep=y\n");
        let mut txn = KvsTxn::open(&p).unwrap();
        txn.remove("f/2/gk/1");
        txn.commit().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("f/2/gk/1"), "{text}");
        assert!(text.contains("keep=y"));
    }

    #[test]
    fn kvs_txn_open_fails_without_default_section() {
        let (_d, p) = tmp_ini("[Other]\nk=v\n");
        assert!(matches!(KvsTxn::open(&p), Err(KvsError::SectionMissing)));
    }

    #[test]
    fn kvs_txn_second_open_would_block() {
        let (_d, p) = tmp_ini("[Default]\n");
        let _held = KvsTxn::open(&p).unwrap();
        assert!(matches!(KvsTxn::open(&p), Err(KvsError::Locked)));
    }
}
