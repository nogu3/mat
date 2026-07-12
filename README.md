# mat

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

`mat` is a CLI for controlling Matter devices. It calls a Matter controller
(`chip-tool`) as a subprocess and returns its long text output as **pure
structured JSON**, normalized to `mat`'s own schema.

- stdout = one JSON object per command. No human decoration.
- diagnostics go to stderr as structured logs (`tracing`).
- it holds no state except the credential KVS (the process is one-shot).

For the design background, the `mat` / `matd` split, and what `mat` does and
does not do, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Status

Everything documented below is implemented: discover / commission, state
operations (read / write / invoke / describe / on / off), multi-admin share
(`open-window`), groupcast (`group provision` / `group invoke`), the resident
daemon `matd` (warm CASE sessions, `mat --matd`), and `mat diag thread`. All of
it passes the fake-chip-tool / fake-ws integration tests, and real-device E2E
has confirmed commissioning, read/write/invoke, `matd`'s warm sessions, error
classification, group provisioning, and `diag thread`. Group *delivery* is
unacknowledged multicast by design, so per-device actuation cannot be confirmed
from the controller side (see Groupcast below).

The development roadmap (including the optional, not-started native-backend
replacement) lives in [ARCHITECTURE.md](./ARCHITECTURE.md).

## Requirements

- Rust (stable) and [Task](https://taskfile.dev) to build.
- A `chip-tool` binary on your `PATH` (or set `MAT_CHIP_TOOL_BIN` to its full
  path). Building `chip-tool` is heavy, so a Docker image with it baked in is
  provided (see [Backend](#backend-chip-tool)).
- Matter uses mDNS / IPv6 multicast, so on a real network the host must be able
  to send and receive these.

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

With `--probe`, each `commissioned` node is checked against live mDNS
(`avahi-browse _matter._tcp`, one browse for all nodes) and annotated:

- `reachable: true` — advertising now; `address` is the live-resolved value
  (may differ from the ledger).
- `reachable: false` — not advertising; `address` is the last-known ledger
  value with `stale: true`.
- `reachable: null` — the mDNS probe could not run (e.g. `avahi-browse`
  missing); reachability is unknown. A diagnostic is logged to stderr.

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

#### Attestation / PAA trust store

Production Matter devices ship a DAC signed by a **production PAA** (Product
Attestation Authority). With only chip-tool's built-in development PAA,
commissioning fails attestation (`device_rejected`, "Failed Device
Attestation"). Point `mat` at a directory of PAA root certificates:

```bash
# Option 1: explicit env var
export MAT_PAA_TRUST_STORE=/path/to/paa-root-certs
# Option 2: drop the certs under the store, no env needed
#   <store>/paa-trust-store/   (e.g. ~/.config/mat/paa-trust-store/)
mat commission --target 192.0.2.10 --setup-code "MT:Y.K9042C00KA0648G00" --node 5
```

Resolution order: `MAT_PAA_TRUST_STORE` > `<store>/paa-trust-store/`. If neither
exists, `mat` passes no trust store and only chip-tool's development PAA applies
(fine for test devices, not for retail ones). Get the certificates from
connectedhomeip's `credentials/production/paa-root-certs/`.

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

# Run a command: --command plus trailing chip-tool args
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
// read — value is chip-tool's `Data = ...` normalized to bool/number/string/null
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "onoff", "attribute": "on-off", "value": true }

// write
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "levelcontrol", "attribute": "on-level", "value": "128", "status": "success" }

// invoke (mat on / off have the same shape)
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "onoff", "command": "on", "status": "success" }

// color-temp — echoes both the input kelvin and the converted mireds so the
// result can be cross-checked against a `color-temperature-mireds` read
// (when --mireds is given, kelvin is back-computed the same way for the echo)
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "colorcontrol", "command": "move-to-color-temperature", "kelvin": 2700, "mireds": 370, "transition": 0, "status": "success" }

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

> `describe` calls chip-tool several times (parts-list plus each endpoint's
> server-list), so it is slow, but it finishes in one shot.

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

> Field names inside `neighbor_table` / `route_table` are kept verbatim from
> chip-tool (note `Lqi` in neighbors but `LQIIn` / `LQIOut` in routes), and
> `routing_role` is the numeric enum (5 = Router) — `mat` does not resolve names.

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
> `unreachable` / `timeout` instead. Like `describe`, this calls chip-tool
> several times but finishes in one shot. `mat diag` runs only on the direct
> chip-tool path (not via `--matd`).

