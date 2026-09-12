//! listen へ流す **イベント行**（デバイス発 EventReport 由来）と、属性行との
//! 合流点 `Emitted`（spec 2026-09-06-events-subscribe-design §6.1）。
//!
//! 属性行（[`Event`]）は無改変で、イベント行は `attribute` の代わりに
//! `event` キーを持つのが消費者側の判別点。`recovered` は付けない —
//! 属性のような差分推定ではなく EventNumber で欠落を回収するため
//! （§6.2、再購読の EventMin）。

use mat_controller::im::{EventPriority, EventReport, EventTimestamp, ReportDataMessage};
use mat_core::output::now_iso8601;

/// listen へ配る 1 行。属性変化（既存の `Event`）とデバイスイベント
/// （`EventItem`）が同じ broadcast に相乗りする。
#[derive(Debug, Clone)]
pub enum Emitted {
    Attribute(Event),
    Event(EventItem),
}

impl Emitted {
    /// NDJSON 1 行分。どちらの行かは中身が決める（stream ループは分岐しない）。
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Emitted::Attribute(e) => e.to_json(),
            Emitted::Event(e) => e.to_json(),
        }
    }
}

/// listen へ配る 1 イベント行。cluster / event は数値で持ち、JSON 化時に
/// `mat-core::ids` で名前化する（フィルタ照合は数値で行うため — 属性行と
/// 同じ規律）。`timestamp` は report 受信時に一度だけ採取した値
/// （同一 ReportData 由来の属性行・イベント行は同じ文字列）。
#[derive(Debug, Clone)]
pub struct EventItem {
    pub timestamp: String,
    pub node_id: u64,
    pub endpoint: u16,
    pub cluster: u32,
    pub event: u32,
    pub event_number: u64,
    pub priority: EventPriority,
    /// EventDataIB の Data（context tag の 10 進文字列キー）。フィールドの
    /// 無いイベントは `None`。
    pub data: Option<serde_json::Value>,
    /// デバイス側時刻。Delta 形が解決できなかった場合は `Some(Delta*)` の
    /// まま来るので、JSON には出さない（`to_json` 参照）。
    pub device_time: Option<EventTimestamp>,
    /// 起動直後の priming（= デバイスのイベントログ全量）か。再購読の
    /// EventMin で拾った盲目窓中のイベントは実イベントなので `false`（§6.2）。
    pub priming: bool,
}

impl EventItem {
    /// mat スキーマの NDJSON 1 行分（spec §6.1）。
    pub fn to_json(&self) -> serde_json::Value {
        let def = mat_core::ids::find_event(self.cluster, self.event);
        let cluster = match mat_core::ids::find_cluster(self.cluster) {
            Some(c) => serde_json::json!(c.name),
            None => serde_json::json!(self.cluster),
        };
        // 未知 ID は属性行の cluster / attribute と同じく JSON 数値のまま
        //（10 進文字列になるのは `data` のオブジェクトキーだけ — JSON の
        // キーは文字列しか取れないため。spec §6.1）。
        let event = match def {
            Some(d) => serde_json::json!(d.name),
            None => serde_json::json!(self.event),
        };
        let mut out = serde_json::json!({
            "timestamp": self.timestamp.clone(),
            "node_id": self.node_id,
            "endpoint": self.endpoint,
            "cluster": cluster,
            "event": event,
            "event_number": self.event_number,
            "priority": self.priority.as_str(),
            "priming": self.priming,
        });
        let obj = out.as_object_mut().expect("json! object");
        if let Some(data) = &self.data {
            obj.insert("data".to_string(), name_data_fields(data, def));
        }
        if let Some(dt) = self.device_time.and_then(device_time_json) {
            obj.insert("device_time".to_string(), dt);
        }
        out
    }
}

/// デコード済み Data のキー（context tag の 10 進文字列）をイベント
/// フィールド名へ写す。テーブルに無いフィールド（未知イベント含む）は
/// 10 進文字列のまま残す — read の struct 規約と同じ。
fn name_data_fields(
    data: &serde_json::Value,
    def: Option<&'static mat_core::ids::EventDef>,
) -> serde_json::Value {
    let (Some(obj), Some(def)) = (data.as_object(), def) else {
        return data.clone();
    };
    let mut out = serde_json::Map::with_capacity(obj.len());
    for (key, value) in obj {
        // フィールドは**配列添字ではなく id** で引く（data-model XML の
        // field id は 0 起算とは限らない — `EventFieldDef` のコメント）。
        let named = key
            .parse::<u8>()
            .ok()
            .and_then(|id| def.fields.iter().find(|f| f.id == id))
            .map(|f| f.name.to_string())
            .unwrap_or_else(|| key.clone());
        out.insert(named, value.clone());
    }
    serde_json::Value::Object(out)
}

