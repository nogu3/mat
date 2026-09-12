use super::*;

impl Node {
    pub(super) fn handle_read(
        &self,
        payload: &[u8],
        read_ctx: &ReadCtx,
    ) -> Result<ImOutcome, ImServerError> {
        let paths = im::decode_read_request(payload)?;
        // `usize::MAX` never triggers a split, so this always yields
        // exactly one chunk — the same single-message
        // `more_chunks=false, suppress_response=true` reply `handle_read`
        // has always returned. `handle_im` is only reached by direct unit
        // tests and any opcode still routed through the generic dispatch;
        // the real chunk-aware flow (`read_chunks` with `net::runtime`'s
        // `REPORT_CHUNK_BUDGET`) is `net::runtime::serve_secured_message`'s
        // job (Task 6) — it bypasses `handle_im` for `OPCODE_READ_REQUEST`
        // entirely so it can drive the multi-chunk StatusResponse
        // round-trip.
        let chunks = self.read_chunks(&paths, read_ctx, usize::MAX, None, false);
        Ok(ImOutcome::unchanged(
            im::OPCODE_REPORT_DATA,
            chunks
                .into_iter()
                .next()
                .expect("read_chunks always yields at least one chunk"),
        ))
    }

    /// Splits `read_entries(paths, read_ctx)`'s report into one or more
    /// encoded `ReportData` payloads, each at most `budget` bytes — except
    /// a single entry that alone exceeds `budget`, which is never split
    /// (no sub-report structure to split at this layer) and goes out alone
    /// in its own over-budget chunk. Greedy: entries are appended to the
    /// current chunk one at a time; the first one that would push the
    /// *non-final-shape* encoded length (`more_chunks=true` — see the
    /// probe comment inline) over `budget` starts a new chunk instead of
    /// splitting mid-report — simplicity over packing efficiency, since
    /// this runs once per read/priming, not in a hot loop.
    ///
    /// Every chunk but the last is encoded `more_chunks=true,
    /// suppress_response=false` — the receiver must answer
    /// `StatusResponse(0)` on the same exchange before the next chunk
    /// (spec §8.9.2.3's chunk handshake; `net::runtime::
    /// serve_read_request_chunked` drives it, mirroring `SecureSession::
    /// subscribe_wildcard`'s priming-report loop on the initiator side).
    ///
    /// `subscription_id` says which of the two callers this is, and changes
    /// the *last* chunk's shape accordingly:
    /// - `None` (a plain `ReadRequest`): the last chunk is `more_chunks=
    ///   false, suppress_response=true` — identical to what a single-chunk
    ///   read has always encoded, so a read that fits in one chunk is
    ///   unchanged (no regression). The read interaction ends there.
    /// - `Some(id)` (a subscription's priming report, Task 12): every chunk
    ///   including the last carries the SubscriptionId and
    ///   `suppress_response=false`, because the priming report is *not* the
    ///   end of the interaction — a `SubscribeResponse` follows on the same
    ///   exchange (spec §8.10), and the initiator answers every priming
    ///   chunk with `StatusResponse(0)` first (`SecureSession::
    ///   subscribe_wildcard`'s loop does exactly that).
    ///
    /// `trailer_follows` says another ReportData chunk — one this call does
    /// not produce — still follows the last one: the caller is going to
    /// append event reports of its own (`event_entries`, on a subscription's
    /// priming report). The last chunk is then encoded `more_chunks=true`
    /// (and never suppressed), so the receiver keeps reading instead of
    /// treating the attribute chunks as the end of the report. `false` is
    /// the plain case (attributes are all there is) and keeps the byte-for-
    /// byte shape this method has always produced.
    ///
    /// Always returns at least one chunk, even for zero entries (an empty
    /// `ReportData`, matching the pre-Task-6 always-one-chunk behavior for
    /// a read that matches nothing).
    pub fn read_chunks(
        &self,
        paths: &[AttrPathIn],
        read_ctx: &ReadCtx,
        budget: usize,
        subscription_id: Option<u32>,
        trailer_follows: bool,
    ) -> Vec<Vec<u8>> {
        let entries = self.read_entries(paths, read_ctx);
        let mut batches: Vec<Vec<ReportEntryOut>> = Vec::new();
        let mut current: Vec<ReportEntryOut> = Vec::new();
        for entry in entries {
            let mut candidate = current.clone();
            candidate.push(entry.clone());
            // Probe with `more_chunks=true` (the shape every non-final
            // batch is actually encoded with below — `MoreChunkedMessages`
            // adds a 2-byte TLV element that `more_chunks=false` doesn't
            // have) rather than the smaller final-chunk shape. Probing
            // with the smaller shape could let a batch through whose real
            // non-final encoding then lands 1-2 bytes over `budget` (fix
            // round 1, code review). Only the *last* batch ends up encoded
            // smaller (`more_chunks=false`) than what it was probed at —
            // strictly safe, since a batch that already fit the larger
            // probed shape still fits the smaller final one.
            let candidate_len =
                im::encode_report_data_entries(&candidate, false, subscription_id, true).len();
            if candidate_len > budget && !current.is_empty() {
                batches.push(std::mem::take(&mut current));
                current.push(entry);
            } else {
                current = candidate;
            }
        }
        batches.push(current); // always ≥1 batch, even for zero entries

        let last = batches.len() - 1;
        batches
            .into_iter()
            .enumerate()
            .map(|(i, batch)| {
                let is_last = i == last;
                // Priming (`subscription_id.is_some()`): never suppress —
                // the SubscribeResponse still has to follow on this
                // exchange, so the initiator must answer even the last
                // chunk with `StatusResponse(0)`. Same for a caller that
                // still has an event chunk to append (`trailer_follows`).
                let suppress = is_last && subscription_id.is_none() && !trailer_follows;
                let more = !is_last || trailer_follows;
                im::encode_report_data_entries(&batch, suppress, subscription_id, more)
            })
            .collect()
    }

