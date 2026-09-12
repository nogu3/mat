//! `listen` op: ack 後の接続を占有してフィルタ一致イベントを NDJSON で流し続ける（[`stream_events`]）とそのフィルタ（[`ListenFilter`]）。

use serde_json::json;
use tokio::io::BufReader;
use tokio::sync::broadcast;

use mat_core::error::MatError;
use mat_core::output::now_iso8601;

use crate::subscription::Emitted;

use super::wire::write_line;

/// listen ストリーム: フィルタ一致イベントを NDJSON で流し続ける。lag した
/// listener は黙って欠落させず、エラー行を送って切断する（spec ②）。
/// クライアント切断（EOF）でも抜ける。
pub(super) async fn stream_events(
    mut rx: broadcast::Receiver<Emitted>,
    filter: ListenFilter,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<()> {
    // 配信件数。切断時に「そもそも 1 件も流れていない」のか
    // 「流れていたのにクライアントが消えた」のかを区別するため。
    let mut delivered: u64 = 0;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if !filter.matches(&ev) {
                        continue;
                    }
                    write_line(write_half, &ev.to_json()).await?;
                    delivered += 1;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        skipped = n,
                        delivered,
                        node_id = filter.node_id,
                        "listen client lagged; disconnecting"
                    );
                    let body = json!({
                        "error": { "kind": "other", "detail": "event stream lagged" },
                        "timestamp": now_iso8601(),
                    });
                    write_line(write_half, &body).await?;
                    return Ok(());
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!(
                        delivered,
                        node_id = filter.node_id,
                        reason = "channel_closed",
                        "listen client detached"
                    );
                    return Ok(());
                }
            },
            line = lines.next_line() => {
                // クライアント切断（None/Err）でストリーム終了。listen 中の追加
                // リクエスト行は無視する（この op は接続占有の例外）。
                match line {
                    Ok(Some(_)) => continue,
                    _ => {
                        tracing::info!(
                            delivered,
                            node_id = filter.node_id,
                            reason = "client_disconnected",
                            "listen client detached"
                        );
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// listen のイベントフィルタ。リクエストの cluster/attribute/event 名はここで
/// 数値へ解決して照合する（イベント側・属性側とも数値を持つ）。属性名・イベント名
/// はいずれも cluster 無しでは解決できない（数値なら可）。`attribute` と `event`
/// は同時指定不可（別クライアントが両方送ってくる可能性があるため、CLI の
/// `conflicts_with` だけに頼らずここでも拒否する）。
#[derive(Debug)]
pub(crate) struct ListenFilter {
    pub(super) node_id: Option<u64>,
    pub(super) endpoint: Option<u16>,
    pub(super) cluster: Option<u32>,
    pub(super) attribute: Option<u32>,
    /// `None` = イベント行フィルタなし（属性行のみ listen、または全省略）。
    /// `Some(None)` = `"*"`（全イベント名）。`Some(Some(id))` = 特定 1 件。
    event: Option<Option<u32>>,
}

impl ListenFilter {
    pub(crate) fn from_op(
        node_id: &Option<u64>,
        endpoint: &Option<u16>,
        cluster: &Option<String>,
        attribute: &Option<String>,
        event: &Option<String>,
    ) -> Result<Self, MatError> {
        if attribute.is_some() && event.is_some() {
            return Err(MatError::parse_error(
                "--attribute and --event are mutually exclusive",
            ));
        }
        let cluster_id = match cluster {
            None => None,
            Some(c) => Some(mat_core::ids::resolve_cluster(c).ok_or_else(|| {
                MatError::parse_error(format!(
                    "unknown cluster name {c:?}; numeric IDs are accepted"
                ))
            })?),
        };
        let attribute_id =
            match attribute {
                None => None,
                Some(a) => match cluster_id {
                    Some(cid) => Some(
                        mat_core::ids::resolve_attribute(cid, a)
                            .ok_or_else(|| {
                                MatError::parse_error(format!(
                                    "unknown attribute name {a:?}; numeric IDs are accepted"
                                ))
                            })?
                            .id,
                    ),
                    None => match mat_core::ids::parse_num(a) {
                        Some(n) => Some(
                            u32::try_from(n)
                                .map_err(|_| MatError::parse_error("attribute id out of range"))?,
                        ),
                        None => return Err(MatError::parse_error(
                            "attribute name filter requires a cluster filter (or use a numeric id)",
                        )),
                    },
                },
            };
        let event_id: Option<Option<u32>> = match event {
            None => None,
            Some(e) if e == "*" => Some(None),
            Some(e) => Some(Some(match cluster_id {
                Some(cid) => {
                    mat_core::ids::resolve_event(cid, e)
                        .ok_or_else(|| {
                            MatError::parse_error(format!(
                                "unknown event name {e:?}; numeric IDs are accepted"
                            ))
                        })?
                        .id
                }
                None => match mat_core::ids::parse_num(e) {
                    Some(n) => u32::try_from(n)
                        .map_err(|_| MatError::parse_error("event id out of range"))?,
                    None => {
                        return Err(MatError::parse_error(
                            "event name filter requires a cluster filter (or use a numeric id)",
                        ))
                    }
                },
            })),
        };
        Ok(Self {
            node_id: *node_id,
            endpoint: *endpoint,
            cluster: cluster_id,
            attribute: attribute_id,
            event: event_id,
        })
    }

    /// 属性行は node/endpoint/cluster/attribute の一致に加え `--event` 指定
    /// listen には流さない（`event.is_none()`）。イベント行は `--attribute`
    /// 指定 listen には流さない（イベントに attribute は無い）ことに加え、
    /// node/endpoint/cluster が一致し、かつ `event` フィルタが無い / `"*"` /
    /// 指定イベント ID と一致のいずれかを満たすときだけ流す（spec §6.1）。
    pub(crate) fn matches(&self, ev: &Emitted) -> bool {
        match ev {
            Emitted::Attribute(ev) => {
                self.event.is_none()
                    && self.node_id.is_none_or(|n| n == ev.node_id)
                    && self.endpoint.is_none_or(|e| e == ev.endpoint)
                    && self.cluster.is_none_or(|c| c == ev.cluster)
                    && self.attribute.is_none_or(|a| a == ev.attribute)
            }
            Emitted::Event(ev) => {
                self.attribute.is_none()
                    && self.node_id.is_none_or(|n| n == ev.node_id)
                    && self.endpoint.is_none_or(|e| e == ev.endpoint)
                    && self.cluster.is_none_or(|c| c == ev.cluster)
                    && match self.event {
                        None => true,
                        Some(None) => true,
                        Some(Some(id)) => id == ev.event,
                    }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_filter_matches_by_resolved_ids() {
        use crate::subscription::Event;
        let ev = Emitted::Attribute(Event {
            timestamp: "2026-07-20T00:00:00+09:00".to_string(),
            node_id: 21,
            endpoint: 1,
            cluster: 0x0406,
            attribute: 0x0000,
            value: serde_json::json!(1),
            priming: false,
            recovered: false,
        });
        let f = ListenFilter::from_op(
            &Some(21),
            &Some(1),
            &Some("occupancysensing".into()),
            &Some("occupancy".into()),
            &None,
        )
        .unwrap();
        assert!(f.matches(&ev));
        // node 不一致
        let f = ListenFilter::from_op(&Some(22), &None, &None, &None, &None).unwrap();
        assert!(!f.matches(&ev));
        // 全省略 = 全イベント
        let f = ListenFilter::from_op(&None, &None, &None, &None, &None).unwrap();
        assert!(f.matches(&ev));
        // 数値 cluster/attribute も可
        let f = ListenFilter::from_op(
            &None,
            &None,
            &Some("0x0406".into()),
            &Some("0".into()),
            &None,
        )
        .unwrap();
        assert!(f.matches(&ev));
        // 未知 cluster 名は parse_error
        let err =
            ListenFilter::from_op(&None, &None, &Some("nosuch".into()), &None, &None).unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
        // 属性名フィルタは cluster 無しでは解決できない（数値なら可）
        let err = ListenFilter::from_op(&None, &None, &None, &Some("occupancy".into()), &None)
            .unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
        let f = ListenFilter::from_op(&None, &None, &None, &Some("0".into()), &None).unwrap();
        assert!(f.matches(&ev));
    }

    /// イベント行のフィルタ規則: node / endpoint / cluster は属性行と同じに
    /// 掛かるが、`--attribute` を指定した listen には流れない（イベントに
    /// attribute は無い）。`--event` によるさらなる絞り込みは後続のテストで
    /// 検証する（spec §6.1）。
    #[test]
    fn listen_filter_event_lines_match_by_cluster_but_never_with_an_attribute_filter() {
        let ev = Emitted::Event(crate::subscription::EventItem {
            timestamp: "2026-09-06T21:00:00+09:00".to_string(),
            node_id: 25,
            endpoint: 2,
            cluster: 0x003B,
            event: 0x01,
            event_number: 7,
            priority: mat_controller::im::EventPriority::Info,
            data: None,
            device_time: None,
            priming: false,
        });
        // 全省略 = 属性行もイベント行も流れる。
        assert!(ListenFilter::from_op(&None, &None, &None, &None, &None)
            .unwrap()
            .matches(&ev));
        assert!(
            ListenFilter::from_op(&Some(25), &Some(2), &Some("switch".into()), &None, &None)
                .unwrap()
                .matches(&ev)
        );
        // node / endpoint / cluster の不一致は落とす。
        for f in [
            ListenFilter::from_op(&Some(24), &None, &None, &None, &None).unwrap(),
            ListenFilter::from_op(&None, &Some(1), &None, &None, &None).unwrap(),
            ListenFilter::from_op(&None, &None, &Some("onoff".into()), &None, &None).unwrap(),
        ] {
            assert!(!f.matches(&ev));
        }
        // 属性フィルタ付きの listen にイベント行は流れない。
        assert!(!ListenFilter::from_op(
            &None,
            &None,
            &Some("switch".into()),
            &Some("current-position".into()),
            &None,
        )
        .unwrap()
        .matches(&ev));
    }

    /// `ListenFilter::from_op` は `attribute` と `event` の同時指定を拒否する
    /// （CLI の `conflicts_with` を通らない別クライアントからの直送も想定）。
    #[test]
    fn listen_filter_rejects_attribute_and_event_together() {
        let err = ListenFilter::from_op(
            &None,
            &None,
            &None,
            &Some("occupancy".into()),
            &Some("*".into()),
        )
        .unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
    }

    /// `--event` フィルタの 2x2: 属性フィルタ設定時 / イベントフィルタ設定時 ×
    /// 属性行 / イベント行。加えて `"*"`（全イベント名）と特定イベント ID の
    /// 一致・不一致を確認する（spec §6.1）。
    #[test]
    fn listen_filter_event_filter_2x2_and_specific_id() {
        use crate::subscription::Event;
        let attr_ev = Emitted::Attribute(Event {
            timestamp: "2026-07-20T00:00:00+09:00".to_string(),
            node_id: 21,
            endpoint: 1,
            cluster: 0x0406,
            attribute: 0x0000,
            value: serde_json::json!(1),
            priming: false,
            recovered: false,
        });
        let event_ev = Emitted::Event(crate::subscription::EventItem {
            timestamp: "2026-09-06T21:00:00+09:00".to_string(),
            node_id: 25,
            endpoint: 2,
            cluster: 0x003B, // switch
            event: 0x01,     // initial-press
            event_number: 7,
            priority: mat_controller::im::EventPriority::Info,
            data: None,
            device_time: None,
            priming: false,
        });

        // attribute フィルタ設定時: 属性行にマッチ、イベント行には絶対マッチしない。
        let attr_filter = ListenFilter::from_op(
            &None,
            &None,
            &Some("occupancysensing".into()),
            &Some("occupancy".into()),
            &None,
        )
        .unwrap();
        assert!(attr_filter.matches(&attr_ev));
        assert!(!attr_filter.matches(&event_ev));

        // event フィルタ設定時（"*"）: 属性行には絶対マッチしない、イベント行にはマッチする。
        let wildcard_event_filter =
            ListenFilter::from_op(&None, &None, &None, &None, &Some("*".into())).unwrap();
        assert!(!wildcard_event_filter.matches(&attr_ev));
        assert!(wildcard_event_filter.matches(&event_ev));

        // event フィルタ設定時（特定 ID、名前解決）: 一致するイベントにのみマッチ。
        let specific_event_filter = ListenFilter::from_op(
            &None,
            &None,
            &Some("switch".into()),
            &None,
            &Some("initial-press".into()),
        )
        .unwrap();
        assert!(!specific_event_filter.matches(&attr_ev));
        assert!(specific_event_filter.matches(&event_ev));

        // 別のイベント ID を指定すると不一致。
        let other_event_filter = ListenFilter::from_op(
            &None,
            &None,
            &Some("switch".into()),
            &None,
            &Some("0x06".into()), // multi-press-complete
        )
        .unwrap();
        assert!(!other_event_filter.matches(&event_ev));
    }
}
