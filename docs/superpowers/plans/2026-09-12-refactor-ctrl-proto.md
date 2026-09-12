# Refactor lane ctrl-proto (mat-controller protocol core) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the mirror-image duplication inside `mat-controller`'s protocol core (SecureChannel constants, CASE wire codec, unsecured exchange, MRP send/recv loops, IM client preamble/postamble, IM path/status encoders) and delete its dead code, with zero behaviour change on the wire.

**Architecture:** Pure refactor. New single-source modules: `secure_channel` (constants + StatusReport codec), `case/wire.rs` (Sigma1/2/3 + TBS/TBE codecs, ECDH, salts), `exchange::ExchangeCore` (one unsecured exchange engine behind the two public newtypes `UnsecuredExchange` / `ResponderExchange`), and two generic MRP loops (`mrp_send_loop` / `recv_until`) shared by the unsecured exchange and `SecureSession`. Everything else is deletion or extraction of small helpers. Public type/function names other crates use are preserved (checked by grep in this plan).

**Tech Stack:** Rust 2021, tokio, MSRV 1.87 (async closures `AsyncFnMut` are stable since 1.85 and verified to compile on the local 1.98 toolchain). Tests: `cargo test -p mat-controller`; final gate `task check`.

**Spec:** `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/ctrl-proto.md` (lane sheet), `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/_common.md` (common rules), `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/audit.md` (audit backlog; line numbers are as of main 411ba68).

## Global Constraints

- Worktree: `/home/noguk/ghq/github.com/nogu3/mat-wt/ctrl-proto`, branch `refactor/ctrl-proto`. Run every command from there. Never `cd` into the main checkout. Never merge to `main`, never push, never release/deploy.
- **Only these files may be edited:** `crates/mat-controller/src/{tlv,message,exchange,transport,counter,crypto,spake2p,pase,case,case_responder,race,sync,btp,ble,test_support,lib}.rs`, `crates/mat-controller/src/session/*`, `crates/mat-controller/src/im/*`, `crates/mat-controller/tests/*.rs`, plus new files `crates/mat-controller/src/secure_channel.rs` and `crates/mat-controller/src/case/wire.rs`. **Do not touch** `cert.rs` / `fabric.rs` / `x509.rs` / `kvs.rs` / `commissioning/*` / `dnssd/*` / `attestation.rs` / `group*.rs` / `cd.rs` / `asn1.rs` / `setup_code.rs`, nor any other crate (`mat-device`, `mat-native`, `mat`, `matd`, `mat-core`). Other lanes edit those in parallel; touching them causes merge conflicts.
- Behaviour change must be zero. Wire bytes, error variants matched by other crates, log wording pinned by tests, and ordering must stay identical. The only accepted non-zero deltas are listed explicitly in a task and must be recorded in the DONE file.
- Public API used by other crates must keep working unchanged (verified by grep in this session): `case::{establish, establish_any, encode_status_report, parse_status_report, encode_sigma1, derive_sigma_key, derive_session_keys, random_p256_secret, random_nonzero_u16, eph_pub_bytes, CaseError, RECV_TIMEOUT, RACE_STAGGER, Established, EstablishAnyError}`, `case_responder::{CaseFabric, CaseOutput, CaseCoreError, CaseResponderCore}`, `exchange::{ExchangeError, IncomingMessage, MrpConfig, UnsecuredExchange, ResponderExchange, total_budget, unit_random, jittered_interval, MRP_BACKOFF_JITTER}`, `ResponderExchange::{adopt, recv, reply_reliable, reply_final}`, `UnsecuredExchange::{new, exchange_id, last_sent_counter, send_reliable, send_once, recv}`, `pase::{establish, PaseError, encode_*/decode_*, pake_context, validate_pbkdf_params, OPCODE_*}`, every `SecureSession` method listed in `session/mod.rs`, `im::*` re-exports except the ones deleted in Task 3, `btp::*` public items (`Packet.seq` type change is accepted, see Task 3), `test_support::*` public items.
- Adding a new `CaseError` variant is safe: `mat-native/src/commission.rs::kind_of` matches with a `_ =>` wildcard (verified).
- After each task: `cargo test -p mat-controller` green (the loopback tests `case_self_handshake`, `pase_self_handshake`, `btp_pase_plumbing` must pass; `live_*` stay `#[ignore]`). Then commit on `refactor/ctrl-proto` with a small, descriptive message. The final task runs `task check` (fmt:check + clippy + doc:check + test).
- `cargo fmt` before every commit (`cargo fmt -p mat-controller`). Clippy must stay warning-free (`cargo clippy -p mat-controller --all-targets --features test-responder -- -D warnings`).
- Other sessions run cargo concurrently: builds can be slow. Wait; do not kill them.
- Unit tests inside `src/` can use `crate::test_support` because the self dev-dependency enables `test-responder` for the lib's own test build. If a `cfg(feature = "test-responder")` item turns out to be unavailable in a unit test, gate the module as `#[cfg(any(test, feature = "test-responder"))]` in `lib.rs` (allowed; keep `#[doc(hidden)]`).
- Commit message format: Japanese or English is fine; end with the attribution lines from the session reminder (`Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and the `Claude-Session:` line).

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `src/secure_channel.rs` (new) | SecureChannel protocol constants (`GENERAL_CODE_*`, `SC_PROTOCOL_CODE_*`, `OPCODE_SIGMA*`, `OPCODE_PBKDF_*`/`OPCODE_PASE_PAKE*` stay in `pase`), StatusReport encode/parse, `session_keys_from_hkdf` (HKDF-48 → `SessionKeys` split shared by PASE and CASE). |
| `src/case.rs` | CASE initiator state machine only (`establish`, `establish_any`, `CaseError`). Re-exports the wire codec it used to define (`encode_sigma1`, `parse_sigma2`, `decrypt_tbe2`, …) and the StatusReport codec from `secure_channel` so external paths stay valid. |
| `src/case/wire.rs` (new) | Sigma1/2/3 encode+parse (both directions), TBS/TBE encode, TBE parse, ECDH, S2K/S3K salt builders, nonces/info constants. Error type is `&'static str` (callers wrap in `CaseError::Sigma2Malformed` / `CaseCoreError::Decode`). |
| `src/case_responder.rs` | CASE responder state machine only; codec comes from `case::wire`. |
| `src/exchange.rs` | `MrpConfig` + retry math (unchanged), `MrpEndpoint` trait, generic `mrp_send_loop` / `recv_until`, `ExchangeCore { role }`, public newtypes `UnsecuredExchange` / `ResponderExchange`. |
| `src/session/{mod,mrp,responder,subscribe,client}.rs` | `SecureSession` implements `MrpEndpoint`; every MRP loop delegates to the generic loops; `client.rs` gains `im_request` / `expect_im`. |
| `src/im/{read,invoke,write,mod}.rs` | Shared `put_attribute_path` / `put_status_ib` / `decode_status_ib_code`; generic `decode_first_invoke_response_ib`; dead constants and `encode_write_request`/`encode_im_value` removed. |
| `src/pase.rs` | `derive_session_keys` becomes a `pub fn`; test helpers move to `test_support`; struct-field loops use `tlv::StructFields` (Task 10, optional). |
| `src/test_support.rs` | Shared unsecured-datagram helpers (`build_unsecured`, `recv_dg`, `decode_unsecured`) become `pub`; `FakePeripheral` (BTP) moves here. |
| `src/session/test_util.rs` | `udp_session_pair()`, `device_initiated_datagram()`. |
| `src/tlv.rs` | `expect` messages on the `take(n)` conversions; `StructFields` iterator (Task 10). |
| `src/message.rs` | `expect` messages on the `take(n)` conversions. |
| `src/btp.rs` | `Packet.seq: u8`; `FakePeripheral` removed from its test module. |
| `src/spake2p.rs` | The seven `pub(crate)` helpers become private; stale doc comments removed. |
| `tests/live_all_clusters.rs` | Uses `secure_channel::OPCODE_SIGMA1` instead of a local constant. |
| `tests/btp_pase_plumbing.rs` | Uses `test_support::FakePeripheral`. |

---

### Task 1: `secure_channel` module (constants + StatusReport codec) and the `Sigma2Malformed` mislabel

**Files:**
- Create: `crates/mat-controller/src/secure_channel.rs`
- Modify: `crates/mat-controller/src/lib.rs` (add `pub mod secure_channel;`)
- Modify: `crates/mat-controller/src/case.rs:25-39, 288-315, 491, 586-592` (constants, `parse_status_report`/`encode_status_report`, `CaseError`)
- Modify: `crates/mat-controller/src/case_responder.rs:58-71, 393-394` (opcodes, `encode_status_report` import)
- Modify: `crates/mat-controller/src/pase.rs:17, 44-49, 537-543, 558-566, 595-600, 619-626, 649-656, 926-930, 1066-1070`
- Modify: `crates/mat-controller/src/session/mrp.rs:121-145, 650`
- Modify: `crates/mat-controller/src/test_support.rs:30-32, 41, 102`
- Modify: `crates/mat-controller/tests/live_all_clusters.rs:19-20, 39`

**Interfaces:**
- Produces:
  ```rust
  // secure_channel.rs
  pub const OPCODE_SIGMA1: u8 = 0x30;
  pub const OPCODE_SIGMA2: u8 = 0x31;
  pub const OPCODE_SIGMA3: u8 = 0x32;
  pub const GENERAL_CODE_SUCCESS: u16 = 0;
  pub const GENERAL_CODE_FAILURE: u16 = 1;
  pub const SC_PROTOCOL_CODE_NO_SHARED_TRUST_ROOTS: u16 = 1;
  pub const SC_PROTOCOL_CODE_INVALID_PARAMETER: u16 = 2;
  pub const SC_PROTOCOL_CODE_CLOSE_SESSION: u16 = 2;
  pub const SC_PROTOCOL_CODE_BUSY: u16 = 4;
  pub const STATUS_REPORT_SUCCESS: (u16, u32, u16) = (0, 0, 0);
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct StatusReportTruncated;
  pub fn parse_status_report(payload: &[u8]) -> Result<(u16, u32, u16), StatusReportTruncated>;
  pub fn encode_status_report(general_code: u16, protocol_id: u32, protocol_code: u16) -> Vec<u8>;
  ```
- `case.rs` keeps `pub use crate::secure_channel::{encode_status_report, parse_status_report};` so `mat_controller::case::encode_status_report` (used by mat-device) still resolves. `case::SC_PROTOCOL_CODE_CLOSE_SESSION` becomes `pub use crate::secure_channel::SC_PROTOCOL_CODE_CLOSE_SESSION;` (keep `pub(crate)` visibility via `pub(crate) use`).
- `case_responder::{OPCODE_SIGMA1, OPCODE_SIGMA2, OPCODE_SIGMA3}` become `pub(crate) use crate::secure_channel::{OPCODE_SIGMA1, OPCODE_SIGMA2, OPCODE_SIGMA3};` (test_support imports them from `case_responder` today; keep that path working).
- New `CaseError::StatusReportMalformed { stage: &'static str }` with Display `"case {stage}: malformed StatusReport (truncated)"`. Used where `parse_status_report` fails: stage `"sigma1"` (reply to Sigma1) and `"sigma3"` (reply to Sigma3). This is the fix for the audit's "Sigma2Malformed mislabel" (a truncated StatusReport after Sigma3 used to print "case sigma2: malformed message (status report truncated)"). `mat-native`'s `kind_of` maps both old and new variants through its `_ =>` wildcard, so the error kind is unchanged. **Record this Display-text change in the DONE file.**

- [ ] **Step 1: Write the failing tests** — add to the new file `src/secure_channel.rs` (create the file with just the tests + a `pub mod` declaration in `lib.rs` first so the failure is "function not found"):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_report() {
        let ok = [0u8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_status_report(&ok).unwrap(), (0, 0, 0));
        let busy = [1u8, 0, 0, 0, 0, 0, 4, 0]; // FAILURE / SC / BUSY
        assert_eq!(parse_status_report(&busy).unwrap(), (1, 0, 4));
        assert_eq!(parse_status_report(&[0u8; 4]), Err(StatusReportTruncated));
    }

    #[test]
    fn encode_status_report_round_trips_through_parse() {
        let buf = encode_status_report(GENERAL_CODE_FAILURE, 0, SC_PROTOCOL_CODE_BUSY);
        assert_eq!(buf, [1u8, 0, 0, 0, 0, 0, 4, 0]);
        assert_eq!(parse_status_report(&buf).unwrap(), (1, 0, 4));
    }

    #[test]
    fn constants_match_spec_tables() {
        assert_eq!((OPCODE_SIGMA1, OPCODE_SIGMA2, OPCODE_SIGMA3), (0x30, 0x31, 0x32));
        assert_eq!(GENERAL_CODE_SUCCESS, 0);
        assert_eq!(GENERAL_CODE_FAILURE, 1);
        assert_eq!(SC_PROTOCOL_CODE_NO_SHARED_TRUST_ROOTS, 1);
        assert_eq!(SC_PROTOCOL_CODE_INVALID_PARAMETER, 2);
        assert_eq!(SC_PROTOCOL_CODE_CLOSE_SESSION, 2);
        assert_eq!(SC_PROTOCOL_CODE_BUSY, 4);
        assert_eq!(STATUS_REPORT_SUCCESS, (0, 0, 0));
    }
}
```

Also add to `case.rs` tests (next to `parses_status_report`):

```rust
    /// A truncated StatusReport after Sigma3 must be labelled with the
    /// sigma3 stage, not "sigma2" (audit 2026-09-12 bug candidate).
    #[test]
    fn status_report_malformed_names_its_stage() {
        let e = CaseError::StatusReportMalformed { stage: "sigma3" };
        assert_eq!(e.to_string(), "case sigma3: malformed StatusReport (truncated)");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mat-controller --lib secure_channel 2>&1 | tail -20`
Expected: compile error (`parse_status_report` / constants not found in `secure_channel`).

- [ ] **Step 3: Implement `src/secure_channel.rs`**

```rust
//! SecureChannel protocol (spec §4.11, §4.13, §4.14): the wire constants and
//! the StatusReport codec every SecureChannel state machine shares — the
//! PASE initiator (`pase`), the CASE initiator (`case`), the CASE responder
//! (`case_responder`), `SecureSession::send_close_session` and the test
//! responders. One definition here; the state machines only re-export.
//!
//! PASE's own opcodes (`OPCODE_PBKDF_PARAM_REQUEST` …) stay in `pase`
//! because `tests/btp_pase_plumbing.rs` and mat-device import them from
//! there; the CASE opcodes live here because both `case` and
//! `case_responder` need them.

/// CASE opcodes (spec §4.14.1).
pub const OPCODE_SIGMA1: u8 = 0x30;
pub const OPCODE_SIGMA2: u8 = 0x31;
pub const OPCODE_SIGMA3: u8 = 0x32;
// StatusReport is `message::OPCODE_STATUS_REPORT` (0x40).

/// StatusReport `GeneralCode` (spec §4.11.3, Table 22).
pub const GENERAL_CODE_SUCCESS: u16 = 0;
pub const GENERAL_CODE_FAILURE: u16 = 1;

/// SecureChannel-specific `ProtocolCode`s (spec §4.11.3.1). Note that the
/// value 2 means `CloseSession` under `GENERAL_CODE_SUCCESS` and
/// `InvalidParameter` under `GENERAL_CODE_FAILURE` — the general code
/// disambiguates, so both constants exist.
pub const SC_PROTOCOL_CODE_NO_SHARED_TRUST_ROOTS: u16 = 1;
pub const SC_PROTOCOL_CODE_INVALID_PARAMETER: u16 = 2;
pub const SC_PROTOCOL_CODE_CLOSE_SESSION: u16 = 2;
pub const SC_PROTOCOL_CODE_BUSY: u16 = 4;

/// The `(general_code, protocol_id, protocol_code)` triple of a successful
/// StatusReport (session established).
pub const STATUS_REPORT_SUCCESS: (u16, u32, u16) = (0, 0, 0);

/// `parse_status_report`'s only failure: fewer than 8 payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusReportTruncated;

impl std::fmt::Display for StatusReportTruncated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "status report truncated")
    }
}

