//! レスポンダ側（デバイス役 CASE/PASE セッション + 購読の StatusResponse）:
//! ピア発リクエストの受理・待避・返送と、reliable な応答送信。

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;

use crate::exchange::{IncomingMessage, MrpConfig};
use crate::message::{OPCODE_MRP_STANDALONE_ACK, PROTOCOL_ID_SECURE_CHANNEL};
use crate::transport::MAX_DATAGRAM;

use super::mrp::{ScreenFilter, MAX_PEER_INITIATED_BUFFER};
use super::{SecureSession, SessionError};

impl SecureSession {
    /// デバイス起点の exchange へ StatusResponse(status) を返す。UDP では
    /// needs_ack + 再送で相手の standalone ack を待つ（購読 report の確認応答は
    /// IM 契約上必須 — 取りこぼすとデバイスが購読を落とす）。Reliable transport
    /// は 1 回送るだけ。
    pub async fn respond_status(
        &mut self,
        exchange_id: u16,
        status: u8,
        cfg: &MrpConfig,
    ) -> Result<(), SessionError> {
        use crate::im;
        let payload = im::encode_status_response(status);
        if self.transport.is_reliable() {
            let (datagram, _) = self.seal(
                exchange_id,
                false,
                im::PROTOCOL_ID_IM,
                im::OPCODE_STATUS_RESPONSE,
                false,
                None,
                &payload,
            )?;
            self.transport.send_to(&datagram, self.peer).await?;
            return Ok(());
        }
        let (datagram, our_counter) = self.seal(
            exchange_id,
            false,
            im::PROTOCOL_ID_IM,
            im::OPCODE_STATUS_RESPONSE,
            true,
            None,
            &payload,
        )?;
        let mut interval = crate::exchange::retrans_base(self.last_rx, cfg);
        let mut attempts = 0u32;
        loop {
            self.transport.send_to(&datagram, self.peer).await?;
            let deadline = Instant::now()
                + crate::exchange::jittered_interval(
                    interval,
                    cfg.jitter,
                    crate::exchange::unit_random(),
                );
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(recv) =
                    tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
                else {
                    break;
                };
                let (n, from) = recv?;
                let Some(msg) = self
                    .screen_with(&buf[..n], from, ScreenFilter::PeerExchange(exchange_id))
                    .await?
                else {
                    continue;
                };
                let acked = msg.proto.acked_counter == Some(our_counter);
                // ack 待ち中に届いた続きチャンク（device 発 ReportData）は
                // ack 照合の副産物として捨てない — screen_with のフィルタ落ち
                // 待避と同じ規律で peer_initiated へ積み、購読 API が消費する
                // （監査#1 経路B）。
                if msg.proto.protocol_id == im::PROTOCOL_ID_IM
                    && msg.proto.opcode == im::OPCODE_REPORT_DATA
                {
                    if self.peer_initiated.len() >= MAX_PEER_INITIATED_BUFFER {
                        tracing::warn!("peer-initiated report buffer full; dropping oldest");
                        self.peer_initiated.pop_front();
                    }
                    self.peer_initiated.push_back(msg);
                }
                if acked {
                    return Ok(());
                }
            }
            attempts += 1;
            if attempts > cfg.max_retries {
                return Err(SessionError::Timeout);
            }
            interval = interval.mul_f64(cfg.backoff);
        }
    }

    /// デバイス役: peer-initiated のリクエストを 1 件受ける。ack 送出/重複排除は
    /// `screen_with` が(フィルタに関わらず)常に処理するため、ここでは
    /// `ScreenFilter::AnyPeerInitiated` で待ち受けて最初の実メッセージ（standalone
    /// ack を除く）を返すだけでよい。`next_subscription_report` のポンプと同型
    /// （あちらは opcode を `ReportData` に固定してデコードまでするのに対し、
    /// こちらはデコードせず `IncomingMessage` を生で返す汎用版）。screen が
    /// フィルタ落ちで `peer_initiated` に待避したメッセージがあれば先にそれを
    /// 返す（待避条件は今のところ ReportData のみだが、将来の待避種別が増えても
    /// 取りこぼさないよう同じ規律に倣う）。
    /// Feeds one already-received raw datagram (`buf`, from `from`) through
    /// the same decrypt/screen/ack pipeline `recv_request`'s internal loop
    /// uses, without this call itself reading from the transport. A
    /// device-role runtime that owns a single shared socket must classify
    /// each datagram (unsecured PASE/CASE opcode vs. secured session
    /// traffic) *before* deciding which flow handles it — by the time that
    /// classification has happened, the datagram has already been read off
    /// the socket, so `recv_request`'s own internal `recv_from` can't be the
    /// one to see it. Returns `Ok(None)` for standalone acks / screened-out
    /// (foreign, duplicate, delivery-filter-mismatch) traffic — the caller
    /// should just move on to the next datagram, exactly as `recv_request`'s
    /// loop does internally.
    pub async fn deliver_request(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        let Some(msg) = self
            .screen_with(buf, from, ScreenFilter::AnyPeerInitiated)
            .await?
        else {
            return Ok(None);
        };
        if msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
            && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK
        {
            return Ok(None);
        }
        Ok(Some(msg))
    }

    pub async fn recv_request(
        &mut self,
        timeout: Duration,
    ) -> Result<IncomingMessage, SessionError> {
        if let Some(m) = self.peer_initiated.pop_front() {
            return Ok(m);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SessionError::Timeout);
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(recv) =
                tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
            else {
                return Err(SessionError::Timeout);
            };
            let (n, from) = recv?;
            let Some(msg) = self
                .screen_with(&buf[..n], from, ScreenFilter::AnyPeerInitiated)
                .await?
            else {
                continue;
            };
            if msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK
            {
                continue;
            }
            return Ok(msg);
        }
    }

    /// デバイス役: ソケットに触れずに `peer_initiated` から 1 件だけ取り出す
    /// （`recv_request` の非ブロッキング版）。`reply_reliable` がある応答の ack
    /// 待ちをしている間に別 exchange 宛の新規リクエストが届き、`screen_with`
    /// のフィルタ落ちで待避された場合、その応答を送り終えた直後にこれで
    /// drain してから次のソケット読みに戻る、という使い方を想定している
    /// （device-role ランタイムの `serve_secured` が呼ぶ）。バッファが空なら
    /// `None`（ソケットは読まない — `recv_request` と違いここでブロックしない）。
    pub fn take_buffered_request(&mut self) -> Option<IncomingMessage> {
        self.peer_initiated.pop_front()
    }

    /// `take_buffered_request`/`recv_request` の鏡像: 取り出したものの今は
    /// 処理できないピア発リクエストを待避バッファの**先頭**へ戻す
    /// （`pop_front` の逆なので、戻した直後の `take_buffered_request` は
    /// それを返す = 取り出す前と同じ順序が保たれる）。
    ///
    /// 用途は「特定の exchange の応答だけを待っている最中に、別 exchange の
    /// リクエストを引いてしまった」ケース（device-role ランタイムのチャンク
    /// StatusResponse 待ち `await_peer_status_ok`）。`screen_with` は配送
    /// フィルタに関わらず認証済み needs_ack メッセージを ack 済みなので、
    /// ここで捨てるとピアは再送せず永久に失われる（「cross-exchange secured
    /// request の ack-then-drop」と同じ事故）。捨てずに戻し、呼び出し元の
    /// drain（`serve_secured`）に拾わせる。
    ///
    /// バッファ上限に達しているときは**最新**（`push_back` 側）を落として
    /// でも戻す — 戻そうとしている方が古く、ピアから見て先に送ったリクエスト
    /// だから。
    pub fn requeue_buffered_request(&mut self, msg: IncomingMessage) {
        if self.peer_initiated.len() >= MAX_PEER_INITIATED_BUFFER {
            tracing::warn!("peer-initiated request buffer full on requeue; dropping newest");
            self.peer_initiated.pop_back();
        }
        self.peer_initiated.push_front(msg);
    }

    /// デバイス役: `request` が乗っていた exchange（`request.proto.exchange_id`）
    /// へ `initiator:false` で応答する。`respond_status`（IM StatusResponse 専用）
    /// の一般形 — 任意の `protocol_id`/`opcode`/`payload` を送れる。UDP では
    /// needs_ack + 再送でピアの ack を待ち、ack の代わりに同一 exchange への実
    /// メッセージが届いたらそれを返す（`send_reliable` と対称的な契約）。Reliable
    /// transport は 1 回送るだけ（呼び出し元が明示的に応答を待ちたい場合は続けて
    /// `recv_request`/`recv` を呼べばよい — `ReliableChannel` はバッファするので
    /// 送信の取りこぼしはない）。
    pub async fn reply_reliable(
        &mut self,
        request: &IncomingMessage,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        let exchange_id = request.proto.exchange_id;
        if self.transport.is_reliable() {
            let (datagram, _) = self.seal(
                exchange_id,
                false,
                protocol_id,
                opcode,
                false,
                None,
                payload,
            )?;
            self.transport.send_to(&datagram, self.peer).await?;
            return Ok(None);
        }
        let (datagram, our_counter) =
            self.seal(exchange_id, false, protocol_id, opcode, true, None, payload)?;
        let mut interval = crate::exchange::retrans_base(self.last_rx, cfg);
        let mut attempts = 0u32;
        loop {
            self.transport.send_to(&datagram, self.peer).await?;
            let deadline = Instant::now()
                + crate::exchange::jittered_interval(
                    interval,
                    cfg.jitter,
                    crate::exchange::unit_random(),
                );
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(recv) =
                    tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
                else {
                    break;
                };
                let (n, from) = recv?;
                let Some(msg) = self
                    .screen_with(&buf[..n], from, ScreenFilter::PeerExchange(exchange_id))
                    .await?
                else {
                    // フィルタ落ち（別 exchange 宛など）でも、その datagram の
                    // ack フィールドは screen_with が exchange 不問で
                    // `last_peer_ack` に記録済み。real controller/commissioner
                    // が standalone ack を送らず次のリクエストに我々への ack
                    // を piggyback しただけ、というケースをここで拾う
                    // （そのリクエスト自体は `peer_initiated` に待避済み —
                    // 呼び出し元が drain する）。
                    if self.last_peer_ack == Some(our_counter) {
                        return Ok(None);
                    }
                    continue;
                };
                let acked = msg.proto.acked_counter == Some(our_counter);
                let is_standalone_ack = msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                    && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK;
                if is_standalone_ack {
                    if acked {
                        return Ok(None);
                    }
                    continue;
                }
                return Ok(Some(msg));
            }
            attempts += 1;
            if attempts > cfg.max_retries {
                return Err(SessionError::Timeout);
            }
            interval = interval.mul_f64(cfg.backoff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::seal_message;
    use crate::message::{
        Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
        PROTOCOL_ID_SECURE_CHANNEL,
    };
    use crate::session::test_util::*;
    use crate::session::SessionKeys;
    use crate::transport::{ReliableChannel, Transport, MAX_DATAGRAM, RELIABLE_PEER};
    use std::sync::Arc;
    use std::time::Duration;

    /// 実機バグの釘（secure 経路）: priming チャンク受信直後＝ピア active の
    /// 再送（respond_status / send_reliable 共通の base 選択）は active
    /// interval で行う。SII=5000ms のまま再送するとデバイス側 chunk
    /// タイムアウトに負けて購読が 0x80 死する（2026-07-20 実機ワイヤ確認）。
    #[tokio::test]
    async fn respond_status_retransmits_fast_after_recent_peer_rx() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        s.last_rx = Some(Instant::now()); // チャンク受信直後の状況を注入
        let cfg = MrpConfig {
            initial_interval: Duration::from_secs(5),
            active_interval: Duration::from_millis(50),
            max_retries: 2,
            backoff: 1.0,
            jitter: 0.0,
        };
        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let _ = device.recv_from(&mut buf).await.unwrap();
            let again =
                tokio::time::timeout(Duration::from_secs(1), device.recv_from(&mut buf)).await;
            assert!(
                again.is_ok(),
                "no retransmission within 1s: active interval not applied"
            );
        });
        let t0 = std::time::Instant::now();
        let err = s.respond_status(1234, 0, &cfg).await.unwrap_err();
        assert!(matches!(err, SessionError::Timeout));
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "timeout took {:?}; idle interval used despite recent rx?",
            t0.elapsed()
        );
        dev.await.unwrap();
    }

    /// 監査#1 経路A: 購読 report への StatusResponse が ack されず MRP 予算を
    /// 使い切っても、デコード済み report は道連れにしない。1 回目の呼び出しは
    /// Ok(report) を返し、失敗は deferred error として 2 回目の呼び出しで返る
    /// （read 経路の best-effort と違い、セッション死の即時検知は保つ）。
    #[tokio::test]
    async fn respond_status_failure_defers_error_and_still_delivers_report() {
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

        // デバイス: report を送るが、以後一切 ack しない（無応答デバイス）。
        let dev = tokio::spawn(async move {
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 700,
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
            let d = seal_message(
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(9, true, false),
                DEV_NODE,
            )
            .unwrap();
            device.send_to(&d, local).await.unwrap();
            // StatusResponse（+再送）を受けるが ack は返さない。
            let mut buf = [0u8; MAX_DATAGRAM];
            while tokio::time::timeout(Duration::from_secs(1), device.recv_from(&mut buf))
                .await
                .is_ok()
            {}
        });

        // 1 回目: respond_status は MRP 予算切れで失敗するが、report は返る。
        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .expect("decoded report must survive a failed status response");
        assert_eq!(rd.subscription_id, Some(9));

        // 2 回目: 持ち越された Timeout が返る（セッション死の即時検知）。
        let err = s
            .next_subscription_report(Duration::from_millis(100), &fast_cfg())
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::Timeout), "deferred: {err:?}");
        dev.await.unwrap();
    }

    /// T5 の要: `new_device_role` で構築したデバイス役セッションが、
    /// `recv_request` でコントローラの InvokeRequest を受け、`reply_reliable` で
    /// InvokeResponse を返す往復が `SecureSession::invoke` から見て成功すること。
    /// 鍵/セッション id/ノード id の入れ替えが 1 か所でも狂うと open/seal の
    /// どちらかが失敗し invoke がエラーになるので、これは入れ替えの正しさの
    /// 実質的な検証も兼ねる。
    #[tokio::test]
    async fn device_role_session_serves_invoke() {
        use crate::im;
        let (ta, tb) = ReliableChannel::pair();
        let keys = SessionKeys {
            i2r: [1; 16],
            r2i: [2; 16],
            attestation_challenge: [0; 16],
        };
        let cfg = fast_cfg();
        let mut ctrl = SecureSession::new(
            Arc::new(ta),
            RELIABLE_PEER,
            10,
            20,
            SessionKeys {
                i2r: keys.i2r,
                r2i: keys.r2i,
                attestation_challenge: keys.attestation_challenge,
            },
            111,
            222,
        );
        let mut dev =
            SecureSession::new_device_role(Arc::new(tb), RELIABLE_PEER, 20, 10, keys, 222, 111);
        // A device-role session starts with no peer CATs; `with_peer_cats`
        // is how the CASE driver attaches the ones read off the peer's NOC.
        assert!(dev.peer_cats().is_empty());
        let cats = crate::cert::CaseAuthTags::new(&[0x0001_0002]).unwrap();
        dev = dev.with_peer_cats(cats);
        assert_eq!(dev.peer_cats(), cats);
        assert_eq!(dev.peer_node_id(), 111);
        let server = tokio::spawn(async move {
            let req = dev
                .recv_request(Duration::from_secs(5))
                .await
                .expect("device recv_request");
            assert_eq!(req.proto.opcode, im::OPCODE_INVOKE_REQUEST);
            let resp = invoke_response_status_ok();
            dev.reply_reliable(
                &req,
                im::PROTOCOL_ID_IM,
                im::OPCODE_INVOKE_RESPONSE,
                &resp,
                &fast_cfg(),
            )
            .await
            .expect("device reply_reliable");
        });
        let out = ctrl.invoke(1, 0x0006, 1, None, &cfg).await.unwrap();
        assert_eq!(out.status, 0);
        server.await.unwrap();
    }

    /// レビュー対応: `device_role_session_serves_invoke` は `ReliableChannel`
    /// (is_reliable()==true) を使うため `reply_reliable` の UDP/MRP 分岐（1 発
    /// 送って `Ok(None)` を返すだけ）しか通らない。needs_ack + 再送ループ、
    /// `ScreenFilter::PeerExchange` でのマッチ、ack 到達での完了は未検証だった。
    /// この test はプレーンな `UdpTransport` 2 本で `SecureSession::new`（役割の
    /// 入れ替えは `new_device_role` 側で既に別途検証済みなので、ここでは MRP
    /// 機構そのものの検証に集中するため素の `new` を使う — `respond_status` の
    /// 既存 UDP テスト群と同じ流儀）を組み、デバイス起点(initiator=true)の
    /// リクエストを `recv_request` で受け、`reply_reliable` の最初の送信をわざと
    /// ack せず、再送された 2 発目にだけ standalone ack を返して完了
    /// (`Ok(None)`) することを検証する。
    #[tokio::test]
    async fn reply_reliable_udp_retransmits_until_acked() {
        use crate::im;
        let device = bind_local().await;
        let dev_addr = device.local_addr().unwrap();
        let s_transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let s_addr = s_transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&s_transport),
            dev_addr,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        const REQ_EXCHANGE: u16 = 0xABCD;
        let resp_payload = invoke_response_status_ok();

        let dev_task = tokio::spawn(async move {
            // デバイス起点(initiator=true)の InvokeRequest を送る（recv_request の
            // 材料。ack は要求しない — このテストの主眼は応答側の再送なので、
            // リクエスト自体の MRP は関与させない）。
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
                opcode: im::OPCODE_INVOKE_REQUEST,
                exchange_id: REQ_EXCHANGE,
                protocol_id: im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let req_dg = seal_message(&R2I, &header, &proto, &[], DEV_NODE).unwrap();
            device.send_to(&req_dg, s_addr).await.unwrap();

            // 1 発目: 受けるだけで ack しない（再送を誘発する）。
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n1, _) = device.recv_from(&mut buf).await.unwrap();
            let (_, p1, body1) = open_from_controller(&buf[..n1]);
            assert_eq!(p1.exchange_id, REQ_EXCHANGE);
            assert!(!p1.initiator, "reply must be initiator:false");
            assert!(p1.needs_ack, "UDP reply must request an ack");
            assert_eq!(p1.protocol_id, im::PROTOCOL_ID_IM);
            assert_eq!(p1.opcode, im::OPCODE_INVOKE_RESPONSE);
            assert_eq!(body1, invoke_response_status_ok());

            // 2 発目（再送）: 同一内容が再送されてくること。今度は standalone ack
            // を返す — reply_reliable はこれで完了するはず。
            let (n2, from2) = device.recv_from(&mut buf).await.unwrap();
            let (h2, p2, body2) = open_from_controller(&buf[..n2]);
            assert_eq!(p2.exchange_id, REQ_EXCHANGE);
            assert_eq!(
                body2, body1,
                "retransmission must resend the same datagram content"
            );
            let ack_header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 101,
                source_node_id: None,
                destination: Destination::None,
            };
            let ack_proto = ProtocolHeader {
                initiator: true, // デバイスがこの exchange の initiator
                needs_ack: false,
                acked_counter: Some(h2.message_counter),
                opcode: OPCODE_MRP_STANDALONE_ACK,
                exchange_id: REQ_EXCHANGE,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            };
            let ack_dg = seal_message(&R2I, &ack_header, &ack_proto, &[], DEV_NODE).unwrap();
            device.send_to(&ack_dg, from2).await.unwrap();
        });

        let req = s
            .recv_request(Duration::from_secs(5))
            .await
            .expect("recv_request over UDP");
        assert_eq!(req.proto.exchange_id, REQ_EXCHANGE);
        assert_eq!(req.proto.opcode, im::OPCODE_INVOKE_REQUEST);

        let t0 = std::time::Instant::now();
        let out = s
            .reply_reliable(
                &req,
                im::PROTOCOL_ID_IM,
                im::OPCODE_INVOKE_RESPONSE,
                &resp_payload,
                &fast_cfg(),
            )
            .await
            .expect("reply_reliable completes once acked");
        assert!(out.is_none(), "standalone ack must yield Ok(None)");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "took {:?}; retransmit interval not honored?",
            t0.elapsed()
        );
        dev_task.await.unwrap();
    }

    /// レビュー対応: `reply_reliable` の UDP 分岐には、ack の代わりに同一
    /// exchange へ実メッセージが届いたらそれを `Ok(Some(msg))` として返す枝
    /// (`send_reliable` と対称的な契約) もある。これを ack 到達完了のケースと
    /// 分けて別枠で検証する。
    #[tokio::test]
    async fn reply_reliable_udp_returns_real_message_instead_of_ack() {
        use crate::im;
        let device = bind_local().await;
        let dev_addr = device.local_addr().unwrap();
        let s_transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let s_addr = s_transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&s_transport),
            dev_addr,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        const REQ_EXCHANGE: u16 = 0xBEEF;

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
                opcode: im::OPCODE_INVOKE_REQUEST,
                exchange_id: REQ_EXCHANGE,
                protocol_id: im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let req_dg = seal_message(&R2I, &header, &proto, &[], DEV_NODE).unwrap();
            device.send_to(&req_dg, s_addr).await.unwrap();

            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (_, p, _) = open_from_controller(&buf[..n]);
            assert_eq!(p.exchange_id, REQ_EXCHANGE);
            assert_eq!(p.opcode, im::OPCODE_INVOKE_RESPONSE);

            // ack ではなく、同一 exchange へ実メッセージ（StatusResponse）を返す。
            let real_header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 201,
                source_node_id: None,
                destination: Destination::None,
            };
            let real_proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: im::OPCODE_STATUS_RESPONSE,
                exchange_id: REQ_EXCHANGE,
                protocol_id: im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let real_dg = seal_message(
                &R2I,
                &real_header,
                &real_proto,
                &im::encode_status_response(0),
                DEV_NODE,
            )
            .unwrap();
            device.send_to(&real_dg, from).await.unwrap();
        });

        let req = s
            .recv_request(Duration::from_secs(5))
            .await
            .expect("recv_request over UDP");
        let out = s
            .reply_reliable(
                &req,
                im::PROTOCOL_ID_IM,
                im::OPCODE_INVOKE_RESPONSE,
                &invoke_response_status_ok(),
                &fast_cfg(),
            )
            .await
            .expect("reply_reliable returns the real message");
        let msg = out.expect("a real (non-ack) message must be returned, not None");
        assert_eq!(msg.proto.opcode, im::OPCODE_STATUS_RESPONSE);
        assert_eq!(msg.proto.exchange_id, REQ_EXCHANGE);
        assert_eq!(im::decode_status_response(&msg.payload).unwrap(), 0);
        dev_task.await.unwrap();
    }

    /// レビュー対応（Important）: cross-exchange secured request の
    /// ack-then-drop。real chip/Echo 系コミッショナーは standalone ack を
    /// 単独で送らず、次のリクエストの ack フィールドに我々への ack を
    /// piggyback することがあり、それが（この応答とは別の）新しい exchange
    /// に乗ることも珍しくない。メッセージカウンタはセッションスコープ
    /// （`screen_with`, `last_peer_ack`）なので、`reply_reliable` の ack 待ちは
    /// その別 exchange のメッセージだけでも完了しなければならない。かつ、
    /// その新規リクエスト自体は screen_with が認証・ack 済みである以上
    /// （ピアはもう再送しない）破棄されてはならず、`recv_request` で
    /// 取り出せる必要がある。
    #[tokio::test]
    async fn reply_reliable_completes_via_cross_exchange_piggyback_ack() {
        use crate::im;
        let device = bind_local().await;
        let dev_addr = device.local_addr().unwrap();
        let s_transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let s_addr = s_transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&s_transport),
            dev_addr,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        const REQ_EXCHANGE: u16 = 0x1111;
        const NEW_EXCHANGE: u16 = 0x2222;
        let resp_payload = invoke_response_status_ok();

        let dev_task = tokio::spawn(async move {
            // デバイスが reply_reliable で応答することになる、最初のリクエスト。
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 400,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: im::OPCODE_INVOKE_REQUEST,
                exchange_id: REQ_EXCHANGE,
                protocol_id: im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let req_dg = seal_message(&R2I, &header, &proto, &[], DEV_NODE).unwrap();
            device.send_to(&req_dg, s_addr).await.unwrap();

            // reply_reliable の応答を受け、その message_counter を控える。
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _) = open_from_controller(&buf[..n]);
            assert_eq!(p.exchange_id, REQ_EXCHANGE);
            assert!(!p.initiator, "reply must be initiator:false");
            assert!(p.needs_ack, "UDP reply must request an ack");

            // standalone ack は送らない。代わりに別 exchange への新規リクエス
            // トの ack フィールドに、この応答への ack を piggyback する。
            let header2 = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 401,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto2 = ProtocolHeader {
                initiator: true,
                needs_ack: true,
                acked_counter: Some(h.message_counter),
                opcode: im::OPCODE_INVOKE_REQUEST,
                exchange_id: NEW_EXCHANGE,
                protocol_id: im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let req2_dg = seal_message(&R2I, &header2, &proto2, &[], DEV_NODE).unwrap();
            device.send_to(&req2_dg, from).await.unwrap();

            // 新規リクエスト自体は needs_ack なので、我々からの standalone ack
            // が来ることを確認する（"ACKed" 側 — dropped であってはならない）。
            let (n2, _) = device.recv_from(&mut buf).await.unwrap();
            let (_, p2, _) = open_from_controller(&buf[..n2]);
            assert_eq!(p2.protocol_id, PROTOCOL_ID_SECURE_CHANNEL);
            assert_eq!(p2.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(p2.exchange_id, NEW_EXCHANGE);
        });

        let req = s
            .recv_request(Duration::from_secs(5))
            .await
            .expect("recv_request over UDP");
        assert_eq!(req.proto.exchange_id, REQ_EXCHANGE);

        let out = s
            .reply_reliable(
                &req,
                im::PROTOCOL_ID_IM,
                im::OPCODE_INVOKE_RESPONSE,
                &resp_payload,
                &fast_cfg(),
            )
            .await
            .expect("reply_reliable must complete via the cross-exchange piggyback ack");
        assert!(
            out.is_none(),
            "a cross-exchange piggyback ack must complete as Ok(None), not be mistaken for a real reply"
        );

        // 新規リクエストは破棄されず、recv_request で取り出せる（peer_initiated
        // 待避経由 — ソケットを新たに読まなくても即座に返るはず）。
        let new_req = s
            .recv_request(Duration::from_millis(500))
            .await
            .expect("the piggybacking request must not be lost");
        assert_eq!(new_req.proto.exchange_id, NEW_EXCHANGE);
        assert_eq!(new_req.proto.opcode, im::OPCODE_INVOKE_REQUEST);

        dev_task.await.unwrap();
    }
}