/// デバイス側時刻の JSON。絶対時刻のときだけ値を返す（未解決の Delta は
/// 基準が無く、そのまま出すと epoch と紛らわしいので出さない）。
fn device_time_json(ts: EventTimestamp) -> Option<serde_json::Value> {
    match ts {
        EventTimestamp::System(ms) => Some(serde_json::json!({ "system_ms": ms })),
        EventTimestamp::Epoch(ms) => Some(serde_json::json!({ "epoch_ms": ms })),
        EventTimestamp::DeltaSystem(_) | EventTimestamp::DeltaEpoch(_) => None,
    }
}

/// 1 通の ReportData（または priming 全体）から集めた EventReport 群を
/// listen のイベント行へ。`ts` は呼び手が 1 回だけ採った受信時刻
/// （同一 report 由来の属性行と同じ文字列を共有する）。
///
/// `Status` エントリ（そのイベント path が拒否された）は行にしない:
/// listen の消費者にとっては「起きなかった」と区別できず、行として流す意味が
/// ない。ただし運用側は subscriptions.toml のカナリア時に「拒否されたか」を
/// stderr で確かめる（docs/configuration.md）ので、**この 1 通ぶんにつき
/// warn 1 行**へ集約する（件数 + 先頭の status。1 エントリ 1 行にはしない —
/// 拒否は path 数ぶん一斉に来る）。エントリ個別は debug のまま。
/// 並びは EventNumber 昇順（spec §6.2 — デバイスは昇順で送る建前だが、
/// チャンクをまたいで集めるのでここで確定させる）。
pub fn events_from_event_reports(
    node_id: u64,
    events: &[EventReport],
    priming: bool,
    ts: &str,
) -> Vec<EventItem> {
    let mut out: Vec<EventItem> = Vec::with_capacity(events.len());
    // 拒否された path の集約（件数 + 先頭 1 件）。warn は最後に 1 行だけ出す。
    let mut rejected = 0usize;
    let mut first_reject: Option<&EventReport> = None;
    for rep in events {
        match rep {
            EventReport::Data(d) => out.push(EventItem {
                timestamp: ts.to_string(),
                node_id,
                endpoint: d.endpoint,
                cluster: d.cluster,
                event: d.event,
                event_number: d.event_number,
                priority: d.priority,
                data: d.data.clone(),
                device_time: d.timestamp,
                priming,
            }),
            EventReport::Status {
                endpoint,
                cluster,
                event,
                status,
            } => {
                tracing::debug!(
                    node_id,
                    endpoint = ?endpoint,
                    cluster = ?cluster,
                    event = ?event,
                    status,
                    "dropping event status report"
                );
                rejected += 1;
                first_reject.get_or_insert(rep);
            }
        }
    }
    if let Some(EventReport::Status {
        endpoint,
        cluster,
        event,
        status,
    }) = first_reject
    {
        // 拒否は運用の判断材料（そのイベント path をこのデバイスは持たない /
        // ACL で拒む）なので本番の既定レベルで見えるところに 1 行出す。
        tracing::warn!(
            node_id,
            rejected,
            first_endpoint = ?endpoint,
            first_cluster = ?cluster,
            first_event = ?event,
            first_status = status,
            "device rejected event paths (reports dropped)"
        );
    }
    out.sort_by_key(|e| e.event_number);
    out
}

/// listen へ配る 1 イベント。cluster/attribute は数値で持ち、JSON 化時に
/// chip-tool 記法へ名前化する（フィルタ照合は数値で行うため）。timestamp は
/// report 受信時に一度だけ採取した値を保持する（listener ごと・emit 時刻での
/// 再採取はしない — 同一 report 由来のイベントは全リスナーで同じ時刻を返す）。
#[derive(Debug, Clone)]
pub struct Event {
    pub timestamp: String,
    pub node_id: u64,
    pub endpoint: u16,
    pub cluster: u32,
    pub attribute: u32,
    pub value: serde_json::Value,
    pub priming: bool,
    /// priming 差分回復で昇格したイベント（購読の盲目期間中に起きた実遷移を
    /// 再購読時の priming から検出したもの）。`priming` と直交し、昇格時は
    /// `priming: false` + `recovered: true` になる。timestamp は受信時刻で
    /// あり、実際の遷移時刻ではない。
    pub recovered: bool,
}

