use super::*;

/// PBKDF iterations this runtime advertises for PASE (spec §3.9 legal
/// range 1000..=100000). 10k rounds of PBKDF2-SHA256 on the Pi is
/// millisecond-class and paid once per commissioning attempt, so raising it
/// well above the 1000 floor costs nothing in practice while narrowing the
/// brute-force budget an attacker gets per guess.
const PASE_ITERATIONS: u32 = 10_000;

/// The *boot* commissioning window's upper bound (spec §5.4.2.3: the PASE
/// window a commissioner may use to complete commissioning must not exceed
/// 15 minutes) — the duration a freshly booted, never-commissioned device's
/// window runs for. A window opened later by the Administrator
/// Commissioning cluster's `OpenCommissioningWindow` (spec §11.19.8.1, Task
/// 4's `CommissioningWindow::EnhancedOpen`) uses its own `CommissioningTimeout`
/// instead (`apply_window_request`), not this constant.
pub(super) const COMMISSIONING_WINDOW_DURATION: Duration = Duration::from_secs(15 * 60);

/// Whether new PASE attempts are currently admitted (Task 14), and — once
/// opened by the Administrator Commissioning cluster (Task 4) — what
/// verifier material/discriminator that admission should use instead of the
/// boot passcode. Boot-time policy (decided once in `boot`, before
/// `serve_forever`'s loop starts): `Open` if `comm_server.fabrics()` is empty (a never-
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
pub(super) enum CommissioningWindow {
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
    pub(super) fn is_open(&self) -> bool {
        !matches!(self, CommissioningWindow::Closed)
    }
}

/// Same shape as `mdns_retry_deadline`/`fail_safe_expiry_deadline`: resolves
/// once at the open window's deadline (boot or ECM — both variants carry
/// `until`), or never (`std::future::pending`) once it's `Closed`. This
/// `select!` branch is the mechanism that actually enforces spec §5.4.2.3's
/// 15-minute PASE upper bound (boot) / the requested `CommissioningTimeout`
/// (ECM) — nothing else polls `until` against the clock.
pub(super) async fn commissioning_window_deadline(window: &CommissioningWindow) {
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
pub(super) fn apply_window_request(request: WindowRequest) -> CommissioningWindow {
    let until = Instant::now() + Duration::from_secs(u64::from(request.timeout_s));
    CommissioningWindow::EnhancedOpen { until, request }
}

/// The PASE verifier configuration the current window should be served
/// with: the boot passcode for `Open`/`Closed` (a closed window never
/// actually reaches PASE — `admit_unsecured` already refused it — so this
/// arm is only reached, in practice, for the still-open boot window), or the
/// commissioner-supplied verifier material for `EnhancedOpen`.
/// `responder_session_id` is per-attempt
/// (`mat_controller::case::random_nonzero_u16`) and always passed through
/// regardless of which window is active.
pub(super) fn pase_config_for_window(
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
pub(super) fn advert_params_for_window(
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
pub(super) enum AdminWindowAction {
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
pub(super) fn admin_window_action(
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
pub(super) async fn fail_safe_expiry_deadline(comm_server: &CommissioningServer) {
    match comm_server.fail_safe_deadline() {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}