`mat diag node --node <node_id>` answers a different question: **why can't I
control this commissioned node?** It runs layered checks and classifies the
result into a single `verdict` with the evidence and a recommended action —
where `mat invoke` would only return a bare `timeout` / `session_failed`.

```bash
# diag node --node <node_id> [--endpoint EP] [--deep]   (EP defaults to 0)
mat diag node --node 5            # chip-tool only (fast)
mat diag node --node 5 --deep     # also probe ping6 + mDNS (avahi-browse)
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
> `unknown`. The default (chip-tool only) path can't tell `link_starved` (weak
> Thread link, SRP not registered — **the fabric is intact**) apart from
> `fabric_missing` (the device dropped our fabric); `--deep` adds the ping6 and
> mDNS evidence that distinguishes them. Like `diag thread` it returns **partial
> results** (skipped/failed checks go under `unavailable`) and **always exits
> `0`** with a verdict, even when the node is fully unreachable — the value is in
> the classification, not an exit code. `--deep` shells out to `ping6` and
> `avahi-browse` (override with `MAT_PING6_BIN` / `MAT_AVAHI_BROWSE_BIN`).
>
> `mdns.advertised_self_fabric` is whether the node advertises on **our** fabric
> specifically (vs. `advertised_any_fabric`, which is any fabric). It needs our
> compressed-fabric-id, which `mat` reads from the operational read's `chip-tool`
> logs — preferring the resolved `<cfid>-<node>` instance name, falling back to
> the `Compressed FabricId` line (both streams are scanned, since `chip-tool`
> logs to stdout). When neither is available the field is omitted and a
> `{"check": "mdns_self_fabric", "kind": "cfid_unavailable"}` entry appears under
> `unavailable`.

### Multi-admin share

To share a `mat`-owned device with another controller (Alexa / Apple / Google),
open a commissioning window and return a one-time issued code. This wraps
`chip-tool pairing open-commissioning-window` (ECM = option 1).

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
one). It wraps `chip-tool`'s group path (`groupsettings` / `groupkeymanagement` /
`groups`); `mat` holds no group state of its own (it lives in chip-tool's
storage). Logical group names ("the living-room lights") are out of scope —
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
  is never printed to stdout (it is a credential; it lives in chip-tool storage).
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
  `groupsettings` state persists across chip-tool runs, so re-running provision
  on an existing group fails at `bind-keyset` with `Duplicate key id` — worse,
  the earlier `add-keysets` step has already rotated the controller's epoch key,
  leaving it out of sync with the devices (groupcast silently breaks). Without
  `--rebind` this failure is intentional (it stops you from rotating keys by
  accident). With `--rebind`, provision unbinds the keyset binding first
  (best-effort; also safe on a brand-new group) and re-provisions cleanly. Three
  rules: pass **all existing members plus the new node** to `--nodes` (a fresh
  epoch key is generated, so nodes left out stop receiving groupcasts), keep the
  **same `--keyset-id`** (the device keyset table holds max 3 entries and the
  IPK uses one), and confirm membership per node with
  `mat read -e 0 -c groupkeymanagement -a group-key-map`. After a direct-path
  `--rebind`, restart `matd` if it is running (its warm chip-tool still holds
  the old group state in memory; storage is already updated).
- **`mat group grant` repairs older groups.** Groups provisioned before this
  step existed — including any provision routed through a `matd` ≤ 0.12, which
  does not run the ACL step — lack the entry and their groupcast is silently
  ignored. The controller-side `groupsettings` state is not idempotent, so
  provision cannot simply be re-run — use `provision --rebind` to re-run it on
  an existing group; `grant` runs just the ACL step instead. It is always
  direct chip-tool (`--matd` exits 2).

Color shortcuts for groups (same conversions as the single-node `mat
color-temp` / `mat color`, delivered as an unacknowledged groupcast — the
result is `"status": "sent"` only; per-device delivery is not confirmed).
Like all ColorControl commands sent with optionsMask=0, they only take effect
on devices that are currently on:

```bash
mat group color-temp --group 1 --kelvin 2700
mat group color --group 1 --name pink
mat group color --group 1 --rgb "#ff00aa" --transition 30
mat group color --group 1 --hue 330 --sat 80
```

### Routing through `matd`

By default each `mat` call spawns `chip-tool` and pays a fresh CASE handshake.
With a running `matd` the call is routed through its warm session instead —
same subcommands, same JSON on stdout, but the handshake is skipped on repeated
calls. `mat` **auto-detects** `matd`: for supported subcommands it tries a
connect on the default socket, uses `matd` when something answers, and silently
falls back to the direct chip-tool path when nothing does (missing and stale
sockets alike).

```bash
# Start the resident daemon (separate binary; see ARCHITECTURE.md / matd --help).
# With no --socket it uses the default path ($XDG_RUNTIME_DIR/matd.sock, else
# /tmp/matd.sock) — the same default mat probes below.
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

