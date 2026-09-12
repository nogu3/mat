//! The device's one active subscription (spec §8.10) and the pure timing
//! rule that decides when its next ReportData goes out.
//!
//! The device runtime (`net::runtime`) keeps at most **one** subscription
//! alive at a time — same sequential, one-peer-at-a-time posture as the
//! session handling it rides on (see `net::runtime`'s module doc). A new
//! `SubscribeRequest`, or a new PASE/CASE session, replaces whatever was
//! there — except a `SubscribeRequest` with no readable path (spec §8.10),
//! rejected with `INVALID_ACTION`, which leaves any existing subscription
//! untouched *only* when the request asked `KeepSubscriptions=true`; with
//! `KeepSubscriptions=false` the existing subscription is torn down before
//! the refusal, as chip does (see `net::runtime`'s `SubscribeOutcome`).
//!
//! Everything here is deliberately I/O-free: `ActiveSubscription` is state
//! plus arithmetic, so the interval policy (`next_report_deadline`) and the
//! wildcard path matching (`note_changed`) are unit-testable without
//! sockets, timers, or a `Node`. The sending itself — priming chunks,
//! dirty reports, keep-alives — lives in `net::runtime`, which owns the
//! `SecureSession`.
//!
//! **Events** (spec §8.9.2 / §8.10.2, chip's `ReportScheduler`): a new
//! event matching a subscribed `EventPathIB` whose `IsUrgent` is set is
//! reported on the same schedule as a dirty attribute — as soon as
//! `min_interval` since the last report has elapsed (`pending_urgent`).
//! A new event matching only non-urgent paths rides along on whatever
//! report goes out next (a dirty report, or the keep-alive at
//! `max_interval`) and never pulls the deadline forward on its own.

use std::time::Duration;

use mat_controller::im::{AttrPathIn, EventPathIn, ReportEntryOut};
use tokio::time::Instant;

use crate::core::datamodel::Node;
use crate::core::events::StoredEvent;

/// How far *before* `max_interval` a keep-alive report is sent. The spec
/// contract is that the subscriber may consider the subscription dead once
/// `max_interval` elapses with no report (`SecureSession::
/// next_subscription_report`'s `Silence`), so aiming exactly at
/// `max_interval` would lose that race to any scheduling or network delay.
/// Reporting early is always legal; reporting late kills the subscription.
const KEEP_ALIVE_MARGIN: Duration = Duration::from_secs(2);

/// One subscription this device is currently serving.
///
/// `min_interval`/`max_interval` are already the *negotiated* values — what
/// the device put in its `SubscribeResponse` (spec §8.10: the device is
/// free to pick a MaxInterval at or below the requested ceiling), not the
/// raw request. `last_report_at` is when the last ReportData for this
/// subscription went out, priming included, so the very first keep-alive is
/// measured from the end of the subscribe interaction.
#[derive(Debug, Clone)]
pub struct ActiveSubscription {
    pub id: u32,
    pub paths: Vec<AttrPathIn>,
    /// The `IsFabricFiltered` of the `SubscribeRequest` that created this
    /// subscription (spec §8.5.1). Every later report is part of that same
    /// request, so it has to be replayed into the `ReadCtx` of each
    /// dirty/keep-alive report — not just the priming one.
    pub fabric_filtered: bool,
    pub min_interval: Duration,
    pub max_interval: Duration,
    pub last_report_at: Instant,
    /// Full `(endpoint, cluster, attribute)` paths whose values changed
    /// since the last report *and* that this subscription asked for.
    /// Drained into a report when the deadline fires.
    pub dirty: Vec<(u16, u32, u32)>,
    /// The `EventRequests` of the `SubscribeRequest` that created this
    /// subscription (spec §8.9.2.2). Empty for an attribute-only
    /// subscription, which then never carries an event report.
    pub event_paths: Vec<EventPathIn>,
    /// The `EventMin` of the next report (spec §8.9.2.4): every event the
    /// subscriber has already been sent has a number below this, so a
    /// report only ever carries what happened since. Seeded from
    /// `Node::next_event_number` when the subscription is installed (the
    /// priming report is what decides what "already sent" means) and
    /// advanced only when a report is actually acknowledged.
    pub next_event: u64,
    /// Whether an event matching an **urgent** subscribed path happened
    /// since the last report — the event-side twin of a non-empty `dirty`,
    /// and the only reason an event pulls `next_report_deadline` forward.
    pub pending_urgent: bool,
}

