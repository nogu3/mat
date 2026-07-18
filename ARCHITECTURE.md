# Architecture and Design

This document explains why `mat` is built the way it is. For how to use it, see
[README.md](./README.md). For the rules an AI agent must follow when working in
this repo, see [CLAUDE.md](./CLAUDE.md).

`mat` is a CLI for controlling Matter devices. It drives a native,
from-scratch Rust Matter controller (crate `mat-controller`, in this
workspace) in-process and returns pure structured JSON, normalized to `mat`'s
own schema — one clean JSON object per command. (`chip-tool` was the backend
through Phase 5 M8c-2; it was fully retired in 0.22.0 / M8c-3 — see
["Backend: native"](#backend-native-chip-tool-retired-in-0220) below.)

`mat` is AI-native and UNIX-friendly:
- stdout is pure structured JSON (one command = one JSON object).
- diagnostics and machine-readable errors go to stderr.
- the exit code lets the caller branch on the result.

`mat` makes no assumptions about its caller. It is a standalone Matter controller
CLI: a shell, a script, or a higher-level program can drive it. Whatever sits
above it is not `mat`'s concern.

---

## What `mat` does and does not do

### `mat` is responsible for
- A consistent UX over a native Matter controller (`mat-controller`, driven
  in-process — no external controller subprocess).
- Emitting the result in `mat`'s own JSON schema.
- Managing fabric credentials (Root CA, our own NOC, commissioned nodes) in a
  local key-value store (KVS).
- Commissioning: joining a fabric and sharing devices with other admins.

### `mat` is NOT responsible for
- **Resolving human names to (node_id, endpoint, cluster).** `mat` takes a
  numeric `node_id`. Mapping human-facing names is out of scope — with one
  narrow exception: if an optional `<store>/aliases.toml` exists, the CLI
  layer resolves node / group / endpoint aliases to numbers right after arg
  parsing, before dispatch. The wire and the backend (native / `matd`)
  always see numbers; a missing file is exactly the traditional behavior.
  Cluster / command / attribute names are unaffected (chip-tool notation
  only, no aliasing).
- **Scheduling, daemons, or holding state** (except the credential KVS, below).
- **Session cache, subscriptions, freshness.** `mat` is one-shot and caches
  nothing. Keeping sessions warm is the job of the resident binary `matd` (a
  separate binary in this repo, see below), not `mat`.
- **Logical groups** ("the 7 lights in the living room"). That naming concern is
  out of scope. See "Two kinds of groups" below.
- **Being a Matter device (a bridge).** `mat` only controls Matter devices. Re-
  publishing non-Matter devices as Matter devices for Alexa / Apple / Google is a
  separate kind of program that *becomes* a Matter device. That belongs in a
  separate project, not here. Mixing controller and device turns the tool into a
  home automation hub, which is not the goal.
- **Scenes, automation, and voice/UI entry points.** "Set many devices to this
  state" logic, and the triggers for it, are out of scope. `mat` fires one shot
  at one device.

---

## Why `mat` is stateful

A pure one-shot, connectionless model (a single fire-and-forget UDP datagram per
command, no auth, each command independent) is not possible for Matter.

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
- **A single call is allowed to be slow.** Each one-shot pays mDNS resolution
  plus a CASE handshake (hundreds of ms to seconds). `mat` does not cache this
  away (design rule 4). Use cases that need speed run the resident binary `matd`,
  which keeps warm sessions. `mat` itself stays one-shot. Do not break this line.

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

### Thread diagnostics (`mat diag thread`)

The one place Thread shows up in the command set is read-only diagnostics:
`mat diag thread <node_id>` reads the Thread Network Diagnostics cluster
(0x0035) from one node several times and returns a single snapshot (role,
routing, neighbor/route tables, counters). This stays inside the rules above —
it is plain Matter attribute reads against one device, not Thread network
management, and there is no Border Router access. It is one-shot like every
other `mat` command and runs only on the native direct path (not via
`--matd`).

---

## Fabric ownership (multi-admin)

`mat` **owns its own fabric**. It acts as one more admin, next to other admins
like Home Assistant or Apple Home. A Matter device can belong to many fabrics at
once (multi-admin), so `mat` can run alongside them.

### Two ways to commission (native, `mat commission`)
A Matter setup code (QR or 11-digit) is passed the same way to
`mat commission --target <host-or-ip> --setup-code <code>` (native PASE;
`mat-native::commission` auto-selects on-network mDNS vs. BLE+Thread — see
["Backend: native"](#backend-native-chip-tool-retired-in-0220)). Only the
source of the code differs.

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
Google), `mat` can open a commissioning window (native `OpenCommissioningWindow`
over the existing operational session).

- The JSON output includes **both** `manual_code` (11-digit) and `qr_payload`
  (the `MT:...` string): `{ "node_id", "manual_code", "qr_payload", "expires_at" }`.
- **Rendering the QR image is not `mat`'s job.** stdout stays pure JSON with the
  `qr_payload` string only. Drawing the QR is out of scope. Do not mix human
  decoration into stdout.
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

## Backend: native (chip-tool retired in 0.22.0)

`mat`'s backend is a **native, from-scratch Rust Matter controller** (crate
`mat-controller`, driven through the shared `mat-native` engine) — TLV, CASE,
IM, groupcast, mDNS, and commissioning (on-network + BLE+Thread) all run
in-process. There is no `chip-tool` (or any other external controller)
subprocess.

- **Route selection is per-op:** matd auto-discovery (if a `matd` answers the
  probed socket) -> `mat`'s own native direct path. See README's
  [Routing through `matd`](./README.md#routing-through-matd) and
  [Native backend internals](./README.md#native-backend-internals) for
  interface autodetect (`MAT_IFACE` / `MAT_MATD_IFACE`), fabric index, warm
  vs. one-shot sessions, the shared groupcast counter, epoch adoption, and the
  scalar-only generic write/invoke rule.
- **First-fabric bootstrap** is `mat fabric init` (random-epoch IPK). A fabric
  first created by `chip-tool` is handled by verifying its fixed epoch
  against the KVS materials and adopting it (persisted to
  `mat/f/<idx>/ipk-epoch`), so pre-M8c-3 fabrics keep working.
- `mat` is the sole owner/writer of the persistent Matter KVS (chip-tool INI
  compatible form: keysets, operational credentials, group tables, the
  group-send counter) — see [Credential store](#credential-store-kvs) below.

### History: why `chip-tool` first, and why it was retired
Through Phase 5 M8c-2, `mat` drove CSA's reference `chip-tool` binary as a
subprocess and turned its log-style text output into JSON. That was the
pragmatic starting point: `chip-tool` had the fullest spec coverage (including
groupcast, which had no other easy path), was easy to debug against (Matter
forums and docs are all written in `chip-tool` commands), and fit a simple
subprocess-and-exit model. The recurring cost was a fragile text parser (the
`Data = ...` convention, coarse exit codes needing stdout/stderr
reclassification) that had to be re-pinned against every `chip-tool` version.
Phase 5 (decided 2026-07-10) replaced this piece by piece with a from-scratch
Rust controller library, crate `mat-controller`, until milestone M8c-3
(0.22.0) retired `chip-tool` entirely and deleted its text parsers — see the
Phase 5 record below for the milestone-by-milestone history.

### The backend is replaceable (adapter boundary)
`mat` couples to the backend through **only `mat`'s own JSON schema**.
Subcommands and output schema do not change; a future replacement is still one
adapter behind that schema (see "Phase 5" below for the current native
backend's decision record and milestones).

---

## Design rules (must follow)

1. **Protocol code lives only in the backend crates.** TLV, CASE, session
   crypto, multicast routing — all of it belongs to `mat-controller` /
   `mat-native` (Phase 5) and nowhere else. The `mat` CLI and `matd` command
   layers never speak the protocol. As of M8c-3 the native backend is the
   only path (`chip-tool` is retired).
2. **stdout is pure structured JSON only.** Emit the result in `mat`'s schema.
   No human decoration (color, progress, interactive prompts).
3. **Diagnostics go to stderr as structured logs** (`tracing`).
4. **Hold no state except the credential KVS.** No session-cache DB, no daemon, no
   internal scheduler.

---

## Credential store (KVS)

### Location and ownership
- Default path: `$XDG_CONFIG_HOME/mat/` (default `~/.config/mat/`). It holds the
  Root CA, the controller's keys/cert, commissioned nodes' ledger, and the
  Matter KVS (keysets, operational credentials, group tables, the group-send
  counter) in `chip-tool`-compatible INI form — `mat` is the sole owner/writer
  since M8c-3.
- Override with `--store <path>` or the `MAT_STORE` env var.
- **Credentials are never committed** (the repo is public). `.gitignore` excludes
  them.

### Samples and tests in the repo
- Samples use **dummy values only** (e.g. RFC 5737 `192.0.2.0/24`). Never commit
  real IPs, real node_ids, or real certificates.

---

## `mat` and `matd`

This repo ships two binaries from one install:

```
        a shell, a script, or any higher-level caller
                          |
          +---------------+----------------+
          v                                v
   mat  (one-shot; credential KVS only)   matd (resident; Phase 4)
       \   mat-native::Engine (in-process)  \  warm mat-native::Engine(s) / unix socket
        \                                     \
         +------------------+------------------+
                            v
              mat-controller (TLV / CASE / IM / mDNS / commissioning,
                              in-process — no subprocess)
                            v
        real Matter devices (Thread / Wi-Fi / Ethernet)
```

- **`mat`** is the one-shot CLI. It builds a native `mat-native::Engine`
  in-process, runs one command, and exits. Design rule 4 (no daemon / cache
  inside `mat`) always holds.
- **`matd`** is the resident binary (Phase 4). It holds a warm native `Engine`
  per node (CASE/Sigma sessions) so repeated Matter calls skip the handshake —
  the same model as ssh `ControlMaster`/`ControlPersist`. `matd` is allowed to
  be resident precisely because it is a **separate binary and layer**, not
  `mat`. `mat` **auto-detects** a running `matd` by default (a connect probe on
  the default socket, falling back to its own native direct path when nothing
  answers); `--matd`/`MAT_MATD=1` force the matd path, `MAT_MATD=0` disables
  probing entirely.

Both binaries share a library crate `mat-core` (the `parse` / `output` /
`error` / `group` / `acl` modules: shared value normalization, the JSON
schema, exit-code classification, group key logic) and the shared engine
crate `mat-native` (built on the protocol library `mat-controller`), so the
fragile parts are maintained once.

### Two kinds of groups
There are two "groups." Do not confuse them or define them twice.

- **Logical group** ("the 7 lights in the living room") = a naming concern. Out
  of `mat`'s scope; `mat` holds no human-facing group names.
- **Matter wire group** (a GroupId + Key Set burned into each device, a multicast
  address) = an on-wire protocol operation. `mat` owns it
  (`mat group provision` / `mat group invoke`).

A caller resolves a logical group to a numeric GroupId and calls `mat`'s
wire-group operations to realize it.

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

### Phase 3 — groupcast  *(done)*
Synchronized ON/OFF of many lights via a Matter wire group. This is the original
motivation (lights turning on one by one instead of together), but it is the most
fragile, so it comes last.
- `mat group provision` (KeySetWrite / GroupKeyMap / AddGroup / ACL
  read-merge-write on every node).
- `mat group grant` (repair: just the ACL step, for groups provisioned before
  the ACL step existed; direct chip-tool only).
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
> - **Heavy pre-provisioning:** KeySetWrite / GroupKeyMap / AddGroup / ACL write
>   on every node. This is the most breakable feature in Matter.

### Phase 4 — `matd`, the resident binary for Matter  *(done)*
Make repeated operations fast without breaking `mat`'s one-shot model. Each `mat`
call pays mDNS resolution plus a CASE (Sigma) handshake, so a single call is slow
(hundreds of ms to seconds). That latency is inherent to a stateless CLI and is
**not** cached inside `mat` (design rule 4). Instead, a separate resident binary
keeps the sessions warm.
- **Cargo workspace.** Split a shared library `mat-core` (the `parse` / `output`
  / `error` modules: chip-tool parsing, the JSON schema, exit-code
  classification) so the fragile `Data = ...` parser is maintained once.
- **`mat`** — the one-shot CLI, unchanged behavior; depends on `mat-core`.
- **`matd`** — drives `chip-tool` in interactive mode and holds warm CASE
  sessions behind a local unix socket (ssh `ControlMaster`/`ControlPersist`
  model). It is the resident variant of `mat`'s own layer (Matter-only).
- Both binaries ship from this repo, so one install provides both.

**Backend driver: `chip-tool interactive server` (websocket).** `chip-tool` ships
two long-lived modes: `interactive start` (a human shell over stdin) and
`interactive server` (a websocket another process drives). `matd` uses the
**server** mode — it is the path the Matter SDK's own test harness drives, returns
structured responses, and avoids re-parsing human log output / prompt boundaries
(the fragile path CLAUDE.md warns about). `matd` spawns one `chip-tool interactive
server --port <P>`, holds a single ws connection (warm CASE sessions), and
serializes commands over it.

**Upstream socket protocol.** `matd` listens on a unix socket and speaks
newline-delimited JSON (one line = one request = one response), same "one op = one
JSON object" spirit as the `mat` CLI. A request is `{ "id"?, "op", ... }`;
`op` ∈ `read | write | invoke | on | off | color_temp | color | describe | group | ping`. The response
is a mat-schema object (with `timestamp`, echoing `id`) or
`{ "error": { "kind", "detail" } }`. node_id resolution is re-checked against the
KVS per request.

The ws result shape is `{ "results": [...], "logs": [...] }` where `results[i]`
is `{ endpointId, clusterId, attributeId, dataVersion, value }`. `matd` is built
on top of that:
- **`logs` dropped.** The backend strips the verbose base64 `logs` right after
  each ws exchange (count logged at debug) so `matd` never carries it. Responses
  are the pure `mat` schema — the raw ws result is not attached (CLAUDE.md rule 2).
- **`describe`** op: `parts-list` (ep 0) → per-endpoint `server-list`, reading the
  ID arrays from `results[0].value`. Same output shape as `mat describe`.
- **`group`** ops: `group_provision` (controller groupsettings + per-node
  KeySetWrite / GroupKeyMap / AddGroup / ACL read-merge-write) and
  `group_invoke` (multicast, reports `sent`). The shared epoch-key /
  group-node-id logic lives in `mat-core::group`, and the ACL
  interpretation/merge logic in `mat-core::acl`, so `mat` and `matd` use one
  copy. `group grant` (ACL repair) is deliberately **not** a matd op: it is a
  rare repair operation with little warm-session benefit, and keeping it
  direct-only avoids mat/matd version-skew hazards.
- **Error classification:** a device-side error in `results[i].error` is run
  through the existing `classify_failure` text matcher (→ unreachable / timeout /
  device_rejected, unknown falls back to device_rejected). The `error` value is a
  status-name **string** (e.g. `"FAILURE"`), not numeric.
- **`mat` client path — auto-detected by default:** for
  read/write/invoke/on/off/color-temp/color/describe/group, `mat` probes the default `matd`
  socket with a connect and, when something answers, routes the call through
  it (std `UnixStream`, newline JSON) instead of spawning chip-tool; on no
  answer it falls back to the direct chip-tool path. `--matd`/`MAT_MATD=1`
  force the matd path (no fallback on connection failure); `MAT_MATD=0`
  disables probing and always goes direct. discover / commission /
  open-window / diag stay direct-only (exit 2 under forced `--matd`; auto
  mode skips probing for them silently).

Inline-JSON tokenization: `group_provision`'s key-set JSON is passed inline on the
ws command line as a single compact (no-space) token, e.g. `groupkeymanagement
key-set-write {"epochKey0":..., "groupKeySetID":77} 5 0`.

> Design rule 4 (no daemon, no session cache) continues to apply to **`mat`**.
> `matd` is a separate binary and layer; it is allowed to be resident precisely
> because it is not `mat`.

### Phase 5 — native backend (from-scratch Rust controller)  *(decided 2026-07-10, in progress)*
Decision record: `docs/superpowers/specs/2026-07-10-phase5-backend-direction-design.md`.
- A from-scratch Rust Matter controller library, crate `mat-controller` in
  this workspace. First stage is operational-only (CASE initiator, IM
  read/invoke, group sessions) riding the existing fabric by reading
  chip-tool's KVS; commissioning stays on chip-tool one-shot. Second stage
  (PASE / BTP / attestation) removes chip-tool entirely.
- rust-matc (tom-code/rust-matc, BSD-2) is a reference to read, never to
  copy.
- Crate layout (as of M7): `mat` / `matd` (command layers, JSON schema,
  chip-tool child-runner adapter) sit on top of **`mat-native`** (shared
  engine crate, extracted in M7 from `matd`'s native module — the
  process-shape-independent core: KVS-backed engine construction, CASE
  establish + unicast invoke/read, group send context), which itself sits on
  **`mat-controller`** (the protocol library: TLV, CASE, session crypto,
  multicast routing, commissioning) and `mat-core` (shared types, errors,
  color, alias resolution used by every crate including chip-tool-only
  code). `mat` and `matd` differ only in what they do with `mat-native`'s
  `Engine`: `matd` holds warm per-node sessions in a `HashMap`, `mat`
  establishes → runs one op → discards (design rule 4).
- Milestones M1–M7 with independent acceptance criteria; the chip-tool
  path stays untouched until M4 swaps matd's adapter in-process.
- The replacement is still one adapter with `mat`'s JSON schema as the
  contract. Subcommands and output schema do not change.
- M1 完了(2026-07-10): TLV/メッセージ層/セッション暗号（unsecured/secured
  unicast）。ローカル + Thread 実機 Nanoleaf 2 ノードで E2E 合格。
- M2 完了: CASE + IM read/invoke（fabric/kvs/cert/case/session/im）。M2b:
  chip-tool の永続 root CA 鍵で operational identity を自己発行し、ローカル
  all-clusters-app に対し CASE + onoff toggle/read の E2E 合格（`task e2e:m2`）。
- M3 完了(2026-07-12): 相乗りの堅牢化（node/fabric id を fabric テーブルの
  NOC subject から取得 — KVS index 非依存）+ 自前 one-shot mDNS 解決
  （TXT SII→MRP 接続）+ colorcontrol。jarvis 実機 Nanoleaf に本番 fabric
  相乗り（fabric index ≠ fabric id 環境）で CASE + onoff/色変更の E2E 合格
  （`task e2e:m3`）。
- M4 完了(2026-07-12): matd のホットパス（on / off / color=move-to-hue-and-saturation
  / color-temp=move-to-color-temperature / onoff の `on-off` read）を、
  `mat-controller` の in-process warm CASE セッションで処理する経路に差し替え。
  有効化は `MAT_MATD_IFACE=<Thread mesh iface>` env（または `matd --iface <name>`）。
  未指定なら従来どおり全 op が chip-tool interactive server 経由（安全な既定挙動）。
  native 構築に失敗した場合（KVS 読み取り不可等）も warn ログの上で chip-tool に
  フォールバックし、matd は落ちない。write / describe / 任意 cluster の read・invoke
  / group 系は引き続き chip-tool 経由（group 送信 3 op は M5 で native 化済み — 次項）。実機 E2E 合格
  （`task e2e:m4`、本番 matd を止めず別 socket/port で検証）: ホットパス往復 + warm
  再利用（cold 1.16s → warm 120ms、mDNS+CASE を払わない）+ describe の chip-tool
  フォールバック（lazy spawn）を実機で確認。
- M5 完了(2026-07-12): matd の group 送信 3 op（`GroupInvoke` の onoff 引数なし
  on/off/toggle、`GroupColor`、`GroupColorTemp`）を native groupcast 化。鍵は
  chip-tool KVS の導出済み operational credentials（GroupKeyMap
  `f/<idx>/gk/<n>` → keyset の GKH + operational key）を送信のたびに読み直し
  （re-provision が即反映される）、AES-CCM で封止して ff35::（site-local
  transient multicast, hop limit 64）へ一発送信（応答なし・MRP なし、chip-tool
  と同じ unacknowledged groupcast の意味論）。counter は
  `<store>/native_group_counter`（10進テキスト、persist-ahead 4096）に永続化し、
  起動時に `max(自前, chip-tool の g/gdc) + 4096` へ jump-ahead する（chip-tool
  と同一 source node id で counter 空間を共有するため、低い値から始めると受信側の
  重複窓判定で全滅する）。`GroupProvision` と汎用 group invoke（onoff 以外・引数
  付き）は引き続き chip-tool。group 未 provision・KVS 不備・`g/gdc` 欠落など native
  で送れない事情は warn ログの上 chip-tool へフォールバックする。応答 JSON
  スキーマは両経路で共通（`group_sent_body`）。native 無効時（`MAT_MATD_IFACE`
  未指定）は全 group op が chip-tool のまま（挙動不変）。設計は
  `docs/superpowers/specs/2026-07-12-phase5-m5-group-native-design.md`。
  **実機 E2E 合格**（2026-07-13、`task e2e:m5`、実 fabric の 7 ノード group）:
  native groupcast の off / on / color-temp が 7/7 配達（各ノードを unicast read
  で検証）、matd 再起動後も jump-ahead した counter（+8192、単調増加をログで
  確認）で 7/7 配達。初回走行は 0/7 不達で、tcpdump により「宛先 sockaddr の
  `sin6_scope_id` だけでは egress iface を選べず、VPN（tailscale0）の広い v6
  経路が multicast の経路解決を勝って LAN に出ていない」ことを特定 —
  `GroupSender::new` で `IPV6_MULTICAST_IF` を明示設定して解決（回帰テスト付き）。
- M6a 完了(2026-07-13): native commissioning（on-network PASE、attestation は
  DAC/PAI/PAA チェーン検証 厳格 + CD signer 検証は warn のみ、RCAC/NOC の
  自己生成による使い捨て第二 fabric、既存 operational セッション上での
  native OpenCommissioningWindow、`_matterc` browse による discriminator
  探索）を `mat-controller` に実装。ライブラリ + E2E のみで、本番の
  `mat commission` / `matd` は無変更（chip-tool 一発コミッショニングのまま）。
  設計は `docs/superpowers/specs/2026-07-13-phase5-m6a-commissioning-design.md`。
  ローカル E2E 合格（`task e2e:m6`、all-clusters-app 相手、5 手順: 誤
  passcode 拒否 / native commission+制御 / native open-window / 第二 admin
  commission / RemoveFabric 撤収で最初の fabric が生存）。実機 E2E ハーネス
  （`task e2e:m6:real` — 本番 fabric の Nanoleaf へ相乗り open-window→
  使い捨て第二 fabric へ native commission→onoff 制御→RemoveFabric 撤収→
  本番 fabric 無傷確認、本物 DAC の厳格 attestation を通す）は実装済みで
  コンパイル確認済みだが、ユーザー立ち会いでの実機実行はまだ行っていない
  （次セッションで実施）。
- M6b 完了(実装 2026-07-13、実機 E2E 合格 2026-07-15 — 玄関ライトで BLE+Thread
  commissioning を chip-tool 無しでフル完走): BTP/BLE native commissioning（bluer 経由、GATT
  ペリフェラル接続 + BTP ハンドシェイクの上に PASE を通す。libdbus にリンクする
  bluer は feature `ble` で隔離 — 本番 `mat`/`matd` の musl クロスビルドは
  この feature を使わず、chip-tool 廃止後も本番バイナリは BLE 依存を持たない）
  + Thread operational dataset 配布（`NetworkCommissioning` cluster への
  `AddOrUpdateThreadNetwork` + `ConnectNetwork` で書き込み、以後は operational
  mDNS 経由で追跡）を追加。BTP 上では MRP は無効（GATT notify 自体が信頼性を
  担保するため、BLE 区間は unreliable-send の単純往復）。本番経路は無変更
  （`mat commission` / `matd` は引き続き chip-tool 一発コミッショニング）。
  テストはモック `GattLink` を使った統合テスト（`tests/btp_pase_plumbing.rs` —
  実際に PASE の最初のメッセージまで通す、feature 不要）+ 実機ハーネス
  （`crates/mat-controller/tests/live_commission_ble.rs`, feature `ble` 必須、
  `task e2e:m6b:real` — 工場リセットした玄関ライトを対象に BLE+Thread native
  commission→使い捨て fabric で onoff 制御→native open-window→別端末で本番
  `mat commission` 実行→使い捨て fabric を RemoveFabric 撤収、という spec の
  実機受け入れ手順そのまま。jarvis 上で実行する前提、BLE は WSL では動かない）。
  実機 E2E はこのタスクでは未実施 — 別途実施後、結果をここに追記して最終化する。
- M7 実装済み(2026-07-15): native 版 mat（one-shot 直経路の native 化）+
  本番 matd の native 化。設計は
  `docs/superpowers/specs/2026-07-15-phase5-m7-native-mat-design.md`。
  決定は 4 つ: **決定1** matd `native.rs`（849 行）からプロセス形態非依存の
  コアを共有 crate **`mat-native`** に抽出（`NativeConfig`/`Engine`
  構築・`Establisher`/`NodeConn`・`GroupCtx`/group 送信/`ErrorKind` 写像。
  matd に残るのは warm セッション slot 管理と `Op`→native 判定のみで、matd の
  外部挙動・既存統合テストは無改変）。**決定2** `mat` one-shot 直経路の配線
  — グローバル `--iface`/`MAT_IFACE`（+ `MAT_FABRIC_INDEX` 既定1 /
  `MAT_ISSUER_INDEX` 既定0）で opt-in、経路優先順位は op 単位で
  matd 自動発見 → native 直 → chip-tool 直、対象 op は matd M4/M5 の
  ホットパスと完全パリティ（unicast on/off/color/color-temp/onoff read、
  group の onoff 引数なし on/off/toggle・color・color-temp）。失敗分岐も
  matd と同型（エンジン構築失敗→ warn+chip-tool フォールバック、unicast 失敗
  →即エラー（フォールバックしない、二重実行回避）、group native 不可→
  chip-tool フォールバック）。one-shot は warm セッションを持たず確立→1 op→
  破棄（設計ルール4維持）。**決定3** group counter のプロセス間共有 —
  `<store>/native_group_counter` を one-shot/matd で `flock` 排他共有
  （`PersistedGroupCounter` に `<path>.lock` の non-blocking exclusive
  flock を追加、保持中の相手がいれば `WouldBlock`→chip-tool フォールバック）。
  **決定4** ブランチ運用 — M7 実装+実機 E2E 合格後に `matter-controller` を
  `main` にマージし main マージ禁止（2026-07-10 決定）を解除、本番=main の
  原則を回復。バージョンは 0.17.0。受け入れ基準は 5 項目（one-shot 直
  native・counter 共有・フォールバック・本番 matd native・`task check` 回帰
  — 詳細は spec 参照）。**実機 E2E 合格 + 本番反映済み（2026-07-15 夜）**:
  受け入れ 1〜3 を jarvis で全通過（one-shot 直 native の unicast 5 形 +
  group 3 形の N/N 配達、counter のプロセス跨ぎ jump-ahead・単調増加、
  describe/diag/write の chip-tool フォールバック健全）。matter-controller を
  main に `--no-ff` マージ（決定 4 の実行）、本番 jarvis の matd を 0.17.0 +
  systemd drop-in（`MAT_MATD_IFACE=eth0` / `MAT_MATD_FABRIC_INDEX=2`）で
  native 有効化。本番受け入れ: warm unicast は native in-process（再起動後も
  chip-tool 未 spawn のまま処理）、group N/N 配達、describe は chip-tool
  lazy spawn フォールバックで成功。ロールバックは drop-in 削除 + restart
  （native 無効化）またはバイナリ退避分（`*.bak-0.16.0`）の復元。
  親 spec（2026-07-10）の未決事項のうち「mat 直経路（one-shot）を新 crate に
  載せ替える時期」は本 M7 で解決（決定2）。「chip-tool KVS のフォーマット
  互換をどのバージョン範囲で保証するか」は未解決のまま **M8**（chip-tool
  完全廃止: write/describe/diag/discover/commission の native 化、汎用
  name→ID テーブル、KVS 書込所有、バイナリ撤去）に送る。
- **M8（chip-tool 完全廃止、ユーザー決定 2026-07-16）**: 規模が大きいため
  M6a/M6b と同様にサブマイルストーンへ 3 分割。各段で実機 E2E 合格を受け入れ
  条件とし、M8a/M8b までは chip-tool フォールバックが生きているため撤退可能。
  設計は `docs/superpowers/specs/2026-07-16-phase5-m8a-generic-im-native-design.md`
  （冒頭に M8 全体分割と横断決定を記録）。
  - **M8a（本節、0.18.0）— 汎用 IM native 化**。
  - **M8b（0.19.0）— discover native 化**: mDNS browse
    （`_matter._tcp` operational + `_matterc._udp` commissionable）+ probe
    reachability。既存 `dnssd.rs`（operational 解決）の browse 拡張。
    **実装済み**: (1) **dnssd browse**（`mat-controller::dnssd`）: one-shot
    legacy unicast mDNS で `_matterc._udp`（commissionable）/ `_matter._tcp`
    （operational）の PTR を列挙し、instance ごとに SRV/TXT/AAAA を畳み込む。
    resolve 系と違い早期 return せず固定 window（`BROWSE_WINDOW` = 3 秒、
    CLI フラグ化なし）で打ち切り、クエリは 1 秒間隔で再送、フォローアップ
    質問は 1 メッセージ 8 件ずつに分割、受信バッファ 9000 byte。flood 耐性
    キャップ（instance 32 件 / AAAA プール 64 件）、他プロトコルの壊れた
    データグラムは読み捨てて継続。operational は announce のみ（SRV/AAAA
    が期限内に揃わない）でも addresses 空で保持し、commissionable は素材
    ゼロなら skip。browse は現在 **commissionable 専用**（下記実機知見で
    operational 側は targeted resolve に転換し、`browse_operational` は
    撤去済み）。(2) **`mat::probe::mdns` の native 化 = 台帳ノードごとの
    targeted `resolve_operational` を並行実行**（`tokio::task::JoinSet`、
    per-node timeout 3 秒、CFID は KVS（読み取りのみ）から計算。マーカー
    ログ `probe executed (native resolve)`）。IO エラー（iface 解決失敗・
    全ノード送信失敗）は warn ログ + `avahi-browse` フォールバック。
    `discover --probe` と `diag node --deep` の両方が対象（`diag node` の
    他チェックは引き続き chip-tool 経由 — IM 自体の native 化は M8c）。
    (3) **`mat discover` の commissionable 探索の native 化**: `MAT_IFACE`
    設定時は native `browse_commissionable` を既存 `DiscoveredDevice`
    スキーマへそのまま写す（JSON はバイト一致）。IO エラー時は warn ログ +
    chip-tool フォールバック。(4) **フォールバック規則はどちらも共通**:
    フォールバックは IO エラー時のみ、探索/probe が 0 件なのは正常で
    フォールバックしない。(5) discover/probe は matd プロトコルの対象外の
    まま（one-shot 直経路のみ）。(6) **dead API 掃除**: M8a で呼び出し
    ゼロになった matd `NativeBackend::ensure_group_acl` を削除。バージョン
    は 0.19.0。**実機 E2E 合格（2026-07-17、jarvis、検証 1〜5 全 GREEN**
    — 検証 2 の commissionable は玄関ライト非広告のため設計どおり WARN +
    確認続行）。**★実機知見（E2E 中に 2 回 FAIL して確定した設計転換）**:
    ① Thread mesh の advertising proxy は保持全 instance をサービス型 PTR
    列挙に載せない — 集約応答は 1 データグラム（実測 1428B / 29 PTR / TC
    ビット）で切れ、しかも Known-Answer suppression（RFC 6762 §7.1/7.2、
    今回 browse に実装済み・維持）を入れても**残りのノードはワイヤに一切
    出ない**（node 6/8/9 で tcpdump 実証）。一方 targeted な instance
    解決には同じ proxy が正しく答える（native read で実証）。avahi-browse
    が「全部見える」のは常駐デーモンの長期キャッシュ（PTR TTL 75 分）に
    よるもので、キャッシュなし one-shot の列挙では構造的に再現できない。
    → **probe は列挙+照合ではなく targeted resolve が正**（CFID+node_id は
    既知）。② 列挙が本質の commissionable browse には KA suppression +
    `MAX_INSTANCES` 128 を適用（マルチ fabric の実レジストリは 30 件超）。
  - **M8c（chip-tool 完全撤去、ユーザー決定 2026-07-17 に 3 分割）**: 規模が
    大きいため M8a/M8b と同様にサブマイルストーンへ分割。各段で実機 E2E 合格
    を受け入れ条件とし、M8c-2 までは chip-tool フォールバックが生きているため
    撤退可能。設計は
    `docs/superpowers/specs/2026-07-17-phase5-m8c1-commission-native-design.md`
    （冒頭に M8c 全体分割とビルド検証スパイクの結果を記録）。
    - **M8c-1（本節、0.20.0）— commission native 化**: 既存 fabric 上の
      `mat commission` を M6a（on-network）/ M6b（BLE+Thread）実装へ配線。
      KVS 書込なし（既存 fabric 上の commission は read 系 API で足りる）。
    - **M8c-2（本節、0.21.0）— KVS group 書込所有 + diag node 再訪**:
      controller 側 `groupsettings`（chip-tool interactive）の native 化 =
      keyset / group table を mat が chip-tool INI 形式 KVS へ flock 排他で
      書く。matd provision のハイブリッド（M8a）解消。`diag node` の IM
      部分 native 化。**実装済み（本文後段、実機 E2E 未実施 — ハーネス
      `scripts/e2e-m8c2-real.sh` / `task e2e:m8c2:real` 用意済み）**。
    - **M8c-3（0.22.0）— native 既定化 + chip-tool 完全撤去**: 初回 fabric
      bootstrap（root CA 生成 + KVS 新規作成。chip-tool が同じ KVS を読む
      間は「実 chip-tool も受理できる新規 fabric」という互換問題を抱える
      ため、chip-tool 撤去後に回すのが安全）、`MAT_IFACE` 未設定でも
      native（group 送信 iface の自動選択 — multicast egress の罠、
      tailscale0 が経路解決で勝つ問題の設計をここで詰める）、runner.rs /
      chip-tool 分岐 / fake-chip-tool テスト基盤の置換 / Docker・repo 直下
      バイナリ・`MAT_CHIP_TOOL_BIN` の全削除、avahi-browse 撤去、BLE ビルド
      既定化の判断。
  - **横断決定（M8c 全体）**: ① KVS 書込所有は chip-tool INI 形式を継続
    （既存 `kvs.rs` リーダと実機 fabric データを流用、flock 排他で mat が
    書く。chip-tool 撤去後は mat が唯一のライター。M8c-2）。② name→ID は
    全クラスタ生成テーブル（M8a で実施済み）。③ BLE 本番既定化 —
    musl×bluer ビルド検証スパイク（M8c-1 冒頭で実施）の結果は
    **現状ビルド不可**: bluer → dbus → `libdbus-sys` が pkg-config で
    libdbus を見つけられず失敗、vendored ビルドの道はあるが aarch64-musl の
    C クロスツールチェーンが未整備。M8c-1 は確立済みの代替（M6b、
    `cross build --target aarch64-unknown-linux-gnu --features ble`、
    jarvis の glibc/libdbus-1.so.3 で動作実績あり）を使い、musl 経路は
    BLE なしのまま無変更で残す。gnu 一本化 / musl+gnu 二本立て / vendored
    整備のどれにするかは **M8c-3 で判断**。④ native を既定化（`MAT_IFACE`
    未設定でも動作。group 送信の iface は自動選択 + 設定で上書き、
    multicast egress の罠があるため自動選択の設計は M8c-3 spec で詰める）
    し、chip-tool 経路をコード・Docker イメージとも全削除（M8c-3）。
  - **M8c-1 実装済み**: (1) **epoch IPK 設計転換**（起草時点の計画は KVS
    から epoch を読み出す想定だったが、上流 connectedhomeip v1.4.2.0
    `TestGroupData.h`（`DefaultIpkValue::GetDefaultIpk`）の実証で epoch は
    KVS に永続されない（chip-tool は commissioner 初期化のたびに定数
    "temporary ipk 01" を投入し、KDF 導出した *operational* 鍵だけを
    `f/<idx>/k/0` に永続する）と判明したため、既定定数
    `fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH` + 実行時 KDF ガード
    `verify_default_ipk_epoch`（root_public_key・fabric_id・KVS の
    ipk_operational から `compressed_fabric_id`→`derive_ipk_operational` の
    一致を検証）へ設計転換。不一致（IPK ローテーション済み / chip-tool 産
    でない fabric）は native が引き受けず chip-tool フォールバック。
    (2) `CommissioningFabric::from_materials`（`generate()`＝新規 fabric
    パスと対をなす、既存 fabric の `SelfIssueMaterials` から commission 用
    コンテキストを組む新設 API）。(3) **`mat-native::commission`**（薄い
    ラッパー、プロトコル本体は mat-controller）: iface 解決→KVS 読み出し→
    epoch ガード→`CommissioningFabric` 構築（ここまで失敗は全てワイヤ
    未接触＝`Unavailable`）→発見＝**自動 mDNS→BLE**（QR は
    `resolve_commissionable`（long discriminator、5 秒 timeout）で
    on-network、timeout なら feature `ble` かつ `--thread-dataset`/
    `MAT_THREAD_DATASET` 指定時のみ BLE スキャン→`commission_ble_thread`
    へ；manual code は `browse_commissionable`+short discriminator フィルタ
    — 0 件は未発見でフォールバック、2 件以上は曖昧エラー（chip-tool でも
    同じ曖昧さのためフォールバックしない）、manual code に BLE 経路は無く
    mDNS miss は直接フォールバック）→on-network/BLE 実行。(4) **フォール
    バック境界**: ワイヤ未接触（iface/KVS/epoch ガード/発見の空振り、
    `commission_on_network` 内部の事前 resolve 後の狭い競合窓での再空振り
    含む、レビュー修正で `Discovery` エラーも `Unavailable` に是正済み）は
    chip-tool フォールバック可、PASE 開始後の失敗は即エラー（二重
    commission 回避、unicast native の「フォールバックしない」と同じ理屈）。
    (5) **ErrorKind 写像**（M6b fix-later の解消）: Timeout→timeout /
    Attestation・Noc・CommandStatus→device_rejected / NetworkConfig→
    unreachable / Malformed・Csr→parse_error / 他（Ble 含む）→
    commission_failed。(6) CLI: 新引数 `--thread-dataset`（env
    `MAT_THREAD_DATASET`、BLE 経路のみ使用）、PAA と同型解決順の
    `MAT_CD_SIGNER_STORE`→`<store>/cd-signer-store`（無くても続行、CD 検証
    は warn のみ、M6b 決定どおり）。マーカーログ
    `commission executed (native on-network)` /
    `(native ble-thread)`（成功時 info）、`falling back to chip-tool`
    （フォールバック時 warn）。(7) **feature `ble` 貫通**: mat→mat-native→
    mat-controller の 3 層に伝播、musl ビルドは BLE 分岐がコンパイル時に
    消え「このビルドは BLE 非対応」という `Unavailable` にフォールバック
    （ハードエラーにしない）。(8) matd への配線なし（commission は恒久的に
    matd 対象外、diag thread/open-window/group grant と同じ理由）。
    (9) 実機 E2E ハーネス `scripts/e2e-m8c1-real.sh` /
    `task e2e:m8c1:real` 新設（要 jarvis + 玄関ライト + BLE、setup code は
    実行時人力入力、chip-tool バイナリ不在下での成功を持って純 native 実行
    の証明とする方式は M8a Task11 の二重チェック流儀を踏襲）。バージョンは
    0.20.0。**実機 E2E 合格（2026-07-17、jarvis）**: 玄関ライト
    （Nanoleaf、disc 3841 / vid 0x4442 / pid 0x68）を `MAT_CHIP_TOOL_BIN=
    /nonexistent` 下で native BLE+Thread commission → BLE scan→BTP→PASE→
    attestation→CSR→NOC→Thread 参加→operational mDNS→CASE→
    CommissioningComplete まで chip-tool 無しで完走（マーカー
    `commission executed (native ble-thread)`、node 15 として台帳記録、
    native on/off 往復成功）。フォールバック健全性も実証: BLE 未検出時に
    `Unavailable(ble scan: btp timeout)` → `falling back to chip-tool` warn +
    exit 12（未接触失敗のフォールバック規則どおり）。★実機知見: 玄関ライトを
    先に Matter ペアリングモード（commissioning window）に入れておくこと —
    window が閉じていると BLE に `_matterc`(0xFFF6) 広告が出ず native は
    設計どおり Unavailable になる（btmon で 0xFFF6 の有無を事前確認可）。
    この E2E で玄関ライトは mat fabric に復帰（M6b 以来の fabric 無し状態が
    解消）。
  - **M8c-2 実装済み**: 設計は
    `docs/superpowers/specs/2026-07-17-phase5-m8c2-groupsettings-native-design.md`。
    (1) **`KvsTxn`**（`mat-controller::kvs`）: chip-tool の `chip_tool_config.ini`
    を flock 排他（non-blocking exclusive、他プロセス保持中は `KvsError::Locked`
    即エラー）+ 読み・変更・commit は tmp ファイルへ書いてから rename する
    原子置換で書く汎用トランザクション。値は INI 内 base64 の TLV バイト列
    （既存 KVS リーダと同じ表現）。GKH（Group Key Hash = group session id）の
    HKDF-SHA256 導出（`derive_group_session_id`、salt なし・info
    `"GroupKeyHash"`、上流 v1.4.2.0 `CHIPCryptoPAL.cpp
    DeriveGroupSessionId` と同一）を `mat-controller::fabric` に追加。
    (2) **`mat-controller::group_settings`**: `write_group_provision` が
    chip-tool `groupsettings add-group`/`add-keysets`/`(unbind-keyset)`/
    `bind-keyset` 相当の 5 レコード（`g/gfl`, `f/<idx>/g`,
    `f/<idx>/g/<gid>`, `f/<idx>/gk/<id>`, `f/<idx>/k/<ksid>`）を 1 回の
    `KvsTxn`（＝1 flock 区間）内で読み・変更・commit まで完結させる。上流
    v1.4.2.0 `GroupDataProviderImpl` と同じリンク規律を再現（group リストは
    末尾挿入・終端 0、keyset リストは head 挿入・終端 `0xFFFF`（id 0 は IPK
    で有効値）、keymap は末尾連結・id は sparse に `max+1`、走査は count が
    正）。上流との意図的な差分は 2 つ: ①リンク切れ・解釈不能レコードは
    黙って進まず `GroupSettingsError::Corrupt`（不整合ストアをこれ以上
    悪化させない）。②新規 `GroupData` の `first_endpoint` は常に
    `kInvalidEndpointId`（`0xFFFF`）— 上流は直前に走査した他レコードの値が
    漏れ込むが `endpoint_count=0` のとき読者はこの欄を見ないため互換に
    影響しない。`rebind: bool` は chip-tool と同じ意味論（無しで既存 bind
    と衝突すると `DuplicateBind`＝「`--rebind` で解消」を指す detail 付き
    エラー、有りは best-effort unbind→bind）。
    (3) **`mat-native::group_settings`**: 薄いラッパー
    `GroupSettingsCtx { main_ini, fabric_index, cfid }` +
    `write_group_provision`。`GroupSettingsError` は全て `ErrorKind::Other`
    の hard error に写像（フォールバック可否の判断はここでは行わない —
    呼び出し側は「ctx 未構成（KVS 資材が解決できていない）」のときだけ
    フォールバックし、書込を試みた後の失敗（`DuplicateBind` /
    flock `WouldBlock` 含む）は常にそのままエラーとして返す）。
    (4) **配線**: 直経路（`native_direct.rs::run_op`
    `NativeOp::GroupProvision`）は `engine.group_settings` が `None`
    （ctx 未構成＝ワイヤ・KVS とも未接触）なら chip-tool へフォールバック、
    `Some` なら chip-tool を一切 spawn せず native KVS へ書いてからデバイス
    側 4 ステップ（M8a のまま・native unicast）を実行する。matd
    （`server.rs::group_provision`）も対称: `NativeBackend::group_settings_ctx()`
    が `Some` ならコントローラ側を native 書込に、`None`（native 無効・
    テスト注入等）なら従来どおり chip-tool ws 経由の 4 コマンドに完全に
    フォールバックする（M8a のハイブリッド — デバイス側のみ native・
    コントローラ側は常に chip-tool — を解消）。matd 側はテストで実証:
    `groupsettings` で始まる ws コマンドを受信したら panic する fake ws を
    用意し、native ctx 存在時にそのソケットへ一切トラフィックが飛ばない
    ことを確認。出力: native 書込のときは `--rebind` の有無によらず常に
    `"note": "controller group state written natively to kvs; if matd is
    running, restart it to reload group state"`（`emit_provision_success`
    に `native_kvs: bool` を追加、chip-tool 経路の出力・note は無変更）。
    (5) **`diag node` の IM native 化**（直経路のみ — `diag node` は matd
    プロトコル対象外のまま、`--deep` の ping6/mDNS は M8b のまま無変更）:
    `native_direct::diag_im_probe`/`diag_im_with_engine` が 1 CASE
    セッションで operational（descriptor/parts-list read、
    `mat-core::ids` の名前表から解決）と thread シグナルの両方を賄う。
    thread の field-id 知識（`NEIGHBOR_TABLE_FIELDS` 等）はプロトコル知識を
    `mat` command 層に持ち込まないよう `mat-native::ops::diag_thread` /
    `thread_check_from_snapshot` 経由に一本化（Task7 レビュー修正、
    CLAUDE.md 設計ルール1）。self-CFID はログパースをやめ、
    エンジンが保持する fabric 資材（`engine.group_settings.cfid`）から直接
    計算する — native 経路では `cfid_unavailable` の系がそもそも発生しない
    （chip-tool 経路は従来どおりログ由来）。エンジン構築失敗は
    `diag_im_probe` が `None` を返し、呼び出し側が chip-tool 経路へ
    フォールバックする（他 native op の構築失敗フォールバックと同型）。
    (6) 実機 E2E ハーネス `scripts/e2e-m8c2-real.sh` / `task e2e:m8c2:real`
    新設（jarvis、living_lights の 2 台・既定 node 8/9 を使い捨てグループ
    99 に入れて検証。M8a/M8b/M8c-1 と同じ二重チェック方式 —
    `MAT_CHIP_TOOL_BIN=/nonexistent` での chip-tool spawn ゼロ強制 +
    positive marker（`group provision controller state written (native
    kvs)` / `group provision executed (native direct)` / `diag node
    executed (native)`）grep + `assert_no_fallback`）。検証項目: native
    provision（chip-tool spawn ゼロ、note 付き）/ native groupcast
    往復（各ノード on-off 反転を native read で確認）/ `--rebind`
    再実行が成功・`--rebind` 無しの再実行が `use --rebind` 誘導の detail
    で失敗（exit 1 = `ErrorKind::Other`）/ chip-tool 互換（mat が書いた
    KVS を実 `chip-tool groupsettings show-groups`/`show-keysets` が読める
    ことの実証）/ `diag node --deep` の operational・thread が native で
    完走 / `MAT_IFACE` 未設定時の chip-tool 経路健全性。ハーネスは KVS
    バックアップを取ってから走らせる前提（Task8 レビュー修正で `g/gdc`
    書換をバックアップ実在ガードで保護 — バックアップが無い状態で誤って
    破壊的な書換に入らないようにする安全策）。バージョンは 0.21.0。
    **実機 E2E は本タスクの範囲外（未実施）**。
  - **M8c-3 実装済み（0.22.0、native 既定化 + chip-tool 完全撤去 +
    fabric bootstrap）**: 設計は
    `docs/superpowers/specs/2026-07-17-phase5-m8c3-native-default-design.md`。
    内部二段構え（撤去は不可逆なため間に実機 E2E ゲートを置く）。
    - **Stage 1（native 既定化、フォールバック温存）**: (1) **iface 自動検出**
      （`mat-native::iface_select`）— up（carrier 有）・MULTICAST・非 loopback・
      非 POINTOPOINT（tun/tailscale 除外）・IPv6 link-local 保有の候補を
      `/sys/class/net` + `/proc/net/if_inet6` から集め、ちょうど 1 つで採用、
      0/2+ は kind `other` のハードエラー（候補列挙 + `set MAT_IFACE`）。
      純関数 `select` を表駆動ユニットテスト。`MAT_IFACE`/`MAT_MATD_IFACE`
      未設定でも native 経路に入る（env は明示上書きとして存続、matd は曖昧なら
      起動拒否）。(2) **epoch 採用永続**（`mat-native::commission::resolve_ipk_epoch`）
      — 解決順は (1) KVS の `mat/f/<idx>/ipk-epoch` 読み出し → (2) 無ければ既定定数を
      `verify_default_ipk_epoch` で検証し一致ならその場で同キーへ採用永続（flock
      書込、失敗はハードエラー）→ (3) 不一致は `store_parse` ハードエラー
      （M8c-1 の「不一致→フォールバック」から恒久挙動へ前倒し）。
    - **Stage 2（完全撤去 + fabric bootstrap）**: (1) **chip-tool / avahi-browse
      経路の全削除**（`mat` の runner spawn・各コマンドの chip-tool 分岐・
      `MAT_CHIP_TOOL_BIN`・probe/discover の avahi フォールバック、matd の
      chip-tool ランナーとフォールバック分岐、`Data = ...` パーサ、
      `fake-chip-tool.sh`、統合テストの chip-tool 依存 — Task 9/10/11）。exit
      12（`ChildNotFound`）は歴史的欠番として予約（variant は wire 互換で残置、
      `diag node --deep` が ping6 不在を `unavailable` 配列の `tool_missing`
      へ吸収する内部用途のみ）。新 error kind は追加せず（iface 曖昧 = `other`、
      KVS 系 = `store_missing`/`store_parse`）。(2) **`mat fabric init`**
      （`crates/mat/src/commands/fabric.rs`、直経路のみ・ネットワーク未接触）—
      `CommissioningFabric::generate` で root CA + fabric 資材、OS 乱数で 16 バイト
      ランダム epoch IPK、chip-tool INI 互換 KVS を新規 bootstrap 書込
      （`write_kvs_bootstrap`）。既存 KVS があれば拒否（`--force` 無し、再初期化は
      手動 ini 削除）。出力は fabric 識別子のみ（鍵素材は stdout に出さない）。
      他コマンドは KVS 不在なら `store_missing`（exit 10）+ detail に
      `mat fabric init` 誘導。(3) **ビルド一本化**: deploy 標準を
      `cross build --release --target aarch64-unknown-linux-gnu --features ble`
      に統一（`task dist:arm64`）、musl deploy 経路と Cross.toml の musl 設定を
      撤去。ローカル `task check` は host build・BLE なしのまま無変更。Docker は
      chip-tool 焼込ステージ・avahi/dbus/glib/ssl runtime 依存を撤去し
      mat/matd バイナリ + 最小 runtime（`debian:bookworm-slim` + ca-certificates）
      にスリム化（`docker:test` は維持）。旧 E2E スクリプト（e2e-m2〜m8c2）は
      歴史的アーカイブ注記を付けて残置（0.21.0 タグでのみ動作）、Taskfile の
      対応タスクは削除し `e2e:m8c3:real` のみ残す。
    - **実機 E2E ゲート 1（Stage 1、jarvis 実運用 fabric、env 未設定）**:
      **GREEN**。全 op が native 完走・「falling back to chip-tool」発火ゼロ、
      iface 自動検出が jarvis（eth0+tailscale0）で eth0 を一意選択、初回 native
      commission で epoch 採用永続が発生し `mat/f/<idx>/ipk-epoch` が書かれ 2 回目
      以降は KVS 読み出しで通ることを実測。**文書化した逸脱**: 受け入れ手順の
      「RemoveFabric → on-network 再 commission」は NL68（玄関ライト）では構造的に
      不可能（工場リセットせずに commissioning window を開く手段がデバイス側に
      無い）ため、epoch の検証は手動 native commission を複数回走らせて採用永続の
      冪等性・単調性を確認する形に置換した。**ゲート 2（chip-tool を PATH から
      外した環境での最終受け入れ + `mat fabric init` 実機 + deploy 成果物の実機
      動作）は Task 13 で実施予定（pending）**。
    - **将来候補（M8c-3 でやらないと決めたもの、記録のみ）**: (1) fake Matter
      デバイス（UDP loopback で PASE/CASE/IM 応答するテスト基盤 — バックエンド
      挙動を実機なしで回帰させる）。(2) 汎用 list/struct TLV エンコード（現状
      scalar のみが仕様、汎用 write/invoke の後退を受容）。(3) IPK ローテーション
      （全ノード KeySetWrite での epoch 完全移行 — 現状は既存 fabric の定数 epoch を
      検証して採用永続するのみ）。(4) CASE の多アドレス試行（ゲート 1 で観測した
      弱点: 1 ノードが複数 AAAA を広告するとき現状は 1 アドレスにしか CASE を
      張らず、その経路が不調だと session_failed になる — 複数アドレスへ順次
      試行する堅牢化）。
  - **M8a 実装済み**: (1) **name→ID 全クラスタ生成テーブル**
    （`mat-core::ids` / `ids_gen.rs`、connectedhomeip v1.4.2.0 data-model
    XML から `scripts/gen-ids.py` で生成しチェックイン — ビルド時に XML・
    ネットワーク不要。140 クラスタ / 属性 1844 / コマンド 371、global ZCL
    属性 5 種は全クラスタに合流。cluster/attribute/command 名→ID・型タグ・
    フィールド順序と逆引きを保持。数値 ID 直指定は全経路で常に許可、
    cluster/attribute の意味論は従来どおり chip-tool 記法のまま
    — CLAUDE.md の「Cluster / attribute names stay chip-tool notation」は
    不変）。(2) **IM 拡張**（`mat-controller`）: WriteRequest/WriteResponse
    の encode/decode（timed write は M6a 実装済みの
    `encode_timed_request` を流用）、attribute wildcard read（cluster 内
    全属性の一括 read、describe/diag が使う）、チャンク/リスト追記対応
    ReportData、TLV→JSON（深さ上限 32、チャンク上限 64）。(3) **native 化
    op**（経路優先順位・失敗分岐は M7 と同型）: `read`（汎用形）/ `write` /
    `invoke`（汎用形）/ `describe` / `group invoke`（汎用形）/
    `group provision`（コントローラ側 groupsettings は M8c まで chip-tool、
    デバイス側 4 ステップのみ native のハイブリッド）は one-shot 直経路
    （`MAT_IFACE`）と matd（`MAT_MATD_IFACE`）の**両方**に配線。
    `diag thread`（`diag node` は対象外、M8c で再訪）/ `open-window`
    （M6b 実装済みの配線のみ）/ `group grant`（デバイス側のみ）は
    matd プロトコルが元々扱わない op 群のため（warm session の恩恵が薄い
    稀な操作、M8a 以前からの設計）one-shot 直経路 `MAT_IFACE` のみに配線 —
    matd 経由では従来どおり常に chip-tool（`mat` 側が `matd` に op を送らず
    直接処理する）。(4) **JSON→TLV の型サポートはスカラーのみ**（bool/int/uint/
    enum/bitmap/string/octstr、bytes は `hex:` 形式）。既知名で list/struct/
    float を指定した汎用 write/invoke は `parse_error` で明示拒否。未知名は
    従来どおり chip-tool にフォールバック。list/struct 書込が要る
    group provision/grant（KeySetWrite・GroupKeyMap write・binding
    write・ACL read-modify-write）は生成テーブルに依存しない専用エンコーダ
    （`group.rs`/`acl.rs` の延長）。(5) **group-key-map はデバイス側で
    read-merge-write 化**（chip-tool 経路の全置換より安全な意図的改善）。
    (6) 判定中核 **`classify_write` / `classify_invoke`** は `mat-core::ids`
    に一本化し mat/matd で共有。バージョンは 0.18.0。受け入れ基準は
    5 項目（spec 参照）。**実機 E2E 合格（2026-07-16 夜、jarvis）**:
    検証 11 項目全 PASS — 直経路 native の read 汎用/write（読み返し+null
    後始末）/未対応型 write の parse_error 拒否/invoke 汎用/describe（chip-tool
    経路と構造一致）/diag thread（主要キー一致 — field-id テーブルの実機実証）/
    open-window/grant 冪等（2回目全ノード unchanged）/provision --rebind +
    groupcast N/N（Matter 状態で 7/7 確認）、matd 経由の同 op native
    （`chip-tool ws raw response` ログ不在 = 実トラフィックゼロの実アサーション）、
    `MAT_IFACE` 未設定のフォールバック健全性（同項目を chip-tool 経路で全通過）。
    ACL は全 7 ノードで前後とも 2 エントリのまま（provision×3・grant×4 を経て
    汚染・重複ゼロ — IsFabricFiltered 修正の実機実証）。**実機知見 2 点**:
    ① 本番 matd（native）が group counter の flock を保持している間は one-shot
    native の group 送信は設計どおり chip-tool へフォールバックする — E2E で
    native group 送信を実証するには事前に `systemctl restart matd` で flock を
    解放しておくこと（M7 E2E 時は本番がまだ 0.16.0 で顕在化しなかった）。
    ② E2E 中の chip-tool spawn 群が g/gdc を進めるため、送信経路を跨ぐと
    counter 窓の逆転で silent drop が起きる（既知の混在知見の再確認。matd
    再起動で回復）。フォールバック検証はメッシュ不調の node を避けて実施
    （node の一過性 CASE 失敗 ×2 で 2 回中断 → 検証 1〜10 は同一走行で PASS、
    検証 11 は同ハーネスから抽出した同一手順を安定 node で PASS）。

---

## Things we never do

- Implement TLV / CASE / multicast routing inside `mat` or `matd` command
  layers (protocol code lives only in the `mat-controller` crate; the
  chip-tool delegation path remains until Phase 5 lands).
- Hold human names or logical groups in `mat` (out of scope; exception: the
  optional `aliases.toml` name→number map for node / group / endpoint, see
  above — it resolves to numbers before anything reaches chip-tool/matd).
- Add session cache, subscriptions, a daemon, or an internal scheduler to `mat`
  (that is `matd`'s role, a separate binary).
- Bring a Matter bridge (becoming a Matter device) into `mat`.
- Hold scenes, automation, or voice/UI entry points in `mat`.
- Render or display QR images on stdout (emit the `qr_payload` string only).
- Commit credentials, real topology, or real certificates to the repo.