# Opt out (always direct chip-tool, no probing):
MAT_MATD=0 mat read --node 5 --cluster onoff --attribute on-off
# export MAT_MATD_SOCKET=/run/mat/matd.sock   # pins which socket to probe/use
```

Stop the daemon with `matd stop` — do **not** `kill` it, which orphans the child
`chip-tool` holding the ws port. `matd stop` sends a shutdown request over the
same socket and triggers a graceful teardown (child `chip-tool` killed, socket
removed):

```bash
matd stop                             # default socket
matd stop --socket /run/mat/matd.sock
```

Only one `matd` runs per socket: startup takes an exclusive `flock` on
`<socket>.lock`, so a second launch on the same socket exits `1` with `matd
already running (lock held at ...)` instead of silently hijacking it.

`matd`'s child `chip-tool interactive server` burns ~100% of one CPU core the
whole time it is alive — a busy-loop in its websocket service loop (upstream
[project-chip/connectedhomeip#29971](https://github.com/project-chip/connectedhomeip/issues/29971),
open since 2023). `matd` keeps the websocket alive with a periodic keepalive
ping and preserves the warm child across transient socket errors, but the only
way to stop the CPU burn is to let the idle reaper kill the child. Keep
`--idle-timeout` moderate (default 300 s; 600–900 s is a reasonable ceiling) —
do **not** raise it to "practically forever" unless you are happy trading a
permanently hot core for never paying a cold start.

- Route selection: `--matd` / `MAT_MATD=<truthy>` **force** the matd path
  (connection failure is an error, no fallback). `MAT_MATD=<falsy>`
  (`0`/`false`/`no`/`off`) forces the direct path, no probing. Otherwise
  (default) `mat` **auto-detects**: it probes the socket with a connect and
  falls back to the direct path when nobody answers. `MAT_MATD_SOCKET` just
  selects *which* socket in every mode.
- Socket path precedence (all modes): `--matd <path>` > `MAT_MATD_SOCKET=<path>`
  > default socket (`$XDG_RUNTIME_DIR/matd.sock`, else `/tmp/matd.sock`).
- Once connected, errors are reported from the matd path as-is — `mat` never
  re-runs the command on the direct path (no double execution of writes).
  Which path ran is logged to stderr at info level (`MAT_LOG=info`).
- Supported over matd: `read` / `write` / `invoke` / `on` / `off` /
  `color-temp` / `color` / `describe` / `group` (`provision` / `invoke` /
  `color-temp` / `color`; `group grant` is always direct — see Groupcast
  above). `discover` / `commission` / `open-window` / `diag` are direct-only:
  auto-detection skips them silently; explicit `--matd` exits `2`.
  The group color shortcuts (`group color-temp` and `group color`) require
  matd ≥ 0.14; an older matd rejects the unknown op with a `parse_error`
  (exit 1) and `mat` does not fall back to the direct path (no double execution).
  `group provision --rebind` through matd needs matd ≥ 0.15: an older matd
  silently ignores the unknown `rebind` field and fails at `bind-keyset`
  (`Duplicate key id`) — same as running provision without the flag.
- node_id commissioning is re-checked by `matd` against the same credential store
  per request, so the error kinds and exit codes match the direct path.

### matd's native backend (Phase 5 M4)

Set `MAT_MATD_IFACE=<thread mesh iface>` (or pass `matd --iface <name>`) to let
`matd` handle its hotpath ops — `on` / `off` / `color`
(`move-to-hue-and-saturation`) / `color-temp` (`move-to-color-temperature`) /
`read`ing the `onoff` cluster's `on-off` attribute — in-process, over a warm
CASE session held by the embedded Matter controller (crate `mat-controller`,
Phase 5's from-scratch native backend), instead of going through the
`chip-tool interactive server`. Leave it unset and nothing changes: every op
still goes through chip-tool, exactly as before.

```bash
matd --iface thread0 &
# or: MAT_MATD_IFACE=thread0 matd &
```

- Native sessions are held per-node indefinitely once established (unlike
  `chip-tool`, no busy-loop cost — a few KB per node). A send that exhausts
  MRP retransmission (timeout) discards the session and does one automatic
  mDNS re-resolve + re-CASE before failing.
- If the fabric table index isn't `1` (e.g. sharing a fabric with another
  admin), pass `--fabric-index <n>` (env `MAT_MATD_FABRIC_INDEX`). The CA
  issuer index defaults to `0`; override with `--issuer-index` (env
  `MAT_MATD_ISSUER_INDEX`).
- If native construction fails (e.g. the credential store can't be read),
  `matd` logs a warning and falls back to chip-tool for everything — it does
  not refuse to start.
- Everything outside the hotpath — `write`, `describe`, arbitrary
  cluster `read`/`invoke`, and `group provision` / generic `group invoke`
  (non-onoff, or onoff with arguments) — still goes through chip-tool.
  Because of this, `chip-tool` is only spawned lazily on the first op that
  needs it: a workload that only ever hits the native hotpath never spawns
  `chip-tool` at all, and `matd` can start even if `chip-tool` isn't
  installed.
- Error kinds and exit codes are identical regardless of path: mDNS
  resolution failure -> `unreachable`, CASE failure -> `session_failed`, MRP
  exhaustion -> `timeout`, IM status rejection -> `device_rejected`.
- Real-device acceptance harness: `task e2e:m4` (needs `MAT_E2E_HOST` /
  `MAT_E2E_NODE_ID` / `MAT_E2E_IFACE`; designed to run against a separate
  socket/port so it doesn't disturb a production `matd`).

### matd's native groupcast (Phase 5 M5)

With native enabled (`MAT_MATD_IFACE` set and the native backend built
successfully), `matd` also handles its three group-send ops in-process
instead of going through chip-tool: `group invoke` for onoff `on`/`off`/
`toggle` with no extra arguments, `group color`, and `group color-temp`.
`group provision` and any other `group invoke` (non-onoff cluster, or onoff
with arguments) are unaffected — they always go through chip-tool.

- Each send re-reads the group's operational credentials from the chip-tool
  KVS (derived GroupKeyMap -> keyset GKH + operational key), so a
  `group provision --rebind` takes effect on the very next native send with
  no matd restart needed.
- The send is a single unacknowledged AES-CCM-sealed packet to the
  site-local transient multicast address (`ff35::.../64` scope, hop limit
  64) — same wire semantics as chip-tool's groupcast (no response, no MRP).
- The send counter is persisted at `<store>/native_group_counter` (plain
  decimal text, written ahead by 4096 so a crash never reuses a counter
  value). On first use it jumps to `max(own file, chip-tool's `g/gdc`) +
  4096`, because native shares the same source node id as chip-tool and
  therefore the same per-sender counter window on the receiving devices.
- If native can't send (group not provisioned, KVS incomplete, `g/gdc`
  missing, etc.), `matd` logs a warning and falls back to chip-tool for that
  op — it does not fail the request.
- The response JSON is identical on both paths (same `group_sent_body`
  schema as the direct/chip-tool group commands above).
- With native disabled (`MAT_MATD_IFACE` unset, or native construction
  failed), all group ops go through chip-tool exactly as before — no
  behavior change.
- **Recommendation: pick one group sender.** Once native groupcast is
  active, send all groupcasts through native `matd` and stop sending them
  through the direct `mat` (chip-tool) path. Native's counter jump-ahead at
  startup only protects the *first* send after enabling native — it reads
  chip-tool's `g/gdc` once and then advances independently. If you go back
  to sending via direct chip-tool *after* native has been sending for a
  while, chip-tool's counter is now behind native's and devices will drop
  its groupcasts as stale/duplicate (the same counter-collision failure
  mode as mixing two chip-tool senders — see `groupcast-e2e-findings` in
  project notes). Real-device acceptance harness: `task e2e:m5` (needs
  `MAT_E2E_HOST` / `MAT_E2E_IFACE` / `MAT_E2E_GROUP_NODES`; stop production
  matd's group traffic for the duration — unicast ops are fine to leave
  running).

## Credential store

Resolution order: `--store <path>` > `$MAT_STORE` > `$XDG_CONFIG_HOME/mat` >
`~/.config/mat`. It holds the Root CA, the controller's keys/cert, the
commissioned-node ledger (`nodes.json`), the optional alias map
(`aliases.toml`, below), and `chip-tool`'s persistent storage.
**It is never committed** (excluded by `.gitignore`).

## Aliases (`aliases.toml`, optional)

Numeric node / group / endpoint ids are easy to get wrong. If the file
`<store>/aliases.toml` exists, `mat` resolves locally defined names to those
numbers right after arg parsing — a purely local convenience. **Without the
file, behavior is exactly the traditional numeric-only one.** The wire and the
backend (`chip-tool` / `matd`) always receive numbers, and stdout keeps the
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
```

- `nodes`: alias → node_id. Accepted by `-n/--node` (read / write / invoke /
  describe / on / off / color-temp / color / open-window / diag thread / diag node) and
  by `--nodes` in `group provision` (each element resolved independently).
- `groups`: alias → GroupId. Accepted by `-g/--group` in every `group`
  subcommand (`provision` / `invoke` / `grant` / `color-temp` / `color`).
- `endpoints`: defined **per node** — the outer key is a node alias or a
  node_id digit string, the inner map is alias → endpoint number (endpoint
  numbers mean different things on different nodes, so there is no global
  endpoint dictionary). Accepted by `-e/--endpoint` on node-taking commands;
  the lookup uses the *resolved* node, so `-n 5 -e main` and
  `-n living-light -e main` give the same result. The `-e` of `group
  provision` / `group invoke` / `group color-temp` / `group color` is
  **numeric only** (no node context to resolve against).
- `colors`: custom color name → RGB value (`#rrggbb` / `rrggbb` / `R,G,B`),
  used by `--name` in `color` / `group color`. Entries are defined as RGB and
  go through the same RGB → HSV pipeline as `--rgb`. A user-defined name
  **overrides** the built-in color table (you can redefine `red`). Without the
  file the built-in table still works. A value that does not parse as RGB is
  `store_parse` (exit `10`); an unknown color name is a CLI argument error
  (exit `2`) listing the known names.

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
already taken → exit `2`, before chip-tool is even launched), and it is written
to `aliases.toml` only on success (the file is created if absent). Without
`--alias`, `commission` never touches `aliases.toml`. Deleting or renaming an
alias is a hand edit of the file — there is no management subcommand.

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
| 6 | CASE session establishment failed |
| 1 | other |

