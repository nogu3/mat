//! The device runtime: a single `[::]:{port}` UDP socket serving PASE
//! commissioning, CASE, and (post-commissioning) secured Interaction Model
//! traffic — sequentially, one session at a time (spec's own design
//! principle for a constrained device; also the simplest correct thing to
//! build for M1).
//!
//! `Device::new` (`crate::device`) does all the synchronous setup (bind the
//! socket, generate a dev attestation chain, load/create the fabric store,
//! build the `Node`/`CommissioningServer`); this module is just the async
//! loop `Device::run` hands off to.
//!
//! ## Wire classification (one `recv_from` per iteration)
//!
//! Every datagram is read exactly once, right here, and classified by
//! `MessageHeader`/`ProtocolHeader`:
//! - unsecured (`session_id == 0`) + a PASE opcode (0x20-0x24) → hand off to
//!   `net::pase::drive_established` (a fresh `PaseResponderCore` self-rejects
//!   anything that isn't `PBKDFParamRequest` as the first message of a new
//!   attempt, so routing *any* PASE opcode here is safe, not just 0x20).
//! - unsecured + Sigma1 (0x30) → `net::case::drive_established`.
//! - secured, `session_id` matching the current session → fed into that
//!   `SecureSession` via `deliver_request` (a small addition to
//!   `mat_controller::session::SecureSession` for exactly this "I already
//!   read the datagram myself" case — see its doc comment) and then
//!   `Node::handle_im`.
//! - anything else (foreign secured session id, undecodable, standalone ack
//!   with no session) is silently dropped, matching every other responder
//!   in this workspace's DoS-hardening posture.
//!
//! Establishing a new PASE or CASE session replaces whatever the "current
//! session" was — this runtime never serves two peers at once (a second
//! commissioner's first datagram during an in-flight session is simply not
//! classified as a new attempt yet, since its opcode still routes to the
//! PASE/CASE handlers, which will just start a *second* concurrent
//! exchange... note below).
//!
//! Fail-safe expiry needs no special handling here: `CommissioningServer`
//! checks `is_armed()` on every gated command itself (`core::commissioning`)
//! and answers `STATUS_FAILSAFE_REQUIRED` once it lapses — this runtime just
//! forwards whatever `Node::handle_im` returns.
//!
//! ## Groupcast (spec §4.15, Task 7)
//!
//! A second, independent UDP socket (`net::group_rx::GroupSocket`) receives
//! group-session datagrams — Matter's fixed multicast port 5540, bound
//! `SO_REUSEPORT` alongside the unicast socket rather than shared with it,
//! since multicast join/leave is per-socket state the unicast path has no
//! business carrying. `sync_group_joins` runs at the top of every loop
//! iteration (desired = each fabric's own `GroupMembershipStore` groups, via
//! `group_rx::desired_group_addrs`) so an `AddGroup`, `RemoveFabric`, or
//! fail-safe rollback that changes membership is picked up on the very next
//! spin, with no dedicated event plumbing. The `select!` grows a `grecv`
//! branch reading that socket (`group_rx::group_recv`, which is
//! `std::future::pending` when the socket never bound); a decoded datagram
//! is classified by `group_rx::classify_group_datagram` and, on success,
//! applied via `Node::handle_group_invoke` under a `Subject::group(..)` —
//! never a response, per spec §4.15's fire-and-forget contract. The unicast
//! socket, in turn, drops any datagram whose `security_flags` says group
//! session right after header decode (`SESSION_TYPE_MASK`) — group traffic
//! is the group socket's job even if it happens to also reach the unicast
//! one.

