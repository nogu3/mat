# mat

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

`mat` is a CLI for controlling Matter devices. It drives a **from-scratch native
Matter controller** (crate `mat-controller`, in this workspace) in-process and
returns **pure structured JSON**, normalized to `mat`'s own schema.

- stdout = one JSON object per command. No human decoration.
- diagnostics go to stderr as structured logs (`tracing`).
- it holds no state except the credential KVS (the process is one-shot).

For the design background, the `mat` / `matd` split, and what `mat` does and
does not do, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Why mat

- **Pure JSON in, pure JSON out** — one object per command on stdout; stable
  error `kind`s and exit codes. Built to be driven by scripts and AI agents.
- **Native pure-Rust controller** — TLV, CASE, IM, groupcast, mDNS, and
  commissioning (on-network and BLE+Thread) run in-process. No external
  controller subprocess.
- **One-shot CLI, optional resident daemon** — `mat` holds no state except the
  credential store; `matd` adds warm sessions and a resident Subscribe
  (`mat listen`).
- **Optional alias layer** — human names live in a local `aliases.toml` only;
  the wire stays numeric.

## Quickstart

```bash
# build & install -> ~/.cargo/bin/{mat,matd}
task install

# create your first fabric (writes the credential store; no network I/O)
mat fabric init

# commission a device (all values here are dummy)
mat commission --setup-code "MT:Y.K9042C00KA0648G00" --node 5

# control and read it
mat on --node 5
mat read --node 5 --cluster onoff --attribute on-off
```

Every command prints exactly one JSON object on stdout:

```json
// mat commission
{ "timestamp": "2026-06-06T12:34:56+09:00", "node_id": 5, "status": "success" }

// mat on — control is an invoke of the OnOff cluster's on command
{ "timestamp": "2026-06-06T12:34:57+09:00", "node_id": 5, "endpoint": 1, "cluster": "onoff", "command": "on", "status": "success" }

// mat read — the attribute's TLV value, normalized
{ "timestamp": "2026-06-06T12:34:58+09:00", "node_id": 5, "endpoint": 1, "cluster": "onoff", "attribute": "on-off", "value": true }
```

Errors are structured the same way (stderr, stable `kind` + exit code — see
[Errors and exit codes](./docs/errors.md)).

## Requirements

- Rust (stable) and [Task](https://taskfile.dev) to build. No external Matter
  controller is needed — the backend is native and pure Rust.
- Matter uses mDNS / IPv6 multicast, so on a real network the host must be able
  to send and receive these. `mat` auto-detects the network interface (override
  with `MAT_IFACE`; see [Backend](./docs/backend.md)).
- BLE commissioning (BLE+Thread) is an opt-in `ble` cargo feature (pulls in
  `libdbus`); the default build and local `task check` do not need it. Deploy
  builds enable it — see [Backend](./docs/backend.md).

## Install

```bash
task build      # release build -> target/release/{mat,matd}
task install    # install both binaries into ~/.cargo/bin
```

## Documentation

The full reference lives at **<https://nogu3.github.io/mat/>** (also browsable
in [`docs/`](./docs/)):

| Page | Contents |
|---|---|
| [Commands](./docs/commands.md) | Every command with its JSON output: discover / commissioning, state operations, diagnostics, listen, multi-admin share, groupcast, routing through `matd` |
| [Configuration](./docs/configuration.md) | The credential store, `aliases.toml`, `subscriptions.toml` |
| [Errors and exit codes](./docs/errors.md) | Error schema, `kind` table, exit codes, stderr logs |
| [Backend](./docs/backend.md) | The native backend, interface auto-detection, environment variables |
| [Development](./docs/development.md) | Build / test tasks, manual E2E with real devices |

For the design background and roadmap, see
[ARCHITECTURE.md](./ARCHITECTURE.md).

## Status

Everything documented is implemented on the native backend and passes the
fake-connection / binary integration tests; real-device E2E has confirmed the
full op sweep runs natively with no fallback. Group *delivery* is
unacknowledged multicast by design, so per-device actuation cannot be
confirmed from the controller side (see
[Groupcast](./docs/commands.md#groupcast)).

## Contributing

Issues and pull requests are welcome. Before sending a PR, run `task check`
(format check + clippy with `-D warnings` + tests); it needs no real devices.
Please keep stdout pure JSON and follow the design rules in
[ARCHITECTURE.md](./ARCHITECTURE.md).

## License

[MIT](./LICENSE).
