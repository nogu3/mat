# mat

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

`mat` is a CLI for controlling Matter devices. It drives a **from-scratch native
Matter controller** (crate `mat-controller`, in this workspace) in-process and
returns **pure structured JSON**, normalized to `mat`'s own schema. (`chip-tool`
was the backend through Phase 5 M8c-2; as of **0.22.0** it is fully retired — see
[Backend](#backend).)

- stdout = one JSON object per command. No human decoration.
- diagnostics go to stderr as structured logs (`tracing`).
- it holds no state except the credential KVS (the process is one-shot).

For the design background, the `mat` / `matd` split, and what `mat` does and
does not do, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Status

Everything documented below is implemented on the native backend: discover /
commission (on-network and BLE+Thread), first-fabric bootstrap (`fabric init`),
state operations (read / write / invoke / describe / on / off), multi-admin
share (`open-window`), groupcast (`group provision` / `group invoke`), the
resident daemon `matd` (warm CASE sessions, `mat --matd`), diagnostics
(`diag thread` / `diag node`), and `matd`'s resident wildcard Subscribe with
`mat listen` streaming device-originated events (matd-only, no direct
fallback). It passes the fake-connection / binary integration tests, and
real-device E2E (Phase 5 gate 1) has confirmed the full op sweep runs
natively with no fallback; `mat listen`'s real-device E2E is pending a
separate deploy session. Group *delivery* is unacknowledged multicast by
design, so per-device actuation cannot be confirmed from the controller side
(see Groupcast below).

The development roadmap and the Phase 5 native-backend record live in
[ARCHITECTURE.md](./ARCHITECTURE.md).

## Requirements

- Rust (stable) and [Task](https://taskfile.dev) to build. No external Matter
  controller is needed — the backend is native and pure Rust.
- Matter uses mDNS / IPv6 multicast, so on a real network the host must be able
  to send and receive these. `mat` auto-detects the network interface (override
  with `MAT_IFACE`; see [Backend](#backend)).
- BLE commissioning (BLE+Thread) is an opt-in `ble` cargo feature (pulls in
  `libdbus`); the default build and local `task check` do not need it. Deploy
  builds enable it — see [Backend](#backend).

## Install

```bash
task build      # release build -> target/release/{mat,matd}
task install    # install both binaries into ~/.cargo/bin
```

## Commands

### Discover and commissioning

```bash
# Discover commissionable / commissioned nodes (ledger only, fast)
mat discover

# Also probe live reachability of commissioned nodes via mDNS
mat discover --probe

# Join a fabric (first commission OR multi-admin join, both supported)
# All values here are dummy (RFC 5737 192.0.2.0/24)
mat commission --target 192.0.2.10 --setup-code "MT:Y.K9042C00KA0648G00" --node 5
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

With `--probe`, each `commissioned` node is checked against a live mDNS resolve
(a native targeted `_matter._tcp` lookup per node, run concurrently) and
annotated:

- `reachable: true` — advertising now; `address` is the live-resolved value
  (may differ from the ledger).
- `reachable: false` — not advertising; `address` is the last-known ledger
  value with `stale: true`.
- `reachable: null` — the mDNS probe could not run (e.g. an interface I/O
  error); reachability is unknown. A diagnostic is logged to stderr.

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "devices": [
    { "state": "commissioned", "node_id": 5, "address": "192.0.2.99", "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": true },
    { "state": "commissioned", "node_id": 7, "address": "192.0.2.10", "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": false, "stale": true },
    { "state": "commissioned", "node_id": 9, "address": "192.0.2.20", "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": null }
  ]
}
```

Without `--probe` the output is unchanged (no `reachable` / `stale`); the
ledger is reported as-is and reflects no live reachability. Node-id matching
is best-effort (a cross-fabric node_id collision could false-positive); for a
deeper single-node check use `mat diag node --deep`.

`commission` output:

```json
{ "timestamp": "2026-06-06T12:34:56+09:00", "node_id": 5, "status": "success" }
```

#### First-fabric bootstrap (`fabric init`)

Before the very first commission you need a fabric: a Root CA, the controller's
operational identity, and a random-epoch IPK, all written into a fresh
credential store. `mat fabric init` creates them (direct path only, no network
touched — it just writes the KVS):

```bash
# fabric init [--fabric-id N] [--admin-node-id N]   (defaults: 1 / 112233)
mat fabric init
```

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "store": "/home/you/.config/mat",
  "fabric_id": 1,
  "fabric_index": 1,
  "compressed_fabric_id": "AAAAAAAAAAAAAAAA",
  "admin_node_id": 112233
}
```

- The generated IPK epoch is random (16 bytes from the OS CSPRNG), not
  `chip-tool`'s old fixed `temporary ipk 01` constant. The key material never
  appears on stdout — only the fabric identifiers.
- **It refuses if the store already holds a KVS** (no `--force`); re-initialize
  by deleting the store's `.ini` files by hand. Any other command run before
  `fabric init` returns `store_missing` (exit 10) with a hint to run it.
- If you are joining a fabric that was created by `chip-tool` (fixed-epoch),
  you do **not** run `fabric init` — the first native `commission` verifies the
  fixed epoch against the fabric's KVS materials and adopts it (see
  [Backend](#backend), "epoch").

#### Attestation / PAA trust store

Production Matter devices ship a DAC signed by a **production PAA** (Product
Attestation Authority). Without the matching PAA root, commissioning fails
attestation (`device_rejected`, "Failed Device Attestation"). Point `mat` at a
directory of PAA root certificates:

```bash
# Option 1: explicit env var
export MAT_PAA_TRUST_STORE=/path/to/paa-root-certs
# Option 2: drop the certs under the store, no env needed
#   <store>/paa-trust-store/   (e.g. ~/.config/mat/paa-trust-store/)
mat commission --target 192.0.2.10 --setup-code "MT:Y.K9042C00KA0648G00" --node 5
```

Resolution order: `MAT_PAA_TRUST_STORE` > `<store>/paa-trust-store/`. If neither
exists, `mat` trusts only the built-in development PAA (fine for test devices,
not for retail ones). Get the certificates from connectedhomeip's
`credentials/production/paa-root-certs/`. A CD (Certification Declaration) signer
trust store resolves the same way via `MAT_CD_SIGNER_STORE` >
`<store>/cd-signer-store/` (absent = CD verification is warn-only).

### State operations

`<node_id>` must be **already commissioned** (if not, exit `11`; if the store
itself is missing, exit `10`). Cluster / attribute / command names are passed in
**chip-tool form** (`mat` works in numeric / chip-tool terms; cluster /
attribute / command names are never aliased).

All device-addressing commands take named flags: `--node` (required),
`--endpoint` (defaults to 1), `--cluster`, `--attribute`, each with a short flag
(`-n` / `-e` / `-c` / `-a`) for terser typing. `--node` / `--endpoint` take the
numeric Matter identifiers; optionally, if `<store>/aliases.toml` exists, they
also accept a locally defined name that `mat` resolves to the number right after
arg parsing (see [Aliases](#aliases-aliasestoml-optional)). Without that file,
numbers are the only form, exactly as before.

```bash
# Read an attribute (--endpoint defaults to 1)
mat read --node 5 --cluster onoff --attribute on-off
mat read -n 5 -c onoff -a on-off                 # same, short aliases

# Set a writable attribute
mat write --node 5 --cluster levelcontrol --attribute on-level --value 128

# Run a command: --command plus trailing command args
mat invoke --node 5 --cluster levelcontrol --command move-to-level 128 0 0 0

# Introspect a node
mat describe --node 5

# High-frequency shortcuts (--endpoint defaults to 1)
mat on --node 5
mat off --node 5 --endpoint 2

# Color temperature (ColorControl MoveToColorTemperature): give Kelvin and mat
# converts to mireds (round(1,000,000 / K)), or pass mireds directly. The two
# flags are mutually exclusive and one is required. --transition is in tenths
# of a second (30 = 3 s, default 0). Values outside the device's supported
# range are clamped by the device itself (mat does not pre-read or validate).
mat color-temp --node 5 --kelvin 2700
mat color-temp --node 5 --kelvin 2700 --transition 30
mat color-temp --node 5 --mireds 370

# Brightness (LevelControl MoveToLevel): give a percentage (0-100) and mat
# converts to the raw 0-254 level (round(percent / 100 * 254); 255 is
# reserved). --transition is in tenths of a second (30 = 3 s, default 0).
# Values outside the device's supported range are clamped by the device
# itself (mat does not pre-read or validate).
mat level --node 5 --percent 50
mat level --node 5 --percent 100 --transition 30

# Hue / saturation (ColorControl MoveToHueAndSaturation): --hue in degrees
# (0-360) and --sat in percent (0-100), both required. mat converts each to
# Matter's 0-254 scale (round(v / full * 254); 255 is reserved so full scale
# tops out at 254). --transition is in tenths of a second (default 0). Values
# outside the device's supported range are clamped by the device itself.
mat color --node 5 --hue 330 --sat 80
mat color --node 5 --hue 330 --sat 80 --transition 30

# Named colors and RGB: --name looks up a built-in table (red / pink / orange /
# purple / cyan / green / blue / yellow / magenta / white; extend or override
# via [colors] in aliases.toml), --rgb takes #rrggbb / rrggbb / R,G,B. Both are
# converted RGB -> HSV -> hue/sat; the V (brightness) component is discarded,
# so these set the color only and never change brightness (use LevelControl
# for that). `--name white` naturally lands on sat=0 (desaturate); color-temp
# can also produce white but through a different pipeline — both are kept.
# The three spec systems (--name / --rgb / --hue+--sat) are mutually exclusive.
mat color --node 5 --name pink
mat color --node 5 --rgb "#ff00aa"
mat color --node 5 --rgb 255,0,170
```

**Important asymmetry: read is an attribute, control is an invoke.** Turning a
light ON/OFF is not a `write` of the OnOff attribute; it is an `invoke` of the
On/Off command. `mat on` / `mat off` are shortcuts for this and **map to the
`on` / `off` command of the OnOff cluster as an `invoke`** (not a write).

Outputs:

```json
// read — the attribute's TLV value normalized to bool/number/string/null
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "onoff", "attribute": "on-off", "value": true }

// write
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "levelcontrol", "attribute": "on-level", "value": "128", "status": "success" }

// invoke (mat on / off have the same shape)
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "onoff", "command": "on", "status": "success" }

// color-temp — echoes both the input kelvin and the converted mireds so the
// result can be cross-checked against a `color-temperature-mireds` read
// (when --mireds is given, kelvin is back-computed the same way for the echo)
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "colorcontrol", "command": "move-to-color-temperature", "kelvin": 2700, "mireds": 370, "transition": 0, "status": "success" }

