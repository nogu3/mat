# Refactor lane `device` (mat-device + matv) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the duplicated / dead code the 2026-09-12 audit found in `crates/mat-device` (Tier 1, 4, 5, 6 items assigned to this lane) with **zero behavior change**, and split the two 4k-line files mechanically.

**Architecture:** Every task is either (a) "delete a private copy, call the already-`pub` original", (b) "extract one helper, call it from N sites", or (c) "pure move into submodules, keep `pub` paths via `pub use`". Bytes on the wire, error strings, record order and test expectations must not change. Tests are the guard: after every task `cargo test -p mat-device --all-features` must be green (no test deleted except where the function it pinned is itself deleted, and then only if the original crate pins the same thing).

**Tech Stack:** Rust workspace (`task check` = fmt:check + clippy + doc:check + test). Crate under edit: `crates/mat-device` (+ `crates/matv` only for running its tests). Branch `refactor/device`, worktree `/home/noguk/ghq/github.com/nogu3/mat-wt/device`.

**Spec:** `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/device.md` + `_common.md` + `audit.md` (Tier 4/5/6 + the mat-device lines of Tier 1).

## Global Constraints

- **Touch only** `crates/mat-device/**` and `crates/matv/**` (other lanes edit the rest of the repo concurrently; a touched file elsewhere = merge conflict).
- **Do not** touch `pase::derive_session_keys` duplicates or the SecureChannel constants (`GENERAL_CODE_*` / `SC_PROTOCOL_CODE_*` / `OPCODE_SIGMA*`) — the ctrl-proto lane is making them `pub` right now. Leave them in place; they go in the DONE follow-up list.
- `mat_controller::case::{random_p256_secret, eph_pub_bytes, encode_status_report, random_nonzero_u16}` **are already `pub`** (verified at `crates/mat-controller/src/case.rs:309,347,362,378`). The mat-device comments saying "`pub(crate)` there" are wrong.
- Zero behavior change: byte-identical TLV/mDNS output, identical error strings/statuses, identical call order of side effects. Where a task changes an observable value on purpose (only Task 10's `fast_cfg` unification, per the lane instruction), it must be recorded in the DONE file.
- `core/` stays I/O-free (`cargo check -p mat-device --no-default-features` must still pass — CI checks it).
- After each task: `cargo test -p mat-device --all-features` and `cargo check -p mat-device --no-default-features`; then commit on `refactor/device`. Never merge/push.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ
  ```
- Run `cargo fmt -p mat-device` before every commit; clippy must be warning-free (`cargo clippy -p mat-device --all-targets --all-features -- -D warnings`).
- 5 sessions share the machine and `target/`; cargo may be slow — wait, do not kill.
- Use `/usr/bin/grep` for counts (an `rtk` proxy hook rewrites bare `grep` and can corrupt counts).

---

### Task 1: Tier 1 — delete the "controller side is private" copies + small dead code

**Files:**
- Modify: `crates/mat-device/src/core/commissioning/mod.rs:547-573` (delete `random_p256_secret`, `public_key_bytes`), `:368-372` (`set_group_key_store`)
- Modify: `crates/mat-device/src/core/commissioning/attestation.rs:19,92-93`
- Modify: `crates/mat-device/src/net/pase.rs:118-131` (`status_report_failure`), `:138-170` (`recv_first`), `:235`
- Modify: `crates/mat-device/src/net/case.rs:125-160` (`recv_first`)
- Modify: `crates/mat-device/src/net/mod.rs` (new shared `recv_first`)
- Modify: `crates/mat-device/src/net/runtime.rs:455-471` (`random_session_id`), `:1132`, `:1172`, `:2760-2766` (its test)
- Modify: `crates/mat-device/src/core/events.rs:98-105` (`len`/`is_empty`)

**Interfaces:**
- Produces: `pub(crate) async fn recv_first(transport: &Transport) -> std::io::Result<(IncomingMessage, SocketAddr)>` in `crate::net`.

- [ ] **Step 1: Re-grep that every site still exists**

```bash
cd crates/mat-device
/usr/bin/grep -n 'fn random_p256_secret\|fn public_key_bytes\|fn status_report_failure\|fn recv_first\|fn random_session_id\|pub fn len\|pub fn is_empty' -r src
/usr/bin/grep -n '\.lock()' src/core/commissioning/mod.rs
```
Expected: the lines listed under **Files** (line numbers may drift by a few).

- [ ] **Step 2: `random_p256_secret` / `public_key_bytes` → mat-controller**

In `core/commissioning/attestation.rs:19` change the `use super::{...}` list: remove `public_key_bytes, random_p256_secret`, and add
```rust
use mat_controller::case::{eph_pub_bytes, random_p256_secret};
```
At `:92-93`:
```rust
        let secret = random_p256_secret();
        let op_public_key = eph_pub_bytes(&secret);
```
Delete `core/commissioning/mod.rs:547-573` (both functions and their doc comments). If mod.rs still imports `p256` only for these, remove the now-unused import (compiler tells you).

- [ ] **Step 3: `status_report_failure` in `net/pase.rs` → `encode_status_report`**

Delete `net/pase.rs:118-131`. Add `use mat_controller::case::encode_status_report;` to the imports. At the call site (`:235`, inside the error path that sends the failure StatusReport) replace `&status_report_failure()` with:
```rust
                        &encode_status_report(
                            GENERAL_CODE_FAILURE,
                            u32::from(PROTOCOL_ID_SECURE_CHANNEL),
                            SC_PROTOCOL_CODE_INVALID_PARAMETER,
                        ),
```
Keep the local `GENERAL_CODE_FAILURE` / `SC_PROTOCOL_CODE_INVALID_PARAMETER` constants (ctrl-proto lane owns their `pub`-ification). The bytes are identical: `encode_status_report` writes the same three LE fields (`case.rs:309-315`).

- [ ] **Step 4: One `recv_first` in `net/mod.rs`**

Append to `crates/mat-device/src/net/mod.rs`:
```rust
use std::net::SocketAddr;

use mat_controller::exchange::IncomingMessage;
use mat_controller::message::{
    MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, PROTOCOL_ID_SECURE_CHANNEL,
};
use mat_controller::transport::{Transport, MAX_DATAGRAM};

/// Reads the very first unsecured datagram from any sender — there's no
/// `ResponderExchange` yet to hand this off to (that's what `adopt` is
/// for), so this is a one-shot raw read, not a loop: PASE/CASE responder
/// drivers serve one initiator at a time and each handles exactly one
/// handshake. Malformed datagrams and standalone acks (which can't
/// legitimately arrive before any exchange exists) are skipped. Shared by
/// `net::pase` and `net::case` (each maps the `io::Error` into its own
/// error enum via `From`).
pub(crate) async fn recv_first(
    transport: &Transport,
) -> std::io::Result<(IncomingMessage, SocketAddr)> {
    loop {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = transport.recv_from(&mut buf).await?;
        let Ok((header, off)) = MessageHeader::decode(&buf[..n]) else {
            continue;
        };
        if header.session_id != 0 || header.security_flags != 0 {
            continue;
        }
        let Ok((proto, body_off)) = ProtocolHeader::decode(&buf[off..n]) else {
            continue;
        };
        if !proto.initiator {
            continue;
        }
        if proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
            && proto.opcode == OPCODE_MRP_STANDALONE_ACK
        {
            continue;
        }
        return Ok((
            IncomingMessage {
                header,
                proto,
                payload: buf[off + body_off..n].to_vec(),
            },
            from,
        ));
    }
}
```
Delete the two local copies (`net/pase.rs:133-170`, `net/case.rs:125-160`) and replace their call sites with `crate::net::recv_first(transport).await?` (both `NetPaseError` and `NetCaseError` already have `From<std::io::Error>` — `pase.rs:100`, `case.rs:92`). Remove imports that become unused (`MessageHeader`, `ProtocolHeader`, `OPCODE_MRP_STANDALONE_ACK`, `MAX_DATAGRAM`… — let rustc/clippy tell you).

- [ ] **Step 5: `random_session_id` → `random_nonzero_u16`**

Delete `net/runtime.rs:449-471` (doc + fn) — **keep** the doc paragraph about "why not collision-checked" by moving it onto the first call site as a `//` comment. At `:1132` and `:1172`:
```rust
                let local_session_id = mat_controller::case::random_nonzero_u16();
```
Delete the unit test `random_session_id_is_never_zero` (`:2760-2766`) — the invariant is pinned by mat-controller's own tests for `random_nonzero_u16`.

- [ ] **Step 6: `EventLog::len/is_empty` and `set_group_key_store`**

Delete `core/events.rs:98-105` (`len`, `is_empty`, and the `/// 現在の保持件数…` doc). Confirm no callers:
```bash
/usr/bin/grep -rn 'event_log\.\(len\|is_empty\)\|log\.\(len\|is_empty\)()' src tests
```
Expected: no output (the `ctx.events.len()` hits are `Vec`, not `EventLog`).

`core/commissioning/mod.rs:368-372`:
```rust
    pub fn set_group_key_store(&mut self, store: GroupKeyStore) {
        locked(&self.inner).group_key_store = Some(store);
    }
```

- [ ] **Step 7: Build, test, commit**

```bash
cargo fmt -p mat-device
cargo check -p mat-device --no-default-features
cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
git add crates/mat-device
git commit -m "refactor(mat-device): drop copies of already-pub mat-controller helpers (Tier 1)

random_p256_secret/public_key_bytes -> case::{random_p256_secret,eph_pub_bytes},
status_report_failure -> case::encode_status_report, random_session_id ->
case::random_nonzero_u16, one recv_first in net/mod.rs, EventLog::len/is_empty
(no callers) removed, set_group_key_store uses sync::locked like its siblings.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 2: Tier 4 — `core::tlv_value::{uint, str, bool, null}`

**Files:**
- Create: `crates/mat-device/src/core/tlv_value.rs`
- Modify: `crates/mat-device/src/core/mod.rs` (add `pub mod tlv_value;`)
- Modify (delete named copies): `core/datamodel.rs:1681-1693` (`uint_value`, `str_value`), `core/access_control.rs:482-490`, `core/commissioning/mod.rs:523-545` (`uint_value`, `bool_value`, `null_value`), `core/bridged_device_basic_information.rs:22-36` (`str_value`, `bool_value`), `core/generic_switch.rs:35-39` (`uint_tlv`)
- Modify (inline sites): `core/group_key_management.rs:459-468`, `core/general_diagnostics.rs:76-90`, `core/boolean_state.rs:53-57`, `core/identify.rs:75-79`, `core/onoff.rs:51-55`, `core/network_commissioning.rs:56-82`, `core/groups.rs:87-91`, `core/datamodel.rs:3995-3999` (test)

**Interfaces:**
- Produces:
  ```rust
  pub fn uint(v: u64) -> Vec<u8>;
  pub fn str(v: &str) -> Vec<u8>;
  pub fn bool(v: bool) -> Vec<u8>;
  pub fn null() -> Vec<u8>;
  ```

- [ ] **Step 1: Write the module with a byte-pin test**

`crates/mat-device/src/core/tlv_value.rs`:
```rust
//! Standalone scalar TLV values — the `ClusterHandler::read` contract is
//! "one `Tag::Anonymous`-tagged element", and every cluster used to carry
//! its own three-line `Writer::new(); put_*; finish()` copy of this.

use mat_controller::tlv::{Tag, Writer};

/// One anonymous unsigned-integer element.
pub fn uint(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_uint(Tag::Anonymous, v);
    w.finish()
}

/// One anonymous UTF-8 string element.
pub fn str(v: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_str(Tag::Anonymous, v);
    w.finish()
}

/// One anonymous boolean element.
pub fn bool(v: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_bool(Tag::Anonymous, v);
    w.finish()
}

/// One anonymous `null` element — for nullable attributes that must read
/// back distinct from a valid `0` (e.g. `AdminFabricIndex` while the
/// Administrator Commissioning window is closed).
pub fn null() -> Vec<u8> {
    let mut w = Writer::new();
    w.put_null(Tag::Anonymous);
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_are_single_anonymous_elements() {
        // Control tag 0x04 = uint8, 0x0C = utf8 string 1-byte length,
        // 0x08/0x09 = false/true, 0x14 = null (spec §A.7.1 / §A.8).
        assert_eq!(uint(7), vec![0x04, 0x07]);
        assert_eq!(str("ab"), vec![0x0C, 0x02, b'a', b'b']);
        assert_eq!(bool(false), vec![0x08]);
        assert_eq!(bool(true), vec![0x09]);
        assert_eq!(null(), vec![0x14]);
    }
}
```
Add `pub mod tlv_value;` to `core/mod.rs` (alphabetical, after `stimulus`).

- [ ] **Step 2: Run the pin test**

Run: `cargo test -p mat-device --all-features tlv_value`
Expected: PASS (if a control byte differs, fix the *test* to match `Writer`'s real output — the point is only to pin the helpers to the writer, not to assert the spec).

- [ ] **Step 3: Replace the named copies**

For each file, delete the local fn and rewrite calls:
- `datamodel.rs`: `uint_value(x)` → `tlv_value::uint(x)`, `str_value(x)` → `tlv_value::str(x)` (both production code and the test at `:3933`). Add `use crate::core::tlv_value;`.
- `access_control.rs`: `uint_value` → `tlv_value::uint`.
- `commissioning/mod.rs`: `uint_value`/`bool_value`/`null_value` → `tlv_value::{uint,bool,null}`. Check `commissioning/*.rs` siblings that import them via `use super::{...}` (`/usr/bin/grep -rn 'uint_value\|bool_value\|null_value' src/core/commissioning`).
- `bridged_device_basic_information.rs`: `str_value`/`bool_value` → `tlv_value::{str,bool}`.
- `generic_switch.rs`: `uint_tlv` → `tlv_value::uint`.

- [ ] **Step 4: Replace the inline sites**

Each inline site has the shape
```rust
            X => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, VALUE);
                Some(w.finish())
            }
```
(or `put_bool` / `put_null`). Rewrite to `X => Some(tlv_value::uint(VALUE)),` etc. Sites: `group_key_management.rs:459-468` (2× uint), `general_diagnostics.rs:76-90` (2× uint, 1× bool), `boolean_state.rs:53-57` (bool), `identify.rs:75-79` (uint — read the surrounding fn; it may not be a `match` arm), `onoff.rs:51-55` (bool), `network_commissioning.rs:56-82` (uint, bool, null), `groups.rs:87-91` (uint), `datamodel.rs:3995-3999` (test `bool`). Do **not** touch sites that write more than one element or use a non-anonymous tag. Remove `Writer`/`Tag` imports that become unused.

- [ ] **Step 5: Verify no copy remains, test, commit**

```bash
/usr/bin/grep -rn 'fn uint_value\|fn str_value\|fn bool_value\|fn null_value\|fn uint_tlv' src   # expect: nothing
cargo fmt -p mat-device && cargo check -p mat-device --no-default-features
cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
git add crates/mat-device && git commit -m "refactor(mat-device): one core::tlv_value for standalone scalar TLV (6 copies + 12 inline sites)

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 3: Tier 4 — `Node::commit_changes` + `handler_mut`, `Inner::purge_fabric_stores`

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs:631-651` (`drain_events`), `:696-712` (in `stimulate`), `:1350-1406` (`invoke_on_endpoint`), `:1523-1608` (`handle_write`)
- Modify: `crates/mat-device/src/core/commissioning/mod.rs` (add `impl Inner { fn purge_fabric_stores }` near `struct Inner`, `:269`)
- Modify: `crates/mat-device/src/core/commissioning/noc.rs:243-253`, `:288-298`; `crates/mat-device/src/core/commissioning/general_commissioning.rs:120-128`

**Interfaces:**
- Produces (private to `datamodel`):
  ```rust
  /// Returns Err(status) = UNSUPPORTED_ENDPOINT / UNSUPPORTED_CLUSTER.
  fn handler_mut<'a>(endpoints: &'a mut [(u16, Vec<Box<dyn ClusterHandler>>)], endpoint: u16, cluster: u32) -> Result<&'a mut dyn ClusterHandler, u8>;
  impl Node { fn commit_changes(&mut self, endpoint: u16, cluster: u32, ctx: &mut InvokeCtx, system_timestamp_ms: u64) -> (Vec<(u16, u32, u32)>, Vec<u64>); }
  ```
- Produces (private to `commissioning`): `impl Inner { pub(super) fn purge_fabric_stores(&self, fabric_index: u8) }`.

- [ ] **Step 1: `commit_changes`**

Add to `impl Node` right after `drain_events` (`datamodel.rs:651`):
```rust
    /// The bookkeeping every mutation path shares once a handler has run:
    /// the bare attribute ids it pushed on `ctx.changed` become concrete
    /// paths, this cluster's DataVersion is bumped once if anything
    /// changed, and whatever it emitted goes into the event log
    /// (`drain_events`). Returns `(changed_paths, event_numbers)`.
    fn commit_changes(
        &mut self,
        endpoint: u16,
        cluster: u32,
        ctx: &mut InvokeCtx,
        system_timestamp_ms: u64,
    ) -> (Vec<(u16, u32, u32)>, Vec<u64>) {
        let changed: Vec<(u16, u32, u32)> = ctx
            .changed
            .drain(..)
            .map(|attribute| (endpoint, cluster, attribute))
            .collect();
        if !changed.is_empty() {
            let version = self
                .versions
                .entry((endpoint, cluster))
                .or_insert(self.version_base);
            *version = version.wrapping_add(1);
        }
        let event_numbers = self.drain_events(endpoint, cluster, ctx, system_timestamp_ms);
        (changed, event_numbers)
    }
```
Replace the three inline blocks:
- `stimulate` (`:696-712`): `let (changed, event_numbers) = self.commit_changes(endpoint, cluster, &mut ctx, system_timestamp_ms);` then `Ok(StimulusOutcome { changed, event_numbers })`.
- `invoke_on_endpoint` (`:1387-1405`): `let (changed, _) = self.commit_changes(endpoint, cluster, ctx, 0);` then `Ok((reply, changed))`. Keep the "Timestamp 0: this dispatch has no clock" comment.
- `handle_write` (`:1590-1605`): `let (entry_changed, _) = self.commit_changes(endpoint, cluster, ctx, 0);` then `changed.extend(entry_changed);`.

Order of side effects is unchanged in all three (version bump, then event append).

- [ ] **Step 2: `handler_mut`**

Add as a free function next to `acl_allows` (`:1634`):
```rust
/// Resolves `(endpoint, cluster)` to its handler for a mutation
/// (`invoke_on_endpoint` / `handle_write`), with the IM status the caller
/// reports when either half is missing. A free function over the
/// `endpoints` field rather than a `Node` method so the caller can keep
/// borrowing `self.acl` / `self.versions` alongside the returned handler.
fn handler_mut<'a>(
    endpoints: &'a mut [(u16, Vec<Box<dyn ClusterHandler>>)],
    endpoint: u16,
    cluster: u32,
) -> Result<&'a mut dyn ClusterHandler, u8> {
    let Some((_, clusters)) = endpoints.iter_mut().find(|(id, _)| *id == endpoint) else {
        return Err(im::STATUS_UNSUPPORTED_ENDPOINT);
    };
    let Some(handler) = clusters.iter_mut().find(|h| h.cluster_id() == cluster) else {
        return Err(im::STATUS_UNSUPPORTED_CLUSTER);
    };
    Ok(handler.as_mut())
}
```
In `invoke_on_endpoint` replace the two `let Some(..) else { return Err(..) }` blocks with `let handler = handler_mut(&mut self.endpoints, endpoint, cluster)?;`. In `handle_write` replace the two blocks with
```rust
            let handler = match handler_mut(&mut self.endpoints, endpoint, cluster) {
                Ok(handler) => handler,
                Err(status) => {
                    results.push((endpoint, cluster, attribute, status));
                    continue;
                }
            };
```
`handler.invoke_privilege(..)` / `handler.write_privilege(..)` / `handler.invoke(..)` / `handler.write(..)` calls stay as they are (`&mut dyn ClusterHandler` auto-refs). If the borrow checker complains about `self.commit_changes` while `handler` is alive, the last use of `handler` is the `invoke`/`write` call — NLL ends the borrow there; do not restructure further.

- [ ] **Step 3: `purge_fabric_stores`**

In `commissioning/mod.rs`, after `struct Inner { .. }` (find the existing `impl Inner` if there is one; otherwise add):
```rust
impl Inner {
    /// Drops every per-fabric row `fabric_index` owns in the three shared
    /// stores (ACL, group keys, group membership). Called wherever a fabric
    /// leaves the store — `RemoveFabric` (both the persisted and the
    /// persist-failed branch) and the fail-safe rollback — so a later
    /// `AddNOC` reusing the index never inherits the previous occupant's
    /// rows (cross-fabric leak; see `handle_remove_fabric`'s comment).
    pub(super) fn purge_fabric_stores(&self, fabric_index: u8) {
        if let Some(store) = &self.acl_store {
            store.purge_fabric(fabric_index);
        }
        if let Some(store) = &self.group_key_store {
            store.purge_fabric(fabric_index);
        }
        if let Some(store) = &self.group_membership_store {
            store.purge_fabric(fabric_index);
        }
    }
}
```
Replace the three 9-line blocks (`noc.rs:243-251`, `noc.rs:288-296`, `general_commissioning.rs:120-128`) with `self.purge_fabric_stores(fabric_index);` (the `impl Inner` blocks in those files are `impl Inner` too — confirm with `/usr/bin/grep -n '^impl Inner' src/core/commissioning/*.rs`; if the methods are on a different receiver type, make `purge_fabric_stores` a method of that type instead).

- [ ] **Step 4: Test, commit**

```bash
cargo fmt -p mat-device && cargo check -p mat-device --no-default-features
cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
git add crates/mat-device && git commit -m "refactor(mat-device): Node::commit_changes + handler_mut, Inner::purge_fabric_stores

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 4: Tier 4 — `runtime.rs` small duplications + `pending_urgent` doc

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs:512-532` (`operational_advert`), `:533-538` (`MdnsCtx`), `:576-635` (`bring_up_mdns`), `:1304-1309` and `:1818-1824` (`remove_operational`), `:1329-1342`, `:1377-1378`, `:1636-1639` (`note_changed`+`note_events`), `:1718-1757` (`reconcile_admin_window`), `:1760-1810` (`close_window_on_commissioning_complete`), `:2210-2216` (doc)
- Modify: `crates/mat-device/src/net/subscription.rs` (add `note_outcome`)

**Interfaces:**
- Produces:
  ```rust
  impl MdnsCtx {
      fn commissionable_advert(&self, config: &DeviceConfig, discriminator: u16, cm: u8) -> CommissionableAdvert;
      fn operational_advert(&self, entry: &FabricEntry) -> OperationalAdvert;
      async fn retire_operational(&self, entry: &FabricEntry);
  }
  impl ActiveSubscription { pub fn note_outcome(&mut self, changed: &[(u16, u32, u32)], node: &Node); }
  ```

- [ ] **Step 1: `MdnsCtx` helpers**

Replace the free fn `operational_advert` (`:511-532`) and extend `MdnsCtx`:
```rust
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
    async fn retire_operational(&self, entry: &FabricEntry) {
        let cfid = compressed_fabric_id(&entry.root_public_key, entry.fabric_id);
        self.mdns
            .remove_operational(u64::from_be_bytes(cfid), entry.node_id)
            .await;
    }
}
```
`bring_up_mdns` (`:576-635`): build `let ctx = MdnsCtx { mdns, hostname, port, addr_v6 };` **immediately after** `MdnsAdvertiser::spawn`, then keep the same call sequence using `ctx.mdns.set_commissionable(Some(ctx.commissionable_advert(config, discriminator, cm))).await`, `ctx.mdns.add_operational(ctx.operational_advert(&entry)).await`, `ctx.mdns.announce().await`, `Ok(ctx)`. The struct literal in `reconcile_admin_window` (`:1733-1742`) becomes `ctx.commissionable_advert(config, discriminator, cm)`. `advertise_added_fabric` (`:1697-1702`) becomes `ctx.mdns.add_operational(ctx.operational_advert(entry)).await`. The two `remove_operational` blocks (`:1304-1309` in `on_fail_safe_expired`, `:1818-1824` in `retire_removed_fabric`) become `ctx.retire_operational(&entry).await`.

- [ ] **Step 2: `ActiveSubscription::note_outcome`**

In `net/subscription.rs` after `note_events` (`:161-167`):
```rust
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
```
(add `use crate::core::datamodel::Node;`). Replace the three call pairs (`runtime.rs:1335-1342`, `:1377-1378`, `:1636-1639`) with `sub.note_outcome(&changed, &self.state.node)` / `sub.note_outcome(&out.changed, &self.state.node)` / `sub.note_outcome(&changed, node)`. Fold the per-site comments into the method doc (delete them at the sites). If `self.state.node` is mutably borrowed at a site, bind `let node = &self.state.node;` before taking `self.state.subscription.as_mut()` — both are distinct fields, so the disjoint borrow compiles.

- [ ] **Step 3: Flatten `close_window_on_commissioning_complete`**

Replace the body (`:1767-1810`) with early returns, same conditions in the same order:
```rust
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
    // (keep the two existing Task 14 / Task 4 comments here, verbatim)
    *window = CommissioningWindow::Closed;
    comm_server.close_admin_window();
```

- [ ] **Step 4: `pending_urgent` doc (code unchanged)**

`runtime.rs:2212-2214` currently says "`pending_urgent` is kept set whenever something was left out". The code (`:2370`, `sub.pending_urgent = left_out > 0 && sub.pending_urgent;`) *keeps whatever `note_events` set* only when something was left out, and clears it otherwise. Rewrite the sentence to:
```
/// first; `sub.next_event` then advances to *what was actually sent* + 1,
/// and `pending_urgent` is left as `note_events` set it whenever something
/// was left out (so an urgent remainder follows at the next min-interval
/// instead of waiting for the max-interval keep-alive) and cleared once a
/// report drained everything. Nothing is lost — the log holds it until it
/// is reported (or until the FIFO overruns, which is the pre-existing cap).
```
Do **not** change the code line; whether it should be `left_out > 0 || ..` is the parent session's call.

`random_subscription_id` stays as is: its twin `random_session_id` left the crate in Task 1, so there is nothing left to share with.

- [ ] **Step 5: Test, commit**

```bash
cargo fmt -p mat-device && cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
git add crates/mat-device && git commit -m "refactor(mat-device): MdnsCtx advert helpers, ActiveSubscription::note_outcome, flat close_window; pending_urgent doc matches code

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 5: Tier 4 — list-attribute write decoders ×2 + `decode_struct_uint_field` ×3

**Files:**
- Modify: `crates/mat-device/src/core/tlv_value.rs` (add decoders + tests)
- Modify: `crates/mat-device/src/core/access_control.rs:522-546` (`decode_acl_entries`, `decode_single_acl_entry`), `:413-460` (`write`)
- Modify: `crates/mat-device/src/core/group_key_management.rs:790-826` (`decode_group_key_map_entries`, `decode_single_group_key_map_entry`), `:607-640` (`write`), `:852-875` (`decode_key_set_id`)
- Modify: `crates/mat-device/src/core/identify.rs:115-135` (`decode_identify_time`), `crates/mat-device/src/core/groups.rs:217-245` (`decode_group_id`)

**Interfaces:**
- Produces in `core::tlv_value`:
  ```rust
  /// `data_tlv` = array[struct...]; `body` reads one struct after its StructStart was consumed.
  pub fn decode_struct_list<T>(data_tlv: &[u8], body: impl Fn(&mut Reader<'_>) -> Option<T>) -> Option<Vec<T>>;
  /// `data_tlv` = one bare struct (the ListIndex-null append path).
  pub fn decode_single_struct<T>(data_tlv: &[u8], body: impl Fn(&mut Reader<'_>) -> Option<T>) -> Option<T>;
  /// Top-level `Context(field)` uint of a command-fields struct; nested containers are skipped; last occurrence wins.
  pub fn decode_struct_uint_field(fields_tlv: &[u8], field: u8) -> Option<u64>;
  ```

- [ ] **Step 1: Add the decoders (with `groups.rs::decode_group_id` as the reference semantics)**

Append to `core/tlv_value.rs`:
```rust
use mat_controller::tlv::{Reader, Value};

/// Decodes the full-replace form of a list-attribute write: `data_tlv` is
/// an anonymous array whose elements are structs, and `body` reads one
/// element's fields (its `StructStart` already consumed) up to and
/// including its `ContainerEnd`. Any shape mismatch is `None` — callers
/// map that to `STATUS_CONSTRAINT_ERROR`.
pub fn decode_struct_list<T>(
    data_tlv: &[u8],
    body: impl Fn(&mut Reader<'_>) -> Option<T>,
) -> Option<Vec<T>> {
    let mut r = Reader::new(data_tlv);
    let el = r.next().ok()??;
    if el.value != Value::ArrayStart {
        return None;
    }
    let mut entries = Vec::new();
    loop {
        let el = r.next().ok()??;
        match el.value {
            Value::ContainerEnd => break,
            Value::StructStart => entries.push(body(&mut r)?),
            _ => return None,
        }
    }
    Some(entries)
}

/// Decodes the `ListIndex = null` append form: `data_tlv` is one bare
/// struct (not wrapped in an array). Same `body` contract as
/// [`decode_struct_list`].
pub fn decode_single_struct<T>(
    data_tlv: &[u8],
    body: impl Fn(&mut Reader<'_>) -> Option<T>,
) -> Option<T> {
    let mut r = Reader::new(data_tlv);
    let el = r.next().ok()??;
    if el.value != Value::StructStart {
        return None;
    }
    body(&mut r)
}

/// Reads `Context(field)` (an unsigned integer) off the **top level** of a
/// command-fields struct — `{0: GroupID}`, `{0: IdentifyTime}`, `{0:
/// GroupKeySetID}` all share this shape. Nested containers are skipped
/// wholesale (a same-numbered tag inside one is not the field). A repeated
/// tag: last one wins. Malformed TLV or a missing field is `None`; callers
/// map that to `STATUS_INVALID_COMMAND`.
pub fn decode_struct_uint_field(fields_tlv: &[u8], field: u8) -> Option<u64> {
    let mut r = Reader::new(fields_tlv);
    match r.next() {
        Ok(Some(el)) if el.value == Value::StructStart => {}
        _ => return None,
    }
    let mut found = None;
    loop {
        match r.next() {
            Ok(Some(el)) => match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(t), Value::Uint(v)) if t == field => found = Some(v),
                (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                    mat_controller::tlv::skip_container(&mut r).ok()?;
                }
                _ => {}
            },
            _ => return None,
        }
    }
    found
}
```
Add tests in the same `mod tests`:
```rust
    #[test]
    fn struct_uint_field_reads_top_level_only_and_skips_nested() {
        // {0: 7, 1: {0: 99}} — the nested Context(0) must not win.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 7);
        w.start_struct(Tag::Context(1));
        w.put_uint(Tag::Context(0), 99);
        w.end_container();
        w.end_container();
        assert_eq!(decode_struct_uint_field(&w.finish(), 0), Some(7));
        assert_eq!(decode_struct_uint_field(&[0x15, 0x18], 0), None); // {} — missing
        assert_eq!(decode_struct_uint_field(&[0x04, 0x01], 0), None); // not a struct
    }

    #[test]
    fn struct_list_and_single_struct_share_one_body_reader() {
        let body = |r: &mut Reader<'_>| -> Option<u64> {
            let mut v = None;
            loop {
                let el = r.next().ok()??;
                match (el.tag, el.value) {
                    (_, Value::ContainerEnd) => break,
                    (Tag::Context(1), Value::Uint(x)) => v = Some(x),
                    _ => {}
                }
            }
            v
        };
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for x in [1u64, 2] {
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(1), x);
            w.end_container();
        }
        w.end_container();
        assert_eq!(decode_struct_list(&w.finish(), body), Some(vec![1, 2]));

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 5);
        w.end_container();
        let single = w.finish();
        assert_eq!(decode_single_struct(&single, body), Some(5));
        assert_eq!(decode_struct_list(&single, body), None); // bare struct is not a list
    }
```
Run: `cargo test -p mat-device --all-features tlv_value` — expect PASS (adjust the raw byte literals `[0x15,0x18]`/`[0x04,0x01]` if `Writer` output shows different control bytes; build them with `Writer` if unsure).

- [ ] **Step 2: Rewire the two `write` impls**

`access_control.rs`: delete `decode_acl_entries` and `decode_single_acl_entry` (`:522-546`), keep `decode_acl_entry_body` (its doc says the test helper `decode_entries_for_test` also uses it — leave that). In `write`:
```rust
            let Some(entry) = tlv_value::decode_single_struct(data_tlv, decode_acl_entry_body) else {
```
and
```rust
            let Some(entries) = tlv_value::decode_struct_list(data_tlv, decode_acl_entry_body) else {
```
`group_key_management.rs`: delete `decode_group_key_map_entries` / `decode_single_group_key_map_entry` (`:790-826`), keep `decode_group_key_map_entry_body`; same two substitutions in its `write`. Move the deleted fns' doc comments (wire-shape notes referencing `mat_controller::im::encode_group_key_map_tlv`) onto `decode_group_key_map_entry_body`. If `decode_acl_entry_body`'s signature is `fn(&mut Reader) -> Option<..>` with an elided lifetime it satisfies `impl Fn(&mut Reader<'_>) -> Option<T>` directly; if rustc complains about lifetimes, pass a closure `|r| decode_acl_entry_body(r)`.

- [ ] **Step 3: Rewire the three command-field decoders**

- `groups.rs:217-245`: body of `decode_group_id` becomes
  ```rust
  fn decode_group_id(fields_tlv: &[u8]) -> Option<u16> {
      tlv_value::decode_struct_uint_field(fields_tlv, 0).and_then(|v| u16::try_from(v).ok())
  }
  ```
  (keep the fn and doc — it is the crate's named "GroupID field" reader; only its body collapses).
- `group_key_management.rs:852-875` `decode_key_set_id`: same one-liner with field `0`.
- `identify.rs:115-135` `decode_identify_time`: same one-liner. **Note in the commit message**: the old body did not skip nested containers, so a (malformed) `{0: {0: 5}}` used to yield `Some(5)` and now yields `None` — the reference semantics are `decode_group_id`'s, per the lane instruction. Real `Identify` requests are flat, so nothing observable changes.

Equivalence of "last wins": every old loop assigned `x = u16::try_from(v).ok()` on each hit (so an out-of-range *last* hit produced `None`); the new code keeps the last `u64` and converts once — identical results for every input.

- [ ] **Step 4: Test, commit**

```bash
/usr/bin/grep -rn 'fn decode_acl_entries\|fn decode_single_acl_entry\|fn decode_group_key_map_entries\|fn decode_single_group_key_map_entry' src   # expect: nothing
cargo fmt -p mat-device && cargo check -p mat-device --no-default-features
cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
git add crates/mat-device && git commit -m "refactor(mat-device): tlv_value::{decode_struct_list,decode_single_struct,decode_struct_uint_field} replace 2 list-write decoders + 3 field readers

decode_identify_time now skips nested containers like decode_group_id
(reference semantics per lane instruction); flat requests unchanged.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 6: Tier 4 — `mdns_records.rs` one record-set builder

**Files:**
- Modify: `crates/mat-device/src/core/mdns_records.rs:193-455`

**Interfaces:**
- Produces (private):
  ```rust
  struct ServiceRecords { ptr_owners: Vec<String>, instance_full: String, host_full: String, port: u16, txt: Vec<String>, addr_v6: Ipv6Addr }
  fn commissionable_records(ad: &CommissionableAdvert) -> ServiceRecords;
  fn operational_records(ad: &OperationalAdvert) -> ServiceRecords;
  impl ServiceRecords {
      fn respond(&self, q_name: &str, unicast: bool) -> Option<Vec<u8>>;
      fn push_all(&self, answers: &mut Vec<Rr>, ttl: &dyn Fn(u32) -> u32);
  }
  ```

- [ ] **Step 1: Confirm the 17 byte-pinning tests are the guard**

Run: `cargo test -p mat-device --all-features mdns_records` — expect PASS, note the count (should be ≥ 17). These tests pin record order/TTL/cache-flush; they are the only spec you need.

- [ ] **Step 2: Introduce `ServiceRecords`**

Replace `CommissionableNames`/`commissionable_names`/`OperationalNames`/`operational_names` with:
```rust
/// Everything one advertised service contributes to a response or an
/// announcement: the PTR owners it answers under (`_matterc._udp.local`
/// plus the two discriminator subtypes for commissionable; the bare
/// `_matter._tcp.local` for operational), its SRV/TXT owner, its AAAA
/// owner, and the SRV/TXT/AAAA payloads. Built per advert by
/// `commissionable_records` / `operational_records`; the record *order*
/// (PTRs in `ptr_owners` order, then SRV, TXT, AAAA) is what the
/// byte-pinning tests below assert, so keep it.
struct ServiceRecords {
    ptr_owners: Vec<String>,
    instance_full: String,
    host_full: String,
    port: u16,
    txt: Vec<String>,
    addr_v6: Ipv6Addr,
}

/// Long-discriminator subtype (spec §4.3.1: `_L<discriminator>._sub.
/// _matterc._udp.local`, decimal, no zero padding) and short-discriminator
/// subtype (`_S<short>._sub...`, `short` = top 4 bits of the 12-bit
/// discriminator — same derivation as `commissioning::manual_pairing_code`
/// and `mat-native::commission`'s short-code matcher). TXT: `D`
/// (discriminator), `VP` (`<vendor>+<product>`), `CM`/`SII`/`SAI`.
fn commissionable_records(ad: &CommissionableAdvert) -> ServiceRecords {
    let short = (ad.discriminator >> 8) as u8;
    ServiceRecords {
        ptr_owners: vec![
            "_matterc._udp.local".to_string(),
            format!("_L{}._sub._matterc._udp.local", ad.discriminator),
            format!("_S{short}._sub._matterc._udp.local"),
        ],
        instance_full: format!("{}._matterc._udp.local", ad.instance),
        host_full: format!("{}.local", ad.hostname),
        port: ad.port,
        txt: vec![
            format!("D={}", ad.discriminator),
            format!("VP={}+{}", ad.vendor_id, ad.product_id),
            format!("CM={}", ad.cm),
            "SII=300".to_string(),
            "SAI=300".to_string(),
        ],
        addr_v6: ad.addr_v6,
    }
}

/// Operational service: no subtypes (spec §4.3.1 has none for
/// `_matter._tcp`), TXT `SII`/`SAI` only — the querier's
/// `resolve_operational` reads just those two keys.
fn operational_records(ad: &OperationalAdvert) -> ServiceRecords {
    let instance = operational_instance(&ad.compressed_fabric_id, ad.node_id);
    ServiceRecords {
        ptr_owners: vec!["_matter._tcp.local".to_string()],
        instance_full: format!("{instance}._matter._tcp.local"),
        host_full: format!("{}.local", ad.hostname),
        port: ad.port,
        txt: vec!["SII=300".to_string(), "SAI=300".to_string()],
        addr_v6: ad.addr_v6,
    }
}

impl ServiceRecords {
    fn srv(&self, ttl: u32) -> Rr {
        Rr::Srv {
            owner: self.instance_full.clone(),
            target: self.host_full.clone(),
            port: self.port,
            ttl,
        }
    }
    fn txt(&self, ttl: u32) -> Rr {
        Rr::Txt {
            owner: self.instance_full.clone(),
            strings: self.txt.clone(),
            ttl,
        }
    }
    fn aaaa(&self, ttl: u32) -> Rr {
        Rr::Aaaa {
            owner: self.host_full.clone(),
            addr: self.addr_v6,
            ttl,
        }
    }

    /// Answers one question, if `q_name` is something this service can
    /// answer: a PTR owner → PTR (answer) + SRV/TXT/AAAA (additional); the
    /// instance's own name → SRV/TXT (answer) + AAAA (additional); the
    /// hostname → AAAA (answer). `None` = not for us.
    fn respond(&self, q_name: &str, unicast: bool) -> Option<Vec<u8>> {
        let (srv, txt, aaaa) = (self.srv(HOST_TTL), self.txt(PTR_TXT_TTL), self.aaaa(HOST_TTL));
        if self.ptr_owners.iter().any(|o| q_name.eq_ignore_ascii_case(o)) {
            let ptr = Rr::Ptr {
                owner: q_name.to_string(),
                target: self.instance_full.clone(),
                ttl: PTR_TXT_TTL,
            };
            return Some(encode_message(&[ptr], &[srv, txt, aaaa], unicast));
        }
        if q_name.eq_ignore_ascii_case(&self.instance_full) {
            return Some(encode_message(&[srv, txt], &[aaaa], unicast));
        }
        if q_name.eq_ignore_ascii_case(&self.host_full) {
            return Some(encode_message(&[aaaa], &[], unicast));
        }
        None
    }

    /// Every record of this service, announcement order: one PTR per
    /// owner, then SRV, TXT, AAAA. `ttl` maps each record type's normal
    /// TTL to the one to emit (identity for an announcement, `|_| 0` for a
    /// goodbye).
    fn push_all(&self, answers: &mut Vec<Rr>, ttl: &dyn Fn(u32) -> u32) {
        for owner in &self.ptr_owners {
            answers.push(Rr::Ptr {
                owner: owner.clone(),
                target: self.instance_full.clone(),
                ttl: ttl(PTR_TXT_TTL),
            });
        }
        answers.push(self.srv(ttl(HOST_TTL)));
        answers.push(self.txt(ttl(PTR_TXT_TTL)));
        answers.push(self.aaaa(ttl(HOST_TTL)));
    }
}
```
Then the three public builders collapse:
```rust
pub fn encode_commissionable_response(q_name: &str, ad: &CommissionableAdvert, unicast: bool) -> Option<Vec<u8>> {
    commissionable_records(ad).respond(q_name, unicast)
}
pub fn encode_operational_response(q_name: &str, ad: &OperationalAdvert, unicast: bool) -> Option<Vec<u8>> {
    operational_records(ad).respond(q_name, unicast)
}
fn encode_announcement_or_goodbye(commissionable: Option<&CommissionableAdvert>, operational: &[OperationalAdvert], ttl_override: Option<u32>) -> Vec<u8> {
    let ttl = |normal: u32| ttl_override.unwrap_or(normal);
    let mut answers = Vec::new();
    if let Some(ad) = commissionable {
        commissionable_records(ad).push_all(&mut answers, &ttl);
    }
    for ad in operational {
        operational_records(ad).push_all(&mut answers, &ttl);
    }
    encode_message(&answers, &[], false)
}
```
Keep the existing doc comments on the three `pub fn`s and `encode_unsolicited_announcement`/`encode_goodbye` verbatim.

- [ ] **Step 3: Run the pin tests, commit**

```bash
cargo test -p mat-device --all-features mdns_records   # same count as Step 1, all PASS
cargo fmt -p mat-device && cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
git add crates/mat-device && git commit -m "refactor(mat-device): mdns_records — one ServiceRecords builder for response/announcement/goodbye (record order byte-pinned by tests)

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 7: Tier 4 — split `Device::new` (266 lines) into phases

**Files:**
- Modify: `crates/mat-device/src/device.rs:262-528`

**Interfaces:**
- Produces (private free fns in `device.rs`, called in this order by `Device::new`):
  ```rust
  fn build_attestation(config: &DeviceConfig) -> Result<DevAttestation, DeviceError>;          // + writes <store>/paa/paa.der
  struct SharedStores { acl: AclStore, gk: GroupKeyStore, membership: GroupMembershipStore }
  fn build_shared_stores(config: &DeviceConfig, comm_server: &mut CommissioningServer) -> SharedStores;
  fn build_root_node(config: &DeviceConfig, comm_server: CommissioningServer, stores: &SharedStores, unique_id: &str) -> Result<Node, DeviceError>;
  struct BridgeLayout { states: Vec<(String, BridgedState)>, endpoint_by_device: HashMap<String, u16> }
  fn build_bridge(config: &DeviceConfig, node: &mut Node, membership: &GroupMembershipStore, unique_id: &str) -> Result<BridgeLayout, DeviceError>;
  fn bind_sockets(config: &DeviceConfig, stores: &SharedStores) -> Result<(Arc<Transport>, SocketAddr, GroupRx), DeviceError>;
  ```

- [ ] **Step 1: Cut the body at its existing comment boundaries**

The current body already has 5 phases; move each into a free fn **without reordering any statement or side effect**:
1. `:263-283` (`create_dir_all(store_dir)` stays in `new`; attestation match + PAA dir/write) → `build_attestation`. Return type: whatever `x509::generate_dev_attestation` returns (`DevAttestation` — check the `use` line).
2. `:285-320` (FabricStore + `CommissioningServer::new` stay in `new`; the three `*_store` + `set_*` calls) → `build_shared_stores(config, &mut comm_server) -> SharedStores`.
3. `:322-395` (`load_or_create_unique_id` stays in `new`; `load_basic_info`, `Node::with_root_endpoint_persisted`, version seed, `set_acl_store`, `into_cluster_handlers`, the 7 `add_cluster(0, ..)`) → `build_root_node(config, comm_server, &stores, &unique_id) -> Result<Node, DeviceError>` (takes `comm_server` by value because `into_cluster_handlers` consumes it — **check**: `Device` also stores `comm_server`; if `into_cluster_handlers(&self)` clones an `Arc`, take `&CommissioningServer` instead. Read `:376` to see which).
4. `:397-476` (ledger load/assign/save, membership prune, EP1 aggregator, bridged endpoints loop, `endpoint_by_device`, event log seed) → `build_bridge(config, &mut node, &stores.membership, &unique_id) -> Result<BridgeLayout, DeviceError>`. `node.set_event_log(..)` stays inside (it is the last thing that touches `node`).
5. `:478-513` (stimulus channel stays in `new`; `bind_addr`/UDP bind/`Transport`, `iface_index`, group socket, `GroupRx`) → `bind_sockets(config, &stores) -> Result<(Arc<Transport>, SocketAddr, GroupRx), DeviceError>`.

Resulting `new`:
```rust
    pub fn new(config: DeviceConfig) -> Result<Self, DeviceError> {
        std::fs::create_dir_all(&config.store_dir).map_err(DeviceError::Io)?;
        let dev = build_attestation(&config)?;
        let fabric_store = FabricStore::with_persist(Box::new(store_in_dir(&config.store_dir)));
        let mut comm_server = CommissioningServer::new(dev, fabric_store);
        let stores = build_shared_stores(&config, &mut comm_server);
        let unique_id = load_or_create_unique_id(&config.store_dir).map_err(DeviceError::Io)?;
        let mut node = build_root_node(&config, /* comm_server or &comm_server */, &stores, &unique_id)?;
        let BridgeLayout { states, endpoint_by_device } =
            build_bridge(&config, &mut node, &stores.membership, &unique_id)?;
        let (stimulus_handle, stimuli) = StimulusHandle::channel(STIMULUS_CHANNEL_CAPACITY);
        let (transport, local_addr, group) = bind_sockets(&config, &stores)?;
        Ok(Self { config, transport, local_addr, node, comm_server, group, states, stimulus_handle, stimuli: Some(stimuli), endpoint_by_device })
    }
