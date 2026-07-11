//! Fabric credentials and CASE-related derivations (spec §4.3.2, §4.14.2).

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Compressed fabric id (spec §4.3.2.2): HKDF over the root public key
/// (uncompressed-point prefix dropped) with the fabric id as big-endian salt.
pub fn compressed_fabric_id(root_public_key: &[u8; 65], fabric_id: u64) -> [u8; 8] {
    let hk = Hkdf::<Sha256>::new(Some(&fabric_id.to_be_bytes()), &root_public_key[1..]);
    let mut out = [0u8; 8];
    hk.expand(b"CompressedFabric", &mut out)
        .expect("8 bytes is a valid hkdf-sha256 output length");
    out
}

/// CASE destination identifier (spec §4.14.2.1.2). Fabric id / node id are
/// little-endian here (unlike the big-endian salt above).
pub fn case_destination_id(
    ipk_operational: &[u8; 16],
    initiator_random: &[u8; 32],
    root_public_key: &[u8; 65],
    fabric_id: u64,
    node_id: u64,
) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(ipk_operational).expect("hmac accepts any key length");
    mac.update(initiator_random);
    mac.update(root_public_key);
    mac.update(&fabric_id.to_le_bytes());
    mac.update(&node_id.to_le_bytes());
    mac.finalize().into_bytes().into()
}

/// Assembled, verified fabric credentials for CASE (own-chain sanity already
/// checked; TLV byte forms retained for Sigma3).
#[derive(Clone)]
pub struct FabricCredentials {
    pub rcac_tlv: Vec<u8>,
    pub icac_tlv: Option<Vec<u8>>,
    pub noc_tlv: Vec<u8>,
    pub op_public_key: [u8; 65],
    pub op_private_key: [u8; 32],
    pub ipk_operational: [u8; 16],
    pub node_id: u64,
    pub fabric_id: u64,
    pub root_public_key: [u8; 65],
}

/// Manual `Debug`: this struct carries the operational private key and the
/// fabric's identity-protection key, both secret. Never derive `Debug` here
/// again — certs/keys are logged incidentally via `{:?}` (error contexts,
/// test failure output, etc.) and this repo is public.
impl std::fmt::Debug for FabricCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FabricCredentials")
            .field("rcac_tlv_len", &self.rcac_tlv.len())
            .field("icac_tlv_len", &self.icac_tlv.as_ref().map(Vec::len))
            .field("noc_tlv_len", &self.noc_tlv.len())
            .field("op_public_key_len", &self.op_public_key.len())
            .field("op_private_key", &"[REDACTED]")
            .field("ipk_operational", &"[REDACTED]")
            .field("node_id", &self.node_id)
            .field("fabric_id", &self.fabric_id)
            .field("root_public_key_len", &self.root_public_key.len())
            .finish()
    }
}

/// `FabricCredentials::from_raw` / `from_self_issued` error.
#[derive(Debug)]
pub enum FabricError {
    /// Certificate parse/verification failure (own-chain sanity check).
    Cert(crate::cert::CertError),
    /// NOC subject is missing node id and/or fabric id. Defensive only: for
    /// both constructors the NOC has already passed through
    /// `verify_noc_chain`, which itself requires both ids to be present, so
    /// this variant is not reachable from `from_raw` or `from_self_issued`
    /// today. Kept as a belt-and-suspenders guard against a future
    /// `verify_noc_chain` change that relaxes that guarantee.
    NocMissingIds,
    /// KVS operational public key does not match the NOC's public key.
    OpKeyMismatch,
    /// Operational key pair generation failed (self-issued path only).
    GenKey,
    /// Self-issued NOC failed to build or self-verify.
    SelfIssue(crate::cert::CertError),
}

impl std::fmt::Display for FabricError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FabricError::Cert(e) => write!(f, "fabric credentials: {e}"),
            FabricError::NocMissingIds => {
                write!(f, "fabric credentials: NOC subject missing node/fabric id")
            }
            FabricError::OpKeyMismatch => write!(
                f,
                "fabric credentials: operational key does not match NOC public key"
            ),
            FabricError::GenKey => write!(f, "operational key generation failed"),
            FabricError::SelfIssue(e) => write!(f, "self-issued NOC invalid: {e}"),
        }
    }
}

impl std::error::Error for FabricError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FabricError::Cert(e) => Some(e),
            FabricError::SelfIssue(e) => Some(e),
            FabricError::NocMissingIds | FabricError::OpKeyMismatch | FabricError::GenKey => None,
        }
    }
}

impl From<crate::cert::CertError> for FabricError {
    fn from(e: crate::cert::CertError) -> Self {
        FabricError::Cert(e)
    }
}