impl std::error::Error for StatusReportTruncated {}

/// Parses a StatusReport payload: 8 bytes LE `{general_code: u16,
/// protocol_id: u32, protocol_code: u16}` (spec §4.11.3). Trailing
/// protocol-specific data is ignored.
pub fn parse_status_report(payload: &[u8]) -> Result<(u16, u32, u16), StatusReportTruncated> {
    if payload.len() < 8 {
        return Err(StatusReportTruncated);
    }
    let general_code = u16::from_le_bytes(payload[0..2].try_into().expect("2 bytes"));
    let protocol_id = u32::from_le_bytes(payload[2..6].try_into().expect("4 bytes"));
    let protocol_code = u16::from_le_bytes(payload[6..8].try_into().expect("2 bytes"));
    Ok((general_code, protocol_id, protocol_code))
}

/// Encodes a StatusReport payload (the inverse of [`parse_status_report`]):
/// 8 bytes LE `{general_code, protocol_id, protocol_code}`. Used to abort a
/// handshake explicitly (PASE confirm mismatch, bad PBKDF params) — dropping
/// the exchange silently leaves the responder holding its establishment
/// slot until its Pake3/Sigma3 timeout (spec §4.11.3 / §4.13.1.4) — and by
/// responders for their success/failure replies.
pub fn encode_status_report(general_code: u16, protocol_id: u32, protocol_code: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&general_code.to_le_bytes());
    buf.extend_from_slice(&protocol_id.to_le_bytes());
    buf.extend_from_slice(&protocol_code.to_le_bytes());
    buf
}
```

Add `pub mod secure_channel;` to `lib.rs` (alphabetical, after `pub mod race;` / `pub mod session;` → place after `session`).

- [ ] **Step 4: Rewire `case.rs`**

Replace lines 25-28 (`OPCODE_CASE_SIGMA1/2/3` consts) with `use crate::secure_channel::{OPCODE_SIGMA1, OPCODE_SIGMA2, OPCODE_SIGMA3, StatusReportTruncated};` and rename the three uses in `establish` (`OPCODE_CASE_SIGMA1` → `OPCODE_SIGMA1`, etc.). Replace `const STATUS_SUCCESS: (u16, u32, u16) = (0, 0, 0);` with `use crate::secure_channel::STATUS_REPORT_SUCCESS;` and its use at line 587. Replace line 37-39 (`pub(crate) const SC_PROTOCOL_CODE_CLOSE_SESSION`) with:

```rust
pub(crate) use crate::secure_channel::SC_PROTOCOL_CODE_CLOSE_SESSION;
/// StatusReport codec — lives in [`crate::secure_channel`]; re-exported so
/// mat-device's CASE/PASE net drivers keep importing it from here.
pub use crate::secure_channel::{encode_status_report, parse_status_report};
```

Delete the `parse_status_report` / `encode_status_report` definitions (old lines 288-315). Add the variant to `CaseError` (after `Sigma2Malformed`):

```rust
    /// A StatusReport reply that was too short to decode. `stage` is the
    /// message it answered (`"sigma1"` or `"sigma3"`).
    StatusReportMalformed {
        stage: &'static str,
    },
```
with Display arm `CaseError::StatusReportMalformed { stage } => write!(f, "case {stage}: malformed StatusReport (truncated)")`.

Update the two call sites in `establish`:
```rust
            let (general_code, _protocol_id, protocol_code) = parse_status_report(&msg.payload)
                .map_err(|StatusReportTruncated| CaseError::StatusReportMalformed { stage: "sigma1" })?;
```
and after Sigma3:
```rust
    let (general_code, _protocol_id, protocol_code) = parse_status_report(&msg.payload)
        .map_err(|StatusReportTruncated| CaseError::StatusReportMalformed { stage: "sigma3" })?;
    if (general_code, _protocol_id, protocol_code) != STATUS_REPORT_SUCCESS {
```
In `case.rs` tests: delete `parses_status_report` and `encode_status_report_round_trips_through_parse` (they moved to `secure_channel`), keep everything else.

- [ ] **Step 5: Rewire `case_responder.rs`, `pase.rs`, `session/mrp.rs`, `test_support.rs`, `tests/live_all_clusters.rs`**

`case_responder.rs`: replace lines 68-71 with
```rust
// Wire opcodes (spec §4.14) — single definition in `secure_channel`;
// re-exported crate-wide because `test_support` reads them from here.
pub(crate) use crate::secure_channel::{OPCODE_SIGMA1, OPCODE_SIGMA2, OPCODE_SIGMA3};
```
and change `use crate::case::{derive_session_keys, derive_sigma_key, encode_status_report, eph_pub_bytes, random_p256_secret};` to import `encode_status_report` from `crate::secure_channel` instead (and `GENERAL_CODE_SUCCESS`); the reply becomes `encode_status_report(GENERAL_CODE_SUCCESS, 0, 0)` (same bytes).

`pase.rs`: line 17 becomes `use crate::case::random_nonzero_u16;` plus `use crate::secure_channel::{encode_status_report, parse_status_report, GENERAL_CODE_FAILURE, SC_PROTOCOL_CODE_INVALID_PARAMETER};`. Delete lines 44-49 (`STATUS_SUCCESS`, `GENERAL_CODE_FAILURE`, `SC_PROTOCOL_CODE_INVALID_PARAMETER`) and replace `STATUS_SUCCESS` at line 651 with a local comparison against `crate::secure_channel::STATUS_REPORT_SUCCESS`:
```rust
    let (general_code, protocol_id, protocol_code) =
        parse_status_report(&msg3.payload).map_err(|_| PaseError::Malformed("status report"))?;
    if (general_code, protocol_id, protocol_code) != STATUS_REPORT_SUCCESS {
```
(PASE's old check ignored `protocol_id`; the success StatusReport carries protocol id 0, and every conformant responder — chip, matter.js, `test_support::pase_responder_task` (`[0u8; 8]`) — sends 0. **If you prefer strict zero-change, keep the 2-tuple compare `(general_code, protocol_code) != (0, 0)`. Do that: keep the 2-tuple, do not introduce the 3-tuple compare in PASE.**) The tests at old lines 926-930 / 1066-1070 keep using `GENERAL_CODE_FAILURE` / `SC_PROTOCOL_CODE_INVALID_PARAMETER` through the new import.

`session/mrp.rs:127-131`: use `crate::secure_channel::{encode_status_report, SC_PROTOCOL_CODE_CLOSE_SESSION, GENERAL_CODE_SUCCESS}` (`encode_status_report(GENERAL_CODE_SUCCESS, u32::from(PROTOCOL_ID_SECURE_CHANNEL), SC_PROTOCOL_CODE_CLOSE_SESSION)`); line 650 `crate::case::parse_status_report` → `crate::secure_channel::parse_status_report`.

`test_support.rs`: line 41 `const PROTO_SECURE_CHANNEL: u16 = 0x0000;` → delete, use `PROTOCOL_ID_SECURE_CHANNEL` from `crate::message` at line 102 (already imports from `crate::message`; extend the import list). The `OPCODE_SIGMA1/3` import from `case_responder` still works (re-export) — leave it.

`tests/live_all_clusters.rs`: delete the local `const OPCODE_CASE_SIGMA1` and `use mat_controller::secure_channel::OPCODE_SIGMA1;` instead.

- [ ] **Step 6: Run tests**

Run: `cargo test -p mat-controller 2>&1 | tail -30`
Expected: all green (including `case_self_handshake`, `pase_self_handshake`, `btp_pase_plumbing`).

- [ ] **Step 7: Verify no stray duplicates remain**

Run: `/usr/bin/grep -rn -E "OPCODE_CASE_SIGMA|const GENERAL_CODE_|const SC_PROTOCOL_CODE_|const STATUS_SUCCESS|PROTO_SECURE_CHANNEL" crates/mat-controller/src crates/mat-controller/tests`
Expected: only the definitions inside `src/secure_channel.rs`.

- [ ] **Step 8: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/secure_channel.rs crates/mat-controller/src/lib.rs crates/mat-controller/src/case.rs crates/mat-controller/src/case_responder.rs crates/mat-controller/src/pase.rs crates/mat-controller/src/session/mrp.rs crates/mat-controller/src/test_support.rs crates/mat-controller/tests/live_all_clusters.rs
git commit -m "refactor(mat-controller): secure_channel に SC 定数と StatusReport codec を一本化、Sigma3 後の truncated StatusReport を sigma3 段として報告"
```

---

### Task 2: `pase::derive_session_keys` becomes `pub`; HKDF-48 split shared with CASE

**Files:**
- Modify: `crates/mat-controller/src/secure_channel.rs` (add `session_keys_from_hkdf`)
- Modify: `crates/mat-controller/src/pase.rs:43, 658-669`
- Modify: `crates/mat-controller/src/case.rs:34, 326-341`
- Modify: `crates/mat-controller/src/test_support.rs:28, 42-43, 68-77, 471-474`

**Interfaces:**
- Produces:
  ```rust
  // secure_channel.rs
  /// HKDF-SHA256(salt, ikm, info="SessionKeys") expanded to 48 bytes and split
  /// into I2R / R2I / AttestationChallenge (spec §4.13.2.3 / §4.14.2.6).
  pub fn session_keys_from_hkdf(salt: &[u8], ikm: &[u8]) -> crate::session::SessionKeys;
  // pase.rs
  /// PASE session keys: HKDF(salt=[], ikm=Ke) (spec §4.13.2.3).
  pub fn derive_session_keys(k_e: &[u8; 16]) -> SessionKeys;
  ```
- `case::derive_session_keys(shared, ipk, transcript)` keeps its signature and calls `session_keys_from_hkdf(&salt, shared)`.

- [ ] **Step 1: Write the failing tests**

In `pase.rs` tests:
```rust
    /// PASE SessionKeys golden (HKDF-SHA256, salt=[], ikm=Ke, "SessionKeys").
    /// Computed once from the pre-refactor inline derivation; pins the
    /// `derive_session_keys` extraction byte-for-byte.
    #[test]
    fn golden_pase_session_keys_are_stable() {
        let k_e: [u8; 16] = core::array::from_fn(|i| 0x10 + i as u8);
        let keys = derive_session_keys(&k_e);
        // Expected values: fill in from the FIRST run of this test against
        // the inline derivation (see Step 2) — do NOT invent them.
        assert_eq!(keys.i2r, EXPECTED_I2R);
        assert_eq!(keys.r2i, EXPECTED_R2I);
        assert_eq!(keys.attestation_challenge, EXPECTED_AC);
    }
```
To obtain the golden values without guessing: temporarily add a test that computes the 48-byte OKM with the existing inline code (`hkdf::Hkdf::<sha2::Sha256>::new(Some(&[]), &k_e)` + `expand(b"SessionKeys", &mut okm)`), prints it with `eprintln!("{okm:02x?}")`, run it with `-- --nocapture`, paste the three 16-byte slices into the constants, then delete the printing test. The CASE golden in `case.rs` (`golden_session_keys_are_stable`) already pins the CASE path.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mat-controller --lib pase::tests::golden_pase 2>&1 | tail`
Expected: compile error, `derive_session_keys` not found in `pase`.

- [ ] **Step 3: Implement**

`secure_channel.rs`:
```rust
const INFO_SESSION_KEYS: &[u8] = b"SessionKeys";

pub fn session_keys_from_hkdf(salt: &[u8], ikm: &[u8]) -> crate::session::SessionKeys {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), ikm);
    let mut okm = [0u8; 48];
    hk.expand(INFO_SESSION_KEYS, &mut okm).expect("valid length");
    crate::session::SessionKeys {
        i2r: okm[..16].try_into().expect("16"),
        r2i: okm[16..32].try_into().expect("16"),
        attestation_challenge: okm[32..].try_into().expect("16"),
    }
}
```
`pase.rs`: delete `const INFO_SESSION_KEYS` (line 43); replace the inline block at 658-669 with
```rust
    // 6. Session keys (spec §4.13.2.3) — ikm is Ke (16 B, TT hash's second
    // half), not the full SPAKE2+ shared secret.
    let keys = derive_session_keys(&shared.k_e);
