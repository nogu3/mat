//! BTP → exchange → PASE の配管貫通テスト（実 BLE なし）。
//!
//! fake BTP peripheral の上で pase::establish を走らせ、
//! (1) PBKDFParamRequest が BTP フレームとして届くこと、
//! (2) R フラグ（MRP）が立っていないこと、
//! (3) peripheral が不正応答を返すと PaseError で終わること、を確認する。
//!
//! fake peripheral は `btp.rs` の `#[cfg(test)]` と共有の
//! `mat_controller::test_support::btp_fake`（feature `test-responder`）。

use std::sync::Arc;
use std::time::Duration;

use mat_controller::exchange::MrpConfig;
use mat_controller::test_support::btp_fake::fake_link;
use mat_controller::{btp, pase, transport};

#[tokio::test]
async fn pase_over_btp_sends_unreliable_pbkdf_request() {
    let (link, mut p) = fake_link();

    let peripheral = tokio::spawn(async move {
        p.do_handshake(244, 4).await;
        // PBKDFParamRequest を再構成
        let (msg, _seq) = p.recv_message().await;
        // Matter message header を素で解いて R フラグ無しを確認
        use mat_controller::message::{MessageHeader, ProtocolHeader};
        let (h, off) = MessageHeader::decode(&msg).unwrap();
        assert_eq!(h.session_id, 0);
        let (proto, _) = ProtocolHeader::decode(&msg[off..]).unwrap();
        assert!(!proto.needs_ack, "MRP must be off over BTP");
        assert_eq!(
            proto.opcode,
            pase::OPCODE_PBKDF_PARAM_REQUEST,
            "PBKDFParamRequest opcode"
        );
        // 不正応答（ゴミ TLV の PBKDFParamResponse）を返して abort させる
        // → establish 側は PaseError で終了するはず。壊れ方は問わないので
        //   opcode 0x21 + 空 payload を返す。
        let reply = {
            let rh = MessageHeader {
                session_id: 0,
                security_flags: 0,
                message_counter: 1,
                source_node_id: None,
                destination: mat_controller::message::Destination::None,
            };
            let rp = ProtocolHeader {
                initiator: false,
                needs_ack: false,
                acked_counter: None,
                opcode: pase::OPCODE_PBKDF_PARAM_RESPONSE,
                exchange_id: proto.exchange_id,
                protocol_id: proto.protocol_id,
                vendor_id: None,
            };
            let mut b = rh.encoded();
            rp.encode(&mut b);
            b
        };
        // 1 フレームで送る（BTP data packet, handshake response が seq 0 を
        // 暗黙消費するため、データフレームは seq 1 から）
        p.send_message(&reply, 244, None).await;
    });

    let (_params, t) = btp::connect(link, btp::PROPOSED_WINDOW).await.unwrap();
    let result = pase::establish(
        Arc::new(t),
        transport::RELIABLE_PEER,
        20202021,
        &MrpConfig {
            initial_interval: Duration::from_millis(200),
            active_interval: Duration::from_millis(200),
            max_retries: 1,
            backoff: 1.0,
            jitter: 0.0,
        },
    )
    .await;
    // `Ok` 側の `SecureSession` は `Debug` 非実装なので `unwrap_err`/`expect_err`
    // は使えない——`is_err` で判定する。種別は問わない（Malformed / StatusReport
    // いずれでも良い。層の貫通が主眼）。
    assert!(result.is_err(), "garbage PBKDFParamResponse must fail");
    peripheral.await.unwrap();
}
