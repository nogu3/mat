//! on-network コミッショニングのステップマシン（PASE → 資格情報ステップ → CASE → Complete）。
//!
//! --- ステップマシン（Task 10） ---
//!
//! 順序: ターゲット解決 → PASE → ArmFailSafe(必須) → SetRegulatoryConfig(任
//! 意、失敗は warn で続行) → attestation(厳格) → CSR → NOC 発行 →
//! AddTrustedRootCertificate → AddNOC → 新 fabric で CASE(リトライ) →
//! CommissioningComplete。failsafe は明示 disarm しない——失敗時は 120s の
//! 期限切れに任せる（spec 決定 6: 中断ハンドラを持たない一発フロー）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::attestation;
use crate::case;
use crate::crypto;
use crate::dnssd;
use crate::exchange::MrpConfig;
use crate::im;
use crate::pase;
use crate::session::SecureSession;
use crate::transport::{Transport, UdpTransport};
use crate::x509;

use super::{
    decode_attestation_response, decode_cert_chain_response, decode_commissioning_status_response,
    decode_csr_response, decode_noc_response, encode_add_noc, encode_add_trusted_root,
    encode_arm_fail_safe, encode_attestation_request, encode_cert_chain_request,
    encode_csr_request, encode_set_regulatory_config, parse_nocsr_elements, CommissionError,
    CommissioningFabric, CERT_TYPE_DAC, CERT_TYPE_PAI, CLUSTER_GENERAL_COMMISSIONING,
    CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_NOC, CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE,
    CMD_ATTESTATION_REQUEST, CMD_CERT_CHAIN_REQUEST, CMD_COMMISSIONING_COMPLETE, CMD_CSR_REQUEST,
    CMD_SET_REGULATORY_CONFIG,
};

/// commissioning 対象デバイスの指定方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommissionTarget {
    /// アドレス既知（ローカル E2E、または呼び出し側が別途探索済み）。
    Addr(SocketAddr),
    /// `_matterc` browse で long discriminator から探索する。
    Discriminator(u16),
}

/// `commission_on_network` の入力一式。
pub struct CommissionParams<'a> {
    pub passcode: u32,
    pub target: CommissionTarget,
    pub device_node_id: u64,
    /// PAA 信頼ストアのディレクトリ（`*.der`）。`None` は「PAA なし」——
    /// attestation チェーン検証は必ず失敗する（PAA 必須運用、警告なしで
    /// 弱めない）。
    pub paa_dir: Option<&'a std::path::Path>,
    /// CD signer 証明書ストアのディレクトリ。CD 検証は warn のみ（spec
    /// 決定どおり戻り値には影響しない）ので `None` でも commissioning 自体
    /// は続行できる。
    pub cd_signer_dir: Option<&'a std::path::Path>,
    /// mDNS / link-local アドレス用の interface index。`Addr` 直指定なら
    /// リンクローカルでなければ `0` で構わない。
    pub scope_id: u32,
}

/// commissioning 完了後のデバイス。
pub struct CommissionedDevice {
    pub node_id: u64,
    pub fabric_index: Option<u8>,
    /// 新 fabric 上の operational CASE セッション（CommissioningComplete
    /// 送信済み）。呼び出し側はこれをそのまま以後の操作に使い回せる。
    pub session: SecureSession,
}

/// `InvokeResponseData` から command fields TLV を取り出す。応答の
/// `status` が非ゼロならその時点で `CommandStatus` に、fields が無ければ
/// `Malformed` にする。
pub(super) fn fields_of<'a>(
    step: &'static str,
    resp: &'a im::InvokeResponseData,
) -> Result<&'a [u8], CommissionError> {
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus {
            step,
            code: resp.status,
        });
    }
    resp.fields_tlv
        .as_deref()
        .ok_or(CommissionError::Malformed {
            step,
            detail: "no command fields",
        })
}