impl FabricCredentials {
    /// Parse RCAC/ICAC/NOC, verify the own-fabric chain is internally
    /// consistent (fail fast, before CASE), and cross-check the KVS
    /// operational public key against the NOC's public key.
    pub fn from_raw(raw: crate::kvs::RawFabricCredentials) -> Result<Self, FabricError> {
        let rcac_cert = crate::cert::MatterCert::parse(&raw.rcac)?;
        let icac_cert = raw
            .icac
            .as_deref()
            .map(crate::cert::MatterCert::parse)
            .transpose()?;
        let noc_cert = crate::cert::MatterCert::parse(&raw.noc)?;

        crate::cert::verify_noc_chain(&noc_cert, icac_cert.as_ref(), &rcac_cert)?;

        let node_id = noc_cert.node_id().ok_or(FabricError::NocMissingIds)?;
        let fabric_id = noc_cert.fabric_id().ok_or(FabricError::NocMissingIds)?;
        let root_public_key = rcac_cert.pub_key;

        if raw.op_public_key != noc_cert.pub_key {
            return Err(FabricError::OpKeyMismatch);
        }

        Ok(FabricCredentials {
            rcac_tlv: raw.rcac,
            icac_tlv: raw.icac,
            noc_tlv: raw.noc,
            op_public_key: raw.op_public_key,
            op_private_key: raw.op_private_key,
            ipk_operational: raw.ipk_operational,
            node_id,
            fabric_id,
            root_public_key,
        })
    }

