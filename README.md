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

## Contributing

Issues and pull requests are welcome. Before sending a PR, run `task check`
(format check + clippy with `-D warnings` + tests); it needs no real devices.
Please keep stdout pure JSON and follow the design rules in
[ARCHITECTURE.md](./ARCHITECTURE.md).

## License

[MIT](./LICENSE).
