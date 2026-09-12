use super::*;

/// Random 64-bit hex instance/hostname (spec §4.3.1: the commissionable
/// service's instance name SHOULD be random). Reused as both the mDNS
/// instance name and the hostname (`<name>.local`) — legal, and simplest
/// for M1 (a real hostname-vs-instance split buys nothing here since both
/// ultimately resolve to the same one address this device advertises).
fn random_hex_name() -> String {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("os rng");
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Reads `/proc/net/if_inet6` for `iface`'s link-local (scope 0x20) IPv6
/// address — same parsing technique `mat_native::iface_select::scan` uses
/// for the same file, duplicated locally rather than shared (that helper
/// lives in a different crate and returns iface *names*, not addresses).
/// Linux-specific; this runtime targets Linux like the rest of the
/// `net` feature (raw `socket2`/`/proc` usage throughout `net::mdns`).
fn iface_link_local_addr(iface: &str) -> Result<Ipv6Addr, DeviceError> {
    let content = std::fs::read_to_string("/proc/net/if_inet6").map_err(DeviceError::Io)?;
    for line in content.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 6 && cols[3] == "20" && cols[5] == iface {
            let hex = cols[0];
            if hex.len() != 32 {
                continue;
            }
            let mut bytes = [0u8; 16];
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                    .map_err(|_| DeviceError::Iface(format!("bad if_inet6 hex for {iface}")))?;
            }
            return Ok(Ipv6Addr::from(bytes));
        }
    }
    Err(DeviceError::Iface(format!(
        "no link-local ipv6 address on interface {iface}"
    )))
}

/// The mDNS advertiser plus everything needed to build adverts for it
/// (hostname/port/address are fixed for the process lifetime once
/// resolved). Kept together, behind one `Option`, because bringing up mDNS
/// is best-effort — see `run`'s doc comment on why a device that can't
/// join the multicast group must still serve PASE/CASE/IM traffic to a
/// peer that already knows its address.
pub(super) struct MdnsCtx {
    pub(super) mdns: Arc<MdnsAdvertiser>,
    hostname: String,
    port: u16,
    addr_v6: Ipv6Addr,
}

impl MdnsCtx {
    /// The commissionable advert for the current window
    /// (`advert_params_for_window` decides `discriminator`/`cm`). A fresh
    /// random instance name every time, per spec §4.3.1.
    fn commissionable_advert(
        &self,
        config: &DeviceConfig,
        discriminator: u16,
        cm: u8,
    ) -> CommissionableAdvert {
        CommissionableAdvert {
            instance: random_hex_name(),
            hostname: self.hostname.clone(),
            discriminator,
            vendor_id: config.vendor_id,
            product_id: config.product_id,
            port: self.port,
            addr_v6: self.addr_v6,
            cm,
        }
    }

    /// Builds the `OperationalAdvert` for one installed fabric entry.
    fn operational_advert(&self, entry: &FabricEntry) -> OperationalAdvert {
        OperationalAdvert {
            compressed_fabric_id: compressed_fabric_id(&entry.root_public_key, entry.fabric_id),
            node_id: entry.node_id,
            hostname: self.hostname.clone(),
            port: self.port,
            addr_v6: self.addr_v6,
        }
    }

    /// Withdraws `entry`'s operational advert (goodbye + drop).
    pub(super) async fn retire_operational(&self, entry: &FabricEntry) {
        let cfid = compressed_fabric_id(&entry.root_public_key, entry.fabric_id);
        self.mdns
            .remove_operational(u64::from_be_bytes(cfid), entry.node_id)
            .await;
    }
}

