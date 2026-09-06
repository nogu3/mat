//! I/O-free なイベントログ（spec §7.14 / §8.9.2.6）。EventNumber はノード
//! 単位で単調増加、容量固定の FIFO（満杯なら最古を捨てる — chip の
//! `EventManagement` と同じ）。ここにはタイマも時計もない: 経過時間
//! （`system_timestamp_ms`、spec §8.9.2.6 の SystemTimestamp）は必ず
//! 呼び側（`net::runtime` の uptime）から渡される — `core` は I/O-free
//! （`cargo check -p mat-device --no-default-features`）。
use std::collections::VecDeque;

use mat_controller::im::EventPriority;

/// `ClusterHandler` が「今このイベントが起きた」と申告する形（`InvokeCtx::
/// events` に push する）。endpoint / cluster / EventNumber /
/// タイムスタンプは `Node` 側が知っているので、クラスタは自分のイベント
/// id・priority・ペイロード（TLV 要素 1 個、無いなら `None`）だけを言う
/// — `InvokeCtx::changed` が属性 id だけを言うのと同じ分業。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedEvent {
    pub event: u32,
    pub priority: EventPriority,
    pub data_tlv: Option<Vec<u8>>,
}

/// ログに載った 1 件（`EmittedEvent` に `Node` が endpoint / cluster /
/// EventNumber / SystemTimestamp を付けたもの）。購読レポートの
/// `EventReportOut` はここから組まれる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub number: u64,
    pub endpoint: u16,
    pub cluster: u32,
    pub event: u32,
    pub priority: EventPriority,
    pub system_timestamp_ms: u64,
    pub data_tlv: Option<Vec<u8>>,
}

/// ノード単位のイベントログ。EventNumber は `first_number` から単調増加し
/// （容量あふれで最古を捨てても番号は戻らない — 購読側の
/// `EventFilterIB::EventMin` が単調性に依存する）、保持は直近 `cap` 件。
pub struct EventLog {
    next_number: u64,
    entries: VecDeque<StoredEvent>,
    cap: usize,
}

impl EventLog {
    /// 既定の保持件数。デバイス 1 台のイベントは秒間数件が上限なので、
    /// 購読の 1 レポート往復ぶんを取りこぼさない程度の小さな値で足りる。
    pub const DEFAULT_CAP: usize = 64;

    /// `first_number` から採番するログ。`cap` は 0 を渡されても 1 に
    /// 丸める（容量 0 のログは append した瞬間に消えるので、呼び側の
    /// バグを黙って飲み込むより 1 件保持のほうが安全）。
    pub fn new(first_number: u64, cap: usize) -> Self {
        Self {
            next_number: first_number,
            entries: VecDeque::with_capacity(cap.min(Self::DEFAULT_CAP)),
            cap: cap.max(1),
        }
    }

    /// 次に採番される EventNumber（＝まだ誰にも渡っていない番号）。
    pub fn next_number(&self) -> u64 {
        self.next_number
    }

    /// 1 件追記し、割り当てた EventNumber を返す。満杯なら最古を捨てる。
    pub fn append(
        &mut self,
        endpoint: u16,
        cluster: u32,
        ev: EmittedEvent,
        system_timestamp_ms: u64,
    ) -> u64 {
        let number = self.next_number;
        self.next_number = self.next_number.wrapping_add(1);
        if self.entries.len() == self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back(StoredEvent {
            number,
            endpoint,
            cluster,
            event: ev.event,
            priority: ev.priority,
            system_timestamp_ms,
            data_tlv: ev.data_tlv,
        });
        number
    }

    /// EventNumber が `min` 以上の保持イベント（古い順）。購読の
    /// `EventFilterIB::EventMin`（spec §8.9.2.4）そのもの。
    pub fn since(&self, min: u64) -> impl Iterator<Item = &StoredEvent> {
        self.entries.iter().filter(move |e| e.number >= min)
    }

    /// 現在の保持件数（採番済み総数ではない）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new(1, Self::DEFAULT_CAP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: u32) -> EmittedEvent {
        EmittedEvent {
            event: id,
            priority: EventPriority::Info,
            data_tlv: None,
        }
    }

    #[test]
    fn numbers_are_monotonic_from_the_seed() {
        let mut log = EventLog::new(1000, 8);
        assert_eq!(log.next_number(), 1000);
        assert_eq!(log.append(2, 0x3B, ev(1), 5), 1000);
        assert_eq!(log.append(2, 0x3B, ev(3), 6), 1001);
        assert_eq!(log.next_number(), 1002);
        let all: Vec<u64> = log.since(0).map(|e| e.number).collect();
        assert_eq!(all, vec![1000, 1001]);
        let tail: Vec<u64> = log.since(1001).map(|e| e.number).collect();
        assert_eq!(tail, vec![1001]);
        assert!(log.since(1002).next().is_none());
    }

    #[test]
    fn cap_drops_the_oldest_but_keeps_numbering() {
        let mut log = EventLog::new(1, 2);
        for i in 0..3 {
            log.append(1, 1, ev(i), 0);
        }
        let nums: Vec<u64> = log.since(0).map(|e| e.number).collect();
        assert_eq!(nums, vec![2, 3]);
        assert_eq!(log.next_number(), 4);
    }
}
