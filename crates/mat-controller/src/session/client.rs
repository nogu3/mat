//! IM クライアント（コントローラ役）: read / invoke / timed invoke / write を
//! 1 exchange で完結させる。chunk 結合と StatusResponse の返送も含む。

use crate::exchange::{IncomingMessage, MrpConfig};

use super::{SecureSession, SessionError, IM_RECV_TIMEOUT};

/// チャンク読みの上限。実デバイスの wildcard read は数チャンクで収まる。
/// 上限到達は「デバイスが more_chunks を返し続けている」異常で、打ち切って
/// エラーにする（無限拘束の防止）。
pub(super) const MAX_REPORT_CHUNKS: usize = 64;

impl SecureSession {
    /// Reads a single attribute over the Interaction Model (spec §8.9.2).
    /// If the device's ReportData doesn't suppress the response, this also
    /// sends back the required StatusResponse(SUCCESS) — best-effort: the
    /// read has already succeeded from our side, so we don't fail it if
    /// that ack round-trip itself times out.
    pub async fn read_attribute(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        cfg: &MrpConfig,
    ) -> Result<crate::im::ImValue, SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_read_request(endpoint, cluster, attribute);
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_READ_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_REPORT_DATA => {
                let rd = im::decode_report_data(&msg.payload).map_err(SessionError::Im)?;
                if !rd.suppress_response {
                    // Best-effort close: the read already succeeded (we
                    // have `rd` in hand), so a lost ack on this trailing
                    // StatusResponse must not turn it into an error here —
                    // it's the peer's retransmit problem, not ours.
                    let ok = im::encode_status_response(0);
                    let _ = self
                        .send_reliable(
                            exchange_id,
                            im::PROTOCOL_ID_IM,
                            im::OPCODE_STATUS_RESPONSE,
                            &ok,
                            cfg,
                        )
                        .await;
                }
                if let Some(status) = rd.status {
                    return Err(SessionError::Im(ImError::AttributeStatus(status)));
                }
                rd.value
                    .ok_or(SessionError::Im(ImError::Malformed("no value")))
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

    /// Invokes a single command over the Interaction Model (spec §8.9.4).
    ///
    /// The interaction ends with the InvokeResponse itself: for a
    /// non-chunked response (M2's only case) we send no closing
    /// StatusResponse — this mirrors CHIP's `CommandSender`, where the MRP
    /// ack of the InvokeResponse is what closes the exchange, not a
    /// follow-up message. The InvokeResponseMessage's own `SuppressResponse`
    /// field is intentionally ignored in M2.
    pub async fn invoke(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
        cfg: &MrpConfig,
    ) -> Result<crate::im::InvokeOutcome, SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_invoke_request(endpoint, cluster, command, fields_tlv);
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_INVOKE_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_INVOKE_RESPONSE => {
                let outcome = im::decode_invoke_response(&msg.payload).map_err(SessionError::Im)?;
                if outcome.status != 0 {
                    return Err(SessionError::Im(ImError::CommandStatus {
                        status: outcome.status,
                        cluster_status: outcome.cluster_status,
                    }));
                }
                Ok(outcome)
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

    /// Invokes a single command over the Interaction Model, optionally as a
    /// *timed* invoke (spec §8.5, タイムド呼び出し), and returns the full
    /// `InvokeResponseData` (status plus any CommandFields the device sent
    /// back) rather than the fields-discarding `InvokeOutcome` that `invoke`
    /// returns.
    ///
    /// When `timed_timeout_ms` is `Some(t)`, a `TimedRequest(t)` is sent
    /// first on a freshly allocated exchange, and the following
    /// `InvokeRequest` (TimedRequest flag set) is sent on that *same*
    /// exchange — spec §8.5.1 requires the timed action to arrive on the
    /// exchange the TimedRequest opened, within the window it establishes.
    /// A non-SUCCESS `StatusResponse` to the TimedRequest itself aborts
    /// before the InvokeRequest is ever sent. When `timed_timeout_ms` is
    /// `None`, this sends a plain (non-timed) InvokeRequest, identical to
    /// `invoke`'s own request — only the response decoding differs.
    pub async fn invoke_for_data(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
        timed_timeout_ms: Option<u16>,
        cfg: &MrpConfig,
    ) -> Result<crate::im::InvokeResponseData, SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();

        if let Some(timeout_ms) = timed_timeout_ms {
            self.send_timed_request(exchange_id, timeout_ms, cfg)
                .await?;
        }

        let req = if timed_timeout_ms.is_some() {
            im::encode_invoke_request_timed(endpoint, cluster, command, fields_tlv)
        } else {
            im::encode_invoke_request(endpoint, cluster, command, fields_tlv)
        };
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_INVOKE_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_INVOKE_RESPONSE => {
                let data =
                    im::decode_invoke_response_data(&msg.payload).map_err(SessionError::Im)?;
                if data.status != 0 {
                    return Err(SessionError::Im(ImError::CommandStatus {
                        status: data.status,
                        cluster_status: data.cluster_status,
                    }));
                }
                Ok(data)
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

    /// Sends `TimedRequest(timeout_ms)` on `exchange_id` and waits for its
    /// `StatusResponse` (spec §8.5.1), erroring on anything but SUCCESS (0).
    /// Shared by `invoke_for_data`'s and `write_attribute_tlv`'s timed
    /// pre-step — both must open the timeout window on the same exchange the
    /// following InvokeRequest/WriteRequest is sent on.
    async fn send_timed_request(
        &mut self,
        exchange_id: u16,
        timeout_ms: u16,
        cfg: &MrpConfig,
    ) -> Result<(), SessionError> {
        use crate::im::{self, ImError};
        let timed_req = im::encode_timed_request(timeout_ms);
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_TIMED_REQUEST,
                &timed_req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                if s != 0 {
                    return Err(SessionError::Im(ImError::StatusResponse(s)));
                }
                Ok(())
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

    /// Drives a ReadRequest's response through any `MoreChunkedMessages`
    /// continuation (spec §8.9.2), collecting every `ReportDataMessage`
    /// chunk. `first` is the message already received in response to the
    /// initial request (either the piggybacked reply or the standalone
    /// `recv` fallback, same pattern as every other IM exchange here).
    async fn collect_reports(
        &mut self,
        exchange_id: u16,
        first: IncomingMessage,
        cfg: &MrpConfig,
    ) -> Result<Vec<crate::im::ReportDataMessage>, SessionError> {
        use crate::im;
        let mut msgs = Vec::new();
        let mut msg = first;
        loop {
            match msg.proto.opcode {
                im::OPCODE_REPORT_DATA => {
                    let rd =
                        im::decode_report_data_message(&msg.payload).map_err(SessionError::Im)?;
                    let more = rd.more_chunks;
                    let suppress = rd.suppress_response;
                    msgs.push(rd);
                    if msgs.len() > MAX_REPORT_CHUNKS {
                        return Err(SessionError::Im(im::ImError::Malformed(
                            "too many report chunks",
                        )));
                    }
                    if more {
                        // Chunk continuation: a StatusResponse(0) prompts
                        // the device to send the next chunk.
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
                        continue;
                    }
                    if !suppress {
                        // Best-effort close of the final chunk, same
                        // rationale as `read_attribute`'s trailing
                        // StatusResponse: the data is already in hand, so a
                        // lost ack here must not turn a successful read into
                        // an error.
                        let ok = im::encode_status_response(0);
                        let _ = self
                            .send_reliable(
                                exchange_id,
                                im::PROTOCOL_ID_IM,
                                im::OPCODE_STATUS_RESPONSE,
                                &ok,
                                cfg,
                            )
                            .await;
                    }
                    return Ok(msgs);
                }
                im::OPCODE_STATUS_RESPONSE => {
                    let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                    return Err(SessionError::Im(im::ImError::StatusResponse(s)));
                }
                op => return Err(SessionError::UnexpectedOpcode(op)),
            }
        }
    }

    /// Reads a single attribute (spec §8.9.2), chunk-aware, returning its
    /// value as JSON via `im::tlv_element_to_json`'s conventions (see
    /// `im::merge_reports`). Unlike `read_attribute` (M2, scalar-only), this
    /// accepts any TLV shape (struct/array/list) and reassembles
    /// `MoreChunkedMessages` chunks. A status-only report (device rejected
    /// the read) surfaces as `ImError::AttributeStatus`.
    pub async fn read_attribute_json(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        cfg: &MrpConfig,
    ) -> Result<serde_json::Value, SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_read_request(endpoint, cluster, attribute);
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_READ_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        let msgs = self.collect_reports(exchange_id, msg, cfg).await?;
        if let Some((_, value)) = im::merge_reports(&msgs).into_iter().next() {
            return Ok(value);
        }
        // No data reports: a single-attribute read that came back
        // status-only means the device rejected it — surface that status
        // rather than a generic "no value" if we have one in hand.
        let status = msgs
            .iter()
            .flat_map(|m| m.reports.iter())
            .find_map(|r| r.status);
        Err(match status {
            Some(s) => SessionError::Im(ImError::AttributeStatus(s)),
            None => SessionError::Im(ImError::Malformed("no value")),
        })
    }

    /// Wildcard-reads every attribute of a cluster (spec §8.9.2), chunk-aware
    /// (see `read_attribute_json`). Returns `(attribute_id, JSON value)`
    /// pairs in first-seen order, per `im::merge_reports`.
    pub async fn read_cluster_json(
        &mut self,
        endpoint: u16,
        cluster: u32,
        cfg: &MrpConfig,
    ) -> Result<Vec<(u32, serde_json::Value)>, SessionError> {
        use crate::im;
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_read_request_cluster(endpoint, cluster);
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_READ_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        let msgs = self.collect_reports(exchange_id, msg, cfg).await?;
        Ok(im::merge_reports(&msgs))
    }

    /// Writes a single attribute (spec §8.9.2.4). `data_tlv` must be one
    /// complete, well-formed TLV element holding the new value (any
    /// top-level tag; `im::encode_write_request_tlv` re-tags it). When
    /// `timed_ms` is `Some(t)`, sends `TimedRequest(t)` first on the same
    /// exchange (spec §8.5.1) — same pre-step as `invoke_for_data`'s timed
    /// path, via `send_timed_request`. A non-zero `AttributeStatusIB` status
    /// in the WriteResponse is returned as `ImError::AttributeStatus`.
    pub async fn write_attribute_tlv(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        data_tlv: &[u8],
        timed_ms: Option<u16>,
        cfg: &MrpConfig,
    ) -> Result<(), SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();

        if let Some(timeout_ms) = timed_ms {
            self.send_timed_request(exchange_id, timeout_ms, cfg)
                .await?;
        }

        let req = if timed_ms.is_some() {
            im::encode_write_request_tlv_timed(endpoint, cluster, attribute, data_tlv)
        } else {
            im::encode_write_request_tlv(endpoint, cluster, attribute, data_tlv)
        };
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_WRITE_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_WRITE_RESPONSE => {
                let status = im::decode_write_response(&msg.payload).map_err(SessionError::Im)?;
                if status != 0 {
                    return Err(SessionError::Im(ImError::AttributeStatus(status)));
                }
                Ok(())
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::OPCODE_MRP_STANDALONE_ACK;
    use crate::session::test_util::*;
    use crate::transport::{Transport, MAX_DATAGRAM};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn read_attribute_roundtrip() {
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

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);
            // ack the request while carrying the real ReportData reply.
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                Some(h.message_counter),
                true,
                9100,
                &report_data_payload(true, true), // suppress=true: no StatusResponse expected back
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let value = s
            .read_attribute(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::ATTR_ON_OFF,
                &fast_cfg(),
            )
            .await
            .unwrap();
        assert_eq!(value, crate::im::ImValue::Bool(true));
        dev.await.unwrap();
    }

    /// Regression for the read closing-ack: `read_attribute`'s own read has
    /// already succeeded once we've decoded the device's ReportData. The
    /// trailing StatusResponse(SUCCESS) we send back is a courtesy close of
    /// the exchange — if the device never acks it, that must NOT turn an
    /// already-successful read into a `Timeout` error.
    #[tokio::test]
    async fn read_attribute_succeeds_even_if_closing_status_response_unacked() {
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

        // Fast MRP so the unacked closing send exhausts its retry budget
        // quickly instead of stalling the test.
        let cfg = MrpConfig {
            initial_interval: Duration::from_millis(50),
            active_interval: Duration::from_millis(50),
            max_retries: 1,
            backoff: 1.0,
            jitter: 0.0,
        };

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);
            // Ack the request while carrying the real ReportData reply.
            // suppress_response = false → the controller will try to close
            // the exchange with a StatusResponse(SUCCESS) that we
            // deliberately never ack, and never send anything else.
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                Some(h.message_counter),
                true,
                9400,
                &report_data_payload(true, false),
            );
            device.send_to(&resp, from).await.unwrap();

            // Drain (and discard) whatever the controller sends next: the
            // standalone ack for this ReportData, then the closing
            // StatusResponse retried per MrpConfig. Read them so the
            // in-flight sends complete, but never ack any of them — that's
            // the whole point of the test.
            loop {
                let mut b2 = [0u8; MAX_DATAGRAM];
                let recv =
                    tokio::time::timeout(Duration::from_millis(500), device.recv_from(&mut b2))
                        .await;
                let Ok(Ok((n2, _))) = recv else { break };
                let _ = open_from_controller(&b2[..n2]);
            }
        });