    /// Expands every `AttrPathIn` in `paths` (wildcard endpoint/cluster/
    /// attribute fields included) against the registry into concrete
    /// report entries. Also the entry point Task 6/12 (subscriptions) will
    /// reuse for priming/dirty reports.
    ///
    /// Wildcard expansion rule (mirrors spec §8.9.2.3's path-resolution
    /// semantics at the level this skeleton needs): a field left wildcard
    /// (`None`) always expands to every matching registry entry, silently
    /// (no report, no error) when a resolved-but-lower-level lookup comes
    /// up empty — e.g. a wildcard-cluster expansion landing on an endpoint
    /// that doesn't implement some concrete attribute just contributes
    /// nothing for that combination. A field that was itself concrete
    /// (`Some`) and fails to resolve, with every *more significant* field
    /// in the same path also concrete, is reported as a per-path
    /// `ReportEntryOut::Status` instead: `UNSUPPORTED_ENDPOINT` /
    /// `UNSUPPORTED_CLUSTER` / `UNSUPPORTED_ATTRIBUTE`. Global attributes
    /// (`ATTR_CLUSTER_REVISION` etc., spec §7.13) are only ever answered
    /// when concretely requested — wildcard attribute expansion enumerates
    /// `ClusterHandler::attributes()` alone, keeping full-wildcard reads
    /// from ballooning (chip-tool/Echo commonly issue one).
    pub fn read_entries(&self, paths: &[AttrPathIn], read_ctx: &ReadCtx) -> Vec<ReportEntryOut> {
        let mut out = Vec::new();
        for path in paths {
            self.expand_endpoint(path, read_ctx, &mut out);
        }
        out
    }