impl ActiveSubscription {
    /// When the next ReportData for this subscription is due.
    ///
    /// Two regimes (spec §8.10.2's MinIntervalFloor/MaxInterval contract):
    /// - **dirty** — there is something to report, so report it as soon as
    ///   the minimum interval since the last report has elapsed. This is
    ///   the floor that keeps a rapidly-toggling attribute from flooding
    ///   the subscriber; with `min_interval` 0 (what a controller asking
    ///   for immediate updates sends) it means "right now".
    /// - **clean** — nothing to say, but silence past `max_interval` reads
    ///   as a dead subscription, so an empty keep-alive goes out
    ///   `KEEP_ALIVE_MARGIN` early. For a `max_interval` small enough that
    ///   a flat 2s margin would dominate (or invert) the interval, the
    ///   margin is halved instead: never later than `max_interval`, never
    ///   sooner than half of it.
    ///
    /// A pending **urgent** event (`pending_urgent`) counts as dirty for
    /// this decision: chip's `ReportScheduler` treats an urgent event
    /// exactly like a changed attribute — report it at the min-interval
    /// floor. A non-urgent event doesn't move the deadline at all; it
    /// simply rides on whatever report goes out next.
    pub fn next_report_deadline(&self) -> Instant {
        if self.dirty.is_empty() && !self.pending_urgent {
            self.last_report_at + self.max_interval - self.keep_alive_margin()
        } else {
            self.last_report_at + self.min_interval
        }
    }

    /// `KEEP_ALIVE_MARGIN`, clamped to half of `max_interval` so a short
    /// interval doesn't get eaten by (or run negative from) a flat 2s.
    fn keep_alive_margin(&self) -> Duration {
        KEEP_ALIVE_MARGIN.min(self.max_interval / 2)
    }

    /// Records the paths one invoke changed, keeping only the ones this
    /// subscription actually covers, and without duplicating a path already
    /// waiting to be reported (a value changing twice between reports is
    /// still one entry — the report carries the current value, not a
    /// history).
    pub fn note_changed(&mut self, changed: &[(u16, u32, u32)]) {
        for path in changed {
            if self.covers(*path) && !self.dirty.contains(path) {
                self.dirty.push(*path);
            }
        }
    }

    /// Whether any of this subscription's requested paths matches a
    /// concrete `(endpoint, cluster, attribute)`.
    pub fn covers(&self, path: (u16, u32, u32)) -> bool {
        self.paths.iter().any(|p| path_matches(p, path))
    }

    /// Whether some subscribed path names `path` with **all three fields
    /// concrete** — i.e. the subscriber asked for exactly this attribute,
    /// not for a wildcard that happens to expand to it. Decides whether a
    /// refused attribute in a dirty report is answered with a status entry
    /// (concrete: yes) or dropped (wildcard: silent, the same asymmetry
    /// `Node::read_entries` applies to the priming report — spec §8.4.2.2).
    pub fn covered_concretely(&self, path: (u16, u32, u32)) -> bool {
        self.paths.iter().any(|p| {
            p.endpoint.is_some()
                && p.cluster.is_some()
                && p.attribute.is_some()
                && path_matches(p, path)
        })
    }

    /// Records the events that just happened, the event-side twin of
    /// `note_changed`. Nothing is buffered — the report reads the log back
    /// through `next_event` — so the only state this updates is
    /// `pending_urgent`, i.e. whether the deadline moves to the
    /// min-interval regime. Events this subscription doesn't cover, and
    /// covered ones whose matching paths are all non-urgent, leave it
    /// alone.
    pub fn note_events(&mut self, events: &[StoredEvent]) {
        for e in events {
            if let Some(true) = self.covers_event(e.endpoint, e.cluster, e.event) {
                self.pending_urgent = true;
            }
        }
    }

    /// `note_changed` + `note_events` for one dispatch outcome: `changed`
    /// is what the request/stimulus/groupcast mutated, and the events it
    /// emitted are read back off `node`'s log from `next_event` on. No
    /// cluster emits from an invoke or a write today (`Node::drain_events`),
    /// so for those callers the event half is a no-op — kept so the first
    /// one that does gets the urgent regime, exactly like a stimulus.
    pub fn note_outcome(&mut self, changed: &[(u16, u32, u32)], node: &Node) {
        self.note_changed(changed);
        self.note_events(&node.recent_events(self.next_event));
    }