/// `{0: errorCode, 1: debugText}` 型（ArmFailSafeResponse などの共通形）の
/// 応答を検査し、`errorCode` が非ゼロなら `CommandStatus` にする。
pub(super) fn check_commissioning_response(
    step: &'static str,
    resp: &im::InvokeResponseData,
) -> Result<(), CommissionError> {
    let (code, _text) = decode_commissioning_status_response(fields_of(step, resp)?)?;
    if code != 0 {
        return Err(CommissionError::CommandStatus { step, code });
    }
    Ok(())
}

/// CertificateChainRequest を送って証明書 DER を取り出す（DAC / PAI 共
/// 通、`cert_type` で切り替え）。
async fn request_cert(
    step: &'static str,
    session: &mut SecureSession,
    cert_type: u8,
    cfg: &MrpConfig,
) -> Result<Vec<u8>, CommissionError> {
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            Some(&encode_cert_chain_request(cert_type)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    decode_cert_chain_response(fields_of(step, &resp)?)
}

/// 共有ステップ 3〜7（ArmFailSafe → SetRegulatoryConfig(任意) →
/// attestation(厳格) → CSR → NOC 発行 → AddTrustedRootCertificate →
/// AddNOC）。`commission_on_network`（PASE over UDP）と
/// `commission_btp_thread`（PASE over BTP）の両方から呼ばれる — セッション
/// が UDP か Reliable(BTP) かに関わらず同一（`SecureSession` はどちらの
/// transport の上でも同じ invoke インタフェースを持つ）。戻り値は AddNOC が
/// 返した fabric index（spec 上 optional）。
pub(super) async fn run_credential_steps(
    session: &mut SecureSession,
    fabric: &CommissioningFabric,
    device_node_id: u64,
    paa_dir: Option<&std::path::Path>,
    cd_signer_dir: Option<&std::path::Path>,
    cfg: &MrpConfig,
) -> Result<Option<u8>, CommissionError> {
    let challenge = session.attestation_challenge();

    // 3. ArmFailSafe(120s)（必須）。
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 1)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    check_commissioning_response("arm-fail-safe", &resp)?;

    // 4. SetRegulatoryConfig（任意 — spec 決定 7: 失敗は warn で続行）。
    match session
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_SET_REGULATORY_CONFIG,
            Some(&encode_set_regulatory_config(2, "XX", 2)),
            None,
            cfg,
        )
        .await
    {
        Ok(resp) => {
            if let Err(e) = check_commissioning_response("set-regulatory", &resp) {
                tracing::warn!(error = %e, "SetRegulatoryConfig rejected — continuing");
            }
        }
        Err(e) => tracing::warn!(error = %e, "SetRegulatoryConfig failed — continuing"),
    }

    // 5. attestation（厳格）。
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).expect("os rng");
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ATTESTATION_REQUEST,
            Some(&encode_attestation_request(&nonce)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (elements, att_sig) = decode_attestation_response(fields_of("attestation", &resp)?)?;
    let dac = request_cert("dac", session, CERT_TYPE_DAC, cfg).await?;
    let pai = request_cert("pai", session, CERT_TYPE_PAI, cfg).await?;
    let paa = match paa_dir {
        Some(d) => attestation::load_der_dir(d).map_err(CommissionError::Attestation)?,
        None => Vec::new(), // 空 → チェーン検証は必ず失敗する（PAA 必須運用）
    };
    let cd_signers = match cd_signer_dir {
        Some(d) => attestation::load_der_dir(d).map_err(CommissionError::Attestation)?,
        None => Vec::new(),
    };
    attestation::verify_device_attestation(
        &dac,
        &pai,
        &paa,
        &cd_signers,
        &elements,
        &att_sig,
        &nonce,
        &challenge,
    )
    .map_err(CommissionError::Attestation)?;

    // 6. CSR → NOC 発行。
    let mut csr_nonce = [0u8; 32];
    getrandom::fill(&mut csr_nonce).expect("os rng");
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            Some(&encode_csr_request(&csr_nonce)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (nocsr_elements, nocsr_sig) = decode_csr_response(fields_of("csr", &resp)?)?;
    // NOCSR 署名も DAC 鍵で elements||challenge に対して（spec §11.17.5.6）。
    {
        let dac_cert = x509::parse_x509(&dac).map_err(|_| CommissionError::Csr("dac reparse"))?;
        let msg = attestation::attestation_tbs(&nocsr_elements, &challenge);
        crypto::verify_ecdsa_p256(&dac_cert.public_key, &msg, &nocsr_sig)
            .map_err(|_| CommissionError::Csr("nocsr signature"))?;
    }
    let (csr_der, returned_nonce) = parse_nocsr_elements(&nocsr_elements)?;
    if returned_nonce != csr_nonce {
        return Err(CommissionError::Csr("csr nonce mismatch"));
    }
    let device_pub = x509::parse_csr(&csr_der).map_err(|_| CommissionError::Csr("csr parse"))?;
    let noc_tlv = fabric.issue_device_noc(&device_pub, device_node_id)?;

    // 7. AddTrustedRootCertificate → AddNOC。
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_TRUSTED_ROOT,
            Some(&encode_add_trusted_root(&fabric.rcac_tlv)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus {
            step: "add-trusted-root",
            code: resp.status,
        });
    }
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            Some(&encode_add_noc(
                &noc_tlv,
                &fabric.ipk_epoch,
                fabric.admin_node_id,
                0xFFF1,
            )),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (noc_status, fabric_index) = decode_noc_response(fields_of("add-noc", &resp)?)?;
    if noc_status != 0 {
        return Err(CommissionError::Noc(noc_status));
    }

    Ok(fabric_index)
}

