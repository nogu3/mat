//! BTP/BLE 経由の Thread コミッショニング。
//!
//! --- BTP/BLE Thread commissioning（M6b Task6） ---

use std::sync::Arc;
use std::time::Duration;

use crate::cert::MatterCert;
use crate::dnssd;
use crate::exchange::{MrpConfig, MRP_BACKOFF_JITTER};
use crate::fabric::{self};
use crate::pase;
use crate::transport::{Transport, UdpTransport};

use super::{
    check_commissioning_response, decode_connect_network_response, decode_network_config_response,
    encode_add_or_update_thread_network, encode_arm_fail_safe, encode_connect_network, fields_of,
    operational_case_and_complete, run_credential_steps, thread_ext_pan_id, CommissionError,
    CommissionedDevice, CommissioningFabric, CLUSTER_GENERAL_COMMISSIONING,
    CLUSTER_NETWORK_COMMISSIONING, CMD_ADD_OR_UPDATE_THREAD, CMD_ARM_FAIL_SAFE,
    CMD_CONNECT_NETWORK,
};

/// `commission_ble_thread` / `commission_btp_thread` の入力一式。
pub struct BleThreadParams<'a> {
    pub passcode: u32,
    pub discriminator: u16,
    /// OTBR の active operational dataset（Thread TLV 生バイト列）。
    pub thread_dataset: &'a [u8],
    pub device_node_id: u64,
    pub paa_dir: Option<&'a std::path::Path>,
    pub cd_signer_dir: Option<&'a std::path::Path>,
    /// Thread 参加後の operational mDNS 発見に使う iface index。
    pub scope_id: u32,
}

