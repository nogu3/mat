//! Live E2E against a local chip-all-clusters-app. Not run in CI.
//!
//! Setup:
//!   task chip:extract:app
//!   ./chip-all-clusters-app          # udp/5540 で待ち受け
//!   task e2e:m1                      # または cargo test ... -- --ignored
//!
//! 宛先は `MAT_E2E_PEER`（例: `[fd00::1]:5540`）で差し替え可能。未設定なら
//! ローカルの `[::1]:5540`。実機（Thread 上の Matter デバイス）にも同じ
//! テストを向けられる — unsecured の reliable メッセージなので fabric や
//! 既存セッションには触れない。

use std::time::Duration;

use mat_controller::exchange::{MrpConfig, UnsecuredExchange};
use mat_controller::message::{MATTER_PORT, PROTOCOL_ID_SECURE_CHANNEL};
use mat_controller::transport::UdpTransport;

/// Secure Channel: CASE Sigma1 opcode.
const OPCODE_CASE_SIGMA1: u8 = 0x30;

#[tokio::test]
#[ignore = "requires a Matter device on udp/5540 (local chip-all-clusters-app or MAT_E2E_PEER)"]
async fn reliable_message_gets_acked_by_real_device() {
    let transport = UdpTransport::bind().await.unwrap();
    let peer = std::env::var("MAT_E2E_PEER")
        .unwrap_or_else(|_| format!("[::1]:{MATTER_PORT}"))
        .parse()
        .expect("MAT_E2E_PEER must be a socket address like [fd00::1]:5540");
    eprintln!("peer: {peer}");
    let mut ex = UnsecuredExchange::new(&transport, peer);

    // 中身が TLV として不正な Sigma1。デバイスの CASE ハンドラはパースに失敗して
    // StatusReport を返すが、MRP 層は処理結果と無関係に受信を ACK する。
    // M1 の合格条件は「実デバイスがこちらの reliable メッセージを ACK する」まで。
    let res = ex
        .send_reliable(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_CASE_SIGMA1,
            &[0xDE, 0xAD],
            &MrpConfig::default(),
        )
        .await
        .expect("device must acknowledge our reliable message");

    match res {
        Some(msg) => {
            assert_eq!(msg.proto.protocol_id, PROTOCOL_ID_SECURE_CHANNEL);
            eprintln!(
                "device responded: SC opcode 0x{:02X}, {} byte payload",
                msg.proto.opcode,
                msg.payload.len()
            );
            // 追加応答が reliable で来た場合の後始末（ACK は screen 内で送信済み）
        }
        None => eprintln!("device sent a standalone ack"),
    }

    // 少し待って、遅れて届く応答（standalone ack 後の StatusReport）も観測して
    // ACK を返しておく。失敗しても M1 合格条件には影響しない。
    if let Ok(late) = ex.recv(Duration::from_millis(800)).await {
        eprintln!("late response: SC opcode 0x{:02X}", late.proto.opcode);
    }
}