impl Event {
    /// mat スキーマの NDJSON 1 行分。cluster/attribute は `mat-core::ids` に
    /// あれば chip-tool 記法名、無ければ数値のまま（read と同じ規律）。
    /// timestamp は report 受信時に採取済みの値をそのまま使う（emit 時刻の
    /// 再採取はしない）。
    pub fn to_json(&self) -> serde_json::Value {
        let cluster = match mat_core::ids::find_cluster(self.cluster) {
            Some(def) => serde_json::json!(def.name),
            None => serde_json::json!(self.cluster),
        };
        let attribute = match mat_core::ids::find_cluster(self.cluster)
            .and_then(|c| c.attrs.iter().find(|a| a.id == self.attribute))
        {
            Some(def) => serde_json::json!(def.name),
            None => serde_json::json!(self.attribute),
        };
        serde_json::json!({
            "timestamp": self.timestamp.clone(),
            "node_id": self.node_id,
            "endpoint": self.endpoint,
            "cluster": cluster,
            "attribute": attribute,
            "value": self.value,
            "priming": self.priming,
            "recovered": self.recovered,
        })
    }
}

/// ReportDataMessage をイベント列へ。scalar 値のみイベント化し、list/struct
/// （ACL・server-list 等 wildcard priming に混ざるもの）は debug ログで捨てる。
/// これは **listen だけの制限**（generic read/write は list/struct を JSON で
/// 扱う）: この経路は ReportData を 1 通ずつ処理しチャンク再組み立て
/// （MoreChunkedMessages + ListIndex:null 追記列）を持たないので途中 list しか
/// 作れず、list-diff priming recovery も要素追記を遷移と誤認する。path が欠けた
/// report・status-only も捨てる。
pub fn events_from_report(node_id: u64, msg: &ReportDataMessage, priming: bool) -> Vec<Event> {
    // 1 report から生まれる全イベントで同じ受信時刻を共有する（listener ごと・
    // emit 時刻での再採取はしない — 同時到着イベントは同じ timestamp が正しい）。
    events_from_report_at(node_id, msg, priming, &now_iso8601())
}

