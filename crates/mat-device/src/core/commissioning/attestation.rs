//! Node Operational Credentials — the attestation / CSR half (spec
//! §11.17.6.1–§11.17.6.5, §11.17.6.11): AttestationRequest /
//! CertificateChainRequest / CSRRequest / AddTrustedRootCertificate. All
//! stage material in `Inner::pending` for `AddNOC` (`noc.rs`).

use mat_controller::attestation::{attestation_tbs, encode_attestation_elements};
use mat_controller::case::{eph_pub_bytes, random_p256_secret};
use mat_controller::commissioning::{
    decode_add_trusted_root, decode_attestation_request, decode_cert_chain_request,
    decode_csr_request, encode_attestation_response, encode_cert_chain_response,
    encode_csr_response, encode_nocsr_elements, CERT_TYPE_DAC, CERT_TYPE_PAI,
};
use mat_controller::crypto::sign_ecdsa_p256;
use mat_controller::im;
use mat_controller::x509::generate_csr;

use crate::core::datamodel::{InvokeCtx, InvokeReply};

use super::{Inner, RESP_ATTESTATION, RESP_CERT_CHAIN, RESP_CSR};

impl Inner {
    /// AttestationRequest（spec §11.17.6.7）: signs `AttestationElements`
    /// with the DAC key over `elements ‖ attestation_challenge` — the same
    /// construction `mat_controller::attestation::verify_device_attestation`
    /// checks on the commissioner side. Requires the fail-safe to be armed
    /// (spec §11.17: this and the other commissioning-flow commands below
    /// are only meaningful inside an armed fail-safe window).
    pub(super) fn handle_attestation_request(
        &mut self,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        if !self.fail_safe.is_armed() {
            return InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED);
        }
        let Ok(nonce) = decode_attestation_request(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        // Real CMS-signed Certification Declaration (`mat_controller::cd`),
        // not a placeholder: chip-derived commissioners (chip-tool, Alexa,
        // Google) extract the CMS signer key id, look the verifying key up
        // in their own CD trust store, check the signature, and match the
        // CD's vendor_id/product_id against Basic Information — a device
        // whose CD they can't parse fails commissioning outright
        // (`kCertificationDeclarationNoKeyId`). `mat`'s own commissioner is
        // the lenient one (`attestation::verify_cd_warn` only warns), which
        // is why M1 got away with a placeholder here.
        let elements = encode_attestation_elements(&self.dev.certification_declaration, &nonce, 0);
        let tbs = attestation_tbs(&elements, &ctx.attestation_challenge);
        let signature = sign_ecdsa_p256(&self.dev.dac_private_key, &tbs)
            .expect("dac private key from generate_dev_attestation is always a valid p256 key");
        InvokeReply::Data {
            response_command: RESP_ATTESTATION,
            fields_tlv: encode_attestation_response(&elements, &signature),
        }
    }

