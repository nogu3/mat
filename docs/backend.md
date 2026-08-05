# Backend

`mat`'s backend is a **native, from-scratch Rust Matter controller** (crate
`mat-controller`, driven through the shared `mat-native` engine) — TLV, CASE,
IM, groupcast, mDNS, and commissioning (on-network + BLE+Thread) are all
in-process. There is no `chip-tool` (or any external controller) subprocess.

- **Route selection is per-op:** matd auto-discovery (if a `matd` answers the
  probed socket) → `mat`'s own native direct path. See
  [Routing through `matd`](commands.md#routing-through-matd) and
  [Native backend internals](commands.md#native-backend-internals) for interface
  autodetect (`MAT_IFACE` / `MAT_MATD_IFACE` override), fabric index, warm vs
  one-shot sessions, the shared groupcast counter, epoch adoption, and the
  scalar-only generic write/invoke rule.
- **First-fabric bootstrap** is `mat fabric init` (random-epoch IPK); see
  [that section](commands.md#first-fabric-bootstrap-fabric-init).

Environment variables:

| variable | purpose |
|---|---|
| `MAT_STORE` | credential store path (see the resolution order in [Configuration](configuration.md#credential-store)) |
| `MAT_IFACE` | override interface autodetect for `mat`'s direct path |
| `MAT_MATD_IFACE` | override interface autodetect for `matd` |
| `MAT_FABRIC_INDEX` / `MAT_ISSUER_INDEX` | `mat` KVS fabric-table / CA-issuer index (default `1` / `0`) |
| `MAT_MATD_FABRIC_INDEX` / `MAT_MATD_ISSUER_INDEX` | same for `matd` |
| `MAT_MATD` / `MAT_MATD_SOCKET` | force / opt out of the matd path; pin its socket |
| `MAT_OP_TIMEOUT_MS` | budget (ms) for a single-node op; default `60000`, `0` = unlimited — see [Op timeout budget](commands.md#op-timeout-budget---op-timeout-ms) |
| `MAT_PAA_TRUST_STORE` | directory of PAA root certs for attestation |
| `MAT_CD_SIGNER_STORE` | CD signer trust store (warn-only if absent) |
| `MAT_THREAD_DATASET` | Thread active operational dataset (hex) for BLE+Thread commission |
| `MAT_TRANSPORT` | commission route: `auto` (default) / `on-network` / `ble` |
| `MAT_PING6_BIN` | override the `ping6` binary used by `diag node --deep` |
| `MAT_LOG` | `tracing` filter for stderr logs (e.g. `info`); empty counts as unset — see [Logs](errors.md#logs-stderr) |

> Matter uses mDNS / IPv6 multicast, so running in Docker **requires host
> networking** (`docker run --network host`). A bridge network cannot receive
> the responses.