```
and add, next to `pake_context`:
```rust
/// PASE session keys: `HKDF-SHA256(salt=[], ikm=Ke, info="SessionKeys")`
/// split into I2R / R2I / AttestationChallenge (spec §4.13.2.3). `pub` so
/// the responder role (`test_support::pase_responder_task`, mat-device's
/// `core::pase`) derives the same keys through the same code.
pub fn derive_session_keys(k_e: &[u8; 16]) -> SessionKeys {
    crate::secure_channel::session_keys_from_hkdf(&[], k_e)
}
```
`case.rs`: delete `const INFO_SESSION_KEYS` (line 34); body of `derive_session_keys` becomes salt build + `crate::secure_channel::session_keys_from_hkdf(&salt, shared)`.
`test_support.rs`: delete `hkdf48`, `INFO_SESSION_KEYS`, the `use sha2::Sha256;` (if unused afterwards); lines 471-474 become
```rust
    let keys = pase::derive_session_keys(&k_e);
    let i2r = keys.i2r;
    let r2i = keys.r2i;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p mat-controller 2>&1 | tail -20` — expected green, including both goldens and `pase_self_handshake`.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/secure_channel.rs crates/mat-controller/src/pase.rs crates/mat-controller/src/case.rs crates/mat-controller/src/test_support.rs
git commit -m "refactor(mat-controller): pase::derive_session_keys を pub 化し HKDF-48 分割を secure_channel::session_keys_from_hkdf に集約"
```

---

### Task 3: Tier 1 dead code

**Files:**
- Modify: `crates/mat-controller/src/exchange.rs:386-388, 398-401, 411, 415-421` (`first_needs_ack`)
- Modify: `crates/mat-controller/src/spake2p.rs:56-58, 87-89, 100-102, 120-123, 151-153, 164-166, 178-180, 188-190, 222, 235, 363`
- Modify: `crates/mat-controller/src/im/mod.rs:43, 181, 189, 412-428, 480-495`
- Modify: `crates/mat-controller/src/im/write.rs:6-7, 66-75, 293-309`
- Modify: `crates/mat-controller/src/btp.rs:143, 180, 327, 382, 593, 755, 778, 797, 885`
- Modify: `crates/mat-controller/tests/btp_pase_plumbing.rs` (no change needed — it never reads `pkt.seq`; verify)

**Interfaces:**
- `btp::Packet.seq` type changes from `Option<u8>` to `u8` (every data/ack packet is sequenced, spec §4.19.3.5 — `Packet::decode` already always sets `Some`). Verified: no code outside `btp.rs` reads `.seq`. **Record in DONE as an API-shape change (no behaviour change).**
- Removed public items (verified unused workspace-wide): `im::ATTR_COLOR_TEMPERATURE_MIREDS`, `im::EVENT_SWITCH_SWITCH_LATCHED`, `im::SWITCH_FEATURE_LATCHING`, `im::encode_write_request`, `ResponderExchange::first_needs_ack`. **Record in DONE.**

- [ ] **Step 1: Confirm dead-ness again (line numbers may have shifted since the audit)**

Run: `/usr/bin/grep -rn -E "first_needs_ack|encode_write_request\b|encode_im_value|ATTR_COLOR_TEMPERATURE_MIREDS|EVENT_SWITCH_SWITCH_LATCHED|SWITCH_FEATURE_LATCHING|\.seq\b" crates --include='*.rs' | /usr/bin/grep -v -E "crates/mat-controller/src/(btp|exchange|spake2p)\.rs|crates/mat-controller/src/im/(mod|write)\.rs"`
Expected: no output (only definitions/uses inside the files being edited).

- [ ] **Step 2: exchange.rs** — remove the `first_needs_ack` field, its computation in `adopt`, and the accessor; delete the assertion `assert_eq!(re.first_needs_ack(), Some(500));` in the test `reply_reliable_dedupes_replay_then_retransmits_until_real_message`.

- [ ] **Step 3: spake2p.rs** — change `pub(crate) fn` → `fn` for `scalar_from_be_bytes_mod_n`, `decode_point`, `encode_point`, `build_transcript`, `split_hash`, `confirmation_keys`, `hmac32`, `random_scalar`, `Spake2pProver::new_with_x`, `Spake2pProver::transcript`, and `Spake2pVerifier::w0_l_bytes` (this one is `#[cfg(test)]`, keep the cfg). Delete every doc line reading `` `pub(crate)`: also used by `test_support`'s PASE verifier responder (audit Tier 5) … `` (the responder uses `Spake2pVerifier` and never these helpers). Keep the `#[allow(clippy::too_many_arguments)]` on `build_transcript`. `derive_w0_w1` and `compute_verifier` stay `pub`.