```
Move every explanatory comment block with the code it explains (they are the design record; do not drop any). Each phase fn gets a one-line doc naming the phase.

- [ ] **Step 2: Test, commit**

`device.rs` has its own tests (`bridge_topology_and_ledger_stability`, `stale_group_membership_of_removed_devices_is_pruned_on_start`, `bridged_unique_ids_are_string32_stable_and_distinct`, `group_socket_is_bound_on_the_configured_port`) plus every `tests/*.rs` integration test drives `Device::new`.
```bash
cargo fmt -p mat-device && cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
cargo test -p matv
git add crates/mat-device && git commit -m "refactor(mat-device): split Device::new into attestation / stores / root node / bridge / sockets phases

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 8: Tier 5 — split `core/datamodel.rs` (4350 lines) into a module directory

**Files:**
- Create: `crates/mat-device/src/core/datamodel/{mod.rs, read.rs, events.rs, descriptor.rs, basic_information.rs, tests.rs}`
- Delete: `crates/mat-device/src/core/datamodel.rs` (use `git mv src/core/datamodel.rs src/core/datamodel/mod.rs` first so history follows)

**Interfaces:**
- Every path that is `pub` today stays valid: `crate::core::datamodel::{InvokeCtx, ImOutcome, ReadCtx, InvokeReply, ClusterHandler, ImServerError, Node, DescriptorHandler, BasicInfoPersist}` (+ anything else `pub` — grep `^pub ` in the file first and preserve each via `pub use`).
- Items that move into a child but are used by `mod.rs`/siblings/tests become `pub(super)`.

- [ ] **Step 1: Inventory the public surface**

```bash
cd crates/mat-device
/usr/bin/grep -n '^pub \|^    pub fn\|^pub(crate)' src/core/datamodel.rs > /tmp/dm_pub.txt
/usr/bin/grep -rn 'datamodel::' src tests ../matv/src | /usr/bin/grep -v '^src/core/datamodel.rs' | sed 's/.*datamodel::\([A-Za-z_{}, ]*\).*/\1/' | sort -u
```
Keep the second list: every name in it must resolve after the split.

- [ ] **Step 2: Move by line range (numbers are post-Task-2/3 approximations — locate by the fn names)**

| Destination | Content (by name) |
|---|---|
| `mod.rs` | module doc `//!`, `use`s, `INITIAL_DATA_VERSION`, `DATA_MODEL_REVISION`, `InvokeCtx`, `ImOutcome`, `ReadCtx`, `InvokeReply`, `ClusterHandler`, `ImServerError`, `Node` struct + `ExpandCtx` + `InvokeOnEndpointOk`, `impl Node { new, with_root_endpoint*, add_endpoint, add_cluster, data_version, set_data_version_base, set_acl_store, handle_im, invoke_on_endpoint, handle_invoke, handle_group_invoke, handle_write, commit_changes }`, `impl Default for Node`, `acl_allows`, `is_global_attribute`, `read_privilege_for`, `handler_mut`; then `mod read; mod events; mod descriptor; mod basic_information; pub use descriptor::DescriptorHandler; pub use basic_information::BasicInfoPersist; #[cfg(test)] mod tests;` |
| `read.rs` | `impl Node { handle_read, read_chunks, read_entries, has_readable_path, expand_endpoint, expand_cluster, expand_attribute, read_allowed, read_attribute_value }`, `encode_server_list`, `encode_parts_list`, `encode_attribute_list`, `GLOBAL_ATTRIBUTE_IDS`, `encode_command_list` |
| `events.rs` | `impl Node { set_event_log, next_event_number, recent_events, drain_events, stimulate, event_entries, concrete_event_path_status, handler_for, has_readable_event_path }`, `event_path_matches` |
| `descriptor.rs` | `DescriptorHandler` + its `impl`s |
| `basic_information.rs` | `BasicInfoPersist`, `BasicInformationHandler`, `CAPABILITY_MINIMA_*`, `SPECIFICATION_VERSION`, `NODE_LABEL_MAX_CHARS`, `LOCATION_CHARS`, `decode_utf8_write`, `impl ClusterHandler for BasicInformationHandler`, `encode_capability_minima` |
| `tests.rs` | the entire `mod tests` body (`use super::*;` + everything) |