/// `events_from_report` の受信時刻を呼び手が渡す版。同じ ReportData から
/// 出る属性行とイベント行（`events_from_event_reports`）に同一の `timestamp`
/// を持たせるために pump が使う（spec §6.2）。
pub fn events_from_report_at(
    node_id: u64,
    msg: &ReportDataMessage,
    priming: bool,
    ts: &str,
) -> Vec<Event> {
    let mut out = Vec::new();
    for rep in &msg.reports {
        let (Some(endpoint), Some(cluster), Some(attribute)) =
            (rep.endpoint, rep.cluster, rep.attribute)
        else {
            continue;
        };
        let Some(data) = &rep.data else { continue };
        // list 属性は要素ごとに別々の report として届くことがある
        // (AttributePathIB.ListIndex = null → デコーダは list_append: true)。
        // その 1 要素は scalar なので下の is_array/is_object 判定を素通りして
        // しまう — list_append 単体で落とす（list-diff priming recovery が
        // 同一キーへの要素ごとの値変化を実遷移と誤認して recovered を大量発生
        // させる害の元。README の「list/struct attributes are dropped」契約どおり）。
        if rep.list_append {
            tracing::debug!(
                node_id,
                endpoint,
                cluster,
                attribute,
                "dropping list-append element report"
            );
            continue;
        }
        if data.is_array() || data.is_object() {
            tracing::debug!(
                node_id,
                endpoint,
                cluster,
                attribute,
                "dropping non-scalar report"
            );
            continue;
        }
        out.push(Event {
            timestamp: ts.to_string(),
            node_id,
            endpoint,
            cluster,
            attribute,
            value: data.clone(),
            priming,
            recovered: false,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use mat_controller::im::{EventData, EventPriority, EventReport, EventTimestamp};
    use mat_native::test_support::onoff_report;
    use serde_json::json;

    fn item(cluster: u32, event: u32, data: serde_json::Value) -> EventItem {
        EventItem {
            timestamp: "2026-09-06T21:00:00+09:00".to_string(),
            node_id: 25,
            endpoint: 2,
            cluster,
            event,
            event_number: 1_725_600_000_123,
            priority: EventPriority::Info,
            data: Some(data),
            device_time: None,
            priming: false,
        }
    }

    /// spec §6.1 の 1 行目: switch/initial-press。
    #[test]
    fn event_json_switch_initial_press() {
        let j = item(0x003B, 0x01, json!({"0": 1})).to_json();
        assert_eq!(j["timestamp"], "2026-09-06T21:00:00+09:00");
        assert_eq!(j["node_id"], 25);
        assert_eq!(j["endpoint"], 2);
        assert_eq!(j["cluster"], "switch");
        assert_eq!(j["event"], "initial-press");
        assert_eq!(j["event_number"], 1_725_600_000_123u64);
        assert_eq!(j["priority"], "info");
        assert_eq!(j["data"], json!({"new-position": 1}));
        assert_eq!(j["priming"], false);
        // イベントは EventNumber で欠落回収するので recovered は付けない。
        assert!(j.get("recovered").is_none());
        // device_time 無し = キーごと出さない。
        assert!(j.get("device_time").is_none());
    }

    /// spec §6.1 の 2 行目: switch/multi-press-complete（複数フィールド）。
    #[test]
    fn event_json_switch_multi_press_complete() {
        let j = item(0x003B, 0x06, json!({"0": 1, "1": 2})).to_json();
        assert_eq!(j["event"], "multi-press-complete");
        assert_eq!(
            j["data"],
            json!({"previous-position": 1, "total-number-of-presses-counted": 2})
        );
    }

    /// spec §6.1 の 3 行目: booleanstate/state-change。
    #[test]
    fn event_json_booleanstate_state_change() {
        let mut it = item(0x0045, 0x00, json!({"0": true}));
        it.node_id = 24;
        it.endpoint = 1;
        it.event_number = 9;
        let j = it.to_json();
        assert_eq!(j["node_id"], 24);
        assert_eq!(j["endpoint"], 1);
        assert_eq!(j["cluster"], "booleanstate");
        assert_eq!(j["event"], "state-change");
        assert_eq!(j["event_number"], 9);
        assert_eq!(j["data"], json!({"state-value": true}));
    }

    /// ids に無いクラスタ / イベント は属性行と同じく JSON **数値**のまま出す。
    /// 10 進**文字列**になるのは `data` のオブジェクトキーだけ（read の struct
    /// 規約と同じ — JSON のキーは文字列しか取れない）。
    #[test]
    fn event_json_falls_back_to_numbers_for_unknown_ids() {
        let j = item(0xFFF1_0001, 0x99, json!({"7": 5})).to_json();
        assert_eq!(j["cluster"], json!(0xFFF1_0001u32));
        assert_eq!(j["event"], json!(0x99));
        assert!(j["event"].is_number(), "文字列化しない: {}", j["event"]);
        assert_eq!(
            j["data"],
            json!({"7": 5}),
            "data のキーは 10 進文字列のまま"
        );
        // 既知クラスタ・未知イベントも数値。
        let j = item(0x003B, 0x77, json!({"0": 1})).to_json();
        assert_eq!(j["cluster"], "switch");
        assert_eq!(j["event"], json!(0x77));
        assert!(j["event"].is_number());
        assert_eq!(j["data"], json!({"0": 1}));
    }

    /// device_time は絶対時刻（System / Epoch）のときだけ付ける。
    /// 未解決 Delta は「デバイス側時刻が分からない」ので省略する。
    #[test]
    fn device_time_only_for_absolute_timestamps() {
        let with = |ts| {
            let mut it = item(0x003B, 0x01, json!({"0": 1}));
            it.device_time = Some(ts);
            it.to_json()
        };
        assert_eq!(
            with(EventTimestamp::System(5_000))["device_time"],
            json!({"system_ms": 5_000})
        );
        assert_eq!(
            with(EventTimestamp::Epoch(1_700_000_000_000))["device_time"],
            json!({"epoch_ms": 1_700_000_000_000u64})
        );
        assert!(with(EventTimestamp::DeltaSystem(10))
            .get("device_time")
            .is_none());
        assert!(with(EventTimestamp::DeltaEpoch(10))
            .get("device_time")
            .is_none());
    }

    /// data が無いイベント（フィールド無し）は `data` キーごと出さない。
    #[test]
    fn event_json_omits_data_when_absent() {
        let mut it = item(0x003B, 0x01, json!({}));
        it.data = None;
        let j = it.to_json();
        assert!(j.get("data").is_none());
    }

    fn data_report(event: u32, number: u64, data: serde_json::Value) -> EventReport {
        EventReport::Data(EventData {
            endpoint: 2,
            cluster: 0x003B,
            event,
            event_number: number,
            priority: EventPriority::Info,
            timestamp: Some(EventTimestamp::System(5_000)),
            data: Some(data),
        })
    }

    /// EventNumber 昇順に並べ替え、Status エントリは捨てる（spec §6.2）。
    /// 拒否は warn 1 行に集約されるが、**行としては流さない**のが契約 —
    /// ここで釘打つのは出力側（ログはアサートしない）。
    #[test]
    fn events_from_event_reports_sorts_and_drops_status() {
        let reports = vec![
            data_report(0x01, 12, json!({"0": 1})),
            EventReport::Status {
                endpoint: Some(9),
                cluster: Some(0x003B),
                event: Some(0x02),
                status: 0x7F,
            },
            // 拒否は path 数ぶん一斉に来る（集約 warn の想定入力）。
            EventReport::Status {
                endpoint: Some(9),
                cluster: Some(0x0045),
                event: Some(0x00),
                status: 0x7F,
            },
            data_report(0x03, 10, json!({"0": 1})),
        ];
        let out = events_from_event_reports(25, &reports, true, "2026-09-06T21:00:00+09:00");
        assert_eq!(out.len(), 2, "Status は捨てる");
        assert_eq!(
            out.iter().map(|e| e.event_number).collect::<Vec<_>>(),
            vec![10, 12]
        );
        // 受信時刻は呼び手が採った 1 つを全件で共有する。
        assert!(out
            .iter()
            .all(|e| e.timestamp == "2026-09-06T21:00:00+09:00"));
        assert!(out.iter().all(|e| e.priming));
        assert_eq!(out[0].node_id, 25);
        assert_eq!(out[0].endpoint, 2);
        assert_eq!(out[0].cluster, 0x003B);
        assert_eq!(out[0].event, 0x03);
        assert_eq!(out[0].device_time, Some(EventTimestamp::System(5_000)));
    }

    /// `Emitted::to_json` は中身の行をそのまま出す（stream ループの分岐点）。
    #[test]
    fn emitted_to_json_dispatches_to_the_inner_line() {
        let attr = Emitted::Attribute(Event {
            timestamp: "2026-09-06T21:00:00+09:00".to_string(),
            node_id: 25,
            endpoint: 1,
            cluster: 0x0006,
            attribute: 0x0000,
            value: json!(true),
            priming: false,
            recovered: false,
        });
        assert_eq!(attr.to_json()["attribute"], "on-off");
        let ev = Emitted::Event(item(0x003B, 0x01, json!({"0": 1})));
        assert_eq!(ev.to_json()["event"], "initial-press");
        // 判別点は `attribute` / `event` キーの有無（spec §6.1）。
        assert!(attr.to_json().get("event").is_none());
        assert!(ev.to_json().get("attribute").is_none());
    }

    #[test]
    fn event_json_uses_chip_tool_names_and_numeric_fallback() {
        let ev = Event {
            timestamp: "2026-07-20T00:00:00+09:00".to_string(),
            node_id: 21,
            endpoint: 1,
            cluster: 0x0406,   // occupancysensing
            attribute: 0x0000, // occupancy
            value: json!(1),
            priming: false,
            recovered: false,
        };
        let j = ev.to_json();
        assert_eq!(j["node_id"], 21);
        assert_eq!(j["endpoint"], 1);
        assert_eq!(j["cluster"], "occupancysensing");
        assert_eq!(j["attribute"], "occupancy");
        assert_eq!(j["value"], 1);
        assert_eq!(j["priming"], false);
        // 差分回復で昇格したイベントかどうかは常に載る（既定 false）。
        assert_eq!(j["recovered"], false);
        assert_eq!(j["timestamp"], "2026-07-20T00:00:00+09:00");

        // ids テーブルに無いものは数値のまま。
        let ev = Event {
            cluster: 0xFFF1_0001,
            attribute: 0x9999,
            ..ev
        };
        let j = ev.to_json();
        assert_eq!(j["cluster"], 0xFFF1_0001u32);
        assert_eq!(j["attribute"], 0x9999);

        // 昇格イベントは priming=false と recovered=true が同居する。
        let ev = Event {
            priming: false,
            recovered: true,
            ..ev
        };
        assert_eq!(ev.to_json()["recovered"], true);
    }

    #[test]
    fn events_from_report_keeps_scalars_and_drops_containers() {
        let mut msg = onoff_report(1, true);
        // list 要素として届く scalar（AttributePathIB.ListIndex=null → デコーダは
        // list_append: true として表現）はイベント化せず捨てる。値そのものは
        // scalar（JSON array/object ではない）なので is_array/is_object 判定は
        // 素通りしてしまう — list_append フラグでの判定が必要。
        msg.reports.push(mat_controller::im::AttributeReport {
            endpoint: Some(1),
            cluster: Some(0x0008),
            attribute: Some(0xFFFB), // attribute-list
            list_append: true,
            data: Some(json!(65531)),
            status: None,
        });
        // list/struct（wildcard priming に混ざる ACL / server-list 等）は捨てる。
        msg.reports.push(mat_controller::im::AttributeReport {
            endpoint: Some(0),
            cluster: Some(0x001F),
            attribute: Some(0x0000),
            list_append: false,
            data: Some(json!([{ "1": 5 }])),
            status: None,
        });
        // status-only / path 欠落も捨てる。
        msg.reports.push(mat_controller::im::AttributeReport {
            endpoint: None,
            cluster: None,
            attribute: None,
            list_append: false,
            data: None,
            status: Some(0x7E),
        });
        let evs = events_from_report(7, &msg, true);
        // 唯一残るのは onoff_report が積んだ通常の scalar（list_append: false）。
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].node_id, 7);
        assert_eq!(evs[0].cluster, 0x0006);
        assert_eq!(evs[0].value, json!(true));
        assert!(evs[0].priming);
    }

    /// このバグが差分回復へ及ぼしていた害の釘打ち: list 属性は要素ごとに別々の
    /// AttributeReport（list_append: true, data: scalar）として届き、同一キー
    /// (node/endpoint/cluster/attribute) に対して値が要素ごとに違う。もし
    /// list_append 要素をイベント化してしまうと、classify_against_cache が
    /// 「値が変わった priming」と誤認して recovered: true を大量発生させる
    /// （実機 node 13 の再購読 priming で観測: onoff/attribute-list 等）。
    /// events_from_report が list_append 要素を落とす限り、SubHealth::observe
    /// を通しても recovered イベントは 1 つも出ない。
    #[test]
    fn list_append_elements_never_produce_recovered_events() {
        let health = SubHealth::new(None);
        let round = |values: &[i64]| {
            let msg = mat_controller::im::ReportDataMessage {
                reports: values
                    .iter()
                    .map(|&v| mat_controller::im::AttributeReport {
                        endpoint: Some(1),
                        cluster: Some(0x0006),
                        attribute: Some(0xFFFB), // attribute-list
                        list_append: true,
                        data: Some(json!(v)),
                        status: None,
                    })
                    .collect(),
                subscription_id: Some(1),
                more_chunks: false,
                suppress_response: false,
            };
            events_from_report(13, &msg, true)
                .into_iter()
                .map(|ev| health.observe(ev))
                .collect::<Vec<_>>()
        };
        // 1 回目の priming（要素 0, 1）: 同一キーに異なる値が複数届く。
        let first = round(&[0, 1]);
        // 再購読後の 2 回目の priming（要素の値も変わり得る = list append の実態）。
        let second = round(&[0, 1, 65533]);
        assert!(
            first.iter().all(|e| !e.recovered) && second.iter().all(|e| !e.recovered),
            "list_append 要素起因の偽 recovered は出ない: first={first:?} second={second:?}"
        );
        // list_append 要素はそもそもイベント化されない（events_from_report の契約）。
        assert!(first.is_empty() && second.is_empty());
    }
}
