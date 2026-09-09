//! open-window（Administrator Commissioning の OpenCommissioningWindow 発行と setup code 生成）。
//!
//! --- open-window（Task 10） ---

use crate::exchange::MrpConfig;
use crate::session::SecureSession;

use super::{
    encode_open_commissioning_window, CommissionError, CLUSTER_ADMIN_COMMISSIONING,
    CMD_OPEN_COMMISSIONING_WINDOW,
};

/// spec §5.1.3.1 の「trivial/attack-prone」として禁止される setup passcode
/// の一覧。native window open では毎回ランダムに引き直して避ける。
pub const INVALID_PASSCODES: [u32; 12] = [
    0, 11111111, 22222222, 33333333, 44444444, 55555555, 66666666, 77777777, 88888888, 99999999,
    12345678, 87654321,
];

/// `1..=99_999_998` の範囲かつ [`INVALID_PASSCODES`] に含まれない passcode
/// を引き直しながら返す。
fn random_valid_passcode() -> u32 {
    loop {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).expect("os rng");
        let candidate = u32::from_le_bytes(b) % 99_999_998 + 1; // 1..=99_999_998
        if !INVALID_PASSCODES.contains(&candidate) {
            return candidate;
        }
    }
}

/// 12-bit discriminator を乱数で引く。`open_commissioning_window` の呼び出し
/// 元（CLI 未指定時の補完、ライブ E2E ハーネス）が使う共通ヘルパー
/// （M8a Task8: discriminator は呼び出し側指定になったため、旧来の内部乱数
/// 生成をここへ切り出した——挙動は不変）。
pub fn random_discriminator() -> u16 {
    let mut disc_b = [0u8; 2];
    getrandom::fill(&mut disc_b).expect("os rng");
    u16::from_le_bytes(disc_b) & 0x0FFF
}

/// [`open_commissioning_window`] が生成した一時 window の設定コード一式。
pub struct OpenedWindow {
    pub passcode: u32,
    /// 12-bit discriminator。
    pub discriminator: u16,
    pub manual_code: String,
    pub qr_payload: String,
    pub window_timeout_s: u16,
}

/// open-window 引数の値域検証。iterations は PASE と同じ spec §3.9 範囲、
/// discriminator は 12-bit。違反は invoke 送信前に InvalidArgument で弾く —
/// 特に discriminator 超過は、デバイスが受理してしまうと window 開放後に
/// setup_code の assert で panic して生成 passcode が失われ、開いた window
/// が放置されるため、送信前に止める順序が重要。
fn validate_window_params(discriminator: u16, iterations: u32) -> Result<(), CommissionError> {
    if discriminator > 0x0FFF {
        return Err(CommissionError::InvalidArgument {
            what: "discriminator must fit in 12 bits (<= 0x0FFF)",
        });
    }
    if !(crate::pase::PBKDF_ITERATIONS_MIN..=crate::pase::PBKDF_ITERATIONS_MAX)
        .contains(&iterations)
    {
        return Err(CommissionError::InvalidArgument {
            what: "iterations must be in 1000..=100000",
        });
    }
    Ok(())
}

/// Enhanced Commissioning Method（spec §5.5）で一時的な commissioning
/// window を開く。既存の operational CASE セッション上で
/// `AdministratorCommissioning::OpenCommissioningWindow` を送る——PASE は使
/// わない（対象デバイスは既にこの fabric にコミッショニング済み）。
///
/// `discriminator` / `iterations` は呼び出し側が指定する（M8a Task8:
/// 直経路 CLI の `--discriminator` / `--iteration` を尊重するため。従来は
/// 内部で乱数 disc / 固定 1000 を生成していた —— その既定値が要る呼び出し元
/// は [`random_discriminator`] を使う）。
pub async fn open_commissioning_window(
    session: &mut SecureSession,
    timeout_s: u16,
    discriminator: u16,
    iterations: u32,
    cfg: &MrpConfig,
) -> Result<OpenedWindow, CommissionError> {
    validate_window_params(discriminator, iterations)?;
    let passcode = random_valid_passcode();
    let mut salt = [0u8; 32];
    getrandom::fill(&mut salt).expect("os rng");
    let verifier = crate::spake2p::compute_verifier(passcode, &salt, iterations);
    // OpenCommissioningWindow は timed invoke 必須（spec §11.19.8.1）。
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            Some(&encode_open_commissioning_window(
                timeout_s,
                &verifier,
                discriminator,
                iterations,
                &salt,
            )),
            Some(10_000),
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus {
            step: "open-window",
            code: resp.status,
        });
    }
    Ok(OpenedWindow {
        passcode,
        discriminator,
        manual_code: build_manual_code(passcode, discriminator),
        qr_payload: build_window_qr(passcode, discriminator),
        window_timeout_s: timeout_s,
    })
}

fn build_manual_code(passcode: u32, discriminator12: u16) -> String {
    crate::setup_code::encode_manual_code(passcode, (discriminator12 >> 8) as u8)
}

fn build_window_qr(passcode: u32, discriminator12: u16) -> String {
    crate::setup_code::encode_qr(&crate::setup_code::SetupPayload {
        version: 0,
        vendor_id: 0, // ECM window の QR は VID/PID 不定で良い
        product_id: 0,
        custom_flow: 0,
        discovery_capabilities: 0x04, // on-network
        discriminator: discriminator12,
        passcode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Task 10: open-window の純関数部分（フロー全体は Task 11 のライブ
    // E2E が検証する — ここでは PASE/CASE を伴わない部分だけを unit test する）。

    #[test]
    fn random_passcode_is_valid() {
        for _ in 0..64 {
            let p = random_valid_passcode();
            assert!((1..=99_999_998).contains(&p));
            assert!(!INVALID_PASSCODES.contains(&p));
        }
    }

    #[test]
    fn opened_window_setup_codes_are_consistent() {
        let w = OpenedWindow {
            passcode: 20202021,
            discriminator: 3840,
            manual_code: build_manual_code(20202021, 3840),
            qr_payload: build_window_qr(20202021, 3840),
            window_timeout_s: 180,
        };
        let m = crate::setup_code::parse_manual_code(&w.manual_code).unwrap();
        assert_eq!(m.passcode, 20202021);
        assert_eq!(u16::from(m.short_discriminator), 3840 >> 8);
        let q = crate::setup_code::parse_qr(&w.qr_payload).unwrap();
        assert_eq!(q.passcode, 20202021);
        assert_eq!(q.discriminator, 3840);
    }

    #[test]
    fn validates_window_params() {
        // 境界値は受理
        assert!(validate_window_params(0x0FFF, 1000).is_ok());
        assert!(validate_window_params(0, 100_000).is_ok());
        // 範囲外は InvalidArgument
        assert!(matches!(
            validate_window_params(0x1000, 1000),
            Err(CommissionError::InvalidArgument { .. })
        ));
        assert!(matches!(
            validate_window_params(0, 999),
            Err(CommissionError::InvalidArgument { .. })
        ));
        assert!(matches!(
            validate_window_params(0, 100_001),
            Err(CommissionError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn random_discriminator_fits_12_bits() {
        for _ in 0..64 {
            assert!(random_discriminator() <= 0x0FFF);
        }
    }
}