use std::net::{Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::compressed_fabric_id;
use mat_controller::im;
use mat_controller::message::{
    MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, PROTOCOL_ID_INTERACTION_MODEL,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use mat_controller::pase::{
    OPCODE_PASE_PAKE1, OPCODE_PASE_PAKE2, OPCODE_PASE_PAKE3, OPCODE_PBKDF_PARAM_REQUEST,
    OPCODE_PBKDF_PARAM_RESPONSE,
};
use mat_controller::session::SecureSession;
use mat_controller::transport::{Transport, MAX_DATAGRAM};

use crate::core::access_control::Subject;
use crate::core::commissioning::{CommissioningServer, WindowRequest};
use crate::core::datamodel::{ImOutcome, InvokeCtx, Node, ReadCtx};
use crate::core::fabric_store::FabricEntry;
use crate::core::mdns_records::{CommissionableAdvert, OperationalAdvert};
use crate::core::pase::{PaseSecret, PaseVerifierConfig};
use crate::device::{DeviceConfig, DeviceError};
use crate::net::group_rx::{
    classify_group_datagram, desired_group_addrs, group_recv, GroupReplayGuard, GroupRx,
    GroupRxDeps, SESSION_TYPE_MASK,
};
use crate::net::mdns::MdnsAdvertiser;
use crate::net::subscription::ActiveSubscription;

/// PBKDF iterations this runtime advertises for PASE (spec §3.9 legal
/// range 1000..=100000). 10k rounds of PBKDF2-SHA256 on the Pi is
/// millisecond-class and paid once per commissioning attempt, so raising it
/// well above the 1000 floor costs nothing in practice while narrowing the
/// brute-force budget an attacker gets per guess.
const PASE_ITERATIONS: u32 = 10_000;

/// Sigma1's opcode (spec §4.14) — `case_responder::OPCODE_SIGMA1` is
/// `pub(crate)` to mat-controller (see `core::case`'s test module for the
/// same literal), so the runtime's classifier uses the wire value directly.
const OPCODE_CASE_SIGMA1: u8 = 0x30;

/// One secured request's MRP retry budget while replying — generous (this
/// is a device answering, not a controller racing a user-visible deadline).
fn reply_cfg() -> MrpConfig {
    MrpConfig::default()
}

/// The payload budget for one `ReportData` chunk (Task 6:
/// `Node::read_chunks`'s `budget` argument for real reads). `MAX_DATAGRAM`
/// (1280B, `mat_controller::transport`) is the hard ceiling on one UDP
/// datagram; `REPORT_CHUNK_BUDGET` leaves headroom below it for everything
/// `read_chunks` itself doesn't account for — the Matter message header,
/// the IM protocol header, and the AES-CCM 16B MIC/tag that
/// `SecureSession::seal` adds after encoding — so an encoded chunk at or
/// under this budget still fits in one real datagram once sealed. Echo/
/// chip full-wildcard reads pull in Operational Credentials' NOCs/
/// TrustedRootCertificates (certificates, ~500B class each) — well past
/// this budget on their own, which is exactly why chunking exists.
const REPORT_CHUNK_BUDGET: usize = 900;

/// How long this device is willing to stay silent on a subscription before
/// it must send *something* (spec §8.10: the MaxInterval the device
/// answers with may be anywhere at or below the subscriber's requested
/// ceiling). The requested ceiling is clamped into this range: below the
/// floor a chatty controller would have this device sending keep-alives
/// faster than a battery-less-but-still-sequential runtime wants to; above
/// the ceiling a subscription would take too long to notice a dead peer.
const MIN_MAX_INTERVAL_S: u16 = 3;
const MAX_MAX_INTERVAL_S: u16 = 60;

/// How long to wait for the peer's `StatusResponse` to a ReportData we
/// sent, when it didn't come piggybacked on the MRP ack — a LAN round-trip
/// to a controller/hub, same budget `serve_read_request_chunked` uses.
const REPORT_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// A random non-zero `SubscriptionId` (spec §8.10.3). Not collision-checked
/// for the same reason `random_session_id` isn't: this runtime holds at most
/// one subscription at a time, so there is nothing to collide with.
fn random_subscription_id() -> u32 {
    loop {
        let mut b = [0u8; 4];
        getrandom::getrandom(&mut b).expect("os rng");
        let v = u32::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

/// One classified unsecured datagram's destination flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsecuredFlow {
    Pase,
    Case,
    /// Not a flow this runtime starts (foreign protocol id, or a
    /// SecureChannel opcode we don't originate a session from — e.g. a
    /// stray `StatusReport`/standalone-ack with no matching exchange).
    Ignore,
}

/// Pure classifier — kept separate from the loop so it's unit-testable
/// without a socket (brief: "単体テストの datagram classifier if it's
/// nontrivial" — the opcode ranges below aren't obvious from the wire
/// consts alone, so it earns its own test).
fn classify_unsecured(protocol_id: u16, opcode: u8) -> UnsecuredFlow {
    if protocol_id != PROTOCOL_ID_SECURE_CHANNEL {
        return UnsecuredFlow::Ignore;
    }
    match opcode {
        OPCODE_PBKDF_PARAM_REQUEST
        | OPCODE_PBKDF_PARAM_RESPONSE
        | OPCODE_PASE_PAKE1
        | OPCODE_PASE_PAKE2
        | OPCODE_PASE_PAKE3 => UnsecuredFlow::Pase,
        OPCODE_CASE_SIGMA1 => UnsecuredFlow::Case,
        _ => UnsecuredFlow::Ignore,
    }
}

/// The commissioning window's admission decision (Task 14) for one already-
/// classified unsecured flow: `None` means "drop it — no response, no
/// session start", `Some` means "let `run`'s existing match handle it
/// unchanged". Kept as a pure function separate from the `select!` loop for
/// the same reason `classify_unsecured` is: unit-testable without a socket.
///
/// - `UnsecuredFlow::Pase` is gated on `window_open` — a closed window
///   (spec §5.4.2.3's 15-minute PASE upper bound elapsed, or
///   `CommissioningComplete` already closed it, or the device booted with a
///   fabric already installed) must refuse *every* PASE opcode with total
///   silence, not a `StatusReport` (申し送り 7 項: this runtime's DoS-
///   hardening posture treats a closed window exactly like every other
///   "nothing to route this to" drop elsewhere in this module — see the
///   module doc's wire-classification list).
/// - `UnsecuredFlow::Case` is never gated: CASE is how an already-
///   commissioned controller reconnects on every subsequent boot, spec's
///   commissioning window only bounds *PASE* (§5.4.2.3), and closing CASE
///   too would brick every fabric already on the device.
/// - `UnsecuredFlow::Ignore` passes through unchanged — it was never a flow
///   this runtime starts in the first place (see its variant doc), so the
///   window has nothing to say about it.
fn admit_unsecured(flow: UnsecuredFlow, window_open: bool) -> Option<UnsecuredFlow> {
    match flow {
        UnsecuredFlow::Pase if !window_open => None,
        other => Some(other),
    }
}

/// The *boot* commissioning window's upper bound (spec §5.4.2.3: the PASE
/// window a commissioner may use to complete commissioning must not exceed
/// 15 minutes) — the duration a freshly booted, never-commissioned device's
/// window runs for. A window opened later by the Administrator
/// Commissioning cluster's `OpenCommissioningWindow` (spec §11.19.8.1, Task
/// 4's `CommissioningWindow::EnhancedOpen`) uses its own `CommissioningTimeout`
/// instead (`apply_window_request`), not this constant.
const COMMISSIONING_WINDOW_DURATION: Duration = Duration::from_secs(15 * 60);

/// Whether new PASE attempts are currently admitted (Task 14), and — once
/// opened by the Administrator Commissioning cluster (Task 4) — what
/// verifier material/discriminator that admission should use instead of the
/// boot passcode. Boot-time policy (decided once in `run`, before the loop
/// starts): `Open` if `comm_server.fabrics()` is empty (a never-
/// commissioned, or freshly wiped, device), `Closed` if any fabric is
/// already installed (this device was commissioned in a previous run; a
/// fresh commissioner has no business PASE-ing into it again *until* an
/// already-commissioned controller — over an established CASE session —
/// explicitly reopens ECM access via `OpenCommissioningWindow`).
///
/// From that starting point:
/// - `Open -> EnhancedOpen`: a successful `OpenCommissioningWindow` stages a
///   `WindowRequest` (`core::commissioning`) the runtime picks up per
///   dispatch iteration (`serve_secured_message`) and turns into
///   `EnhancedOpen` via `apply_window_request`.
/// - `{Open,EnhancedOpen} -> Closed`: the 15-minute (boot) or
///   `CommissioningTimeout` (ECM) deadline lapsing
///   (`commissioning_window_deadline`), `CommissioningComplete` succeeding,
///   or `RevokeCommissioning` clearing the core's admin window out from
///   under an `EnhancedOpen` runtime window (detected the same
///   per-iteration place `EnhancedOpen` is entered) — all three also send
///   the mDNS commissionable goodbye (`set_commissionable(None)`).
#[derive(Debug, Clone)]
enum CommissioningWindow {
    /// The boot-time window: PASE against the QR/manual-pairing passcode
    /// and boot discriminator.
    Open {
        until: Instant,
    },
    /// A window opened at runtime by `OpenCommissioningWindow`: PASE against
    /// the commissioner-supplied verifier material and discriminator
    /// (`request`) instead of the boot passcode (`pase_config_for_window`),
    /// advertised with `CM=2` instead of `CM=1` (`advert_params_for_window`).
    EnhancedOpen {
        until: Instant,
        request: WindowRequest,
    },
    Closed,
}

impl CommissioningWindow {
    /// Whether `admit_unsecured` should currently let a `Pase` flow
    /// through. Doesn't re-check `until` against `Instant::now()` — the
    /// `select!` loop's `commissioning_window_deadline` branch is what
    /// transitions `{Open,EnhancedOpen} -> Closed` exactly at that instant
    /// (same single-threaded-loop invariant `fail_safe_expiry_deadline`'s
    /// doc comment relies on), so by construction this is never read past
    /// its own deadline while still reporting open.
    fn is_open(&self) -> bool {
        !matches!(self, CommissioningWindow::Closed)
    }
}

/// Same shape as `mdns_retry_deadline`/`fail_safe_expiry_deadline`: resolves
/// once at the open window's deadline (boot or ECM — both variants carry
/// `until`), or never (`std::future::pending`) once it's `Closed`. This
/// `select!` branch is the mechanism that actually enforces spec §5.4.2.3's
/// 15-minute PASE upper bound (boot) / the requested `CommissioningTimeout`
/// (ECM) — nothing else polls `until` against the clock.
async fn commissioning_window_deadline(window: &CommissioningWindow) {
    match window {
        CommissioningWindow::Open { until } | CommissioningWindow::EnhancedOpen { until, .. } => {
            tokio::time::sleep_until(*until).await
        }
        CommissioningWindow::Closed => std::future::pending().await,
    }
}

/// Stages a `WindowRequest` (a successful `OpenCommissioningWindow`, staged
/// by `core::commissioning` and collected by the runtime per dispatch
/// iteration) into an ECM window state — spec §11.19.8.1's
/// `CommissioningTimeout` becomes the window's deadline directly, the same
/// way the boot window uses `COMMISSIONING_WINDOW_DURATION`.
fn apply_window_request(request: WindowRequest) -> CommissioningWindow {
    let until = Instant::now() + Duration::from_secs(u64::from(request.timeout_s));
    CommissioningWindow::EnhancedOpen { until, request }
}

/// The PASE verifier configuration the current window should be served
/// with: the boot passcode for `Open`/`Closed` (a closed window never
/// actually reaches PASE — `admit_unsecured` already refused it — so this
/// arm is only reached, in practice, for the still-open boot window), or the
/// commissioner-supplied verifier material for `EnhancedOpen`.
/// `responder_session_id` is per-attempt (`random_session_id`) and always
/// passed through regardless of which window is active.
fn pase_config_for_window(
    window: &CommissioningWindow,
    boot_passcode: u32,
    boot_salt: &[u8],
    responder_session_id: u16,
) -> PaseVerifierConfig {
    match window {
        CommissioningWindow::EnhancedOpen { request, .. } => PaseVerifierConfig {
            secret: PaseSecret::VerifierMaterial(request.verifier),
            salt: request.salt.clone(),
            iterations: request.iterations,
            responder_session_id,
        },
        CommissioningWindow::Open { .. } | CommissioningWindow::Closed => PaseVerifierConfig {
            secret: PaseSecret::Passcode(boot_passcode),
            salt: boot_salt.to_vec(),
            iterations: PASE_ITERATIONS,
            responder_session_id,
        },
    }
}

/// The commissionable mDNS advert's `(discriminator, CM)` for the current
/// window — `None` for `Closed` (no advert to publish at all). `Open` keeps
/// the boot discriminator and `CM=1`; `EnhancedOpen` switches to the
/// `WindowRequest`'s discriminator and `CM=2` (spec §5.1.4.2/§4.3.1: a
/// commissioner distinguishes an ECM window from the boot window by `CM`,
/// and the discriminator an ECM window advertises is the one the
/// `OpenCommissioningWindow` caller chose, not necessarily the boot one).
fn advert_params_for_window(
    window: &CommissioningWindow,
    boot_discriminator: u16,
) -> Option<(u16, u8)> {
    match window {
        CommissioningWindow::Open { .. } => Some((boot_discriminator, 1)),
        CommissioningWindow::EnhancedOpen { request, .. } => Some((request.discriminator, 2)),
        CommissioningWindow::Closed => None,
    }
}

/// What `serve_secured_message`'s per-dispatch-iteration ECM reconciliation
/// should do, decided as a pure function so the ordering invariant it
/// encodes (review fix, Task 4 follow-up) can be unit-tested without a
/// socket harness — the `serve_secured_message` block that calls this only
/// performs the side effects (`**window` assignment, mDNS
/// publish/goodbye), it makes no decisions of its own.
#[derive(Debug)]
enum AdminWindowAction {
    /// Nothing to do this iteration — either nothing was staged and the
    /// window (if any) already agrees with `admin_open`, or a stale staged
    /// request was silently dropped and the window was already `Closed` (so
    /// there's nothing left to reconcile).
    None,
    /// Apply this staged `WindowRequest`: `Open`/`Closed -> EnhancedOpen`.
    Apply(WindowRequest),
    /// The admin window closed (timeout, `CommissioningComplete`, or
    /// `RevokeCommissioning`) while the runtime's own window is still
    /// `EnhancedOpen` — bring the two back in sync.
    Close,
}

/// Decides `AdminWindowAction` from the three inputs
/// `serve_secured_message` reads every dispatch iteration:
/// `comm_server.take_pending_window_request()` (`staged`),
/// `comm_server.admin_window_is_open()` (`admin_open`), and the runtime's
/// own `window`.
///
/// Order matters (Task 3 review carry-over, restated here since this is now
/// the one place the invariant lives): the caller must call
/// `take_pending_window_request()` *before* `admin_window_is_open()`, so a
/// `RevokeCommissioning` arriving in the same dispatch as an earlier-staged
/// `OpenCommissioningWindow` — both invoked back-to-back on one timed
/// exchange — is already visible in `admin_open` by the time this function
/// runs, regardless of which of the two ran first within that exchange.
/// Given that ordering, this function is a pure decision table:
/// - `staged: Some(_)`, `admin_open: true` → `Apply` (the common case: a
///   fresh `OpenCommissioningWindow` that wasn't immediately revoked).
/// - `staged: Some(_)`, `admin_open: false` → the request is stale (a
///   same-dispatch Revoke beat it) and must **not** be applied; falls
///   through to the same `Close`-or-`None` decision as `staged: None` below,
///   since dropping the stale request doesn't by itself tell us whether the
///   *window* (which may have been `EnhancedOpen` from an earlier dispatch)
///   still needs closing.
/// - `staged: None`, `window` is `EnhancedOpen`, `admin_open: false` →
///   `Close` (a Revoke — this dispatch or an earlier one — closed the admin
///   window out from under an already-open ECM window).
/// - Anything else (steady state: nothing staged and `admin_open` already
///   agrees with whether `window` is `EnhancedOpen`) → `None`.
fn admin_window_action(
    staged: Option<WindowRequest>,
    admin_open: bool,
    window: &CommissioningWindow,
) -> AdminWindowAction {
    if let Some(request) = staged {
        if admin_open {
            return AdminWindowAction::Apply(request);
        }
        // else: stale — drop it, fall through to the Close-or-None check.
    }
    if !admin_open && matches!(window, CommissioningWindow::EnhancedOpen { .. }) {
        AdminWindowAction::Close
    } else {
        AdminWindowAction::None
    }
}

/// What `serve_secured_message` should do once it's finished dispatching
/// and replying to one request — `Continue` (the overwhelming common case)
/// or `DropSession` (Task 6: a `RemoveFabric` this dispatch removed the
/// invoking session's own fabric — spec §2.5.11, removing a fabric SHALL
/// terminate any session associated with it). Propagated back up through
/// `drain_buffered_requests`/`serve_secured` to `run`'s loop, which is the
/// only place that actually owns `current_session` and can set it to
/// `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeOutcome {
    Continue,
    DropSession,
}

/// Whether a `RemoveFabric` that just removed `removed_fabric_index` from
/// the store should end the current secured session — true iff the
/// removed fabric is the one `session_fabric_index` (this session's own,
/// carried alongside `SecureSession` in `run`'s `current_session`)
/// authenticated against. Extracted as a pure function (mirrors
/// `admin_window_action` above) so this one-line decision has a unit test
/// that doesn't need a socket harness — `serve_secured_message`'s
/// `RemoveFabric` block below only calls it and acts on the result.
///
/// A PASE session (`session_fabric_index == 0`) can never match: `0` isn't
/// a valid fabric index (spec §2.5.1, fabric indices are 1-based), and
/// `RemoveFabric` itself is only reachable post-`AddNOC` in practice, but
/// nothing here assumes that — a `0 == 0` false-positive would be wrong
/// (there is no "fabric 0" to remove), so this only fires for handled
/// FabricIndex bytes, which `decode_remove_fabric` already restricts to
/// values `FabricStore` actually assigned (1+).
fn remove_fabric_drops_session(removed_fabric_index: u8, session_fabric_index: u8) -> bool {
    removed_fabric_index == session_fabric_index && session_fabric_index != 0
}

/// A random non-zero u16 — local (responder) session id for a fresh PASE or
/// CASE attempt. Not collision-checked against a previous session: this
/// runtime keeps at most one "current session" alive at a time (see module
/// doc), so the only way a collision could matter is astronomically
/// unlikely (1/65535) and even then just means the old session's next
/// datagram gets fed into the new one's `SecureSession` — a screen-level
/// session-id match with a peer address that no longer matches would drop
/// it harmlessly (`screen_with`'s `from != self.peer` check).
fn random_session_id() -> u16 {
    loop {
        let mut b = [0u8; 2];
        getrandom::getrandom(&mut b).expect("os rng");
        let v = u16::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

/// Random 64-bit hex instance/hostname (spec §4.3.1: the commissionable
/// service's instance name SHOULD be random). Reused as both the mDNS
/// instance name and the hostname (`<name>.local`) — legal, and simplest
/// for M1 (a real hostname-vs-instance split buys nothing here since both
/// ultimately resolve to the same one address this device advertises).
fn random_hex_name() -> String {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b).expect("os rng");
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

/// Builds the `OperationalAdvert` for one installed fabric entry.
fn operational_advert(
    entry: &FabricEntry,
    hostname: &str,
    port: u16,
    addr_v6: Ipv6Addr,
) -> OperationalAdvert {
    OperationalAdvert {
        compressed_fabric_id: compressed_fabric_id(&entry.root_public_key, entry.fabric_id),
        node_id: entry.node_id,
        hostname: hostname.to_string(),
        port,
        addr_v6,
    }
}

/// The mDNS advertiser plus everything needed to build adverts for it
/// (hostname/port/address are fixed for the process lifetime once
/// resolved). Kept together, behind one `Option`, because bringing up mDNS
/// is best-effort — see `run`'s doc comment on why a device that can't
/// join the multicast group must still serve PASE/CASE/IM traffic to a
/// peer that already knows its address.
struct MdnsCtx {
    mdns: Arc<MdnsAdvertiser>,
    hostname: String,
    port: u16,
    addr_v6: Ipv6Addr,
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
async fn bring_up_mdns(
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
    if let Some((discriminator, cm)) = advert_params_for_window(window, config.discriminator) {
        mdns.set_commissionable(Some(CommissionableAdvert {
            instance: random_hex_name(),
            hostname: hostname.clone(),
            discriminator,
            vendor_id: config.vendor_id,
            product_id: config.product_id,
            port,
            addr_v6,
            cm,
        }))
        .await;
    }
    for entry in comm_server.fabrics() {
        mdns.add_operational(operational_advert(&entry, &hostname, port, addr_v6))
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
    mdns.announce().await;

    Ok(MdnsCtx {
        mdns,
        hostname,
        port,
        addr_v6,
    })
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
struct MdnsRetry {
    first_failure_at: Instant,
    next_attempt_at: Instant,
}

/// Every 5s while mDNS has been down for less than a minute...
const MDNS_RETRY_INTERVAL_INITIAL: Duration = Duration::from_secs(5);
/// ...then every 60s after that.
const MDNS_RETRY_INTERVAL_LONG: Duration = Duration::from_secs(60);
const MDNS_RETRY_BACKOFF_THRESHOLD: Duration = Duration::from_secs(60);

impl MdnsRetry {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            first_failure_at: now,
            next_attempt_at: now + MDNS_RETRY_INTERVAL_INITIAL,
        }
    }

    /// Schedules the next attempt after another failure.
    fn schedule_next(&mut self) {
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
async fn mdns_retry_deadline(retry: &Option<MdnsRetry>) {
    match retry {
        Some(r) => tokio::time::sleep_until(r.next_attempt_at).await,
        None => std::future::pending().await,
    }
}

/// Same shape as `mdns_retry_deadline`, for the fail-safe window's expiry
/// (spec §11.10.7.2): resolves once at `comm_server.fail_safe_deadline()`,
/// or never (`std::future::pending`) when no window is currently open. This
/// `select!` branch *is* the mechanism that bounds how long an uncommitted
/// `AddNOC` fabric — and its operational mDNS advert — can stay visible
/// after the fail-safe lapses without a following `CommissioningComplete`;
/// no other code path (e.g. a lazy check on the next incoming command) also
/// tears it down, so a device that never receives another datagram after
/// the deadline still gets its goodbye sent, because this branch fires on
/// its own regardless.
///
/// `CommissioningServer::fail_safe_deadline` returns a `std::time::Instant`
/// (`core` stays free of any async-runtime dependency); `tokio::time::
/// sleep_until` needs `tokio::time::Instant`, hence the `from_std`
/// conversion — both wrap the same monotonic clock, so this is a lossless
/// reinterpretation, not a resampling of "now".
async fn fail_safe_expiry_deadline(comm_server: &CommissioningServer) {
    match comm_server.fail_safe_deadline() {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

/// Runs the device: binds nothing itself (the caller already bound
/// `transport`/`local_addr` — `Device::new` does that synchronously, see
/// its doc comment for why); brings up mDNS best-effort (see
/// `bring_up_mdns`'s doc comment — a failure there is logged and retried in
/// the background per `MdnsRetry`, never fatal); then serves datagrams
/// forever — this only returns early if a caller-supplied future it's
/// raced against elsewhere completes first (it never returns on its own;
/// see `Device::run`'s doc comment for the exact contract).
pub(crate) async fn run(
    transport: Arc<Transport>,
    local_addr: SocketAddr,
    config: DeviceConfig,
    mut node: Node,
    comm_server: CommissioningServer,
    mut group: GroupRx,
) -> Result<(), DeviceError> {
    let port = local_addr.port();
    // A fresh random PASE salt each boot (spec §3.9 permits any salt; a
    // fixed one is weak against a precomputed rainbow table across every
    // device running this firmware). Generated once here and reused for
    // every PASE attempt this run serves — it only needs to be consistent
    // within one handshake (it round-trips to the peer in
    // PBKDFParamResponse), not secret or per-attempt.
    let mut pase_salt = [0u8; 16];
    getrandom::getrandom(&mut pase_salt).expect("os rng");
    // Commissioning window boot-time policy (Task 14, `CommissioningWindow`'s
    // doc comment): open only for a device with no fabric yet — one already
    // on disk means this device was commissioned in an earlier run, so a
    // fresh PASE attempt now has no business succeeding. Decided *before*
    // the first `bring_up_mdns` call (fix round 1, review item 1) — that
    // call needs `window.is_open()` to know whether to publish a
    // commissionable advert at all.
    let mut window = if comm_server.fabrics().is_empty() {
        CommissioningWindow::Open {
            until: Instant::now() + COMMISSIONING_WINDOW_DURATION,
        }
    } else {
        CommissioningWindow::Closed
    };
    let mut mdns_ctx: Option<MdnsCtx> = None;
    let mut mdns_retry: Option<MdnsRetry> = None;
    match bring_up_mdns(&config, port, &comm_server, &window).await {
        Ok(ctx) => mdns_ctx = Some(ctx),
        Err(e) => {
            tracing::warn!(
                error = %e,
                iface = %config.iface,
                "mDNS advertiser did not come up — device still serves PASE/CASE/IM to peers that already have its address; retrying in the background"
            );
            mdns_retry = Some(MdnsRetry::new());
        }
    }

    // Third tuple element: the session's fabric index (spec §7.9) — `0` for
    // PASE (no fabric yet), the CASE-selected fabric otherwise. Carried
    // through to every `ReadRequest` this session serves via `ReadCtx`
    // (`serve_secured`/`serve_secured_message`), so e.g. Operational
    // Credentials' `CurrentFabricIndex` reflects the reading session, not a
    // hardcoded value.
    let mut current_session: Option<(u16, SecureSession, u8)> = None;
    // The node's single active subscription (spec §8.10, Task 12). Tied to
    // the session that created it: a new PASE/CASE session below drops it,
    // since its reports could only ever go out over the session it was
    // subscribed on.
    let mut subscription: Option<ActiveSubscription> = None;
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut replay = GroupReplayGuard::new();
    let mut gbuf = [0u8; MAX_DATAGRAM];
    loop {
        sync_group_joins(&mut group, &comm_server);
        tokio::select! {
            recv = transport.recv_from(&mut buf) => {
                let (n, peer) = match recv {
                    Ok(v) => v,
                    Err(_) => continue, // best-effort responder — a transient recv error isn't fatal
                };
                let Ok((header, off)) = MessageHeader::decode(&buf[..n]) else {
                    tracing::debug!(peer = %peer, len = n, "datagram dropped: header decode failed");
                    continue;
                };
                if header.security_flags & SESSION_TYPE_MASK != 0 {
                    tracing::debug!(peer = %peer, security_flags = header.security_flags, "group-session datagram on the unicast socket dropped (the group socket serves those)");
                    continue;
                }
                if header.session_id == 0 && header.security_flags == 0 {
                    let Ok((proto, body_off)) = ProtocolHeader::decode(&buf[off..n]) else {
                        tracing::debug!(peer = %peer, "unsecured datagram dropped: protocol header decode failed");
                        continue;
                    };
                    if !proto.initiator {
                        tracing::debug!(
                            peer = %peer,
                            exchange_id = proto.exchange_id,
                            opcode = format_args!("0x{:02X}", proto.opcode),
                            "unsecured datagram dropped: not an initiator message"
                        );
                        continue;
                    }
                    if proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                        && proto.opcode == OPCODE_MRP_STANDALONE_ACK
                    {
                        tracing::debug!(
                            peer = %peer,
                            exchange_id = proto.exchange_id,
                            "unsecured datagram dropped: standalone MRP ack (no session to route it to)"
                        );
                        continue;
                    }
                    let first = mat_controller::exchange::IncomingMessage {
                        header,
                        proto,
                        payload: buf[off + body_off..n].to_vec(),
                    };
                    let flow = classify_unsecured(proto.protocol_id, proto.opcode);
                    tracing::debug!(
                        opcode = format_args!("0x{:02X}", proto.opcode),
                        protocol_id = format_args!("0x{:04X}", proto.protocol_id),
                        exchange_id = proto.exchange_id,
                        peer = %peer,
                        peer_node_id = ?header.source_node_id,
                        ?flow,
                        "unsecured datagram received"
                    );
                    let Some(flow) = admit_unsecured(flow, window.is_open()) else {
                        tracing::debug!(
                            peer = %peer,
                            exchange_id = proto.exchange_id,
                            "PASE datagram dropped: commissioning window closed"
                        );
                        continue;
                    };
                    match flow {
                        UnsecuredFlow::Pase => {
                            let local_session_id = random_session_id();
                            let outcome = crate::net::pase::drive_established(
                                &transport,
                                peer,
                                first,
                                pase_config_for_window(
                                    &window,
                                    config.passcode,
                                    &pase_salt,
                                    local_session_id,
                                ),
                            )
                            .await;
                            match outcome {
                                Ok((keys, peer_session_id)) => {
                                    tracing::debug!(
                                        local_session_id,
                                        peer_session_id,
                                        peer = %peer,
                                        "PASE established"
                                    );
                                    let session = SecureSession::new_device_role(
                                        Arc::clone(&transport),
                                        peer,
                                        local_session_id,
                                        peer_session_id,
                                        keys,
                                        0, // PASE: both sides are node id 0 (spec §4.13)
                                        0,
                                    );
                                    current_session = Some((local_session_id, session, 0)); // PASE: no fabric yet
                                    subscription = None; // belonged to the replaced session
                                }
                                // Established failure: best-effort responder, nothing
                                // more to do — the initiator's own retry/StatusReport
                                // handling covers it (logged so a failing
                                // interop run says *where* it stopped).
                                Err(e) => tracing::debug!(error = %e, peer = %peer, "PASE failed"),
                            }
                        }
                        UnsecuredFlow::Case => {
                            let local_session_id = random_session_id();
                            let fabrics = comm_server.fabrics();
                            let outcome = crate::net::case::drive_established(
                                Arc::clone(&transport),
                                peer,
                                first,
                                fabrics,
                                local_session_id,
                            )
                            .await;
                            match outcome {
                                Ok((session, fabric_index)) => {
                                    tracing::debug!(
                                        local_session_id,
                                        fabric_index,
                                        peer = %peer,
                                        "CASE established"
                                    );
                                    current_session =
                                        Some((local_session_id, session, fabric_index));
                                    subscription = None; // belonged to the replaced session
                                }
                                Err(e) => tracing::debug!(error = %e, peer = %peer, "CASE failed"),
                            }
                        }
                        UnsecuredFlow::Ignore => {}
                    }
                    continue;
                }

                // Secured traffic: only ever the current session (sequential,
                // one-at-a-time — see module doc).
                let Some((sid, session, fabric_index)) = current_session.as_mut() else {
                    tracing::debug!(
                        session_id = header.session_id,
                        peer = %peer,
                        "secured datagram dropped: no session established"
                    );
                    continue;
                };
                if header.session_id != *sid {
                    tracing::debug!(
                        session_id = header.session_id,
                        current_session_id = *sid,
                        peer = %peer,
                        "secured datagram dropped: session id does not match the current session"
                    );
                    continue;
                }
                let outcome = serve_secured(
                    &buf[..n],
                    peer,
                    session,
                    *fabric_index,
                    &mut ServeState {
                        node: &mut node,
                        comm_server: &comm_server,
                        mdns: mdns_ctx.as_ref(),
                        subscription: &mut subscription,
                        window: &mut window,
                        config: &config,
                    },
                )
                .await;
                // Task 6: a `RemoveFabric` that removed this session's own
                // fabric — `session`/`fabric_index` (borrowed out of
                // `current_session` above) are no longer used past this
                // point in this iteration, so this is the first place the
                // borrow checker lets `current_session` be reassigned.
                if outcome == ServeOutcome::DropSession {
                    current_session = None;
                }
            }
            () = mdns_retry_deadline(&mdns_retry) => {
                // `window.is_open()` read *now*, not whatever it was when
                // this retry was scheduled (fix round 1, review item 1):
                // the window may have closed (15-minute expiry or
                // `CommissioningComplete`) while mDNS was still down, and a
                // stale "was open when scheduled" read would let this retry
                // revive a commissionable advert for a window that already
                // sent its goodbye.
                match bring_up_mdns(&config, port, &comm_server, &window).await {
                    Ok(ctx) => {
                        tracing::info!("mDNS advertiser came up on retry");
                        mdns_ctx = Some(ctx);
                        mdns_retry = None;
                    }
                    Err(e) => {
                        // Warned once already (either at startup, above, or
                        // on the very first retry — from here on this is
                        // expected/repetitive noise for a device on a
                        // genuinely bad interface, hence debug not warn.
                        tracing::debug!(error = %e, "mDNS retry attempt failed, will retry again");
                        if let Some(state) = mdns_retry.as_mut() {
                            state.schedule_next();
                        }
                    }
                }
            }
            () = subscription_deadline(&subscription) => {
                // The active subscription is due for a report: the dirty
                // attributes' current values, or an empty keep-alive. Both
                // go out on a fresh device-initiated exchange and both must
                // be acknowledged — anything else drops the subscription
                // (`send_subscription_report`'s doc comment).
                //
                // No reentrancy hazard against the datagram branch above:
                // `select!` runs exactly one branch to completion per
                // iteration, so while this one awaits its StatusResponse it
                // is `SecureSession`'s own socket read — not the loop's —
                // that consumes datagrams. Requests arriving meanwhile are
                // buffered by `screen_with` and served by the drain below,
                // exactly like the ones landing during a reply's ack-wait.
                let delivered = match (subscription.as_mut(), current_session.as_mut()) {
                    (Some(sub), Some((_, session, fabric_index))) => {
                        send_subscription_report(session, *fabric_index, &mut node, sub).await
                    }
                    // A subscription that outlived its session has nothing
                    // to report over — drop it.
                    _ => false,
                };
                if !delivered {
                    tracing::debug!(
                        subscription_id = subscription.as_ref().map(|s| s.id),
                        "subscription dropped: report was not acknowledged"
                    );
                    subscription = None;
                }
                let mut drop_session = false;
                if let Some((_, session, fabric_index)) = current_session.as_mut() {
                    drop_session = drain_buffered_requests(
                        session,
                        *fabric_index,
                        &mut ServeState {
                            node: &mut node,
                            comm_server: &comm_server,
                            mdns: mdns_ctx.as_ref(),
                            subscription: &mut subscription,
                            window: &mut window,
                            config: &config,
                        },
                    )
                    .await
                        == ServeOutcome::DropSession;
                }
                // Task 6: same reasoning as the datagram branch above — a
                // buffered `RemoveFabric` piggybacked on this session's own
                // fabric ends the session too, not just one arriving as a
                // fresh datagram.
                if drop_session {
                    current_session = None;
                }
            }
            () = commissioning_window_deadline(&window) => {
                // Spec §5.4.2.3's 15-minute PASE window upper bound (boot
                // window) or spec §11.19.8.1's `CommissioningTimeout` (ECM
                // window, Task 4) has elapsed: close the window (no more
                // PASE admitted — see `admit_unsecured`) and send the same
                // commissionable-advert goodbye `CommissioningComplete`
                // sends on success (`set_commissionable(None)`, goodbye
                // wired since Task 8). No fabric rollback here — unlike
                // fail-safe expiry, an already-*established* PASE session
                // (if one happened to be mid-flight right at the deadline)
                // is left alone; this branch only stops *new* PASE attempts
                // from being admitted going forward.
                tracing::info!("commissioning window expired — closing");
                window = CommissioningWindow::Closed;
                if let Some(ctx) = mdns_ctx.as_ref() {
                    ctx.mdns.set_commissionable(None).await;
                }
                // Task 4: this timer-driven close is the runtime noticing
                // on its own — unlike `CommissioningComplete`/`Revoke`
                // (dispatched IM commands the core cluster handler already
                // reacts to), nothing on the core side knows the deadline
                // just passed, so the runtime must tell it explicitly to
                // keep the AC cluster's `WindowStatus` attribute honest. A
                // no-op if this was the boot window (never opened an admin
                // window in the first place).
                comm_server.close_admin_window();
            }
            () = fail_safe_expiry_deadline(&comm_server) => {
                // `expire_fail_safe` is the one primitive that both decides
                // "was there actually something to roll back" and does the
                // rollback — `Some(entry)` only for the fabric an
                // uncommitted `AddNOC` installed within this now-lapsed
                // window (see its doc comment). Nothing to do here beyond
                // that if it returns `None` (e.g. a plain `ArmFailSafe`
                // that never called `AddNOC`, or this branch racing another
                // caller that already consumed the same expiry) — the
                // `select!` loop simply comes back around, and
                // `fail_safe_expiry_deadline` reads as "no window open"
                // (`std::future::pending`) on the next iteration.
                let expired = comm_server.expire_fail_safe();
                tracing::debug!(
                    rolled_back_fabric_index = ?expired.as_ref().map(|e| e.fabric_index),
                    "fail-safe expiry deadline fired"
                );
                if let Some(entry) = expired {
                    tracing::info!(
                        fabric_id = entry.fabric_id,
                        node_id = entry.node_id,
                        "fail-safe expired without CommissioningComplete — rolling back fabric and its mDNS advert"
                    );
                    if let Some(ctx) = mdns_ctx.as_ref() {
                        let cfid = compressed_fabric_id(&entry.root_public_key, entry.fabric_id);
                        ctx.mdns
                            .remove_operational(u64::from_be_bytes(cfid), entry.node_id)
                            .await;
                    }
                }
            }
            grecv = group_recv(&group.socket, &mut gbuf) => {
                let Ok((n, from)) = grecv else { continue };
                let fabrics = comm_server.fabrics();
                let deps = GroupRxDeps { fabrics: &fabrics, gk_store: &group.gk_store, membership: &group.membership };
                match classify_group_datagram(&gbuf[..n], &deps, &mut replay) {
                    Ok(batch) => {
                        let mut ctx = InvokeCtx {
                            fabric_index: batch.fabric_index,
                            subject: Subject::group(batch.group_id),
                            ..InvokeCtx::default()
                        };
                        let changed = node.handle_group_invoke(&batch.endpoints, &batch.invokes, &mut ctx);
                        tracing::debug!(peer = %from, fabric_index = batch.fabric_index, group_id = batch.group_id, source_node_id = batch.source_node_id, endpoints = ?batch.endpoints, changed = changed.len(), "groupcast invoke applied");
                        if let Some(sub) = subscription.as_mut() {
                            sub.note_changed(&changed);
                        }
                    }
                    Err(reason) => tracing::debug!(peer = %from, len = n, ?reason, "groupcast datagram dropped"),
                }
            }
        }
    }
}

/// Multicast join/leave against `group.socket`, differenced from the last
/// call by `GroupSocket::sync_joins` itself — a no-op when membership hasn't
/// changed since the previous iteration. Run at the top of every `run` loop
/// iteration (before `select!`) so an `AddGroup`, `RemoveFabric`, or
/// fail-safe rollback that changed the set of joined groups is picked up on
/// the very next spin, without any dedicated event plumbing from those call
/// sites back into this loop. A `None` socket (bind failed at `Device::new`
/// time) makes this a no-op.
fn sync_group_joins(group: &mut GroupRx, comm_server: &CommissioningServer) {
    if let Some(sock) = group.socket.as_mut() {
        sock.sync_joins(&desired_group_addrs(
            &comm_server.fabrics(),
            &group.membership,
        ));
    }
}

/// Handles one datagram already known to be secured traffic for `session`:
/// decrypt/screen it, dispatch Interaction Model requests to `node`, reply,
/// and react to the two commissioning milestones the brief calls out —
/// AddNOC success (detected by `comm_server.fabrics()` growing across the
/// call, rather than decoding which command it was — robust to *any*
/// gated command eventually installing a fabric, not just this one) and
/// CommissioningComplete (detected by decoding the request we're about to
/// serve, cheap and side-effect-free, purely for this notification).
/// `mdns` is `None` when `bring_up_mdns` failed at startup (`run`'s doc
/// comment) — commissioning still completes, just without an operational
/// advert or a commissionable-window teardown to publish.
///
/// Returns `ServeOutcome::DropSession` (Task 6) if this datagram's dispatch
/// — or, when it wasn't, one of the buffered requests drained afterward —
/// contained a `RemoveFabric` that removed the invoking session's own
/// fabric (`remove_fabric_drops_session`); `run`'s caller then sets
/// `current_session = None`. `DropSession` skips the buffered-request drain
/// below entirely (rather than draining what's left first): the session is
/// about to be torn down, so there is no longer anywhere to route replies
/// for whatever else `screen_with` buffered — and per spec §2.5.11 removing
/// a fabric SHALL terminate sessions associated with it, so continuing to
/// serve *other* requests on it, even briefly, would be wrong regardless.
async fn serve_secured(
    buf: &[u8],
    from: SocketAddr,
    session: &mut SecureSession,
    fabric_index: u8,
    state: &mut ServeState<'_>,
) -> ServeOutcome {
    let msg = match session.deliver_request(buf, from).await {
        Ok(Some(msg)) => msg,
        Ok(None) => {
            tracing::debug!(
                peer = %from,
                "secured datagram dropped: deliver_request returned no message (standalone ack or screened-out)"
            );
            return ServeOutcome::Continue;
        }
        Err(e) => {
            tracing::debug!(error = %e, peer = %from, "secured datagram dropped: decrypt/screen failure");
            return ServeOutcome::Continue; // decrypt/screen failure — drop, don't kill the session on noise
        }
    };
    if serve_secured_message(msg, session, fabric_index, state).await == ServeOutcome::DropSession {
        return ServeOutcome::DropSession;
    }

    // While `reply_reliable` (inside `serve_secured_message`) was waiting
    // for the ack of *that* reply, a new peer-initiated request on a
    // *different* exchange may have arrived — real controllers/commissioners
    // commonly piggyback their ack on the very next request rather than
    // sending a standalone one. `screen_with` still acks and buffers it
    // (`peer_initiated`) even though it failed that wait's `PeerExchange`
    // filter, so it must be served here rather than silently lost (review
    // fix: "ack-then-drop of cross-exchange secured requests"). Draining in
    // a loop (not just once) covers the same thing happening again while
    // *this* reply's own ack-wait is in flight.
    drain_buffered_requests(session, fabric_index, state).await
}

/// Serves every peer-initiated request `screen_with` buffered
/// (`peer_initiated`) while this side was busy waiting for an ack — see
/// `serve_secured`'s comment above the original call site for why dropping
/// them would be a permanent loss. Also run after a device-initiated
/// subscription report, whose own ack-wait reads the socket the same way.
/// Stops (returning `ServeOutcome::DropSession`) as soon as any drained
/// request drops the session — same reasoning as `serve_secured`'s doc
/// comment: nothing buffered after that point should still be served.
async fn drain_buffered_requests(
    session: &mut SecureSession,
    fabric_index: u8,
    state: &mut ServeState<'_>,
) -> ServeOutcome {
    while let Some(buffered) = session.take_buffered_request() {
        if buffered.proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL {
            continue;
        }
        if serve_secured_message(buffered, session, fabric_index, state).await
            == ServeOutcome::DropSession
        {
            return ServeOutcome::DropSession;
        }
    }
    ServeOutcome::Continue
}

/// Dispatches one already-classified Interaction Model request (`msg`) to
/// `node`, replies, and reacts to the two commissioning milestones (see
/// `serve_secured`'s original doc comment for why: AddNOC success detected
/// by fabric count growth, CommissioningComplete by decoding the request).
/// Split out from `serve_secured` so both the datagram just read off the
/// socket and any buffered peer-initiated request drained afterward go
/// through the identical path. `fabric_index` is this session's fabric
/// index (0 for PASE) — threaded into `ReadCtx` for every `ReadRequest`.
///
/// Returns `ServeOutcome` (Task 6) — `DropSession` once, per dispatch
/// iteration, if a `RemoveFabric` handled this iteration removed the
/// invoking session's own fabric (see the `RemoveFabric` block near the
/// end of the loop body); every other exit path (non-IM traffic, Read/
/// Subscribe's own flows, a `handle_im` decode failure, or the ack-wait
/// loop simply running out of same-exchange follow-ups) is `Continue`.
async fn serve_secured_message(
    msg: mat_controller::exchange::IncomingMessage,
    session: &mut SecureSession,
    fabric_index: u8,
    state: &mut ServeState<'_>,
) -> ServeOutcome {
    // Destructured (rather than used through `state.*`) so the borrow
    // checker sees the four fields as independent borrows — the
    // subscription is updated while `node` is also borrowed.
    let ServeState {
        node,
        comm_server,
        mdns,
        subscription,
        window,
        config,
    } = state;
    let node: &mut Node = node;
    let comm_server: &CommissioningServer = comm_server;
    let mdns: Option<&MdnsCtx> = *mdns;
    let config: &DeviceConfig = config;

    // 同一 exchange の後続リクエストを処理し切るまで回るループ。Timed
    // Interaction（spec §8.9.4）では initiator が StatusResponse(SUCCESS) の
    // 受領後、**同じ exchange** で timed Invoke/Write を送ってくる（ack は
    // そこに piggyback）。`reply_reliable` はそれを `Ok(Some(msg))` として
    // 返すので、捨てずにここで続けて処理する（捨てると invoke は MRP ack
    // 済みのまま永遠に応答されず、Google Play Services スタックの
    // commissioning が中断する — 2026-08-18 実測）。
    let mut msg = msg;
    loop {
        if msg.proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL {
            // Secure-channel traffic on an established session (e.g. a
            // device-initiated-exchange StatusReport) — out of M1 scope.
            return ServeOutcome::Continue;
        }

        // ReadRequest gets its own chunk-aware flow (Task 6) instead of going
        // through `Node::handle_im` (whose `handle_read` always answers with a
        // single message) — see `serve_read_request_chunked`'s doc comment.
        // Reads never trigger the AddNOC/CommissioningComplete milestones
        // below (those are Invoke-only), so returning here is safe.
        if msg.proto.opcode == im::OPCODE_READ_REQUEST {
            serve_read_request_chunked(&msg, session, fabric_index, node).await;
            return ServeOutcome::Continue;
        }

        // SubscribeRequest likewise owns its whole interaction (priming chunks
        // + SubscribeResponse on this exchange, Task 12) rather than producing
        // one reply through `Node::handle_im`. Success installs the node's one
        // active subscription; failure anywhere in the flow leaves it with
        // none — including tearing down whatever was subscribed before, since
        // this same peer just asked to start over. An up-front `INVALID_ACTION`
        // refusal leaves the existing subscription alone only when the request
        // asked `KeepSubscriptions=true` (`SubscribeOutcome::Rejected`); with
        // `KeepSubscriptions=false` the old subscription is torn down first,
        // as chip does (`SubscribeOutcome::TornDown`).
        if msg.proto.opcode == im::OPCODE_SUBSCRIBE_REQUEST {
            match serve_subscribe_request(&msg, session, fabric_index, node).await {
                SubscribeOutcome::Installed(sub) => **subscription = Some(sub),
                SubscribeOutcome::TornDown => **subscription = None,
                SubscribeOutcome::Rejected => {}
            }
            return ServeOutcome::Continue;
        }

        tracing::debug!(
            im_opcode = format_args!("0x{:02X}", msg.proto.opcode),
            exchange_id = msg.proto.exchange_id,
            request = ?im::decode_invoke_request(&msg.payload)
                .ok()
                .map(|r| (r.endpoint, r.cluster, r.command)),
            "IM request"
        );

        // Only meaningful for CommissioningComplete detection below; a decode
        // failure here just means we won't recognize that milestone (the
        // dispatch to `node.handle_im` below still runs against the raw bytes
        // regardless, so a malformed request is still answered/rejected
        // normally).
        let req_cluster_command = im::decode_invoke_request(&msg.payload)
            .ok()
            .map(|r| (r.cluster, r.command));

        let mut ctx = InvokeCtx {
            attestation_challenge: session.attestation_challenge(),
            fabric_index,
            subject: session_subject(session),
            ..InvokeCtx::default()
        };
        // Invoke/write dispatch: no `IsFabricFiltered` on the wire for those
        // requests, so use the fabric-filtered side — the same default the
        // read/subscribe decoders apply when the flag is absent.
        let read_ctx = ReadCtx {
            fabric_index,
            fabric_filtered: true,
            subject: session_subject(session),
        };
        let fabrics_before = comm_server.fabrics().len();
        let Ok(outcome) = node.handle_im(msg.proto.opcode, &msg.payload, &mut ctx, &read_ctx)
        else {
            return ServeOutcome::Continue;
        };
        let ImOutcome {
            opcode: resp_opcode,
            payload: resp_payload,
            changed,
        } = outcome;
        // Anything this request changed that the active subscription covers
        // becomes dirty — the `select!` report branch (`run`) picks it up at
        // the subscription's next deadline. Recorded *before* the reply is
        // sent so a change is never lost to a failing reply.
        if let Some(sub) = subscription.as_mut() {
            sub.note_changed(&changed);
        }
        let reply_result = session
            .reply_reliable(
                &msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                resp_opcode,
                &resp_payload,
                &reply_cfg(),
            )
            .await;
        tracing::debug!(
            resp_opcode = format_args!("0x{:02X}", resp_opcode),
            exchange_id = msg.proto.exchange_id,
            payload_len = resp_payload.len(),
            ok = reply_result.is_ok(),
            error = reply_result.as_ref().err().map(|e| e.to_string()),
            "IM reply sent"
        );

        // AddNOC success: a fabric appeared that wasn't there before this call.
        let fabrics_after = comm_server.fabrics();
        if fabrics_after.len() > fabrics_before {
            if let (Some(entry), Some(ctx)) = (fabrics_after.last(), mdns) {
                ctx.mdns
                    .add_operational(operational_advert(
                        entry,
                        &ctx.hostname,
                        ctx.port,
                        ctx.addr_v6,
                    ))
                    .await;
            }
        }

        // ECM window reconciliation (Task 4), per dispatch iteration — same
        // spot the AddNOC fabric-diff check above lives, so a timed
        // `OpenCommissioningWindow`/`RevokeCommissioning` invoke (piggybacked
        // on this same exchange, per the timed-invoke loop this function
        // runs) is handled without waiting for the next datagram. The
        // ordering invariant this depends on (take the staged request
        // *before* reading `admin_open`) and the resulting decision table
        // are `admin_window_action`'s doc comment/unit tests, not repeated
        // here — this block only performs the side effects.
        let pending_request = comm_server.take_pending_window_request();
        let admin_open = comm_server.admin_window_is_open();
        match admin_window_action(pending_request, admin_open, window) {
            AdminWindowAction::Apply(request) => {
                **window = apply_window_request(request);
                if let (Some(ctx), Some((discriminator, cm))) =
                    (mdns, advert_params_for_window(window, config.discriminator))
                {
                    ctx.mdns
                        .set_commissionable(Some(CommissionableAdvert {
                            instance: random_hex_name(),
                            hostname: ctx.hostname.clone(),
                            discriminator,
                            vendor_id: config.vendor_id,
                            product_id: config.product_id,
                            port: ctx.port,
                            addr_v6: ctx.addr_v6,
                            cm,
                        }))
                        .await;
                }
            }
            AdminWindowAction::Close => {
                tracing::info!("administrator commissioning window revoked — closing");
                **window = CommissioningWindow::Closed;
                if let Some(ctx) = mdns {
                    ctx.mdns.set_commissionable(None).await;
                }
            }
            AdminWindowAction::None => {}
        }

        // CommissioningComplete success: stop advertising commissionable.
        if resp_opcode == im::OPCODE_INVOKE_RESPONSE {
            if let Some((cluster, command)) = req_cluster_command {
                if cluster == mat_controller::commissioning::CLUSTER_GENERAL_COMMISSIONING
                    && command == mat_controller::commissioning::CMD_COMMISSIONING_COMPLETE
                {
                    if let Ok(outcome) = im::decode_invoke_response(&resp_payload) {
                        if outcome.status == im::STATUS_SUCCESS {
                            if let Some(ctx) = mdns {
                                ctx.mdns.set_commissionable(None).await;
                            }
                            // Task 14: CommissioningComplete is the other event
                            // (besides the 15-minute/`CommissioningTimeout`
                            // deadline in `run`'s `select!`) that closes the
                            // commissioning window — a controller that just
                            // finished commissioning has no reason to PASE in
                            // again, and refusing it stops a second
                            // commissioner from racing in during whatever's
                            // left of the window.
                            **window = CommissioningWindow::Closed;
                            // Task 4: this close is runtime-initiated (General
                            // Commissioning's CommissioningComplete doesn't
                            // touch the AC cluster's admin_window itself), so
                            // tell core explicitly — keeps `WindowStatus`
                            // honest for an ECM window that just got
                            // committed by completion rather than expiry/
                            // revoke. A no-op for the boot window.
                            comm_server.close_admin_window();
                        }
                    }
                }
            }
        }

        // RemoveFabric (Task 6): the store may have shed a fabric this
        // dispatch — either the invoking session's own (the motivating
        // case: an Android phone removing its ephemeral fabric right after
        // handing the device off to Home Assistant via
        // `OpenCommissioningWindow`) or a different one named explicitly in
        // the command fields. Either way its mDNS operational advert must
        // go. Checked *after* `reply_reliable` above, per the brief: the
        // `RemoveFabric` response itself has already been sent (and, along
        // `reply_reliable`'s normal path, acked) by this point, so dropping
        // the session below never races the response that announces the
        // removal.
        if let Some(entry) = comm_server.take_removed_fabric() {
            if let Some(ctx) = mdns {
                let cfid = compressed_fabric_id(&entry.root_public_key, entry.fabric_id);
                ctx.mdns
                    .remove_operational(u64::from_be_bytes(cfid), entry.node_id)
                    .await;
            }
            if remove_fabric_drops_session(entry.fabric_index, fabric_index) {
                tracing::info!(
                    fabric_index,
                    node_id = entry.node_id,
                    "RemoveFabric removed the invoking session's own fabric — dropping session"
                );
                return ServeOutcome::DropSession;
            }
        }

        // `reply_reliable` が実メッセージ（同一 exchange の後続リクエスト —
        // 典型は Timed Interaction の timed Invoke）を返したら、この iteration
        // と同じ経路で続けて処理する。ack 完了 (`Ok(None)`) と送信失敗 (`Err`)
        // はどちらもこのメッセージ列の終端。
        match reply_result {
            Ok(Some(next)) => msg = next,
            _ => return ServeOutcome::Continue,
        }
    }
}

/// Everything one secured message is allowed to touch, bundled so
/// `serve_secured`/`serve_secured_message` keep a readable arity as the
/// runtime grows state (Task 12 added the subscription). Borrowed as a
/// whole from `run`'s loop locals; the fields are destructured inside
/// `serve_secured_message` so they stay independent borrows.
struct ServeState<'a> {
    node: &'a mut Node,
    comm_server: &'a CommissioningServer,
    /// `None` when `bring_up_mdns` hasn't succeeded (see `run`'s doc).
    mdns: Option<&'a MdnsCtx>,
    /// The node's single active subscription, if any (see
    /// `net::subscription`'s module doc for why there's only one).
    subscription: &'a mut Option<ActiveSubscription>,
    /// The commissioning window (Task 14, widened by Task 4). Mutated here
    /// by: a staged `WindowRequest` opening it (`Open -> EnhancedOpen`,
    /// `apply_window_request`); `CommissioningComplete` succeeding; and
    /// `RevokeCommissioning` having cleared the core's admin window out from
    /// under an `EnhancedOpen` runtime window — all three detected per
    /// dispatch iteration, same place as the AddNOC fabric-diff check. The
    /// 15-minute/`CommissioningTimeout`-elapsed close happens in `run`'s own
    /// `select!` branch, outside any `ServeState`.
    window: &'a mut CommissioningWindow,
    /// Boot-time identity (passcode/discriminator/vendor+product id) — Task
    /// 4 needs `config.discriminator`/`vendor_id`/`product_id` to rebuild
    /// the commissionable advert when a `WindowRequest` reopens the window,
    /// and `config.passcode` was already reachable via `run`'s closure but
    /// not through `ServeState` until now (`pase_config_for_window` is
    /// called directly in `run`, not from here — this is only for the
    /// mDNS-side advert rebuild).
    config: &'a DeviceConfig,
}

/// Resolves once at the active subscription's next report deadline, or
/// never (`std::future::pending`) when nothing is subscribed — same
/// `select!`-branch shape as `mdns_retry_deadline`/
/// `fail_safe_expiry_deadline`, so a subscription's timer can't block
/// datagram serving and its absence simply makes the branch inert.
async fn subscription_deadline(subscription: &Option<ActiveSubscription>) {
    match subscription {
        Some(sub) => tokio::time::sleep_until(sub.next_report_deadline()).await,
        None => std::future::pending().await,
    }
}

/// What `serve_subscribe_request` did to the node's single subscription slot.
enum SubscribeOutcome {
    /// A new subscription is live (replaces whatever was there).
    Installed(ActiveSubscription),
    /// The request was accepted but the interaction failed partway
    /// (undecodable request, priming send failure, missing ack) — the peer
    /// asked to start over and the flow broke, so nothing is subscribed.
    TornDown,
    /// The request was refused up front with `StatusResponse(INVALID_ACTION)`
    /// (spec §8.10: no readable path) *and* it asked `KeepSubscriptions=true`
    /// — a refusal, not a restart, so the existing subscription (if any) is
    /// left alone, as chip does. A refusal with `KeepSubscriptions=false`
    /// tears the existing subscription down instead (`TornDown`) — chip
    /// applies that teardown *before* path validation, so it happens
    /// regardless of why the request was ultimately refused.
    Rejected,
}

/// Serves one `SubscribeRequest` end to end (spec §8.10): priming
/// ReportData (chunked, each chunk acknowledged with `StatusResponse(0)`
/// by the initiator) followed by a `SubscribeResponse`, all on the
/// requesting exchange, and returns a `SubscribeOutcome`: `Installed` with
/// the resulting `ActiveSubscription` on success; `TornDown` if any step
/// failed mid-flow, in which case this device simply has no subscription
/// and the initiator is free to retry from scratch (same
/// abort-and-let-the-initiator-retry policy `serve_read_request_chunked`
/// uses for chunked reads); or, if the request was refused up front with
/// `StatusResponse(INVALID_ACTION)` before any of that flow started,
/// `Rejected` when the request asked `KeepSubscriptions=true` (leaves any
/// existing subscription on this node untouched) or `TornDown` when it
/// asked `KeepSubscriptions=false` — chip tears down the subscriber's
/// existing subscriptions *before* path validation, so a refused request
/// with `KeepSubscriptions=false` still discards them.
///
/// Two ways this differs from a chunked read (both are why priming needs
/// its own flow rather than reusing `serve_read_request_chunked`): every
/// chunk carries the SubscriptionId and none of them suppress the
/// response — not even the last, because the interaction isn't over until
/// the `SubscribeResponse` goes out on the same exchange.
///
/// The subscription is registered by the caller only *after* all of that
/// completes, so a half-finished subscribe never leaves a live
/// subscription behind reporting to a controller that never got its
/// SubscribeResponse.
async fn serve_subscribe_request(
    msg: &mat_controller::exchange::IncomingMessage,
    session: &mut SecureSession,
    fabric_index: u8,
    node: &mut Node,
) -> SubscribeOutcome {
    let Ok(req) = im::decode_subscribe_request(&msg.payload) else {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            "SubscribeRequest dropped: undecodable"
        );
        return SubscribeOutcome::TornDown;
    };
    let subscription_id = random_subscription_id();
    // The device picks the MaxInterval it can actually honor, at or below
    // the requested ceiling (spec §8.10) — `SubscribeResponse` tells the
    // subscriber what it settled on.
    let max_interval_s = req
        .max_interval_ceiling_s
        .clamp(MIN_MAX_INTERVAL_S, MAX_MAX_INTERVAL_S);
    let max_interval = Duration::from_secs(u64::from(max_interval_s));
    // A floor above the interval we just settled on would starve the
    // subscription of its own keep-alives; clamp it down rather than
    // reject the (otherwise legal) request.
    let min_interval = Duration::from_secs(u64::from(req.min_interval_floor_s)).min(max_interval);

    let read_ctx = ReadCtx {
        fabric_index,
        fabric_filtered: req.fabric_filtered,
        subject: session_subject(session),
    };

    // spec §8.10 / chip `ParseAttributePaths`: a request none of whose
    // paths can yield anything this subject may read is refused outright
    // rather than answered with an empty priming report and a dead
    // subscription. (Concrete paths always count — their refusal shows up
    // as a status entry in the priming report; see
    // `Node::has_readable_path`.)
    if !node.has_readable_path(&req.paths, &read_ctx) {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            paths = ?req.paths,
            subject = ?read_ctx.subject,
            fabric_index,
            "SubscribeRequest rejected: no readable attribute path (INVALID_ACTION)"
        );
        let reply_result = session
            .reply_reliable(
                msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                im::OPCODE_STATUS_RESPONSE,
                &im::encode_status_response(im::STATUS_INVALID_ACTION),
                &reply_cfg(),
            )
            .await;
        match &reply_result {
            Err(e) => {
                tracing::debug!(exchange_id = msg.proto.exchange_id, error = %e, "INVALID_ACTION StatusResponse not delivered");
            }
            Ok(Some(piggybacked)) => {
                // A peer message piggybacked on the ack — not consumed here
                // (this refusal has nothing more to do with it), but logged
                // so it's visibly dropped rather than silently ack-then-lost.
                tracing::debug!(
                    exchange_id = piggybacked.proto.exchange_id,
                    opcode = format_args!("0x{:02X}", piggybacked.proto.opcode),
                    "INVALID_ACTION StatusResponse ack carried a piggybacked peer message, discarded"
                );
            }
            Ok(None) => {}
        }
        // chip tears down the subscriber's existing subscriptions *before*
        // path validation, so a refusal only leaves them alone when the
        // request asked KeepSubscriptions=true; KeepSubscriptions=false
        // discards them here too, as chip does.
        return if req.keep_subscriptions {
            SubscribeOutcome::Rejected
        } else {
            SubscribeOutcome::TornDown
        };
    }

    let chunks = node.read_chunks(
        &req.paths,
        &read_ctx,
        REPORT_CHUNK_BUDGET,
        Some(subscription_id),
    );
    tracing::debug!(
        exchange_id = msg.proto.exchange_id,
        subscription_id,
        paths = ?req.paths,
        fabric_filtered = req.fabric_filtered,
        min_interval_floor_s = req.min_interval_floor_s,
        max_interval_ceiling_s = req.max_interval_ceiling_s,
        max_interval_s,
        chunks = chunks.len(),
        "SubscribeRequest"
    );

    for (i, chunk) in chunks.iter().enumerate() {
        let reply_result = session
            .reply_reliable(
                msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                im::OPCODE_REPORT_DATA,
                chunk,
                &reply_cfg(),
            )
            .await;
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            subscription_id,
            chunk_index = i,
            payload_len = chunk.len(),
            ok = reply_result.is_ok(),
            error = reply_result.as_ref().err().map(|e| e.to_string()),
            "priming ReportData chunk sent"
        );
        let Ok(piggybacked) = reply_result else {
            return SubscribeOutcome::TornDown; // ack never came — exchange is dead, give up
        };
        if !await_peer_status_ok(session, piggybacked, msg.proto.exchange_id).await {
            return SubscribeOutcome::TornDown;
        }
    }

    let resp = im::encode_subscribe_response(subscription_id, max_interval_s);
    let reply_result = session
        .reply_reliable(
            msg,
            PROTOCOL_ID_INTERACTION_MODEL,
            im::OPCODE_SUBSCRIBE_RESPONSE,
            &resp,
            &reply_cfg(),
        )
        .await;
    tracing::debug!(
        exchange_id = msg.proto.exchange_id,
        subscription_id,
        ok = reply_result.is_ok(),
        error = reply_result.as_ref().err().map(|e| e.to_string()),
        "SubscribeResponse sent"
    );
    if reply_result.is_err() {
        return SubscribeOutcome::TornDown;
    }

    SubscribeOutcome::Installed(ActiveSubscription {
        id: subscription_id,
        paths: req.paths,
        fabric_filtered: req.fabric_filtered,
        min_interval,
        max_interval,
        // The priming report counts as this subscription's first report:
        // the keep-alive clock starts at the end of the subscribe
        // interaction, not before it.
        last_report_at: Instant::now(),
        dirty: Vec::new(),
    })
}

/// Sends one subscription ReportData on a **new**, device-initiated
/// exchange (spec §8.10.3) and waits for the subscriber's
/// `StatusResponse(0)`. Carries the dirty attributes' current values, or
/// no reports at all when nothing changed — an empty ReportData is the
/// keep-alive that tells the subscriber the subscription is still alive
/// (`SecureSession::next_subscription_report` delivers it as such).
///
/// Returns `false` if the report couldn't be delivered or the subscriber
/// answered anything other than SUCCESS; the caller then drops the
/// subscription, which is the only sane response — MRP already retried the
/// send, so a failure here means the peer is gone or has forgotten this
/// subscription, and a real controller re-subscribes on its own.
///
/// Takes `&mut Node` even though it only reads: this future is held across
/// `await` points inside `run`'s `select!`, and `Device::run`'s task is
/// `tokio::spawn`ed — a shared `&Node` living across an await would make
/// the whole runtime future require `Node: Sync`, which `Box<dyn
/// ClusterHandler>` (declared `: Send`, not `: Sync`) is not.
async fn send_subscription_report(
    session: &mut SecureSession,
    fabric_index: u8,
    node: &mut Node,
    sub: &mut ActiveSubscription,
) -> bool {
    let paths: Vec<mat_controller::im::AttrPathIn> = sub
        .dirty
        .iter()
        .map(
            |(endpoint, cluster, attribute)| mat_controller::im::AttrPathIn {
                endpoint: Some(*endpoint),
                cluster: Some(*cluster),
                attribute: Some(*attribute),
            },
        )
        .collect();
    // Same `IsFabricFiltered` the subscribe request asked for: every report
    // on a subscription is a continuation of that one read request, so a
    // dirty/keep-alive report must not widen what the priming report showed.
    let read_ctx = ReadCtx {
        fabric_index,
        fabric_filtered: sub.fabric_filtered,
        subject: session_subject(session),
    };
    // Values are read *now*, not captured when the change happened: the
    // report carries the attribute's current value (spec §8.10.2), so two
    // changes between reports collapse into one entry with the latest
    // value — which is also why `dirty` holds paths, not values.
    // `retain_reportable` drops the status entries a wildcard subscription
    // would otherwise get for attributes it may not read (see its doc).
    let entries = if paths.is_empty() {
        Vec::new()
    } else {
        crate::net::subscription::retain_reportable(sub, node.read_entries(&paths, &read_ctx))
    };
    // `more_chunks=false`, one message: a dirty set is a handful of
    // scalar attributes, orders of magnitude below `REPORT_CHUNK_BUDGET`
    // (unlike priming, which can pull in whole certificate attributes).
    let payload = im::encode_report_data_entries(&entries, false, Some(sub.id), false);
    if payload.len() > REPORT_CHUNK_BUDGET {
        // Not a hard failure (MRP/`seal` will just fail to send it, and the
        // subscription gets dropped below) — but a silent oversized report
        // is exactly the failure mode a future non-scalar subscribed
        // attribute would hit, so say so loudly enough to find in a log.
        tracing::debug!(
            subscription_id = sub.id,
            payload_len = payload.len(),
            budget = REPORT_CHUNK_BUDGET,
            reports = entries.len(),
            "subscription report exceeds the chunk budget — dirty reports are not chunked (see send_subscription_report)"
        );
    }
    let exchange_id = SecureSession::new_exchange_id();
    let send_result = session
        .send_reliable(
            exchange_id,
            PROTOCOL_ID_INTERACTION_MODEL,
            im::OPCODE_REPORT_DATA,
            &payload,
            &reply_cfg(),
        )
        .await;
    tracing::debug!(
        exchange_id,
        subscription_id = sub.id,
        reports = entries.len(),
        keep_alive = entries.is_empty(),
        payload_len = payload.len(),
        ok = send_result.is_ok(),
        error = send_result.as_ref().err().map(|e| e.to_string()),
        "subscription ReportData sent"
    );
    let Ok(piggybacked) = send_result else {
        return false;
    };
    // Our own exchange this time (we're the initiator), so the plain
    // `recv` filter is the right one — unlike priming, which answers on
    // the *peer's* exchange (see `await_peer_status_ok`).
    let status_msg = match piggybacked {
        Some(m) => m,
        None => match session.recv(exchange_id, REPORT_STATUS_TIMEOUT).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(exchange_id, error = %e, "subscription report: no StatusResponse");
                return false;
            }
        },
    };
    if !is_status_response_ok(&status_msg) {
        return false;
    }

    sub.last_report_at = Instant::now();
    sub.dirty.clear();
    true
}

/// How many mis-addressed peer-initiated messages `await_peer_status_ok`
/// will set aside while waiting for its StatusResponse. Matches
/// `SecureSession`'s own `peer_initiated` capacity: a peer that sends more
/// unrelated requests than that inside one chunk's status wait is flooding,
/// and the wait gives up rather than growing without bound.
const MAX_DEFERRED_REQUESTS: usize = 32;

/// Waits for the peer's `StatusResponse(0)` to a ReportData we just sent on
/// an exchange **the peer initiated** (a priming chunk, or a non-final read
/// chunk). `piggybacked` is whatever `reply_reliable` already had in hand —
/// real controllers (`SecureSession::subscribe_wildcard`, chip-tool) answer
/// with the StatusResponse itself rather than a standalone ack, so that's
/// the normal path; the fallback pulls messages with `recv_request`, whose
/// `AnyPeerInitiated` filter is the only one that can deliver on an
/// exchange we didn't initiate (plain `recv` requires `!initiator` and so
/// would sit here until it timed out).
///
/// **Nothing pulled here is ever discarded.** A message on another exchange
/// is a request the peer expects an answer to, and `screen_with` has
/// already MRP-acked it — dropping it would make the peer wait forever with
/// no retransmit to save it (the "ack-then-drop of cross-exchange secured
/// requests" class of bug). Such messages are set aside and handed back to
/// `SecureSession`'s buffer in their original order on the way out
/// (`requeue_buffered_request`), where `serve_secured`'s drain picks them
/// up once the chunked interaction finishes.
///
/// ## Why this loop terminates
///
/// Every iteration does exactly one of: return on a match, return on the
/// deadline, or move one message from `SecureSession`'s buffer/socket into
/// the local `deferred` vec — which is *not* fed back until the loop is
/// over, so a set-aside message can never be re-pulled within this call.
/// That leaves two bounded sources of iterations: the buffer, which is
/// finite (`MAX_PEER_INITIATED_BUFFER`) and strictly shrinks as it is
/// drained, and the socket, whose reads are bounded by `REPORT_STATUS_
/// TIMEOUT` (`recv_request` gets the *remaining* time, so buffered
/// messages — which return instantly — can't extend the total wait).
/// `MAX_DEFERRED_REQUESTS` is a third, redundant bound covering a peer that
/// floods new requests faster than they can be set aside.
async fn await_peer_status_ok(
    session: &mut SecureSession,
    piggybacked: Option<mat_controller::exchange::IncomingMessage>,
    exchange_id: u16,
) -> bool {
    let deadline = Instant::now() + REPORT_STATUS_TIMEOUT;
    let mut candidate = piggybacked;
    let mut deferred: Vec<mat_controller::exchange::IncomingMessage> = Vec::new();
    let mut ok = false;

    loop {
        let msg = match candidate.take() {
            Some(m) => m,
            None => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    tracing::debug!(
                        exchange_id,
                        "chunk ack: timed out waiting for StatusResponse"
                    );
                    break;
                }
                match session.recv_request(remaining).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!(exchange_id, error = %e, "chunk ack: no StatusResponse");
                        break;
                    }
                }
            }
        };
        if msg.proto.exchange_id == exchange_id {
            ok = is_status_response_ok(&msg);
            break;
        }
        // Someone else's exchange: not ours to answer here, not ours to
        // throw away either.
        tracing::debug!(
            exchange_id,
            other_exchange_id = msg.proto.exchange_id,
            opcode = format_args!("0x{:02X}", msg.proto.opcode),
            "chunk ack: setting aside a request on another exchange"
        );
        deferred.push(msg);
        if deferred.len() >= MAX_DEFERRED_REQUESTS {
            tracing::debug!(
                exchange_id,
                "chunk ack: too many unrelated requests while waiting; giving up"
            );
            break;
        }
    }

    // Back to the front of the buffer, oldest first (push each to the
    // front in reverse, so the head ends up being the one pulled first).
    for msg in deferred.into_iter().rev() {
        session.requeue_buffered_request(msg);
    }
    ok
}

