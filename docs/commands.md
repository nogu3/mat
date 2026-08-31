# Commands


### Discover and commissioning

```bash
# Discover commissionable / commissioned nodes (ledger only, fast)
mat discover

# Also probe live reachability of commissioned nodes via mDNS
mat discover --probe

# Join a fabric (first commission OR multi-admin join, both supported)
# All values here are dummy (RFC 5737 192.0.2.0/24)
mat commission --setup-code "MT:Y.K9042C00KA0648G00" --node 5
```

#### Route selection (`--transport`)

`commission` picks how to reach the device. mDNS finding a record only proves the
record exists — a Thread device's SRP registration outlives the device's
reachability — so `auto` keeps BLE as a fallback:

| `--transport` | QR payload (`MT:`) | manual code |
|---|---|---|
| `auto` (default) | mDNS hit → on-network, then BLE if PASE times out; miss → BLE | mDNS hit → on-network; miss → `unreachable` |
| `on-network` | mDNS only; never falls back to BLE | same |
| `ble` | skips mDNS entirely | rejected (exit `2`) |

The fallback fires **only** when PASE exhausts its MRP retry budget (or the device
disappears before PASE) — i.e. before any failsafe is armed, so nothing was ever
established on the device. A failure after PASE (attestation, NOC, CASE) stops
immediately: the device holds partial state under its failsafe and must not be
re-driven automatically.

After an mDNS **hit**, BLE is added to the plan only when it can actually run — a
build with the `ble` cargo feature **and** a `--thread-dataset`. Without both,
`auto` behaves exactly like `on-network` on a hit (an `INFO` line on stderr says
so), instead of replacing the real on-network error with a BLE one. After an mDNS
**miss** BLE is always attempted, so the error names whichever piece is missing.

A manual code carries only a 4-bit short discriminator, which cannot drive the
12-bit BLE scan; use the QR payload for BLE.

```bash
# force BLE (skip mDNS entirely)
mat commission --setup-code "MT:Y.K9042C00KA0648G00" --transport ble
```

`discover` output:

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "devices": [
    { "state": "commissionable", "hostname": "B827EBA8C9F0", "addresses": ["192.0.2.10"], "port": 5540, "discriminator": 3840, "vendor_id": 65521, "product_id": 32769 },
    { "state": "commissioned", "node_id": 5, "commissioned_at": "2026-06-06T12:00:00+09:00" }
  ]
}
```

With `--probe`, each `commissioned` node is checked against a live mDNS resolve
(a native targeted `_matter._tcp` lookup per node, run concurrently) and
annotated:

- `reachable: true` — advertising now; `address` is the live-resolved value
  (absent if the instance advertises no addresses — announce-only).
- `reachable: false` — not advertising; no `address` (the ledger stores none —
  addresses are resolved live, never persisted).
- `reachable: null` — the mDNS probe could not run (e.g. an interface I/O
  error); reachability is unknown. A diagnostic is logged to stderr.

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "devices": [
    { "state": "commissioned", "node_id": 5, "address": "192.0.2.99", "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": true },
    { "state": "commissioned", "node_id": 7, "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": false },
    { "state": "commissioned", "node_id": 9, "commissioned_at": "2026-06-06T12:00:00+09:00", "reachable": null }
  ]
}
```

Without `--probe` the output is unchanged (no `reachable`); the
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
- The store is groupcast-ready from the start: the global group data counter
  (`g/gdc`) is seeded with a random spec-range value, the same discipline the
  upstream SDK uses on first boot. (A missing `g/gdc` makes `mat` refuse group
  sends rather than start the counter low.)
- **It refuses if the store already holds a KVS** (no `--force`); re-initialize
  by deleting the store's `.ini` files by hand. Any other command run before
  `fabric init` returns `store_missing` (exit 10) with a hint to run it.
