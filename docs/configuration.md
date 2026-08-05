# Configuration

## Credential store

Resolution order: `--store <path>` > `$MAT_STORE` > `$XDG_CONFIG_HOME/mat` >
`~/.config/mat`. It holds the Root CA, the controller's keys/cert, the
commissioned-node ledger (`nodes.json`), the optional alias map
(`aliases.toml`, below), and the persistent Matter KVS (chip-tool-compatible
INI form — group keysets, operational credentials, the group-send counter;
`mat` is its sole reader/writer). **It is never committed** (excluded by
`.gitignore`).

## Aliases (`aliases.toml`, optional)

Numeric node / group / endpoint ids are easy to get wrong. If the file
`<store>/aliases.toml` exists, `mat` resolves locally defined names to those
numbers right after arg parsing — a purely local convenience. **Without the
file, behavior is exactly the traditional numeric-only one.** The wire and the
backend (native engine / `matd`) always receive numbers, and stdout keeps the
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

[thread]
"AABBCCDDEEFF0011" = "otbr-br"
```

- `nodes`: alias → node_id. Accepted by `-n/--node` (read / write / invoke /
  describe / on / off / color-temp / color / level / open-window / diag thread / diag node) and
  by `--nodes` in `group provision` / `diag mesh` (each element resolved independently).
- `groups`: alias → GroupId. Accepted by `-g/--group` in `group provision` /
  `invoke` / `grant` / `color-temp` / `color` / `level` (`bump` takes none —
  the counter is fabric-global).
- `endpoints`: defined **per node** — the outer key is a node alias or a
  node_id digit string, the inner map is alias → endpoint number (endpoint
  numbers mean different things on different nodes, so there is no global
  endpoint dictionary). Accepted by `-e/--endpoint` on node-taking commands;
  the lookup uses the *resolved* node, so `-n 5 -e main` and
  `-n living-light -e main` give the same result. The `-e` of `group
  provision` / `group invoke` / `group color-temp` / `group color` / `group
  level` is **numeric only** (no node context to resolve against).
- `colors`: custom color name → RGB value (`#rrggbb` / `rrggbb` / `R,G,B`),
  used by `--name` in `color` / `group color`. Entries are defined as RGB and
  go through the same RGB → HSV pipeline as `--rgb`. A user-defined name
  **overrides** the built-in color table (you can redefine `red`). Without the
  file the built-in table still works. A value that does not parse as RGB is
  `store_parse` (exit `10`); an unknown color name is a CLI argument error
  (exit `2`) listing the known names.
- `thread`: Thread ExtAddress (16 hex, case-insensitive) → display label, used
  by `mat diag mesh` to name unknown participants (OTBR border routers, other-
  fabric devices) that show up in a neighbor/route table but were never
  commissioned onto this fabric, so they have no `nodes` alias to fall back
  on. The graph's `label` field matches on ExtAddress regardless of fabric
  status, so a commissioned node whose ExtAddress happens to be listed here
  gets a `label` too, alongside its `nodes` `alias`.

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
[Errors and exit codes](errors.md#errors-and-exit-codes) table is unchanged.

To register an alias while commissioning, add `--alias`:

```bash
mat commission --setup-code "MT:Y.K9042C00KA0648G00" --node 5 --alias living-light
```

The name is validated **before** commissioning starts (all-digits / empty /
already taken → exit `2`, before any network op runs), and it is written
to `aliases.toml` only on success (the file is created if absent). Without
`--alias`, `commission` never touches `aliases.toml`. Deleting or renaming an
alias is a hand edit of the file — there is no management subcommand.

## Subscriptions (`subscriptions.toml`, optional, matd only)

By default `matd`'s resident Subscribe (see [Resident Subscribe and `mat
listen`](commands.md#resident-subscribe-and-mat-listen)) is a full **wildcard**: every
endpoint/cluster/attribute, on every commissioned node. If
`<store>/subscriptions.toml` exists, `matd` narrows that to just the listed
clusters' paths instead.

Full-wildcard priming (the initial full-attribute dump right after a
subscription is (re)established — dozens of request/response round trips) can
fail to complete on a weak Thread link, leaving a subscription unestablished
for tens of minutes to hours. Narrowing to a handful of clusters shrinks
priming to one or two chunks, so a link good enough for `read` is usually
good enough to subscribe on too.

```toml
clusters = [
  "onoff",
  "occupancysensing",
  "temperaturemeasurement",
]
```

- Cluster names use chip-tool notation (same as `mat read`); numeric ids
  (`"0x0006"` / `"6"`) also work, the same escape hatch as elsewhere for names
  `mat-core::ids` doesn't know.
- **Absent file = full wildcard, unchanged** — the same absent-file discipline
  as `aliases.toml`.
- A parse failure, an unknown cluster name, or an empty list makes `matd`
  **refuse to start** (`store_parse`, exit `10`); it never silently falls back
  to wildcard, so a misconfiguration can't quietly disable the weak-link
  workaround.
- **Edge case: nodes that serve none of the listed clusters.** When the file is
  present, the narrowed Subscribe is sent to every commissioned node; a node
  that exposes none of the listed clusters will never establish its subscription
  (it retries on backoff forever). Ensure each node serves at least one of the
  listed clusters.
- When this file is present, `mat listen` only ever sees events for the
  listed clusters — a `--cluster` filter naming a cluster outside that set
  simply never matches anything.
- Read once at `matd` startup; an edit needs a `matd` restart to take effect
  (e.g. `systemctl --user restart matd`).
- `mat` (one-shot) never reads this file — like the rest of the resident
  Subscribe, it is matd-only.

