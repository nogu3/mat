# mat

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

`mat` is a CLI for controlling Matter devices. It calls a Matter controller
(`chip-tool`) as a subprocess and returns its long text output as **pure
structured JSON**, normalized to `mat`'s own schema.

- stdout = one JSON object per command. No human decoration.
- diagnostics go to stderr as structured logs (`tracing`).
- it holds no state except the credential KVS (the process is one-shot).

For the design background, the three-layer separation, and what `mat` does and
does not do, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Status

**Phase 0 + Phase 1 + Phase 2 are implemented:**
- Phase 0: scaffold + chip-tool wrapper + commission + credential KVS + discover.
- Phase 1: read / write / invoke + describe + on / off.
- Phase 2: open-window (multi-admin share).

Group commands come in a later phase (see the roadmap in ARCHITECTURE.md).

## Requirements

- Rust (stable) and [Task](https://taskfile.dev) to build.
- A `chip-tool` binary on your `PATH` (or set `MAT_CHIP_TOOL_BIN` to its full
  path). Building `chip-tool` is heavy, so a Docker image with it baked in is
  provided (see [Backend](#backend-chip-tool)).
- Matter uses mDNS / IPv6 multicast, so on a real network the host must be able
  to send and receive these.

## Install

```bash
task build      # release build -> target/release/mat
task install    # install into ~/.cargo/bin
```

## Commands

### Discover and commissioning (Phase 0)

```bash
# Discover commissionable / commissioned nodes
mat discover

# Join a fabric (first commission OR multi-admin join, both supported)
# All values here are dummy (RFC 5737 192.0.2.0/24)
mat commission 192.0.2.10 "MT:Y.K9042C00KA0648G00" --node-id 5
```

`discover` output:

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "devices": [
    { "state": "commissionable", "hostname": "B827EBA8C9F0", "addresses": ["192.0.2.10"], "port": 5540, "discriminator": 3840, "vendor_id": 65521, "product_id": 32769 },
    { "state": "commissioned", "node_id": 5, "address": "192.0.2.10", "commissioned_at": "2026-06-06T12:00:00+09:00" }
  ]
}
```

`commission` output:

```json
{ "timestamp": "2026-06-06T12:34:56+09:00", "node_id": 5, "status": "success" }
```

### State operations (Phase 1)

`<node_id>` must be **already commissioned** (if not, exit `11`; if the store
itself is missing, exit `10`). Cluster / attribute / command names are passed in
**chip-tool form** (numeric resolution and human names are the upper layer's
job).

```bash
# Read an attribute: read <node_id> <endpoint> <cluster> <attribute>
mat read 5 1 onoff on-off

# Set a writable attribute: write <node_id> <endpoint> <cluster> <attribute> <value>
mat write 5 1 levelcontrol on-level 128

# Run a command: invoke <node_id> <endpoint> <cluster> <command> [args...]
mat invoke 5 1 levelcontrol move-to-level 128 0 0 0

# Introspect a node: describe <node_id>
mat describe 5

# High-frequency shortcuts (--endpoint defaults to 1)
mat on 5
mat off 5 --endpoint 2
```

**Important asymmetry: read is an attribute, control is an invoke.** Turning a
light ON/OFF is not a `write` of the OnOff attribute; it is an `invoke` of the
On/Off command. `mat on` / `mat off` are shortcuts for this and **map to the
`on` / `off` command of the OnOff cluster as an `invoke`** (not a write).

Outputs:

```json
// read — value is chip-tool's `Data = ...` normalized to bool/number/string/null
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "onoff", "attribute": "on-off", "value": true }

// write
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "levelcontrol", "attribute": "on-level", "value": "128", "status": "success" }

// invoke (mat on / off have the same shape)
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "onoff", "command": "on", "status": "success" }

