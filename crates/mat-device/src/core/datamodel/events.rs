use super::*;

impl Node {
    /// Replaces this node's event log (spec §7.14). Call once, right after
    /// construction — like `set_data_version_base`, whose randomized-at-boot
    /// rationale the first EventNumber shares (a subscriber must not have a
    /// cached EventMin from a previous boot that silently swallows this
    /// boot's events). A `Node` that never gets one keeps `EventLog::
    /// default()` (numbering from 1).
    pub fn set_event_log(&mut self, log: EventLog) {
        self.event_log = log;
    }

    /// The EventNumber the next emitted event will get — i.e. one past
    /// everything already in the log. `net::runtime` records it as a fresh
    /// subscription's starting `EventMin` so the priming report doesn't
    /// replay history the subscriber never asked for.
    pub fn next_event_number(&self) -> u64 {
        self.event_log.next_number()
    }

    /// Every retained event with an EventNumber ≥ `since`, cloned out of the
    /// log (the caller — `net::runtime`'s subscription bookkeeping — needs
    /// them past the borrow of `Node` that produced them).
    pub fn recent_events(&self, since: u64) -> Vec<StoredEvent> {
        self.event_log.since(since).cloned().collect()
    }

    /// Moves whatever a handler emitted (`ctx.events`) into the node's event
    /// log, tagged with the `(endpoint, cluster)` it was dispatched to, and
    /// returns the EventNumbers assigned — the event-side twin of the
    /// `ctx.changed` → `(endpoint, cluster, attribute)` + DataVersion bump
    /// that every caller does right before calling this.
    ///
    /// `system_timestamp_ms` (spec §8.9.2.6) has to come from the caller:
    /// `core` has no clock (I/O-free). The invoke/write dispatch paths pass
    /// **0** — `Node::handle_im` is handed no time reference and no cluster
    /// implemented so far emits an event from a command or a write, so
    /// there is nothing to be wrong about yet. `stimulate`, whose caller
    /// (`net::runtime`) does know the device's uptime, passes the real
    /// value. Give the invoke path a real clock the day a cluster emits
    /// from `invoke`.
    pub(super) fn drain_events(
        &mut self,
        endpoint: u16,
        cluster: u32,
        ctx: &mut InvokeCtx,
        system_timestamp_ms: u64,
    ) -> Vec<u64> {
        ctx.events
            .drain(..)
            .map(|ev| {
                self.event_log
                    .append(endpoint, cluster, ev, system_timestamp_ms)
            })
            .collect()
    }

    /// Applies an external stimulus (`core::stimulus`) to `endpoint`: the
    /// first cluster there that claims it (`ClusterHandler::stimulate`
    /// answering anything but `Unsupported`) handles it, and whatever it
    /// changed/emitted is turned into a DataVersion bump plus event-log
    /// entries — the same bookkeeping `invoke_on_endpoint` does for a
    /// command. `system_timestamp_ms` is the device's uptime in
    /// milliseconds (see `drain_events`).
    ///
    /// No ACL check: a stimulus is the physical world acting on the device
    /// (a finger on a button), not a Matter session acting on it — there is
    /// no subject to check. The reporting side is where access control
    /// applies (`event_entries`).
    pub fn stimulate(
        &mut self,
        endpoint: u16,
        stimulus: &Stimulus,
        system_timestamp_ms: u64,
    ) -> Result<StimulusOutcome, StimulusError> {
        let Some((_, clusters)) = self.endpoints.iter_mut().find(|(id, _)| *id == endpoint) else {
            return Err(StimulusError::UnknownEndpoint);
        };
        let mut ctx = InvokeCtx::default();
        let mut target = None;
        for handler in clusters.iter_mut() {
            // Each candidate starts clean: a cluster that answers
            // `Unsupported` after pushing something (a bug, but a cheap one
            // to contain) must not leak into the next one's report.
            ctx.changed.clear();
            ctx.events.clear();
            match handler.stimulate(stimulus, &mut ctx) {
                StimulusReply::Unsupported => continue,
                StimulusReply::Rejected(reason) => return Err(StimulusError::Rejected(reason)),
                StimulusReply::Applied => {
                    target = Some(handler.cluster_id());
                    break;
                }
            }
        }
        // `clusters`' mutable borrow of `self.endpoints` ends here — every
        // line below touches `self.versions` / `self.event_log`, which the
        // borrow checker (rightly) won't allow while a handler is borrowed.
        let Some(cluster) = target else {
            return Err(StimulusError::Unsupported);
        };
        let (changed, event_numbers) =
            self.commit_changes(endpoint, cluster, &mut ctx, system_timestamp_ms);
        Ok(StimulusOutcome {
            changed,
            event_numbers,
        })
    }