Rules:
- A child `impl Node { .. }` block may keep its methods **private** if only `Node` methods in other files call them? **No** — privacy is per module: a private method defined in `read.rs` is invisible to `mod.rs`. Make every moved fn/method that is called from another file `pub(super)`; leave `pub` ones `pub`.
- `ExpandCtx` and `Node` fields are defined in `mod.rs`; children can use them as-is (descendants see the parent's private items).
- `tests.rs` is a child too: `use super::*;` gives it everything in `mod.rs` plus the `pub(super)` items of siblings via `super::read::..`? **No** — `pub(super)` in `read.rs` = visible in `datamodel` and its descendants, and `tests` is a descendant, but the *path* is `super::read::name` unless `mod.rs` re-exports. Simplest: in `mod.rs` add `#[cfg(test)] pub(super) use read::{encode_..., ...};` for the private helpers tests call (find them with `/usr/bin/grep -n 'encode_server_list\|encode_parts_list\|encode_attribute_list\|encode_command_list\|event_path_matches\|decode_utf8_write\|encode_capability_minima\|is_global_attribute\|acl_allows\|read_privilege_for' src/core/datamodel/tests.rs`). Alternatively move such a helper's test next to the helper — either is fine; do not change assertions.
- `#[allow(dead_code)]` must not appear; if a `pub(super)` is unused after the move, it was over-widened — narrow it.

- [ ] **Step 3: Compile, run, compare test count**

Before the split: `cargo test -p mat-device --all-features datamodel 2>&1 | /usr/bin/grep 'test result'` — record the number. After: same command, same number; all PASS. Also `cargo check -p mat-device --no-default-features` and `cargo doc -p mat-device --no-deps` (broken intra-doc links show up here).

- [ ] **Step 4: Commit**

```bash
cargo fmt -p mat-device && cargo clippy -p mat-device --all-targets --all-features -- -D warnings
git add -A crates/mat-device/src/core
git commit -m "refactor(mat-device): split core/datamodel.rs into datamodel/{mod,read,events,descriptor,basic_information,tests}.rs (pure move)

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 9: Tier 5 — split `net/runtime.rs` (4186 lines) into a module directory

**Files:**
- Create: `crates/mat-device/src/net/runtime/{mod.rs, classify.rs, window.rs, mdns_up.rs, serve.rs, subscribe.rs, tests.rs, socket_tests.rs}`
- Delete: `crates/mat-device/src/net/runtime.rs` (`git mv` to `runtime/mod.rs` first)

**Interfaces:**
- `crate::net::runtime` is `pub(crate)`; the only external entry is `pub(crate) async fn run(..)` (`device.rs:603`). Keep it in `mod.rs`.
- Everything moved out of `mod.rs` that another file in `runtime/` uses becomes `pub(super)`.

- [ ] **Step 1: Move by name**

| Destination | Content |
|---|---|
| `mod.rs` | module doc, `use`s, `run`, `NodeState` + impl, `Runtime` + impl (`boot`, `serve_forever`, `on_subscription_due`, `on_unicast_datagram`, `on_secured_datagram`, `on_unsecured_datagram`, `establish_session`, `install_session`, `on_mdns_retry`, `on_commissioning_window_expired`, `on_fail_safe_expired`, `on_group_datagram`, `on_stimulus`), `sync_group_joins`, then `mod classify; mod window; mod mdns_up; mod serve; mod subscribe; #[cfg(test)] mod tests; #[cfg(test)] mod socket_tests;` + `use` of the `pub(super)` items |
| `classify.rs` | `OPCODE_CASE_SIGMA1`, `UnsecuredFlow`, `classify_unsecured`, `admit_unsecured`, `remove_fabric_drops_session` |
| `window.rs` | `PASE_ITERATIONS`, `COMMISSIONING_WINDOW_DURATION`, `CommissioningWindow` + impl, `commissioning_window_deadline`, `apply_window_request`, `pase_config_for_window`, `advert_params_for_window`, `AdminWindowAction`, `admin_window_action`, `fail_safe_expiry_deadline` |
| `mdns_up.rs` | `random_hex_name`, `iface_link_local_addr`, `MdnsCtx` + impl (Task 4), `bring_up_mdns`, `MdnsRetry` + consts + impl, `mdns_retry_deadline`, `advertise_added_fabric`, `reconcile_admin_window`, `close_window_on_commissioning_complete`, `retire_removed_fabric` |
| `serve.rs` | `reply_cfg`, `REPORT_CHUNK_BUDGET`, `REPORT_STATUS_TIMEOUT`, `ServeOutcome`, `serve_secured`, `drain_buffered_requests`, `serve_secured_message`, `ServeState`, `MAX_DEFERRED_REQUESTS`, `await_peer_status_ok`, `is_status_response_ok`, `serve_read_request_chunked`, `session_subject` |
| `subscribe.rs` | `MIN_MAX_INTERVAL_S`, `MAX_MAX_INTERVAL_S`, `random_subscription_id`, `SubscribeOutcome`, `subscription_deadline`, `serve_subscribe_request`, `chunk_events`, `fit_events`, `send_subscription_report` |
| `tests.rs` | `mod tests` content from its start through the end of `fail_safe_expiry_deadline_resolves_once_the_armed_window_passes` (the non-socket unit tests: `event_entry`, `test_config`, classify/admit/window/mdns-retry/fail-safe/`remove_fabric_*` tests) |
| `socket_tests.rs` | the 5 `#[tokio::test]` socket tests: `serve_secured_drains_and_serves_a_cross_exchange_piggybacked_request`, `read_request_chunked_flow_round_trips_two_or_more_chunks`, `subscription_priming_round_trips_multiple_chunks`, `a_request_interleaved_into_a_chunk_status_wait_is_not_lost`, `a_timed_invoke_on_the_same_exchange_is_served_not_dropped` (harness dedupe is Task 10 — here only move) |

`ServeState` is built by `NodeState::serve_state` in `mod.rs` and destructured in `serve.rs`: its fields need `pub(super)`. Same for `MdnsCtx` fields if `mod.rs` reads them (`self.state.mdns.as_ref()`), and `Runtime`/`NodeState` fields read by `serve.rs`.

- [ ] **Step 2: Compile, run, compare**

```bash
cargo test -p mat-device --all-features runtime 2>&1 | /usr/bin/grep 'test result'   # same count as before the move, all PASS
cargo check -p mat-device --no-default-features
cargo doc -p mat-device --no-deps 2>&1 | /usr/bin/grep -i warn   # expect: nothing
```

- [ ] **Step 3: Commit**

```bash
cargo fmt -p mat-device && cargo clippy -p mat-device --all-targets --all-features -- -D warnings
git add -A crates/mat-device/src/net
git commit -m "refactor(mat-device): split net/runtime.rs into runtime/{mod,classify,window,mdns_up,serve,subscribe,tests,socket_tests}.rs (pure move)

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 10: Tier 6 — socket-test harness + one `fast_cfg`

**Files:**
- Modify: `crates/mat-device/src/net/runtime/socket_tests.rs` (from Task 9)
- Modify: `crates/mat-device/src/net/mod.rs` (add `pub fn fast_cfg`)
- Modify: `crates/mat-device/src/net/pase.rs:63-73` (`retry_cfg`), `crates/mat-device/src/net/case.rs:47-57` (`retry_cfg`)
- Modify: `crates/mat-device/tests/support/mod.rs:61-69`, `crates/mat-device/tests/case_establish.rs:40-48`, `crates/mat-device/tests/pase_establish.rs:32-40`

**Interfaces:**
- Produces: `pub fn mat_device::net::fast_cfg() -> MrpConfig` = `{initial 50ms, active 50ms, max_retries 4, backoff 1.2, jitter 0.0}` (the `tests/support/mod.rs` values, per the lane instruction).
- Produces in `socket_tests.rs`:
  ```rust
  const LOCAL_SID: u16 = 0xAAAA; const PEER_SID: u16 = 0xBBBB; const CTRL_NODE: u64 = 1; const DEV_NODE: u64 = 2;
  const I2R: [u8; 16] = [0x11; 16]; const R2I: [u8; 16] = [0x22; 16];
  struct DevicePair { ctrl: Arc<Transport>, ctrl_addr: SocketAddr, dev_transport: Arc<Transport>, dev_addr: SocketAddr, session: SecureSession }
  async fn device_pair() -> DevicePair;
  fn controller_session(pair: &DevicePair) -> SecureSession;     // mirror-image controller role (test 3)
  struct FatHandler { cluster: u32 }                              // one copy
  fn encode_full_wildcard_read_request() -> Vec<u8>;             // one copy
  fn seal_from_controller(counter: &mut u32, opcode: u8, protocol_id: u16, exchange_id: u16, needs_ack: bool, acked: Option<u32>, payload: &[u8]) -> Vec<u8>;
  fn root_node_with_fat_clusters(n: u32) -> Node;                // Node::with_root_endpoint(0xFFF1,0x8000) + n FatHandlers at 0x9999_0000+i
  fn empty_comm_server() -> CommissioningServer;                 // generate_dev_attestation(0xFFF1,0x8000) + FabricStore::new()
  ```

- [ ] **Step 1: `net::fast_cfg`**

In `net/mod.rs`:
```rust
use mat_controller::exchange::MrpConfig;

/// The loopback MRP regime every in-crate driver and test uses: 50 ms
/// intervals, no jitter, four retries with mild backoff — fast enough for
/// a unit test, tolerant enough for a real one-shot handshake driver
/// (`net::pase` / `net::case`). One definition so the six former copies
/// cannot drift apart again.
pub fn fast_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: std::time::Duration::from_millis(50),
        active_interval: std::time::Duration::from_millis(50),
        max_retries: 4,
        backoff: 1.2,
        jitter: 0.0,
    }
}
```
Delete `retry_cfg` in `net/pase.rs` and `net/case.rs` and call `crate::net::fast_cfg()` at their call sites (**this raises the responder drivers' retries 2→4 and backoff 1.0→1.2 — record in DONE as the one intentional value change**). `tests/support/mod.rs`: replace the body with `pub use mat_device::net::fast_cfg;` (delete the fn; keep the name exported). `tests/case_establish.rs` and `tests/pase_establish.rs`: delete their `fast_cfg` (and the comment explaining why it was hand-rolled) and `use mat_device::net::fast_cfg;`. `socket_tests.rs` (the ex-`:3668` literal): `let cfg = crate::net::fast_cfg();`.

Run: `cargo test -p mat-device --all-features --test case_establish --test pase_establish` — expect PASS (they only assert outcomes, not timing; if a timeout assertion trips, report it rather than tuning the value).

- [ ] **Step 2: Hoist the harness in `socket_tests.rs`**

Module-level (top of `socket_tests.rs`, after `use super::*;`): the six consts (test 3 spells them `DEV_SID`/`CTRL_SID` — rename its uses to `LOCAL_SID`/`PEER_SID`, same values), one `FatHandler` (its doc: "600-byte `bytes` attribute so two of them exceed `REPORT_CHUNK_BUDGET`; cluster ids far outside any real range"), one `encode_full_wildcard_read_request` (keep its doc), and:
```rust
struct DevicePair {
    ctrl: Arc<Transport>,
    ctrl_addr: SocketAddr,
    dev_transport: Arc<Transport>,
    dev_addr: SocketAddr,
    /// Device-role session already pointed at `ctrl_addr`.
    session: SecureSession,
}

