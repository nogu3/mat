//! BLE central adapter (bluer / BlueZ) — feature "ble".
//!
//! Matter commissionable の発見（0xFFF6 service data）と、BTP が使う
//! GATT C1/C2 を GattLink（チャネル対）へ橋渡しする。プロトコルは一切
//! 持たない——BTP は btp.rs、その上の Matter は既存層。

use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;

// bluer は uuid crate (v1) を再エクスポートしている（bluer::Uuid ==
// uuid::Uuid）ので、別途 uuid crate に依存しない。
use bluer::Uuid;

use crate::btp::{BtpError, GattLink};

/// Matter BLE service（16-bit alias 0xFFF6）。
pub const MATTER_BLE_SERVICE: Uuid = Uuid::from_u128(0x0000FFF6_0000_1000_8000_00805F9B34FB);
/// BTP C1（client→server write）。
pub const BTP_C1: Uuid = Uuid::from_u128(0x18EE2EF5_263D_4559_959F_4F9C429F9D11);
/// BTP C2（server→client indication）。
pub const BTP_C2: Uuid = Uuid::from_u128(0x18EE2EF5_263D_4559_959F_4F9C429F9D12);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatterAdvert {
    pub discriminator: u16,
    pub vendor_id: u16,
    pub product_id: u16,
}

/// Matter commissionable advertisement の service data（spec §4.17.3.2、
/// opcode 0x00 / 8 バイト）を解釈する。cfg なしの純関数（feature off でも
/// テストは走らないが、bluer 型に依存しないためロジックだけなら移植可能）。
pub fn parse_matter_service_data(sd: &[u8]) -> Option<MatterAdvert> {
    if sd.len() < 8 || sd[0] != 0x00 {
        return None;
    }
    Some(MatterAdvert {
        discriminator: u16::from_le_bytes([sd[1], sd[2]]) & 0x0FFF,
        vendor_id: u16::from_le_bytes([sd[3], sd[4]]),
        product_id: u16::from_le_bytes([sd[5], sd[6]]),
    })
}

fn gatt(step: &'static str) -> impl FnOnce(bluer::Error) -> BtpError {
    move |e| BtpError::Gatt(format!("{step}: {e}"))
}

/// discriminator 一致の commissionable デバイスをスキャンする。
pub async fn find_commissionable(
    adapter: &bluer::Adapter,
    discriminator: u16,
    timeout: Duration,
) -> Result<bluer::Device, BtpError> {
    adapter.set_powered(true).await.map_err(gatt("power"))?;
    let mut events = adapter.discover_devices().await.map_err(gatt("discover"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(BtpError::Timeout("ble scan"));
        }
        let Ok(Some(ev)) = tokio::time::timeout(remaining, events.next()).await else {
            return Err(BtpError::Timeout("ble scan"));
        };
        let bluer::AdapterEvent::DeviceAdded(addr) = ev else {
            continue;
        };
        let Ok(device) = adapter.device(addr) else {
            continue;
        };
        let Ok(Some(sd)) = device.service_data().await else {
            continue;
        };
        let Some(bytes) = sd.get(&MATTER_BLE_SERVICE) else {
            continue;
        };
        if let Some(adv) = parse_matter_service_data(bytes) {
            tracing::info!(
                %addr,
                disc = adv.discriminator,
                vid = adv.vendor_id,
                pid = adv.product_id,
                "matter commissionable found"
            );
            if adv.discriminator == discriminator {
                return Ok(device);
            }
        }
    }
}

/// 接続済み GATT リンク。**必ず明示的に `disconnect()` を呼ぶこと** —
/// 素の drop は転送タスクのハンドルを手放すだけで、GATT 接続は残る
/// （tokio の JoinHandle drop はタスクを abort しない）。
pub struct BleConnection {
    device: bluer::Device,
    writer: tokio::task::JoinHandle<()>,
}

impl BleConnection {
    pub async fn disconnect(self) {
        self.writer.abort();
        let _ = self.device.disconnect().await;
    }
}

