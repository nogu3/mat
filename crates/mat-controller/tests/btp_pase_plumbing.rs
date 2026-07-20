//! BTP → exchange → PASE の配管貫通テスト（実 BLE なし）。
//!
//! fake BTP peripheral の上で pase::establish を走らせ、
//! (1) PBKDFParamRequest が BTP フレームとして届くこと、
//! (2) R フラグ（MRP）が立っていないこと、
//! (3) peripheral が不正応答を返すと PaseError で終わること、を確認する。
//!
//! Task 3 の btp.rs 内部テストにある FakePeripheral と同型のヘルパをここで
//! 再掲する——tests/ は別クレートなので btp.rs の `#[cfg(test)]` ヘルパは
//! 使えない（M6b Task6 の brief どおり）。

use std::sync::Arc;
use std::time::Duration;

use mat_controller::btp::{self, GattLink, Packet, Reassembler, SegmentPos};
use mat_controller::exchange::MrpConfig;
use mat_controller::{pase, transport};

#[tokio::test]
async fn pase_over_btp_sends_unreliable_pbkdf_request() {
    let (wtx, mut wrx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (itx, irx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let link = GattLink {
        writes: wtx,
        indications: irx,
    };

    let peripheral = tokio::spawn(async move {
        // handshake
        let req = wrx.recv().await.expect("handshake request");
        assert_eq!(req[0], 0x65);
        itx.send(vec![0x65, 0x6C, 4, 244, 0, 4]).await.unwrap();
        // PBKDFParamRequest を再構成
        let mut reasm = Reassembler::new();
        let msg = loop {
            let frame = wrx.recv().await.expect("frame");
            let pkt = Packet::decode(&frame).unwrap();
            if let Some(m) = reasm.push(&pkt).unwrap() {
                break m;
            }
        };
        // Matter message header を素で解いて R フラグ無しを確認
        use mat_controller::message::{MessageHeader, ProtocolHeader};
        let (h, off) = MessageHeader::decode(&msg).unwrap();
        assert_eq!(h.session_id, 0);
        let (p, _) = ProtocolHeader::decode(&msg[off..]).unwrap();
        assert!(!p.needs_ack, "MRP must be off over BTP");
        assert_eq!(
            p.opcode,
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
                exchange_id: p.exchange_id,
                protocol_id: p.protocol_id,
                vendor_id: None,
            };
            let mut b = rh.encoded();
            rp.encode(&mut b);
            b
        };
        // 1 フレームで送る（BTP data packet, seq=1 相当は fake 側管理: seq 0 は
        // まだ使っていないので 0 から）
        let frame = btp::encode_data_packet(
            0,
            None,
            SegmentPos::First { ending: true },
            Some(reply.len() as u16),
            &reply,
        );
        itx.send(frame).await.unwrap();
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
        },
    )
    .await;
    // `Ok` 側の `SecureSession` は `Debug` 非実装なので `unwrap_err`/`expect_err`
    // は使えない——`is_err` で判定する。種別は問わない（Malformed / StatusReport
    // いずれでも良い。層の貫通が主眼）。
    assert!(result.is_err(), "garbage PBKDFParamResponse must fail");
    peripheral.await.unwrap();
}