    /// Whether a SubscribeRequest's `paths` may be accepted at all (spec
    /// §8.10; chip `InteractionModelEngine::ParseAttributePaths`): a path
    /// with any wildcard field counts only if it expands to at least one
    /// attribute this session may read (ACL included — `read_allowed`,
    /// the same gate `read_entries` applies), while a fully concrete path
    /// always counts (a missing or refused attribute is answered by a
    /// status entry in the priming report instead). Values are not read,
    /// except in one sub-case: a wildcard endpoint/cluster paired with a
    /// concrete attribute id also needs the value-resolution check
    /// (`read_attribute_value`, mirroring `expand_attribute`'s concrete-
    /// attribute branch) — otherwise a nonexistent attribute id would
    /// count as valid, since every handler's default `read_privilege` lets
    /// `read_allowed` pass for any id. `false` for an empty `paths` — but
    /// that alone is not a refusal: an event-only request qualifies through
    /// [`Node::has_readable_event_path`], and only a request readable on
    /// *neither* side is answered with `INVALID_ACTION` (the two gates are
    /// OR'd in `net::runtime::serve_subscribe_request`).
    pub fn has_readable_path(&self, paths: &[AttrPathIn], read_ctx: &ReadCtx) -> bool {
        paths.iter().any(|path| {
            if path.endpoint.is_some() && path.cluster.is_some() && path.attribute.is_some() {
                return true;
            }
            self.endpoints
                .iter()
                .filter(|(ep, _)| path.endpoint.is_none_or(|e| e == *ep))
                .any(|(endpoint, clusters)| {
                    let ectx = ExpandCtx {
                        endpoint: *endpoint,
                        clusters,
                        read_ctx,
                    };
                    clusters
                        .iter()
                        .filter(|h| path.cluster.is_none_or(|c| c == h.cluster_id()))
                        .any(|handler| match path.attribute {
                            Some(attribute) => {
                                self.read_allowed(&ectx, handler.as_ref(), attribute)
                                    && self
                                        .read_attribute_value(&ectx, handler.as_ref(), attribute)
                                        .is_some()
                            }
                            None => handler
                                .attributes()
                                .into_iter()
                                .any(|a| self.read_allowed(&ectx, handler.as_ref(), a)),
                        })
                })
        })
    }

    fn expand_endpoint(
        &self,
        path: &AttrPathIn,
        read_ctx: &ReadCtx,
        out: &mut Vec<ReportEntryOut>,
    ) {
        match path.endpoint {
            Some(endpoint) => match self.endpoints.iter().find(|(id, _)| *id == endpoint) {
                Some((_, clusters)) => {
                    let ectx = ExpandCtx {
                        endpoint,
                        clusters,
                        read_ctx,
                    };
                    self.expand_cluster(&ectx, path, true, out)
                }
                None => out.push(ReportEntryOut::Status {
                    endpoint,
                    cluster: path.cluster.unwrap_or(0),
                    attribute: path.attribute.unwrap_or(0),
                    status: im::STATUS_UNSUPPORTED_ENDPOINT,
                }),
            },
            None => {
                for (endpoint, clusters) in &self.endpoints {
                    let ectx = ExpandCtx {
                        endpoint: *endpoint,
                        clusters,
                        read_ctx,
                    };
                    self.expand_cluster(&ectx, path, false, out);
                }
            }
        }
    }

    /// `endpoint_concrete`: whether `ectx.endpoint` was resolved from a
    /// concrete path field (`true`) or a wildcard expansion (`false`) —
    /// determines whether an unresolvable *concrete* cluster on this
    /// endpoint is a per-path error (`endpoint_concrete` path) or just
    /// skipped (wildcard endpoint expansion landing on an endpoint without
    /// this cluster).
    fn expand_cluster(
        &self,
        ectx: &ExpandCtx,
        path: &AttrPathIn,
        endpoint_concrete: bool,
        out: &mut Vec<ReportEntryOut>,
    ) {
        match path.cluster {
            Some(cluster) => match ectx.clusters.iter().find(|h| h.cluster_id() == cluster) {
                Some(handler) => {
                    self.expand_attribute(ectx, handler.as_ref(), path, endpoint_concrete, out)
                }
                None => {
                    if endpoint_concrete {
                        out.push(ReportEntryOut::Status {
                            endpoint: ectx.endpoint,
                            cluster,
                            attribute: path.attribute.unwrap_or(0),
                            status: im::STATUS_UNSUPPORTED_CLUSTER,
                        });
                    }
                }
            },
            None => {
                for handler in ectx.clusters {
                    self.expand_attribute(ectx, handler.as_ref(), path, false, out);
                }
            }
        }
    }