/// Brings up the mDNS advertiser: resolves `config.iface`, spawns the
/// advertiser, sets the commissionable advert (only while `window` is open —
/// see below), and republishes every fabric already on disk (the restart
/// path: a second `Device::new` over the same `store_dir` reloads
/// `comm_server.fabrics()` from disk, and this makes sure those fabrics are
/// still discoverable operationally after the restart). Failure (bad
/// interface name, no link-local address, socket bind failure) is reported
/// via `Err` so `run` can log it — but `run` itself treats that as
/// *non-fatal*: mDNS is how a real controller finds this device, but a
/// device unreachable by discovery still MUST answer a peer that already
/// has its address (exactly `direct_drive_*`'s test setup, and not
/// unrealistic — e.g. a controller with a cached address).
///
/// `window` (Task 14 fix round 1, review item 1; widened from a plain
/// `window_open: bool` to `&CommissioningWindow` by Task 4 so a retry
/// republishes the *right* advert — boot `CM=1`/discriminator or ECM
/// `CM=2`/`WindowRequest` discriminator, via `advert_params_for_window` —
/// not just whether one should exist at all): a fresh `MdnsAdvertiser::spawn`
/// starts with no commissionable advert set (its `commissionable` field is
/// `RwLock::new(None)`), so simply *not* calling `set_commissionable(Some(..))`
/// when the window is closed is sufficient — no explicit `None` call needed
/// to reach the right end state. Threaded from both of `run`'s call sites
/// (initial bring-up and every retry), this closes two related holes at
/// once:
/// - **Retry-after-close**: without this, a `CommissioningComplete`- or
///   deadline-expiry close that lands *while* mDNS is still down (mid
///   `MdnsRetry` backoff) would be silently undone the moment the retry
///   later succeeds — `bring_up_mdns` used to always publish commissionable
///   unconditionally, reviving an advert for a window that had already
///   sent its goodbye. `run` now re-reads the *current* window at the
///   moment each retry actually runs, not the state from when the retry was
///   scheduled.
/// - **Boot-with-fabric**: a device restarting with a fabric already on
///   disk starts with `window = Closed` (Task 14's boot policy) — this
///   parameter means such a restart no longer publishes a commissionable
///   advert it would just silently refuse PASE against.
pub(super) async fn bring_up_mdns(
    config: &DeviceConfig,
    port: u16,
    comm_server: &CommissioningServer,
    window: &CommissioningWindow,
) -> Result<MdnsCtx, DeviceError> {
    let scope_id =
        mat_controller::dnssd::iface_index(&config.iface).map_err(DeviceError::IfaceIndex)?;
    let addr_v6 = iface_link_local_addr(&config.iface)?;
    let hostname = random_hex_name();

    let mdns = MdnsAdvertiser::spawn(scope_id)
        .await
        .map_err(DeviceError::Io)?;
    let ctx = MdnsCtx {
        mdns,
        hostname,
        port,
        addr_v6,
    };
    if let Some((discriminator, cm)) = advert_params_for_window(window, config.discriminator) {
        ctx.mdns
            .set_commissionable(Some(ctx.commissionable_advert(config, discriminator, cm)))
            .await;
    }
    for entry in comm_server.fabrics() {
        ctx.mdns
            .add_operational(ctx.operational_advert(&entry))
            .await;
    }
    // Every `set_commissionable`/`add_operational` call above already
    // announces the advert set as it stood at that point (see
    // `MdnsAdvertiser`'s doc comment), so restoring N fabrics on a restart
    // already sends N+1 announcements. One more explicit announce here
    // covers the case that matters most for a *fresh* boot with zero
    // restored fabrics — a bare commissionable-only advert still gets
    // proactively broadcast the moment mDNS is up, not just answered on
    // demand — and is otherwise harmless (RFC 6762 puts no limit on how
    // often a responder may announce its own records).
    ctx.mdns.announce().await;

    Ok(ctx)
}