/// Whether `msg` is a `StatusResponse(SUCCESS)` — the acknowledgement
/// every non-suppressed ReportData expects (spec §8.9.2.3). Anything else
/// (wrong opcode, non-zero status, malformed payload) means the peer isn't
/// following the interaction and the caller gives up on it.
fn is_status_response_ok(msg: &mat_controller::exchange::IncomingMessage) -> bool {
    if msg.proto.opcode != im::OPCODE_STATUS_RESPONSE {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            opcode = format_args!("0x{:02X}", msg.proto.opcode),
            "expected StatusResponse, got another opcode"
        );
        return false;
    }
    match im::decode_status_response(&msg.payload) {
        Ok(0) => true,
        other => {
            tracing::debug!(
                exchange_id = msg.proto.exchange_id,
                status = ?other,
                "StatusResponse was not SUCCESS"
            );
            false
        }
    }
}

/// Serves one `ReadRequest` with `Node::read_chunks`'s chunked reply flow
/// (Task 6), bypassing `Node::handle_im`/`handle_read` (which only ever
/// return a single message) entirely. Mirrors `SecureSession::
/// subscribe_wildcard`'s priming-report chunk loop on the *initiator* side
/// (`mat_controller::session::subscribe::subscribe_wildcard`): every chunk
/// but the last is sent `more_chunks=true, suppress_response=false` and
/// this side then waits for the peer's `StatusResponse(0)` on the same
/// exchange before sending the next one; the last chunk is sent
/// `more_chunks=false, suppress_response=true` and nothing further is
/// awaited — exactly what `handle_read`'s old single-message reply always
/// sent, so a read whose data fits in one chunk (`Node::read_chunks`
/// returns exactly one chunk) behaves identically to before Task 6.
///
/// `reply_reliable`'s `Some(msg)`/`None` return mirrors `send_reliable`'s:
/// if the peer piggybacks its real `StatusResponse` on the MRP ack instead
/// of sending a standalone one, `reply_reliable` already has it in hand;
/// otherwise a separate `session.recv` on the same exchange (bounded to 5s
/// — this is a LAN round-trip to a controller/hub, not a WAN call) waits
/// for it. Any failure along the way — the reply itself failing to send,
/// a wrong opcode, a non-zero status, a malformed StatusResponse, or a
/// timeout — aborts the remaining chunks rather than retrying or looping:
/// the exchange is effectively dead at that point, and the initiator sees
/// an incomplete read it can retry from scratch.
async fn serve_read_request_chunked(
    msg: &mat_controller::exchange::IncomingMessage,
    session: &mut SecureSession,
    fabric_index: u8,
    node: &mut Node,
) {
    let Ok(req) = im::decode_read_request_message(&msg.payload) else {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            "ReadRequest dropped: undecodable"
        );
        return;
    };
    let paths = req.paths;
    let read_ctx = ReadCtx {
        fabric_index,
        fabric_filtered: req.fabric_filtered,
        subject: session_subject(session),
    };
    let chunks = node.read_chunks(&paths, &read_ctx, REPORT_CHUNK_BUDGET, None);
    let last_index = chunks.len().saturating_sub(1);
    tracing::debug!(
        exchange_id = msg.proto.exchange_id,
        ?paths,
        fabric_filtered = req.fabric_filtered,
        chunks = chunks.len(),
        "ReadRequest"
    );

    for (i, chunk) in chunks.into_iter().enumerate() {
        let is_last = i == last_index;
        let reply_result = session
            .reply_reliable(
                msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                im::OPCODE_REPORT_DATA,
                &chunk,
                &reply_cfg(),
            )
            .await;
        tracing::debug!(
            resp_opcode = format_args!("0x{:02X}", im::OPCODE_REPORT_DATA),
            exchange_id = msg.proto.exchange_id,
            payload_len = chunk.len(),
            chunk_index = i,
            is_last,
            ok = reply_result.is_ok(),
            error = reply_result.as_ref().err().map(|e| e.to_string()),
            "IM reply sent (ReportData chunk)"
        );
        let Ok(piggybacked) = reply_result else {
            return; // ack never came — exchange is dead, give up
        };

        if is_last {
            return; // final chunk: no StatusResponse expected, done
        }

        // Same wait the subscription's priming loop does — this is the
        // peer's exchange, so `await_peer_status_ok`'s `recv_request`
        // fallback is what can actually deliver a StatusResponse that
        // didn't come piggybacked on the ack (plain `session.recv` filters
        // out messages on exchanges we didn't initiate and would sit here
        // until it timed out).
        if !await_peer_status_ok(session, piggybacked, msg.proto.exchange_id).await {
            return;
        }
    }
}