    /// `concrete_so_far`: `true` only when both `ectx.endpoint` and the
    /// cluster were resolved from concrete path fields — the precondition
    /// (spec §8.9.2.3) for a missing concrete attribute to be a per-path
    /// `UNSUPPORTED_ATTRIBUTE` rather than silently dropped.
    fn expand_attribute(
        &self,
        ectx: &ExpandCtx,
        handler: &dyn ClusterHandler,
        path: &AttrPathIn,
        concrete_so_far: bool,
        out: &mut Vec<ReportEntryOut>,
    ) {
        let cluster = handler.cluster_id();
        match path.attribute {
            Some(attribute) => {
                // ACL (spec §9.10) before the value is even read. A denied
                // *concrete* path is reported as `UNSUPPORTED_ACCESS` (the
                // requester asked for exactly this attribute and deserves
                // to know it was refused); a denied path that got here by
                // wildcard expansion is dropped silently, the same
                // treatment `UNSUPPORTED_ATTRIBUTE` gets below — otherwise
                // one wildcard read from a low-privilege controller would
                // come back as a wall of status entries.
                if !self.read_allowed(ectx, handler, attribute) {
                    if concrete_so_far {
                        out.push(ReportEntryOut::Status {
                            endpoint: ectx.endpoint,
                            cluster,
                            attribute,
                            status: im::STATUS_UNSUPPORTED_ACCESS,
                        });
                    }
                    return;
                }
                match self.read_attribute_value(ectx, handler, attribute) {
                    Some(value_tlv) => out.push(ReportEntryOut::Data(AttrReportOut {
                        endpoint: ectx.endpoint,
                        cluster,
                        attribute,
                        data_version: self.data_version(ectx.endpoint, cluster),
                        value_tlv,
                    })),
                    None if concrete_so_far => out.push(ReportEntryOut::Status {
                        endpoint: ectx.endpoint,
                        cluster,
                        attribute,
                        status: im::STATUS_UNSUPPORTED_ATTRIBUTE,
                    }),
                    // Wildcard-expanded attribute that resolved to nothing:
                    // shouldn't happen (only ids from `attributes()` reach
                    // here in the wildcard branch below), but dropped
                    // defensively rather than reported.
                    None => {}
                }
            }
            None => {
                // Wildcard attribute: enumerate the cluster's own
                // attributes only — global attributes are deliberately
                // excluded from wildcard expansion (see `read_entries`'s
                // doc).
                for attribute in handler.attributes() {
                    // Per-attribute ACL: a cluster can be readable at View
                    // for most of its attributes and Administer for one
                    // (AccessControl's `ACL`), so the check belongs here,
                    // not once per cluster. Denied = silently skipped.
                    if !self.read_allowed(ectx, handler, attribute) {
                        continue;
                    }
                    if let Some(value_tlv) = self.read_attribute_value(ectx, handler, attribute) {
                        out.push(ReportEntryOut::Data(AttrReportOut {
                            endpoint: ectx.endpoint,
                            cluster,
                            attribute,
                            data_version: self.data_version(ectx.endpoint, cluster),
                            value_tlv,
                        }));
                    }
                    // `None`: `attributes()` promised this id but `read`
                    // disagrees — dropped silently (brief: defensive, not
                    // expected to happen).
                }
            }
        }
    }

    /// Whether the reading session (`ectx.read_ctx`'s fabric/subject) may
    /// read this `(endpoint, cluster, attribute)` — see `acl_allows` and
    /// `read_privilege_for` for the two halves of the decision.
    fn read_allowed(&self, ectx: &ExpandCtx, handler: &dyn ClusterHandler, attribute: u32) -> bool {
        acl_allows(
            &self.acl,
            ectx.read_ctx.fabric_index,
            ectx.read_ctx.subject,
            read_privilege_for(handler, attribute),
            ectx.endpoint,
            handler.cluster_id(),
        )
    }

