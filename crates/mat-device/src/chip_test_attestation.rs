//! Task 10 (M2 Echo attestation checkpoint) experiment: the canonical
//! connectedhomeip test attestation chain (VID `0xFFF1` / PID `0x8000`),
//! vendored under `testdata/chip-test-attestation/` (see that directory's
//! `README.md` for full provenance/sha256s and an important deviation from
//! the task brief — the PAA is `Chip-Test-PAA-FFF1-Cert.der`, not
//! `Chip-Test-PAA-NoVID-Cert.der`, and the CD is locally built rather than
//! vendored because connectedhomeip doesn't publish a pre-built one for
//! this VID/PID pair).
//!
//! `device::Device::new` uses [`dev_attestation`] instead of
//! `x509::generate_dev_attestation` when `matv.toml` sets `attestation =
//! "chip-test"` — an experiment to test the hypothesis that Echo's
//! cloud-side attestation validation accepts the canonical chip test chain
//! where it rejects our self-generated one. Public Matter **TEST**
//! material only, same caveat as `mat_controller::cd::TEST_CD_SIGNING_KEY`
//! — never a production device identity.

use mat_controller::x509::DevAttestation;

const PAA_DER: &[u8] =
    include_bytes!("../testdata/chip-test-attestation/Chip-Test-PAA-FFF1-Cert.der");
const PAI_DER: &[u8] =
    include_bytes!("../testdata/chip-test-attestation/Chip-Test-PAI-FFF1-8000-Cert.der");
const DAC_DER: &[u8] =
    include_bytes!("../testdata/chip-test-attestation/Chip-Test-DAC-FFF1-8000-0000-Cert.der");
const CD_DER: &[u8] =
    include_bytes!("../testdata/chip-test-attestation/Chip-Test-CD-FFF1-8000.der");

/// Raw 32-byte P-256 scalar for `Chip-Test-DAC-FFF1-8000-0000-Key.der`'s
/// private key (SEC1/PKCS8 DER upstream — not parsed here, per Task 10's
/// "no new DER-parsing dependency" constraint). Extracted with:
///
/// ```text
/// $ openssl ec -inform der -in Chip-Test-DAC-FFF1-8000-0000-Key.der -text -noout
/// priv:
///     21:f2:e3:e4:20:c0:70:17:34:81:04:69:b6:ba:d1:
///     5c:f3:06:78:22:c9:a4:a5:96:c1:86:fa:9b:ef:15:
///     3f:a1
/// ```
///
/// This is the identical private key connectedhomeip publishes in
/// plaintext at `credentials/test/attestation/Chip-Test-DAC-FFF1-8000-0000-Key.der`
/// — public Matter TEST material, not secret and never for production
/// device identity (testdata `README.md` has the full caveat). The test
/// `chip_test_dac_key_matches_cert` below checks this scalar's derived
/// public key against the vendored DAC cert's `SubjectPublicKey`.
const CHIP_TEST_DAC_PRIVATE_KEY: [u8; 32] = [
    0x21, 0xf2, 0xe3, 0xe4, 0x20, 0xc0, 0x70, 0x17, 0x34, 0x81, 0x04, 0x69, 0xb6, 0xba, 0xd1, 0x5c,
    0xf3, 0x06, 0x78, 0x22, 0xc9, 0xa4, 0xa5, 0x96, 0xc1, 0x86, 0xfa, 0x9b, 0xef, 0x15, 0x3f, 0xa1,
];

/// Builds the canonical connectedhomeip test `DevAttestation` chain in
/// place of `x509::generate_dev_attestation`. Infallible — everything here
/// is a fixed vendored constant, not generated. The certification
/// declaration is the vendored/pre-built blob used **verbatim** (not
/// re-signed or reconstructed) — see `testdata/chip-test-attestation/README.md`
/// for how it was built.
pub fn dev_attestation() -> DevAttestation {
    DevAttestation {
        paa_der: PAA_DER.to_vec(),
        pai_der: PAI_DER.to_vec(),
        dac_der: DAC_DER.to_vec(),
        dac_private_key: CHIP_TEST_DAC_PRIVATE_KEY,
        certification_declaration: CD_DER.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::x509::parse_x509;

    /// (a) embedded key scalar <-> vendored DAC public key match.
    #[test]
    fn chip_test_dac_key_matches_cert() {
        use p256::elliptic_curve::sec1::ToEncodedPoint;

        let secret = p256::SecretKey::from_slice(&CHIP_TEST_DAC_PRIVATE_KEY)
            .expect("embedded scalar is a valid p256 secret key");
        let derived_pubkey: [u8; 65] = secret
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .expect("uncompressed p256 point is 65 bytes");

        let cert = parse_x509(DAC_DER).expect("vendored DAC cert parses");
        assert_eq!(
            derived_pubkey, cert.public_key,
            "embedded DAC scalar's derived public key must match the vendored \
             DAC cert's SubjectPublicKey byte-for-byte"
        );
    }

    /// (b) chip-test mode's `DevAttestation` carries the vendored CD (and
    /// the rest of the chain) verbatim — no on-the-fly generation.
    #[test]
    fn dev_attestation_carries_vendored_material_verbatim() {
        let dev = dev_attestation();
        assert_eq!(
            dev.certification_declaration, CD_DER,
            "chip-test mode must use the pre-built CD blob verbatim"
        );
        assert_eq!(dev.paa_der, PAA_DER);
        assert_eq!(dev.pai_der, PAI_DER);
        assert_eq!(dev.dac_der, DAC_DER);
        assert_eq!(dev.dac_private_key, CHIP_TEST_DAC_PRIVATE_KEY);
    }

    /// The vendored chain is a real, verifiable PAA->PAI->DAC chain (not
    /// just individually well-formed DER) — each cert's signature checks
    /// out against its issuer's public key.
    #[test]
    fn vendored_chain_verifies_paa_to_pai_to_dac() {
        let paa = parse_x509(PAA_DER).expect("PAA parses");
        let pai = parse_x509(PAI_DER).expect("PAI parses");
        let dac = parse_x509(DAC_DER).expect("DAC parses");

        pai.verify_signed_by(&paa)
            .expect("PAI must be signed by PAA");
        dac.verify_signed_by(&pai)
            .expect("DAC must be signed by PAI");
    }
}