- If you are joining a fabric that was created by `chip-tool` (fixed-epoch),
  you do **not** run `fabric init` — the first native `commission` verifies the
  fixed epoch against the fabric's KVS materials and adopts it (see
  [Backend](backend.md#backend), "epoch").

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
mat commission --setup-code "MT:Y.K9042C00KA0648G00" --node 5
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
arg parsing (see [Aliases](configuration.md#aliases-aliasestoml-optional)). Without that file,
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
mat diag node --node 5 --deep     # also probe native targeted mDNS + ping6 (to the live-resolved address)
```

```json
{
  "timestamp": "...", "node_id": 5, "endpoint": 0,
  "verdict": "link_starved",
  "summary": "Not advertising Matter on any fabric; weak Thread link (best LQI 3) — SRP registration likely incomplete.",
  "checks": {
    "mdns": { "advertised_self_fabric": false, "advertised_any_fabric": false },
    "operational": { "resolved": false, "kind": "timeout" },
    "thread": { "neighbor_count": 1, "best_lqi": 3, "routing_role": 2 }
  },
  "unavailable": [ { "check": "ip", "kind": "mdns_unresolved" } ],
  "recommendation": "Improve the Thread link (move the device near a router) or wait; do NOT factory reset — the fabric is intact."
}
```

> `verdict` is one of `ok`, `ip_unreachable`, `link_starved`, `fabric_missing`,
> `not_advertised`, `unresolvable`, `session_failed`, `device_rejected`,
> `unknown`. Without `--deep` the fast path can't tell `link_starved` (weak
> Thread link, SRP not registered — **the fabric is intact**) apart from
> `fabric_missing` (the device dropped our fabric); `--deep` adds the mDNS and
> ping6 evidence that distinguishes them. `--deep` resolves the node via a
> native targeted mDNS lookup first and pings the **live-resolved** address
> (the ledger stores no address — Issue #18); if the node is not advertising
> at all there is nothing to ping, the `ip` check lands in `unavailable` as
> `mdns_unresolved`, and the split then rests on the Thread-side evidence. A
> failed mDNS probe itself (e.g. an interface I/O error) also lands `ip` as
> `mdns_unresolved`; the two cases are distinguishable because the
> probe-failure case additionally reports a `{"check": "mdns", ...}` entry
> under `unavailable`.
> Like `diag thread` it returns **partial
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
      "node_id": 42, "alias": "hall_motion", "probed": true },
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

> `--nodes` takes node_ids or `aliases.toml` node aliases, one or more,
> deduped preserving first-seen order (`--nodes 7 7`, or the same node given
> twice via alias + id, probes it once and emits one graph node — `id` is the
> graph's stable key, so a duplicate target would otherwise duplicate the
> node); omitted, it means every commissioned node in the store. A node with 0
> targets (empty store) is not an error — it returns an empty graph
> (`"nodes":[]`, `"edges":[]`) plus a `network` object (all fields absent
> except an empty `partition_ids`) and a `timestamp`, without touching the
> backend.
>
> A node's stable `id` is `ext:<HEX16>` when its ExtAddress is known (either
> self-identified via cluster 0x33, for fabric nodes, or observed in a
> neighbor/route table row, for unknown participants), and `node:<node_id>`
> for a fabric node whose ExtAddress could not be determined at all (e.g.
> probe failure). Unknown participants
> get a `label` from `aliases.toml`'s `[thread]` section (see
> [Aliases](configuration.md#aliases-aliasestoml-optional)) instead of an `alias`,
> which is reserved for commissioned nodes' own node alias.
>
> A probed fabric node whose cluster 0x33 self-identification failed
> (unreadable, or a duplicate self-ext claim invalidated below) is rescued
> deterministically before falling back to `node:<id>` (issue #13). Real-device
> finding (2026-09-01): a router/leader's own route table contains a row for
> itself, shaped `NextHop` = 63 (invalid) plus `PathCost` = 0 with `Age` ≈ 0
> (OpenThread gives routers with a lost route the same 63/0 shape, so an
> aged 63/0 row is not treated as a self-row, and 63/0 rows are never
> ingested as participants or edges). Vendor-dependently the self-row carries
> either the node's real ExtAddress (used directly; the node gets
> `"identified_by": "route-table"`) or `ExtAddress: 0` with the node's own
> Rloc16. A self Rloc16 — from the self-row, or derived from cluster 0x33
> IPv6Addresses + mesh-local-prefix (this derivation is independent of
> ExtAddress canonicalization) — is then matched against every probed node's
> neighbor/route table observations (real ext + Rloc16): exactly one observed
> ext, not already claimed by another fabric node, merges the two vertices
> into one (`"identified_by": "rloc16"`). Anything ambiguous is rejected —
> several exts observed under that Rloc16, a resolution colliding with an
> already-identified ext or with another rescued node, or a mesh currently
> split into several partitions (Rloc16 uniqueness only holds within one
> partition, so the whole correlation is skipped while `partition_ids` has
> more than one entry). `mat` never merges by guesswork: neighbor-set
> similarity heuristics were evaluated on the real mesh and mis-merge dense
> routers into the border router, so they are deliberately not used. Note the
> rescue trusts the freshest tables available in one snapshot; a Rloc16
> reassigned moments before the probe while every peer's table is still stale
> can in principle mislabel a node for one run — `identified_by` marks such
> merges as evidence-based rather than 0x33-claimed, and the next run
> self-heals.
>
> A fabric node that still could not self-identify (probe failed outright, or
> every rescue path was rejected as ambiguous) shows up as a `node:<id>`
> graph node without a `rloc16` — and still contributes its own viewpoint
> edges, anchored at `node:<id>` rather than an `ext:` vertex, from its own
> neighbor/route table rows. The same physical radio may *also* surface
> separately as an `ext:` unknown-participant node if another probed node's
> neighbor/route table observed it.
>
> Real-device finding (2026-07-23): some Thread devices (ESP32-based tape
> lights) report an identical, firmware-hardcoded HardwareAddress on cluster
> 0x33 across every unit (with an empty IPv6Addresses list). Since that ext
> claim is physically impossible once two or more fabric nodes make it, `mat`
> invalidates the self-identification of *all* nodes that claimed the same
> ext — the bogus claim is discarded for *all* of them (the rescue above then
> usually recovers each node's real ext from its route-table self-row).
> Separately, some devices encode `mesh-local-prefix` as a
> length-prefixed octstr (`0x40` = 64-bit length byte + 8-byte prefix, 18 hex
> chars) rather than the bare 8-byte form (16 hex chars); `mat` normalizes both
> to the 16-hex form before deriving RLOC16. A route-table row with
> `ExtAddress: 0` that is not the self-row is treated as garbage and ignored
> (no node, no edge — REEDs list far routers this way).
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
  only)](configuration.md#subscriptions-subscriptionstoml-optional-matd-only).
- `--count` (default `1`) is how many events to receive before exiting `0`;
  `0` means no count limit — keep streaming (symmetric with `--timeout-ms 0`).
  `--timeout-ms` (default `60000`) cuts the wait short; `0` means wait
  forever. Reaching `--count` exits `0`; the timeout firing with **zero**
  events received exits `3` (with at least one event received, it still exits
  `0` — same UX as `enl listen`).
- `mat` connects to `matd`, sends the `listen` request, and prints the ack
  line followed by one JSON event per line to stdout as they arrive:
  ```json
  {"timestamp":"...","listening":true}
  {"timestamp":"2026-07-20T21:00:00+09:00","node_id":21,"endpoint":1,"cluster":"occupancysensing","attribute":"occupancy","value":1,"priming":false,"recovered":false}
  ```
  `priming: true` marks events from the initial report burst right after
  matd (re)establishes a subscription, so a consumer does not mistake
  matd-restart residual state (e.g. `occupancy` still `1` from before a
  restart) for a fresh trigger. Only **scalar** values become events —
  `list`/`struct` attributes (ACL, server-list, etc., which show up in a
  wildcard priming burst) are dropped, the same known limitation as generic
  `read` (see [Scalar-only generic write / invoke](#scalar-only-generic-write--invoke)).
- `recovered: true` marks an event `matd` reconstructed from a priming
  report: the attribute's value in the new subscription's priming burst
  differs from the last value `matd` saw, so the transition happened while
  the subscription was down. Such an event is delivered with
  `priming: false` **and** `recovered: true`, so an existing consumer trigger
  fires on it without any change. Its `timestamp` is the **receive** time,
  not the time of the actual transition (which is unknowable — somewhere in
  the blind window). Values `matd` has never seen before (first priming after
  a `matd` restart) are **not** promoted: they are plain `priming: true`
  events, so a restart never fires a consumer's automation.
- Promotion applies to **any** subscribed attribute whose value changed, not
  just the ones a consumer cares about — after a blind window a full-wildcard
  subscription can promote diagnostics counters and similar monotonic
  attributes too. Narrowing via `subscriptions.toml` or the `mat listen
  --cluster` / `--attribute` filters is the control.
- A transition that `matd`'s **own** op caused during the blind window also
  comes back as `recovered: true` (matd never observed the report either). A
  consumer whose rule is toggle-shaped should key off the value, not the
  event's arrival.
- `matd` absent, refusing the connection, or dying mid-stream is
  `matd_unavailable` (exit **13**) — see
  [Errors and exit codes](errors.md#errors-and-exit-codes). Events already printed
  before a mid-stream matd loss stay printed; the process still exits `13`
  (not `3`), even if `--count` was not reached.
- Usage form (a consumer like casa reacts per line; `mat`/`matd` never run
  automations — see [Backend](backend.md#backend) / ARCHITECTURE.md "Design rules"):
  ```bash
  mat listen --node 21 --cluster occupancysensing --count 0 --timeout-ms 0 |
  while read -r ev; do
    # inspect $ev and react, e.g. mat on / mat off
  done
  ```
  Prefer this resident stream over respawning one-shot `--count 1` calls in a
  loop: `matd` fans events out over a live broadcast, so events arriving in
  the gap between one `listen` exiting and the next attaching (e.g. the rest
  of a priming burst after its first event) are dropped, not queued.

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
- `--iteration` must be in `1000..=100000` (spec §3.9) and `--discriminator`
  within 12 bits (`0..=4095`). Out-of-range values fail fast with
  `parse_error` before any invoke is sent.
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
number; see [Aliases](configuration.md#aliases-aliasestoml-optional)).

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

### `mat group bump` — jump the group counter window (first aid)

If some devices silently drop groupcast (unicast fine, group settings
identical to a working peer), their replay window may have run ahead of the
controller's send counter. `mat group bump` jumps the counter forward by one
matd-restart-equivalent window — the same remedy a matd restart applied,
without dropping warm sessions or resident subscriptions.

```bash
mat group bump
```

```json
{"timestamp":"...","group_counter":{"from":176561405,"to":176569504}}
```

The counter is fabric-global (one series for all groups), so there is no
`--group` argument. Routed like other group ops: via matd when one is
running, else directly (a direct run while matd holds the counter lock
fails with `store_parse` — use the matd route).

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

Ask the running daemon what it is doing with `matd status` — one JSON line on
stdout with daemon basics and the per-node state of the resident subscriptions
(the same lifecycle the logs narrate: `establishing` → `established` →
`down` with backoff and the last error). Durations are all "seconds ago"
fields; `subscribed_clusters` mirrors `subscriptions.toml` (`null` = full
wildcard); `pending_op_ago_s` is non-null only while a state-changing op has
gone unanswered by the device (the op-correlation window):

```bash
matd status                           # default socket
matd status --socket /run/mat/matd.sock
```

```json
{
  "timestamp": "2026-06-03T12:34:56+09:00",
  "version": "1.12.0",
  "uptime_s": 86400,
  "native": "ready",
  "iface": "wpan0",
  "fabric_index": 1,
  "store": "/home/user/.config/mat",
  "subscribed_clusters": ["onoff", "occupancysensing"],
  "listen_clients": 1,
  "nodes": [
    {"node_id": 5, "state": "established", "for_s": 3600,
     "subscription_id": 7, "max_interval_s": 300,
     "last_device_msg_ago_s": 42, "pending_op_ago_s": null},
    {"node_id": 6, "state": "down", "for_s": 120, "attempts": 14,
     "backoff_s": 60, "last_error": {"kind": "unreachable", "detail": "..."}}
  ]
}
```

If the native backend failed to build at startup, `native` carries that error
(`{"kind": "store_missing", ...}`) and `nodes` is empty. If no daemon answers
the socket, `matd status` exits `1` with `matd not running at ...` (same
contract as `matd stop`).

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
  `invoke` / `color-temp` / `color` / `level` / `bump`; `group grant` is
  direct only — see Groupcast above). `discover` / `commission` / `fabric init` /
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
  only)](configuration.md#subscriptions-subscriptionstoml-optional-matd-only).

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

#### Op timeout budget (`--op-timeout-ms`)

The global `--op-timeout-ms` (env `MAT_OP_TIMEOUT_MS`, default `60000`, `0` =
unlimited) bounds how long a **single-node** op (`read` / `write` / `invoke` /
`on` / `off` / `color` / `color-temp` / `level` / `describe`) may run, on
either path. It is unrelated to `mat listen`'s `--timeout-ms` (that one bounds
the event stream's receive wait, not a single op).

- **matd path**: the budget rides the request as `deadline_ms` (a relative
  ms value). `matd` enforces it and returns a structured `timeout` (exit `3`)
  once the budget is spent. `mat`'s own socket read is given a backstop
  timeout of budget + 2s, so an old `matd` (which ignores `deadline_ms`) that
  is alive but stuck — hung, or just slow to answer — still surfaces as
  `timeout` instead of hanging forever; the detail notes the request may
  already have executed. A `matd` that dies mid-request closes the socket
  instead, which `mat` sees as EOF and reports as `matd_unavailable`
  (exit `13`), same as any other mid-stream matd loss.
- **Direct path**: the same budget wraps the whole op in a
  `tokio::time::timeout`; exceeding it is `timeout` (exit `3`) too, so the
  flag behaves the same regardless of which path answers.
- A request that omits `deadline_ms` (an old `mat` talking to a new `matd`)
  gets `matd`'s own default budget of 60s applied — the same number
  `--op-timeout-ms` defaults to, so old and new clients see the same
  behavior without needing to agree on anything.
- Ops outside this list (`discover` / `commission` / `fabric init` /
  `open-window` / `diag` / `group ...`) ignore `--op-timeout-ms` entirely —
  unchanged, no read timeout either.

#### Fabric index, sessions, epoch

- `MAT_FABRIC_INDEX` (default `1`) and `MAT_ISSUER_INDEX` (default `0`) select
  the KVS fabric-table and CA-issuer entries for `mat`; `matd` mirrors them as
  `MAT_MATD_FABRIC_INDEX` / `MAT_MATD_ISSUER_INDEX` (also `--fabric-index` /
  `--issuer-index`). Pass the same values to both on the same host. If you share
  a fabric with another admin the index is usually not `1`.
- **Warm sessions** (matd only) are held per node indefinitely. A send that
  exhausts MRP retransmission (timeout) discards the session and does one
  automatic mDNS re-resolve + re-CASE before failing. That one retry only
  fires when at least 10s of the op's budget remains (see [Op timeout
  budget](#op-timeout-budget---op-timeout-ms)); with less left, `matd` skips
  the re-establish and returns the timeout immediately instead of spending
  the remaining budget on a retry that cannot finish before the deadline.
  `mat`'s one-shot session can't be stale, so it never retries — a failure is
  reported as-is.
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

#### Groupcast egress (LAN + Thread TUN)

Groupcast is sent on the operational interface (the same one mDNS uses), and
— when a Thread TUN is available — **also directly on that interface** (MPL
injection, no dependency on another border router's multicast relay). The
Thread interface is picked once at startup: `--thread-iface` /
`MAT_THREAD_IFACE` (`MAT_MATD_THREAD_IFACE` for `matd`) wins; otherwise, if
exactly one multicast-capable `wpan*` interface is up it is auto-selected;
otherwise groupcast
stays LAN-only (previous behavior). An explicitly configured interface that
fails to resolve is a hard error on `mat`'s one-shot direct path; for `matd`
it starts up regardless, but every op then fails with that error — it does
not silently degrade. An auto-detected interface that fails to resolve
instead degrades to LAN-only with a warning. The same
datagram (same counter) goes out on every egress — receivers drop the
duplicate via replay protection. The result JSON reports the interfaces
actually used: `"egress": ["eth0", "wpan0"]`.

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

Running the op direct is not the same as `matd` never hearing about it: any
node-targeted op that ends up establishing a CASE session **on the direct
path** sends a single fire-and-forget `node_touched` hint to `matd` (if one
is running) right after the op closes, so a resident subscription for that
node resubscribes immediately instead of waiting on matd's 330s silence
deadline (Issue #20, 1.15.0; see ARCHITECTURE.md). An op cut short by
`--op-timeout-ms` sends the same hint when the deadline fires (Issue #22,
1.17.0): the timed-out attempt may already have established a CASE
session the device re-anchors onto, and the close itself can no longer
be sent. That covers the
always-direct ops above (`open-window`, `diag thread` / `diag node` / `diag
mesh` — once per touched node — and `group grant`), but just as much any
normally-matd-routed op (`on` / `off` / `read` / `write` / `invoke` /
`describe` / `group provision` / `group invoke` / ...) whenever it happens
to run on the direct path instead — `matd` unreachable, `MAT_MATD=0`, or
auto-detect falling back. The op itself still runs entirely on the direct
path either way — `matd` never executes it, only reacts afterward — and
`discover` / `commission` / `fabric init` send no hint at all (no CASE
session, or, for `commission`, no subscription yet to refresh).

```bash
mat --iface eth0 on --node 5
# or: MAT_IFACE=eth0 MAT_FABRIC_INDEX=2 mat group invoke --group 10 --cluster onoff --command on
matd --iface eth0 &      # or MAT_MATD_IFACE=eth0 matd &
```