/// The ACL identity of a device-role CASE session: node id + CATs as read
/// off the peer's NOC by `net::case` (`SecureSession::peer_cats`). On a
/// PASE session both are their placeholders (node 0, no CATs), which the
/// fabric-0 bypass in `datamodel::acl_allows` never consults.
fn session_subject(session: &SecureSession) -> Subject {
    Subject::new(session.peer_node_id(), session.peer_cats())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `DeviceConfig` fixture for tests that need a `ServeState`
    /// (`config` is only read when a `WindowRequest` reopens the window
    /// with `mdns: Some(..)`, neither of which any `serve_secured`-driving
    /// test below exercises — `store_dir`/`iface` are never touched by
    /// those paths, so their placeholder values are never resolved).
    fn test_config() -> DeviceConfig {
        DeviceConfig {
            passcode: 20202021,
            discriminator: 3840,
            vendor_id: 0xFFF1,
            product_id: 0x8000,
            port: 5540,
            store_dir: std::path::PathBuf::new(),
            iface: String::new(),
            attestation: Default::default(),
            group_port: 0,
            devices: vec![],
        }
    }

    #[test]
    fn classifies_pase_opcodes() {
        for op in [
            OPCODE_PBKDF_PARAM_REQUEST,
            OPCODE_PBKDF_PARAM_RESPONSE,
            OPCODE_PASE_PAKE1,
            OPCODE_PASE_PAKE2,
            OPCODE_PASE_PAKE3,
        ] {
            assert_eq!(
                classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, op),
                UnsecuredFlow::Pase,
                "opcode 0x{op:02X} should classify as Pase"
            );
        }
    }

    #[test]
    fn classifies_case_sigma1() {
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_CASE_SIGMA1),
            UnsecuredFlow::Case
        );
    }

    #[test]
    fn ignores_foreign_protocol_id() {
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_INTERACTION_MODEL, OPCODE_PBKDF_PARAM_REQUEST),
            UnsecuredFlow::Ignore
        );
    }

    #[test]
    fn ignores_unknown_secure_channel_opcode() {
        // e.g. OPCODE_STATUS_REPORT (0x40) or OPCODE_MRP_STANDALONE_ACK
        // (0x10) reaching the classifier as a "first" datagram — the main
        // loop actually filters standalone acks before classifying, but
        // the classifier itself should still be inert on them (defense in
        // depth / doesn't assume its caller's pre-filtering).
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, 0x40),
            UnsecuredFlow::Ignore
        );
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_MRP_STANDALONE_ACK),
            UnsecuredFlow::Ignore
        );
    }

    #[test]
    fn random_session_id_is_never_zero() {
        for _ in 0..1000 {
            assert_ne!(random_session_id(), 0);
        }
    }

    // ── commissioning window admission (Task 14) ────────────────────────

    #[test]
    fn admit_unsecured_drops_pase_when_window_closed() {
        assert_eq!(admit_unsecured(UnsecuredFlow::Pase, false), None);
    }

    #[test]
    fn admit_unsecured_allows_pase_when_window_open() {
        assert_eq!(
            admit_unsecured(UnsecuredFlow::Pase, true),
            Some(UnsecuredFlow::Pase)
        );
    }

    #[test]
    fn admit_unsecured_allows_case_regardless_of_window() {
        assert_eq!(
            admit_unsecured(UnsecuredFlow::Case, true),
            Some(UnsecuredFlow::Case)
        );
        assert_eq!(
            admit_unsecured(UnsecuredFlow::Case, false),
            Some(UnsecuredFlow::Case)
        );
    }

    #[test]
    fn admit_unsecured_allows_ignore_regardless_of_window() {
        assert_eq!(
            admit_unsecured(UnsecuredFlow::Ignore, true),
            Some(UnsecuredFlow::Ignore)
        );
        assert_eq!(
            admit_unsecured(UnsecuredFlow::Ignore, false),
            Some(UnsecuredFlow::Ignore)
        );
    }

    // ── mDNS retry backoff (review fix round 1, item 1) ────────────────

    #[tokio::test(start_paused = true)]
    async fn mdns_retry_schedules_the_initial_interval_first() {
        let retry = MdnsRetry::new();
        assert_eq!(
            retry.next_attempt_at - retry.first_failure_at,
            MDNS_RETRY_INTERVAL_INITIAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mdns_retry_keeps_the_short_interval_before_the_threshold() {
        let mut retry = MdnsRetry::new();
        // Advance to just before the backoff threshold and fail again —
        // still short-interval territory.
        tokio::time::advance(MDNS_RETRY_BACKOFF_THRESHOLD - Duration::from_secs(1)).await;
        retry.schedule_next();
        assert_eq!(
            retry.next_attempt_at - Instant::now(),
            MDNS_RETRY_INTERVAL_INITIAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mdns_retry_switches_to_the_long_interval_past_the_threshold() {
        let mut retry = MdnsRetry::new();
        tokio::time::advance(MDNS_RETRY_BACKOFF_THRESHOLD + Duration::from_secs(1)).await;
        retry.schedule_next();
        assert_eq!(
            retry.next_attempt_at - Instant::now(),
            MDNS_RETRY_INTERVAL_LONG
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mdns_retry_deadline_never_resolves_when_no_retry_pending() {
        let none: Option<MdnsRetry> = None;
        // If this ever resolved, the test would hang until its own harness
        // timeout — so this is really "doesn't hang" plus a bounded race
        // against a short sleep to make that assertion concrete.
        tokio::select! {
            () = mdns_retry_deadline(&none) => panic!("deadline resolved with no retry pending"),
            () = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }

    // ── commissioning window deadline (Task 14) ─────────────────────────

    #[tokio::test(start_paused = true)]
    async fn commissioning_window_deadline_never_resolves_when_closed() {
        // Same "doesn't hang" technique as the mDNS retry/fail-safe tests
        // above: if this ever resolved, the closed-window branch would
        // spuriously fire and start dropping PASE that should have been
        // fine.
        tokio::select! {
            () = commissioning_window_deadline(&CommissioningWindow::Closed) => {
                panic!("deadline resolved for a closed window")
            }
            () = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn commissioning_window_deadline_resolves_once_the_open_window_lapses() {
        let window = CommissioningWindow::Open {
            until: Instant::now() + COMMISSIONING_WINDOW_DURATION,
        };
        tokio::select! {
            () = commissioning_window_deadline(&window) => {}
            () = tokio::time::sleep(COMMISSIONING_WINDOW_DURATION + Duration::from_secs(1)) => {
                panic!("commissioning_window_deadline never resolved for an open window")
            }
        }
    }

    #[test]
    fn commissioning_window_is_open_reports_correctly() {
        assert!(CommissioningWindow::Open {
            until: Instant::now() + COMMISSIONING_WINDOW_DURATION
        }
        .is_open());
        assert!(CommissioningWindow::EnhancedOpen {
            until: Instant::now() + Duration::from_secs(60),
            request: WindowRequest {
                verifier: [0x11; 97],
                discriminator: 100,
                iterations: 1000,
                salt: vec![1, 2, 3],
                timeout_s: 60,
            },
        }
        .is_open());
        assert!(!CommissioningWindow::Closed.is_open());
    }

    // ── ECM window (Task 4) ──────────────────────────────────────────────

    /// dispatch 後に WindowRequest が stage されていれば、runtime の窓が
    /// EnhancedOpen になり ECM 用 PASE 設定が得られる（純粋ロジック部分の
    /// 単体テスト — apply_window_request を関数に切り出してテストする）。
    #[test]
    fn apply_window_request_transitions_to_enhanced_open() {
        let req = WindowRequest {
            verifier: [0x42; 97],
            discriminator: 0x0ABC,
            iterations: 1000,
            salt: vec![0x5A; 16],
            timeout_s: 300,
        };
        let window = apply_window_request(req.clone());
        // Destructured from a clone, not `window` itself, so `window` stays
        // usable below (`CommissioningWindow` can't be `Copy` — it carries
        // `WindowRequest`'s `Vec<u8>` salt).
        let CommissioningWindow::EnhancedOpen { until, request } = window.clone() else {
            panic!("expected EnhancedOpen");
        };
        assert_eq!(request.discriminator, 0x0ABC);
        assert!(until > Instant::now());
        // ECM 中の PASE 設定が verifier 素材になること
        let config = pase_config_for_window(
            &window, /*boot passcode*/ 20202021, /*boot salt*/ &[0u8; 32], 0x1234,
        );
        assert!(matches!(config.secret, PaseSecret::VerifierMaterial(m) if m == [0x42; 97]));
        assert_eq!(config.iterations, 1000);
        assert_eq!(config.salt, vec![0x5A; 16]);
    }

    /// 窓 variant ごとの mDNS 広告パラメータ（CM 値と discriminator）。
    #[test]
    fn commissionable_advert_params_reflect_window_kind() {
        let boot = CommissioningWindow::Open {
            until: Instant::now() + Duration::from_secs(60),
        };
        assert_eq!(advert_params_for_window(&boot, 3210), Some((3210, 1)));
        let req = WindowRequest {
            verifier: [0x42; 97],
            discriminator: 0x0ABC,
            iterations: 1000,
            salt: vec![0x5A; 16],
            timeout_s: 300,
        };
        let ecm = CommissioningWindow::EnhancedOpen {
            until: Instant::now() + Duration::from_secs(300),
            request: req,
        };
        assert_eq!(advert_params_for_window(&ecm, 3210), Some((0x0ABC, 2)));
        assert_eq!(
            advert_params_for_window(&CommissioningWindow::Closed, 3210),
            None
        );
    }

    fn test_window_request() -> WindowRequest {
        WindowRequest {
            verifier: [0x11; 97],
            discriminator: 100,
            iterations: 1000,
            salt: vec![1, 2, 3],
            timeout_s: 60,
        }
    }

    /// staged Some + admin_open true → Apply(request) — the common case: a
    /// fresh `OpenCommissioningWindow` that wasn't immediately revoked.
    #[test]
    fn admin_window_action_applies_staged_request_when_admin_open() {
        let req = test_window_request();
        let window = CommissioningWindow::Open {
            until: Instant::now() + COMMISSIONING_WINDOW_DURATION,
        };
        match admin_window_action(Some(req.clone()), true, &window) {
            AdminWindowAction::Apply(applied) => {
                assert_eq!(applied.discriminator, req.discriminator);
                assert_eq!(applied.timeout_s, req.timeout_s);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    /// staged Some + admin_open false (a same-dispatch Revoke raced ahead of
    /// the Open) → the stale request must not be applied. With `window`
    /// already `Closed` (never `EnhancedOpen`), there is also nothing left
    /// to reconcile — `None`, not `Close`.
    #[test]
    fn admin_window_action_drops_stale_request_without_closing_an_already_closed_window() {
        let req = test_window_request();
        let action = admin_window_action(Some(req), false, &CommissioningWindow::Closed);
        assert!(
            matches!(action, AdminWindowAction::None),
            "expected None, got {action:?}"
        );
    }

    /// staged None + admin_open false + window `EnhancedOpen` → `Close`: a
    /// `RevokeCommissioning` (this dispatch or an earlier one) closed the
    /// admin window out from under an already-open ECM window.
    #[test]
    fn admin_window_action_closes_an_enhanced_open_window_once_admin_window_is_revoked() {
        let window = CommissioningWindow::EnhancedOpen {
            until: Instant::now() + Duration::from_secs(60),
            request: test_window_request(),
        };
        let action = admin_window_action(None, false, &window);
        assert!(
            matches!(action, AdminWindowAction::Close),
            "expected Close, got {action:?}"
        );
    }

    /// staged None + admin_open true + window `EnhancedOpen` → `None`
    /// (steady state: an ECM window that's still open and nothing new
    /// staged this iteration — no side effect should fire every single
    /// dispatch while a commissioner is just using the window it opened).
    #[test]
    fn admin_window_action_is_steady_state_none_for_a_still_open_enhanced_window() {
        let window = CommissioningWindow::EnhancedOpen {
            until: Instant::now() + Duration::from_secs(60),
            request: test_window_request(),
        };
        let action = admin_window_action(None, true, &window);
        assert!(
            matches!(action, AdminWindowAction::None),
            "expected None, got {action:?}"
        );
    }

    // ── RemoveFabric session-drop decision (Task 6) ─────────────────────
    //
    // `remove_fabric_drops_session` is the one-line decision
    // `serve_secured_message`'s RemoveFabric block acts on; a socket-
    // harness test proving the whole PASE/CASE→AddNOC→RemoveFabric→session-
    // torn-down flow end to end would be disproportionate here (it would
    // mostly re-exercise `net::case`/`net::pase`, already covered
    // elsewhere) — this unit-tests the decision itself, same rationale as
    // the `admin_window_action` tests above.

    #[test]
    fn remove_fabric_drops_session_when_removed_index_matches_session() {
        assert!(remove_fabric_drops_session(1, 1));
    }

    #[test]
    fn remove_fabric_does_not_drop_session_when_removed_index_differs() {
        assert!(!remove_fabric_drops_session(2, 1));
    }

    /// A PASE session's `fabric_index` is `0` (no fabric yet) — `0` can
    /// never be a real `FabricIndex` (spec §2.5.1, 1-based), so it must
    /// never match even a (hypothetically malformed) `removed_fabric_index
    /// == 0`.
    #[test]
    fn remove_fabric_never_drops_a_pase_session() {
        assert!(!remove_fabric_drops_session(0, 0));
    }

    // ── fail-safe expiry deadline (Task 8) ──────────────────────────────
    //
    // Brief-scoped: only the `fail_safe_deadline()` Some/None → resolves/
    // never-resolves mapping. Whether `expire_fail_safe()` then actually
    // rolls back the right fabric is `core::commissioning`'s own test
    // territory (Task 7); whether the runtime's `select!` branch drives the
    // real mDNS goodbye end-to-end is Task 9's real-hardware gate.

    fn fail_safe_test_server() -> CommissioningServer {
        let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        CommissioningServer::new(dev, crate::core::fabric_store::FabricStore::new())
    }

    #[tokio::test(start_paused = true)]
    async fn fail_safe_expiry_deadline_never_resolves_when_not_armed() {
        let comm_server = fail_safe_test_server();
        assert!(comm_server.fail_safe_deadline().is_none());
        // Same "doesn't hang" technique as
        // `mdns_retry_deadline_never_resolves_when_no_retry_pending`.
        tokio::select! {
            () = fail_safe_expiry_deadline(&comm_server) => {
                panic!("deadline resolved with no fail-safe armed")
            }
            () = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }

    // Not `start_paused`, unlike the mDNS retry tests above: those poll a
    // `tokio::time::Instant` deadline, which a paused runtime's virtual
    // clock auto-advances through freely. `fail_safe_deadline()` returns a
    // `std::time::Instant` (`core::commissioning` stays runtime-agnostic on
    // purpose) — real wall-clock time, which a paused *tokio* clock does
    // not advance. `ArmFailSafe`'s `ExpiryLengthSeconds` also bottoms out
    // at whole seconds, so this test just eats one real second rather than
    // fighting the two clocks.
    #[tokio::test]
    async fn fail_safe_expiry_deadline_resolves_once_the_armed_window_passes() {
        use mat_controller::commissioning::{encode_arm_fail_safe, CMD_ARM_FAIL_SAFE};

        let comm_server = fail_safe_test_server();
        let (mut gc, _oc, _ac) = comm_server.into_cluster_handlers();
        let mut ctx = InvokeCtx::default();
        gc.invoke(CMD_ARM_FAIL_SAFE, &encode_arm_fail_safe(1, 1), &mut ctx);
        assert!(
            comm_server.fail_safe_deadline().is_some(),
            "ArmFailSafe should have opened a window"
        );

        // Bounded by a much longer sleep so a regression (never resolving)
        // fails the test instead of hanging — mirrors the mDNS retry tests'
        // technique of racing an unambiguous outcome.
        tokio::select! {
            () = fail_safe_expiry_deadline(&comm_server) => {}
            () = tokio::time::sleep(Duration::from_secs(30)) => {
                panic!("fail_safe_expiry_deadline never resolved for an armed window")
            }
        }
        // The window has now lapsed — `fail_safe_deadline` reads back
        // `None` (`FailSafeState::deadline`'s doc: `None` once passed).
        assert!(comm_server.fail_safe_deadline().is_none());
    }

    // ── review fix: cross-exchange piggyback ack must not lose a request ──

    /// Runtime-level companion to `mat_controller::session`'s
    /// `reply_reliable_completes_via_cross_exchange_piggyback_ack`: proves
    /// `serve_secured`'s drain loop doesn't just retain a request buffered
    /// while the first reply's `reply_reliable` was waiting on its
    /// (piggybacked, cross-exchange) ack — it actually dispatches it through
    /// `Node::handle_im` and replies, exactly like a datagram read fresh off
    /// the socket. No mDNS, no commissioning — a bare `Node`/
    /// `CommissioningServer` pair driven directly, `serve_secured` called by
    /// hand the same way `run`'s loop calls it.
    #[tokio::test]
    async fn serve_secured_drains_and_serves_a_cross_exchange_piggybacked_request() {
        use mat_controller::crypto::{open_message, seal_message};
        use mat_controller::message::Destination;
        use mat_controller::session::SessionKeys;
        use mat_controller::transport::UdpTransport;

        use crate::core::fabric_store::FabricStore;

        const LOCAL_SID: u16 = 0xAAAA; // device's own session id
        const PEER_SID: u16 = 0xBBBB; // controller's session id
        const CTRL_NODE: u64 = 1;
        const DEV_NODE: u64 = 2;
        const I2R: [u8; 16] = [0x11; 16];
        const R2I: [u8; 16] = [0x22; 16];
        const REQ_EXCHANGE: u16 = 0x10;
        const NEW_EXCHANGE: u16 = 0x20;

        let controller = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let ctrl_addr = controller.local_addr().unwrap();
        let dev_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));
        let dev_addr = dev_transport.local_addr().unwrap();

        let mut session = SecureSession::new_device_role(
            Arc::clone(&dev_transport),
            ctrl_addr,
            LOCAL_SID,
            PEER_SID,
            SessionKeys {
                i2r: I2R,
                r2i: R2I,
                attestation_challenge: [0; 16],
            },
            DEV_NODE,
            CTRL_NODE,
        );
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let comm_server = CommissioningServer::new(dev, FabricStore::new());

        // Controller side, run concurrently with `serve_secured` below: send
        // the first ReadRequest, wait for its reply, then — instead of a
        // standalone ack — send a second ReadRequest on a *different*
        // exchange that piggybacks the first reply's ack, and finally wait
        // for its own reply too.
        let ctrl_task = tokio::spawn(async move {
            let req1 = im::encode_read_request(
                0,
                mat_controller::im::CLUSTER_BASIC_INFORMATION,
                mat_controller::im::ATTR_DATA_MODEL_REVISION,
            );
            let header1 = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 10,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto1 = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: im::OPCODE_READ_REQUEST,
                exchange_id: REQ_EXCHANGE,
                protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
                vendor_id: None,
            };
            let dg1 = seal_message(&I2R, &header1, &proto1, &req1, CTRL_NODE).unwrap();
            controller.send_to(&dg1, dev_addr).await.unwrap();

            let mut buf = [0u8; MAX_DATAGRAM];
            let (n1, from) = controller.recv_from(&mut buf).await.unwrap();
            let (h1, p1, _) = open_message(&R2I, &buf[..n1], DEV_NODE).unwrap();
            assert_eq!(p1.exchange_id, REQ_EXCHANGE);
            assert_eq!(p1.opcode, im::OPCODE_REPORT_DATA);
            assert!(p1.needs_ack);

            let req2 = im::encode_read_request(
                0,
                mat_controller::im::CLUSTER_BASIC_INFORMATION,
                mat_controller::im::ATTR_VENDOR_ID,
            );
            let header2 = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 11,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto2 = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: Some(h1.message_counter), // piggyback: acks dg1's reply
                opcode: im::OPCODE_READ_REQUEST,
                exchange_id: NEW_EXCHANGE,
                protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
                vendor_id: None,
            };
            let dg2 = seal_message(&I2R, &header2, &proto2, &req2, CTRL_NODE).unwrap();
            controller.send_to(&dg2, from).await.unwrap();

            // Proof the drain loop actually served dg2 (not just buffered
            // it): a real ReportData for VendorID must arrive on
            // NEW_EXCHANGE.
            let (n2, from2) = controller.recv_from(&mut buf).await.unwrap();
            let (h2, p2, payload2) = open_message(&R2I, &buf[..n2], DEV_NODE).unwrap();
            assert_eq!(p2.exchange_id, NEW_EXCHANGE);
            assert_eq!(p2.opcode, im::OPCODE_REPORT_DATA);
            let rd = im::decode_report_data_message(&payload2).unwrap();
            assert_eq!(
                rd.reports[0].attribute,
                Some(mat_controller::im::ATTR_VENDOR_ID)
            );
            assert_eq!(rd.reports[0].data, Some(serde_json::json!(0xFFF1)));

            // Ack this second reply too, so `serve_secured`'s own
            // `reply_reliable` for it completes promptly instead of
            // exhausting `MrpConfig::default()`'s retry budget.
            let ack_header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 12,
                source_node_id: None,
                destination: Destination::None,
            };
            let ack_proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: Some(h2.message_counter),
                opcode: OPCODE_MRP_STANDALONE_ACK,
                exchange_id: NEW_EXCHANGE,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            };
            let ack_dg = seal_message(&I2R, &ack_header, &ack_proto, &[], CTRL_NODE).unwrap();
            controller.send_to(&ack_dg, from2).await.unwrap();
        });

        // Device side: read dg1 off the (real) socket exactly like `run`'s
        // loop does, then hand it to `serve_secured` — whose internal
        // `reply_reliable` ack-wait is what actually reads dg2 off the
        // socket and resolves it via the cross-exchange piggyback ack,
        // which is what makes the drain loop kick in afterward.
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = dev_transport.recv_from(&mut buf).await.unwrap();
        serve_secured(
            &buf[..n],
            from,
            &mut session,
            0,
            &mut ServeState {
                node: &mut node,
                comm_server: &comm_server,
                mdns: None,
                subscription: &mut None,
                window: &mut CommissioningWindow::Closed,
                config: &test_config(),
            },
        )
        .await;

        ctrl_task.await.unwrap();
    }

    // ── Task 6: chunked ReadRequest reply flow ──────────────────────────

    /// A device-role closed-loop drive of `serve_read_request_chunked`
    /// (Task 6): a full-wildcard read against a `Node` carrying two ~600B
    /// attributes (each alone under `REPORT_CHUNK_BUDGET`, but the two
    /// together well past it) must come back as 2+ `ReportData` chunks,
    /// each non-final one answered with `StatusResponse(0)` on the same
    /// exchange before the next is sent — `mat` (`read_attribute`) has no
    /// chunk support to drive this against (brief's Step 4), so this test
    /// plays the controller role by hand at the raw-datagram level, the
    /// same technique `serve_secured_drains_and_serves_a_cross_exchange_
    /// piggybacked_request` above uses. `read_chunks`'s own split/flag
    /// correctness is covered by `datamodel.rs`'s unit tests; this test's
    /// job is only proving the runtime's send-chunk/await-StatusResponse
    /// loop actually round-trips over real sockets — the initiator-side
    /// counterpart to what `SecureSession::subscribe_wildcard`'s priming
    /// loop already exercises from the other end
    /// (`mat_controller::session::subscribe::subscribe_wildcard`).
    #[tokio::test]
    async fn read_request_chunked_flow_round_trips_two_or_more_chunks() {
        use mat_controller::crypto::{open_message, seal_message};
        use mat_controller::message::Destination;
        use mat_controller::session::SessionKeys;
        use mat_controller::tlv::{Tag, Writer};
        use mat_controller::transport::UdpTransport;

        use crate::core::datamodel::{ClusterHandler, InvokeReply};
        use crate::core::fabric_store::FabricStore;

        const LOCAL_SID: u16 = 0xAAAA;
        const PEER_SID: u16 = 0xBBBB;
        const CTRL_NODE: u64 = 1;
        const DEV_NODE: u64 = 2;
        const I2R: [u8; 16] = [0x11; 16];
        const R2I: [u8; 16] = [0x22; 16];
        const REQ_EXCHANGE: u16 = 0x30;

        /// Test-only cluster exposing one ~600B attribute — two of these
        /// registered on the node force `read_chunks` to split (two
        /// together exceed `REPORT_CHUNK_BUDGET`, though neither alone
        /// does). Cluster id is far outside any real cluster id range.
        struct FatHandler {
            cluster: u32,
        }
        impl ClusterHandler for FatHandler {
            fn cluster_id(&self) -> u32 {
                self.cluster
            }
            fn attributes(&self) -> Vec<u32> {
                vec![1]
            }
            fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
                if attribute != 1 {
                    return None;
                }
                let mut w = Writer::new();
                w.put_bytes(Tag::Anonymous, &[0xCD; 600]);
                Some(w.finish())
            }
            fn invoke(
                &mut self,
                _command: u32,
                _fields_tlv: &[u8],
                _ctx: &mut InvokeCtx,
            ) -> InvokeReply {
                InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
            }
        }

        /// Full-wildcard ReadRequest (every field of the one
        /// AttributePathIB omitted) — `mat_controller::im` has no public
        /// encoder for this shape (its `encode_read_request*` helpers all
        /// pin at least endpoint+cluster), so built by hand the same way
        /// `datamodel.rs`'s test-only `encode_read_request_paths` does.
        fn encode_full_wildcard_read_request() -> Vec<u8> {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.start_array(Tag::Context(0)); // AttributeRequests
            w.start_list(Tag::Anonymous); // AttributePathIB, all wildcard
            w.end_container();
            w.end_container(); // AttributeRequests
            w.put_bool(Tag::Context(3), true); // IsFabricFiltered
            w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
            w.end_container();
            w.finish()
        }

        let controller = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let dev_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));
        let dev_addr = dev_transport.local_addr().unwrap();

        let mut session = SecureSession::new_device_role(
            Arc::clone(&dev_transport),
            controller.local_addr().unwrap(),
            LOCAL_SID,
            PEER_SID,
            SessionKeys {
                i2r: I2R,
                r2i: R2I,
                attestation_challenge: [0; 16],
            },
            DEV_NODE,
            CTRL_NODE,
        );
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        node.add_cluster(
            0,
            Box::new(FatHandler {
                cluster: 0x9999_0001,
            }),
        );
        node.add_cluster(
            0,
            Box::new(FatHandler {
                cluster: 0x9999_0002,
            }),
        );
        let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let comm_server = CommissioningServer::new(dev, FabricStore::new());

        let ctrl_task = tokio::spawn(async move {
            let req = encode_full_wildcard_read_request();
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 10,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: im::OPCODE_READ_REQUEST,
                exchange_id: REQ_EXCHANGE,
                protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
                vendor_id: None,
            };
            let dg = seal_message(&I2R, &header, &proto, &req, CTRL_NODE).unwrap();
            controller.send_to(&dg, dev_addr).await.unwrap();

            let mut counter = 11u32;
            let mut chunk_count = 0usize;
            let mut buf = [0u8; MAX_DATAGRAM];
            loop {
                let (n, from) = controller.recv_from(&mut buf).await.unwrap();
                let peer = from;
                let (h, p, payload) = open_message(&R2I, &buf[..n], DEV_NODE).unwrap();
                assert_eq!(p.exchange_id, REQ_EXCHANGE);
                assert_eq!(p.opcode, im::OPCODE_REPORT_DATA);
                let rd = im::decode_report_data_message(&payload).unwrap();
                chunk_count += 1;

                if rd.more_chunks {
                    assert!(!rd.suppress_response);
                    // Reply with StatusResponse(0) on the same exchange —
                    // `serve_read_request_chunked`'s `reply_reliable` for
                    // this chunk resolves on any non-standalone-ack
                    // message on the exchange (same idiom
                    // `send_reliable`/`SecureSession::subscribe_wildcard`
                    // use), so this single reply both acks the chunk and
                    // is what the runtime's `session.recv` StatusResponse
                    // wait is looking for.
                    let ok = im::encode_status_response(0);
                    let resp_header = MessageHeader {
                        session_id: LOCAL_SID,
                        security_flags: 0,
                        message_counter: counter,
                        source_node_id: None,
                        destination: Destination::None,
                    };
                    let resp_proto = ProtocolHeader {
                        initiator: true,
                        needs_ack: false,
                        acked_counter: Some(h.message_counter),
                        opcode: im::OPCODE_STATUS_RESPONSE,
                        exchange_id: REQ_EXCHANGE,
                        protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
                        vendor_id: None,
                    };
                    let dg = seal_message(&I2R, &resp_header, &resp_proto, &ok, CTRL_NODE).unwrap();
                    controller.send_to(&dg, peer).await.unwrap();
                    counter += 1;
                } else {
                    assert!(rd.suppress_response);
                    // Final chunk: no StatusResponse expected from us, but
                    // still ack it (standalone) so the runtime's own
                    // `reply_reliable` for this last send completes
                    // promptly instead of exhausting its retry budget.
                    let ack_header = MessageHeader {
                        session_id: LOCAL_SID,
                        security_flags: 0,
                        message_counter: counter,
                        source_node_id: None,
                        destination: Destination::None,
                    };
                    let ack_proto = ProtocolHeader {
                        initiator: true,
                        needs_ack: false,
                        acked_counter: Some(h.message_counter),
                        opcode: OPCODE_MRP_STANDALONE_ACK,
                        exchange_id: REQ_EXCHANGE,
                        protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                        vendor_id: None,
                    };
                    let ack_dg =
                        seal_message(&I2R, &ack_header, &ack_proto, &[], CTRL_NODE).unwrap();
                    controller.send_to(&ack_dg, peer).await.unwrap();
                    break;
                }
            }
            chunk_count
        });

        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = dev_transport.recv_from(&mut buf).await.unwrap();
        serve_secured(
            &buf[..n],
            from,
            &mut session,
            0,
            &mut ServeState {
                node: &mut node,
                comm_server: &comm_server,
                mdns: None,
                subscription: &mut None,
                window: &mut CommissioningWindow::Closed,
                config: &test_config(),
            },
        )
        .await;

        let chunk_count = ctrl_task.await.unwrap();
        assert!(chunk_count >= 2, "expected 2+ chunks, got {chunk_count}");
    }
    // ── Task 12: chunked subscription priming ───────────────────────────

    /// Task 6's homework, settled here: priming a subscription against a
    /// `Node` fat enough to blow past `REPORT_CHUNK_BUDGET` must come back
    /// as several `ReportData` chunks and still complete with a
    /// `SubscribeResponse`.
    ///
    /// Unlike `read_request_chunked_flow_round_trips_two_or_more_chunks`
    /// above (which hand-rolls the controller at the datagram level,
    /// because `mat` has no chunk-aware read), the controller here is the
    /// real `SecureSession::subscribe_wildcard` — the same code path a
    /// commissioned `mat`/`matd` uses against real devices. It is
    /// therefore the authority on the wire contract: it acknowledges each
    /// priming chunk with `StatusResponse(0)` on the subscribe exchange and
    /// insists the `SubscribeResponse` follow on that same exchange, so a
    /// device that suppressed the final chunk's response, forgot the
    /// SubscriptionId, or answered on a fresh exchange would fail here.
    ///
    /// Only the device's *session* is built by hand (no PASE/CASE — that's
    /// `subscribe_loop.rs`'s job); everything above the session is real.
    #[tokio::test]
    async fn subscription_priming_round_trips_multiple_chunks() {
        use mat_controller::session::SessionKeys;
        use mat_controller::tlv::{Tag, Writer};
        use mat_controller::transport::UdpTransport;

        use crate::core::datamodel::{ClusterHandler, InvokeReply};
        use crate::core::fabric_store::FabricStore;

        const DEV_SID: u16 = 0xAAAA; // device's own session id
        const CTRL_SID: u16 = 0xBBBB; // controller's session id
        const CTRL_NODE: u64 = 1;
        const DEV_NODE: u64 = 2;
        const I2R: [u8; 16] = [0x11; 16];
        const R2I: [u8; 16] = [0x22; 16];

        /// One ~600B attribute per instance — three of them force priming
        /// past `REPORT_CHUNK_BUDGET`. Cluster ids are far outside any real
        /// range (same fixture idea as `datamodel`'s own chunking tests).
        struct FatHandler {
            cluster: u32,
        }
        impl ClusterHandler for FatHandler {
            fn cluster_id(&self) -> u32 {
                self.cluster
            }
            fn attributes(&self) -> Vec<u32> {
                vec![1]
            }
            fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
                if attribute != 1 {
                    return None;
                }
                let mut w = Writer::new();
                w.put_bytes(Tag::Anonymous, &[0xCD; 600]);
                Some(w.finish())
            }
            fn invoke(
                &mut self,
                _command: u32,
                _fields_tlv: &[u8],
                _ctx: &mut InvokeCtx,
            ) -> InvokeReply {
                InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
            }
        }

        let ctrl_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));
        let ctrl_addr = ctrl_transport.local_addr().unwrap();
        let dev_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));
        let dev_addr = dev_transport.local_addr().unwrap();

        let mut session = SecureSession::new_device_role(
            Arc::clone(&dev_transport),
            ctrl_addr,
            DEV_SID,
            CTRL_SID,
            SessionKeys {
                i2r: I2R,
                r2i: R2I,
                attestation_challenge: [0; 16],
            },
            DEV_NODE,
            CTRL_NODE,
        );
        // Controller role: mirror image of the device's ids (see
        // `SecureSession::new_device_role`'s doc for the key swap).
        let mut ctrl = SecureSession::new(
            Arc::clone(&ctrl_transport),
            dev_addr,
            CTRL_SID,
            DEV_SID,
            SessionKeys {
                i2r: I2R,
                r2i: R2I,
                attestation_challenge: [0; 16],
            },
            CTRL_NODE,
            DEV_NODE,
        );

        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        for i in 0..3u32 {
            node.add_cluster(
                0,
                Box::new(FatHandler {
                    cluster: 0x9999_0000 + i,
                }),
            );
        }
        let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let comm_server = CommissioningServer::new(dev, FabricStore::new());

        let ctrl_task = tokio::spawn(async move {
            let cfg = MrpConfig {
                initial_interval: Duration::from_millis(50),
                active_interval: Duration::from_millis(50),
                max_retries: 4,
                backoff: 1.2,
                jitter: 0.0,
            };
            // Full wildcard (`clusters` empty) so every fat attribute is
            // primed.
            ctrl.subscribe_wildcard(0, 30, false, &[], &cfg).await
        });

        // One datagram in: `serve_secured` drives the entire subscribe
        // interaction (every chunk plus the SubscribeResponse) from here,
        // reading the controller's StatusResponses off the socket itself.
        let mut subscription: Option<ActiveSubscription> = None;
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = dev_transport.recv_from(&mut buf).await.unwrap();
        serve_secured(
            &buf[..n],
            from,
            &mut session,
            0,
            &mut ServeState {
                node: &mut node,
                comm_server: &comm_server,
                mdns: None,
                subscription: &mut subscription,
                window: &mut CommissioningWindow::Closed,
                config: &test_config(),
            },
        )
        .await;

        let (sr, priming) = ctrl_task.await.unwrap().expect("subscribe should complete");
        assert!(
            priming.len() >= 2,
            "expected priming to arrive in 2+ chunks, got {}",
            priming.len()
        );
        for (i, chunk) in priming.iter().enumerate() {
            assert_eq!(
                chunk.subscription_id,
                Some(sr.subscription_id),
                "priming chunk {i} must carry the SubscriptionId"
            );
            assert!(
                !chunk.suppress_response,
                "priming chunk {i} must not suppress the response"
            );
        }
        let sub = subscription.expect("a completed subscribe must register the subscription");
        assert_eq!(sub.id, sr.subscription_id);
        assert_eq!(sub.max_interval, Duration::from_secs(30));
    }
    /// Fix round 1 (code review): a request that lands on *another*
    /// exchange while the device is waiting for a chunk's
    /// `StatusResponse(0)` must survive that wait and still be served.
    ///
    /// `screen_with` MRP-acks every authenticated request the moment it
    /// decodes it, delivery filter or not — so a request pulled out of the
    /// buffer by the status wait and then thrown away is gone for good: the
    /// peer has its ack and will never retransmit ("ack-then-drop of
    /// cross-exchange secured requests"). `await_peer_status_ok` therefore
    /// sets such messages aside and hands them back
    /// (`SecureSession::requeue_buffered_request`) for `serve_secured`'s
    /// drain.
    ///
    /// Drives the exact interleaving by hand: the controller answers the
    /// first chunk with a *standalone* ack (forcing the fallback wait
    /// instead of the piggybacked fast path), then squeezes a ReadRequest
    /// on a second exchange in before the chunk's StatusResponse. The read
    /// must still complete, and the second exchange must still get its
    /// ReportData.
    ///
    /// This also exercises the wait loop's termination: every *subsequent*
    /// chunk's status wait pulls that same requeued request out of the
    /// buffer again, sets it aside again, and has to fall through to the
    /// socket for the real StatusResponse. A loop that re-consumed its own
    /// set-aside messages would spin here, and one that gave up on them
    /// would stall the read — both show up as the controller's 20s
    /// "device went silent" timeout rather than a passing test.
    #[tokio::test]
    async fn a_request_interleaved_into_a_chunk_status_wait_is_not_lost() {
        use mat_controller::crypto::{open_message, seal_message};
        use mat_controller::message::Destination;
        use mat_controller::session::SessionKeys;
        use mat_controller::tlv::{Tag, Writer};
        use mat_controller::transport::UdpTransport;

        use crate::core::datamodel::{ClusterHandler, InvokeReply};
        use crate::core::fabric_store::FabricStore;

        const LOCAL_SID: u16 = 0xAAAA;
        const PEER_SID: u16 = 0xBBBB;
        const CTRL_NODE: u64 = 1;
        const DEV_NODE: u64 = 2;
        const I2R: [u8; 16] = [0x11; 16];
        const R2I: [u8; 16] = [0x22; 16];
        const EX_READ: u16 = 0x40;
        const EX_OTHER: u16 = 0x41;

        struct FatHandler {
            cluster: u32,
        }
        impl ClusterHandler for FatHandler {
            fn cluster_id(&self) -> u32 {
                self.cluster
            }
            fn attributes(&self) -> Vec<u32> {
                vec![1]
            }
            fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
                if attribute != 1 {
                    return None;
                }
                let mut w = Writer::new();
                w.put_bytes(Tag::Anonymous, &[0xCD; 600]);
                Some(w.finish())
            }
            fn invoke(
                &mut self,
                _command: u32,
                _fields_tlv: &[u8],
                _ctx: &mut InvokeCtx,
            ) -> InvokeReply {
                InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
            }
        }

        fn encode_full_wildcard_read_request() -> Vec<u8> {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.start_array(Tag::Context(0));
            w.start_list(Tag::Anonymous);
            w.end_container();
            w.end_container();
            w.put_bool(Tag::Context(3), true);
            w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
            w.end_container();
            w.finish()
        }

        let controller = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let dev_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));
        let dev_addr = dev_transport.local_addr().unwrap();

        let mut session = SecureSession::new_device_role(
            Arc::clone(&dev_transport),
            controller.local_addr().unwrap(),
            LOCAL_SID,
            PEER_SID,
            SessionKeys {
                i2r: I2R,
                r2i: R2I,
                attestation_challenge: [0; 16],
            },
            DEV_NODE,
            CTRL_NODE,
        );
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        node.add_cluster(
            0,
            Box::new(FatHandler {
                cluster: 0x9999_0001,
            }),
        );
        node.add_cluster(
            0,
            Box::new(FatHandler {
                cluster: 0x9999_0002,
            }),
        );
        let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let comm_server = CommissioningServer::new(dev, FabricStore::new());

        let ctrl_task = tokio::spawn(async move {
            let mut counter = 10u32;
            let mut send = |opcode: u8,
                            protocol_id: u16,
                            exchange_id: u16,
                            needs_ack: bool,
                            acked: Option<u32>,
                            payload: &[u8]| {
                let header = MessageHeader {
                    session_id: LOCAL_SID,
                    security_flags: 0,
                    message_counter: counter,
                    source_node_id: None,
                    destination: Destination::None,
                };
                let proto = ProtocolHeader {
                    initiator: true,
                    needs_ack,
                    acked_counter: acked,
                    opcode,
                    exchange_id,
                    protocol_id,
                    vendor_id: None,
                };
                counter += 1;
                seal_message(&I2R, &header, &proto, payload, CTRL_NODE).unwrap()
            };

            let dg = send(
                im::OPCODE_READ_REQUEST,
                PROTOCOL_ID_INTERACTION_MODEL,
                EX_READ,
                false,
                None,
                &encode_full_wildcard_read_request(),
            );
            controller.send_to(&dg, dev_addr).await.unwrap();

            let mut buf = [0u8; MAX_DATAGRAM];
            let mut chunks = 0usize;
            let mut interleaved_sent = false;
            let mut interleaved_answered = false;
            let mut read_done = false;

            // One loop for both exchanges: the device's answer to the
            // interleaved read can only arrive after the chunked read
            // finishes (the drain runs last), but the loop doesn't assume
            // that ordering.
            while !(read_done && interleaved_answered) {
                let (n, from) =
                    tokio::time::timeout(Duration::from_secs(20), controller.recv_from(&mut buf))
                        .await
                        .expect("device went silent")
                        .unwrap();
                let (h, p, payload) = open_message(&R2I, &buf[..n], DEV_NODE).unwrap();
                if p.protocol_id == PROTOCOL_ID_SECURE_CHANNEL {
                    continue; // the device's own standalone acks
                }
                assert_eq!(p.opcode, im::OPCODE_REPORT_DATA);

                if p.exchange_id == EX_OTHER {
                    let rd = im::decode_report_data_message(&payload).unwrap();
                    assert_eq!(
                        rd.reports[0].attribute,
                        Some(mat_controller::im::ATTR_VENDOR_ID),
                        "the interleaved read must be answered, not dropped"
                    );
                    interleaved_answered = true;
                    let ack = send(
                        OPCODE_MRP_STANDALONE_ACK,
                        PROTOCOL_ID_SECURE_CHANNEL,
                        EX_OTHER,
                        false,
                        Some(h.message_counter),
                        &[],
                    );
                    controller.send_to(&ack, from).await.unwrap();
                    continue;
                }

                assert_eq!(p.exchange_id, EX_READ);
                let rd = im::decode_report_data_message(&payload).unwrap();
                chunks += 1;

                // Always a *standalone* ack first — never a piggybacked
                // StatusResponse — so the device has to take
                // `await_peer_status_ok`'s fallback wait.
                let ack = send(
                    OPCODE_MRP_STANDALONE_ACK,
                    PROTOCOL_ID_SECURE_CHANNEL,
                    EX_READ,
                    false,
                    Some(h.message_counter),
                    &[],
                );
                controller.send_to(&ack, from).await.unwrap();

                if !rd.more_chunks {
                    read_done = true;
                    continue;
                }

                // ...and, exactly once, a request on another exchange
                // squeezed in while the device is inside that wait.
                if !interleaved_sent {
                    interleaved_sent = true;
                    let other = send(
                        im::OPCODE_READ_REQUEST,
                        PROTOCOL_ID_INTERACTION_MODEL,
                        EX_OTHER,
                        true, // needs_ack: this is the message screen_with acks then buffers
                        None,
                        &im::encode_read_request(
                            0,
                            mat_controller::im::CLUSTER_BASIC_INFORMATION,
                            mat_controller::im::ATTR_VENDOR_ID,
                        ),
                    );
                    controller.send_to(&other, from).await.unwrap();
                }

                let ok = send(
                    im::OPCODE_STATUS_RESPONSE,
                    PROTOCOL_ID_INTERACTION_MODEL,
                    EX_READ,
                    false,
                    None,
                    &im::encode_status_response(0),
                );
                controller.send_to(&ok, from).await.unwrap();
            }
            (chunks, interleaved_answered)
        });

        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = dev_transport.recv_from(&mut buf).await.unwrap();
        serve_secured(
            &buf[..n],
            from,
            &mut session,
            0,
            &mut ServeState {
                node: &mut node,
                comm_server: &comm_server,
                mdns: None,
                subscription: &mut None,
                window: &mut CommissioningWindow::Closed,
                config: &test_config(),
            },
        )
        .await;

        let (chunks, interleaved_answered) =
            tokio::time::timeout(Duration::from_secs(30), ctrl_task)
                .await
                .expect("controller task hung — an interleaved request was probably dropped")
                .unwrap();
        assert!(
            chunks >= 2,
            "expected a chunked read, got {chunks} chunk(s)"
        );
        assert!(interleaved_answered);
    }

    /// Timed Interaction（spec §8.9.4）の後続リクエスト: initiator は
    /// TimedRequest → StatusResponse(SUCCESS) 受領後、**同一 exchange** で
    /// timed Invoke を送る（StatusResponse への ack はそこに piggyback）。
    /// `reply_reliable` はこの後続リクエストを `Ok(Some(msg))` として返すが、
    /// 旧 `serve_secured_message` は戻り値を捨てていたため invoke は MRP ack
    /// だけされて永遠に応答されず、Google Play Services スタック（Android HA
    /// アプリ経由の commissioning）が 45 秒タイムアウトで中断していた
    /// （2026-08-18 実測）。
    #[tokio::test]
    async fn a_timed_invoke_on_the_same_exchange_is_served_not_dropped() {
        use mat_controller::crypto::{open_message, seal_message};
        use mat_controller::message::Destination;
        use mat_controller::session::SessionKeys;
        use mat_controller::tlv::{Tag, Writer};
        use mat_controller::transport::UdpTransport;

        use crate::core::fabric_store::FabricStore;

        const LOCAL_SID: u16 = 0xAAAA;
        const PEER_SID: u16 = 0xBBBB;
        const CTRL_NODE: u64 = 1;
        const DEV_NODE: u64 = 2;
        const I2R: [u8; 16] = [0x11; 16];
        const R2I: [u8; 16] = [0x22; 16];
        const EX: u16 = 0x50;

        let controller = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let dev_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));
        let dev_addr = dev_transport.local_addr().unwrap();

        let mut session = SecureSession::new_device_role(
            Arc::clone(&dev_transport),
            controller.local_addr().unwrap(),
            LOCAL_SID,
            PEER_SID,
            SessionKeys {
                i2r: I2R,
                r2i: R2I,
                attestation_challenge: [0; 16],
            },
            DEV_NODE,
            CTRL_NODE,
        );
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let comm_server = CommissioningServer::new(dev, FabricStore::new());

        let ctrl_task = tokio::spawn(async move {
            let mut counter = 10u32;
            let mut send = |opcode: u8,
                            protocol_id: u16,
                            needs_ack: bool,
                            acked: Option<u32>,
                            payload: &[u8]| {
                let header = MessageHeader {
                    session_id: LOCAL_SID,
                    security_flags: 0,
                    message_counter: counter,
                    source_node_id: None,
                    destination: Destination::None,
                };
                let proto = ProtocolHeader {
                    initiator: true,
                    needs_ack,
                    acked_counter: acked,
                    opcode,
                    exchange_id: EX,
                    protocol_id,
                    vendor_id: None,
                };
                counter += 1;
                seal_message(&I2R, &header, &proto, payload, CTRL_NODE).unwrap()
            };

            // TimedRequest: struct{0: timeout-ms, 255: revision}
            let timed_payload = {
                let mut w = Writer::new();
                w.start_struct(Tag::Anonymous);
                w.put_uint(Tag::Context(0), 300);
                w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
                w.end_container();
                w.finish()
            };
            let dg = send(
                im::OPCODE_TIMED_REQUEST,
                PROTOCOL_ID_INTERACTION_MODEL,
                true,
                None,
                &timed_payload,
            );
            controller.send_to(&dg, dev_addr).await.unwrap();

            let mut buf = [0u8; MAX_DATAGRAM];
            let mut invoke_sent = false;
            loop {
                let (n, from) =
                    tokio::time::timeout(Duration::from_secs(20), controller.recv_from(&mut buf))
                        .await
                        .expect("device went silent — the timed invoke was probably dropped")
                        .unwrap();
                let (h, p, payload) = open_message(&R2I, &buf[..n], DEV_NODE).unwrap();
                if p.protocol_id == PROTOCOL_ID_SECURE_CHANNEL {
                    continue; // the device's own standalone acks
                }
                if p.opcode == im::OPCODE_STATUS_RESPONSE {
                    assert_eq!(
                        im::decode_status_response(&payload).unwrap(),
                        im::STATUS_SUCCESS
                    );
                    assert!(!invoke_sent, "one StatusResponse expected");
                    invoke_sent = true;
                    // 後続の timed invoke: 同一 exchange、StatusResponse への
                    // ack を piggyback（standalone ack は送らない — 実機の
                    // chip スタックの挙動に合わせる）。
                    let invoke = send(
                        im::OPCODE_INVOKE_REQUEST,
                        PROTOCOL_ID_INTERACTION_MODEL,
                        true,
                        Some(h.message_counter),
                        &im::encode_invoke_request(0, im::CLUSTER_BASIC_INFORMATION, 0x7F, None),
                    );
                    controller.send_to(&invoke, from).await.unwrap();
                    continue;
                }
                assert_eq!(
                    p.opcode,
                    im::OPCODE_INVOKE_RESPONSE,
                    "the timed invoke must be answered with an InvokeResponse"
                );
                let out = im::decode_invoke_response(&payload).unwrap();
                assert_eq!(out.status, im::STATUS_UNSUPPORTED_COMMAND);
                let ack = send(
                    OPCODE_MRP_STANDALONE_ACK,
                    PROTOCOL_ID_SECURE_CHANNEL,
                    false,
                    Some(h.message_counter),
                    &[],
                );
                controller.send_to(&ack, from).await.unwrap();
                return;
            }
        });

        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = dev_transport.recv_from(&mut buf).await.unwrap();
        serve_secured(
            &buf[..n],
            from,
            &mut session,
            0,
            &mut ServeState {
                node: &mut node,
                comm_server: &comm_server,
                mdns: None,
                subscription: &mut None,
                window: &mut CommissioningWindow::Closed,
                config: &test_config(),
            },
        )
        .await;

        tokio::time::timeout(Duration::from_secs(30), ctrl_task)
            .await
            .expect("controller task hung — the same-exchange timed invoke was dropped")
            .unwrap();
    }
}