- [ ] **Step 4: im/mod.rs + im/write.rs** — delete the three constants; delete `encode_im_value` and the doc comment above it; delete `encode_write_request` in `write.rs` and fix its `use super::{...}` (drop `encode_im_value`, `ImValue`); replace the test `write_request_roundtrip_scalar` with the same assertions built on `encode_write_request_tlv`:
```rust
    #[test]
    fn write_request_roundtrip_scalar() {
        let mut dw = Writer::new();
        dw.put_uint(Tag::Anonymous, 128);
        let b = encode_write_request_tlv(1, 0x0008, 0x0011, &dw.finish());
        // 形の検証: WriteRequests(2) 配列の中に AttributeDataIB があり、
        // path(ep=1, cluster=8, attr=0x11) と Data(Context2)=128 を含む。
        let mut r = Reader::new(&b);
        let (mut saw_ep, mut saw_data) = (false, false);
        while let Some(el) = r.next().unwrap() {
            if el.tag == Tag::Context(2) && el.value == Value::Uint(128) {
                saw_data = true;
            }
            if el.tag == Tag::Context(2) && el.value == Value::Uint(1) {
                saw_ep = true;
            }
        }
        assert!(saw_ep && saw_data);
    }
```
Rewrite `im/mod.rs` test `im_value_floats_roundtrip_through_encode_and_decode` to encode with `Writer` directly:
```rust
    #[test]
    fn im_value_floats_roundtrip_through_encode_and_decode() {
        let mut w32 = Writer::new();
        w32.put_f32(Tag::Anonymous, 1.5);
        let mut w64 = Writer::new();
        w64.put_f64(Tag::Anonymous, -2.25);
        for (tlv, v, expect) in [
            (w32.finish(), ImValue::F32(1.5), 0x0A),
            (w64.finish(), ImValue::F64(-2.25), 0x0B),
        ] {
            // 要素型: single = 0x0A, double = 0x0B（anonymous tag → control byte だけ）。
            assert_eq!(tlv[0] & 0x1F, expect, "{v:?}");
            let mut r = Reader::new(&tlv);
            let el = r.next().unwrap().unwrap();
            assert_eq!(value_to_im(el.value).unwrap(), v);
        }
    }
```
If `Writer` is then unused in `im/mod.rs` non-test code, drop it from the `use crate::tlv::{...}` line (keep it in the test's imports via `use super::*` — it re-imports through `crate::tlv`; add `use crate::tlv::Writer;` inside the test module if needed).

- [ ] **Step 5: btp.rs** — `pub seq: u8`; in `Packet::decode` `seq,` instead of `seq: Some(seq),`; in `process_incoming` replace `if let Some(s) = pkt.seq {` with `let s = pkt.seq;` (de-indent the block by one level, keep every statement); the `tracing::debug!(... seq = ?pkt.seq ...)` becomes `seq = pkt.seq`; tests: `assert_eq!(pkt.seq, 0)`, struct literals `seq: 1`, `seq: s`; `FakePeripheral::recv_message` uses `let seq = pkt.seq;`.

- [ ] **Step 6: Run tests + clippy**

Run: `cargo test -p mat-controller 2>&1 | tail -20 && cargo clippy -p mat-controller --all-targets --features test-responder -- -D warnings 2>&1 | tail -5`
Expected: green, no warnings (dead-code warnings would surface here if a now-private spake2p helper lost its last caller — none should).

- [ ] **Step 7: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/exchange.rs crates/mat-controller/src/spake2p.rs crates/mat-controller/src/im/mod.rs crates/mat-controller/src/im/write.rs crates/mat-controller/src/btp.rs
git commit -m "refactor(mat-controller): Tier 1 死コード削除（first_needs_ack・spake2p の pub(crate)・im 未使用定数・encode_write_request/encode_im_value・btp Packet.seq を u8 に）"
```

---

### Task 4: `ExchangeCore { role }` behind `UnsecuredExchange` / `ResponderExchange`, generic MRP loops

**Files:**
- Modify: `crates/mat-controller/src/exchange.rs:121-703` (everything from `IncomingMessage` to the end of `impl ResponderExchange`), plus its tests where they touch private fields/methods (`re.screen(...)` calls stay valid via a private delegate).

**Interfaces:**
- Produces (crate-private, used by Task 5's `session/*`):
  ```rust
  /// Screening outcome for one datagram inside `mrp_send_loop` / `recv_until`.
  pub(crate) enum Verdict<T> { Ignore, Done(T) }

  /// What the generic MRP loops need from an endpoint.
  pub(crate) trait MrpEndpoint {
      type Error: From<std::io::Error>;
      fn transport(&self) -> &Transport;
      fn peer(&self) -> SocketAddr;
      fn last_rx(&self) -> Option<Instant>;
      fn timeout_error() -> Self::Error;
  }

  /// Sends `datagram`, then retransmits per `cfg` until `on_datagram` returns `Done`.
  pub(crate) async fn mrp_send_loop<E: MrpEndpoint, T>(
      ep: &mut E, datagram: &[u8], cfg: &MrpConfig,
      on_datagram: impl AsyncFnMut(&mut E, &[u8], SocketAddr) -> Result<Verdict<T>, E::Error>,
  ) -> Result<T, E::Error>;

  /// Receives until `on_datagram` returns `Done` or `timeout` elapses (then `on_timeout()`).
  pub(crate) async fn recv_until<E: MrpEndpoint, T>(
      ep: &mut E, timeout: Duration, on_timeout: impl Fn() -> E::Error,
      on_datagram: impl AsyncFnMut(&mut E, &[u8], SocketAddr) -> Result<Verdict<T>, E::Error>,
  ) -> Result<T, E::Error>;

  /// `true` when `msg` is an MRP standalone ack (SecureChannel 0x10).
  pub(crate) fn is_standalone_ack(proto: &ProtocolHeader) -> bool;
  ```
- Public surface of `UnsecuredExchange` / `ResponderExchange` is unchanged (see Global Constraints).

- [ ] **Step 1: No new tests needed for the public behaviour — the existing 20 exchange tests pin it.** Add one small test for the generic loop so a future edit of `mrp_send_loop` is caught independently of the exchange types:

```rust
    /// `mrp_send_loop` retransmits the same datagram `max_retries` + 1 times
    /// and returns the endpoint's timeout error when nothing arrives.
    #[tokio::test]
    async fn mrp_send_loop_retransmits_then_times_out() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);
        let cfg = fast_cfg(); // max_retries 2 → 3 sends
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = counter.clone();
        let responder_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            while tokio::time::timeout(Duration::from_millis(300), responder.recv_from(&mut buf))
                .await
                .is_ok()
            {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let err = mrp_send_loop(&mut ex.0, b"payload", &cfg, async |_ex: &mut ExchangeCore<'_>, _buf: &[u8], _from: SocketAddr| {
            Ok::<Verdict<()>, ExchangeError>(Verdict::Ignore)
        })
        .await
        .unwrap_err();
        assert!(matches!(err, ExchangeError::Timeout));
        responder_task.await.unwrap();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
```
(`ex.0` is the newtype's inner `ExchangeCore` — tests are in the same module so the private field is reachable.)

- [ ] **Step 2: Run it to see it fail to compile** (`mrp_send_loop`, `ExchangeCore`, `Verdict` unknown).

- [ ] **Step 3: Implement the generic loops and `ExchangeCore`** — replace old lines 128-703 with the following (keep `MrpConfig`, `retrans_base`, `total_budget`, `ExchangeError`, `IncomingMessage` as they are):

```rust
/// `true` for an MRP standalone ack (SecureChannel `0x10`), which carries no
/// payload and is never a "real" reply.
pub(crate) fn is_standalone_ack(proto: &ProtocolHeader) -> bool {
    proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL && proto.opcode == OPCODE_MRP_STANDALONE_ACK
}

/// Screening outcome for one received datagram inside [`mrp_send_loop`] /
/// [`recv_until`]: keep waiting, or finish with a value.
pub(crate) enum Verdict<T> {
    Ignore,
    Done(T),
}

/// What the generic MRP loops need from an endpoint — implemented by
/// [`ExchangeCore`] (unsecured) and `session::SecureSession` (secured). The
/// per-call screening logic stays with the caller (closure), so the loops
/// only own the retransmit schedule / deadline arithmetic.
pub(crate) trait MrpEndpoint {
    type Error: From<std::io::Error>;
    fn transport(&self) -> &Transport;
    fn peer(&self) -> SocketAddr;
    /// Time of the last valid message from the peer (spec 4.12.8 active/idle).
    fn last_rx(&self) -> Option<Instant>;
    /// The endpoint's "MRP retry budget exhausted" error.
    fn timeout_error() -> Self::Error;
}

/// MRP retransmission loop (spec §4.12): sends `datagram` once, then again
/// after each (jittered, backed-off) interval until `on_datagram` reports
/// `Done` or `max_retries` retransmissions have gone unanswered
/// (`E::timeout_error()`). `on_datagram` is called for every datagram read
/// off the socket and is expected to do the screening (decode / dedup /
/// ack) itself.
pub(crate) async fn mrp_send_loop<E: MrpEndpoint, T>(
    ep: &mut E,
    datagram: &[u8],
    cfg: &MrpConfig,
    mut on_datagram: impl AsyncFnMut(&mut E, &[u8], SocketAddr) -> Result<Verdict<T>, E::Error>,
) -> Result<T, E::Error> {
    let mut interval = retrans_base(ep.last_rx(), cfg);
    let mut attempts = 0u32;
    loop {
        ep.transport().send_to(datagram, ep.peer()).await?;
        let deadline = Instant::now() + jittered_interval(interval, cfg.jitter, unit_random());
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(recv) =
                tokio::time::timeout(remaining, ep.transport().recv_from(&mut buf)).await
            else {
                break; // interval 経過 → 再送
            };
            let (n, from) = recv?;
            if let Verdict::Done(v) = on_datagram(ep, &buf[..n], from).await? {
                return Ok(v);
            }
        }
        attempts += 1;
        if attempts > cfg.max_retries {
            return Err(E::timeout_error());
        }
        interval = interval.mul_f64(cfg.backoff);
    }
}

/// Receive loop with a fixed deadline: reads datagrams until `on_datagram`
/// reports `Done`; returns `on_timeout()` once `timeout` has elapsed.
pub(crate) async fn recv_until<E: MrpEndpoint, T>(
    ep: &mut E,
    timeout: Duration,
    on_timeout: impl Fn() -> E::Error,
    mut on_datagram: impl AsyncFnMut(&mut E, &[u8], SocketAddr) -> Result<Verdict<T>, E::Error>,
) -> Result<T, E::Error> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(on_timeout());
        }
        let mut buf = [0u8; MAX_DATAGRAM];
        let Ok(recv) = tokio::time::timeout(remaining, ep.transport().recv_from(&mut buf)).await
        else {
            return Err(on_timeout());
        };
        let (n, from) = recv?;
        if let Verdict::Done(v) = on_datagram(ep, &buf[..n], from).await? {
            return Ok(v);
        }
    }
}

/// Which side of the unsecured exchange we are (spec §4.6.1 / §4.4.1.2).
/// The role decides three things: the `I` flag on what we send, which
/// `I` flag we accept on what we receive, and where the initiator's
/// ephemeral node id is carried (initiator: `source`; responder:
/// `destination`).
enum Role {
    /// We opened the exchange (`UnsecuredExchange::new`).
    Initiator { source_node_id: u64 },
    /// The peer opened it (`ResponderExchange::adopt`).
    Responder {
        /// initiator が名乗った ephemeral node id（unsecured セッションの
        /// 最初のメッセージの source node id）。応答では **destination** に
        /// 載せ替える（spec §4.4.1.2 / §4.6.1.5）。`build` の doc 参照。
        /// `None` は相手が source を載せていない場合 — 両方省略する。
        peer_ephemeral_node_id: Option<u64>,
        /// 直近に受理した peer メッセージの counter。応答の ack piggyback に使う。
        last_peer_counter: u32,
    },
}

/// One unsecured (session id 0) exchange with MRP, either role. The public
/// API is the two newtypes below; this is the single implementation.
struct ExchangeCore<'t> {
    transport: &'t Transport,
    peer: SocketAddr,
    exchange_id: u16,
    counter: TxCounter,
    rx_window: RxWindow,
    last_sent_counter: Option<u32>,
    /// ピアから最後に有効なメッセージを受けた時刻（MRP active/idle 判定用）。
    last_rx: Option<Instant>,
    role: Role,
}

impl MrpEndpoint for ExchangeCore<'_> {
    type Error = ExchangeError;
    fn transport(&self) -> &Transport {
        self.transport
    }
    fn peer(&self) -> SocketAddr {
        self.peer
    }
    fn last_rx(&self) -> Option<Instant> {
        self.last_rx
    }
    fn timeout_error() -> ExchangeError {
        ExchangeError::Timeout
    }
}

impl<'t> ExchangeCore<'t> {
    fn is_initiator(&self) -> bool {
        matches!(self.role, Role::Initiator { .. })
    }

    /// ack to piggyback on our next send: the responder always acks the
    /// latest accepted peer message; the initiator piggybacks nothing.
    fn piggyback_ack(&self) -> Option<u32> {
        match self.role {
            Role::Initiator { .. } => None,
            Role::Responder {
                last_peer_counter, ..
            } => Some(last_peer_counter),
        }
    }

    /// unsecured セッションのメッセージを組む。
    ///
    /// **ヘッダのアドレス指定**: unsecured セッションでは initiator の
    /// ephemeral node id をメッセージ 1 通につきちょうど 1 箇所に載せる
    /// （spec §4.4.1.2 / §4.6.1.5）。initiator は source に、responder は
    /// destination に載せる。両方載せる／両方省くのはプロトコル違反で、
    /// 参照実装（chip の `SessionManager::UnauthenticatedMessageDispatch`）
    /// はその場でデータグラムを捨てる — 応答が「届いているのに無かったこと
    /// にされる」ので、症状は上位プロトコルのエラーではなく無言のタイムアウト
    /// になる。M2 ゲート 1 で実際にこれを踏んだ（`docs/superpowers/plans/
    /// m2-chip-tool-probe.md`）。
    fn build(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        needs_ack: bool,
        acked_counter: Option<u32>,
        payload: &[u8],
    ) -> (Vec<u8>, u32) {
        let needs_ack = needs_ack && !self.transport.is_reliable();
        let message_counter = self.counter.next();
        let (source_node_id, destination, initiator) = match self.role {
            Role::Initiator { source_node_id } => (Some(source_node_id), Destination::None, true),
            Role::Responder {
                peer_ephemeral_node_id,
                ..
            } => (
                None,
                peer_ephemeral_node_id.map_or(Destination::None, Destination::Node),
                false,
            ),
        };
        let header = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter,
            source_node_id,
            destination,
        };
        let proto = ProtocolHeader {
            initiator,
            needs_ack,
            acked_counter,
            opcode,
            exchange_id: self.exchange_id,
            protocol_id,
            vendor_id: None,
        };
        let mut buf = header.encoded();
        proto.encode(&mut buf);
        buf.extend_from_slice(payload);
        (buf, message_counter)
    }

    async fn send_standalone_ack(&mut self, acked: u32) -> Result<(), ExchangeError> {
        let (buf, _) = self.build(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_MRP_STANDALONE_ACK,
            false,
            Some(acked),
            &[],
        );
        self.transport.send_to(&buf, self.peer).await?;
        Ok(())
    }

    /// Decodes a datagram and screens it for this exchange. Returns `None`
    /// for foreign or duplicate traffic the caller should skip (duplicates
    /// are re-acked here). Traffic with our own role's `I` flag (an
    /// initiator seeing `initiator: true`, a responder seeing `false`) is
    /// stray/spoofed and dropped. Standalone acks pass screening and are
    /// returned as `Some`; callers filter them by opcode.
    async fn screen(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        if from != self.peer {
            return Ok(None);
        }
        let (header, off) = match MessageHeader::decode(buf) {
            Ok(v) => v,
            Err(_) => return Ok(None), // 不正データグラムは無視（DoS 耐性）
        };
        if header.session_id != 0 || header.security_flags != 0 {
            return Ok(None);
        }
        let (proto, body_off) = match ProtocolHeader::decode(&buf[off..]) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if proto.exchange_id != self.exchange_id || proto.initiator == self.is_initiator() {
            return Ok(None);
        }
        // ここまで来た = このピアからの当該 exchange の有効トラフィック。
        // MRP active/idle 判定の材料として受信時刻を記録する（重複でも良い —
        // ピアが生きて送っている事実に変わりない）。
        self.last_rx = Some(Instant::now());
        if !self.rx_window.check_and_commit(header.message_counter) {
            if proto.needs_ack && !self.transport.is_reliable() {
                self.send_standalone_ack(header.message_counter).await?;
            }
            return Ok(None);
        }
        if let Role::Responder {
            last_peer_counter, ..
        } = &mut self.role
        {
            *last_peer_counter = header.message_counter;
        }
        if proto.needs_ack && !self.transport.is_reliable() {
            self.send_standalone_ack(header.message_counter).await?;
        }
        Ok(Some(IncomingMessage {
            header,
            proto,
            payload: buf[off + body_off..].to_vec(),
        }))
    }

    /// Sends a reliability-flagged message and retransmits until the peer
    /// acknowledges it. Returns the peer's real response if one carried the
    /// ack (or arrived on the exchange), `None` for a standalone ack. The
    /// responder role piggybacks the ack for the peer's latest message.
    async fn send_reliable(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        let ack = self.piggyback_ack();
        if self.transport.is_reliable() {
            // BTP: transport が信頼性を持つ。1 回送って実応答を待つだけ。
            let (datagram, our_counter) = self.build(protocol_id, opcode, false, ack, payload);
            self.last_sent_counter = Some(our_counter);
            self.transport.send_to(&datagram, self.peer).await?;
            let budget = total_budget(cfg);
            return self.recv(budget).await.map(Some);
        }
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, ack, payload);
        self.last_sent_counter = Some(our_counter);
        mrp_send_loop(self, &datagram, cfg, async |ex: &mut Self, buf: &[u8], from: SocketAddr| {
            let Some(msg) = ex.screen(buf, from).await? else {
                // ack-only の可能性: screen は standalone ack も Some で返す
                return Ok(Verdict::Ignore);
            };
            if is_standalone_ack(&msg.proto) {
                return Ok(if msg.proto.acked_counter == Some(our_counter) {
                    Verdict::Done(None)
                } else {
                    Verdict::Ignore
                });
            }
            // exchange 上の実メッセージは応答とみなす（相手が処理した証拠）
            Ok(Verdict::Done(Some(msg)))
        })
        .await
    }

    /// Sends a reliability-flagged message exactly once and returns without
    /// waiting for an ack (see `UnsecuredExchange::send_once`).
    async fn send_once(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
    ) -> Result<(), ExchangeError> {
        let ack = self.piggyback_ack();
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, ack, payload);
        self.last_sent_counter = Some(our_counter);
        self.transport.send_to(&datagram, self.peer).await?;
        Ok(())
    }

    /// Sends a final message and waits only for its ack (standalone or
    /// piggybacked). On a reliable transport there is no MRP, so it returns
    /// right after the send — unlike `send_reliable`, which waits for the
    /// peer's *real* reply on both transports; a "final" message expects no
    /// reply, so on BTP there is nothing to wait for (the asymmetry is
    /// intentional).
    async fn send_final(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<(), ExchangeError> {
        let ack = self.piggyback_ack();
        if self.transport.is_reliable() {
            let (datagram, _) = self.build(protocol_id, opcode, false, ack, payload);
            self.transport.send_to(&datagram, self.peer).await?;
            return Ok(());
        }
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, ack, payload);
        mrp_send_loop(self, &datagram, cfg, async |ex: &mut Self, buf: &[u8], from: SocketAddr| {
            let Some(msg) = ex.screen(buf, from).await? else {
                return Ok(Verdict::Ignore);
            };
            Ok(if msg.proto.acked_counter == Some(our_counter) {
                Verdict::Done(())
            } else {
                Verdict::Ignore
            })
        })
        .await
    }

    /// Waits for the next real (non-ack) message on this exchange.
    async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        recv_until(self, timeout, || ExchangeError::Timeout, async |ex: &mut Self, buf: &[u8], from: SocketAddr| {
            let Some(msg) = ex.screen(buf, from).await? else {
                return Ok(Verdict::Ignore);
            };
            Ok(if is_standalone_ack(&msg.proto) {
                Verdict::Ignore
            } else {
                Verdict::Done(msg)
            })
        })
        .await
    }
}

/// One unsecured (session id 0) exchange, this side as initiator, with MRP.
pub struct UnsecuredExchange<'t>(ExchangeCore<'t>);

impl<'t> UnsecuredExchange<'t> {
    pub fn new(transport: &'t Transport, peer: SocketAddr) -> Self {
        let mut b = [0u8; 10];
        getrandom::fill(&mut b).expect("os rng");
        Self(ExchangeCore {
            transport,
            peer,
            exchange_id: u16::from_le_bytes([b[0], b[1]]),
            counter: TxCounter::new_random(),
            rx_window: RxWindow::new(),
            last_sent_counter: None,
            last_rx: None,
            role: Role::Initiator {
                source_node_id: u64::from_le_bytes(b[2..10].try_into().expect("8 bytes")),
            },
        })
    }

    pub fn exchange_id(&self) -> u16 {
        self.0.exchange_id
    }

    /// The message counter used by the most recent `send_reliable` /
    /// `send_once` call, if any.
    pub fn last_sent_counter(&self) -> Option<u32> {
        self.0.last_sent_counter
    }

    /// Sends a reliability-flagged message and retransmits until the peer
    /// acknowledges it. Returns the peer's real response if one carried the
    /// ack (or arrived on the exchange), `None` for a standalone ack.
    pub async fn send_reliable(&mut self, protocol_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig) -> Result<Option<IncomingMessage>, ExchangeError> {
        self.0.send_reliable(protocol_id, opcode, payload, cfg).await
    }

    /// Sends a reliability-flagged message exactly once and returns
    /// immediately, without waiting for (or retransmitting on a missing)
    /// acknowledgement. The R flag is still set, so the peer's own MRP layer
    /// tracks and acks it normally — only *our* wait/retry loop is skipped.
    /// For genuine fire-and-forget sends where the caller cannot afford
    /// `send_reliable`'s worst-case retry budget (e.g. an abort notification
    /// sent while already unwinding to an error).
    pub async fn send_once(&mut self, protocol_id: u16, opcode: u8, payload: &[u8]) -> Result<(), ExchangeError> {
        self.0.send_once(protocol_id, opcode, payload).await
    }

    /// Waits for the next real (non-ack) message on this exchange.
    pub async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        self.0.recv(timeout).await
    }
}

/// peer が開始した unsecured exchange の応答側。PASE/CASE の全フローを 1
/// exchange で捌く（spec §4.6, §4.12）。`UnsecuredExchange` の鏡像 —
/// あちらは自分から exchange を開く初期側、こちらは peer から届いた最初の
/// メッセージ（`adopt`）から採番を引き継いで応答する側。実装は共通の
/// `ExchangeCore`（役割だけが違う）。
pub struct ResponderExchange<'t>(ExchangeCore<'t>);

impl<'t> ResponderExchange<'t> {
    /// 受信済みの最初の peer-initiated メッセージから採番を引き継いで作る。
    /// `first` の counter は即座に rx_window へコミットする — 再送されて
    /// きた同一メッセージは `screen` の重複判定に落ちて standalone-ack のみ
    /// 返す。
    pub fn adopt(transport: &'t Transport, peer: SocketAddr, first: &IncomingMessage) -> Self {
        let mut rx_window = RxWindow::new();
        rx_window.check_and_commit(first.header.message_counter);
        Self(ExchangeCore {
            transport,
            peer,
            exchange_id: first.proto.exchange_id,
            counter: TxCounter::new_random(),
            rx_window,
            last_sent_counter: None,
            last_rx: Some(Instant::now()),
            role: Role::Responder {
                peer_ephemeral_node_id: first.header.source_node_id,
                last_peer_counter: first.header.message_counter,
            },
        })
    }

    /// Waits for the next real (non-ack) peer message on this exchange.
    /// (Keep the existing long doc comment about why re-`adopt`ing per
    /// message is not a substitute — copy it verbatim from the old code.)
    pub async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        self.0.recv(timeout).await
    }

    /// initiator:false で応答し、同一 exchange の次の peer メッセージを待つ。
    /// (Keep the existing doc comment verbatim.)
    pub async fn reply_reliable(&mut self, protocol_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig) -> Result<Option<IncomingMessage>, ExchangeError> {
        self.0.send_reliable(protocol_id, opcode, payload, cfg).await
    }

    /// 応答して待たない（StatusReport 終端用）。needs_ack を立て、ack
    /// （standalone または piggyback、どちらも `acked_counter` が我々の
    /// counter と一致していること）を受け取るまで MRP 再送する。Reliable
    /// transport では 1 回送って即 return（`ExchangeCore::send_final` 参照）。
    pub async fn reply_final(&mut self, protocol_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig) -> Result<(), ExchangeError> {
        self.0.send_final(protocol_id, opcode, payload, cfg).await
    }

    /// Test hook: screen one datagram through the shared core.
    #[cfg(test)]
    async fn screen(&mut self, buf: &[u8], from: SocketAddr) -> Result<Option<IncomingMessage>, ExchangeError> {
        self.0.screen(buf, from).await
    }
}
```

Behavioural equivalences to double-check while porting (they are all preserved by the code above; re-read the old code side by side):
1. Initiator `send_reliable` used `acked_counter: None`; responder used `Some(last_peer_counter)` → `piggyback_ack()`.
2. Responder `reply_reliable`/`reply_final` never set `last_sent_counter`; the core now sets it in both roles — it is only readable through `UnsecuredExchange::last_sent_counter`, so no observable change.
3. Initiator `screen` dropped `proto.initiator == true`; responder dropped `false` → `proto.initiator == self.is_initiator()`.
4. Responder `screen` updated `last_peer_counter` **after** the dedup check and **before** the ack send — same order in the core.
5. `reply_final` on the reliable transport returns immediately (documented in `send_final`'s doc — this closes audit bug-candidate "BTP branch asymmetry" with a comment, no behaviour change).
6. `UnsecuredExchange::send_once` previously had no piggyback (initiator → `None`); unchanged.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mat-controller 2>&1 | tail -30`
Expected: every `exchange::tests::*` test passes, plus `case_self_handshake` / `pase_self_handshake` / `btp_pase_plumbing`. If the `async |ex: &mut Self, ...|` closures fail to type-check, spell `Self` as `ExchangeCore<'t>`.

- [ ] **Step 5: Clippy + fmt, commit**

```bash
cargo fmt -p mat-controller
cargo clippy -p mat-controller --all-targets --features test-responder -- -D warnings
git add crates/mat-controller/src/exchange.rs
git commit -m "refactor(mat-controller): UnsecuredExchange/ResponderExchange を ExchangeCore{role} の newtype に統合、MRP 再送/受信ループを mrp_send_loop/recv_until に一本化"
```

---

### Task 5: `SecureSession` MRP loops on the generic helpers (`session/mrp.rs`, `responder.rs`, `subscribe.rs`)

**Files:**
- Modify: `crates/mat-controller/src/session/mod.rs` (impl `MrpEndpoint for SecureSession`)
- Modify: `crates/mat-controller/src/session/mrp.rs:295-392` (`send_reliable`, `recv`)
- Modify: `crates/mat-controller/src/session/responder.rs:21-103, 145-178, 223-302` (`respond_status`, `recv_request`, `reply_reliable`)
- Modify: `crates/mat-controller/src/session/subscribe.rs:232-262` (pump receive loop)

**Interfaces:**
- Consumes: `crate::exchange::{mrp_send_loop, recv_until, MrpEndpoint, Verdict, is_standalone_ack}` from Task 4.
- Produces: none new; public `SecureSession` methods unchanged.

Preserve these three documented deviations exactly (they are the reason the audit says "keep"):
- `responder.rs::reply_reliable` — when `screen_with` returns `None` (filter miss), still complete with `Ok(None)` if `self.last_peer_ack == Some(our_counter)` (cross-exchange piggyback ack). The existing test `reply_reliable_completes_via_cross_exchange_piggyback_ack` pins it.
- `responder.rs::respond_status` — while waiting for the ack, a `ReportData` (IM) message arriving on the peer's exchange is pushed to `peer_initiated` (with the `MAX_PEER_INITIATED_BUFFER` eviction + the exact `warn!` text `"peer-initiated report buffer full; dropping oldest"`) and the wait continues unless it also acks us. Pinned by `report_chunk_arriving_during_status_ack_wait_is_not_lost`.
- `subscribe.rs` pump — timeout error is `SessionError::Silence`, not `Timeout`, and a `debug!(len = n, %from, "sub pump: datagram received")` is logged for every datagram before screening. Pinned by `next_subscription_report_times_out_on_silence`.

- [ ] **Step 1: The existing session tests are the pins (30+ tests across `mrp.rs`, `responder.rs`, `subscribe.rs`, `client.rs`).** Run them once before editing to record the baseline: `cargo test -p mat-controller --lib session:: 2>&1 | tail -5`.

- [ ] **Step 2: `session/mod.rs`** — add after the `impl SecureSession { ... }` block:

```rust
impl crate::exchange::MrpEndpoint for SecureSession {
    type Error = SessionError;
    fn transport(&self) -> &Transport {
        &self.transport
    }
    fn peer(&self) -> SocketAddr {
        self.peer
    }
    fn last_rx(&self) -> Option<Instant> {
        self.last_rx
    }
    fn timeout_error() -> SessionError {
        SessionError::Timeout
    }
}
```

- [ ] **Step 3: `session/mrp.rs`** — replace the bodies of `send_reliable` (UDP branch) and `recv`:

```rust
        let (datagram, our_counter) =
            self.seal(exchange_id, true, protocol_id, opcode, true, None, payload)?;
        mrp_send_loop(self, &datagram, cfg, async |s: &mut Self, buf: &[u8], from: SocketAddr| {
            let Some(msg) = s.screen(buf, from, exchange_id).await? else {
                return Ok(Verdict::Ignore);
            };
            if is_standalone_ack(&msg.proto) {
                return Ok(if msg.proto.acked_counter == Some(our_counter) {
                    Verdict::Done(None)
                } else {
                    Verdict::Ignore
                });
            }
            Ok(Verdict::Done(Some(msg)))
        })
        .await
```
and
```rust
    pub async fn recv(&mut self, exchange_id: u16, timeout: Duration) -> Result<IncomingMessage, SessionError> {
        recv_until(self, timeout, || SessionError::Timeout, async |s: &mut Self, buf: &[u8], from: SocketAddr| {
            let Some(msg) = s.screen(buf, from, exchange_id).await? else {
                return Ok(Verdict::Ignore);
            };
            Ok(if is_standalone_ack(&msg.proto) { Verdict::Ignore } else { Verdict::Done(msg) })
        })
        .await
    }
```
Imports: `use crate::exchange::{is_standalone_ack, mrp_send_loop, recv_until, IncomingMessage, MrpConfig, Verdict};`. Remove now-unused `MAX_DATAGRAM` / `Instant` imports if the compiler says so (`Instant` is still used by `screen_with`).

- [ ] **Step 4: `session/responder.rs`**

`respond_status` UDP branch:
```rust
        let (datagram, our_counter) = self.seal(exchange_id, false, im::PROTOCOL_ID_IM, im::OPCODE_STATUS_RESPONSE, true, None, &payload)?;
        mrp_send_loop(self, &datagram, cfg, async |s: &mut Self, buf: &[u8], from: SocketAddr| {
            let Some(msg) = s
                .screen_with(buf, from, ScreenFilter::PeerExchange(exchange_id))
                .await?
            else {
                return Ok(Verdict::Ignore);
            };
            let acked = msg.proto.acked_counter == Some(our_counter);
            // ack 待ち中に届いた続きチャンク（device 発 ReportData）は
            // ack 照合の副産物として捨てない — screen_with のフィルタ落ち
            // 待避と同じ規律で peer_initiated へ積み、購読 API が消費する
            // （監査#1 経路B）。
            if msg.proto.protocol_id == im::PROTOCOL_ID_IM && msg.proto.opcode == im::OPCODE_REPORT_DATA {
                s.stash_peer_initiated(msg);
            }
            Ok(if acked { Verdict::Done(()) } else { Verdict::Ignore })
        })
        .await
```
Add to `session/mrp.rs` (next to `MAX_PEER_INITIATED_BUFFER`) the shared eviction helper and use it in `screen_with` too (identical wording):
```rust
    /// Buffers an already-acked peer-initiated message for the subscription /
    /// request APIs, evicting the oldest when full (identical policy in
    /// `screen_with`'s filter-miss path and `respond_status`'s ack wait).
    pub(super) fn stash_peer_initiated(&mut self, msg: IncomingMessage) {
        if self.peer_initiated.len() >= MAX_PEER_INITIATED_BUFFER {
            tracing::warn!("peer-initiated report buffer full; dropping oldest");
            self.peer_initiated.pop_front();
        }
        self.peer_initiated.push_back(msg);
    }
```

`recv_request`:
```rust
    pub async fn recv_request(&mut self, timeout: Duration) -> Result<IncomingMessage, SessionError> {
        if let Some(m) = self.peer_initiated.pop_front() {
            return Ok(m);
        }
        recv_until(self, timeout, || SessionError::Timeout, async |s: &mut Self, buf: &[u8], from: SocketAddr| {
            Ok(match s.deliver_request(buf, from).await? {
                Some(msg) => Verdict::Done(msg),
                None => Verdict::Ignore,
            })
        })
        .await
    }
```
(`deliver_request` = `screen_with(AnyPeerInitiated)` + standalone-ack filter, exactly what the old inline loop did.) Use `is_standalone_ack` inside `deliver_request` too.

`reply_reliable` UDP branch:
```rust
        let (datagram, our_counter) = self.seal(exchange_id, false, protocol_id, opcode, true, None, payload)?;
        mrp_send_loop(self, &datagram, cfg, async |s: &mut Self, buf: &[u8], from: SocketAddr| {
            let Some(msg) = s
                .screen_with(buf, from, ScreenFilter::PeerExchange(exchange_id))
                .await?
            else {
                // フィルタ落ち（別 exchange 宛など）でも、その datagram の
                // ack フィールドは screen_with が exchange 不問で
                // `last_peer_ack` に記録済み。real controller/commissioner
                // が standalone ack を送らず次のリクエストに我々への ack
                // を piggyback しただけ、というケースをここで拾う
                // （そのリクエスト自体は `peer_initiated` に待避済み —
                // 呼び出し元が drain する）。
                return Ok(if s.last_peer_ack == Some(our_counter) { Verdict::Done(None) } else { Verdict::Ignore });
            };
            let acked = msg.proto.acked_counter == Some(our_counter);
            if is_standalone_ack(&msg.proto) {
                return Ok(if acked { Verdict::Done(None) } else { Verdict::Ignore });
            }
            Ok(Verdict::Done(Some(msg)))
        })
        .await
```

- [ ] **Step 5: `session/subscribe.rs`** pump:
```rust
        let msg = if let Some(m) = self.peer_initiated.pop_front() {
            m
        } else {
            recv_until(self, timeout, || SessionError::Silence, async |s: &mut Self, buf: &[u8], from: SocketAddr| {
                tracing::debug!(len = buf.len(), %from, "sub pump: datagram received");
                let Some(m) = s.screen_with(buf, from, ScreenFilter::AnyPeerInitiated).await? else {
                    return Ok(Verdict::Ignore);
                };
                Ok(if is_standalone_ack(&m.proto) { Verdict::Ignore } else { Verdict::Done(m) })
            })
            .await?
        };
```

- [ ] **Step 6: Run tests, clippy**

Run: `cargo test -p mat-controller 2>&1 | tail -30 && cargo clippy -p mat-controller --all-targets --features test-responder -- -D warnings 2>&1 | tail -5`
Expected: green; unused imports (`MAX_DATAGRAM`, `OPCODE_MRP_STANDALONE_ACK`, `PROTOCOL_ID_SECURE_CHANNEL`, `Instant`) removed where clippy/rustc flag them.

- [ ] **Step 7: Verify the loop bodies are gone**

Run: `/usr/bin/grep -n "attempts += 1" crates/mat-controller/src/exchange.rs crates/mat-controller/src/session/*.rs`
Expected: exactly one hit, inside `mrp_send_loop`.

- [ ] **Step 8: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/session/
git commit -m "refactor(mat-controller): SecureSession の MRP 再送/受信ループ 5 本を exchange::mrp_send_loop/recv_until へ委譲（last_peer_ack 完了・ReportData 待避は保持）"
```

---

### Task 6: `case/wire.rs` — one CASE wire codec for initiator and responder

**Files:**
- Create: `crates/mat-controller/src/case/wire.rs`
- Modify: `crates/mat-controller/src/case.rs:30-33, 131-286, 386-437, 507-520, 543-570` (structs, parse/encode fns, constants, salt building)
- Modify: `crates/mat-controller/src/case_responder.rs:56-76, 267-308, 340-390, 404-632, 786-794, 926-949, 888-903` (codec, salts, doc fix)

**Interfaces:**
- Produces (`crate::case::wire`, `pub(crate)` module; `case.rs` re-exports what was public):
  ```rust
  pub(crate) const TBE2_NONCE: &[u8; 13] = b"NCASE_Sigma2N";
  pub(crate) const TBE3_NONCE: &[u8; 13] = b"NCASE_Sigma3N";
  pub(crate) const INFO_S2K: &[u8] = b"Sigma2";
  pub(crate) const INFO_S3K: &[u8] = b"Sigma3";

  pub struct Sigma1 { pub initiator_random: [u8; 32], pub initiator_session_id: u16, pub dest_id: [u8; 32], pub initiator_eph_pub: [u8; 65] }
  pub struct Sigma2 { pub responder_random: [u8; 32], pub responder_session_id: u16, pub responder_eph_pub: [u8; 65], pub encrypted2: Vec<u8> }
  pub struct Tbe { pub noc: Vec<u8>, pub icac: Option<Vec<u8>>, pub signature: [u8; 64] }

  pub fn encode_sigma1(random: &[u8; 32], session_id: u16, dest_id: &[u8; 32], eph_pub: &[u8; 65]) -> Vec<u8>;   // unchanged bytes
  pub(crate) fn parse_sigma1(payload: &[u8]) -> Result<Sigma1, &'static str>;
  pub(crate) fn encode_sigma2(random: &[u8; 32], session_id: u16, eph: &[u8; 65], encrypted2: &[u8]) -> Vec<u8>;
  pub(crate) fn parse_sigma2(payload: &[u8]) -> Result<Sigma2, &'static str>;   // keeps the "responder session id must be non-zero" check
  pub(crate) fn encode_sigma3(encrypted3: &[u8]) -> Vec<u8>;
  pub(crate) fn parse_sigma3(payload: &[u8]) -> Result<Vec<u8>, &'static str>;
  pub(crate) fn encode_tbs(noc: &[u8], icac: Option<&[u8]>, sender_eph: &[u8; 65], receiver_eph: &[u8; 65]) -> Vec<u8>;
  pub(crate) fn encode_tbe(noc: &[u8], icac: Option<&[u8]>, sig: &[u8; 64], resumption_id: Option<&[u8; 16]>) -> Vec<u8>;
  pub(crate) fn parse_tbe(payload: &[u8]) -> Result<Tbe, &'static str>;
  /// `None` = peer public key is not a valid P-256 point.
  pub(crate) fn ecdh(secret: &p256::SecretKey, peer_pub: &[u8; 65]) -> Option<[u8; 32]>;
  pub(crate) fn s2k_salt(ipk: &[u8; 16], responder_random: &[u8; 32], responder_eph_pub: &[u8; 65], sigma1_hash: &[u8; 32]) -> Vec<u8>;
  pub(crate) fn s3k_salt(ipk: &[u8; 16], sigma12_hash: &[u8; 32]) -> Vec<u8>;
  pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32];
  ```
- Error-label policy: `parse_*` return the **responder's** existing labels (`"sigma1 tlv"`, `"sigma1 truncated"`, `"tbe tlv"`, `"tbe top-level struct"`, `"tbe truncated"`, `"tbe signature length"`, `"tbe noc"`, `"tbe signature"`, `"sigma3 tlv"`, …). For Sigma2 (initiator-only parse) keep the initiator's labels (`"tlv"`, `"top-level struct"`, `"truncated"`, `"responder random length"`, `"responder session id"`, `"responder ephemeral key length"`, `"responder random"`, `"responder ephemeral key"`, `"encrypted2"`, `"responder session id must be non-zero"`). `case.rs` wraps with `CaseError::Sigma2Malformed(label)`, `case_responder.rs` with `CaseCoreError::Decode(label)`. **The only label deltas** are in `case.rs`'s TBE2 parse: `"tlv"`→`"tbe tlv"`, `"truncated tbe"`→`"tbe truncated"` (both remain `CaseError::Sigma2Malformed`). No test pins them (verified: `decrypts_and_parses_tbe2_roundtrip` asserts `Tbe2DecryptFailed` only). **Record in DONE.**
- ECDH error mapping: `case.rs` → `CaseError::Sigma2Malformed("responder ephemeral key")`, `case_responder.rs` → `CaseCoreError::Decode("initiator ephemeral key")` (both unchanged).
- `case::Sigma2`/`case::Tbe2` (`pub(crate)` structs) become type aliases or re-exports of `wire::Sigma2` / `wire::Tbe` (`pub(crate) use wire::{Sigma2, Tbe as Tbe2};`), `case::parse_sigma2` stays as a thin `pub(crate)` wrapper mapping the error to `Sigma2Malformed`, `case::decrypt_tbe2` unchanged in signature.
- `case::encode_sigma1` stays `pub` via `pub use wire::encode_sigma1;` (mat-device's tests import it from `case`).

- [ ] **Step 1: Write the failing tests** — in `case/wire.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn sigma1_roundtrip_and_ignores_nested_session_params() {
        let bytes = encode_sigma1(&[0x42; 32], 0x1234, &[0x24; 32], &[0x04; 65]);
        let s1 = parse_sigma1(&bytes).unwrap();
        assert_eq!(s1.initiator_random, [0x42; 32]);
        assert_eq!(s1.initiator_session_id, 0x1234);
        assert_eq!(s1.dest_id, [0x24; 32]);
        assert_eq!(s1.initiator_eph_pub, [0x04; 65]);
        // matter.js style: initiatorSessionParams (tag 5) must not clobber tag 2.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[0x42; 32]);
        w.put_uint(Tag::Context(2), 0x1234);
        w.put_bytes(Tag::Context(3), &[0x24; 32]);
        w.put_bytes(Tag::Context(4), &[0x04; 65]);
        w.start_struct(Tag::Context(5));
        w.put_uint(Tag::Context(2), 300);
        w.end_container();
        w.end_container();
        assert_eq!(parse_sigma1(&w.finish()).unwrap().initiator_session_id, 0x1234);
    }

    #[test]
    fn sigma2_roundtrip_rejects_zero_session_id() {
        let bytes = encode_sigma2(&[0x11; 32], 0x1234, &[0x22; 65], b"encrypted-blob");
        let s2 = parse_sigma2(&bytes).unwrap();
        assert_eq!(s2.responder_session_id, 0x1234);
        assert_eq!(s2.encrypted2, b"encrypted-blob");
        let zero = encode_sigma2(&[0x11; 32], 0, &[0x22; 65], b"x");
        assert_eq!(parse_sigma2(&zero), Err("responder session id must be non-zero"));
    }

    #[test]
    fn sigma3_roundtrip() {
        assert_eq!(parse_sigma3(&encode_sigma3(b"enc3")).unwrap(), b"enc3");
        assert_eq!(parse_sigma3(&[0x15, 0x18]), Err("sigma3 missing encrypted3"));
    }

    #[test]
    fn tbe_roundtrip_with_and_without_resumption_id() {
        let t = parse_tbe(&encode_tbe(b"noc", Some(b"icac"), &[0x77; 64], None)).unwrap();
        assert_eq!((t.noc.as_slice(), t.icac.as_deref(), t.signature), (&b"noc"[..], Some(&b"icac"[..]), [0x77; 64]));
        let with = encode_tbe(b"noc", None, &[0x77; 64], Some(&[0x88; 16]));
        let t = parse_tbe(&with).unwrap();
        assert_eq!(t.icac, None);
        // tag 4 is present on the wire
        let mut r = Reader::new(&with);
        let mut saw = false;
        while let Some(el) = r.next().unwrap() {
            if el.tag == Tag::Context(4) { assert_eq!(el.value, Value::Bytes(&[0x88; 16])); saw = true; }
        }
        assert!(saw);
    }

    #[test]
    fn tbs_puts_sender_before_receiver() {
        let b = encode_tbs(b"noc", None, &[0xAA; 65], &[0xBB; 65]);
        let mut r = Reader::new(&b);
        r.next().unwrap(); // struct
        r.next().unwrap(); // noc
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(3), Value::Bytes(&[0xAA; 65])));
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(4), Value::Bytes(&[0xBB; 65])));
    }

    #[test]
    fn ecdh_rejects_off_curve_point() {
        let sk = crate::case::random_p256_secret();
        assert!(ecdh(&sk, &[0x04; 65]).is_none());
        let peer = crate::case::random_p256_secret();
        let a = ecdh(&sk, &crate::case::eph_pub_bytes(&peer)).unwrap();
        let b = ecdh(&peer, &crate::case::eph_pub_bytes(&sk)).unwrap();
        assert_eq!(a, b);
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p mat-controller --lib case::wire 2>&1 | tail -5` → module not found.

- [ ] **Step 3: Implement `src/case/wire.rs`** by *moving* the bodies: `encode_sigma1` (case.rs 148-162), `parse_sigma1` (case_responder 419-477, returning `Sigma1` and `&'static str`), `encode_sigma2` (case_responder 481-490), `parse_sigma2` (case.rs 166-232 with `Sigma2Malformed(x)` → `x`), `encode_sigma3` (case.rs 431-437), `parse_sigma3` (case_responder 494-520), `parse_tbe` (case_responder 527-566, returning `Tbe { noc, icac, signature }`), `encode_tbs` (case.rs 398-414), `encode_tbe` (case_responder 597-615), `sha256` (case_responder 617-621), `ecdh` (case.rs 387-394 → `Option`), plus the four constants and the two salt builders:

```rust
/// S2K salt (spec §4.14.2.2): `IPK || responderRandom || responderEphPubKey || SHA256(Sigma1)`.
pub(crate) fn s2k_salt(ipk: &[u8; 16], responder_random: &[u8; 32], responder_eph_pub: &[u8; 65], sigma1_hash: &[u8; 32]) -> Vec<u8> {
    let mut salt = Vec::with_capacity(16 + 32 + 65 + 32);
    salt.extend_from_slice(ipk);
    salt.extend_from_slice(responder_random);
    salt.extend_from_slice(responder_eph_pub);
    salt.extend_from_slice(sigma1_hash);
    salt
}

/// S3K salt (spec §4.14.2.4): `IPK || SHA256(Sigma1 || Sigma2)`.
pub(crate) fn s3k_salt(ipk: &[u8; 16], sigma12_hash: &[u8; 32]) -> Vec<u8> {
    let mut salt = Vec::with_capacity(16 + 32);
    salt.extend_from_slice(ipk);
    salt.extend_from_slice(sigma12_hash);
    salt
}
```
In `case.rs` add `mod wire;` (file at `src/case/wire.rs` is valid alongside `src/case.rs` in edition 2021) and `pub use wire::encode_sigma1; pub(crate) use wire::{Sigma2, Tbe as Tbe2};` then rewire `establish`: `let sigma2 = wire::parse_sigma2(&msg.payload).map_err(CaseError::Sigma2Malformed)?; let shared = wire::ecdh(&eph_secret, &sigma2.responder_eph_pub).ok_or(CaseError::Sigma2Malformed("responder ephemeral key"))?; let s2k = derive_sigma_key(&shared, &wire::s2k_salt(&creds.ipk_operational, &sigma2.responder_random, &sigma2.responder_eph_pub, &sigma1_hash), wire::INFO_S2K);` … `let tbe3 = wire::encode_tbe(&creds.noc_tlv, creds.icac_tlv.as_deref(), &signature, None);` … `let s3k = derive_sigma_key(&shared, &wire::s3k_salt(&creds.ipk_operational, &sigma2_hash), wire::INFO_S3K);`. Keep `pub(crate) fn parse_sigma2` and `pub(crate) fn decrypt_tbe2` in `case.rs` as thin wrappers (the case.rs tests call them). Keep the `sigma1_has_spec_structure` / `parses_sigma2_*` / `decrypts_and_parses_tbe2_roundtrip` tests in `case.rs` unchanged (they now exercise the wrappers).

In `case_responder.rs`: replace its private codec section (old 404-632) with `use crate::case::wire::{self, ecdh, encode_sigma2, encode_tbe, encode_tbs, parse_sigma1, parse_sigma3, parse_tbe, s2k_salt, s3k_salt, sha256, INFO_S2K, INFO_S3K, TBE2_NONCE, TBE3_NONCE};` and map errors: `parse_sigma1(payload).map_err(CaseCoreError::Decode)?`, `ecdh(&resp_secret, &sigma1.initiator_eph_pub).ok_or(CaseCoreError::Decode("initiator ephemeral key"))?`, `let Tbe { noc: init_noc_tlv, icac: init_icac_tlv, signature: sig3 } = parse_tbe(&tbe3).map_err(CaseCoreError::Decode)?;`. Its tests at 786-794 / 926-949 use `ecdh(...).unwrap()` → `.expect("ecdh")` on the `Option`, and `encode_tbe`/`encode_tbs`/`sha256` through the new imports.

- [ ] **Step 4: Fix the concatenated doc comment** at `case_responder.rs` ~888-903: the paragraph starting `/// Drives a *real* Sigma3 (correct S3K, correct TBS3 signature) whose NOC chains cleanly …` up to `… would go undetected without it.` belongs to the test `rejects_peer_noc_chaining_to_our_root_with_a_different_fabric_id` — move it there (above its `#[test]`), leaving `handshake_with_initiator` with only its own paragraph (`/// Drives a full Sigma1 -> Sigma2 -> Sigma3 handshake …`).

- [ ] **Step 5: Run tests**

Run: `cargo test -p mat-controller 2>&1 | tail -30` — expected green (`case_self_handshake` is the byte-level pin: the initiator built on `wire` must still handshake with the responder built on `wire`; mat-device's `tests/case_establish.rs` will pin it against the *other* crate at merge time).

- [ ] **Step 6: Verify no duplicate codec remains**

Run: `/usr/bin/grep -n -E "fn (encode_tbs|encode_tbe3?|parse_tbe|ecdh|encode_sigma[123]|parse_sigma[123]|sha256)\b|NCASE_Sigma|b\"Sigma[23]\"" crates/mat-controller/src/case.rs crates/mat-controller/src/case_responder.rs crates/mat-controller/src/case/wire.rs`
Expected: each hit only in `wire.rs`, except the thin `parse_sigma2`/`decrypt_tbe2` wrappers in `case.rs`.

- [ ] **Step 7: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/case.rs crates/mat-controller/src/case/wire.rs crates/mat-controller/src/case_responder.rs
git commit -m "refactor(mat-controller): CASE の Sigma1/2/3・TBS/TBE・ECDH・salt を case/wire.rs に一本化（initiator/responder 共用）、case_responder の doc 連結を修正"
```

---

### Task 7: `session/client.rs` `im_request` + `expect_im`; subscribe salvage helper

**Files:**
- Modify: `crates/mat-controller/src/session/client.rs:19-232, 239-306, 314-439`
- Modify: `crates/mat-controller/src/session/subscribe.rs:92-203, 263-293`

**Interfaces:**
- Produces (in `client.rs`, `pub(super)`):
  ```rust
  impl SecureSession {
      /// Sends one IM message on `exchange_id` reliably and returns the peer's
      /// first real reply: the one piggybacked on the ack, or — after a
      /// standalone ack — the next message within `IM_RECV_TIMEOUT`.
      pub(super) async fn im_request(&mut self, exchange_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig) -> Result<IncomingMessage, SessionError>;
  }
  /// Checks an IM reply's opcode: `expected` → `Ok(payload)`; `StatusResponse`
  /// → `Err(Im(StatusResponse(status)))` (decode error → `Err(Im(..))`);
  /// anything else → `Err(UnexpectedOpcode(op))`.
  pub(super) fn expect_im(msg: &IncomingMessage, expected: u8) -> Result<&[u8], SessionError>;
  ```
- In `subscribe.rs` (private):
  ```rust
  /// `decode_report_data_message` that never fails: an undecodable payload is
  /// logged (`warn!` with `exchange_id`/`payload_len`/`error`, then `debug!`
  /// with the hex head) and replaced by an empty report (audit ⑨).
  fn decode_report_data_lossy(payload: &[u8], exchange_id: u16, warn_msg: &'static str, debug_msg: &'static str) -> crate::im::ReportDataMessage;
  ```
  called with `("subscribe: undecodable priming chunk; acking and continuing", "undecodable priming chunk payload")` and `("sub pump: undecodable report; delivering as empty", "undecodable report payload")` — same texts as today.

- [ ] **Step 1: Write a failing unit test for `expect_im`** in `client.rs` tests:

```rust
    #[test]
    fn expect_im_maps_status_response_and_unexpected_opcode() {
        use crate::message::{Destination, MessageHeader, ProtocolHeader};
        let mk = |opcode: u8, payload: Vec<u8>| IncomingMessage {
            header: MessageHeader { session_id: 1, security_flags: 0, message_counter: 1, source_node_id: None, destination: Destination::None },
            proto: ProtocolHeader { initiator: false, needs_ack: false, acked_counter: None, opcode, exchange_id: 1, protocol_id: crate::im::PROTOCOL_ID_IM, vendor_id: None },
            payload,
        };
        let ok = mk(crate::im::OPCODE_REPORT_DATA, b"x".to_vec());
        assert_eq!(expect_im(&ok, crate::im::OPCODE_REPORT_DATA).unwrap(), b"x");
        let sr = mk(crate::im::OPCODE_STATUS_RESPONSE, crate::im::encode_status_response(0x7E));
        assert!(matches!(expect_im(&sr, crate::im::OPCODE_REPORT_DATA), Err(SessionError::Im(crate::im::ImError::StatusResponse(0x7E)))));
        let other = mk(0x33, Vec::new());
        assert!(matches!(expect_im(&other, crate::im::OPCODE_REPORT_DATA), Err(SessionError::UnexpectedOpcode(0x33))));
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p mat-controller --lib session::client::tests::expect_im 2>&1 | tail -5`.

- [ ] **Step 3: Implement** in `client.rs`:

```rust
impl SecureSession {
    pub(super) async fn im_request(&mut self, exchange_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig) -> Result<IncomingMessage, SessionError> {
        let resp = self
            .send_reliable(exchange_id, crate::im::PROTOCOL_ID_IM, opcode, payload, cfg)
            .await?;
        match resp {
            Some(m) => Ok(m),
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await,
        }
    }
}

pub(super) fn expect_im(msg: &IncomingMessage, expected: u8) -> Result<&[u8], SessionError> {
    use crate::im::{self, ImError};
    match msg.proto.opcode {
        op if op == expected => Ok(&msg.payload),
        im::OPCODE_STATUS_RESPONSE => {
            let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
            Err(SessionError::Im(ImError::StatusResponse(s)))
        }
        op => Err(SessionError::UnexpectedOpcode(op)),
    }
}
```
Then rewrite each client method on top of them, keeping every decode/branch exactly:
- `read_attribute`: `let msg = self.im_request(exchange_id, im::OPCODE_READ_REQUEST, &req, cfg).await?; let rd = im::decode_report_data(expect_im(&msg, im::OPCODE_REPORT_DATA)?).map_err(SessionError::Im)?;` then the existing `suppress_response` best-effort close (`let _ = self.send_reliable(...)` — NOT `im_request`, it must not wait for a reply), status check, value.
- `invoke`, `invoke_for_data`, `write_attribute_tlv`: same shape (`im_request` + `expect_im(&msg, OPCODE_INVOKE_RESPONSE / OPCODE_WRITE_RESPONSE)`).
- `send_timed_request`: `let msg = self.im_request(exchange_id, im::OPCODE_TIMED_REQUEST, &timed_req, cfg).await?; match msg.proto.opcode { im::OPCODE_STATUS_RESPONSE => { let s = ...; if s != 0 { return Err(StatusResponse(s)) } Ok(()) } op => Err(UnexpectedOpcode(op)) }` — it cannot use `expect_im` because for it StatusResponse is the *expected* opcode; leave that match as is.
- `read_attribute_json` / `read_cluster_json`: `let msg = self.im_request(exchange_id, im::OPCODE_READ_REQUEST, &req, cfg).await?; let msgs = self.collect_reports(exchange_id, msg, cfg).await?;`.
- `collect_reports`: chunk continuation becomes `msg = self.im_request(exchange_id, im::OPCODE_STATUS_RESPONSE, &ok, cfg).await?; continue;`; the final best-effort close stays `let _ = self.send_reliable(...)`; the `OPCODE_STATUS_RESPONSE`/other arms stay (they are in a loop over `msg.proto.opcode` with the REPORT_DATA arm first — keep the match, it is not the postamble shape).
- `subscribe.rs::subscribe`: initial `let mut msg = self.im_request(exchange_id, im::OPCODE_SUBSCRIBE_REQUEST, &req, cfg).await?;`, chunk prompt `msg = self.im_request(exchange_id, im::OPCODE_STATUS_RESPONSE, &ok, cfg).await?;`, and the salvage block → `let rd = decode_report_data_lossy(&msg.payload, exchange_id, "subscribe: undecodable priming chunk; acking and continuing", "undecodable priming chunk payload");`. Pump: `let rd = decode_report_data_lossy(&msg.payload, msg.proto.exchange_id, "sub pump: undecodable report; delivering as empty", "undecodable report payload");`. Note: `tracing::warn!(exchange_id, payload_len = .., error = %e, "{warn_msg}")` — pass the message as the format string argument, keeping the structured fields identical.

- [ ] **Step 4: Run tests** — `cargo test -p mat-controller --lib session:: 2>&1 | tail -10` green, then the whole crate.

- [ ] **Step 5: Verify the preamble is gone**

Run: `/usr/bin/grep -c "None => self.recv(exchange_id, IM_RECV_TIMEOUT)" crates/mat-controller/src/session/client.rs crates/mat-controller/src/session/subscribe.rs`
Expected: `client.rs:1` (inside `im_request`), `subscribe.rs:0`.

- [ ] **Step 6: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/session/client.rs crates/mat-controller/src/session/subscribe.rs
git commit -m "refactor(mat-controller): IM クライアントの send→piggyback/recv 前置きと StatusResponse/UnexpectedOpcode 後置きを im_request/expect_im に集約、購読の救済デコードを 1 本化"
```

---

### Task 8: `im/` — StatusIB dedupe, generic first-InvokeResponseIB, `put_attribute_path` / `put_status_ib`

**Files:**
- Modify: `crates/mat-controller/src/im/read.rs:13-32, 37-70, 261-308, 496-529, 567-582`
- Modify: `crates/mat-controller/src/im/invoke.rs:356-399, 483-524, 539-569`
- Modify: `crates/mat-controller/src/im/write.rs:13-37, 265-285`
- Modify: `crates/mat-controller/src/im/mod.rs` (add the two `pub(super)`/`pub(crate)` encoders next to `expect_struct_start`)

**Interfaces:**
- Produces (in `im/mod.rs`, `pub(crate)`):
  ```rust
  /// AttributePathIB (spec §8.9.2.2) as a list under `tag`:
  /// `list{2: endpoint, 3: cluster, [4: attribute]}`. `attribute: None`
  /// omits tag 4 (cluster-wide wildcard).
  pub(crate) fn put_attribute_path(w: &mut Writer, tag: Tag, endpoint: u16, cluster: u32, attribute: Option<u32>);
  /// StatusIB (spec §8.9.2.3) as a struct under `tag`:
  /// `struct{0: status, [1: cluster_status]}`.
  pub(crate) fn put_status_ib(w: &mut Writer, tag: Tag, status: u8, cluster_status: Option<u8>);
  ```
- In `im/read.rs`, `pub(super) fn decode_status_ib_code(r: &mut Reader) -> Result<Option<u8>, ImError>` — reads a StatusIB whose `StructStart` was consumed; returns tag 0 as `u8` (error `"attribute status code out of range"` / `"truncated status ib"`), `None` if the struct had no tag 0.
- In `im/invoke.rs`, `fn decode_first_invoke_response_ib<T>(payload: &[u8], decode_ib: fn(&mut Reader) -> Result<T, ImError>) -> Result<T, ImError>` — the shared outer walk of `decode_invoke_response` / `decode_invoke_response_data`.

Call sites of `put_attribute_path` (all bytes identical to the inline code): `encode_read_request` (Anonymous, Some(attr)), `encode_read_request_cluster` (Anonymous, None), `encode_attribute_report_ib` (Context(1)/Context(0), Some), `write.rs::encode_write_request_inner` (Context(1), Some), `write.rs::encode_write_response` (Context(0), Some). **Not** `im/subscribe.rs:60-66` (cluster-only path with no endpoint — different shape, leave it) and **not** event paths (different tag numbering). `put_status_ib`: `encode_attribute_report_ib` Status arm, `write.rs::encode_write_response`, `invoke.rs::encode_invoke_response_status`, `event.rs:427-429` (`EventStatusIB`'s StatusIB — Context(1), no cluster status; include it, same bytes).

- [ ] **Step 1: Write failing tests** in `im/mod.rs` tests:

```rust
    #[test]
    fn put_attribute_path_and_status_ib_shapes() {
        let mut w = Writer::new();
        put_attribute_path(&mut w, Tag::Context(1), 1, 0x0006, Some(0));
        put_attribute_path(&mut w, Tag::Anonymous, 2, 0x0035, None);
        put_status_ib(&mut w, Tag::Context(1), 0x81, Some(0x42));
        put_status_ib(&mut w, Tag::Context(1), 0, None);
        let b = w.finish();
        let mut r = Reader::new(&b);
        let mut els = Vec::new();
        while let Some(e) = r.next().unwrap() { els.push((e.tag, e.value)); }
        assert_eq!(els[0], (Tag::Context(1), Value::ListStart));
        assert_eq!(els[1], (Tag::Context(2), Value::Uint(1)));
        assert_eq!(els[2], (Tag::Context(3), Value::Uint(6)));
        assert_eq!(els[3], (Tag::Context(4), Value::Uint(0)));
        assert_eq!(els[4], (Tag::Anonymous, Value::ContainerEnd));
        assert_eq!(els[5], (Tag::Anonymous, Value::ListStart));
        assert_eq!(els[6], (Tag::Context(2), Value::Uint(2)));
        assert_eq!(els[7], (Tag::Context(3), Value::Uint(0x35)));
        assert_eq!(els[8], (Tag::Anonymous, Value::ContainerEnd)); // no tag 4
        assert_eq!(els[9], (Tag::Context(1), Value::StructStart));
        assert_eq!(els[10], (Tag::Context(0), Value::Uint(0x81)));
        assert_eq!(els[11], (Tag::Context(1), Value::Uint(0x42)));
        assert_eq!(els[12], (Tag::Anonymous, Value::ContainerEnd));
        assert_eq!(els[13], (Tag::Context(1), Value::StructStart));
        assert_eq!(els[14], (Tag::Context(0), Value::Uint(0)));
        assert_eq!(els[15], (Tag::Anonymous, Value::ContainerEnd));
    }
```
(If `ContainerEnd` elements carry a different tag in this Reader, adjust the expected tag after the first run — the *value* sequence is what matters.) Also add a byte-equality pin that the existing encoders did not move: capture `encode_read_request(1, 6, 0)`, `encode_read_request_cluster(1, 0x35)`, `encode_write_response(&[(0, 0x1F, 0, 0)])`, `encode_invoke_response_status(1, 6, 1, 0x81, Some(0x42))`, `encode_report_data_entries(&[ReportEntryOut::Status{endpoint:1,cluster:6,attribute:0,status:0x86}], true, None, false)` **before** editing (print with `{:02x?}` from a throwaway test run) and assert them as literal byte vectors in a new test `encoders_are_byte_stable_after_path_status_extraction`.

- [ ] **Step 2: Run to verify failure** (`put_attribute_path` not found).

- [ ] **Step 3: Implement**

`im/mod.rs`:
```rust
pub(crate) fn put_attribute_path(w: &mut Writer, tag: Tag, endpoint: u16, cluster: u32, attribute: Option<u32>) {
    w.start_list(tag);
    w.put_uint(Tag::Context(2), u64::from(endpoint));
    w.put_uint(Tag::Context(3), u64::from(cluster));
    if let Some(attribute) = attribute {
        w.put_uint(Tag::Context(4), u64::from(attribute));
    }
    w.end_container();
}

pub(crate) fn put_status_ib(w: &mut Writer, tag: Tag, status: u8, cluster_status: Option<u8>) {
    w.start_struct(tag);
    w.put_uint(Tag::Context(0), u64::from(status));
    if let Some(cs) = cluster_status {
        w.put_uint(Tag::Context(1), u64::from(cs));
    }
    w.end_container();
}
```
`im/read.rs`:
```rust
pub(super) fn decode_status_ib_code(r: &mut Reader) -> Result<Option<u8>, ImError> {
    let mut status = None;
    loop {
        let e2 = r.next()?.ok_or(ImError::Malformed("truncated status ib"))?;
        match (e2.tag, e2.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                status = Some(u8::try_from(v).map_err(|_| ImError::Malformed("attribute status code out of range"))?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    Ok(status)
}
```
and in both `decode_attribute_status_ib` / `decode_attribute_status_ib_full`: `(Tag::Context(1), Value::StructStart) => { if let Some(s) = decode_status_ib_code(r)? { status = Some(s); } }`.
`im/invoke.rs`:
```rust
fn decode_first_invoke_response_ib<T>(payload: &[u8], decode_ib: fn(&mut Reader) -> Result<T, ImError>) -> Result<T, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut result: Option<T> = None;
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated invoke response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::ArrayStart) => {
                // InvokeResponses
                let mut first = true;
                loop {
                    let e2 = r.next()?.ok_or(ImError::Malformed("truncated invoke responses"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            result = Some(decode_ib(&mut r)?);
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => return Err(ImError::Malformed("unexpected element in invoke responses")),
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(&mut r)?,
            _ => {}
        }
    }
    result.ok_or(ImError::Malformed("invoke response without InvokeResponseIB"))
}

pub fn decode_invoke_response(payload: &[u8]) -> Result<InvokeOutcome, ImError> {
    decode_first_invoke_response_ib(payload, decode_invoke_response_ib)
}

pub fn decode_invoke_response_data(payload: &[u8]) -> Result<InvokeResponseData, ImError> {
    decode_first_invoke_response_ib(payload, decode_invoke_response_ib_data)
}
```
(keep both functions' doc comments.) Then replace the inline path/status encodes listed above with the helpers.

- [ ] **Step 4: Run tests** — `cargo test -p mat-controller 2>&1 | tail -20` (im tests + the new byte pins + mat-device-shaped wire tests inside `im/*`).

- [ ] **Step 5: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/im/
git commit -m "refactor(mat-controller): im の AttributePathIB/StatusIB エンコードを put_attribute_path/put_status_ib に、StatusIB デコードと InvokeResponse 外側走査を共通化"
```

---

### Task 9: `expect` messages on the bare `unwrap()`s in `tlv.rs` / `message.rs`

**Files:**
- Modify: `crates/mat-controller/src/tlv.rs:331-339, 393-409` (11 `try_into().unwrap()` after `take(n)`)
- Modify: `crates/mat-controller/src/message.rs:209-217` (3 `try_into().unwrap()`)

- [ ] **Step 1: Replace every `self.take(N)?.try_into().unwrap()` with `self.take(N)?.try_into().expect("take(N) yields exactly N bytes")`** (spell the concrete N: `"take(2) yields exactly 2 bytes"`, etc.). No test needed — infallible by construction (`take` returns a slice of exactly `n` bytes or `Err`), existing tlv/message tests cover the paths.

- [ ] **Step 2: Run** `cargo test -p mat-controller --lib tlv:: message:: 2>&1 | tail -5` and `/usr/bin/grep -c "try_into().unwrap()" crates/mat-controller/src/tlv.rs crates/mat-controller/src/message.rs` → expected `0` for both.

- [ ] **Step 3: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/tlv.rs crates/mat-controller/src/message.rs
git commit -m "chore(mat-controller): tlv/message の take(n) 直後の bare unwrap に expect メッセージを付与"
```

---

### Task 10 (Tier 6): test scaffolding — `test_support` unsecured helpers, `FakePeripheral`, `udp_session_pair`

**Files:**
- Modify: `crates/mat-controller/src/test_support.rs:79-129` (make `build_unsecured`, `recv_dg`, `decode_unsecured` `pub`; `decode_unsecured` also returns the `MessageHeader`)
- Modify: `crates/mat-controller/src/pase.rs:700-747` (delete the three duplicate helpers, import from `test_support`)
- Modify: `crates/mat-controller/src/btp.rs` (`FakePeripheral` + `fake_link` move out of the test module)
- Modify: `crates/mat-controller/src/test_support.rs` (new `pub mod btp_fake` holding `FakePeripheral` / `fake_link`)
- Modify: `crates/mat-controller/tests/btp_pase_plumbing.rs` (use `test_support::btp_fake::fake_link` instead of hand-rolled channels)
- Modify: `crates/mat-controller/src/session/test_util.rs` (add `udp_session_pair`, `device_initiated_datagram`)
- Modify: `crates/mat-controller/src/session/{client,mrp,responder,subscribe}.rs` tests (use the two helpers)

**Interfaces:**
- `test_support`:
  ```rust
  pub fn build_unsecured(counter: u32, opcode: u8, exchange_id: u16, acked_counter: Option<u32>, needs_ack: bool, payload: &[u8]) -> Vec<u8>;
  pub async fn recv_dg(t: &UdpTransport) -> (Vec<u8>, SocketAddr);
  pub fn decode_unsecured(buf: &[u8]) -> Option<(MessageHeader, ProtocolHeader, Vec<u8>)>;  // header added
  pub mod btp_fake { pub struct FakePeripheral { … }; pub fn fake_link() -> (GattLink, FakePeripheral); impl FakePeripheral { pub async fn do_handshake(&mut self, segment_size: u16, window: u8); pub async fn recv_message(&mut self) -> (Vec<u8>, u8); pub async fn send_ack(&mut self, ack: u8); pub async fn send_message(&mut self, msg: &[u8], segment_size: u16, ack: Option<u8>); } }
  ```
  `FakePeripheral::do_handshake` asserts `req == handshake_request(PROPOSED_WINDOW)` — `handshake_request` and `PROPOSED_WINDOW` are already `pub` in `btp`.
- `session/test_util.rs`:
  ```rust
  /// A controller-role `SecureSession` over a fresh loopback UDP socket plus
  /// the raw device-side socket (`LOCAL_SID`/`PEER_SID`/`keys()`/`OUR_NODE`/`DEV_NODE`).
  pub(super) async fn udp_session_pair() -> (SecureSession, UdpTransport);
  /// A device-*initiated* (initiator=true) secured datagram — the shape
  /// subscription reports and device-side requests arrive in.
  pub(super) fn device_initiated_datagram(exchange_id: u16, protocol_id: u16, opcode: u8, acked: Option<u32>, needs_ack: bool, counter: u32, payload: &[u8]) -> Vec<u8>;
  ```

- [ ] **Step 1: `test_support` unsecured helpers.** Make the three functions `pub` with doc comments; change `decode_unsecured` to return `Option<(MessageHeader, ProtocolHeader, Vec<u8>)>` and update its two internal callers (`recv_unsecured` — which then no longer needs its own second `MessageHeader::decode`). In `pase.rs` tests delete `build_unsecured`, `recv_dg`, `decode_unsecured` and `use crate::test_support::{build_unsecured, decode_unsecured, recv_dg};`; the pase tests call `build_unsecured(counter, opcode, exchange_id, acked, false, &payload)` (add the `false` needs_ack argument). Run `cargo test -p mat-controller --lib pase:: 2>&1 | tail -5`. If `crate::test_support` is not visible in the lib's unit tests, change `lib.rs` to `#[cfg(any(test, feature = "test-responder"))] #[doc(hidden)] pub mod test_support;`.

- [ ] **Step 2: `FakePeripheral`.** Create `pub mod btp_fake` at the bottom of `test_support.rs` with the struct, `fake_link`, and the four methods copied verbatim from `btp.rs` tests (fields become private, methods `pub`). In `btp.rs` tests replace them with `use crate::test_support::btp_fake::{fake_link, FakePeripheral};` (keep every test body). In `tests/btp_pase_plumbing.rs` replace the hand-rolled `wtx/wrx/itx/irx` + inline handshake with:
```rust
    let (link, mut p) = fake_link();
    let peripheral = tokio::spawn(async move {
        p.do_handshake(244, 4).await;
        let (msg, _seq) = p.recv_message().await;
        // … the existing header assertions on `msg` …
        // reply: build the garbage PBKDFParamResponse exactly as before, then
        p.send_message(&reply, 244, None).await;
    });
```
(`send_message` emits `seq` starting at 1, identical to the frame the test built by hand.) Update the file's module doc (the "再掲する" paragraph no longer applies).

- [ ] **Step 3: `udp_session_pair` / `device_initiated_datagram`.** Implement in `test_util.rs`:
```rust
pub(super) async fn udp_session_pair() -> (SecureSession, UdpTransport) {
    let device = bind_local().await;
    let peer = device.local_addr().unwrap();
    let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
    let s = SecureSession::new(transport, peer, LOCAL_SID, PEER_SID, keys(), OUR_NODE, DEV_NODE);
    (s, device)
}

pub(super) fn device_initiated_datagram(exchange_id: u16, protocol_id: u16, opcode: u8, acked: Option<u32>, needs_ack: bool, counter: u32, payload: &[u8]) -> Vec<u8> {
    let header = MessageHeader { session_id: LOCAL_SID, security_flags: 0, message_counter: counter, source_node_id: None, destination: Destination::None };
    let proto = ProtocolHeader { initiator: true, needs_ack, acked_counter: acked, opcode, exchange_id, protocol_id, vendor_id: None };
    seal_message(&R2I, &header, &proto, payload, DEV_NODE).unwrap()
}
```
Then, test by test, replace the 7-line `SecureSession::new(Arc::clone(&transport), peer, LOCAL_SID, PEER_SID, keys(), OUR_NODE, DEV_NODE)` preambles with `let (mut s, device) = udp_session_pair().await;` — **only** where the test does not also need `transport.local_addr()` (tests that need `local` can get it from `s`… they cannot: the field is private. For those, add `pub(super) fn local_addr(&self) -> SocketAddr` on `SecureSession`? No — keep those tests as they are). Replace hand-built `MessageHeader{..}/ProtocolHeader{initiator: true, ..}/seal_message(&R2I, ..)` triplets with `device_initiated_datagram(..)` in `subscribe.rs` and `responder.rs` tests where the fields match exactly (`session_id: LOCAL_SID`, `source_node_id: None`, `destination: None`, `vendor_id: None`, sealed with `R2I` / `DEV_NODE`). Do not touch a test whose header deviates (e.g. `ignores_wrong_key_wrong_session_and_wrong_exchange`).

- [ ] **Step 4: Run** `cargo test -p mat-controller 2>&1 | tail -20` and clippy.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p mat-controller
git add crates/mat-controller/src/test_support.rs crates/mat-controller/src/pase.rs crates/mat-controller/src/btp.rs crates/mat-controller/tests/btp_pase_plumbing.rs crates/mat-controller/src/session/
git commit -m "test(mat-controller): pase/btp のテストヘルパを test_support へ、session テストに udp_session_pair/device_initiated_datagram を導入"
```

---

### Task 11 (optional, last, medium risk): `tlv::StructFields` for `pase.rs` only

Do this task only after Tasks 1–10 are committed and green. Stop after `pase.rs`; the other 13 struct-field loops (case/wire, im, …) go into the DONE file as a follow-up.

**Files:**
- Modify: `crates/mat-controller/src/tlv.rs` (add `StructFields`)
- Modify: `crates/mat-controller/src/pase.rs:158-207, 252-319, 335-369, 386-430, 446-480` (the six decoders)

**Interfaces:**
```rust
/// Cursor over the fields of a struct whose `StructStart` has already been
/// consumed. `next_scalar` yields the next non-container field and returns
/// `Ok(None)` at the matching `ContainerEnd`; nested containers are skipped
/// whole (`skip_container`). Input ending before `ContainerEnd` is
/// `Err(TlvError::Truncated)`.
pub struct StructFields<'r, 'a> { r: &'r mut Reader<'a> }
impl<'r, 'a> StructFields<'r, 'a> {
    /// Consumes the struct's opening element; `Err(TlvError::InvalidType(_))`
    /// if the next element is not a `StructStart` (or the input is empty).
    pub fn open(r: &'r mut Reader<'a>) -> Result<Self, TlvError>;
    /// Wraps a reader already positioned just after a `StructStart`.
    pub fn inside(r: &'r mut Reader<'a>) -> Self;
    pub fn next_scalar(&mut self) -> Result<Option<Element<'a>>, TlvError>;
}
```
Error-label mapping in `pase.rs`: `open` failure → `PaseError::Malformed("top-level struct")` (for `Err(TlvError::InvalidType(_))`) or `"tlv"` (other); `next_scalar` `Err(TlvError::Truncated)` → `Malformed("truncated")`, other `Err` → `Malformed("tlv")`. **Accepted delta (record in DONE):** a truncated *element* inside the struct (Reader-level `Truncated`) used to map to `"tlv"` and now maps to `"truncated"`. `decode_pbkdf_param_response`'s nested `struct{1: iterations, 2: salt}` (tag 4) needs the nested container: `next_scalar` would skip it, so that decoder uses a second mode: add `pub fn next_field(&mut self) -> Result<Option<Element<'a>>, TlvError>` that yields container starts too (caller must consume them via `StructFields::inside(self.r)` or `skip_container`); use `next_field` only in that decoder, `next_scalar` in the other five.

- [ ] **Step 1: Write failing tests** in `tlv.rs` tests (nested struct skipped by `next_scalar`, surfaced by `next_field`; `Truncated` on early end; `open` rejects a non-struct).
- [ ] **Step 2: Run** (`StructFields` unknown).
- [ ] **Step 3: Implement** `StructFields`, then rewrite the six pase decoders (bodies become `let mut f = StructFields::open(&mut r).map_err(...)?; while let Some(el) = f.next_scalar().map_err(...)? { match (el.tag, el.value) { … } }`). Keep every `Malformed(...)` label for field-level errors.
- [ ] **Step 4: Run** `cargo test -p mat-controller` — the pase codec tests (`pbkdf_request_roundtrip`, `pake_message_roundtrips`, `rejects_zero_responder_session_id`, …) and `pase_self_handshake` / `btp_pase_plumbing` pin it.
- [ ] **Step 5: Commit** `refactor(mat-controller): tlv::StructFields を追加し pase の struct フィールド走査 6 本を置換`.

---

### Task 12: Final gate and DONE file

**Files:**
- Create: `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/ctrl-proto.DONE.md`

- [ ] **Step 1: `task check`** from the worktree root. Expected: fmt:check, clippy, doc:check and test all green. Fix anything it reports (clippy `-D warnings` across the workspace includes the other crates compiled against our changes — a compile error in `mat-device`/`mat-native` means a public item they use was broken; restore it).
- [ ] **Step 2: Cross-crate sanity:** `cargo test -p mat-device -p mat-native --no-run` compiles (their tests import `case::encode_sigma1`, `case::encode_status_report`, `exchange::ResponderExchange`, `pase::OPCODE_*`, `im::*`).
- [ ] **Step 3: Write `ctrl-proto.DONE.md`** with sections: やった項目 (per task, with commit hashes), 見送った項目と理由 (M2 scalar read path kept — mat-native `read_onoff` uses it; TLV `StructFields` for the 13 non-pase loops; anything skipped in Task 10/11), 挙動/文言の差分 (the accepted deltas listed in Tasks 1, 3, 6, 11), `task check` の結果 (verbatim tail), 実機 E2E が必要か (**必要**: exchange/MRP/CASE wire are on the commissioning and matd subscribe paths — run e2e m1/m3 + a production `matd` smoke before merge, per `e2e-before-merge`).
- [ ] **Step 4: `git log --oneline main..refactor/ctrl-proto`** and paste into the DONE file. Do not merge, do not push.