/// `bring_up_mdns` retry backoff state, kept only while mDNS hasn't come up
/// yet (review fix round 1, item 1): a boot-time failure — e.g. IPv6
/// Duplicate Address Detection not finished yet on real hardware — must not
/// leave the device permanently invisible to discovery while it otherwise
/// looks up and would happily answer a PASE it never gets to see. Policy is
/// deliberately simple (not adaptive/jittered): retry every
/// `MDNS_RETRY_INTERVAL_INITIAL` for the first `MDNS_RETRY_BACKOFF_THRESHOLD`
/// of failures, then every `MDNS_RETRY_INTERVAL_LONG` — enough to recover
/// quickly from a transient startup race without hammering a genuinely bad
/// interface name forever.
pub(super) struct MdnsRetry {
    pub(super) first_failure_at: Instant,
    pub(super) next_attempt_at: Instant,
}

/// Every 5s while mDNS has been down for less than a minute...
pub(super) const MDNS_RETRY_INTERVAL_INITIAL: Duration = Duration::from_secs(5);
/// ...then every 60s after that.
pub(super) const MDNS_RETRY_INTERVAL_LONG: Duration = Duration::from_secs(60);
pub(super) const MDNS_RETRY_BACKOFF_THRESHOLD: Duration = Duration::from_secs(60);

impl MdnsRetry {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        Self {
            first_failure_at: now,
            next_attempt_at: now + MDNS_RETRY_INTERVAL_INITIAL,
        }
    }

    /// Schedules the next attempt after another failure.
    pub(super) fn schedule_next(&mut self) {
        let interval = if self.first_failure_at.elapsed() < MDNS_RETRY_BACKOFF_THRESHOLD {
            MDNS_RETRY_INTERVAL_INITIAL
        } else {
            MDNS_RETRY_INTERVAL_LONG
        };
        self.next_attempt_at = Instant::now() + interval;
    }
}

/// Resolves once at `retry.next_attempt_at`, or never (`std::future::pending`)
/// when there's no retry pending (mDNS already up, or never attempted).
/// Used as a `tokio::select!` branch alongside `recv_from` so the retry
/// timer can't block datagram serving — a `None` retry state simply makes
/// this branch inert instead of needing `select!`'s `if` precondition
/// syntax (simpler to reason about with a value that's re-borrowed fresh
/// every loop iteration).
pub(super) async fn mdns_retry_deadline(retry: &Option<MdnsRetry>) {
    match retry {
        Some(r) => tokio::time::sleep_until(r.next_attempt_at).await,
        None => std::future::pending().await,
    }
}

/// AddNOC success: a fabric appeared that wasn't there before the dispatch
/// (`fabrics_before` = the count read just before `Node::handle_im`) —
/// publish its operational mDNS advert.
pub(super) async fn advertise_added_fabric(
    comm_server: &CommissioningServer,
    mdns: Option<&MdnsCtx>,
    fabrics_before: usize,
) {
    let fabrics_after = comm_server.fabrics();
    if fabrics_after.len() > fabrics_before {
        if let (Some(entry), Some(ctx)) = (fabrics_after.last(), mdns) {
            ctx.mdns
                .add_operational(ctx.operational_advert(entry))
                .await;
        }
    }
}

/// ECM window reconciliation (Task 4), per dispatch iteration — same
/// spot the AddNOC fabric-diff check (`advertise_added_fabric`) lives,
/// so a timed `OpenCommissioningWindow`/`RevokeCommissioning` invoke
/// (piggybacked on this same exchange, per the timed-invoke loop
/// `serve_secured_message` runs) is handled without waiting for the next
/// datagram. The
/// ordering invariant this depends on (take the staged request
/// *before* reading `admin_open`) and the resulting decision table
/// are `admin_window_action`'s doc comment/unit tests, not repeated
/// here — this function only performs the side effects.
pub(super) async fn reconcile_admin_window(
    comm_server: &CommissioningServer,
    mdns: Option<&MdnsCtx>,
    window: &mut CommissioningWindow,
    config: &DeviceConfig,
) {
    let pending_request = comm_server.take_pending_window_request();
    let admin_open = comm_server.admin_window_is_open();
    match admin_window_action(pending_request, admin_open, window) {
        AdminWindowAction::Apply(request) => {
            *window = apply_window_request(request);
            if let (Some(ctx), Some((discriminator, cm))) =
                (mdns, advert_params_for_window(window, config.discriminator))
            {
                ctx.mdns
                    .set_commissionable(Some(ctx.commissionable_advert(config, discriminator, cm)))
                    .await;
            }
        }
        AdminWindowAction::Close => {
            tracing::info!("administrator commissioning window revoked — closing");
            *window = CommissioningWindow::Closed;
            if let Some(ctx) = mdns {
                ctx.mdns.set_commissionable(None).await;
            }
        }
        AdminWindowAction::None => {}
    }
}