// level — echoes both the input percent and the converted raw level so the
// result can be cross-checked against a `current-level` read
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "levelcontrol", "command": "move-to-level", "percent": 50, "level": 127, "transition": 0, "status": "success" }

// color — echoes the input degrees/percent plus the converted 0-254 raw
// values so the result can be cross-checked against `current-hue` /
// `current-saturation` reads
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "colorcontrol", "command": "move-to-hue-and-saturation", "hue": 330, "saturation": 80, "hue_raw": 233, "saturation_raw": 203, "transition": 0, "status": "success" }

// color with --name / --rgb — additionally echoes the input name and the
// normalized #rrggbb so the conversion can be audited
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "colorcontrol", "command": "move-to-hue-and-saturation", "hue": 350, "saturation": 25, "hue_raw": 247, "saturation_raw": 63, "transition": 0, "name": "pink", "rgb": "#ffc0cb", "status": "success" }

// describe — lists child endpoints from endpoint 0's parts-list, and each
// endpoint's server-list as numeric cluster IDs
{ "timestamp": "...", "node_id": 5, "endpoints": [ { "endpoint": 0, "clusters": [29, 31] }, { "endpoint": 1, "clusters": [6, 8] } ] }
```

> `describe` issues several reads (parts-list plus each endpoint's
> server-list) over one CASE session, so it does a bit of work, but it finishes
> in one shot.

### Diagnostics

`mat diag thread --node <node_id>` returns a one-shot snapshot of a node's **Thread
Network Diagnostics** (cluster 53, normally on endpoint 0) for analyzing mesh
health — "why is this device flaky?". It bundles the scalars `routing-role` /
`network-name` / `extended-pan-id` / `pan-id` / `partition-id` / `channel` with
the list attributes `neighbor-table` and `route-table`, which the generic `mat
read` can't represent (they are lists of structs, not a single value).

```bash
# diag thread --node <node_id> [--endpoint EP]   (EP defaults to 0)
mat diag thread --node 5
```

```json
// routing_role etc. are numeric enums (mat does not resolve names);
// neighbor_table / route_table are arrays of objects with chip-tool field names.
{
  "timestamp": "...", "node_id": 5, "endpoint": 0,
  "thread": {
    "routing_role": 5, "network_name": "ha-thread-6562",
    "extended_pan_id": 14789548233599576168, "pan_id": 25954,
    "partition_id": 597971536, "channel": 15,
    "neighbor_table": [
      { "Age": 21, "ExtAddress": 7110405590318074745, "Rloc16": 38912, "Lqi": 3, "AverageRssi": -65, "LastRssi": -67, "FrameErrorRate": 56, "RxOnWhenIdle": true, "IsChild": false }
    ],
    "route_table": [
      { "ExtAddress": 7110405590318074745, "Rloc16": 38912, "RouterId": 38, "NextHop": 45, "PathCost": 1, "LQIIn": 3, "LQIOut": 3, "LinkEstablished": true, "Allocated": true }
    ]
  }
}
```

> Field names inside `neighbor_table` / `route_table` follow chip-tool's
> field-name convention (note `Lqi` in neighbors but `LQIIn` / `LQIOut` in
> routes), and `routing_role` is the numeric enum (5 = Router) — `mat` does not
> resolve names.

> How to read it: a flaky node usually has **few `neighbor_table` entries** or a
> weak `AverageRssi` to its only neighbor (roughly: > -70 dBm healthy, < -85 dBm
> marginal). Only mains-powered, router-eligible devices relay (`RxOnWhenIdle:
> true` / not `IsChild`); adding battery sleepy end devices does not extend the
> mesh. Devices that share the same `extended_pan_id` are on the same Thread
> network (same border router); a `partition_id` that differs across nodes means
> the mesh has split.
>
> Thread devices drop in and out, so `diag` returns **partial results**: each
> attribute is read independently, failures are listed under `unavailable`
> (`[{ "attribute": ..., "kind": ... }]`), and an unread field is `null` —
> distinct from `[]`, which means a table was read and is genuinely empty (an
> isolated node). If *every* read fails (node fully unreachable) it exits with
> `unreachable` / `timeout` instead. Like `describe`, this reads several
> attributes but finishes in one shot. `mat diag` runs only on the direct path
> (not via `--matd`).

`mat diag node --node <node_id>` answers a different question: **why can't I
control this commissioned node?** It runs layered checks and classifies the
result into a single `verdict` with the evidence and a recommended action —
where `mat invoke` would only return a bare `timeout` / `session_failed`.

```bash
# diag node --node <node_id> [--endpoint EP] [--deep]   (EP defaults to 0)
mat diag node --node 5            # fast (native IM: operational + thread checks)
mat diag node --node 5 --deep     # also probe ping6 + native targeted mDNS
```

```json
{
  "timestamp": "...", "node_id": 5, "endpoint": 0,
  "verdict": "link_starved",
  "summary": "IP reachable but not advertising Matter on any fabric; weak Thread link — SRP registration likely incomplete.",
  "checks": {
    "ip":   { "ok": true, "loss_pct": 50, "rtt_ms": 168.0, "method": "ping6" },
    "mdns": { "advertised_self_fabric": false, "advertised_any_fabric": false },
    "operational": { "resolved": false, "kind": "timeout" },
    "thread": { "neighbor_count": 1, "best_lqi": 3, "routing_role": 2 }
  },
  "recommendation": "Improve the Thread link (move the device near a router) or wait; do NOT factory reset — the fabric is intact."
}
```

> `verdict` is one of `ok`, `ip_unreachable`, `link_starved`, `fabric_missing`,
> `not_advertised`, `unresolvable`, `session_failed`, `device_rejected`,
> `unknown`. Without `--deep` the fast path can't tell `link_starved` (weak
> Thread link, SRP not registered — **the fabric is intact**) apart from
> `fabric_missing` (the device dropped our fabric); `--deep` adds the ping6 and
> mDNS evidence that distinguishes them. Like `diag thread` it returns **partial
> results** (skipped/failed checks go under `unavailable`) and **always exits
> `0`** with a verdict, even when the node is fully unreachable — the value is in
> the classification, not an exit code. `--deep` shells out to `ping6` (override
> with `MAT_PING6_BIN`) and does a native targeted mDNS resolve.
>
> The `operational` and `thread` checks run natively over a single CASE session
> (one session serves both). `diag node` is direct path only — it is not part
> of the `matd` protocol.
>
> `mdns.advertised_self_fabric` is whether the node advertises on **our** fabric
> specifically (vs. `advertised_any_fabric`, which is any fabric). It needs our
> compressed-fabric-id, which `mat` computes directly from the fabric's KVS
> materials — so it is always available (the historical `cfid_unavailable` case,
> from the old chip-tool-log-parsing path, cannot occur on the native path).

`mat diag mesh` answers a third question: **what does the whole Thread mesh
look like?** It probes each fabric node's Thread Network Diagnostics (cluster
53) and NetworkInterfaces (cluster 0x33, for self-identification) in turn and
assembles the results into one node/edge topology graph — neighbor and route
table rows that name a participant `mat` never commissioned (an OTBR border
router, a device on another fabric) become graph nodes too, so the mesh is
visible even where the fabric does not reach.

```bash
# diag mesh [--nodes N|ALIAS ...]   (omit --nodes = every commissioned node)
mat diag mesh
mat diag mesh --nodes 5 16
```

```json
// dummy values; ExtAddress / rloc16 are hex, ids are the graph's stable keys.
{
  "timestamp": "...",
  "network": {
    "name": "ha-thread-6562", "channel": 15,
    "partition_ids": [597971536], "leader_router_id": 8
  },
  "nodes": [
    { "id": "ext:0011223344556677", "ext_address": "0011223344556677",
      "rloc16": "0x1400", "router_id": 5, "role": "router",
      "node_id": 16, "alias": "study_motion", "probed": true },
    { "id": "ext:8899AABBCCDDEEFF", "ext_address": "8899AABBCCDDEEFF",
      "rloc16": "0x0c01", "role": "child", "node_id": 5, "probed": true },
    { "id": "ext:AABBCCDDEEFF0011", "ext_address": "AABBCCDDEEFF0011",
      "rloc16": "0x2000", "router_id": 8, "role": "leader", "label": "otbr-br" }
  ],
  "edges": [
    { "a": "ext:0011223344556677", "b": "ext:8899AABBCCDDEEFF",
      "a_sees_b": { "lqi": 140, "avg_rssi": -60, "last_rssi": -58, "frame_error_rate": 2, "age": 12 },
      "b_sees_a": { "lqi": 130, "avg_rssi": -65, "last_rssi": -64, "frame_error_rate": 5, "age": 8 } },
    { "a": "ext:0011223344556677", "b": "ext:AABBCCDDEEFF0011",
      "a_sees_b": { "lqi": 200, "avg_rssi": -50, "last_rssi": -49, "frame_error_rate": 0, "age": 3 },
      "b_sees_a": null,
      "route": { "lqi_in": 3, "lqi_out": 3, "path_cost": 1 } }
  ]
}
```

> `--nodes` takes node_ids or `aliases.toml` node aliases, one or more;
> omitted, it means every commissioned node in the store. A node with 0
> targets (empty store) is not an error — it returns an empty graph
> (`"nodes":[]`, `"edges":[]`) with just a `timestamp`, without touching the
> backend.
>
> A node's stable `id` is `ext:<HEX16>` when its ExtAddress is known (either
> self-identified via cluster 0x33, for fabric nodes, or observed in a
> neighbor/route table row, for unknown participants), `rloc:<hex>` when only
> the RLOC16 could be derived, and `node:<node_id>` for a fabric node whose
> probe never got far enough to read either (e.g. cluster 0x33 unreadable).
> Unknown participants get a `label` from `aliases.toml`'s `[thread]` section
> (see [Aliases](#aliases-aliasestoml-optional) above) instead of an `alias`,
> which is reserved for commissioned nodes' own node alias.
>
> Like `diag node`, `diag mesh` is direct path only (native, not part of the
> `matd` protocol) and always fixes on endpoint 0 (cluster 53 / 0x33 are
> normally endpoint 0). Collection is **sequential**, one CASE session per
> node, so wall-clock time scales with node count — a handful of seconds per
> node, so an 8-node mesh takes on the order of tens of seconds.
>
> A single node's probe failure does not fail the whole command: it shows up
> as `"probed":false` plus a `probe_error` (`{"kind":...,"detail":...}`) on
> that node, and the command still exits `0` with the partial graph — same
> philosophy as `diag thread`'s per-attribute `unavailable`. Only when *every*
> targeted node's probe fails does `diag mesh` exit non-zero, mapped from the
> most common failure `kind` across nodes (e.g. all nodes `unreachable` exits
> `5`; a tie is broken by first-seen kind).

### Listen (device-originated events)

`mat listen` streams attribute-change events from `matd`'s resident wildcard
Subscribe (occupancy sensors, open/close, temperature/humidity, on-off, ...) —
`mat`/`matd`'s alternative to depending on Home Assistant for these. It is
the **first matd-only op**: there is no direct-native fallback, because a
subscription needs a resident daemon to stay alive between calls (see
[Routing through `matd`](#routing-through-matd)).

```bash
mat listen [--node <id|alias>] [--endpoint <n>] [--cluster <name>] [--attribute <name>]
           [--count <N>] [--timeout-ms <T>]