    /// Whether this subscription asked for a concrete `(endpoint, cluster,
    /// event)`, and if so whether it asked *urgently*: `None` means no
    /// subscribed path matches (the event is not this subscriber's
    /// business), `Some(urgent)` means at least one does — `true` when any
    /// matching path set `IsUrgent` (one urgent path is enough to make the
    /// event urgent, spec §8.9.2.2).
    pub fn covers_event(&self, endpoint: u16, cluster: u32, event: u32) -> Option<bool> {
        let mut urgent = None;
        for p in &self.event_paths {
            if event_path_matches(p, endpoint, cluster, event) {
                urgent = Some(urgent.unwrap_or(false) || p.urgent);
            }
        }
        urgent
    }
}

/// Whether a (possibly wildcard) subscribed `EventPathIn` matches one
/// concrete `(endpoint, cluster, event)` — the event-side twin of
/// `path_matches`, with the same `None`-is-a-wildcard rule. `urgent` is
/// not part of matching: it says how the match is *reported*, not what it
/// matches.
pub fn event_path_matches(p: &EventPathIn, endpoint: u16, cluster: u32, event: u32) -> bool {
    p.endpoint.is_none_or(|e| e == endpoint)
        && p.cluster.is_none_or(|c| c == cluster)
        && p.event.is_none_or(|ev| ev == event)
}

/// Whether a (possibly wildcard) subscribed `AttrPathIn` matches one
/// concrete `(endpoint, cluster, attribute)`: a `None` field is a wildcard
/// that matches anything, a `Some` field must be equal. Same expansion
/// semantics `Node::read_entries` applies to a read, expressed the other
/// way round (concrete path in, yes/no out) — the read side expands a path
/// against the registry, this side tests one already-known path against a
/// request.
pub fn path_matches(
    subscribed: &AttrPathIn,
    (endpoint, cluster, attribute): (u16, u32, u32),
) -> bool {
    subscribed.endpoint.is_none_or(|e| e == endpoint)
        && subscribed.cluster.is_none_or(|c| c == cluster)
        && subscribed.attribute.is_none_or(|a| a == attribute)
}

