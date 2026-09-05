//! MRP 層（spec §4.12）: seal / open、RxWindow dedup、standalone ack、
//! piggyback ack、再送付きの `send_reliable` / `recv`。IM の意味は知らない。

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;

use crate::crypto::{open_message, seal_message, OpenError};
use crate::exchange::{IncomingMessage, MrpConfig};
use crate::message::{
    Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, OPCODE_STATUS_REPORT,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use crate::transport::MAX_DATAGRAM;

use super::{SecureSession, SessionError};

/// `screen_with` の配送フィルタ。ack/dedup はフィルタに依らず常に行う。
#[derive(Clone, Copy)]
pub(super) enum ScreenFilter {
    /// 自分が initiator の exchange 宛て（従来動作）。
    OurExchange(u16),
    /// デバイスが initiator の特定 exchange 宛て（購読 report への応答 ack 待ち用）。
    PeerExchange(u16),
    /// デバイス起点 exchange 全部（購読ポンプの report 待ち用）。
    AnyPeerInitiated,
}

/// `peer_initiated` バッファの上限。超過時は最古を捨てる。
pub(super) const MAX_PEER_INITIATED_BUFFER: usize = 32;

impl SecureSession {
    /// Seals a message for the peer; returns the datagram and the plaintext
    /// message counter used (so callers can match it against an ack).
    ///
    /// `initiator` marks our role on `exchange_id`. Our own requests always
    /// carry `true` (we are always the exchange initiator in M2). Standalone
    /// acks may need `false`: when acking a message on an exchange the
    /// *device* initiated (e.g. a device-initiated secured StatusReport), we
    /// are the non-initiator of that exchange, and the peer can only match
    /// our ack if it carries that role correctly.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn seal(
        &mut self,
        exchange_id: u16,
        initiator: bool,
        protocol_id: u16,
        opcode: u8,
        needs_ack: bool,
        acked_counter: Option<u32>,
        payload: &[u8],
    ) -> Result<(Vec<u8>, u32), SessionError> {
        let needs_ack = needs_ack && !self.transport.is_reliable();
        // ピア発 needs_ack メッセージへの pending ack を、同一 exchange への
        // 次の送信に piggyback する（呼び出し元が明示した ack が優先）。
        // standalone ack は screen で即時送信済みだが、それが RF で失われた
        // ときの唯一の回収経路がこの piggyback（Issue #21）。冪等なので
        // 二重 ack は無害。消費は 1 回（stale な exchange id との偶然一致で
        // 古い counter を ack し続けないため）。
        let acked_counter = match acked_counter {
            Some(c) => Some(c),
            None => match self.pending_peer_ack {
                Some((ex, c)) if ex == exchange_id => {
                    self.pending_peer_ack = None;
                    Some(c)
                }
                _ => None,
            },
        };
        let message_counter = self.counter.next();
        let header = MessageHeader {
            session_id: self.peer_session_id,
            security_flags: 0,
            message_counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator,
            needs_ack,
            acked_counter,
            opcode,
            exchange_id,
            protocol_id,
            vendor_id: None,
        };
        let datagram = seal_message(&self.keys.i2r, &header, &proto, payload, self.local_node_id)?;
        Ok((datagram, message_counter))
    }

    /// Sends a standalone ack for `acked` on `exchange_id`, with our role on
    /// that exchange given explicitly by `initiator` (see `seal`'s doc for
    /// why this can't just be assumed to be `true`).
    async fn send_standalone_ack(
        &mut self,
        exchange_id: u16,
        initiator: bool,
        acked: u32,
    ) -> Result<(), SessionError> {
        let (datagram, _) = self.seal(
            exchange_id,
            initiator,
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_MRP_STANDALONE_ACK,
            false,
            Some(acked),
            &[],
        )?;
        tracing::debug!(
            exchange_id,
            initiator,
            acked,
            peer = %self.peer,
            "sending standalone ack"
        );
        self.transport.send_to(&datagram, self.peer).await?;
        Ok(())
    }

    /// CloseSession（StatusReport SUCCESS/secure channel/2）を best-effort で 1 発
    /// 送る。放置セッションは FP300 系 FW の購読レポート付け替えで常駐購読を
    /// 黙殺する（Issue #20）ため、セッションを手放す全経路がこれを呼ぶ。
    /// MRP に乗せない（teardown を ~4.7s の再送予算でブロックしない —
    /// pase.rs の abort StatusReport と同じ判断）。失敗は握りつぶす。
    pub async fn send_close_session(&mut self) {
        let payload = crate::case::encode_status_report(
            0,
            u32::from(PROTOCOL_ID_SECURE_CHANNEL),
            crate::case::SC_PROTOCOL_CODE_CLOSE_SESSION,
        );
        let sealed = self.seal(
            Self::new_exchange_id(),
            true,
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_STATUS_REPORT,
            false,
            None,
            &payload,
        );
        if let Ok((datagram, _)) = sealed {
            let _ = self.transport.send_to(&datagram, self.peer).await;
            tracing::debug!(peer = %self.peer, "sent CloseSession");
        }
    }

    /// Decrypts a datagram and screens it for the given exchange. Returns
    /// `None` for foreign or duplicate traffic the caller should skip
    /// (duplicates are re-acked here). Standalone acks pass screening and
    /// are returned as `Some`; callers filter them by opcode. Thin wrapper
    /// around `screen_with` for the common "our own exchange" case, kept so
    /// `send_reliable`/`recv` are unaffected by the filter generalization.
    async fn screen(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
        exchange_id: u16,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        self.screen_with(buf, from, ScreenFilter::OurExchange(exchange_id))
            .await
    }

    /// Decrypts a datagram and screens it per `filter`. Returns `None` for
    /// foreign or duplicate traffic, or traffic that fails the delivery
    /// filter (duplicates are re-acked here). Ack/dedup happen unconditionally
    /// before the filter is applied — an authenticated needs_ack message is
    /// acked as soon as it's decoded, regardless of whether it will be
    /// delivered. Any peer-initiated message (standalone acks excepted —
    /// they carry nothing to serve) that fails the filter is therefore
    /// *buffered* (`peer_initiated`) rather than dropped: it has already
    /// been acked, so dropping it here would be a permanent loss (a
    /// cross-exchange request piggybacked on our reply's ack, a
    /// subscription's ReportData chunk arriving during an unrelated
    /// ack-wait, etc.).
    pub(super) async fn screen_with(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
        filter: ScreenFilter,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        if from != self.peer {
            // 共有 op socket では他セッション宛の cross-traffic で正常に起きる。
            // 購読専用 socket では「デバイスが別ソースアドレスから送っている」
            // 兆候なので、切り分け時は trace で可視化する。
            tracing::trace!(%from, peer = %self.peer, "screen: datagram from foreign address; ignored");
            return Ok(None);
        }
        // 平文ヘッダだけ先に見て session id を確認する（復号前フィルタ）。
        // decode 失敗（DSIZ 予約値含む）や session id 不一致は不正/他セッション
        // のデータグラムとして無視する（DoS 耐性、エラーを伝播しない）。
        let header_peek = match MessageHeader::decode(buf) {
            Ok((h, _)) => h,
            Err(_) => {
                tracing::trace!(%from, "screen: undecodable header; ignored");
                return Ok(None);
            }
        };
        if header_peek.session_id != self.local_session_id {
            tracing::trace!(
                session_id = header_peek.session_id,
                ours = self.local_session_id,
                "screen: session id mismatch; ignored"
            );
            return Ok(None);
        }
        let (header, proto, payload) = match open_message(&self.keys.r2i, buf, self.peer_node_id) {
            Ok(v) => v,
            Err(OpenError::Message(_)) | Err(OpenError::Crypto(_)) => {
                tracing::trace!(%from, "screen: authenticated decrypt failed; ignored");
                return Ok(None);
            }
        };
        // 認証済み受信 = ピアは active。MRP 再送間隔の active/idle 判定に使う
        // （重複再送でも「ピアが生きている」証拠として記録してよい）。
        self.last_rx = Some(Instant::now());
        // ピアの ack フィールドは配送フィルタの可否に関わらず常に記録する
        // （メッセージカウンタはセッションスコープなので、この ack が乗って
        // いた exchange が呼び出し元の待ち exchange と違っても、カウンタが
        // 一致すれば「その送信は確かに届いた」という事実は変わらない —
        // レビュー指摘のcross-exchange piggyback ack 対応）。
        if let Some(acked) = proto.acked_counter {
            self.last_peer_ack = Some(acked);
        }
        // RxWindow の重複検知はセッション単位（exchange 単位ではない）なので、
        // exchange フィルタより前にコミットする（コメント: この順序は意図的）。
        if !self.rx_window.check_and_commit(header.message_counter) {
            // 重複の再送。ack は必ずメッセージ自身の exchange id / role で
            // 発行する — 呼び出し元の filter exchange ではない。他 exchange
            // （デバイス起点など）宛の重複を誤った exchange/role で ack する
            // と相手が突合できず、再送予算を使い切るまでリトライし続ける。
            if proto.needs_ack && !self.transport.is_reliable() {
                // 重複再送 = こちらの ack が届いていない証拠。再 ack に加え、
                // 同一 exchange への次の送信でも piggyback できるよう記録する。
                self.pending_peer_ack = Some((proto.exchange_id, header.message_counter));
                self.send_standalone_ack(
                    proto.exchange_id,
                    !proto.initiator,
                    header.message_counter,
                )
                .await?;
            }
            return Ok(None);
        }
        // 認証済みの新規メッセージは、こちらの exchange 宛かどうかに関わらず
        // needs_ack ならここで ack する（初回配送の時点で ack する — 重複の
        // 再送を待たない）。exchange フィルタは配送の可否だけを決め、ack の
        // 有無には影響しない。
        if proto.needs_ack && !self.transport.is_reliable() {
            self.pending_peer_ack = Some((proto.exchange_id, header.message_counter));
            self.send_standalone_ack(proto.exchange_id, !proto.initiator, header.message_counter)
                .await?;
        }
        let deliver = match filter {
            ScreenFilter::OurExchange(ex) => proto.exchange_id == ex && !proto.initiator,
            ScreenFilter::PeerExchange(ex) => proto.exchange_id == ex && proto.initiator,
            ScreenFilter::AnyPeerInitiated => proto.initiator,
        };
        if !deliver {
            tracing::trace!(
                exchange_id = proto.exchange_id,
                initiator = proto.initiator,
                opcode = proto.opcode,
                "screen: delivery filter miss"
            );
            // フィルタ落ちでも peer-initiated メッセージは ack 済みなので待避する
            // (standalone ack 自体は除く — それ自体は待避対象ではなく、ここまで
            // 来る頃には ack/dedup 処理も済んでいる)。以前は device 発
            // ReportData のみを待避しており、それ以外の opcode（例: 別
            // exchange への新規リクエストの piggyback ack で ack だけ届いて
            // 中身は別 exchange のケース）は ack 済みのまま黙って捨てられ、
            // ピアは再送しないため永久喪失していた（レビュー指摘: cross-
            // exchange secured request のack-then-drop）。
            let is_standalone_ack = proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                && proto.opcode == OPCODE_MRP_STANDALONE_ACK;
            if proto.initiator && !is_standalone_ack {
                if self.peer_initiated.len() >= MAX_PEER_INITIATED_BUFFER {
                    tracing::warn!("peer-initiated report buffer full; dropping oldest");
                    self.peer_initiated.pop_front();
                }
                self.peer_initiated.push_back(IncomingMessage {
                    header,
                    proto,
                    payload,
                });
            }
            return Ok(None);
        }
        Ok(Some(IncomingMessage {
            header,
            proto,
            payload,
        }))
    }

    /// Sends a reliability-flagged message and retransmits until the peer
    /// acknowledges it. Returns the peer's real response if one carried the
    /// ack (or arrived on the exchange), `None` for a standalone ack.
    pub async fn send_reliable(
        &mut self,
        exchange_id: u16,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        if self.transport.is_reliable() {
            // BTP: transport が信頼性を持つ。1 回送って実応答を待つだけ。
            let (datagram, _) =
                self.seal(exchange_id, true, protocol_id, opcode, false, None, payload)?;
            self.transport.send_to(&datagram, self.peer).await?;
            let budget = crate::exchange::total_budget(cfg);
            return match self.recv(exchange_id, budget).await {
                Ok(msg) => Ok(Some(msg)),
                Err(e) => Err(e),
            };
        }
        let (datagram, our_counter) =
            self.seal(exchange_id, true, protocol_id, opcode, true, None, payload)?;
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
                    break; // interval 経過 → 再送
                };
                let (n, from) = recv?;
                let Some(msg) = self.screen(&buf[..n], from, exchange_id).await? else {
                    continue;
                };
                let acks_us = msg.proto.acked_counter == Some(our_counter);
                let is_standalone_ack = msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                    && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK;
                if is_standalone_ack {
                    if acks_us {
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

    /// Waits for the next real (non-ack) message on the given exchange.
    pub async fn recv(
        &mut self,
        exchange_id: u16,
        timeout: Duration,
    ) -> Result<IncomingMessage, SessionError> {
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
            let Some(msg) = self.screen(&buf[..n], from, exchange_id).await? else {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_util::*;
    use crate::transport::Transport;
    use std::sync::Arc;

    #[tokio::test]
    async fn send_reliable_encrypts_and_completes_on_sealed_ack() {
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
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            // 平文では読めない（先頭ヘッダ以外は暗号化されている）
            let (h, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(h.session_id, PEER_SID); // デバイス側 session id 宛
            assert!(p.needs_ack);
            assert_eq!(body, b"ping");
            let ack = device_datagram(
                p.exchange_id,
                PROTOCOL_ID_SECURE_CHANNEL,
                OPCODE_MRP_STANDALONE_ACK,
                Some(h.message_counter),
                false,
                9000,
                &[],
            );
            device.send_to(&ack, from).await.unwrap();
        });

        let res = s
            .send_reliable(ex, PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"ping", &fast_cfg())
            .await
            .unwrap();
        assert!(res.is_none());
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn recv_decrypts_dedups_and_acks() {
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
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            let msg = device_datagram(
                ex,
                PROTOCOL_ID_SECURE_CHANNEL,
                OPCODE_STATUS_REPORT,
                None,
                true,
                500,
                b"report",
            );
            device.send_to(&msg, local).await.unwrap();
            device.send_to(&msg, local).await.unwrap(); // 重複
                                                        // ACK は暗号化されて 2 回返る
            for _ in 0..2 {
                let mut buf = [0u8; MAX_DATAGRAM];
                let (n, _) = device.recv_from(&mut buf).await.unwrap();
                let (_, p, _) = open_from_controller(&buf[..n]);
                assert_eq!(p.opcode, OPCODE_MRP_STANDALONE_ACK);
                assert_eq!(p.acked_counter, Some(500));
            }
        });

        let got = s.recv(ex, Duration::from_millis(500)).await.unwrap();
        assert_eq!(got.payload, b"report");
        // 重複は渡ってこない
        assert!(matches!(
            s.recv(ex, Duration::from_millis(200)).await,
            Err(SessionError::Timeout)
        ));
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn ignores_wrong_key_wrong_session_and_wrong_exchange() {
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
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            // 鍵違い（I2R で封緘 = 復号失敗）
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 1,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: false,
                needs_ack: true,
                acked_counter: None,
                opcode: OPCODE_STATUS_REPORT,
                exchange_id: ex,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            };
            let bad_key = seal_message(&I2R, &header, &proto, b"x", DEV_NODE).unwrap();
            device.send_to(&bad_key, local).await.unwrap();
            // session id 違い
            let mut h2 = header;
            h2.session_id = 0x9999;
            let bad_sid = seal_message(&R2I, &h2, &proto, b"x", DEV_NODE).unwrap();
            device.send_to(&bad_sid, local).await.unwrap();
            // exchange 違い（正しく封緘されるが screening で落ちる）
            let other_ex = device_datagram(
                ex.wrapping_add(1),
                PROTOCOL_ID_SECURE_CHANNEL,
                OPCODE_STATUS_REPORT,
                None,
                true,
                7,
                b"x",
            );
            device.send_to(&other_ex, local).await.unwrap();
        });

        assert!(matches!(
            s.recv(ex, Duration::from_millis(300)).await,
            Err(SessionError::Timeout)
        ));
        dev.await.unwrap();
    }

    /// Regression for the MRP ack-attribution bug: a needs-ack message that
    /// arrives on an exchange the *device* initiated (foreign to our own
    /// `exchange_id` filter — e.g. a device-initiated secured StatusReport)
    /// must still be acked, and the ack must carry that message's own
    /// exchange id with us as the non-initiator of THEIR exchange — not our
    /// filter's exchange id with `initiator: true`, which the peer could
    /// never match.
    #[tokio::test]
    async fn acks_foreign_exchange_needs_ack_message_with_its_own_exchange_id() {
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
        let ex = SecureSession::new_exchange_id();
        let foreign_ex = ex.wrapping_add(1);

        let dev = tokio::spawn(async move {
            // デバイス起点の別 exchange 上のメッセージ（例: セキュアな
            // StatusReport をデバイス側から自分の exchange で送ってくる
            // ケース）。initiator: true はデバイスが「その exchange の」
            // initiator であることを示す。
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
                opcode: OPCODE_STATUS_REPORT,
                exchange_id: foreign_ex,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            };
            let msg = seal_message(&R2I, &header, &proto, b"foreign", DEV_NODE).unwrap();
            device.send_to(&msg, local).await.unwrap();

            // controller の standalone ack は、そのメッセージ自身の
            // exchange id で、こちらが「その exchange の」non-initiator
            // として返ってくるはず。
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = device.recv_from(&mut buf).await.unwrap();
            let (_, p, _) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(p.exchange_id, foreign_ex);
            assert!(!p.initiator);
            assert_eq!(p.acked_counter, Some(700));
        });

        // こちらの exchange (`ex`) にはこのメッセージは配送されない —
        // 他 exchange 宛だから。
        assert!(matches!(
            s.recv(ex, Duration::from_millis(300)).await,
            Err(SessionError::Timeout)
        ));
        dev.await.unwrap();
    }

    /// CloseSession は needs_ack なしの 1 データグラムで、payload が
    /// StatusReport(SUCCESS, secure channel, CloseSession=2) であること（Issue #20）。
    #[tokio::test]
    async fn close_session_sends_single_best_effort_status_report() {
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
        s.send_close_session().await;
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = device.recv_from(&mut buf).await.unwrap();
        let (_, proto, payload) = open_from_controller(&buf[..n]);
        assert_eq!(proto.protocol_id, PROTOCOL_ID_SECURE_CHANNEL);
        assert_eq!(proto.opcode, OPCODE_STATUS_REPORT);
        assert!(!proto.needs_ack, "CloseSession must be best-effort");
        let (general, proto_id, code) = crate::case::parse_status_report(&payload).unwrap();
        assert_eq!((general, proto_id, code), (0, 0, 2));
        // 再送しないこと（MRP に乗せない）。
        let again =
            tokio::time::timeout(Duration::from_millis(300), device.recv_from(&mut buf)).await;
        assert!(again.is_err(), "CloseSession must not be retransmitted");
    }
}