// describe — lists child endpoints from endpoint 0's parts-list, and each
// endpoint's server-list as numeric cluster IDs
{ "timestamp": "...", "node_id": 5, "endpoints": [ { "endpoint": 0, "clusters": [29, 31] }, { "endpoint": 1, "clusters": [6, 8] } ] }
```

> `describe` calls chip-tool several times (parts-list plus each endpoint's
> server-list), so it is slow, but it finishes in one shot.

### Multi-admin share (Phase 2)

To share a `mat`-owned device with another controller (Alexa / Apple / Google),
open a commissioning window and return a one-time issued code. This wraps
`chip-tool pairing open-commissioning-window` (ECM = option 1).

```bash
# open-window <node_id> [--timeout S] [--iteration N] [--discriminator D]
mat open-window 5
mat open-window 5 --timeout 300
```

Output:

```json
{ "timestamp": "...", "node_id": 5, "manual_code": "36217551492", "qr_payload": "MT:-24J0AFN00KA0648G00", "expires_at": "2026-06-06T12:37:56+09:00" }
```

- Returns **both** `manual_code` (11-digit) and `qr_payload` (the `MT:...`
  string).
- **Rendering the QR image is not `mat`'s job.** stdout emits the `qr_payload`
  string only; drawing is the upper layer's job.
- `--timeout` defaults to 180 seconds. `expires_at` is the time `mat` built the
  response plus `timeout`.
- If `--discriminator` is omitted, it is derived from the node_id
  deterministically (kept within 12 bits).
- **"Share many devices in one QR" is not possible in Matter** (one commission
  per device). Fronting many devices is a bridge, a separate project, not `mat`.
  `open-window` shares native devices one at a time.
- Watch the fabric count limit. A cheap node may support only ~5 fabrics, so
  several admins plus `mat` can use up the slots. When a hub acts as a bridge,
  `mat` does multi-admin with the one hub, and its sensors appear as bridged
  endpoints.

## Credential store

Resolution order: `--store <path>` > `$MAT_STORE` > `$XDG_CONFIG_HOME/mat` >
`~/.config/mat`. It holds the Root CA, the controller's keys/cert, the
commissioned-node ledger (`nodes.json`), and `chip-tool`'s persistent storage.
**It is never committed** (excluded by `.gitignore`).

## Errors and exit codes

Errors go to stderr as `{"error":{"kind":"...","detail":"..."}}`.

| code | meaning |
|---|---|
| 0 | success |
| 2 | CLI argument error (clap default) |
| 10 | credential store missing / parse failure |
| 11 | node_id not commissioned |
| 12 | `chip-tool` not found / not runnable |
| 3 | timeout |
| 4 | device rejected |
| 5 | unreachable / network |
| 1 | other |

`chip-tool` has coarse exit codes (mostly `1` on failure). `mat` parses
stdout/stderr to classify into `3` / `4` / `5`. If it cannot classify, exit `1`.

`kind` values (stable; callers may branch on these strings):

- `store_missing` / `store_parse` — credential store missing / corrupt (exit 10)
- `node_not_commissioned` — node_id not in the store (exit 11)
- `child_not_found` — `chip-tool` binary not found / not runnable (exit 12)
- `timeout` (exit 3) / `device_rejected` (exit 4) / `unreachable` (exit 5) —
  classified from chip-tool output
- `child_failed` — `chip-tool` exited with failure (unclassified, exit 1)
- `commission_failed` — commissioning failed (unclassified, exit 1)
- `parse_error` — could not parse `chip-tool` output (exit 1)
- `other` — anything else (exit 1)

## Backend (chip-tool)

For local runs, put `chip-tool` on your `PATH`. Override the full path with
`MAT_CHIP_TOOL_BIN`. Building `chip-tool` is heavy, so a Docker image with it
baked in is provided for x86_64 Linux hosts (see [Dockerfile](./Dockerfile)).

> Matter uses mDNS / IPv6 multicast, so running in Docker **requires host
> networking** (`docker run --network host`). A bridge network cannot receive
> the responses.

## Development

Tasks are defined with [Task](https://taskfile.dev) (`task` lists them).

```bash
task build            # release build -> target/release/mat
task install          # install into ~/.cargo/bin
task run -- discover  # run (needs chip-tool on PATH)
task test             # tests (incl. fake-chip-tool integration tests; no real chip-tool)
task clippy           # lint (-D warnings)
task fmt              # format
task check            # CI equivalent (fmt:check + clippy + test)

task docker:build     # image for x86_64 Linux (chip-tool baked in)
task docker:run -- discover
task docker:test      # no local toolchain needed
```

CI does not need a real `chip-tool`. It uses `tests/fixtures/fake-chip-tool.sh`
(a stub that prints fixed text) via `MAT_CHIP_TOOL_BIN` to run integration tests.

## Manual E2E (with real devices; not in CI)

In practice the main path is **multi-admin join**: adding a device that is
already commissioned by another admin (such as Home Assistant) to `mat` as well.
The printed code does not work (the device left commissioning mode), so the
existing admin opens a commissioning window to issue a one-time code.

1. **Share from the other admin:** on the other controller, run "Share" for the
   target device and note the issued setup code (`MT:...` or 11-digit).
2. **Join with `mat`:**
   ```bash
   mat commission <device-ip-or-host> "<issued setup code>" --node-id 5
   ```
   It returns `{ "node_id": 5, "status": "success" }` and records the ledger in
   `~/.config/mat/nodes.json`.
3. **Confirm:** `mat discover` now shows node 5 with `"state": "commissioned"`.

> For a factory-reset device, pass the printed setup code directly to
> `commission` (first commission).

### Phase 1 operations E2E

Against a commissioned node (node 5 above), confirm read / describe / on / off
on a real device.

```bash
# Introspect what you can call (endpoints and numeric cluster IDs)
mat describe 5

# Read the OnOff attribute (for a light, its current on/off state)
mat read 5 1 onoff on-off

# Turn on -> off (invoke of the OnOff command, not an attribute write)
mat on 5
mat off 5

# Read-after-write check (confirm the value took effect)
mat on 5 && mat read 5 1 onoff on-off   # -> "value": true
```

### Phase 2 share E2E (mat -> another admin)

Share `mat`-owned node 5 with another controller.

```bash
# Open a commissioning window (get the issued code)
mat open-window 5 --timeout 300
# -> { "node_id": 5, "manual_code": "...", "qr_payload": "MT:...", "expires_at": "..." }
```

Enter the returned `manual_code` (11-digit) or `qr_payload` (render the QR with
the receiving tool) into the other controller's "Add device" flow (Alexa / Apple
Home / Google Home). Finish before `expires_at`. After sharing, `mat` keeps its
fabric membership (multi-admin).

> Each one-shot run pays mDNS resolution plus a CASE handshake, so a single call
> is slow (hundreds of ms to seconds). Speed-sensitive use cases belong to a
> resident layer that keeps warm sessions (see the three-layer separation in
> ARCHITECTURE.md).

## Contributing

Issues and pull requests are welcome. Before sending a PR, run `task check`
(format check + clippy with `-D warnings` + tests); it needs no real `chip-tool`.
Please keep stdout pure JSON and follow the design rules in
[ARCHITECTURE.md](./ARCHITECTURE.md).

## License

[MIT](./LICENSE).
