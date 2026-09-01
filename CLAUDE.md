# CLAUDE.md

Working rules for AI agents (Claude Code, etc.) editing this repo. Keep these in
mind on every change.

`mat` is a CLI for controlling Matter devices. It drives a native, from-scratch
Rust Matter controller (crate `mat-controller`, via the shared `mat-native`
engine) in-process and emits `mat`'s JSON schema. For the full
design, scope, the `mat` / `matd` split, and roadmap, read
[ARCHITECTURE.md](./ARCHITECTURE.md). For usage, read [README.md](./README.md).
This file is the short list of constraints you must not break.

## Design rules (never break)

1. **Protocol code lives only in the backend crates.** No TLV, no CASE, no
   multicast routing inside `mat` / `matd` command layers — that all
   belongs to the backend crates (`mat-controller` / `mat-native`).
2. **stdout is pure structured JSON only.** Emit the result in `mat`'s schema.
   No human decoration (color, progress, prompts).
3. **Diagnostics go to stderr as structured logs** (`tracing`).
4. **Hold no state except the credential KVS.** No session-cache DB, no daemon,
   no internal scheduler.

## Scope reminders (do not add these to `mat`)

- Resolving human names on the wire or in the backend (the backend / matd
  always receive numeric values). The only exception: if `<store>/aliases.toml`
  exists, the CLI layer resolves node / group / endpoint aliases — and color
  names via `[colors]` (RGB-defined, overriding the built-in color table in
  `mat-core`) — to numbers right after arg parsing — optional, local, and
  absent-file = no behavior change (built-in color names still work without
  the file). Cluster / attribute names stay chip-tool notation (no aliasing).
  `<store>/subscriptions.toml` follows the same absent-file discipline (matd-
  only: narrows the resident Subscribe to listed clusters; absent = full
  wildcard; a bad config makes `matd` refuse to start).
- Logical groups like "the lights in the living room" (out of scope; `mat` takes
  a numeric GroupId).
- Session cache, subscriptions, freshness (`mat` is one-shot; warm sessions are
  `matd`'s role, a separate binary). `mat listen` does not change this: it
  holds no subscription state itself — it is a thin client that connects to
  `matd`'s resident Subscribe and streams what `matd` sends (matd-only op, no
  direct fallback; `matd` absent is `matd_unavailable`, exit 13).
- Scenes, automation, voice/UI entry points (out of scope).
- Being a Matter device / a bridge (not mixed into `mat`/`matd`; the device
  role lives alongside them as the sibling crate `mat-device` plus a separate
  binary `matv`, see the 2026-08-15 spec).
- Rendering or displaying QR images (emit the `qr_payload` string only).

## Output conventions

### stdout
- On success, print the result as JSON to stdout, composed in `mat`'s schema.
- A `timestamp` field is **required** (**ISO 8601**, the time `mat` built the
  response — not Unix epoch). Example:
  ```json
  {
    "timestamp": "2026-06-03T12:34:56+09:00",
    "node_id": 1,
    "endpoint": 1,
    "cluster": "onoff",
    "attribute": "on-off",
    "value": true
  }
  ```

### stderr
- Diagnostics go to stderr as structured `tracing` logs.
- `mat`'s errors use `{"error": {"kind": "...", "detail": "..."}}`.
  `detail` should be specific enough for an AI to decide recovery (e.g.
  `"Node 12 is unreachable"`).
- `kind` values are stable; the full list is in docs/errors.md. (`child_not_found`
  / `child_failed` exist for wire compat only; never emitted as top-level
  errors.)

### exit codes
See the table in [docs/errors.md](./docs/errors.md). The native
backend maps its transport/IM outcomes to `3`/`4`/`5`/`6`, falling back to
`parse_error` + `1`. Exit `12` is a retired vacancy.

## Backend (native)

- The backend is native and pure Rust (crate `mat-controller`, via the shared
  `mat-native` engine): TLV, CASE, IM, groupcast, mDNS, and commissioning
  (on-network + BLE+Thread) run in-process. There is **no** external
  controller subprocess (`chip-tool` is fully retired).
