//! 購読（matd の常駐 Subscribe）: `subscribe_wildcard` のハンドシェイクと
//! priming、`next_subscription_report` のデバイス発 ReportData / keepalive の
//! pump。

use std::time::Duration;

use tokio::time::Instant;

use crate::exchange::MrpConfig;
use crate::message::{OPCODE_MRP_STANDALONE_ACK, PROTOCOL_ID_SECURE_CHANNEL};
use crate::transport::MAX_DATAGRAM;

use super::client::MAX_REPORT_CHUNKS;
use super::mrp::ScreenFilter;
use super::{SecureSession, SessionError, IM_RECV_TIMEOUT};

/// デコード失敗 payload の先頭を hex で（未知エンコーディングの事後診断用、
/// debug ログ専用）。
fn payload_head_hex(payload: &[u8]) -> String {
    payload
        .iter()
        .take(64)
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl SecureSession {
    /// Subscribe を張る（`clusters` 空 = full wildcard、非空 = クラスタ絞り込み）。
    /// spec §8.10、v1: attribute report のみ。
    /// priming ReportData（分割対応、各チャンクに StatusResponse(0) 応答）→
    /// SubscribeResponse 受信で成立。priming の中身も返す（matd が priming=true
    /// イベントとして流す）。
    pub async fn subscribe_wildcard(
        &mut self,
        min_interval_floor_s: u16,
        max_interval_ceiling_s: u16,
        keep_subscriptions: bool,
        clusters: &[u32],
        cfg: &MrpConfig,
    ) -> Result<
        (
            crate::im::SubscribeResponse,
            Vec<crate::im::ReportDataMessage>,
        ),
        SessionError,
    > {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_subscribe_request(
            min_interval_floor_s,
            max_interval_ceiling_s,
            keep_subscriptions,
            clusters,
        );
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_SUBSCRIBE_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let mut msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        let mut priming = Vec::new();
        loop {
            match msg.proto.opcode {
                im::OPCODE_REPORT_DATA => {
                    // 監査⑨: デコード失敗でも購読は殺さない。認証済み（MIC 検証
                    // 済み）のチャンクなので ack して先へ進み、失われるのはこの
                    // チャンクの属性値だけ（matd の state cache は次のレポートで
                    // 自己回復）。空 rd を push するのは MAX_REPORT_CHUNKS の
                    // flood 防御を非デコード可能チャンクにも効かせるため。
                    let rd = match im::decode_report_data_message(&msg.payload) {
                        Ok(rd) => rd,
                        Err(e) => {
                            tracing::warn!(
                                exchange_id,
                                payload_len = msg.payload.len(),
                                error = %e,
                                "subscribe: undecodable priming chunk; acking and continuing"
                            );
                            tracing::debug!(
                                payload_head = %payload_head_hex(&msg.payload),
                                "undecodable priming chunk payload"
                            );
                            im::ReportDataMessage {
                                reports: Vec::new(),
                                subscription_id: None,
                                more_chunks: false,
                                suppress_response: false,
                            }
                        }
                    };
                    tracing::debug!(
                        exchange_id,
                        reports = rd.reports.len(),
                        more_chunks = rd.more_chunks,
                        "subscribe: priming report chunk"
                    );
                    priming.push(rd);
                    if priming.len() > MAX_REPORT_CHUNKS {
                        return Err(SessionError::Im(ImError::Malformed(
                            "too many report chunks",
                        )));
                    }
                    // priming の各チャンクに StatusResponse(0)。最終チャンク後は
                    // SubscribeResponse が同 exchange で続く。
                    let ok = im::encode_status_response(0);
                    let resp = self
                        .send_reliable(
                            exchange_id,
                            im::PROTOCOL_ID_IM,
                            im::OPCODE_STATUS_RESPONSE,
                            &ok,
                            cfg,
                        )
                        .await?;
                    msg = match resp {
                        Some(m) => m,
                        None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
                    };
                }
                im::OPCODE_SUBSCRIBE_RESPONSE => {
                    let sr =
                        im::decode_subscribe_response(&msg.payload).map_err(SessionError::Im)?;
                    tracing::debug!(
                        exchange_id,
                        subscription_id = sr.subscription_id,
                        max_interval_s = sr.max_interval_s,
                        needs_ack = msg.proto.needs_ack,
                        counter = msg.header.message_counter,
                        "subscribe: SubscribeResponse received"
                    );
                    return Ok((sr, priming));
                }
                im::OPCODE_STATUS_RESPONSE => {
                    let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                    return Err(SessionError::Im(ImError::StatusResponse(s)));
                }
                op => return Err(SessionError::UnexpectedOpcode(op)),
            }
        }
    }

    /// 購読成立後のデバイス発 ReportData を 1 通受ける。keep-alive（空 report）も
    /// そのまま返す（deadline リセットは呼び出し側 = matd の責務）。`timeout` 無音は
    /// `SessionError::Silence`（上位が購読死亡として再購読する）。
    pub async fn next_subscription_report(
        &mut self,
        timeout: Duration,
        cfg: &MrpConfig,
    ) -> Result<crate::im::ReportDataMessage, SessionError> {
        use crate::im;
        if let Some(e) = self.deferred_sub_err.take() {
            return Err(e);
        }
        // screen が待避した report が先にあればそれを消費する。
        let msg = if let Some(m) = self.peer_initiated.pop_front() {
            m
        } else {
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(SessionError::Silence);
                }
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(recv) =
                    tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
                else {
                    return Err(SessionError::Silence);
                };
                let (n, from) = recv?;
                tracing::debug!(len = n, %from, "sub pump: datagram received");
                let Some(m) = self
                    .screen_with(&buf[..n], from, ScreenFilter::AnyPeerInitiated)
                    .await?
                else {
                    continue;
                };
                if m.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                    && m.proto.opcode == OPCODE_MRP_STANDALONE_ACK
                {
                    continue;
                }
                break m;
            }
        };
        if msg.proto.opcode != im::OPCODE_REPORT_DATA {
            return Err(SessionError::UnexpectedOpcode(msg.proto.opcode));
        }
        // 監査⑨: デコード失敗でも購読は殺さない。認証済み（MIC 検証済み）の
        // デバイス発メッセージなので生存の証拠としては正しく、空 rd
        // （= keep-alive 相当）に差し替えて届ける。suppress_response は読めない
        // ので false 扱い → 下の分岐が StatusResponse(0) で exchange を閉じる
        // （1.16.0 ワイヤ実測: 実デバイスの購読レポートは suppress=false +
        // StatusResponse 期待。suppress=true の相手への余計な SR は exchange
        // 終端で無害）。
        let rd = match im::decode_report_data_message(&msg.payload) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(
                    exchange_id = msg.proto.exchange_id,
                    payload_len = msg.payload.len(),
                    error = %e,
                    "sub pump: undecodable report; delivering as empty"
                );
                tracing::debug!(
                    payload_head = %payload_head_hex(&msg.payload),
                    "undecodable report payload"
                );
                im::ReportDataMessage {
                    reports: Vec::new(),
                    subscription_id: None,
                    more_chunks: false,
                    suppress_response: false,
                }
            }
        };
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            subscription_id = rd.subscription_id,
            reports = rd.reports.len(),
            suppress_response = rd.suppress_response,
            "sub pump: report delivered"
        );
        if !rd.suppress_response {
            if let Err(e) = self.respond_status(msg.proto.exchange_id, 0, cfg).await {
                // デコード済み report を道連れにしない: report は届け、失敗は
                // 次回呼び出しへ持ち越す（pump は 5s スライスで即座に気づく）。
                tracing::debug!(error = %e, "sub pump: status response failed; delivering report, deferring error");
                self.deferred_sub_err = Some(e);
            }
        }
        Ok(rd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::seal_message;
    use crate::exchange::IncomingMessage;
    use crate::message::{
        Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
        PROTOCOL_ID_SECURE_CHANNEL,
    };
    use crate::session::test_util::*;
    use crate::transport::{Transport, MAX_DATAGRAM, RELIABLE_PEER};
    use std::sync::Arc;
    use std::time::Duration;

    /// 購読ハンドシェイク: priming 2 チャンク（各チャンクに StatusResponse(0)）→
    /// SubscribeResponse で成立。fragile part の釘打ち（spec テスト方針 1）。
    #[tokio::test]
    async fn subscribe_wildcard_handshake_with_chunked_priming() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            // SubscribeRequest を受ける
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_SUBSCRIBE_REQUEST);
            let ex = p.exchange_id;
            // priming チャンク1（more=true）
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                None,
                false,
                9000,
                &subscription_report_payload(42, true, true),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            // StatusResponse(0) を受ける
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p2, body) = open_from_controller(&buf[..n]);
            assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            // priming チャンク2（more=false）
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                None,
                false,
                9001,
                &subscription_report_payload(42, false, false),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            // 最終チャンクにも StatusResponse(0)（SubscribeResponse がこの後に続くため必須）
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p3, body) = open_from_controller(&buf[..n]);
            assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            // SubscribeResponse
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_SUBSCRIBE_RESPONSE,
                None,
                false,
                9002,
                &subscribe_response_payload(42, 120),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        });

        let (resp, priming) = s
            .subscribe_wildcard(0, 3600, false, &[], &fast_cfg())
            .await
            .unwrap();
        assert_eq!(resp.subscription_id, 42);
        assert_eq!(resp.max_interval_s, 120);
        assert_eq!(priming.len(), 2);
        assert_eq!(priming[0].reports[0].data, Some(serde_json::json!(true)));
        dev_task.await.unwrap();
    }

    /// 絞り込み購読: SubscribeRequest の AttributeRequests に指定クラスタの
    /// AttributePathIB が列挙されてワイヤに乗る（priming 軽量化の釘打ち）。
    #[tokio::test]
    async fn subscribe_wildcard_sends_cluster_paths_when_narrowed() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_SUBSCRIBE_REQUEST);
            // SubscribeRequest 中の Uint な Context(3) は AttributePathIB の
            // cluster だけ（トップの Context(3) は ArrayStart、IsFabricFiltered
            // は Context(7)）なので、素朴な全要素走査で拾える。
            use crate::tlv::{Reader, Tag, Value};
            let mut r = Reader::new(&body);
            let mut clusters = Vec::new();
            while let Some(el) = r.next().unwrap() {
                if el.tag == Tag::Context(3) {
                    if let Value::Uint(v) = el.value {
                        clusters.push(u32::try_from(v).unwrap());
                    }
                }
            }
            assert_eq!(clusters, vec![0x0006, 0x0402]);
            let ex = p.exchange_id;
            // priming 1 チャンク（more=false）→ StatusResponse(0) → SubscribeResponse
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                None,
                false,
                9100,
                &subscription_report_payload(43, false, false),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p2, body) = open_from_controller(&buf[..n]);
            assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_SUBSCRIBE_RESPONSE,
                None,
                false,
                9101,
                &subscribe_response_payload(43, 300),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        });

        let (resp, priming) = s
            .subscribe_wildcard(0, 300, false, &[0x0006, 0x0402], &fast_cfg())
            .await
            .unwrap();
        assert_eq!(resp.subscription_id, 43);
        assert_eq!(priming.len(), 1);
        dev_task.await.unwrap();
    }

    /// 監査⑨: 非デコード可能な priming チャンクは購読を殺さない —
    /// warn + StatusResponse(0) + 空 rd 差し替えで続行し、ハンドシェイクは成立する。
    #[tokio::test]
    async fn subscribe_wildcard_survives_undecodable_priming_chunk() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, _) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_SUBSCRIBE_REQUEST);
            let ex = p.exchange_id;
            // garbage チャンク（struct start だけで途中切れ = デコード不能）
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                None,
                false,
                9200,
                &[0x15],
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            // garbage にも StatusResponse(0) が返る
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p2, body) = open_from_controller(&buf[..n]);
            assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            // 正常チャンク（more=false）→ StatusResponse(0) → SubscribeResponse
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                None,
                false,
                9201,
                &subscription_report_payload(44, true, false),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p3, body) = open_from_controller(&buf[..n]);
            assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_SUBSCRIBE_RESPONSE,
                None,
                false,
                9202,
                &subscribe_response_payload(44, 120),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        });

        let (resp, priming) = s
            .subscribe_wildcard(0, 3600, false, &[], &fast_cfg())
            .await
            .expect("undecodable priming chunk must not kill the subscribe");
        assert_eq!(resp.subscription_id, 44);
        assert_eq!(priming.len(), 2);
        assert!(priming[0].reports.is_empty()); // salvage 差し替えの空 rd
        assert_eq!(priming[1].reports[0].data, Some(serde_json::json!(true)));
        dev_task.await.unwrap();
    }

    /// 監査⑨の flood 防御維持: 非デコード可能チャンクも MAX_REPORT_CHUNKS に
    /// 数えられ、超過で subscribe は Malformed で失敗する（無限チャンク防御が
    /// salvage で消えていないことの釘打ち）。
    #[tokio::test]
    async fn subscribe_wildcard_undecodable_chunks_still_count_toward_chunk_cap() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, _) = open_from_controller(&buf[..n]);
            let ex = p.exchange_id;
            // cap(64)+1 = 65 チャンク送る。65 個目は push で cap を超えて
            // subscribe が Err で抜けるため、StatusResponse は 64 回しか返らない。
            for i in 0..=(MAX_REPORT_CHUNKS as u32) {
                let d = device_datagram(
                    ex,
                    crate::im::PROTOCOL_ID_IM,
                    crate::im::OPCODE_REPORT_DATA,
                    None,
                    false,
                    9300 + i,
                    &[0x15],
                );
                dev.send_to(&d, RELIABLE_PEER).await.unwrap();
                if (i as usize) < MAX_REPORT_CHUNKS {
                    let (n, _) = dev.recv_from(&mut buf).await.unwrap();
                    let (_, p2, _) = open_from_controller(&buf[..n]);
                    assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
                }
            }
        });

        let err = s
            .subscribe_wildcard(0, 3600, false, &[], &fast_cfg())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SessionError::Im(crate::im::ImError::Malformed("too many report chunks"))
            ),
            "err: {err:?}"
        );
        dev_task.await.unwrap();
    }

    /// ポンプ: デバイス起点の新 exchange（initiator=true）で届く ReportData を受け、
    /// StatusResponse(0) で閉じる。keep-alive（空 report）も受かる。
    #[tokio::test]
    async fn next_subscription_report_receives_device_initiated_reports_and_keepalive() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            // device 発の新 exchange。initiator=true（デバイスがその exchange の起点）。
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 100,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: crate::im::OPCODE_REPORT_DATA,
                exchange_id: 0x7777,
                protocol_id: crate::im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let d = seal_message(
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(42, true, false),
                DEV_NODE,
            )
            .unwrap();
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            // StatusResponse(0) が device の exchange 上で、こちら=non-initiator として返る
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(p.exchange_id, 0x7777);
            assert!(!p.initiator);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            // keep-alive（別 exchange）
            let mut h2 = header;
            h2.message_counter = 101;
            let mut p2 = proto;
            p2.exchange_id = 0x7778;
            let d = seal_message(&R2I, &h2, &p2, &keepalive_payload(42), DEV_NODE).unwrap();
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p3, _) = open_from_controller(&buf[..n]);
            assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(p3.exchange_id, 0x7778);
        });

        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(42));
        assert_eq!(rd.reports.len(), 1);
        let ka = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert!(ka.reports.is_empty()); // keep-alive
        dev_task.await.unwrap();
    }

    /// 監査⑨: 非デコード可能な live report は購読を殺さない — 空 rd
    /// （keep-alive 相当）として届き、StatusResponse(0) で exchange を閉じ、
    /// 次の正常 report は通常配送される。
    #[tokio::test]
    async fn next_subscription_report_survives_undecodable_report() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 200,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: crate::im::OPCODE_REPORT_DATA,
                exchange_id: 0x7779,
                protocol_id: crate::im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            // garbage report（struct start だけで途中切れ = デコード不能）
            let d = seal_message(&R2I, &header, &proto, &[0x15], DEV_NODE).unwrap();
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            // StatusResponse(0) が同 exchange に返る
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(p.exchange_id, 0x7779);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            // 正常 report（別 exchange）は通常配送される
            let mut h2 = header;
            h2.message_counter = 201;
            let mut p2 = proto;
            p2.exchange_id = 0x777a;
            let d = seal_message(
                &R2I,
                &h2,
                &p2,
                &subscription_report_payload(42, true, false),
                DEV_NODE,
            )
            .unwrap();
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p3, _) = open_from_controller(&buf[..n]);
            assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(p3.exchange_id, 0x777a);
        });

        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .expect("undecodable report must not kill the pump");
        assert!(rd.reports.is_empty());
        assert_eq!(rd.subscription_id, None);
        let rd2 = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd2.subscription_id, Some(42));
        dev_task.await.unwrap();
    }

    /// 実機バグの釘（Issue #21）: needs_ack な購読 ReportData への
    /// StatusResponse は、その report の message counter を piggyback ack
    /// しなければならない。standalone ack が RF で失われたとき、デバイスは
    /// SR を受け取っても report の MRP が未充足のままになり（FP300 実測:
    /// 完了済み exchange の report を再送 → その後購読を silent 破棄）、
    /// piggyback だけがこの経路を塞ぐ。
    #[tokio::test]
    async fn status_response_piggybacks_ack_of_report() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        const REPORT_COUNTER: u32 = 500;
        let dev_task = tokio::spawn(async move {
            // デバイス起点 exchange（initiator=true）の needs_ack ReportData。
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: REPORT_COUNTER,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: true,
                acked_counter: None,
                opcode: crate::im::OPCODE_REPORT_DATA,
                exchange_id: 0x7777,
                protocol_id: crate::im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let report = seal_message(
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(42, true, false),
                DEV_NODE,
            )
            .unwrap();
            device.send_to(&report, local).await.unwrap();
            // standalone ack と StatusResponse の両方が届く。SR を拾って
            // piggyback ack を検査し、SR には ack を返して exchange を閉じる。
            let mut buf = [0u8; MAX_DATAGRAM];
            loop {
                let (n, from) = device.recv_from(&mut buf).await.unwrap();
                let (h, p, _) = open_from_controller(&buf[..n]);
                if p.opcode != crate::im::OPCODE_STATUS_RESPONSE {
                    continue;
                }
                assert_eq!(
                    p.acked_counter,
                    Some(REPORT_COUNTER),
                    "StatusResponse must piggyback the report's ack"
                );
                // SR への ack はデバイスが initiator の exchange 上で返す。
                let ack_header = MessageHeader {
                    session_id: LOCAL_SID,
                    security_flags: 0,
                    message_counter: REPORT_COUNTER + 1,
                    source_node_id: None,
                    destination: Destination::None,
                };
                let ack_proto = ProtocolHeader {
                    initiator: true,
                    needs_ack: false,
                    acked_counter: Some(h.message_counter),
                    opcode: OPCODE_MRP_STANDALONE_ACK,
                    exchange_id: 0x7777,
                    protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                    vendor_id: None,
                };
                let ack = seal_message(&R2I, &ack_header, &ack_proto, &[], DEV_NODE).unwrap();
                device.send_to(&ack, from).await.unwrap();
                break;
            }
        });

        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(42));
        dev_task.await.unwrap();
    }

    /// 無音は Silence（上位=matd が購読死亡と判定して再購読する）。MRP 送信
    /// ack 切れの Timeout とは別 variant（pump 終了ログの切り分けに必須）。
    #[tokio::test]
    async fn next_subscription_report_times_out_on_silence() {
        let (mut s, _dev) = reliable_session_pair();
        assert!(matches!(
            s.next_subscription_report(Duration::from_millis(100), &fast_cfg())
                .await,
            Err(SessionError::Silence)
        ));
    }

    /// UDP: device 発 needs_ack ReportData は screen が ack し、購読 API で取り出せる
    /// （ack 済みメッセージの取り落とし=永久喪失が無いこと）。
    #[tokio::test]
    async fn udp_device_initiated_report_is_acked_and_delivered() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        let dev = tokio::spawn(async move {
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 300,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: true,
                acked_counter: None,
                opcode: crate::im::OPCODE_REPORT_DATA,
                exchange_id: 0x5555,
                protocol_id: crate::im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let d = seal_message(
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(9, true, false),
                DEV_NODE,
            )
            .unwrap();
            device.send_to(&d, local).await.unwrap();
            // standalone ack と StatusResponse(needs_ack) が来る。StatusResponse は ack を返す。
            loop {
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(Ok((n, from))) =
                    tokio::time::timeout(Duration::from_secs(2), device.recv_from(&mut buf)).await
                else {
                    break;
                };
                let (h, p, _) = open_from_controller(&buf[..n]);
                if p.opcode == crate::im::OPCODE_STATUS_RESPONSE {
                    let ack = device_datagram(
                        p.exchange_id,
                        PROTOCOL_ID_SECURE_CHANNEL,
                        OPCODE_MRP_STANDALONE_ACK,
                        Some(h.message_counter),
                        false,
                        9900,
                        &[],
                    );
                    // device は自 exchange の initiator。ack の initiator は device 視点で true。
                    // device_datagram は initiator=false 固定なので直接 seal する。
                    let header2 = MessageHeader {
                        session_id: LOCAL_SID,
                        security_flags: 0,
                        message_counter: 9900,
                        source_node_id: None,
                        destination: Destination::None,
                    };
                    let proto2 = ProtocolHeader {
                        initiator: true,
                        needs_ack: false,
                        acked_counter: Some(h.message_counter),
                        opcode: OPCODE_MRP_STANDALONE_ACK,
                        exchange_id: p.exchange_id,
                        protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                        vendor_id: None,
                    };
                    let _ = ack;
                    let d2 = seal_message(&R2I, &header2, &proto2, &[], DEV_NODE).unwrap();
                    device.send_to(&d2, from).await.unwrap();
                    break;
                }
            }
        });

        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(9));
        dev.await.unwrap();
    }

    /// buffer-then-drain: `screen_with` が（`OurExchange`/`PeerExchange` フィルタ
    /// 中に届いた device 発 ReportData を）`peer_initiated` へ待避した状況を直接
    /// 再現し、`next_subscription_report` がソケットを読む前にそれを drain して
    /// 返すことを示す（"pop_front で先に drain" の回帰検知）。dev 側は何も送らない
    /// ため、drain が外れれば pop_front 後の分岐に落ちてソケット待ちになり、
    /// 短い timeout で `SessionError::Timeout` になって assert が落ちる。
    #[tokio::test]
    async fn next_subscription_report_drains_buffered_report_before_reading_socket() {
        let (mut s, _dev) = reliable_session_pair();

        let header = MessageHeader {
            session_id: LOCAL_SID,
            security_flags: 0,
            message_counter: 500,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: crate::im::OPCODE_REPORT_DATA,
            exchange_id: 0x9999,
            protocol_id: crate::im::PROTOCOL_ID_IM,
            vendor_id: None,
        };
        // screen_with の buffer push（session/mrp.rs の push_back）と同じ形の
        // IncomingMessage を、フィルタ落ちで待避済みだった体で直接 peer_initiated
        // に積む（`tests` は `session` のサブモジュールなので private field に届く）。
        s.peer_initiated.push_back(IncomingMessage {
            header,
            proto,
            payload: subscription_report_payload(77, true, false),
        });

        let rd = s
            .next_subscription_report(Duration::from_millis(200), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(77));
        assert_eq!(rd.reports.len(), 1);
        assert!(s.peer_initiated.is_empty());
    }

    /// 監査#1 経路B: respond_status の ack 待ち中に届いた続きチャンク
    /// （StatusResponse への ack を piggyback した ReportData）は破棄せず
    /// peer_initiated へ待避し、次の next_subscription_report が配信する。
    #[tokio::test]
    async fn report_chunk_arriving_during_status_ack_wait_is_not_lost() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        let dev = tokio::spawn(async move {
            // chunk A（more_chunks=true、needs_ack）。
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 800,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: true,
                acked_counter: None,
                opcode: crate::im::OPCODE_REPORT_DATA,
                exchange_id: 0x8888,
                protocol_id: crate::im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let d = seal_message(
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(11, true, true),
                DEV_NODE,
            )
            .unwrap();
            device.send_to(&d, local).await.unwrap();
            // StatusResponse を待ち、chunk B（piggyback ack、needs_ack）で応える。
            let mut buf = [0u8; MAX_DATAGRAM];
            loop {
                let (n, from) = device.recv_from(&mut buf).await.unwrap();
                let (h, p, _) = open_from_controller(&buf[..n]);
                if p.opcode != crate::im::OPCODE_STATUS_RESPONSE {
                    continue; // standalone ack 等は読み飛ばす
                }
                let header2 = MessageHeader {
                    session_id: LOCAL_SID,
                    security_flags: 0,
                    message_counter: 801,
                    source_node_id: None,
                    destination: Destination::None,
                };
                let proto2 = ProtocolHeader {
                    initiator: true,
                    needs_ack: true,
                    acked_counter: Some(h.message_counter),
                    opcode: crate::im::OPCODE_REPORT_DATA,
                    exchange_id: 0x8888,
                    protocol_id: crate::im::PROTOCOL_ID_IM,
                    vendor_id: None,
                };
                let d2 = seal_message(
                    &R2I,
                    &header2,
                    &proto2,
                    &subscription_report_payload(12, false, false),
                    DEV_NODE,
                )
                .unwrap();
                device.send_to(&d2, from).await.unwrap();
                break;
            }
            // chunk B への StatusResponse に standalone ack を返す（2 回目の
            // next_subscription_report を完走させる）。
            loop {
                let Ok(Ok((n, from))) =
                    tokio::time::timeout(Duration::from_secs(2), device.recv_from(&mut buf)).await
                else {
                    break;
                };
                let (h, p, _) = open_from_controller(&buf[..n]);
                if p.opcode == crate::im::OPCODE_STATUS_RESPONSE {
                    let header3 = MessageHeader {
                        session_id: LOCAL_SID,
                        security_flags: 0,
                        message_counter: 802,
                        source_node_id: None,
                        destination: Destination::None,
                    };
                    let proto3 = ProtocolHeader {
                        initiator: true,
                        needs_ack: false,
                        acked_counter: Some(h.message_counter),
                        opcode: OPCODE_MRP_STANDALONE_ACK,
                        exchange_id: p.exchange_id,
                        protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                        vendor_id: None,
                    };
                    let d3 = seal_message(&R2I, &header3, &proto3, &[], DEV_NODE).unwrap();
                    device.send_to(&d3, from).await.unwrap();
                    break;
                }
            }
        });

        // 1 回目: chunk A が返り、その StatusResponse は chunk B の piggyback
        // ack で確認される。
        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(11));
        // chunk B は破棄されず待避済み。
        assert_eq!(s.peer_initiated.len(), 1, "chunk B must be stashed");
        // 2 回目: 待避済み chunk B がソケットを読まずに返る。
        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(12));
        dev.await.unwrap();
    }
}