`chip-tool` has coarse exit codes (mostly `1` on failure). `mat` parses
stdout/stderr to classify into `3` / `4` / `5` / `6`. If it cannot classify, exit `1`.

`kind` values (stable; callers may branch on these strings):

- `store_missing` / `store_parse` — credential store missing / corrupt (exit 10)
- `node_not_commissioned` — node_id not in the store (exit 11)
- `child_not_found` — `chip-tool` binary not found / not runnable (exit 12)
- `timeout` (exit 3) / `device_rejected` (exit 4) / `unreachable` (exit 5) —
  classified from chip-tool output
- `session_failed` — IP reachable but CASE (operational secure session) could not
  be established, e.g. an intermittent `CHIP Error 0x54 (Invalid CASE parameter)`
  during the Sigma exchange (exit 6). Distinct from `unreachable` (no IP route)
  and `device_rejected` (the device answered and refused); typically retryable.
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

CI (GitHub Actions, `.github/workflows/ci.yml`) runs the same fmt / clippy /
test sequence as `task check` and does not need a real `chip-tool`. The tests
use `crates/mat/tests/fixtures/fake-chip-tool.sh` (a stub that prints fixed
text) via `MAT_CHIP_TOOL_BIN`, and `matd`'s tests use a fake websocket backend.

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
> Thread**; Wi-Fi / Ethernet lights are more reliable. The exact `key-set-write`
> JSON, the `groupsettings add-keysets` policy value, and the group node-id form
> are chip-tool-version dependent — if a chip-tool upgrade breaks provisioning,
> this is the first place to check.
>
> If **no** device reacts although provision reported success, suspect the
> device ACL first: provisions made before the ACL step (or through an old
> `matd` ≤ 0.12) never granted the group permission, and devices silently drop
> unauthorized groupcast. `mat group grant --group 1 --nodes 5 6 7` adds the
> missing entries idempotently.

## Contributing

Issues and pull requests are welcome. Before sending a PR, run `task check`
(format check + clippy with `-D warnings` + tests); it needs no real `chip-tool`.
Please keep stdout pure JSON and follow the design rules in
[ARCHITECTURE.md](./ARCHITECTURE.md).

## License

[MIT](./LICENSE).