/// GATT 接続して BTP 用の GattLink を開く。
///
/// BTP spec §4.19.5 の順序保証: 「handshake request を C1 に write して
/// から C2 を subscribe する」。writer task は最初の 1 write（= btp::connect
/// が送る handshake request）を完了させた後に C2 の indication pump を開始
/// することでこの順序を構造的に守る。
pub async fn open_link(device: &bluer::Device) -> Result<(GattLink, BleConnection), BtpError> {
    if !device.is_connected().await.map_err(gatt("is_connected"))? {
        // BlueZ routinely aborts the first LE connection right after a scan
        // ("le-connection-abort-by-local") because StopDiscovery and Connect
        // race. The abort is transient — retry a few times before giving up.
        let mut last: Option<bluer::Error> = None;
        for attempt in 0..6u32 {
            match device.connect().await {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "ble connect failed — retrying");
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(700)).await;
                }
            }
        }
        if let Some(e) = last {
            return Err(BtpError::Gatt(format!("connect: {e}")));
        }
    }
    let mut c1 = None;
    let mut c2 = None;
    for service in device.services().await.map_err(gatt("services"))? {
        if service.uuid().await.map_err(gatt("svc uuid"))? != MATTER_BLE_SERVICE {
            continue;
        }
        for ch in service.characteristics().await.map_err(gatt("chars"))? {
            match ch.uuid().await.map_err(gatt("char uuid"))? {
                u if u == BTP_C1 => c1 = Some(ch),
                u if u == BTP_C2 => c2 = Some(ch),
                _ => {}
            }
        }
    }
    let c1 = c1.ok_or_else(|| BtpError::Gatt("C1 not found".into()))?;
    let c2 = c2.ok_or_else(|| BtpError::Gatt("C2 not found".into()))?;

    let (wtx, mut wrx) = mpsc::channel::<Vec<u8>>(1);
    let (itx, irx) = mpsc::channel::<Vec<u8>>(8);
    let writer = tokio::spawn(async move {
        // 1 通目（handshake request）を write してから subscribe（順序保証）
        let Some(first) = wrx.recv().await else {
            return;
        };
        let req = bluer::gatt::remote::CharacteristicWriteRequest {
            op_type: bluer::gatt::WriteOp::Request,
            ..Default::default()
        };
        if let Err(e) = c1.write_ext(&first, &req).await {
            tracing::warn!(error = %e, "btp handshake write failed");
            return;
        }
        let ind = match c2.notify().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "C2 subscribe failed");
                return;
            }
        };
        let pump = tokio::spawn(async move {
            futures_util::pin_mut!(ind);
            while let Some(v) = ind.next().await {
                tracing::debug!(bytes = v.len(), "gatt C2 indication");
                if itx.send(v).await.is_err() {
                    break;
                }
            }
            tracing::debug!("gatt C2 indication stream ended");
        });
        while let Some(data) = wrx.recv().await {
            let n = data.len();
            if let Err(e) = c1.write_ext(&data, &req).await {
                tracing::warn!(error = %e, "gatt write failed");
                break;
            }
            tracing::debug!(bytes = n, "gatt C1 write ok");
        }
        pump.abort();
    });

    Ok((
        GattLink {
            writes: wtx,
            indications: irx,
        },
        BleConnection {
            device: device.clone(),
            writer,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_matter_commissionable_service_data() {
        // opcode 0x00, disc/adv版=LE16(下位12bit=0xB47), VID=0x125D, PID=0x0055, flags=0
        let sd = [0x00, 0x47, 0x0B, 0x5D, 0x12, 0x55, 0x00, 0x00];
        let c = parse_matter_service_data(&sd).unwrap();
        assert_eq!(c.discriminator, 0x0B47);
        assert_eq!(c.vendor_id, 0x125D);
        assert_eq!(c.product_id, 0x0055);
        // opcode != 0 / 短すぎは None
        assert!(parse_matter_service_data(&[0x01, 0, 0, 0, 0, 0, 0, 0]).is_none());
        assert!(parse_matter_service_data(&[0x00, 0x47]).is_none());
    }
}