    /// Expands `paths` (EventPathIB wildcards included, spec §8.9.2.2)
    /// against the event log and returns everything with an EventNumber ≥
    /// `event_min` (the request's `EventFilterIB::EventMin`, spec §8.9.2.4)
    /// that `read_ctx`'s session may see, EventNumber ascending and
    /// de-duplicated across overlapping paths.
    ///
    /// The resolution rules mirror `read_entries`' attribute side: a
    /// wildcard field expands silently (a combination that resolves to
    /// nothing simply contributes nothing), while a **fully concrete**
    /// path that doesn't resolve is answered with an `EventEntryOut::
    /// Status` — `UNSUPPORTED_ENDPOINT` / `UNSUPPORTED_CLUSTER` /
    /// `UNSUPPORTED_EVENT`. A concrete path that *does* resolve but has
    /// nothing in the log yields nothing at all (not a status): the event
    /// exists, it just hasn't happened.
    ///
    /// ACL (spec §9.10) is applied per stored event, against the emitting
    /// cluster's `event_privilege`. A stored event whose `(endpoint,
    /// cluster)` no longer resolves (a cluster removed after it was logged)
    /// is dropped — there is no handler left to ask for its privilege, and
    /// reporting it unchecked would leak past the ACL.
    pub fn event_entries(
        &self,
        paths: &[EventPathIn],
        event_min: u64,
        read_ctx: &ReadCtx,
    ) -> Vec<EventEntryOut> {
        let mut out: Vec<EventEntryOut> = Vec::new();
        let mut seen: BTreeSet<u64> = BTreeSet::new();
        for path in paths {
            if let (Some(endpoint), Some(cluster), Some(event)) =
                (path.endpoint, path.cluster, path.event)
            {
                if let Some(status) = self.concrete_event_path_status(endpoint, cluster, event) {
                    out.push(EventEntryOut::Status {
                        endpoint,
                        cluster,
                        event,
                        status,
                    });
                    continue;
                }
            }
            for stored in self.event_log.since(event_min) {
                if seen.contains(&stored.number) || !event_path_matches(path, stored) {
                    continue;
                }
                let Some(handler) = self.handler_for(stored.endpoint, stored.cluster) else {
                    // The cluster that emitted this is gone: no
                    // `event_privilege` to check it against, so it is not
                    // reportable any more.
                    continue;
                };
                if !acl_allows(
                    &self.acl,
                    read_ctx.fabric_index,
                    read_ctx.subject,
                    handler.event_privilege(stored.event),
                    stored.endpoint,
                    stored.cluster,
                ) {
                    continue;
                }
                seen.insert(stored.number);
                out.push(EventEntryOut::Data(EventReportOut {
                    endpoint: stored.endpoint,
                    cluster: stored.cluster,
                    event: stored.event,
                    event_number: stored.number,
                    priority: stored.priority,
                    system_timestamp_ms: stored.system_timestamp_ms,
                    data_tlv: stored.data_tlv.clone(),
                }));
            }
        }
        // `since` already walks the log in ascending order, but several
        // paths each contribute their own ascending run — sort so the
        // report as a whole is ascending (spec §8.9.2.6: EventNumber
        // ordering is what lets a subscriber advance its EventMin).
        out.sort_by_key(|e| match e {
            EventEntryOut::Data(d) => d.event_number,
            // Statuses aren't numbered; keep them ahead of the data they
            // were requested alongside rather than interleaved arbitrarily.
            EventEntryOut::Status { .. } => 0,
        });
        out
    }