/// BTP リンク上で工場出荷デバイスを commission する（spec M6b 決定 3）。
///
/// リンク（[`crate::btp::GattLink`]）は確立済みで渡される——BLE スキャン /
/// 接続は `commission_ble_thread`（feature "ble"）が行う。この関数自体は
/// cfg なし・モックの `GattLink` だけでテストできる（`tests/btp_pase_plumbing.rs`
/// が実際に PASE の最初のメッセージまで通す）。
pub async fn commission_btp_thread(
    link: crate::btp::GattLink,
    fabric: &CommissioningFabric,
    params: BleThreadParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    // BLE devices run SPAKE2+/attestation P-256 math on a constrained MCU and
    // answer slowly — a factory-fresh Nanoleaf takes ~4s per PASE round. On the
    // reliable (BTP) transport `send_reliable` waits `total_budget(cfg)` for the
    // response, so the default ~4.7s budget times out on the final Pake3/status
    // step (observed live). Use a generous per-step budget; a fast response
    // still returns immediately, this only raises the ceiling before we give up.
    let cfg = MrpConfig {
        initial_interval: Duration::from_secs(15),
        // PASE 中の相手は計算で長考する。active 判定で間隔を縮めず、意図した
        // 長 budget を保つ（この cfg は「応答待ち予算」として使っている）。
        active_interval: Duration::from_secs(15),
        max_retries: 1,
        backoff: 1.0,
        jitter: MRP_BACKOFF_JITTER,
    };
    let xpan = thread_ext_pan_id(params.thread_dataset).ok_or(CommissionError::Malformed {
        step: "thread-dataset",
        detail: "no extended pan id (type 2) in dataset",
    })?;

    // 1. BTP handshake → Transport::Reliable。
    let (btp_params, transport) = crate::btp::connect(link, crate::btp::PROPOSED_WINDOW)
        .await
        .map_err(|e| CommissionError::Ble {
            step: "btp-handshake",
            detail: e.to_string(),
        })?;
    tracing::info!(
        segment = btp_params.segment_size,
        window = btp_params.window_size,
        "btp session established"
    );
    let transport = Arc::new(transport);

    // 2. PASE over BTP（Reliable transport 上では MRP は自動 off——
    //    宛先は擬似 peer の `transport::RELIABLE_PEER`）。
    let mut pase = pase::establish(
        Arc::clone(&transport),
        crate::transport::RELIABLE_PEER,
        params.passcode,
        &cfg,
    )
    .await
    .map_err(CommissionError::Pase)?;

    // 3〜7. 資格情報ステップ（on-network commissioning と共通）。
    let fabric_index = run_credential_steps(
        &mut pase,
        fabric,
        params.device_node_id,
        params.paa_dir,
        params.cd_signer_dir,
        &cfg,
    )
    .await?;

    // 4. Thread dataset 書き込み（AddOrUpdateThreadNetwork, breadcrumb=5）。
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_NETWORK_COMMISSIONING,
            CMD_ADD_OR_UPDATE_THREAD,
            Some(&encode_add_or_update_thread_network(
                params.thread_dataset,
                5,
            )),
            None,
            &cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (status, text) = decode_network_config_response(fields_of("add-thread-network", &resp)?)?;
    if status != 0 {
        return Err(CommissionError::NetworkConfig {
            step: "add-thread-network",
            status,
            debug_text: text,
        });
    }

    // 5. failsafe 仕切り直し（spec 決定 5: Thread 参加 + operational 発見が
    //    120s を超えないよう ConnectNetwork 直前に再アーム、breadcrumb=6）。
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 6)),
            None,
            &cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    check_commissioning_response("re-arm-fail-safe", &resp)?;

    // 6. ConnectNetwork（Thread join は遅い——応答待ちだけ長い budget で。
    //    breadcrumb=7）。
    let connect_cfg = MrpConfig {
        initial_interval: Duration::from_secs(60),
        active_interval: Duration::from_secs(60), // Thread join 待ち予算を保つ
        max_retries: 0,
        backoff: 1.0,
        jitter: MRP_BACKOFF_JITTER,
    };
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_NETWORK_COMMISSIONING,
            CMD_CONNECT_NETWORK,
            Some(&encode_connect_network(&xpan, 7)),
            None,
            &connect_cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (status, text) = decode_connect_network_response(fields_of("connect-network", &resp)?)?;
    if status != 0 {
        return Err(CommissionError::NetworkConfig {
            step: "connect-network",
            status,
            debug_text: text,
        });
    }

    // 7. BTP 経路を手放す（BleConnection の切断は呼び出し側の責務）。以後は IP。
    drop(pase);
    drop(transport);

    // 8. operational 発見（リトライ）→ CASE → CommissioningComplete。
    let rcac = MatterCert::parse(&fabric.rcac_tlv)?;
    let cfid = fabric::compressed_fabric_id(&rcac.pub_key, fabric.fabric_id);
    let mut resolved = None;
    let mut last_err = None;
    for _ in 0..12 {
        match dnssd::resolve_operational(
            params.scope_id,
            &cfid,
            params.device_node_id,
            Duration::from_secs(5),
        )
        .await
        {
            Ok(node) => {
                resolved = Some(node);
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    let node = resolved.ok_or_else(|| {
        tracing::warn!(error = ?last_err, "operational advertise did not appear");
        CommissionError::Timeout("operational discovery after thread join")
    })?;
    let addr = node
        .socket_addrs(params.scope_id)
        .into_iter()
        .next()
        .ok_or(CommissionError::Timeout("no usable operational address"))?;
    let udp = UdpTransport::bind()
        .await
        .map_err(|e| CommissionError::Ble {
            step: "udp-bind",
            detail: e.to_string(),
        })?;
    let session = operational_case_and_complete(
        Arc::new(Transport::Udp(Arc::new(udp))),
        addr,
        fabric,
        params.device_node_id,
        &node.mrp_config(),
    )
    .await?;

    Ok(CommissionedDevice {
        node_id: params.device_node_id,
        fabric_index,
        session,
    })
}

/// BLE スキャンから完了までの一括フロー（実機用）。scan → GATT 接続 →
/// [`commission_btp_thread`] へ委譲 → 成否に関わらず GATT を切断する
/// （[`crate::ble::BleConnection`] は drop だけでは切断されない——ドキュメ
/// ント参照）。そのため結果は切断より前に確保しておく。
#[cfg(feature = "ble")]
pub async fn commission_ble_thread(
    fabric: &CommissioningFabric,
    params: BleThreadParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    let ble_err = |step: &'static str| {
        move |e: crate::btp::BtpError| CommissionError::Ble {
            step,
            detail: e.to_string(),
        }
    };
    let session = bluer::Session::new()
        .await
        .map_err(|e| CommissionError::Ble {
            step: "bluez-session",
            detail: e.to_string(),
        })?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(|e| CommissionError::Ble {
            step: "adapter",
            detail: e.to_string(),
        })?;
    let device =
        crate::ble::find_commissionable(&adapter, params.discriminator, Duration::from_secs(30))
            .await
            .map_err(ble_err("scan"))?;
    let (link, conn) = crate::ble::open_link(&device)
        .await
        .map_err(ble_err("gatt"))?;
    let result = commission_btp_thread(link, fabric, params).await;
    conn.disconnect().await; // 成否に関わらず GATT を畳む
    result
}
