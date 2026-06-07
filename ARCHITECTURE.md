# Architecture and Design

This document explains why `mat` is built the way it is. For how to use it, see
[README.md](./README.md). For the rules an AI agent must follow when working in
this repo, see [CLAUDE.md](./CLAUDE.md).

`mat` is a CLI for controlling Matter devices. It calls a Matter controller
(`chip-tool` for now) as a subprocess, and turns its long text output into one
clean JSON object per command.

`mat` is AI-native and UNIX-friendly:
- stdout is pure structured JSON (one command = one JSON object).
- diagnostics and machine-readable errors go to stderr.
- the exit code lets the caller branch on the result.

---

## What `mat` does and does not do

`mat` sits in a three-layer system. It is the protocol-specific CLI for Matter.
A cross-protocol client (`casa`) can dispatch to it when `protocol = "matter"`,
the same way it dispatches to `enl` for ECHONET Lite.

### `mat` is responsible for
- A consistent wrapper UX over a Matter controller (`chip-tool`).
- Turning the controller's verbose text into `mat`'s JSON schema.
- Managing fabric credentials (Root CA, our own NOC, commissioned nodes) in a
  local key-value store (KVS).
- Commissioning: joining a fabric and sharing devices with other admins.

### `mat` is NOT responsible for
- **Resolving human names to (node_id, endpoint, cluster).** That belongs to the
  upper layer. `mat` takes a numeric `node_id`.
- **Scheduling, daemons, or holding state** (except the credential KVS, below).
- **Session cache, subscriptions, freshness.** All of this belongs to a resident
  layer (`casad`).
- **Logical groups** ("the 7 lights in the living room"). That naming concern
  belongs to the upper layer. See "Two kinds of groups" below.
- **Being a Matter device (a bridge).** `mat` only controls Matter devices. Re-
  publishing non-Matter devices (ECHONET, etc.) as Matter devices for Alexa /
  Apple / Google is a separate kind of program that *becomes* a Matter device.
  That belongs in a separate project, not here. Mixing controller and device
  turns the tool into a home automation hub, which is not the goal.
- **Scenes, automation, and voice/UI entry points.** "Set many devices to this
  state" logic, and the triggers for it, belong to the resident layer. `mat`
  fires one shot at one device.

---

## Why `mat` is stateful (unlike `enl`)

A pure one-shot model works for ECHONET Lite because it is connectionless UDP,
has no auth, and each command is independent. Matter is none of these.

To read / write / invoke, you need: (1) to be a fabric member (Root CA + your own
NOC), (2) a CASE session with the device (a Sigma handshake), and (3) to have
commissioned the device once and kept its credentials. So **some persistent
state must exist somewhere.** A pure stateless design is impossible.

### The answer: one-shot interface + persistent credentials
- **The process is one-shot.** `mat read` / `mat write` finish in one call and
  exit.
- **Only credentials live on disk.** This is the same UNIX model as `git`
  (depends on `.git`) or `ssh` (depends on `~/.ssh`) while each command is
  one-shot.
- **The slowness is absorbed by the upper layer.** Each one-shot pays mDNS
  resolution plus a CASE handshake, so a single call is slow (hundreds of ms to
  seconds). Use cases that need speed are handled by a resident layer that keeps
  warm sessions. `mat` itself is allowed to be slow. Do not break this line.

---

## Matter and Thread

"Operate Matter/Thread" mixes two layers. Thread is an IPv6 mesh (802.15.4) at
the **network layer**. Matter is the **application layer** on top. You always
talk to a device with Matter.

- A Thread device and a Wi-Fi/Ethernet Matter device look the **same** to the
  controller once the Thread device is on IPv6 through a Thread Border Router.
- So there is only one tool: `mat`, a Matter controller CLI. **Thread is
  transparent** and does not appear in `mat`'s command set.
- Managing the Thread network itself (Border Router dataset, etc.) is **out of
  scope**. Leave it to the OS / Border Router.

---

## Fabric ownership (multi-admin)

`mat` **owns its own fabric**. It acts as one more admin, next to other admins
like Home Assistant or Apple Home. A Matter device can belong to many fabrics at
once (multi-admin), so `mat` can run alongside them.

### Two ways to commission (both use `chip-tool pairing code`)
A Matter setup code (QR or 11-digit) can be passed the same way to
`chip-tool pairing code <node-id> <code>`. Only the source differs.

1. **First commission** (a factory-reset device): use the printed setup code.
2. **Multi-admin join** (a device already commissioned by another admin): the
   printed code does not work (the device left commissioning mode). The existing
   admin opens a commissioning window, which gives a one-time code, and `mat`
   joins with it.

In practice (2) is the common path: a device often already belongs to another
admin's fabric, so the daily flow is "open share on the other admin -> join with
the issued code."