```

- Filters (`--node` / `--endpoint` / `--cluster` / `--attribute`) narrow which
  events are delivered; omitted filters match everything. `--node` accepts a
  node alias the same way other commands do; `--cluster` / `--attribute` are
  chip-tool notation (never aliased), same as `read`. If `<store>/subscriptions.toml`
  exists, only its listed clusters are ever subscribed by `matd` in the first
  place, so `--cluster` can narrow further within that set but never outside
  it — see [Subscriptions (`subscriptions.toml`, optional, matd
  only)](#subscriptions-subscriptionstoml-optional-matd-only) below.
- `--count` (default `1`) is how many events to receive before exiting `0`.
  `--timeout-ms` (default `60000`) cuts the wait short; `0` means wait
  forever. Reaching `--count` exits `0`; the timeout firing with **zero**
  events received exits `3` (with at least one event received, it still exits
  `0` — same UX as `enl listen`).
- `mat` connects to `matd`, sends the `listen` request, and prints the ack
  line followed by one JSON event per line to stdout as they arrive:
  ```json
  {"timestamp":"...","listening":true}
  {"timestamp":"2026-07-20T21:00:00+09:00","node_id":21,"endpoint":1,"cluster":"occupancysensing","attribute":"occupancy","value":1,"priming":false}
  ```
  `priming: true` marks events from the initial report burst right after
  matd (re)establishes a subscription, so a consumer does not mistake
  matd-restart residual state (e.g. `occupancy` still `1` from before a
  restart) for a fresh trigger. Only **scalar** values become events —
  `list`/`struct` attributes (ACL, server-list, etc., which show up in a
  wildcard priming burst) are dropped, the same known limitation as generic
  `read` (see [Scalar-only generic write / invoke](#scalar-only-generic-write--invoke)).
- `matd` absent, refusing the connection, or dying mid-stream is
  `matd_unavailable` (exit **13**) — see
  [Errors and exit codes](#errors-and-exit-codes). Events already printed
  before a mid-stream matd loss stay printed; the process still exits `13`
  (not `3`), even if `--count` was not reached.
- Usage form (a consumer like casa loops itself; `mat`/`matd` never run
  automations — see [Backend](#backend) / ARCHITECTURE.md "Design rules"):
  ```bash
  while ev=$(mat listen --node 21 --cluster occupancysensing --count 1 --timeout-ms 0); do
    # inspect $ev and react, e.g. mat on / mat off
  done
  ```

See [Routing through `matd`](#routing-through-matd) for what `matd` actually
subscribes to and how events reach it.

### Multi-admin share

To share a `mat`-owned device with another controller (Alexa / Apple / Google),
open a commissioning window and return a one-time issued code. This runs the
Administrator Commissioning cluster's OpenCommissioningWindow (ECM) natively
over a CASE session to the node.

```bash
# open-window --node <node_id> [--timeout S] [--iteration N] [--discriminator D]
mat open-window --node 5
mat open-window --node 5 --timeout 300
```

Output:

```json
{ "timestamp": "...", "node_id": 5, "manual_code": "36217551492", "qr_payload": "MT:-24J0AFN00KA0648G00", "expires_at": "2026-06-06T12:37:56+09:00" }
```

- Returns **both** `manual_code` (11-digit) and `qr_payload` (the `MT:...`
  string).
- **Rendering the QR image is not `mat`'s job.** stdout emits the `qr_payload`
  string only; drawing is out of scope.
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

### Groupcast

Control many devices at once with a Matter **wire group**: a GroupId plus a key
set is burned into each device, then a single multicast send hits all of them.
This is the original motivation (no "popcorn effect" of lights turning on one by
one). `mat` runs the whole path natively: the device-side Group Key Management /
Groups writes over CASE, and the controller-side group state written straight
into the credential KVS (`mat` is the sole owner/writer of that state, in
chip-tool-compatible INI form). Logical group names ("the living-room lights")
are out of scope —
`mat` takes a numeric GroupId (`-g/--group` and `--nodes` also accept an
alias from the optional `aliases.toml`, which is just a local nickname for the
number; see [Aliases](#aliases-aliasestoml-optional)).

```bash
# Provision: burn the key set + mapping + ACL group entry into every node, and
# set up the controller-side group state. --group is the GroupId, --nodes one
# or more commissioned node_ids.
# provision --group <ID> --nodes <N>... [--keyset-id N] [--name NAME]
#                                       [--endpoint EP] [--epoch-key HEX]
mat group provision --group 1 --nodes 5 6 7 --name living