/// Two loopback UDP sockets plus the device-role `SecureSession` every
/// socket test starts from (`SessionKeys` are the fixed `I2R`/`R2I`).
async fn device_pair() -> DevicePair {
    let bind = || async {
        Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap()).await.unwrap(),
        )))
    };
    let ctrl = bind().await;
    let ctrl_addr = ctrl.local_addr().unwrap();
    let dev_transport = bind().await;
    let dev_addr = dev_transport.local_addr().unwrap();
    let session = SecureSession::new_device_role(
        Arc::clone(&dev_transport),
        ctrl_addr,
        LOCAL_SID,
        PEER_SID,
        SessionKeys { i2r: I2R, r2i: R2I, attestation_challenge: [0; 16] },
        DEV_NODE,
        CTRL_NODE,
    );
    DevicePair { ctrl, ctrl_addr, dev_transport, dev_addr, session }
}

/// Controller role: mirror image of the device's ids (see
/// `SecureSession::new_device_role`'s doc for the key swap).
fn controller_session(pair: &DevicePair) -> SecureSession {
    SecureSession::new(
        Arc::clone(&pair.ctrl),
        pair.dev_addr,
        PEER_SID,
        LOCAL_SID,
        SessionKeys { i2r: I2R, r2i: R2I, attestation_challenge: [0; 16] },
        CTRL_NODE,
        DEV_NODE,
    )
}

