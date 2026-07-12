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

`mat` makes no assumptions about its caller. It is a standalone Matter controller
CLI: a shell, a script, or a higher-level program can drive it. Whatever sits
above it is not `mat`'s concern.

---

## What `mat` does and does not do

### `mat` is responsible for
- A consistent wrapper UX over a Matter controller (`chip-tool`).
- Turning the controller's verbose text into `mat`'s JSON schema.
- Managing fabric credentials (Root CA, our own NOC, commissioned nodes) in a
  local key-value store (KVS).
- Commissioning: joining a fabric and sharing devices with other admins.

### `mat` is NOT responsible for
- **Resolving human names to (node_id, endpoint, cluster).** `mat` takes a
  numeric `node_id`. Mapping human-facing names is out of scope — with one
  narrow exception: if an optional `<store>/aliases.toml` exists, the CLI
  layer resolves node / group / endpoint aliases to numbers right after arg
  parsing, before dispatch. The wire and the backend (`chip-tool` / `matd`)
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
other `mat` command and runs only on the direct chip-tool path (not via
`--matd`).

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
4. **Fits the subprocess model.** Launch a native binary and exit.

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
`mat` couples to the backend through **only `mat`'s own JSON schema**.

- Decided (2026-07-10): a from-scratch Rust controller library, crate
  `mat-controller` in this workspace — see "Phase 5" below for the decision
  record and milestones.
- A replacement must be one adapter in the child-runner, with `mat`'s JSON schema
  as the contract. Subcommands and output schema do not change.

---

## Design rules (must follow)

1. **Protocol code lives only in the backend crate.** TLV, CASE, session
   crypto, multicast routing — all of it belongs to `mat-controller`
   (Phase 5) and nowhere else. The `mat` CLI and `matd` command layers
   never speak the protocol; until Phase 5 lands they delegate everything
   to `chip-tool`, which remains the production path.
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

## `mat` and `matd`

This repo ships two binaries from one install:

```
        a shell, a script, or any higher-level caller
                          |
          +---------------+----------------+
          v                                v
   mat  (one-shot; credential KVS only)   matd (resident; Phase 4)
       \   Command::new("chip-tool")        \  warm CASE sessions / unix socket
        \                                     \
         +------------------+------------------+
                            v
   chip-tool ── real Matter devices (Thread / Wi-Fi / Ethernet)
```

- **`mat`** is the one-shot CLI. It spawns `chip-tool`, runs one command, and
  exits. Design rule 4 (no daemon / cache inside `mat`) always holds.
- **`matd`** is the resident binary (Phase 4). It keeps warm CASE (Sigma)
  sessions so repeated Matter calls skip the handshake — the same model as ssh
  `ControlMaster`/`ControlPersist`. `matd` is allowed to be resident precisely
  because it is a **separate binary and layer**, not `mat`. `mat` **auto-detects**
  a running `matd` by default (a connect probe on the default socket, falling
  back to spawning `chip-tool` directly when nothing answers); `--matd`/
  `MAT_MATD=1` force the matd path, `MAT_MATD=0` disables probing entirely.

Both binaries share a library crate `mat-core` (the `parse` / `output` / `error`
/ `group` modules: chip-tool parsing, the JSON schema, exit-code classification,
group key logic) so the fragile parts are maintained once.

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
- Milestones M1–M6 with independent acceptance criteria; the chip-tool
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
  / group 系は引き続き chip-tool 経由（group は M5 で native 化予定）。実機 E2E 合格
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
  **実機 E2E は未実施**（合格後に別コミットで本欄を更新する。M4 と同じ運用）。

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