# Add a node to an existing group: pass --rebind with ALL existing members plus
# the new one, and the SAME --keyset-id the group already uses.
mat group provision --group 1 --nodes 5 6 7 8 --name living --rebind

# Invoke: one multicast send to the group (unacknowledged).
# invoke --group <ID> --cluster <NAME> --command <NAME> [args...] [--endpoint EP]
mat group invoke --group 1 --cluster onoff --command on

# Grant (repair): run just the ACL step on already-provisioned nodes. Use it for
# groups provisioned before the ACL step existed (or through an old matd).
# Idempotent: nodes that already have the entry are reported as "unchanged".
# grant --group <ID> --nodes <N>...
mat group grant --group 1 --nodes 5 6 7
```

Outputs:

```json
// provision — all listed nodes succeeded (provision stops at the first failure)
{ "timestamp": "...", "group_id": 1, "keyset_id": 42, "name": "living", "endpoint": 1, "nodes": [5, 6, 7], "status": "provisioned" }

// provision --rebind via the direct path also notes the matd restart caveat
{ "timestamp": "...", "group_id": 1, "keyset_id": 42, "name": "living", "endpoint": 1, "nodes": [5, 6, 7, 8], "status": "provisioned", "note": "rebound keyset binding; if matd is running, restart it to reload group state" }

// provision when the controller-side write went native (MAT_IFACE/MAT_MATD_IFACE
// set, M8c-2) always carries this note instead — regardless of --rebind
{ "timestamp": "...", "group_id": 1, "keyset_id": 42, "name": "living", "endpoint": 1, "nodes": [5, 6, 7], "status": "provisioned", "note": "controller group state written natively to kvs; if matd is running, restart it to reload group state" }

// invoke — multicast is fire-and-forget; only "sent" can be reported
{ "timestamp": "...", "group_id": 1, "cluster": "onoff", "command": "on", "endpoint": 1, "status": "sent", "note": "unacknowledged groupcast; per-device delivery not confirmed" }

// grant — per-node repair result (ACL updated vs already had the entry)
{ "timestamp": "...", "group_id": 1, "nodes": [5, 6, 7], "updated": [5, 7], "unchanged": [6], "status": "granted" }
```

- **Groupcast is unacknowledged.** `group invoke` reports `"sent"`, never "all 7
  turned on." There is no per-device result and no read-after-write check at the
  group level — confirm individual devices with `mat read` if needed.
- **`--epoch-key` is optional.** It is the 16-byte (32-hex) AES key shared by the
  group. Omit it and `mat` generates a random one (single-controller use); pass a
  fixed key only when several controllers must share the same wire group. The key
  is never printed to stdout (it is a credential; it lives in the KVS).
- `--keyset-id` defaults to 42, `--name` to `grp<group_id>`, `--endpoint` to 1.
- **Provision is heavy and fragile** (KeySetWrite / GroupKeyMap / AddGroup / ACL
  write on every node) and **especially unstable on Thread** (multicast retransmits and
  IPv6 packet drops lower delivery). Wi-Fi / Ethernet Matter lights fare better.
- It stops at the **first failed node/step** (the error `detail` says which) so
  stdout stays pure JSON; re-run after fixing the offending node.
- **Provision also writes the device ACL (its 4th per-node step).** Group
  commands arrive with authMode=Group, so each device needs an ACL entry
  `{privilege: Operate, authMode: Group, subjects: [GroupId]}` — commissioning
  only creates the CASE admin entry, and without the group entry every device
  **silently drops** the groupcast (it is unacknowledged, so nothing fails
  visibly). The step is a read-merge-write: `mat` reads the current ACL, appends
  the entry only when missing (idempotent, existing entries — including other
  groups' — are preserved), and writes the full list back. If the ACL read
  cannot be parsed, `mat` stops with `parse_error` and **never writes** (an ACL
  write replaces the whole list; a blind write could drop the admin entry and
  make the device unmanageable).
- **Adding a node to an existing group: `--rebind`.** The controller-side
  group state persists across runs (in the credential KVS `mat` writes
  directly), so re-running provision on an existing group fails with a
  duplicate-bind error (`use --rebind` in the `detail`) — worse, the earlier
  keyset-add step has already rotated the controller's epoch key, leaving it
  out of sync with the devices (groupcast silently breaks). Without
  `--rebind` this failure is intentional (it stops you from rotating keys by
  accident). With `--rebind`, provision unbinds the keyset binding first
  (best-effort; also safe on a brand-new group) and re-provisions cleanly.
  Three rules: pass **all existing members plus the new node** to `--nodes` (a fresh
  epoch key is generated, so nodes left out stop receiving groupcasts), keep the
  **same `--keyset-id`** (the device keyset table holds max 3 entries and the
  IPK uses one), and confirm membership per node with
  `mat read -e 0 -c groupkeymanagement -a group-key-map`. After a direct-path
  `--rebind`, restart `matd` if it is running (it may still hold the old group
  state in memory; the KVS is already updated) — the output `note` says so
  (see Outputs above).
- **`mat group grant` repairs older groups.** Groups provisioned before this
  step existed — including any provision routed through a `matd` ≤ 0.12, which
  does not run the ACL step — lack the entry and their groupcast is silently
  ignored. The controller-side group state is not idempotent, so provision
  cannot simply be re-run — use `provision --rebind` to re-run it on an
  existing group; `grant` runs just the ACL step instead. It is direct path
  only (`--matd` exits 2).

Color / brightness shortcuts for groups (same conversions as the single-node
`mat color-temp` / `mat color` / `mat level`, delivered as an unacknowledged
groupcast — the result is `"status": "sent"` only; per-device delivery is not
confirmed). Like all ColorControl / LevelControl commands sent with
optionsMask=0, they only take effect on devices that are currently on:

```bash
mat group color-temp --group 1 --kelvin 2700
mat group color --group 1 --name pink
mat group color --group 1 --rgb "#ff00aa" --transition 30
mat group color --group 1 --hue 330 --sat 80
mat group level --group 1 --percent 100
```

### Routing through `matd`

Each `mat` call is a one-shot: it establishes CASE, runs the op, and discards
the session. With a running `matd` the call is routed through its **warm**
session instead — same subcommands, same JSON on stdout, but the handshake is
skipped on repeated calls. `mat` **auto-detects** `matd`: for supported
subcommands it tries a connect on the default socket candidates, uses `matd` when something
answers, and silently falls back to `mat`'s own native direct path when nothing
does (missing and stale sockets alike).

```bash
# Start the resident daemon (separate binary; see ARCHITECTURE.md / matd --help).
# With no --socket it binds the default path ($XDG_RUNTIME_DIR/matd/matd.sock,
# dir auto-created 0700; /tmp/matd.sock without XDG_RUNTIME_DIR) — the first
# default mat probes below.
matd &