    /// Generate a fresh operational key, self-issue a NOC under the KVS root,
    /// and assemble credentials for CASE.
    pub fn from_self_issued(m: crate::kvs::SelfIssueMaterials) -> Result<Self, FabricError> {
        use p256::elliptic_curve::sec1::ToEncodedPoint;

        // 1. new operational key pair.
        let sk = crate::case::random_p256_secret();
        let op_private_key: [u8; 32] = sk.to_bytes().into();
        let op_public_key: [u8; 65] = sk
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .map_err(|_| FabricError::GenKey)?;

        // 2. self-issue a NOC under the root.
        let rcac = crate::cert::MatterCert::parse(&m.rcac).map_err(FabricError::Cert)?;
        let mut serial = [0u8; 8];
        getrandom::getrandom(&mut serial).expect("os rng");
        serial[0] &= 0x7F; // keep the BER INTEGER's minimal positive form
        let noc = crate::cert::issue_noc(
            &op_public_key,
            m.node_id,
            m.fabric_id,
            &rcac,
            &m.root_private_key,
            &serial,
        )
        .map_err(FabricError::SelfIssue)?;

        // 3. self-check (generator and verifier cross-check each other).
        crate::cert::verify_noc_chain(&noc, None, &rcac).map_err(FabricError::SelfIssue)?;

        Ok(FabricCredentials {
            rcac_tlv: m.rcac,
            icac_tlv: None,
            noc_tlv: noc.to_tlv(),
            op_public_key,
            op_private_key,
            ipk_operational: m.ipk_operational,
            node_id: m.node_id,
            fabric_id: m.fabric_id,
            root_public_key: m.root_public_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Matter spec §4.3.2.2 / §4.14.2 掲載のテストベクタ（SDK TestChipCryptoPAL.cpp と同一）
    const ROOT_PUB: [u8; 65] = [
        0x04, 0x4a, 0x9f, 0x42, 0xb1, 0xca, 0x48, 0x40, 0xd3, 0x72, 0x92, 0xbb, 0xc7, 0xf6, 0xa7,
        0xe1, 0x1e, 0x22, 0x20, 0x0c, 0x97, 0x6f, 0xc9, 0x00, 0xdb, 0xc9, 0x8a, 0x7a, 0x38, 0x3a,
        0x64, 0x1c, 0xb8, 0x25, 0x4a, 0x2e, 0x56, 0xd4, 0xe2, 0x95, 0xa8, 0x47, 0x94, 0x3b, 0x4e,
        0x38, 0x97, 0xc4, 0xa7, 0x73, 0xe9, 0x30, 0x27, 0x7b, 0x4d, 0x9f, 0xbe, 0xde, 0x8a, 0x05,
        0x26, 0x86, 0xbf, 0xac, 0xfa,
    ];
    const FABRIC_ID: u64 = 0x2906_C908_D115_D362;

    #[test]
    fn derives_spec_compressed_fabric_id() {
        assert_eq!(
            compressed_fabric_id(&ROOT_PUB, FABRIC_ID),
            [0x87, 0xe1, 0xb0, 0x04, 0xe2, 0x35, 0xa1, 0x30]
        );
    }

    #[test]
    fn derives_spec_destination_id() {
        let ipk = [
            0x9b, 0xc6, 0x1c, 0xd9, 0xc6, 0x2a, 0x2d, 0xf6, 0xd6, 0x4d, 0xfc, 0xaa, 0x9d, 0xc4,
            0x72, 0xd4,
        ];
        let random = [
            0x7e, 0x17, 0x12, 0x31, 0x56, 0x8d, 0xfa, 0x17, 0x20, 0x6b, 0x3a, 0xcc, 0xf8, 0xfa,
            0xec, 0x2f, 0x4d, 0x21, 0xb5, 0x80, 0x11, 0x31, 0x96, 0xf4, 0x7c, 0x7c, 0x4d, 0xeb,
            0x81, 0x0a, 0x73, 0xdc,
        ];
        let expected = [
            0xdc, 0x35, 0xdd, 0x5f, 0xc9, 0x13, 0x4c, 0xc5, 0x54, 0x45, 0x38, 0xc9, 0xc3, 0xfc,
            0x42, 0x97, 0xc1, 0xec, 0x33, 0x70, 0xc8, 0x39, 0x13, 0x6a, 0x80, 0xe1, 0x07, 0x96,
            0x45, 0x1d, 0x4c, 0x53,
        ];
        assert_eq!(
            case_destination_id(&ipk, &random, &ROOT_PUB, FABRIC_ID, 0xCD55_44AA_7B13_EF14),
            expected
        );
    }

    #[test]
    fn builds_credentials_from_fixture_chain() {
        let noc = include_bytes!("../tests/fixtures/node01_01_chip.bin").to_vec();
        let icac = include_bytes!("../tests/fixtures/ica01_chip.bin").to_vec();
        let rcac = include_bytes!("../tests/fixtures/root01_chip.bin").to_vec();
        let node_pub: [u8; 65] = include_bytes!("../tests/fixtures/node01_01_pubkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        let node_priv: [u8; 32] = include_bytes!("../tests/fixtures/node01_01_privkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        let raw = crate::kvs::RawFabricCredentials {
            rcac,
            icac: Some(icac),
            noc,
            op_public_key: node_pub,
            op_private_key: node_priv,
            ipk_operational: [0xCC; 16],
        };
        let creds = FabricCredentials::from_raw(raw).unwrap();
        assert_ne!(creds.node_id, 0);
        assert_ne!(creds.fabric_id, 0);
        assert_eq!(
            creds.root_public_key.as_slice(),
            include_bytes!("../tests/fixtures/root01_pubkey.bin")
        );
    }

    #[test]
    fn from_self_issued_builds_case_ready_credentials() {
        // Treat the root01 fixtures as the KVS's self-issue materials.
        let rcac = include_bytes!("../tests/fixtures/root01_chip.bin").to_vec();
        let root_priv: [u8; 32] = include_bytes!("../tests/fixtures/root01_privkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        let root_pub: [u8; 65] = include_bytes!("../tests/fixtures/root01_pubkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        let m = crate::kvs::SelfIssueMaterials {
            rcac,
            root_public_key: root_pub,
            root_private_key: root_priv,
            ipk_operational: [0xCC; 16],
            node_id: 0x1B669,
            fabric_id: 1,
        };
        let creds = FabricCredentials::from_self_issued(m).unwrap();
        assert_eq!(creds.node_id, 0x1B669);
        assert_eq!(creds.fabric_id, 1);
        assert_eq!(creds.icac_tlv, None); // two-tier chain, signed directly by root
        assert_eq!(creds.root_public_key, root_pub);
        // Generated key and the NOC's public key match.
        let noc = crate::cert::MatterCert::parse(&creds.noc_tlv).unwrap();
        assert_eq!(noc.pub_key, creds.op_public_key);
        // The self-issued NOC chains to the root.
        let rcac_cert = crate::cert::MatterCert::parse(&creds.rcac_tlv).unwrap();
        crate::cert::verify_noc_chain(&noc, None, &rcac_cert).unwrap();
    }

    #[test]
    fn rejects_opkey_not_matching_noc() {
        let raw = crate::kvs::RawFabricCredentials {
            rcac: include_bytes!("../tests/fixtures/root01_chip.bin").to_vec(),
            icac: Some(include_bytes!("../tests/fixtures/ica01_chip.bin").to_vec()),
            noc: include_bytes!("../tests/fixtures/node01_01_chip.bin").to_vec(),
            op_public_key: [0xAA; 65], // NOC の公開鍵と不一致
            op_private_key: [0xBB; 32],
            ipk_operational: [0xCC; 16],
        };
        assert!(matches!(
            FabricCredentials::from_raw(raw),
            Err(FabricError::OpKeyMismatch)
        ));
    }
}