    /// Reads one concrete (endpoint, cluster, attribute), intercepting the
    /// handful of attributes `Node` — not the per-cluster handler — owns:
    /// ServerList/endpoint-0 PartsList (need registry-wide visibility, see
    /// below) and the five global attributes (spec §7.13, synthesized
    /// uniformly for every cluster rather than duplicated into each
    /// `ClusterHandler::read`).
    fn read_attribute_value(
        &self,
        ectx: &ExpandCtx,
        handler: &dyn ClusterHandler,
        attribute: u32,
    ) -> Option<Vec<u8>> {
        let cluster = handler.cluster_id();
        // ServerList (spec §9.5) must reflect the clusters actually
        // registered on this endpoint — including ones added after
        // `with_root_endpoint` (e.g. a device runtime's commissioning
        // clusters via `add_cluster`), so it's derived here from the
        // registry rather than left to `DescriptorHandler`'s own `read`
        // (which has no visibility into its siblings). Endpoint 0's
        // PartsList (spec §9.5, "the endpoint composition tree") is the
        // same story one level up: it must list every *other* endpoint
        // registered on this `Node`, which `DescriptorHandler` — scoped to
        // a single endpoint's own cluster list — has no way to see either.
        // Non-0 endpoints (M2: only endpoint 1) have no children of their
        // own, so their PartsList stays `DescriptorHandler`'s own
        // always-empty answer.
        if cluster == im::CLUSTER_DESCRIPTOR && attribute == im::ATTR_SERVER_LIST {
            return Some(encode_server_list(ectx.clusters));
        }
        if cluster == im::CLUSTER_DESCRIPTOR
            && attribute == im::ATTR_PARTS_LIST
            && ectx.endpoint == 0
        {
            return Some(encode_parts_list(&self.endpoints));
        }
        match attribute {
            im::ATTR_CLUSTER_REVISION => Some(tlv_value::uint(u64::from(handler.revision()))),
            im::ATTR_FEATURE_MAP => Some(tlv_value::uint(u64::from(handler.feature_map()))),
            im::ATTR_ATTRIBUTE_LIST => Some(encode_attribute_list(handler)),
            im::ATTR_ACCEPTED_COMMAND_LIST => {
                Some(encode_command_list(&handler.accepted_commands()))
            }
            im::ATTR_GENERATED_COMMAND_LIST => {
                Some(encode_command_list(&handler.generated_commands()))
            }
            _ => handler.read(attribute, ectx.read_ctx),
        }
    }
}

/// Encodes the Descriptor cluster's `ServerList` (spec §9.5) from the
/// clusters actually registered on the endpoint — see
/// `Node::read_attribute_value`'s override for why this lives here rather
/// than in `DescriptorHandler`.
fn encode_server_list(clusters: &[Box<dyn ClusterHandler>]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for handler in clusters {
        w.put_uint(Tag::Anonymous, u64::from(handler.cluster_id()));
    }
    w.end_container();
    w.finish()
}

/// Encodes the Descriptor cluster's `PartsList` (spec §9.5) for endpoint 0:
/// every *other* endpoint id registered on the `Node`, in registration
/// order — see `Node::read_attribute_value`'s override for why this lives
/// here rather than in `DescriptorHandler` (which only knows its own
/// endpoint).
fn encode_parts_list(endpoints: &[(u16, Vec<Box<dyn ClusterHandler>>)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (id, _) in endpoints {
        if *id != 0 {
            w.put_uint(Tag::Anonymous, u64::from(*id));
        }
    }
    w.end_container();
    w.finish()
}

/// Encodes the global `AttributeList` attribute (spec §7.13, id
/// `ATTR_ATTRIBUTE_LIST`): every attribute id the cluster serves —
/// `handler.attributes()`'s cluster-specific ids plus the five global ids
/// every cluster carries (including `AttributeList`'s own id).
fn encode_attribute_list(handler: &dyn ClusterHandler) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for id in handler.attributes() {
        w.put_uint(Tag::Anonymous, u64::from(id));
    }
    for id in GLOBAL_ATTRIBUTE_IDS {
        w.put_uint(Tag::Anonymous, u64::from(id));
    }
    w.end_container();
    w.finish()
}

/// The five global attributes (spec §7.13) every cluster carries, in
/// addition to whatever `ClusterHandler::attributes()` declares.
const GLOBAL_ATTRIBUTE_IDS: [u32; 5] = [
    im::ATTR_GENERATED_COMMAND_LIST,
    im::ATTR_ACCEPTED_COMMAND_LIST,
    im::ATTR_ATTRIBUTE_LIST,
    im::ATTR_FEATURE_MAP,
    im::ATTR_CLUSTER_REVISION,
];

/// `AcceptedCommandList`/`GeneratedCommandList` (spec §7.13): the cluster's
/// `ClusterHandler::accepted_commands`/`generated_commands` answer, as a
/// TLV array of command ids.
fn encode_command_list(ids: &[u32]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for id in ids {
        w.put_uint(Tag::Anonymous, u64::from(*id));
    }
    w.end_container();
    w.finish()
}