### Sharing your own devices (`mat open-window`)
To share a device that `mat` owns with another controller (Alexa / Apple /
Google), `mat` can open a commissioning window (a wrap of
`chip-tool pairing open-commissioning-window`).

- The JSON output includes **both** `manual_code` (11-digit) and `qr_payload`
  (the `MT:...` string): `{ "node_id", "manual_code", "qr_payload", "expires_at" }`.
- **Rendering the QR image is not `mat`'s job.** stdout stays pure JSON with the
  `qr_payload` string only. Drawing the QR is the upper layer's job. Do not mix
  human decoration into stdout.
- **"Share many devices in one QR" is not possible in Matter** (multi-admin is
  one commission per device). One QR showing many devices always means a
  **bridge** (one Matter node that fronts many), which is a separate project, not
  `mat`. `mat open-window` shares native Matter devices one at a time.

### Notes
- **Fabric count limit.** A device advertises how many fabrics it supports in the
  Operational Credentials cluster. A cheap node may support only ~5, so several
  admins plus `mat` can use up the slots.
- **Bridge vs native.** If a hub exposes Zigbee sensors as a Matter bridge, `mat`
  does multi-admin with **the one hub**, and the sensors appear as bridged
  endpoints. For native Matter-over-Thread, each device is commissioned
  individually.

---

## Backend: `chip-tool`

The rule is "if an official CLI exists for a protocol, use it; do not write your
own." `chip-tool` is CSA's official reference implementation.

### Why `chip-tool`
1. **Groupcast is effectively only possible with `chip-tool` today.** It has the
   full path for Group Key Management and group commands.
2. **Highest spec completeness.** New clusters and features land here first, so
   even niche devices are likely to work.
3. **Easy to debug.** Matter forums, issues, and official docs are all written in
   `chip-tool` commands. Sharing the backend means you do not get lost figuring
   out whether the fault is yours or the device's.
4. **Fits the subprocess model.** Launch a native binary and exit, the same shape
   as the sibling CLIs.

### The one remaining cost: fragile output parsing
`chip-tool` has log-style text output, which `mat` must turn into JSON. A version
change can break the parser.

- The `Data = ...` form for read/write/invoke is fairly regular. Pin it with
  tests.
- **`chip-tool` exit codes are coarse** (mostly `1` on failure; details are in the
  log). So `mat` parses stdout/stderr to classify the failure kind (timeout /
  unreachable / rejected) and maps it to `mat`'s own exit code / error kind.
- Keep parser tests so an upstream update that breaks parsing is noticed.

### The backend is replaceable (adapter boundary)
`mat` couples to the backend through **only `mat`'s own JSON schema** (the same
way `casa` couples to `enl` through stdout JSON only).

- **Future candidates:** if parsing becomes too painful, a thin JS shim based on
  matter.js (structured objects from the start, no C++ build, lightweight); or, to
  stay pure Rust, a Rust-based controller prototype.
- A replacement must be one adapter in the child-runner, with `mat`'s JSON schema
  as the contract. Subcommands and output schema do not change.

---

## Design rules (must follow)

1. **Do not speak the protocol directly.** No building TLV, no opening CASE
   yourself, no multicast routing. Delegate everything to `chip-tool`. If you
   want to bring this in, that is a backend-replacement discussion, not a change
   to `mat` itself.
2. **stdout is pure structured JSON only.** Parse `chip-tool` output and re-emit
   it in `mat`'s schema. No human decoration (color, progress, interactive
   prompts).
3. **Diagnostics go to stderr as structured logs** (`tracing`). Do not swallow
   `chip-tool`'s stderr; keep it at least at debug level.
4. **Hold no state except the credential KVS.** No session-cache DB, no daemon, no
   internal scheduler.

---

## Credential store (KVS)

### Location and ownership
- Default path: `$XDG_CONFIG_HOME/mat/` (default `~/.config/mat/`). It holds the
  Root CA, the controller's keys/cert, commissioned nodes' ledger, and
  `chip-tool`'s persistent storage.
- Override with `--store <path>` or the `MAT_STORE` env var.
- **Credentials are never committed** (the repo is public). `.gitignore` excludes
  them.

### Samples and tests in the repo
- Samples use **dummy values only** (e.g. RFC 5737 `192.0.2.0/24`). Never commit
  real IPs, real node_ids, or real certificates.

---

## Three-layer separation

```
Web page / LLM / other client
       |
       v
   casad  (resident, holds state; separate repo)
       |   warm CASE sessions / cache / subscriptions / freshness
       |   spawns processes (calls mat / enl as CLIs)
       v
   casa   (resolves name -> node_id, etc.; stateless)
       |   Command::new("mat") / "enl" / ...
       v
   mat    (one-shot; only the credential KVS is persistent)
       |   Command::new("chip-tool")
       v
   chip-tool ── real Matter devices (Thread / Wi-Fi / Ethernet)
```