- Route selection is per-op: matd auto-discovery (if a `matd` answers the probed
  socket) -> `mat`'s own native direct path. There is no third fallback tier.
- The interface is **auto-detected** every run (up/multicast/non-loopback/
  non-point-to-point with an IPv6 link-local address; exactly one candidate or a
  hard `other` error). `MAT_IFACE` / `MAT_MATD_IFACE` override it; `matd` refuses
  to start on an ambiguous autodetect. No state is held between runs (design
  rule 4).
- Generic `write`/`invoke`/`group invoke` encode **scalar** JSON→TLV only
  (bool/int/uint/enum/bitmap/string/octstr, bytes as `hex:`). `list`/`struct`/
  `float` fields and names the `mat-core::ids` table does not know are
  `parse_error` (numeric IDs are the escape hatch) — a documented limitation,
  not a fallback. `group provision`/`grant` list/struct writes use dedicated
  encoders.
- `mat` is the sole owner/writer of the persistent Matter KVS (chip-tool-
  compatible INI form: keysets, operational credentials, group tables, the
  group-send counter). Group-settings writes use flock exclusion + tmp+rename
  atomic replace (`mat-controller::group_settings`); a write failure (including
  a flock `WouldBlock`) is a hard error and never silently degrades. First-
  fabric bootstrap is `mat fabric init` (random-epoch IPK). A fabric first
  created by chip-tool is handled by verifying its fixed epoch against the KVS
  materials and adopting it (persisted to `mat/f/<idx>/ipk-epoch`).
- `matd`-only ops vs direct-only ops: `discover` / `commission` / `fabric init`
  / `open-window` / `diag` / `group grant` are never part of the `matd` socket
  protocol — they always run on `mat`'s own one-shot direct path. `listen` is
  the opposite case and the first of its kind: it is **matd-only**, with no
  direct-path fallback at all (subscriptions need a resident daemon). See
  docs/commands.md ("Routing through matd") for the exact op list.
- `mat` couples to the backend only through `mat`'s own JSON schema —
  subcommands and output schema are the contract.
- **Fragile parts (keep tests):** (1) the **chip-tool INI KVS compatibility** —
  base64 TLV records with upstream `GroupDataProviderImpl` link discipline
  (`mat-controller::kvs` / `group_settings`); an upstream format assumption
  breaking would corrupt the store. (2) the **one-shot mDNS (`dnssd`)** browse /
  targeted resolve — real Thread meshes need targeted resolve, not enumeration.
  Keep the unit + real-device tests that pin both.

## Credentials and the repo

- The repo is **public**. Never commit credentials, real IPs, real node_ids, or
  real certificates. Samples use dummy values only (e.g. RFC 5737
  `192.0.2.0/24`). The credential store is excluded by `.gitignore`.

## Roadmap discipline

Phases go **in order** (see ARCHITECTURE.md). Do not start the next phase until
the current one is fully done (all tests pass, acceptance criteria met).
Phases 0–5 are complete, real-device E2E included (Phase 4 = `matd`, the
resident binary; Phase 5 = the native backend, `mat-controller` — records in
ARCHITECTURE.md).

## Development commands

Tasks are defined with [Task](https://taskfile.dev) — run `task` to list them
(build / install / test / clippy / fmt / check / dist:arm64 / docker:*).

Run `task check` (CI equivalent: fmt:check + clippy + test) before any commit.
Matter uses mDNS / IPv6 multicast, so Docker runs require host networking
(`docker run --network host`).

**crates.io publish rule:** the version bump follows `task semver`
(cargo-semver-checks against the published baseline) — mechanical, not
judgment. If it reports a major-level break anywhere in the workspace, the
next publish bumps the workspace major (all 7 crates share one version); no
"probably nobody depends on it yet" downgrades.

## Compact instructions

When compacting, preserve: node ids and their observed device states, version
numbers and deploy status, open issue numbers, and verbatim error/log output.