    /// CertificateChainRequest（spec §11.17.6.4）: returns the DAC or PAI
    /// DER (never PAA — the commissioner never asks for it directly, it
    /// carries its own trust store per spec §6.2.3).
    pub(super) fn handle_cert_chain_request(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        let Ok(cert_type) = decode_cert_chain_request(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        let der: &[u8] = match cert_type {
            CERT_TYPE_DAC => &self.dev.dac_der,
            CERT_TYPE_PAI => &self.dev.pai_der,
            _ => return InvokeReply::Status(im::STATUS_INVALID_COMMAND),
        };
        InvokeReply::Data {
            response_command: RESP_CERT_CHAIN,
            fields_tlv: encode_cert_chain_response(der),
        }
    }

    /// CSRRequest（spec §11.17.6.9）: generates a fresh operational
    /// keypair, stages it in `pending` for `AddNOC` to cross-check, and
    /// signs the `NOCSRElements` the same way `AttestationResponse` is
    /// signed (`elements ‖ attestation_challenge` with the DAC key — spec
    /// §11.17.5.6, mirrored exactly from the verification code in
    /// `mat_controller::commissioning::run_credential_steps`). Requires the
    /// fail-safe to be armed.
    pub(super) fn handle_csr_request(&mut self, fields_tlv: &[u8], ctx: &InvokeCtx) -> InvokeReply {
        if !self.fail_safe.is_armed() {
            return InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED);
        }
        let Ok(nonce) = decode_csr_request(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };

        let secret = random_p256_secret();
        let op_public_key = eph_pub_bytes(&secret);
        let op_private_key: [u8; 32] = secret.to_bytes().into();
        let csr_der = generate_csr(&secret)
            .expect("csr generation over a freshly generated p256 key never fails");

        self.pending.op_private_key = Some(op_private_key);
        self.pending.op_public_key = Some(op_public_key);

        let elements = encode_nocsr_elements(&csr_der, &nonce);
        let tbs = attestation_tbs(&elements, &ctx.attestation_challenge);
        let signature = sign_ecdsa_p256(&self.dev.dac_private_key, &tbs)
            .expect("dac private key from generate_dev_attestation is always a valid p256 key");

        InvokeReply::Data {
            response_command: RESP_CSR,
            fields_tlv: encode_csr_response(&elements, &signature),
        }
    }

    /// AddTrustedRootCertificate（spec §11.17.6.11）: stages the RCAC for
    /// `AddNOC` to verify the NOC's chain against. Response is a plain IM
    /// success status, not a `NOCResponse` (spec defines no dedicated
    /// response command for this one). Requires the fail-safe to be armed.
    pub(super) fn handle_add_trusted_root(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        if !self.fail_safe.is_armed() {
            return InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED);
        }
        let Ok(rcac_tlv) = decode_add_trusted_root(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        self.pending.trusted_root_tlv = Some(rcac_tlv);
        InvokeReply::Status(im::STATUS_SUCCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{drive_invoke, expect_data, TEST_CHALLENGE};
    use super::super::CommissioningServer;
    use super::*;
    use crate::core::fabric_store::FabricStore;
    use mat_controller::attestation::verify_device_attestation;
    use mat_controller::commissioning::{
        decode_attestation_response, encode_arm_fail_safe, encode_attestation_request,
        encode_cert_chain_request, CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS,
        CMD_ARM_FAIL_SAFE, CMD_ATTESTATION_REQUEST, CMD_CERT_CHAIN_REQUEST,
    };
    use mat_controller::x509::generate_dev_attestation;

    #[test]
    fn attestation_response_passes_verify_device_attestation() {
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let (dac_der, pai_der, paa_der) = (
            dev.dac_der.clone(),
            dev.pai_der.clone(),
            dev.paa_der.clone(),
        );
        let mut server = CommissioningServer::new(dev, FabricStore::new());
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );

        let nonce = [9u8; 32];
        let (response_command, fields_tlv) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ATTESTATION_REQUEST,
            &encode_attestation_request(&nonce),
        ));
        assert_eq!(response_command, RESP_ATTESTATION);
        let (elements, signature) = decode_attestation_response(&fields_tlv).unwrap();

        verify_device_attestation(
            &dac_der,
            &pai_der,
            std::slice::from_ref(&paa_der),
            &[],
            &elements,
            &signature,
            &nonce,
            &TEST_CHALLENGE,
        )
        .unwrap();
    }

    #[test]
    fn cert_chain_request_returns_dac_and_pai_der() {
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let (dac_der, pai_der) = (dev.dac_der.clone(), dev.pai_der.clone());
        let mut server = CommissioningServer::new(dev, FabricStore::new());

        let (_, resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            &encode_cert_chain_request(CERT_TYPE_DAC),
        ));
        assert_eq!(
            mat_controller::commissioning::decode_cert_chain_response(&resp).unwrap(),
            dac_der
        );

        let (_, resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            &encode_cert_chain_request(CERT_TYPE_PAI),
        ));
        assert_eq!(
            mat_controller::commissioning::decode_cert_chain_response(&resp).unwrap(),
            pai_der
        );
    }
}