/// CommissioningComplete success: stop advertising commissionable. Decided
/// from the request already decoded (`req_cluster_command`) and the reply
/// just sent (`resp_opcode` / `resp_payload`).
pub(super) async fn close_window_on_commissioning_complete(
    resp_opcode: u8,
    req_cluster_command: Option<(u32, u32)>,
    resp_payload: &[u8],
    comm_server: &CommissioningServer,
    mdns: Option<&MdnsCtx>,
    window: &mut CommissioningWindow,
) {
    if resp_opcode != im::OPCODE_INVOKE_RESPONSE {
        return;
    }
    let Some((cluster, command)) = req_cluster_command else {
        return;
    };
    if cluster != mat_controller::commissioning::CLUSTER_GENERAL_COMMISSIONING
        || command != mat_controller::commissioning::CMD_COMMISSIONING_COMPLETE
    {
        return;
    }
    let Ok(outcome) = im::decode_invoke_response(resp_payload) else {
        return;
    };
    if outcome.status != im::STATUS_SUCCESS {
        return;
    }
    if let Some(ctx) = mdns {
        ctx.mdns.set_commissionable(None).await;
    }
    // Task 14: CommissioningComplete is the other event
    // (besides the 15-minute/`CommissioningTimeout`
    // deadline in `on_commissioning_window_expired`) that closes the
    // commissioning window — a controller that just
    // finished commissioning has no reason to PASE in
    // again, and refusing it stops a second
    // commissioner from racing in during whatever's
    // left of the window.
    *window = CommissioningWindow::Closed;
    // Task 4: this close is runtime-initiated (General
    // Commissioning's CommissioningComplete doesn't
    // touch the AC cluster's admin_window itself), so
    // tell core explicitly — keeps `WindowStatus`
    // honest for an ECM window that just got
    // committed by completion rather than expiry/
    // revoke. A no-op for the boot window.
    comm_server.close_admin_window();
}

/// RemoveFabric (Task 6): the store may have shed a fabric this
/// dispatch — either the invoking session's own (the motivating
/// case: an Android phone removing its ephemeral fabric right after
/// handing the device off to Home Assistant via
/// `OpenCommissioningWindow`) or a different one named explicitly in
/// the command fields. Either way its mDNS operational advert must
/// go. Called *after* `reply_reliable` in `serve_secured_message`, per
/// the brief: the `RemoveFabric` response itself has already been sent
/// (and, along `reply_reliable`'s normal path, acked) by this point, so
/// dropping the session (`ServeOutcome::DropSession`) never races the
/// response that announces the removal.
pub(super) async fn retire_removed_fabric(
    comm_server: &CommissioningServer,
    mdns: Option<&MdnsCtx>,
    session_fabric_index: u8,
) -> ServeOutcome {
    if let Some(entry) = comm_server.take_removed_fabric() {
        if let Some(ctx) = mdns {
            ctx.retire_operational(&entry).await;
        }
        if remove_fabric_drops_session(entry.fabric_index, session_fabric_index) {
            tracing::info!(
                fabric_index = session_fabric_index,
                node_id = entry.node_id,
                "RemoveFabric removed the invoking session's own fabric — dropping session"
            );
            return ServeOutcome::DropSession;
        }
    }
    ServeOutcome::Continue
}