`casa` and `casad` are separate projects. `mat` works on its own as a standalone
Matter controller CLI; the layers above are optional.

### Two kinds of groups
There are two "groups." Do not confuse them or define them twice.

- **Logical group** ("the 7 lights in the living room") = a naming concern. The
  upper layer owns it.
- **Matter wire group** (a GroupId + Key Set burned into each device, a multicast
  address) = an on-wire protocol operation. `mat` owns it
  (`mat group provision` / `mat group invoke`).

The upper layer resolves a logical group and calls `mat`'s wire-group operations
to realize it. `mat` holds no human-facing group names.

---

## Roadmap

Phases go **in order**. Do not start the next phase until the current one is fully
done (all tests pass, acceptance criteria met).

### Phase 0 — scaffold + chip-tool wrapper + commission + KVS  *(done)*
Build a fabric, commission a device, persist its credentials, and discover nodes.
- Cargo project with `clap` (derive), `serde`, `serde_json`, `tracing`,
  `tracing-subscriber`.
- Child-runner module: spawn `chip-tool`, capture stdout/stderr, parse to JSON or
  return an error. `chip-tool` is found on PATH; override the full path with
  `MAT_CHIP_TOOL_BIN`.
- Credential store (`--store` / `MAT_STORE` / default `~/.config/mat/`) with Root
  CA / controller cert bootstrap.
- PAA trust store for attestation of production devices, resolved from
  `MAT_PAA_TRUST_STORE` or `<store>/paa-trust-store/` and passed to `chip-tool`
  as `--paa-trust-store-path` (no built-in certs; the operator supplies them).
- `mat discover`, `mat commission` (first commission and join).
- A multi-stage Docker build that bakes `chip-tool` once and ships only the binary
  in the runtime image.

### Phase 1 — read / write / invoke + describe + on/off  *(done)*
Operate a node daily by node_id (not by name).
- `mat read` / `mat write` / `mat invoke`, normalized to `mat`'s schema with an
  ISO 8601 `timestamp`.
- `mat describe` (introspection).
- `mat on` / `mat off` (mapped to the OnOff **invoke**, not write).
- Classify `chip-tool` failure as `timeout` / `unreachable` / `device_rejected`
  and map to exit `3` / `5` / `4`.

### Phase 2 — multi-admin share (open-window)  *(done)*
Share a `mat`-owned device with another controller.
- `mat open-window` (wraps `chip-tool pairing open-commissioning-window`), returns
  the issued code as JSON.

### Phase 3 — groupcast  *(hardest, needs real-device testing)*
Synchronized ON/OFF of many lights via a Matter wire group. This is the original
motivation (the "popcorn effect" of lights turning on one by one), but it is the
most fragile, so it comes last.
- `mat group provision` (KeySetWrite / GroupKeyMap / AddGroup on every node).
- `mat group invoke` (one multicast send).
- The return value only reports "sent" (unacknowledged, so no per-device success).

> **Groupcast constraints (build them into the design):**
> - **Unacknowledged:** a fire-and-forget multicast. No per-node result returns.
>   `mat group invoke` can only report "sent," not "all 7 turned on." This
>   conflicts with the AI-native ideas of self-describing errors and
>   read-after-write checks; make that clear to the caller.
> - **Especially unstable on Thread:** multicast retransmits eat airtime, and IPv6
>   multicast packet drops lower delivery. "Full sync" depends on the transport
>   and is weak on Thread lights. Wi-Fi/Ethernet Matter lights are better.
> - **Heavy pre-provisioning:** KeySetWrite / GroupKeyMap / AddGroup on every node.
>   This is the most breakable feature in Matter.

### Phase 4 — native / backend replacement  *(optional)*
Only if `chip-tool` parsing or build/ship becomes a bottleneck.
- First candidate: a matter.js shim (structured output, lightweight, no C++ build).
- Second candidate: a Rust-based controller (Rust purity, but a prototype;
  groupcast etc. would need our own work).
- The replacement must be one adapter in the child-runner, with `mat`'s JSON
  schema as the contract. Subcommands and output schema do not change.

---

## Things we never do

- Implement TLV / CASE / multicast routing inside `mat` (always delegate to
  `chip-tool`).
- Hold human names or logical groups in `mat` (upper layer's job).
- Add session cache, subscriptions, a daemon, or an internal scheduler
  (resident layer's job).
- Bring a Matter bridge (becoming a Matter device) into `mat`.
- Hold scenes, automation, or voice/UI entry points in `mat`.
- Render or display QR images on stdout (emit the `qr_payload` string only).
- Commit credentials, real topology, or real certificates to the repo.