        let value = s
            .read_attribute(1, crate::im::CLUSTER_ON_OFF, crate::im::ATTR_ON_OFF, &cfg)
            .await
            .unwrap();
        assert_eq!(value, crate::im::ImValue::Bool(true));
        dev.await.unwrap();
    }

    /// Regression: a non-zero command status in the InvokeResponse must map
    /// to `SessionError::Im(ImError::CommandStatus { .. })`, carrying both
    /// the IM status and (when present) the cluster-specific status.
    #[tokio::test]
    async fn invoke_maps_nonzero_status_to_command_status_error() {
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

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_INVOKE_REQUEST);
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_INVOKE_RESPONSE,
                Some(h.message_counter),
                true,
                9500,
                &invoke_response_error_payload(0x81, Some(0x42)),
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let err = s
            .invoke(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::CMD_ON_OFF_TOGGLE,
                None,
                &fast_cfg(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::Im(crate::im::ImError::CommandStatus {
                status: 0x81,
                cluster_status: Some(0x42),
            })
        ));
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn invoke_roundtrip_and_status_response_error() {
        // Scenario 1: InvokeRequest -> InvokeResponse(status 0) -> Ok.
        {
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

            let dev = tokio::spawn(async move {
                let mut buf = [0u8; MAX_DATAGRAM];
                let (n, from) = device.recv_from(&mut buf).await.unwrap();
                let (h, p, _body) = open_from_controller(&buf[..n]);
                assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
                assert_eq!(p.opcode, crate::im::OPCODE_INVOKE_REQUEST);
                let resp = device_datagram(
                    p.exchange_id,
                    crate::im::PROTOCOL_ID_IM,
                    crate::im::OPCODE_INVOKE_RESPONSE,
                    Some(h.message_counter),
                    true,
                    9200,
                    &invoke_response_success_payload(),
                );
                device.send_to(&resp, from).await.unwrap();
            });

            let out = s
                .invoke(
                    1,
                    crate::im::CLUSTER_ON_OFF,
                    crate::im::CMD_ON_OFF_TOGGLE,
                    None,
                    &fast_cfg(),
                )
                .await
                .unwrap();
            assert_eq!(out.status, 0);
            assert_eq!(out.cluster_status, None);
            dev.await.unwrap();
        }

        // Scenario 2: ReadRequest -> StatusResponse(0x7E ACCESS_DENIED) -> Err.
        {
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

            let dev = tokio::spawn(async move {
                let mut buf = [0u8; MAX_DATAGRAM];
                let (n, from) = device.recv_from(&mut buf).await.unwrap();
                let (h, p, _body) = open_from_controller(&buf[..n]);
                assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
                assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);
                let resp = device_datagram(
                    p.exchange_id,
                    crate::im::PROTOCOL_ID_IM,
                    crate::im::OPCODE_STATUS_RESPONSE,
                    Some(h.message_counter),
                    true,
                    9300,
                    &crate::im::encode_status_response(0x7E),
                );
                device.send_to(&resp, from).await.unwrap();
            });

            let err = s
                .read_attribute(
                    1,
                    crate::im::CLUSTER_ON_OFF,
                    crate::im::ATTR_ON_OFF,
                    &fast_cfg(),
                )
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                SessionError::Im(crate::im::ImError::StatusResponse(0x7E))
            ));
            dev.await.unwrap();
        }
    }

    /// `invoke_for_data` without a timed timeout must send an ordinary
    /// (non-timed) InvokeRequest — same wire shape as `invoke` — and decode
    /// a data-carrying InvokeResponse into `fields_tlv`, which `invoke`'s
    /// `InvokeOutcome` cannot represent.
    #[tokio::test]
    async fn invoke_for_data_untimed_returns_command_fields() {
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

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_INVOKE_REQUEST);
            // TimedRequest フラグ (tag 1) が false のままであること（timed 無し）。
            let mut r = crate::tlv::Reader::new(&body);
            r.next().unwrap(); // struct
            r.next().unwrap(); // SuppressResponse
            let flag = r.next().unwrap().unwrap();
            assert_eq!(
                (flag.tag, flag.value),
                (crate::tlv::Tag::Context(1), crate::tlv::Value::Bool(false))
            );
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_INVOKE_RESPONSE,
                Some(h.message_counter),
                true,
                9600,
                &invoke_response_with_fields_payload(),
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let data = s
            .invoke_for_data(
                1,
                crate::im::CLUSTER_COLOR_CONTROL,
                0x00,
                None,
                None,
                &fast_cfg(),
            )
            .await
            .unwrap();
        assert_eq!(data.status, 0);
        let fields = data.fields_tlv.expect("fields present");
        let mut fr = crate::tlv::Reader::new(&fields);
        assert_eq!(
            fr.next().unwrap().unwrap().value,
            crate::tlv::Value::StructStart
        );
        let e = fr.next().unwrap().unwrap();
        assert_eq!(
            (e.tag, e.value),
            (crate::tlv::Tag::Context(0), crate::tlv::Value::Uint(42))
        );
        dev.await.unwrap();
    }

    /// `invoke_for_data` with a timed timeout must, on the same exchange:
    /// send `TimedRequest(t)` first, wait for `StatusResponse(0)`, then send
    /// the InvokeRequest with its TimedRequest flag set (spec §8.5.1).
    #[tokio::test]
    async fn invoke_for_data_timed_sends_timed_request_then_invoke_with_flag() {
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

        let dev = tokio::spawn(async move {
            // 1. TimedRequest -> StatusResponse(0)
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_TIMED_REQUEST);
            let first_ex = p.exchange_id;
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_STATUS_RESPONSE,
                Some(h.message_counter),
                true,
                9700,
                &crate::im::encode_status_response(0),
            );
            device.send_to(&resp, from).await.unwrap();

            // The StatusResponse we sent asked for its own ack
            // (needs_ack=true, matching real MRP traffic) — drain the
            // controller's standalone ack for it before the next real
            // message.
            let mut ack_buf = [0u8; MAX_DATAGRAM];
            let (ack_n, _) = device.recv_from(&mut ack_buf).await.unwrap();
            let (_, ack_p, _) = open_from_controller(&ack_buf[..ack_n]);
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);

            // 2. InvokeRequest (same exchange, TimedRequest flag true) -> InvokeResponse(status 0)
            let mut buf2 = [0u8; MAX_DATAGRAM];
            let (n2, from2) = device.recv_from(&mut buf2).await.unwrap();
            let (h2, p2, body2) = open_from_controller(&buf2[..n2]);
            assert_eq!(p2.opcode, crate::im::OPCODE_INVOKE_REQUEST);
            assert_eq!(p2.exchange_id, first_ex, "same exchange as TimedRequest");
            let mut r = crate::tlv::Reader::new(&body2);
            r.next().unwrap(); // struct
            r.next().unwrap(); // SuppressResponse
            let flag = r.next().unwrap().unwrap();
            assert_eq!(
                (flag.tag, flag.value),
                (crate::tlv::Tag::Context(1), crate::tlv::Value::Bool(true))
            );
            let resp2 = device_datagram(
                p2.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_INVOKE_RESPONSE,
                Some(h2.message_counter),
                true,
                9701,
                &invoke_response_success_payload(),
            );
            device.send_to(&resp2, from2).await.unwrap();
        });

        let data = s
            .invoke_for_data(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::CMD_ON_OFF_ON,
                None,
                Some(5000),
                &fast_cfg(),
            )
            .await
            .unwrap();
        assert_eq!(data.status, 0);
        assert_eq!(data.fields_tlv, None);
        dev.await.unwrap();
    }

    /// If the device rejects the TimedRequest itself (non-zero
    /// StatusResponse), `invoke_for_data` must abort right there and must
    /// never send the InvokeRequest.
    #[tokio::test]
    async fn invoke_for_data_timed_request_rejected_aborts_before_invoke() {
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

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_TIMED_REQUEST);
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_STATUS_RESPONSE,
                Some(h.message_counter),
                true,
                9800,
                &crate::im::encode_status_response(0x7E), // ACCESS_DENIED
            );
            device.send_to(&resp, from).await.unwrap();

            // The controller still owes us a standalone ack for that
            // needs_ack=true StatusResponse — drain it — but nothing else:
            // no InvokeRequest follows a rejected TimedRequest.
            let mut ack_buf = [0u8; MAX_DATAGRAM];
            let (ack_n, _) = device.recv_from(&mut ack_buf).await.unwrap();
            let (_, ack_p, _) = open_from_controller(&ack_buf[..ack_n]);
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);

            let mut b2 = [0u8; MAX_DATAGRAM];
            let recv =
                tokio::time::timeout(Duration::from_millis(200), device.recv_from(&mut b2)).await;
            assert!(
                recv.is_err(),
                "no further message expected after timed request rejection"
            );
        });

        let err = s
            .invoke_for_data(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::CMD_ON_OFF_ON,
                None,
                Some(5000),
                &fast_cfg(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::Im(crate::im::ImError::StatusResponse(0x7E))
        ));
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn write_attribute_reports_status_zero_as_ok() {
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

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_WRITE_REQUEST);
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_WRITE_RESPONSE,
                Some(h.message_counter),
                true,
                9500,
                &write_response_payload(0),
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let mut w = crate::tlv::Writer::new();
        w.put_uint(crate::tlv::Tag::Anonymous, 128);
        let data_tlv = w.finish();

        s.write_attribute_tlv(
            1,
            crate::im::CLUSTER_ON_OFF,
            crate::im::ATTR_ON_OFF,
            &data_tlv,
            None,
            &fast_cfg(),
        )
        .await
        .unwrap();
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn write_attribute_maps_nonzero_status_to_attribute_status_error() {
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

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_WRITE_REQUEST);
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_WRITE_RESPONSE,
                Some(h.message_counter),
                true,
                9501,
                &write_response_payload(0x87), // CONSTRAINT_ERROR
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let mut w = crate::tlv::Writer::new();
        w.put_uint(crate::tlv::Tag::Anonymous, 999);
        let data_tlv = w.finish();

        let err = s
            .write_attribute_tlv(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::ATTR_ON_OFF,
                &data_tlv,
                None,
                &fast_cfg(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::Im(crate::im::ImError::AttributeStatus(0x87))
        ));
        dev.await.unwrap();
    }

    /// デバイスが報告チャンク上限を超えて more_chunks を返し続ける場合、
    /// `collect_reports` はエラーで打ち切る（無限拘束防止）。
    #[tokio::test]
    async fn read_cluster_json_aborts_on_endless_chunks() {
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

        const ATTR: u32 = 0x0005;

        let dev = tokio::spawn(async move {
            // ReadRequest -> stream of ReportData chunks (all with MoreChunkedMessages=true)
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);

            // Send 70 chunks, each with more_chunks=true
            for chunk_idx in 0..70 {
                let resp = device_datagram(
                    p.exchange_id,
                    crate::im::PROTOCOL_ID_IM,
                    crate::im::OPCODE_REPORT_DATA,
                    if chunk_idx == 0 {
                        Some(h.message_counter)
                    } else {
                        None
                    },
                    true,
                    9999 + chunk_idx,
                    &report_data_message_attr(
                        1,
                        crate::im::CLUSTER_ON_OFF,
                        ATTR,
                        chunk_idx as u64,
                        true, // more_chunks = true for all chunks
                        false,
                    ),
                );
                device.send_to(&resp, from).await.unwrap();

                // Drain standalone ack for needs_ack=true
                let mut ack_buf = [0u8; MAX_DATAGRAM];
                let ack_recv = tokio::time::timeout(
                    Duration::from_millis(500),
                    device.recv_from(&mut ack_buf),
                )
                .await;
                if ack_recv.is_err() {
                    break; // controller already errored
                }

                // Wait for StatusResponse prompt (if not the last chunk)
                let mut prompt_buf = [0u8; MAX_DATAGRAM];
                let prompt_recv = tokio::time::timeout(
                    Duration::from_millis(500),
                    device.recv_from(&mut prompt_buf),
                )
                .await;
                if prompt_recv.is_err() {
                    break; // controller stopped
                }
            }
        });

        let err = s
            .read_cluster_json(1, crate::im::CLUSTER_ON_OFF, &fast_cfg())
            .await
            .unwrap_err();

        // Should fail with "too many report chunks"
        assert!(matches!(
            err,
            SessionError::Im(crate::im::ImError::Malformed("too many report chunks"))
        ));
        dev.await.unwrap();
    }

    /// `read_cluster_json` must follow a `MoreChunkedMessages` continuation
    /// (StatusResponse(0) to prompt the next chunk, per spec §8.9.2) and
    /// merge the resulting reports across chunks via `im::merge_reports`.
    #[tokio::test]
    async fn read_cluster_json_merges_two_chunks() {
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

        const ATTR_A: u32 = 0x0005;
        const ATTR_B: u32 = 0x0006;

        let dev = tokio::spawn(async move {
            // 1. ReadRequest -> ReportData chunk1 (attr A, MoreChunkedMessages=true)
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);
            let resp1 = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                Some(h.message_counter),
                true,
                9900,
                &report_data_message_attr(1, crate::im::CLUSTER_ON_OFF, ATTR_A, 7, true, false),
            );
            device.send_to(&resp1, from).await.unwrap();

            // Our ReportData chunk1 asked for its own ack (needs_ack=true,
            // matching real MRP traffic) — drain the controller's
            // standalone ack for it before the next real message.
            let mut ack_buf = [0u8; MAX_DATAGRAM];
            let (ack_n, _) = device.recv_from(&mut ack_buf).await.unwrap();
            let (_, ack_p, _) = open_from_controller(&ack_buf[..ack_n]);
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);

            // 2. controller prompts the next chunk with StatusResponse(0)
            let mut buf2 = [0u8; MAX_DATAGRAM];
            let (n2, from2) = device.recv_from(&mut buf2).await.unwrap();
            let (h2, p2, _body2) = open_from_controller(&buf2[..n2]);
            assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);

            // 3. ReportData chunk2 (attr B, list-append x2, final chunk, suppressed)
            let resp2 = device_datagram(
                p2.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                Some(h2.message_counter),
                true,
                9901,
                &report_data_message_attr_list_append_2(
                    1,
                    crate::im::CLUSTER_ON_OFF,
                    ATTR_B,
                    10,
                    20,
                    true,
                ),
            );
            device.send_to(&resp2, from2).await.unwrap();
        });

        let got = s
            .read_cluster_json(1, crate::im::CLUSTER_ON_OFF, &fast_cfg())
            .await
            .unwrap();
        assert_eq!(
            got,
            vec![
                (ATTR_A, serde_json::json!(7)),
                (ATTR_B, serde_json::json!([10, 20])),
            ]
        );
        dev.await.unwrap();
    }
}