/// Filters one dirty report's entries the way `Node::read_entries` already
/// filters a priming report: `Data` always stays; a `Status` entry stays
/// only when the subscriber named that attribute concretely
/// (`covered_concretely`). The dirty set is read back through *concrete*
/// paths (`send_subscription_report` builds them from `dirty`), so without
/// this every attribute a wildcard subscription is not allowed to read
/// would resurface as an `UNSUPPORTED_ACCESS` status entry on every
/// change — the asymmetry the priming report avoids.
pub fn retain_reportable(
    sub: &ActiveSubscription,
    entries: Vec<ReportEntryOut>,
) -> Vec<ReportEntryOut> {
    entries
        .into_iter()
        .filter(|e| match e {
            ReportEntryOut::Data(_) => true,
            ReportEntryOut::Status {
                endpoint,
                cluster,
                attribute,
                ..
            } => sub.covered_concretely((*endpoint, *cluster, *attribute)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::im;

    fn sub(min: u64, max: u64, dirty: Vec<(u16, u32, u32)>, now: Instant) -> ActiveSubscription {
        ActiveSubscription {
            id: 1,
            paths: vec![AttrPathIn {
                endpoint: None,
                cluster: None,
                attribute: None,
            }],
            fabric_filtered: true,
            min_interval: Duration::from_secs(min),
            max_interval: Duration::from_secs(max),
            last_report_at: now,
            dirty,
            event_paths: Vec::new(),
            next_event: 0,
            pending_urgent: false,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dirty_subscription_reports_at_the_min_interval() {
        let now = Instant::now();
        let s = sub(2, 60, vec![(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF)], now);
        assert_eq!(s.next_report_deadline(), now + Duration::from_secs(2));
    }

    /// `min_interval` 0 (a controller asking for updates as they happen)
    /// means the report is due immediately — not "never".
    #[tokio::test(start_paused = true)]
    async fn dirty_subscription_with_a_zero_floor_is_due_immediately() {
        let now = Instant::now();
        let s = sub(0, 60, vec![(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF)], now);
        assert_eq!(s.next_report_deadline(), now);
    }

    #[tokio::test(start_paused = true)]
    async fn clean_subscription_keeps_alive_a_margin_before_the_max_interval() {
        let now = Instant::now();
        let s = sub(0, 60, Vec::new(), now);
        assert_eq!(
            s.next_report_deadline(),
            now + Duration::from_secs(60) - KEEP_ALIVE_MARGIN
        );
    }

    /// A short `max_interval` must not be swallowed by the flat 2s margin
    /// (a 3s interval would keep-alive at 1s — needlessly chatty — and a
    /// hypothetical 2s one would land exactly on `last_report_at`): below
    /// the crossover the margin is halved instead, so the keep-alive never
    /// goes out sooner than half the promised interval.
    #[tokio::test(start_paused = true)]
    async fn a_small_max_interval_halves_the_margin_instead_of_being_swallowed() {
        let now = Instant::now();
        let s = sub(0, 3, Vec::new(), now);
        // 3s interval → 1.5s margin → keep-alive at 1.5s, not 1s.
        assert_eq!(s.next_report_deadline(), now + Duration::from_millis(1500));

        // 4s is the crossover: half the interval is exactly the flat margin.
        let s = sub(0, 4, Vec::new(), now);
        assert_eq!(s.next_report_deadline(), now + Duration::from_secs(2));

        // Above it the flat margin applies again.
        let s = sub(0, 10, Vec::new(), now);
        assert_eq!(s.next_report_deadline(), now + Duration::from_secs(8));
    }

    /// The deadline is always in `[last_report_at, last_report_at +
    /// max_interval]` — never past the interval the device promised its
    /// subscriber, and never in the past relative to the last report.
    #[tokio::test(start_paused = true)]
    async fn the_keep_alive_deadline_never_exceeds_the_promised_interval() {
        let now = Instant::now();
        for max in 3..=60u64 {
            let s = sub(0, max, Vec::new(), now);
            let deadline = s.next_report_deadline();
            assert!(deadline > now, "max={max}: deadline must be in the future");
            assert!(
                deadline <= now + Duration::from_secs(max),
                "max={max}: deadline must not exceed the promised interval"
            );
        }
    }

    #[test]
    fn note_changed_keeps_only_covered_paths_and_deduplicates() {
        let now = tokio::time::Instant::now();
        let mut s = ActiveSubscription {
            paths: vec![AttrPathIn {
                endpoint: None,
                cluster: Some(im::CLUSTER_ON_OFF),
                attribute: None,
            }],
            ..sub(0, 60, Vec::new(), now)
        };
        s.note_changed(&[
            (1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF),
            (0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_VENDOR_ID),
        ]);
        assert_eq!(s.dirty, vec![(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF)]);

        // Same path changing again before the report goes out stays one entry.
        s.note_changed(&[(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF)]);
        assert_eq!(s.dirty.len(), 1);
    }

    #[test]
    fn path_matching_treats_none_as_a_wildcard() {
        let concrete = (1u16, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF);
        let full_wildcard = AttrPathIn {
            endpoint: None,
            cluster: None,
            attribute: None,
        };
        assert!(path_matches(&full_wildcard, concrete));

        let exact = AttrPathIn {
            endpoint: Some(1),
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: Some(im::ATTR_ON_OFF),
        };
        assert!(path_matches(&exact, concrete));

        let other_endpoint = AttrPathIn {
            endpoint: Some(2),
            ..exact
        };
        assert!(!path_matches(&other_endpoint, concrete));

        let other_attribute = AttrPathIn {
            attribute: Some(0x1234),
            ..exact
        };
        assert!(!path_matches(&other_attribute, concrete));
    }

    fn stored(
        endpoint: u16,
        cluster: u32,
        event: u32,
        number: u64,
    ) -> crate::core::events::StoredEvent {
        crate::core::events::StoredEvent {
            number,
            endpoint,
            cluster,
            event,
            priority: mat_controller::im::EventPriority::Info,
            system_timestamp_ms: 0,
            data_tlv: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_urgent_events_are_due_at_the_min_interval() {
        let now = Instant::now();
        let mut s = ActiveSubscription {
            event_paths: vec![EventPathIn::WILDCARD_URGENT],
            ..sub(2, 60, Vec::new(), now)
        };
        assert_eq!(
            s.next_report_deadline(),
            now + Duration::from_secs(60) - KEEP_ALIVE_MARGIN
        );
        s.note_events(&[stored(
            2,
            im::CLUSTER_SWITCH,
            im::EVENT_SWITCH_INITIAL_PRESS,
            1,
        )]);
        assert!(s.pending_urgent);
        assert_eq!(s.next_report_deadline(), now + Duration::from_secs(2));
    }

    #[test]
    fn non_urgent_events_do_not_advance_the_deadline() {
        let now = Instant::now();
        let mut s = ActiveSubscription {
            event_paths: vec![EventPathIn {
                urgent: false,
                ..EventPathIn::WILDCARD_URGENT
            }],
            ..sub(0, 60, Vec::new(), now)
        };
        s.note_events(&[stored(
            2,
            im::CLUSTER_SWITCH,
            im::EVENT_SWITCH_INITIAL_PRESS,
            1,
        )]);
        assert!(!s.pending_urgent);
    }

    #[test]
    fn events_outside_the_subscribed_paths_are_ignored() {
        let now = Instant::now();
        let mut s = ActiveSubscription {
            event_paths: vec![EventPathIn {
                cluster: Some(im::CLUSTER_BOOLEAN_STATE),
                ..EventPathIn::WILDCARD_URGENT
            }],
            ..sub(0, 60, Vec::new(), now)
        };
        s.note_events(&[stored(
            2,
            im::CLUSTER_SWITCH,
            im::EVENT_SWITCH_INITIAL_PRESS,
            1,
        )]);
        assert!(!s.pending_urgent);
        assert_eq!(
            s.covers_event(2, im::CLUSTER_BOOLEAN_STATE, im::EVENT_BS_STATE_CHANGE),
            Some(true)
        );
        assert_eq!(s.covers_event(2, im::CLUSTER_SWITCH, 1), None);
    }

    fn sub_with_paths(paths: Vec<AttrPathIn>) -> ActiveSubscription {
        ActiveSubscription {
            id: 1,
            paths,
            fabric_filtered: true,
            min_interval: Duration::from_secs(0),
            max_interval: Duration::from_secs(5),
            last_report_at: Instant::now(),
            dirty: Vec::new(),
            event_paths: Vec::new(),
            next_event: 0,
            pending_urgent: false,
        }
    }

    fn status_entry(endpoint: u16, cluster: u32, attribute: u32) -> ReportEntryOut {
        ReportEntryOut::Status {
            endpoint,
            cluster,
            attribute,
            status: im::STATUS_UNSUPPORTED_ACCESS,
        }
    }

    #[test]
    fn covered_concretely_is_true_only_for_a_fully_concrete_subscribed_path() {
        let concrete = AttrPathIn {
            endpoint: Some(1),
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: Some(im::ATTR_ON_OFF),
        };
        let wildcard = AttrPathIn {
            endpoint: None,
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: None,
        };
        let path = (1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF);
        assert!(sub_with_paths(vec![concrete]).covered_concretely(path));
        assert!(!sub_with_paths(vec![wildcard]).covered_concretely(path));
        // 両方 cover していれば「具体的に頼まれた」側が勝つ。
        assert!(sub_with_paths(vec![wildcard, concrete]).covered_concretely(path));
    }

    /// spec §8.4.2.2 の非対称を dirty report にも適用: wildcard 購読が
    /// 拾った不許可属性の status entry は落とし（priming と同じ「黙る」）、
    /// 具体パス購読の status entry と Data は残す。
    #[test]
    fn retain_reportable_drops_status_entries_only_under_wildcard_paths() {
        let onoff_wildcard = AttrPathIn {
            endpoint: None,
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: None,
        };
        let acl_concrete = AttrPathIn {
            endpoint: Some(0),
            cluster: Some(im::CLUSTER_ACCESS_CONTROL),
            attribute: Some(im::ATTR_ACL),
        };
        let sub = sub_with_paths(vec![onoff_wildcard, acl_concrete]);
        let data = ReportEntryOut::Data(im::AttrReportOut {
            endpoint: 1,
            cluster: im::CLUSTER_ON_OFF,
            attribute: im::ATTR_ON_OFF,
            data_version: 0,
            value_tlv: vec![0x09], // TLV true
        });
        let entries = vec![
            data.clone(),
            status_entry(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF), // wildcard → 落ちる
            status_entry(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL), // 具体 → 残る
        ];
        let kept = retain_reportable(&sub, entries);
        assert_eq!(kept.len(), 2);
        assert!(matches!(&kept[0], ReportEntryOut::Data(d) if d.attribute == im::ATTR_ON_OFF));
        assert!(matches!(
            &kept[1],
            ReportEntryOut::Status { cluster, attribute, .. }
                if *cluster == im::CLUSTER_ACCESS_CONTROL && *attribute == im::ATTR_ACL
        ));
    }
}