# No flag needed: mat finds the running matd on the default socket by itself.
mat read --node 5 --cluster onoff --attribute on-off
mat describe --node 5
mat group invoke --group 1 --cluster onoff --command on

# Force the matd path (connection failure becomes an error instead of a
# fallback); pass a path to use a non-default socket.
# Caution: `--matd` takes an optional value (num_args = 0..=1), so a
# value-less `--matd` placed *before* the subcommand swallows the
# subcommand name as the socket path and fails to parse. Put it after the
# subcommand instead (or give it a value, e.g. `--matd=<path>`).
mat read --node 5 --cluster onoff --attribute on-off --matd
mat --matd /run/mat/matd.sock on --node 5
export MAT_MATD=1                       # same, for a whole shell session

# Opt out (always direct path, no probing):
MAT_MATD=0 mat read --node 5 --cluster onoff --attribute on-off
# export MAT_MATD_SOCKET=/run/mat/matd.sock   # pins which socket to probe/use
```

Stop the daemon with `matd stop`, which sends a shutdown request over the same
socket and triggers a graceful teardown (warm sessions dropped, socket removed):

```bash
matd stop                             # default socket
matd stop --socket /run/mat/matd.sock
```

Only one `matd` runs per socket: startup takes an exclusive `flock` on
`<socket>.lock`, so a second launch on the same socket exits `1` with `matd
already running (lock held at ...)` instead of silently hijacking it.

`matd` is native and pure Rust — it speaks a plain unix-socket protocol and
holds warm per-node CASE sessions in-process (a few KB each). There is no child
process and no CPU busy-loop, so sessions are held indefinitely (no idle
reaper). It **starts even with no fabric materials** — each op returns
`store_missing` (exit 10) until you run `mat fabric init` — and refuses to start
only when interface autodetect is ambiguous (set `MAT_MATD_IFACE`).

- Route selection: `--matd` / `MAT_MATD=<truthy>` **force** the matd path
  (connection failure is an error, no fallback). `MAT_MATD=<falsy>`
  (`0`/`false`/`no`/`off`) forces `mat`'s own direct path, no probing. Otherwise
  (default) `mat` **auto-detects**: it probes the socket with a connect and
  falls back to the direct path when nobody answers. `MAT_MATD_SOCKET` just
  selects *which* socket in every mode.
- Socket path precedence (all modes): `--matd <path>` > `MAT_MATD_SOCKET=<path>`
  (a single socket in both cases) > default candidates, probed in order:
  `$XDG_RUNTIME_DIR/matd/matd.sock` (the systemd `RuntimeDirectory=matd`
  convention, matd's own bind default) then the pre-0.27.0
  `$XDG_RUNTIME_DIR/matd.sock` (transition compat); just `/tmp/matd.sock`
  without `XDG_RUNTIME_DIR`. Stale sockets fail the connect and fall through
  naturally.
- Once connected, errors are reported from the matd path as-is — `mat` never
  re-runs the command on the direct path (no double execution of writes).
  Which path ran is logged to stderr at info level (`MAT_LOG=info`).
- Supported over matd: `read` / `write` / `invoke` / `on` / `off` /
  `color-temp` / `color` / `level` / `describe` / `group` (`provision` /
  `invoke` / `color-temp` / `color` / `level`; `group grant` is direct only —
  see Groupcast above). `discover` / `commission` / `fabric init` /
  `open-window` / `diag` are direct-only: auto-detection skips them silently;
  explicit `--matd` exits `2`. `listen` (below) is the opposite case — it is
  **matd-only**, with no direct-path fallback at all (not even auto-detect
  skip-and-run-direct); without a reachable `matd` it is `matd_unavailable`
  (exit `13`).
- node_id commissioning is re-checked by `matd` against the same credential store
  per request, so the error kinds and exit codes match the direct path.

#### Resident Subscribe and `mat listen`

At startup `matd` reads the commissioned-node ledger and opens one **wildcard**
Subscribe per node (every endpoint/cluster/attribute — the same "all-paths
omitted" shape as a wildcard `read`), so device-originated attribute changes
(occupancy, open/close, temperature, on-off, ...) are captured continuously,
not just when a `mat` caller happens to be polling.

- Subscribe parameters: `MinIntervalFloor = 0` (no artificial delay on
  fast-changing sensors like occupancy), `MaxIntervalCeiling = 300s` (the
  device still picks the actual interval; a device on a flaky Thread link
  silently discards its subscription when report delivery fails, and the
  keepalive cadence is the only liveness signal the subscriber gets — 300s
  bounds that blind window to ≤7.5 min, where the original 3600s left matd
  blind for up to 90 minutes), `KeepSubscriptions = false` (a re-subscribe
  replaces rather than piles onto the device's existing subscription table).
- A subscription that fails to establish, or that goes silent for more than
  **1.5× its negotiated MaxInterval** (subscription-death detection), is
  re-subscribed with exponential backoff starting at 5s, capped at 5 minutes.
  Retries are logged at `debug`; only the established/lost state transitions
  are logged at `info` — a flaky Thread node re-subscribing every few seconds
  does not spam the log.
- Events fan out from each subscription's report pump through one
  `tokio::sync::broadcast` channel to every connected `mat listen` client,
  filtered per client. A listener that falls behind and misses events on the
  channel gets a single `{"error":{"kind":"other","detail":"event stream
  lagged"}}` line and is then disconnected — never silently dropped events.
- `matd` holds **no** event history (no ring buffer, no replay): a `mat
  listen` client only sees events emitted while it is connected, same as
  `enl listen`. `priming` (see [Listen](#listen-device-originated-events))
  is the mechanism for telling initial-state reports apart from later
  changes without needing a replay log.
- `listen` is the **only** op that breaks the "one line request = one line
  response" rule of the `matd` socket protocol: it replies with one ack line
  (`{"timestamp":...,"listening":true}`), then keeps the connection open and
  streams matching event lines until the client disconnects.
- v1 scope is attribute reports only. Not yet implemented (tracked as
  future work): EventReport delivery (buttons / Generic Switch), a
  `DataVersionFilter`, and LIT ICD check-in registration. Cluster-level
  narrowing of what gets subscribed **is** implemented — see
  [Subscriptions (`subscriptions.toml`, optional, matd
  only)](#subscriptions-subscriptionstoml-optional-matd-only) below.

### Native backend internals

`mat` and `matd` share one engine (crate `mat-native`, sitting on the protocol
library `mat-controller`). `matd` holds warm per-node CASE sessions; `mat`
establishes → runs one op → discards (design rule 4). The stdout JSON schema is
identical either way — the process only differs in session lifetime.

#### Interface selection

The engine needs the Thread-mesh network interface. `mat` **auto-detects** it
every run (no stored state): the sole interface that is up (has carrier),
multicast-capable, non-loopback, non-point-to-point (tunnels like `tailscale0`
are excluded), and holds an IPv6 link-local address. If exactly one qualifies it
is used; zero or two-or-more is a hard error (`other`) that lists the candidates
and asks you to set the override.

- `MAT_IFACE` (or the global `--iface <name>`) overrides autodetect for `mat`.
- `MAT_MATD_IFACE` (or `matd --iface <name>`) overrides it for `matd`. These are
  deliberately separate names for two different processes; `matd` refuses to
  start on an ambiguous autodetect (a whole-daemon misconfiguration, so it
  fail-fasts rather than erroring per-op).

On jarvis (`eth0` + `tailscale0`) and WSL (`eth0`) exactly one candidate remains,
so autodetect just works.

#### Fabric index, sessions, epoch

- `MAT_FABRIC_INDEX` (default `1`) and `MAT_ISSUER_INDEX` (default `0`) select
  the KVS fabric-table and CA-issuer entries for `mat`; `matd` mirrors them as
  `MAT_MATD_FABRIC_INDEX` / `MAT_MATD_ISSUER_INDEX` (also `--fabric-index` /
  `--issuer-index`). Pass the same values to both on the same host. If you share
  a fabric with another admin the index is usually not `1`.
- **Warm sessions** (matd only) are held per node indefinitely. A send that
  exhausts MRP retransmission (timeout) discards the session and does one
  automatic mDNS re-resolve + re-CASE before failing. `mat`'s one-shot session
  can't be stale, so it never retries — a failure is reported as-is.
- **Epoch (IPK).** `commission` needs the fabric's epoch IPK (the key `AddNOC`
  hands the device — distinct from the KDF-derived *operational* key that is the
  only one persisted). It is resolved in order: (1) the `mat`-owned KVS key
  `mat/f/<idx>/ipk-epoch` if present; (2) otherwise the fixed chip-tool default
  is checked against the fabric's KVS materials via a KDF guard, and on a match
  it is **adopted and persisted** to that key (so a fabric first created by
  chip-tool keeps working, and later commissions read the persisted value); (3)
  a mismatch (rotated IPK, or a non-chip-tool fabric) is a `store_parse` hard
  error. A fabric created by `mat fabric init` starts at case (1) with a random
  epoch. Adoption happens on the first native commission — no separate step.

#### Scalar-only generic write / invoke

Generic `write` / `invoke` (and `group invoke`) encode **scalar** JSON→TLV types
only: bool / int / uint / enum / bitmap / string / octstr (bytes as a
`hex:`-prefixed string). An attribute or command field the name table knows to be
`list` / `struct` / `float` is rejected up front with `parse_error` (the detail
names the type). This is a deliberate, documented limitation — the practical
cases (onoff / level / color, and the ACL entry `group grant` appends) are all
covered, and the numeric-ID escape hatch remains for names the generated table
does not resolve (an unknown name is also a `parse_error`; pass the numeric id).
The `group provision` / `grant` list/struct writes (KeySetWrite, GroupKeyMap,
binding, ACL read-modify-write) use dedicated encoders, not the generic path.

#### Groupcast counter (shared between `mat` and `matd`)

Native groupcast is a single unacknowledged AES-CCM-sealed packet to the
site-local transient multicast address (`ff35::.../64`, hop limit 64) — no
response, no MRP. The per-sender counter is persisted at
`<store>/native_group_counter` (plain decimal, written ahead by 4096 so a crash
never reuses a value), opened under an exclusive `flock` on `<path>.lock` for the
life of the process.

- **`mat` and `matd` share this one file.** Whichever process holds the lock
  sends with it; the other finds it locked (`WouldBlock`) and reports the group
  op as unavailable rather than racing the counter. Because both send as the
  same source node id, they share one per-sender counter window on the receiving
  devices.
- **Pick one group sender.** If a native `matd` is running, send all groupcasts
  through it. Its warm engine re-reads the group's operational credentials from
  the KVS on every send, so a `group provision --rebind` takes effect on the very
  next send with no restart. Do not mix senders: once one has advanced the
  counter, the other is behind, and devices silently drop its groupcasts as
  stale/duplicate (a `tracing::warn!` is logged, but routing is unchanged —
  refusing the send is a product decision, not made here). With `matd` running,
  route priority already sends every group op to it first, so you normally never
  reach for `MAT_MATD=0`.

#### Ops that never route through `matd`

`discover`, `commission`, `fabric init`, `open-window`, `diag thread` / `diag
node` / `diag mesh`, and `group grant` are not part of the `matd` socket
protocol at all (by design — rare, or no warm session to reuse). They always
run on `mat`'s own one-shot direct path (native), even when a `matd` is
running. `discover --probe`
and `diag node --deep` do a native **targeted** mDNS resolve per ledger node
(run concurrently), not a service-type enumeration: real Thread meshes have
advertising proxies that answer direct instance queries but omit instances from
PTR enumeration, so enumerate-and-match under-reports.

```bash
mat --iface eth0 on --node 5
# or: MAT_IFACE=eth0 MAT_FABRIC_INDEX=2 mat group invoke --group 10 --cluster onoff --command on
matd --iface eth0 &      # or MAT_MATD_IFACE=eth0 matd &
```

## Credential store

Resolution order: `--store <path>` > `$MAT_STORE` > `$XDG_CONFIG_HOME/mat` >
`~/.config/mat`. It holds the Root CA, the controller's keys/cert, the
commissioned-node ledger (`nodes.json`), the optional alias map
(`aliases.toml`, below), and the persistent Matter KVS (chip-tool-compatible
INI form — group keysets, operational credentials, the group-send counter;
`mat` is its sole reader/writer). **It is never committed** (excluded by
`.gitignore`).

## Aliases (`aliases.toml`, optional)

Numeric node / group / endpoint ids are easy to get wrong. If the file
`<store>/aliases.toml` exists, `mat` resolves locally defined names to those
numbers right after arg parsing — a purely local convenience. **Without the
file, behavior is exactly the traditional numeric-only one.** The wire and the
backend (native engine / `matd`) always receive numbers, and stdout keeps the
numeric schema (no alias echo-back).

```toml
version = 1

[nodes]
living-light = 5
hall-sensor = 12

[groups]
all-lights = 258

[endpoints.living-light]
main = 1
night = 2

[endpoints.12]
pir = 3

[colors]
warm = "#ff8c00"
mypink = "255,182,193"

[thread]
"AABBCCDDEEFF0011" = "otbr-br"
```

- `nodes`: alias → node_id. Accepted by `-n/--node` (read / write / invoke /
  describe / on / off / color-temp / color / level / open-window / diag thread / diag node) and
  by `--nodes` in `group provision` / `diag mesh` (each element resolved independently).
- `groups`: alias → GroupId. Accepted by `-g/--group` in every `group`
  subcommand (`provision` / `invoke` / `grant` / `color-temp` / `color` / `level`).
- `endpoints`: defined **per node** — the outer key is a node alias or a
  node_id digit string, the inner map is alias → endpoint number (endpoint
  numbers mean different things on different nodes, so there is no global
  endpoint dictionary). Accepted by `-e/--endpoint` on node-taking commands;
  the lookup uses the *resolved* node, so `-n 5 -e main` and
  `-n living-light -e main` give the same result. The `-e` of `group
  provision` / `group invoke` / `group color-temp` / `group color` / `group
  level` is **numeric only** (no node context to resolve against).
- `colors`: custom color name → RGB value (`#rrggbb` / `rrggbb` / `R,G,B`),
  used by `--name` in `color` / `group color`. Entries are defined as RGB and
  go through the same RGB → HSV pipeline as `--rgb`. A user-defined name
  **overrides** the built-in color table (you can redefine `red`). Without the
  file the built-in table still works. A value that does not parse as RGB is
  `store_parse` (exit `10`); an unknown color name is a CLI argument error
  (exit `2`) listing the known names.
- `thread`: Thread ExtAddress (16 hex, case-insensitive) → display label, used
  by `mat diag mesh` to name unknown participants (OTBR border routers, other-
  fabric devices) that show up in a neighbor/route table but were never
  commissioned onto this fabric, so they have no `nodes` alias to fall back
  on. The graph's `label` field matches on ExtAddress regardless of fabric
  status, so a commissioned node whose ExtAddress happens to be listed here
  gets a `label` too, alongside its `nodes` `alias`.

```bash
# With the aliases.toml above, these are equivalent:
mat on -n living-light
mat on -n 5
```

Resolution rules:

- A value that parses as a number is used as-is (numbers win; full backward
  compatibility). Only non-numeric values are looked up in `aliases.toml`.
- Alias names must be non-empty and not all digits (this shadowing is rejected
  when the file is loaded: `store_parse`, exit `10`).
- An unknown alias — or any alias given when there is no `aliases.toml` in the
  store — is a CLI argument error (exit `2`); the stderr `detail` lists the
  known aliases (or says `no aliases.toml in store`) so the caller can
  self-correct.
- A corrupt `aliases.toml` is `store_parse` (exit `10`).
- Cluster / attribute / command names are **never** aliased (chip-tool
  notation only).

These map onto the existing exit codes (`2` / `10`); the
[Errors and exit codes](#errors-and-exit-codes) table is unchanged.

To register an alias while commissioning, add `--alias`:

```bash
mat commission --target 192.0.2.10 --setup-code "MT:Y.K9042C00KA0648G00" --node 5 --alias living-light
```

The name is validated **before** commissioning starts (all-digits / empty /
already taken → exit `2`, before any network op runs), and it is written
to `aliases.toml` only on success (the file is created if absent). Without
`--alias`, `commission` never touches `aliases.toml`. Deleting or renaming an
alias is a hand edit of the file — there is no management subcommand.

## Subscriptions (`subscriptions.toml`, optional, matd only)

By default `matd`'s resident Subscribe (see [Resident Subscribe and `mat
listen`](#resident-subscribe-and-mat-listen)) is a full **wildcard**: every
endpoint/cluster/attribute, on every commissioned node. If
`<store>/subscriptions.toml` exists, `matd` narrows that to just the listed
clusters' paths instead.

Full-wildcard priming (the initial full-attribute dump right after a
subscription is (re)established — dozens of request/response round trips) can
fail to complete on a weak Thread link, leaving a subscription unestablished
for tens of minutes to hours. Narrowing to a handful of clusters shrinks
priming to one or two chunks, so a link good enough for `read` is usually
good enough to subscribe on too.

```toml
clusters = [
  "onoff",
  "occupancysensing",
  "temperaturemeasurement",
]
```

- Cluster names use chip-tool notation (same as `mat read`); numeric ids
  (`"0x0006"` / `"6"`) also work, the same escape hatch as elsewhere for names
  `mat-core::ids` doesn't know.
- **Absent file = full wildcard, unchanged** — the same absent-file discipline
  as `aliases.toml`.
- A parse failure, an unknown cluster name, or an empty list makes `matd`
  **refuse to start** (`store_parse`, exit `10`); it never silently falls back
  to wildcard, so a misconfiguration can't quietly disable the weak-link
  workaround.
- **Edge case: nodes that serve none of the listed clusters.** When the file is
  present, the narrowed Subscribe is sent to every commissioned node; a node
  that exposes none of the listed clusters will never establish its subscription
  (it retries on backoff forever). Ensure each node serves at least one of the
  listed clusters.
- When this file is present, `mat listen` only ever sees events for the
  listed clusters — a `--cluster` filter naming a cluster outside that set
  simply never matches anything.
- Read once at `matd` startup; an edit needs a `matd` restart to take effect
  (e.g. `systemctl --user restart matd`).
- `mat` (one-shot) never reads this file — like the rest of the resident
  Subscribe, it is matd-only.

## Errors and exit codes

Errors go to stderr as `{"error":{"kind":"...","detail":"..."}}`.

| code | meaning |
|---|---|
| 0 | success |
| 2 | CLI argument error (clap default) |
| 10 | credential store missing / parse failure |
| 11 | node_id not commissioned |
| 12 | *(retired in 0.22.0 — historical vacancy)* |
| 3 | timeout |
| 4 | device rejected |
| 5 | unreachable / network |
| 6 | CASE session establishment failed |
| 13 | `matd` absent / unreachable (`mat listen` only) |
| 1 | other |

The native backend maps its own transport/IM outcomes onto `3` / `4` / `5` /
`6`; anything it cannot classify is exit `1`. An operational mDNS resolve
**timeout** (the node did not advertise within the wait window — often
recoverable by retrying, since Thread border routers advertise on a ~30s
cycle) is `timeout` (exit `3`); any other resolve failure (socket I/O, etc.)
is `unreachable` (exit `5`).

`kind` values (stable; callers may branch on these strings):

- `store_missing` / `store_parse` — credential store missing / corrupt (exit 10).
  `store_missing` typically means you have not run `mat fabric init` yet.
- `node_not_commissioned` — node_id not in the store (exit 11)
- `timeout` (exit 3) / `device_rejected` (exit 4) / `unreachable` (exit 5) —
  classified from the native transport / IM result
- `session_failed` — IP reachable but CASE (operational secure session) could not
  be established, e.g. an intermittent `CHIP Error 0x54 (Invalid CASE parameter)`
  during the Sigma exchange (exit 6). Distinct from `unreachable` (no IP route)
  and `device_rejected` (the device answered and refused); typically retryable.
- `commission_failed` — commissioning failed (unclassified residue, exit 1).
  Since 1.0.0 timeouts during PASE/CASE map to `timeout` and explicit device
  refusals (wrong passcode / StatusReport rejection / bad Sigma2 signature) map
  to `device_rejected` instead of landing here.
- `parse_error` — this kind is returned when a generic `write` / `invoke` names
  a known attribute or command field whose type is `list` / `struct` / `float`
  (not supported by the scalar-only JSON→TLV encoder — rejected up front), or
  names a cluster / attribute / command the generated table does not know (pass
  the numeric id instead).
- `matd_unavailable` (exit 13) — `matd` was not reachable or died mid-request.
  For `mat listen`: no socket, connection refused, `MAT_MATD=0`, or the
  connection was cut partway through the event stream (`mat listen` has no
  direct-path fallback). Since 1.0.0 also for every other op on the matd path:
  forced `--matd` failing to connect, or an I/O failure / silent disconnect
  after the request line was sent (the request may or may not have been
  executed — the detail says so; there is deliberately no direct-path retry, to
  avoid double execution of writes). Distinct from `timeout` (exit 3), which
  `mat listen` uses only for "connected fine, zero events arrived before
  `--timeout-ms`."
- `other` — anything else (exit 1); also what a `group provision` KVS write
  returns once the write is attempted and fails — including a duplicate bind
  (`detail` says `use --rebind`) or the KVS being locked by a concurrent writer
  (`flock` `WouldBlock`). These are hard errors (the KVS may already be touched),
  distinct from an unresolvable KVS, which surfaces as `store_missing` /
  `store_parse`. Ambiguous interface autodetect is also `other`.
- `child_not_found` (exit 12) / `child_failed` (exit 1) — **not emitted as
  top-level errors since 0.22.0** (they classified chip-tool spawn/exit failures,
  now removed). The variants and exit-code mapping are kept only for wire
  compatibility with responses from older `mat` / `matd`. (`mat` still
  constructs `child_not_found` internally to record a missing `ping6` as a
  `tool_missing` entry inside `diag node --deep`'s `unavailable` array — this
  never becomes exit 12.)

## Backend

`mat`'s backend is a **native, from-scratch Rust Matter controller** (crate
`mat-controller`, driven through the shared `mat-native` engine) — TLV, CASE,
IM, groupcast, mDNS, and commissioning (on-network + BLE+Thread) are all
in-process. There is no `chip-tool` (or any external controller) subprocess.

- **Route selection is per-op:** matd auto-discovery (if a `matd` answers the
  probed socket) → `mat`'s own native direct path. See
  [Routing through `matd`](#routing-through-matd) and
  [Native backend internals](#native-backend-internals) for interface
  autodetect (`MAT_IFACE` / `MAT_MATD_IFACE` override), fabric index, warm vs
  one-shot sessions, the shared groupcast counter, epoch adoption, and the
  scalar-only generic write/invoke rule.
- **First-fabric bootstrap** is `mat fabric init` (random-epoch IPK); see
  [that section](#first-fabric-bootstrap-fabric-init).

Environment variables:

| variable | purpose |
|---|---|
| `MAT_STORE` | credential store path (see resolution order above) |
| `MAT_IFACE` | override interface autodetect for `mat`'s direct path |
| `MAT_MATD_IFACE` | override interface autodetect for `matd` |
| `MAT_FABRIC_INDEX` / `MAT_ISSUER_INDEX` | `mat` KVS fabric-table / CA-issuer index (default `1` / `0`) |
| `MAT_MATD_FABRIC_INDEX` / `MAT_MATD_ISSUER_INDEX` | same for `matd` |
| `MAT_MATD` / `MAT_MATD_SOCKET` | force / opt out of the matd path; pin its socket |
| `MAT_PAA_TRUST_STORE` | directory of PAA root certs for attestation |
| `MAT_CD_SIGNER_STORE` | CD signer trust store (warn-only if absent) |
| `MAT_THREAD_DATASET` | Thread active operational dataset (hex) for BLE+Thread commission |
| `MAT_PING6_BIN` | override the `ping6` binary used by `diag node --deep` |
| `MAT_LOG` | `tracing` filter for stderr logs (e.g. `info`) |

> Matter uses mDNS / IPv6 multicast, so running in Docker **requires host
> networking** (`docker run --network host`). A bridge network cannot receive
> the responses.

## Development

Tasks are defined with [Task](https://taskfile.dev) (`task` lists them).

```bash
task build            # release build -> target/release/{mat,matd}
task install          # install both binaries into ~/.cargo/bin
task run -- discover  # run (native backend)
task test             # tests (native FakeConn + binary integration; no real devices)
task clippy           # lint (-D warnings)
task fmt              # format
task check            # CI equivalent (fmt:check + clippy + test)

task dist:arm64       # aarch64-gnu + BLE deploy build -> dist/arm64/{mat,matd}
task docker:build     # slim x86_64 image (mat/matd only)
task docker:run -- discover
task docker:test      # no local toolchain needed
```

CI (GitHub Actions, `.github/workflows/ci.yml`) runs the same fmt / clippy /
test sequence as `task check`. The default build and CI do not use the `ble`
cargo feature; deploy builds (`task dist:arm64`) enable it for BLE+Thread
commissioning.

## Manual E2E (with real devices; not in CI)

In practice the main path is **multi-admin join**: adding a device that is
already commissioned by another admin (such as Home Assistant) to `mat` as well.
The printed code does not work (the device left commissioning mode), so the
existing admin opens a commissioning window to issue a one-time code.

1. **Share from the other admin:** on the other controller, run "Share" for the
   target device and note the issued setup code (`MT:...` or 11-digit).
2. **Join with `mat`:**
   ```bash
   mat commission --target <device-ip-or-host> --setup-code "<issued setup code>" --node 5
   ```
   It returns `{ "node_id": 5, "status": "success" }` and records the ledger in
   `~/.config/mat/nodes.json`.
3. **Confirm:** `mat discover` now shows node 5 with `"state": "commissioned"`.

> For a factory-reset device, pass the printed setup code directly to
> `commission` (first commission).

### State operations E2E

Against a commissioned node (node 5 above), confirm read / describe / on / off
on a real device.

```bash
# Introspect what you can call (endpoints and numeric cluster IDs)
mat describe --node 5

# Read the OnOff attribute (for a light, its current on/off state)
mat read --node 5 --cluster onoff --attribute on-off

# Turn on -> off (invoke of the OnOff command, not an attribute write)
mat on --node 5
mat off --node 5

# Read-after-write check (confirm the value took effect)
mat on --node 5 && mat read --node 5 --cluster onoff --attribute on-off   # -> "value": true
```

### Share E2E (mat -> another admin)

Share `mat`-owned node 5 with another controller.

```bash
# Open a commissioning window (get the issued code)
mat open-window --node 5 --timeout 300
# -> { "node_id": 5, "manual_code": "...", "qr_payload": "MT:...", "expires_at": "..." }
```

Enter the returned `manual_code` (11-digit) or `qr_payload` (render the QR with
the receiving tool) into the other controller's "Add device" flow (Alexa / Apple
Home / Google Home). Finish before `expires_at`. After sharing, `mat` keeps its
fabric membership (multi-admin).

> Each one-shot run pays mDNS resolution plus a CASE handshake, so a single call
> is slow (hundreds of ms to seconds). Speed-sensitive use cases run `matd`,
> which keeps warm sessions (see ARCHITECTURE.md).

### Groupcast E2E (real devices)

With several commissioned lights (say nodes 5, 6, 7), burn a wire group and fire
one multicast send at it.

```bash
# Provision the group onto every node (controller-side state is set up too)
mat group provision --group 1 --nodes 5 6 7 --name living
# -> { "group_id": 1, "keyset_id": 42, "nodes": [5,6,7], "status": "provisioned", ... }

# One multicast send — all three should react together (no popcorn effect)
mat group invoke --group 1 --cluster onoff --command on
mat group invoke --group 1 --cluster onoff --command off
```

> Groupcast is **unacknowledged**, so `group invoke` only confirms the send, not
> delivery. If a light did not react, confirm it individually (`mat read --node 6 -c
> onoff --attribute on-off`) and re-provision that node. Multicast is **especially weak on
> Thread**; Wi-Fi / Ethernet lights are more reliable. The KVS records `mat`
> writes (keyset table, group table, GroupKeyMap) follow the connectedhomeip
> v1.4.2.0 `GroupDataProviderImpl` link discipline, so a real `chip-tool` on the
> same store can still read them — if a devices-side provisioning step regresses,
> the group-settings writer is the first place to check.
>
> If **no** device reacts although provision reported success, suspect the
> device ACL first: provisions made before the ACL step (or through an old
> `matd` ≤ 0.12) never granted the group permission, and devices silently drop
> unauthorized groupcast. `mat group grant --group 1 --nodes 5 6 7` adds the
> missing entries idempotently.

## Contributing

Issues and pull requests are welcome. Before sending a PR, run `task check`
(format check + clippy with `-D warnings` + tests); it needs no real devices.
Please keep stdout pure JSON and follow the design rules in
[ARCHITECTURE.md](./ARCHITECTURE.md).

## License

[MIT](./LICENSE).
