# CLAUDE.md

Working rules for AI agents (Claude Code, etc.) editing this repo. Keep these in
mind on every change.

`mat` is a CLI for controlling Matter devices. It calls a Matter controller
(`chip-tool`) as a subprocess and normalizes its text output into `mat`'s JSON
schema. For the full design, scope, the `mat` / `matd` split, and roadmap, read
[ARCHITECTURE.md](./ARCHITECTURE.md). For usage, read [README.md](./README.md).
This file is the short list of constraints you must not break.

## Design rules (never break)

1. **Protocol code lives only in the backend crates.** No TLV, no CASE, no
   multicast routing inside `mat` / `matd` command layers — that all
   belongs to the backend crates (`mat-controller` / `mat-native`, Phase 5,
   in progress). By default the production path still delegates everything
   to `chip-tool`; the native hotpath (see Backend section below) is opt-in
   until `chip-tool` is fully retired (M8).
2. **stdout is pure structured JSON only.** Parse `chip-tool` output and re-emit
   it in `mat`'s schema. No human decoration (color, progress, prompts). Never
   pass `chip-tool` output through unchanged.
3. **Diagnostics go to stderr as structured logs** (`tracing`). Do not swallow
   `chip-tool`'s stderr; keep it at least at debug level.
4. **Hold no state except the credential KVS.** No session-cache DB, no daemon,
   no internal scheduler.

## Scope reminders (do not add these to `mat`)

- Resolving human names on the wire or in the backend (chip-tool / matd always
  receive numeric values). The only exception: if `<store>/aliases.toml`
  exists, the CLI layer resolves node / group / endpoint aliases — and color
  names via `[colors]` (RGB-defined, overriding the built-in color table in
  `mat-core`) — to numbers right after arg parsing — optional, local, and
  absent-file = no behavior change (built-in color names still work without
  the file). Cluster / attribute names stay chip-tool notation (no aliasing).
- Logical groups like "the lights in the living room" (out of scope; `mat` takes
  a numeric GroupId).
- Session cache, subscriptions, freshness (`mat` is one-shot; warm sessions are
  `matd`'s role, a separate binary).
- Scenes, automation, voice/UI entry points (out of scope).
- Being a Matter device / a bridge (a separate project).
- Rendering or displaying QR images (emit the `qr_payload` string only).

## Output conventions

### stdout
- On success, print the result as JSON to stdout, re-composed in `mat`'s schema
  (do not forward `chip-tool` output raw).
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
- `chip-tool` errors go to stderr as structured logs.
- `mat`'s own errors use the same form: `{"error": {"kind": "...", "detail": "..."}}`.
  `detail` should be specific enough for an AI to decide recovery (e.g.
  `"Node 12 is unreachable"`).
- `kind` values are stable and documented in README ("Errors and exit codes").
  Examples: `store_missing` / `store_parse` / `node_not_commissioned` /
  `child_not_found` / `child_failed` / `commission_failed` / `timeout` /
  `unreachable` / `session_failed` / `device_rejected` / `parse_error` / `other`.

### exit codes
See the table in [README.md](./README.md#errors-and-exit-codes). In short:
`0` success, `2` CLI arg error, `10` store missing/parse, `11` not commissioned,
`12` chip-tool not found, `3` timeout, `4` device rejected, `5` unreachable,
`6` CASE session establishment failed, `1` other. `chip-tool` exit codes are
coarse (mostly `1`); `mat` parses stdout/stderr to classify into `3`/`4`/`5`/`6`,
falling back to `parse_error` + `1`.

## Backend (chip-tool)

- Route selection is per-op: matd auto-discovery (unchanged) -> native direct
  (`mat-native`, only when `MAT_IFACE`/`MAT_MATD_IFACE` is set and the op is
  in the native hotpath) -> direct `chip-tool`. As of Phase 5 M8a the native
  hotpath widened from the M4/M5/M7 fixed shapes to generic `read`/`write`/
  `invoke` (via the `mat-core::ids` name→ID table, scalar types only) plus
  `describe`/`diag thread`/`open-window`/`group provision`/`group grant`/
  `group invoke`; unresolvable names and list/struct/float fields still fall
  back to `chip-tool` (or `parse_error` for the latter). Nothing above this
  list changes when native is unset — see README for the exact op list and
  fallback rules.
- `chip-tool` is found on `PATH`; override the full path with `MAT_CHIP_TOOL_BIN`.
- The backend is replaceable: `mat` couples to it only through `mat`'s own JSON
  schema. Keep all backend specifics inside the child-runner adapter so a future
  swap is one adapter, with subcommands and output schema unchanged.
- The `Data = ...` parse path is the fragile part. Keep parser unit tests
  (normal cases + unparseable = `parse_error`) so an upstream version change is
  noticed.

## Credentials and the repo

- The repo is **public**. Never commit credentials, real IPs, real node_ids, or
  real certificates. Samples use dummy values only (e.g. RFC 5737
  `192.0.2.0/24`). The credential store is excluded by `.gitignore`.

## Roadmap discipline

Phases go **in order** (see ARCHITECTURE.md). Do not start the next phase until
the current one is fully done (all tests pass, acceptance criteria met). Phases
0–4 are implemented, real-device E2E included (Phase 4 = `matd`, the resident
binary with warm CASE sessions; a separate binary in this repo). **Phase 5**
(native backend: from-scratch Rust controller, crate
`mat-controller`) is decided and in progress — see
`docs/superpowers/specs/2026-07-10-phase5-backend-direction-design.md`.

## Development commands

Tasks are defined with [Task](https://taskfile.dev) (`task` lists them).

```bash
task build            # release build -> target/release/{mat,matd}
task install          # install both binaries into ~/.cargo/bin
task run -- discover  # run (needs chip-tool on PATH)
task test             # tests (incl. fake-chip-tool integration tests; no real chip-tool)
task clippy           # lint (-D warnings)
task fmt              # format
task check            # CI equivalent (fmt:check + clippy + test)

task docker:build     # image for x86_64 Linux (chip-tool baked in)
task docker:run -- discover
task docker:test      # no local toolchain needed
```

Run `task check` before any commit. Matter uses mDNS / IPv6 multicast, so Docker
runs require host networking (`docker run --network host`).