fn root_node_with_fat_clusters(n: u32) -> Node {
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    for i in 0..n {
        node.add_cluster(0, Box::new(FatHandler { cluster: 0x9999_0000 + i }));
    }
    node
}

fn empty_comm_server() -> CommissioningServer {
    let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
    CommissioningServer::new(dev, FabricStore::new())
}

/// One controller-sealed datagram (`session_id = LOCAL_SID`, initiator
/// side), bumping `counter` — what the two hand-rolled `send` closures did.
fn seal_from_controller(
    counter: &mut u32,
    opcode: u8,
    protocol_id: u16,
    exchange_id: u16,
    needs_ack: bool,
    acked: Option<u32>,
    payload: &[u8],
) -> Vec<u8> {
    let header = MessageHeader {
        session_id: LOCAL_SID,
        security_flags: 0,
        message_counter: *counter,
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
    *counter += 1;
    seal_message(&I2R, &header, &proto, payload, CTRL_NODE).unwrap()
}
```
Then in each of the 5 tests: delete the per-test consts / `FatHandler` / `encode_full_wildcard_read_request` / bind+session blocks / `send` closures, and use the helpers. Tests 1, 2, 4, 5 currently hold the controller as a bare `UdpTransport`; `Transport` exposes the same `send_to`/`recv_from`/`local_addr` (`mat-controller/src/transport.rs:130-155`), so `pair.ctrl.send_to(..)` / `pair.ctrl.recv_from(..)` are drop-in. Where a test moves the controller into a `tokio::spawn`, move `pair.ctrl` (an `Arc`) — clone it first if the test also needs it afterwards. Test-specific constants (`REQ_EXCHANGE`, `NEW_EXCHANGE`, `EX_READ`, `EX_OTHER`, `EX`) stay local. Test 3 keeps `subscribe_wildcard`'s cluster-1-attr FatHandler count (3) via `root_node_with_fat_clusters(3)`; test 2 uses `(2)` if it registers two — read each test's `add_cluster` loop and pass the same count/ids (if a test uses ids other than `0x9999_0000 + i`, keep its own `add_cluster` calls instead of the helper).

- [ ] **Step 3: Run, commit**

```bash
cargo test -p mat-device --all-features socket_tests    # 5 tests PASS
cargo fmt -p mat-device && cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
git add crates/mat-device && git commit -m "refactor(mat-device): one net::fast_cfg (6 copies; responder drivers now 4 retries/1.2 backoff) + shared socket-test harness

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 11: Tier 6 — `tests/` dedupe: ACL constants `pub`, one ACL entry encoder, `spawn_device`, `test_device_config`

**Files:**
- Modify: `crates/mat-device/src/core/access_control.rs:34-50` (`pub(crate)` → `pub` on `PRIVILEGE_*`, `AUTH_MODE_*`)
- Modify: `crates/mat-device/tests/support/mod.rs` (add `put_acl_entry`, `acl_entries_tlv`, `RunningDevice`, `spawn_device`)
- Modify: `tests/acl_enforce.rs:50-88`, `tests/acl_cat_subject.rs:57-58,115-131`, `tests/acl_write_validation.rs:30-62`, `tests/subscribe_denied.rs:34-35,40-54`, `tests/group_receive.rs:42-72`
- Modify (spawn preambles): `tests/self_commission_live.rs:60-74,105-113,229-239,334-340`, `tests/add_noc_invalid_admin_subject.rs:31-40`, `tests/acl_enforce.rs:92-103`, `tests/onoff_invoke.rs:37-50`, `tests/acl_cat_subject.rs:161-170`, `tests/acl_write_validation.rs:67-76`, `tests/subscribe_loop.rs:51-64`, `tests/subscribe_denied.rs:85-94,194-203`, `tests/group_provision.rs:86-99`, `tests/events_subscribe.rs:78-95`, `tests/group_receive.rs:235-245,305-312`
- Modify: `crates/mat-device/src/device.rs` tests (`:668-680`, `:805-815`, `:868-890`, `:940-951`) + `src/net/runtime/tests.rs` `test_config`

**Interfaces:**
- Produces in `tests/support/mod.rs`:
  ```rust
  pub fn put_acl_entry(w: &mut Writer, privilege: u8, auth_mode: u8, subjects: &[u64], fabric_index: u8);
  pub fn acl_entries_tlv(entries: &[(u8, u8, &[u64])], fabric_index: u8) -> Vec<u8>;
  pub struct RunningDevice { pub addr: SocketAddr, pub group_addr: Option<SocketAddr>, pub paa_der: Vec<u8>, pub stimulus: StimulusHandle, pub task: tokio::task::JoinHandle<()> }
  pub fn spawn_device(config: DeviceConfig) -> RunningDevice;   // must run inside a tokio runtime (Device::new requirement)
  ```
- Produces in `src/device.rs`: `#[cfg(test)] pub(crate) fn test_device_config(store_dir: PathBuf, devices: Vec<VirtualDeviceConfig>) -> DeviceConfig` (`passcode 20202021, discriminator 0xF00, vendor 0xFFF1, product 0x8000, port 0, iface "lo", attestation default, group_port 0`).

- [ ] **Step 1: Constants**

`access_control.rs:34-50`: change the seven `pub(crate) const` to `pub const` (keep docs). Delete the local `const PRIVILEGE_*` / `const AUTH_MODE_*` lines in the five tests and add `use mat_device::core::access_control::{...}` naming exactly the ones each file uses (clippy `unused_imports` is on). Leave each test's `FABRIC_INDEX`/`ADMIN_NODE_ID`/`GROUP_ID` alone.

- [ ] **Step 2: One ACL entry encoder**

In `tests/support/mod.rs`:
```rust
/// One `AccessControlEntryStruct` (spec §11.1.7.1) into `w`: `{1:
/// privilege, 2: authMode, 3: subjects(array), 4: targets(null), 254:
/// fabricIndex}` — the same wire shape as
/// `mat_device::core::access_control::write_acl_entry` /
/// `mat_native::ops::encode_acl_entries_tlv` (both crate-private, hence
/// this test-side copy).
pub fn put_acl_entry(w: &mut Writer, privilege: u8, auth_mode: u8, subjects: &[u64], fabric_index: u8) {
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(privilege));
    w.put_uint(Tag::Context(2), u64::from(auth_mode));
    w.start_array(Tag::Context(3));
    for s in subjects {
        w.put_uint(Tag::Anonymous, *s);
    }
    w.end_container();
    w.put_null(Tag::Context(4));
    w.put_uint(Tag::Context(254), u64::from(fabric_index));
    w.end_container();
}

/// Full-replace `ACL` Data TLV: an anonymous array of [`put_acl_entry`]s.
pub fn acl_entries_tlv(entries: &[(u8, u8, &[u64])], fabric_index: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (privilege, auth_mode, subjects) in entries {
        put_acl_entry(&mut w, *privilege, *auth_mode, subjects, fabric_index);
    }
    w.end_container();
    w.finish()
}
```
(`use mat_controller::tlv::{Tag, Writer};` — check it isn't already imported.) Replacements, byte-identical (same writer calls in the same order):
- `acl_enforce.rs` `encode_single_acl_entry_tlv(p, a, s, f)` → delete; calls become `acl_entries_tlv(&[(p, a, &[s])], f)`.
- `acl_cat_subject.rs` `admin_entry_for(subject)` → delete; `acl_entries_tlv(&[(PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, &[subject])], FABRIC_INDEX)`.
- `acl_write_validation.rs` `put_entry`/`entries_tlv` → delete; `entries_tlv(x)` calls become `acl_entries_tlv(x, FABRIC_INDEX)`.
- `group_receive.rs` `put_entry` → delete; inside its `acl_tlv(with_group)` call `support::put_acl_entry(&mut w, .., 1)` (it hard-codes fabric index 1).
- `subscribe_denied.rs` `operate_entry_tlv()` → delete; `acl_entries_tlv(&[(PRIVILEGE_OPERATE, AUTH_MODE_CASE, &[ADMIN_NODE_ID])], FABRIC_INDEX)`.

`tests/support/mod.rs` is compiled per test binary, so helpers unused by some binary trip `dead_code`; the file already uses `#[allow(dead_code)]` on such items (`device_config`) — follow that convention for the new helpers.

- [ ] **Step 3: `spawn_device`**

In `tests/support/mod.rs`:
```rust
/// A `Device` built from `config` and already `run`ning on its own task —
/// the preamble every integration test used to spell out. `addr` is the
/// `[::]` bind rewritten to `[::1]` (the wildcard is not a destination),
/// `paa_der` is what `Device::new` wrote to `<store>/paa/paa.der` (the
/// trust anchor `commission_directly` needs), `group_addr` is the
/// groupcast socket if it bound. `task` is the running device; `abort()`
/// it to stop.
pub struct RunningDevice {
    pub addr: SocketAddr,
    pub group_addr: Option<SocketAddr>,
    pub paa_der: Vec<u8>,
    pub stimulus: StimulusHandle,
    pub task: tokio::task::JoinHandle<()>,
}

pub fn spawn_device(config: DeviceConfig) -> RunningDevice {
    let store_dir = config.store_dir.clone();
    let device = Device::new(config).expect("device new");
    let loopback = |a: SocketAddr| SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), a.port());
    let addr = loopback(device.local_addr());
    let group_addr = device.group_local_addr().map(loopback);
    let paa_der = std::fs::read(store_dir.join("paa").join("paa.der"))
        .expect("device should have written its PAA DER at Device::new");
    let stimulus = device.stimulus_handle();
    let task = tokio::spawn(async move {
        let _ = device.run().await;
    });
    RunningDevice { addr, group_addr, paa_der, stimulus, task }
}
```
(`use mat_device::net::stimulus::StimulusHandle;`.) Replace each preamble with `let dev = spawn_device(device_config(store_dir.path().to_path_buf()));` and rename uses: `addr` → `dev.addr`, `paa_der` → `dev.paa_der` (pass `&dev.paa_der`), `device_task` → `dev.task`, `handle` → `dev.stimulus` (events_subscribe), `group_addr` → `dev.group_addr.expect("group socket bound")` (group_receive). Where a test defines its own `loopback` helper (group_receive) keep it if still used elsewhere in the file, else delete. `self_commission_live.rs:334-340` uses a custom cfg and doesn't read paa first — `spawn_device(cfg)` still works (the file exists by then). The two restart sites (`self_commission_live.rs:105-113`, `group_receive.rs:305-312`) also become `spawn_device(..)` (they simply ignore `paa_der`).

Check: `/usr/bin/grep -rn 'Device::new' tests` afterwards should list only `support/mod.rs` (plus comments mentioning it in strings/docs).

- [ ] **Step 4: `test_device_config`**

In `src/device.rs` (inside `#[cfg(test)] mod tests` or as a `#[cfg(test)] pub(crate) fn` at module level so `runtime/tests.rs` can reach it — the latter):
```rust
/// The loopback `DeviceConfig` unit tests build: ephemeral unicast and
/// group ports (`0`) so several `Device`s in one test never collide.
#[cfg(test)]
pub(crate) fn test_device_config(store_dir: std::path::PathBuf, devices: Vec<VirtualDeviceConfig>) -> DeviceConfig {
    DeviceConfig {
        passcode: 20202021,
        discriminator: 0xF00,
        vendor_id: 0xFFF1,
        product_id: 0x8000,
        port: 0,
        store_dir,
        iface: "lo".into(),
        attestation: AttestationMode::default(),
        group_port: 0,
        devices,
    }
}
```
Replace the four literals in `device.rs` tests (`cfg` closures become `|devices| test_device_config(dir.path().to_path_buf(), devices)`, etc.). For `runtime/tests.rs::test_config` (discriminator `3840` = `0xF00`, but `port: 5540`, `iface: ""`, `store_dir: PathBuf::new()`): grep its callers — `/usr/bin/grep -n 'test_config()' src/net/runtime/tests.rs` — and check whether any assertion reads `.port` or `.iface`. If none does, replace with `crate::device::test_device_config(std::path::PathBuf::new(), vec![])`; if one does, keep `test_config` and say so in DONE.

- [ ] **Step 5: Run everything, commit**

```bash
cargo fmt -p mat-device && cargo clippy -p mat-device --all-targets --all-features -- -D warnings
cargo test -p mat-device --all-features
cargo test -p matv
git add crates/mat-device && git commit -m "refactor(mat-device tests): pub ACL consts, one ACL entry encoder, support::spawn_device, test_device_config

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VVrZZCMt6WUDLwW5Tro4qQ"
```

---

### Task 12 (main session, not a subagent): final verification + DONE file

- [ ] `task check` (fmt:check + clippy + doc:check + test) — green.
- [ ] `task e2e:device:m1`, `task e2e:device:m3`, `task e2e:device:m4` — all pass (they build release and run `matv` + `mat`/`matd`; needs the multicast-capable interface autodetect; if the environment cannot run them, record the exact failure).
- [ ] Line-count delta: `git diff --stat main...refactor/device -- crates/mat-device crates/matv`.
- [ ] Write `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/device.DONE.md`: done items, skipped items + reasons (`PersistedStore<T>` generalisation, `bind_advertiser_socket`, `derive_session_keys`/SecureChannel constants follow-up for after ctrl-proto merges, `random_subscription_id` left alone, `pending_urgent` code untouched), the one value change (`fast_cfg` 4/1.2 in responder drivers), `task check` result, e2e results, "実機 E2E 不要（仮想デバイスのみ）".

---

## Self-review

- **Spec coverage:** device.md items 1 (Task 1), 2 (Task 2), 3 (Tasks 3–7), 4 (Tasks 8–9), 5 (Tasks 10–11), 6 (Task 4 Step 4), 7 (skipped, recorded in Task 12). Verification (Task 12).
- **Placeholders:** none; every code step carries the code. Line numbers are from `411ba68` and drift after each task — every step also names the function, and Step 1 of Task 1 re-greps.
- **Type consistency:** `tlv_value::{uint,str,bool,null}` (Task 2) used in Task 8's moved tests; `decode_struct_list`/`decode_single_struct`/`decode_struct_uint_field` (Task 5) names match their call sites; `MdnsCtx::{commissionable_advert, operational_advert, retire_operational}` (Task 4) are what Task 9 moves into `mdns_up.rs`; `net::fast_cfg` (Task 10) is what `support::fast_cfg` re-exports in Task 11's file; `spawn_device`/`RunningDevice` fields match the renames listed.