/// 共有ステップ 8〜9（新 fabric で CASE（リトライ）→ CommissioningComplete）。
/// `commission_on_network` / `commission_btp_thread` の双方とも、資格情報
/// ステップ完了後は同一アドレスへ operational discovery 抜きで直接 CASE を
/// 試みる（PASE と同一アドレスなら再解決不要。BTP 経由の場合は呼び出し側
/// が mDNS operational discovery 済みのアドレスを渡す）。
///
/// 実装ノート（brief からの適応）: brief は第 1 引数を `Arc<UdpTransport>`
/// と書いていたが、`case::establish` 自体が `Arc<Transport>` を取るため
/// ここでも `Arc<Transport>` を直接受ける — 呼び出し側で
/// `Arc<UdpTransport>` を都度 `Transport::Udp` に包んでから渡す一段階の
/// 手間を省く。戻り値も `(SecureSession, ())` ではなく `SecureSession` のみ
/// ——fabric index は `run_credential_steps` の戻り値であり、呼び出し側が
/// 組み立てる `CommissionedDevice` の方で合流させる。
pub(super) async fn operational_case_and_complete(
    transport: Arc<Transport>,
    peer: SocketAddr,
    fabric: &CommissioningFabric,
    device_node_id: u64,
    cfg: &MrpConfig,
) -> Result<SecureSession, CommissionError> {
    // 8. 新 fabric で CASE（同一アドレスへ直接。AddNOC 直後は fabric 起動待
    //    ちが必要なことがあるためリトライ、全体 ~30s / failsafe 120s 内）。
    let creds = fabric.admin_credentials()?;
    let mut session = None;
    let mut last = None;
    for _ in 0..6 {
        match case::establish(Arc::clone(&transport), peer, &creds, device_node_id, cfg).await {
            Ok(s) => {
                session = Some(s);
                break;
            }
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    let mut session =
        session.ok_or_else(|| CommissionError::Case(last.expect("at least one try")))?;

    // 9. CommissioningComplete（CASE 上で）。
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            None,
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    check_commissioning_response("commissioning-complete", &resp)?;

    Ok(session)
}

/// on-network commissioning のステップマシン本体（spec §5.5 全体フロー）。
///
/// `fabric` はこのコミッショニングだけで使い捨てる第二 fabric（呼び出し側
/// が事前に [`CommissioningFabric::generate`] しておく）。成功すると新
/// fabric 上の CASE セッションを持つ [`CommissionedDevice`] を返す。
pub async fn commission_on_network(
    transport: Arc<UdpTransport>,
    fabric: &CommissioningFabric,
    params: CommissionParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    // M6b: 内部の pase/case は `Arc<Transport>` を取る（BTP 対応の土台）。
    // 公開シグネチャは既存呼び出し側（M6a）互換のため `Arc<UdpTransport>` の
    // まま維持し、ここで一度だけ wrap する。
    let transport: Arc<Transport> = Arc::new(Transport::Udp(Arc::clone(&transport)));

    // 1. ターゲット解決。
    let (peer, cfg) = match params.target {
        CommissionTarget::Addr(a) => (a, MrpConfig::default()),
        CommissionTarget::Discriminator(d) => {
            let node = dnssd::resolve_commissionable(params.scope_id, d, Duration::from_secs(15))
                .await
                .map_err(CommissionError::Discovery)?;
            let addr = node
                .socket_addrs(params.scope_id)
                .into_iter()
                .next()
                .ok_or(CommissionError::Timeout("no usable address"))?;
            (addr, node.mrp_config())
        }
    };

    // 2. PASE。
    let mut pase = pase::establish(Arc::clone(&transport), peer, params.passcode, &cfg)
        .await
        .map_err(CommissionError::Pase)?;

    // 3〜7. 資格情報ステップ（run_credential_steps に集約——M6b Task6）。
    let fabric_index = run_credential_steps(
        &mut pase,
        fabric,
        params.device_node_id,
        params.paa_dir,
        params.cd_signer_dir,
        &cfg,
    )
    .await?;

    // 8〜9. CASE リトライ + CommissioningComplete（operational_case_and_complete
    // に集約——M6b Task6）。
    let session = operational_case_and_complete(
        Arc::clone(&transport),
        peer,
        fabric,
        params.device_node_id,
        &cfg,
    )
    .await?;

    Ok(CommissionedDevice {
        node_id: params.device_node_id,
        fabric_index,
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::super::encode_commissioning_status_response;
    use super::*;

    #[test]
    fn fields_of_maps_status_and_missing_fields() {
        let bad_status = im::InvokeResponseData {
            status: 0x85,
            cluster_status: None,
            fields_tlv: None,
        };
        match fields_of("step-a", &bad_status) {
            Err(CommissionError::CommandStatus { step, code }) => {
                assert_eq!((step, code), ("step-a", 0x85))
            }
            other => panic!("expected CommandStatus, got {other:?}"),
        }
        let no_fields = im::InvokeResponseData {
            status: 0,
            cluster_status: None,
            fields_tlv: None,
        };
        match fields_of("step-b", &no_fields) {
            Err(CommissionError::Malformed { step, detail }) => {
                assert_eq!((step, detail), ("step-b", "no command fields"))
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        let ok = im::InvokeResponseData {
            status: 0,
            cluster_status: None,
            fields_tlv: Some(vec![0x15, 0x18]),
        };
        assert_eq!(fields_of("step-c", &ok).unwrap(), &[0x15, 0x18]);
    }

    #[test]
    fn check_commissioning_response_rejects_nonzero_error_code() {
        let resp = im::InvokeResponseData {
            status: 0,
            cluster_status: None,
            fields_tlv: Some(encode_commissioning_status_response(3, "busy")),
        };
        match check_commissioning_response("arm", &resp) {
            Err(CommissionError::CommandStatus { step, code }) => {
                assert_eq!((step, code), ("arm", 3))
            }
            other => panic!("expected CommandStatus, got {other:?}"),
        }
        let ok = im::InvokeResponseData {
            status: 0,
            cluster_status: None,
            fields_tlv: Some(encode_commissioning_status_response(0, "")),
        };
        assert!(check_commissioning_response("arm", &ok).is_ok());
    }
}
