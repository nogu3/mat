# Errors and exit codes


Errors go to stderr as `{"error":{"kind":"...","detail":"..."}}`.

| code | meaning |
|---|---|
| 0 | success |
| 2 | CLI argument error (clap default) |
| 10 | credential store missing / parse failure |
| 11 | node_id not commissioned |
| 12 | *(retired in 0.22.0 — historical vacancy)* |
| 3 | timeout (includes a single-node op exceeding its `--op-timeout-ms` budget) |
| 4 | device rejected |
| 5 | unreachable / network |
| 6 | CASE session establishment failed |
| 13 | `matd` absent / unreachable (`mat listen` only) |
| 1 | other |

When `commission` tries more than one route, the reported `kind` and exit code are
those of the **last** route attempted, and `detail` lists every route's result.

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
  classified from the native transport / IM result. `timeout` is also what a
  single-node op returns once its budget runs out (`--op-timeout-ms` on
  `mat`, or `matd`'s own 60s default when the request carried none) — see
  [Op timeout budget](commands.md#op-timeout-budget---op-timeout-ms).
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

## Logs (stderr)

`mat` and `matd` write diagnostics to **stderr** as structured `tracing` logs
(stdout stays pure JSON). The filter comes from `MAT_LOG`, falling back to
`RUST_LOG`; the default is `warn` for `mat` and `info` for `matd`. An empty
value counts as unset, so `MAT_LOG=` falls back to the default instead of
silencing everything.

`matd` never emits ANSI escapes, and `mat` colors only when stderr is a
terminal — so `grep node_id=42` works on a journal or through a pipe.

`matd` logs one line per op:

| line | level | when |
|---|---|---|
| `matd op failed` | warn | the path itself failed — `timeout` / `unreachable` / `session_failed` / `other` / `commission_failed` / `matd_unavailable` (plus the retired `child_*` kinds). Carries `kind` and `detail`. |
| `matd op rejected` | info | the request or its meaning was refused — `store_missing` / `store_parse` / `node_not_commissioned` / `device_rejected` / `parse_error` |
| `matd op slow` | info | success that took **≥ 300 ms**. A warm session is normally 71–149 ms, so this is the early sign of a weak link or a degraded mesh. |
| `matd op ok` | debug | ordinary success — not shown at the default level |

Fields: `op`, `node_id` / `group_id`, `endpoint`, `path` (`cluster/attribute`
or `cluster/command`), `elapsed_ms` (the op itself, excluding JSON handling).
Absent fields are omitted rather than printed as `None`. String values are
quoted by the formatter, numbers are not:

```
WARN matd::server: matd op failed op="read" node_id=42 endpoint=1 path="occupancysensing/occupancy" elapsed_ms=8134 kind=Timeout detail=no acknowledgement within MRP retry budget
```

Note that `kind` prints the Rust variant name, not the JSON spelling — the log says `kind=Timeout` where the error object says `"kind": "timeout"`.

Related lines:

- `no warm session; establishing` (info) — a CASE session had to be built for
  this op. Repeatedly for one node means session churn.
- `listen client attached` / `listen client detached` (info) — an event-stream
  client connected or went away. `detached` carries `delivered` and `reason`
  (`client_disconnected` / `channel_closed`).
- `subscription established` / `report pump ended` / `subscription lost;
  resubscribing` (info) — the resident Subscribe lifecycle (see
  [Subscriptions](configuration.md#subscriptions-subscriptionstoml-optional-matd-only)).

`journalctl -p warning` gives you just the degradation.

