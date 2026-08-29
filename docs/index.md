# mat

`mat` is a command-line Matter controller built for scripts and AI agents:
a from-scratch, pure-Rust Matter stack that runs in-process and prints
exactly one JSON object per command on stdout.

This site is the command and configuration reference. For the project
overview, Quickstart, and design background, see the
[GitHub repository](https://github.com/nogu3/mat).

- [Commands](commands.md) — every command with its JSON output
- [Configuration](configuration.md) — the credential store, `aliases.toml`,
  `subscriptions.toml`
- [Errors and exit codes](errors.md) — error schema, `kind` table, exit
  codes, stderr logs
- [Backend](backend.md) — the native backend, interface auto-detection,
  environment variables
- [Development](development.md) — build / test tasks, manual E2E
