# refactor2 ctrl lane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the mat-controller leftovers from the 2026-09-12 refactor: StructFields rollout, deferred tests, small dedups, module file promotion, test helper consolidation, and a single mDNS socket binder shared with mat-device.

**Architecture:** Pure refactors inside `crates/mat-controller` (plus one call-site file in `mat-device`). No behavior change except the *accepted, listed* decoder error-label deltas caused by `StructFields` merging "input ended" and "element truncated" into one `TlvError::Truncated`.

**Tech Stack:** Rust workspace, cargo, Task (`task check`).

**Spec:** `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/ctrl.md` + `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/_common.md`

## Global Constraints

- Worktree `/home/noguk/ghq/github.com/nogu3/mat-wt/ctrl`, branch `refactor2/ctrl`. Commit only here; **no merge to main, no push**.
- Allowed files: `crates/mat-controller/**` **except** `src/im/read.rs`, `src/im/mod.rs`, `src/session/client.rs` (another lane edits them). Plus exactly `crates/mat-device/src/net/mdns.rs`. Also `lib.rs` of mat-controller is allowed (it's inside mat-controller/**).
- Every cargo command: prefix `CARGO_BUILD_JOBS=3`, run in foreground.
- Zero behavior change unless listed. Keep test-pinned strings / bytes / ordering. KVS (chip-tool INI) and dnssd: pure moves only.
- Line numbers below are from a5fcf4f — re-grep before editing.
- Comments: match surrounding style (this crate mixes Japanese and English doc comments; keep the file's existing language).
- Commit message style: `refactor(ctrl): ...` / `test(ctrl): ...`, Japanese body OK, end with:
  ```
  Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01PrtbnSAy4ktann94vqPN9K
  ```
- Each task: record in your report any error-label delta, anything deferred, and the test command output summary.

---

### Task 1: Deferred tests + small cleanups (items 2, 3)

Must run **before** Task 2 (the golden bytes must be captured from the pre-refactor encoders).

**Files:**
- Modify: `crates/mat-controller/src/case/wire.rs` (tests module)
- Modify: `crates/mat-controller/src/tlv.rs` (tests module)
- Modify: `crates/mat-controller/src/pase.rs` (tests module)
- Modify: `crates/mat-controller/src/case_responder.rs` (serial snippet ~733; test locals ~564-569, ~701-704)
- Modify: `crates/mat-controller/src/session/subscribe.rs` (~915-925 test)
- Modify: `crates/mat-controller/src/case.rs` (tests module; create `#[cfg(test)] mod tests` if absent)

- [ ] **Step 1: Golden bytes for `encode_sigma1` / `encode_tbs`.** In `case/wire.rs` tests, add tests that call the current encoders with fixed inputs and compare to a literal byte array. Capture the literal by first writing the test with `assert_eq!(out, vec![])`, run it, copy the "left" bytes from the failure output into the literal. Inputs: random `[0x11; 32]`, session_id `0x1234`, dest_id `[0x22; 32]`, eph_pub `[0x04, 0x33 × 64]`; for tbs: noc `&[0xAA; 3]`, icac `Some(&[0xBB; 2])` and also a `None` case, sender `[0x04, 0x44 × 64]`, receiver `[0x04, 0x55 × 64]`. Expected shape sanity: starts `0x15`, sigma1 tag1 is `0x30 0x01 0x20 ...`, ends `0x18`. Test names: `encode_sigma1_golden_bytes`, `encode_tbs_golden_bytes`.
- [ ] **Step 2: `Reader::next` never yields `InvalidType(0)`.** In `tlv.rs` tests add `reader_never_reports_invalid_type_zero`: for every control byte `c in 0x00..=0x18u8` (anonymous tag, element type = low 5 bits), build `[c]` followed by 16 zero bytes (enough payload for any fixed-size type / 1-byte length prefix), run `Reader::new(&buf).next()`, and assert the result is not `Err(TlvError::InvalidType(0))` (any other Ok/Err is fine). Add also `[0x19]` → `Err(InvalidType(0x19))` if not already covered (line ~796 covers 0x19 — then skip). Doc comment: this is the invariant that `StructFields::open`'s `InvalidType(0)` sentinel relies on.
- [ ] **Step 3: pase sibling container skip arm.** In `pase.rs` tests, check the existing test near line ~1107/~1172 (`nested_skip`). It may cover `decode_pbkdf_param_request`, not `decode_pbkdf_param_response`. If `decode_pbkdf_param_response` has no test with a tag-5 struct *sibling* (after tag 4), add `pbkdf_param_response_skips_sibling_session_params`: encode via `Writer` struct{1: bytes32, 2: bytes32, 3: uint 7, 4: struct{1: uint 1000, 2: bytes 16×0x5A}, 5: struct{1: uint 500, 2: uint 300}} and assert decode gives session id 7, iterations 1000, salt 16×0x5A (i.e. tag-5 inner tag 1/2 did NOT overwrite iterations). Also a variant where tag 5 is an array containing a struct.
- [ ] **Step 4: Rename shadowing locals** in `case_responder.rs` tests: local `let mut s2k_salt` → `expected_s2k_salt`, `s3k_salt` → `expected_s3k_salt` (update all uses in those test fns only).
- [ ] **Step 5: Dead code** in `session/subscribe.rs` test: remove `let ack = device_datagram(...);` and `let _ = ack;` (keep the explanatory comment if it still makes sense for `d2`, reword minimally). Ensure no unused-import warnings result.
- [ ] **Step 6: `random_nonzero_u16` invariant test** in `case.rs` tests: `random_nonzero_u16_is_never_zero` — loop 10_000 times asserting `!= 0`. Doc comment: replaces mat-device's removed `random_session_id_is_never_zero`.
- [ ] **Step 7: Item 3** — in `case_responder.rs` (~733) replace the 3-line serial generation with `let serial = crate::cert::random_serial();` (check the `cert` import path already used in that file). Behavior identical (`cert::random_serial` does the same fill + `&= 0x7F`).
- [ ] **Step 8: Run** `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --lib` → all pass; `CARGO_BUILD_JOBS=3 cargo clippy -p mat-controller --all-targets --features test-responder -- -D warnings` clean.
- [ ] **Step 9: Commit** (can be 2 commits: tests, then serial dedupe).

---

### Task 2: StructFields in `case/wire.rs` (item 1, file 1/6)

**Files:** Modify `crates/mat-controller/src/case/wire.rs`.

**Interfaces:** Consumes `crate::tlv::{StructFields, TlvError, skip_container}` (`StructFields::open(&mut r) -> Result<Self, TlvError>` with `InvalidType(0)` = "not a struct/empty"; `next_scalar()` skips nested containers; both return `Err(TlvError::Truncated)` for input ending before `ContainerEnd`, incl. a truncated element or truncated nested container).

- [ ] **Step 1: Label-stability test first (must pass on the OLD code).** Add `decoder_error_labels_are_stable` in wire.rs tests, modeled on `pase.rs` `decoder_malformed_labels_are_stable`, covering for each of `parse_sigma1`, `parse_sigma2`, `parse_sigma3`, `parse_tbe`: empty input → `"<p> top-level struct"`, `[0x04,0x2A]` → top-level struct, `[0x19]` → `"<p> tlv"`, `[0x15]` → `"<p> truncated"`, `[0x15,0x19,0x18]` → `"<p> tlv"`, plus every per-field label (length errors, missing field, sigma2 zero session id). Prefix `<p>` = `sigma1`/`sigma3`/`tbe`; sigma2 has no prefix (`"top-level struct"`, `"tlv"`, `"truncated"`). Run it: PASS on old code.
- [ ] **Step 2: Replace the four hand-rolled walks** with `StructFields::open` + `while let Some(el) = f.next_scalar().map_err(...)? { match (el.tag, el.value) { ... } }`. Error mapping per parser (local helper fns in wire.rs):
  ```rust
  /// `StructFields::open` の Err → ラベル（InvalidType(0) = struct でない／空）。
  fn open_err(e: TlvError, not_struct: &'static str, tlv: &'static str) -> &'static str {
      match e { TlvError::InvalidType(0) => not_struct, _ => tlv }
  }
  /// フィールド走査の Err → ラベル（入力終端・要素途中切れ・入れ子途中切れは Truncated）。
  fn field_err(e: TlvError, truncated: &'static str, tlv: &'static str) -> &'static str {
      match e { TlvError::Truncated => truncated, _ => tlv }
  }
  ```
  The explicit `(_, StructStart|ArrayStart|ListStart) => skip_container` arms disappear (next_scalar does it). Keep the sigma1 comment about initiatorSessionParams near the loop, reworded to say next_scalar skips it.
- [ ] **Step 3: Accepted deltas** — update the stability test for cases that now change and mark them `// accepted delta: 以前は "<p> tlv"`: (a) `[0x15, 0x04]` (element truncated inside struct) → `"<p> truncated"`; (b) truncated nested container, e.g. `[0x15, 0x35, 0x01]` → `"<p> truncated"` (was `"<p> tlv"`). Add both cases to the test explicitly.
- [ ] **Step 4:** `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --lib case::` and `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --test case_self_handshake` → PASS. Golden tests from Task 1 still pass.
- [ ] **Step 5: Commit** `refactor(ctrl): case/wire の struct 走査を StructFields に`.

---

### Task 3: StructFields in `im/invoke.rs` (item 1, file 2/6)

**Files:** Modify `crates/mat-controller/src/im/invoke.rs`. **Do not touch `im/mod.rs`.**

**Interfaces:**
- Produces: in invoke.rs, `pub(super) fn field_err(truncated: &'static str) -> impl Fn(TlvError) -> ImError` (Truncated → `ImError::Malformed(truncated)`, other → `ImError::Tlv(e)`). Tasks 4–7 import it as `use super::invoke::field_err;`.

Rules for all IM files (Tasks 3–7):
- Only **struct field walks** become `StructFields`. Array/list element walks (`InvokeRequests`, write requests array, event requests/filters arrays, event reports array) stay hand-rolled.
- Functions taking `r: &mut Reader` positioned after a `StructStart` use `StructFields::inside(r)`. Top-level payload decoders keep `expect_struct_start(&mut r)?` (from im/mod.rs, preserves `"empty payload"`/`"expected struct"`) then `StructFields::inside(&mut r)`.
- Where a nested container must be descended/copied (e.g. `(Tag::Context(1), StructStart) => copy_value(...)`, nested CommandPath list), use `next_field()`; unhandled container starts must then be skipped with `skip_container(f.reader())?` (im's own `skip_container` wrapper, keep its error wording). Where no container is consumed use `next_scalar()`.
- Old behavior: `r.next()?` errors → `ImError::Tlv(e)`; end-of-input → `Malformed("truncated X")`. New: all `Truncated` → `Malformed("truncated X")`. Accepted deltas: (a) element truncated mid-struct `Tlv(Truncated)` → `Malformed("truncated X")`; (b) nested container truncated inside `next_scalar` `Malformed("truncated container")` → `Malformed("truncated X")`. Both map to the same mat-native kind (`errmap.rs:92` groups `Tlv|Malformed`), so only the detail text changes.

- [ ] **Step 1: Stability test first (PASS on old code)** `decoder_error_labels_are_stable` in invoke.rs tests: for `decode_invoke_request`, `decode_invoke_response`, `decode_invoke_response_data`, `decode_status_response` (and the inner IB decoders via crafted payloads) assert the `ImError` for: empty → `Malformed("empty payload")`, non-struct → `Malformed("expected struct")`, `[0x15]` → `Malformed("truncated invoke request")` etc, `[0x15, 0x19, 0x18]` → `Tlv(InvalidType(0x19))`, every "out of range" / "without X" label. Use a `fn err<T>(r: Result<T, ImError>) -> ImError` helper (no `Debug` on Ok). `ImError` derives PartialEq? Check; if not, match with `matches!`.
- [ ] **Step 2: Add `field_err`** and replace struct walks in: `decode_request_command_data_ib`, CommandPath inner walk, `decode_invoke_request` (top struct only; inner InvokeRequests array stays), `decode_status_ib`, `decode_command_status_ib`, `decode_invoke_response_ib`, `decode_first_invoke_response_ib` (top struct only), `decode_command_data_ib`, `decode_invoke_response_ib_data`, `decode_status_response`.
- [ ] **Step 3: Accepted deltas** added to the test with `// accepted delta` comments (one mid-element truncation and one truncated nested container case).
- [ ] **Step 4:** `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --lib im::` PASS; `CARGO_BUILD_JOBS=3 cargo test -p mat-device --all-features` PASS (mat-device uses these decoders).
- [ ] **Step 5: Commit** `refactor(ctrl): im/invoke の struct 走査を StructFields に`.

### Task 4: StructFields in `im/write.rs` (file 3/6)

Same rules as Task 3 (repeat: struct walks only; `expect_struct_start` + `StructFields::inside`; `use super::invoke::field_err;`; test first, PASS on old code; accepted deltas marked). Targets: `decode_write_attribute_data_ib`, `decode_write_request` top struct, `decode_write_response` top struct. Arrays `decode_write_requests_array` and the WriteResponses array stay. Test: `decoder_error_labels_are_stable` covering `decode_write_request` / `decode_write_response` labels. Run `cargo test -p mat-controller --lib im::` and `cargo test -p mat-device --all-features`. Commit `refactor(ctrl): im/write の struct 走査を StructFields に`.

### Task 5: StructFields in `im/event.rs` (file 4/6)

Same rules. Targets: `decode_event_path_ib`, EventFilterIB inner struct walk in `decode_event_filters` (outer array stays), `decode_event_reports` top struct (EventReports array stays), `decode_event_report_ib`, `decode_event_status_ib` and its inner StatusIB walk, `decode_event_data_ib`. Test `decoder_error_labels_are_stable` first. Same test commands. Commit.

### Task 6: StructFields in `im/subscribe.rs` (file 5/6)

Same rules. Targets: `decode_subscribe_request` top struct, `decode_subscribe_response` top struct. Test first. Same commands. Commit.

### Task 7: StructFields in `im/json.rs` (file 6/6)

Target: the `Value::StructStart` branch of `tlv_element_to_json_impl` — `let mut f = StructFields::inside(r); while let Some(el) = f.next_field().map_err(field_err("truncated struct"))? { ... }`; recursion uses `f.reader()`; non-context-tag container start → `skip_container(f.reader())?`. Array branch stays. Test first: `tlv_to_json_error_labels_are_stable` (`[0x15]` → `Malformed("truncated struct")`, `[0x15,0x19,0x18]` → `Tlv(InvalidType(0x19))`, `[]` → `Malformed("empty tlv")`, `[0x18]` → `Malformed("dangling container end")`), plus accepted delta for mid-element truncation `[0x15, 0x24, 0x01]` (context tag 1, uint8, missing byte) → `Malformed("truncated struct")`. Same commands (+ `cargo test -p mat-native` since mat-native pins json shape). Commit.

---

### Task 8: Promote `kvs::fs_util` and `asn1::oids` to files (item 4)

**Files:**
- Create: `crates/mat-controller/src/kvs/fs_util.rs` — contents of the inline `pub(crate) mod fs_util { ... }` (kvs.rs ~568-611), de-indented, with its doc comment turned into a `//!` module doc (drop the sentence "`lib.rs` は他レーンの担当なので独立ファイルにせず `kvs` 配下に置く。").
- Modify: `crates/mat-controller/src/kvs.rs` → `pub(crate) mod fs_util;` (Rust 2018 path: `src/kvs.rs` + `src/kvs/fs_util.rs` works).
- Create: `crates/mat-controller/src/asn1/oids.rs` — contents of `pub mod oids { ... }` (asn1.rs ~120-~170), doc comment → `//!`.
- Modify: `crates/mat-controller/src/asn1.rs` → `pub mod oids;` keeping the doc comment location sensible.

Paths `crate::kvs::fs_util::*` and `crate::asn1::oids::*` stay identical, so no call sites change. Pure move: diff of moved lines must be whitespace/doc-marker only.

- [ ] Move, `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --lib kvs:: asn1:: group` PASS, `cargo doc -p mat-controller --no-deps` no new warnings, commit `refactor(ctrl): kvs::fs_util / asn1::oids を独立ファイルへ`.

---

### Task 9: Test helper consolidation (item 5)

**Files:**
- Modify: `crates/mat-controller/src/test_support.rs` — add `pub fn multicast_ifaces() -> Vec<(String, u32)>` (moved verbatim from `dnssd/test_util.rs` ~137-175, doc comment updated to say group.rs and dnssd tests use it).
- Modify: `crates/mat-controller/src/dnssd/test_util.rs` — remove the fn. **dnssd is pure-move only.**
- Modify: `dnssd/browse.rs`, `dnssd/resolve.rs` test imports, `group.rs` tests (`crate::dnssd::test_util::multicast_ifaces()` ×5) → `crate::test_support::multicast_ifaces()`.
  - Caveat: `test_support` is `#[cfg(feature = "test-responder")]`. In `cargo test -p mat-controller --lib` the dev-dependency self-reference enables the feature for the lib-under-test (verify: if `crate::test_support` is not visible in lib unit tests, instead put it in `test_support` **and** gate the module as `#[cfg(any(test, feature = "test-responder"))]` in lib.rs — this doesn't add it to production builds).
- Create: `crates/mat-controller/tests/common/mod.rs` with the deduped helpers, each `#[allow(dead_code)]` (every test binary compiles its own copy): `env(name) -> String` (panic `"{name} required"`), `env_u64(name) -> u64` (0x-hex or decimal, panic messages as in live_remote), `env_node_id() -> u64` (from live_case_im — check its env var name and keep it), `request(socket, line) -> serde_json::Value`, `assert_ok(v, ctx)`. Only consolidate helpers whose bodies are identical or trivially equal; where a file's variant differs (e.g. `live_commissioning::env` returns `Option<String>`), keep it local and note it in the report.
- Modify: `tests/live_remote.rs`, `live_matd_native.rs`, `live_commission_real.rs`, `live_case_im.rs`, `live_commissioning.rs`, `live_commission_ble.rs`, `live_matd_group.rs` → `mod common;` + `use common::...`, delete local copies. Replace `"chip_tool_config.alpha.ini"` literals in `live_case_im.rs`, `live_remote.rs`, `live_commission_real.rs` with `kvs::ALPHA_INI_FILE`. Keep `#[ignore]` as is.
- [ ] Verify: `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --tests --no-run` and `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --all-features --tests --no-run` (ble feature for live_commission_ble; if bluer/libdbus build fails on this host, record it and use `cargo check -p mat-controller --features ble --tests`), `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --lib dnssd group` PASS, clippy `--all-targets` clean. Commit (2 commits: multicast_ifaces move, live helpers).

---

### Task 10: Single mDNS socket binder (item 6)

**Files:**
- Modify: `crates/mat-controller/src/dnssd/mod.rs` (`fn bind_mdns_socket` ~171) → `pub fn bind_mdns_socket(scope_id: u32, multicast_loop: bool) -> std::io::Result<UdpSocket>`.
- Modify: callers `dnssd/browse.rs:218`, `dnssd/resolve.rs:73,275`, `dnssd/cache.rs:288`, `dnssd/test_util.rs:188,208` → pass `false`.
- Modify: `crates/mat-device/src/net/mdns.rs` — delete `bind_advertiser_socket`, call `mat_controller::dnssd::bind_mdns_socket(iface_scope, true)`; move the still-relevant part of its doc comment (why loop is enabled for the advertiser) to the call site as a short comment.

**CRITICAL — do not change defaults:** the controller today never calls `set_multicast_loop_v6`, so it runs with the OS default (Linux: loop **on**). Therefore `multicast_loop: false` must mean **"leave the OS default untouched"**, NOT `set_multicast_loop_v6(false)`:
```rust
if multicast_loop {
    sock.set_multicast_loop_v6(true)?;
}
```
Document the param exactly that way in the doc comment ("`true` は明示的に loop を有効化（advertiser 用：同一ホストの querier に届ける）。`false` は OS 既定のまま触らない — 既存 querier の挙動を変えない").
Also check `dnssd/mod.rs` visibility: it's reachable as `mat_controller::dnssd` (lib.rs `pub mod dnssd`). Check the `dnssd/mod.rs` module docs mentioning `bind_mdns_socket` still read correctly.

- [ ] `CARGO_BUILD_JOBS=3 cargo test -p mat-controller --lib dnssd` PASS; `CARGO_BUILD_JOBS=3 cargo test -p mat-device --all-features` PASS (incl. discover/mdns tests); clippy clean. Commit `refactor(ctrl): mDNS ソケット bind を mat-controller の bind_mdns_socket に一本化`.

---

### Task 11: Final verification (controller does this, not a subagent)

- [ ] `CARGO_BUILD_JOBS=3 cargo test -p mat-controller` (includes loopback `case_self_handshake`, `pase_self_handshake`, `btp_pase_plumbing`).
- [ ] `CARGO_BUILD_JOBS=3 cargo test -p mat-device --all-features`
- [ ] `CARGO_BUILD_JOBS=3 task check` green.
- [ ] Write `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/ctrl.DONE.md`.