    /// The IM status a *fully concrete* event path resolves to, or `None`
    /// when it resolves cleanly (endpoint exists, cluster exists on it, and
    /// the cluster declares this event id).
    fn concrete_event_path_status(&self, endpoint: u16, cluster: u32, event: u32) -> Option<u8> {
        let Some((_, clusters)) = self.endpoints.iter().find(|(id, _)| *id == endpoint) else {
            return Some(im::STATUS_UNSUPPORTED_ENDPOINT);
        };
        let Some(handler) = clusters.iter().find(|h| h.cluster_id() == cluster) else {
            return Some(im::STATUS_UNSUPPORTED_CLUSTER);
        };
        if handler.events().contains(&event) {
            None
        } else {
            Some(im::STATUS_UNSUPPORTED_EVENT)
        }
    }

    /// The handler serving `(endpoint, cluster)`, if any.
    fn handler_for(&self, endpoint: u16, cluster: u32) -> Option<&dyn ClusterHandler> {
        self.endpoints
            .iter()
            .find(|(id, _)| *id == endpoint)?
            .1
            .iter()
            .find(|h| h.cluster_id() == cluster)
            .map(|h| h.as_ref())
    }

    /// The event-side counterpart of [`has_readable_path`]: whether a
    /// SubscribeRequest's `event_paths` may be accepted at all (spec §8.10).
    /// A fully concrete path always counts (an unresolvable one is answered
    /// by a status entry in the priming report instead); a path with any
    /// wildcard field counts only if some endpoint×cluster it expands to
    /// declares an event (matching `path.event` when that is concrete) this
    /// session is allowed to receive. Empty `paths` is `false` — the caller
    /// answers `INVALID_ACTION`.
    ///
    /// Note this asks the *schema* (`ClusterHandler::events`), not the log:
    /// a subscription to an event that simply hasn't fired yet is perfectly
    /// valid.
    ///
    /// [`has_readable_path`]: Self::has_readable_path
    pub fn has_readable_event_path(&self, paths: &[EventPathIn], read_ctx: &ReadCtx) -> bool {
        paths.iter().any(|path| {
            if path.endpoint.is_some() && path.cluster.is_some() && path.event.is_some() {
                return true;
            }
            self.endpoints
                .iter()
                .filter(|(ep, _)| path.endpoint.is_none_or(|e| e == *ep))
                .any(|(endpoint, clusters)| {
                    clusters
                        .iter()
                        .filter(|h| path.cluster.is_none_or(|c| c == h.cluster_id()))
                        .any(|handler| {
                            handler
                                .events()
                                .into_iter()
                                .filter(|e| path.event.is_none_or(|want| want == *e))
                                .any(|e| {
                                    acl_allows(
                                        &self.acl,
                                        read_ctx.fabric_index,
                                        read_ctx.subject,
                                        handler.event_privilege(e),
                                        *endpoint,
                                        handler.cluster_id(),
                                    )
                                })
                        })
                })
        })
    }
}

/// Whether a stored event satisfies one requested `EventPathIn` — a `None`
/// field is a wildcard that matches anything (spec §8.9.2.2). `urgent` is
/// not a selector (it asks for a faster report, not a different set), so it
/// is deliberately ignored here.
fn event_path_matches(path: &EventPathIn, stored: &StoredEvent) -> bool {
    path.endpoint.is_none_or(|e| e == stored.endpoint)
        && path.cluster.is_none_or(|c| c == stored.cluster)
        && path.event.is_none_or(|ev| ev == stored.event)
}
