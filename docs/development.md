# Development

Tasks are defined with [Task](https://taskfile.dev) (`task` lists them).

```bash
task build            # release build -> target/release/{mat,matd}
task install          # install both binaries into ~/.cargo/bin
task run -- discover  # run (native backend)
task test             # tests (native FakeConn + binary integration; no real devices)
task clippy           # lint (-D warnings)
task fmt              # format
task check            # CI equivalent (fmt:check + clippy + test)

task dist:arm64       # aarch64-gnu + BLE deploy build -> dist/arm64/{mat,matd}
task docker:build     # slim x86_64 image (mat/matd only)
task docker:run -- discover
task docker:test      # no local toolchain needed
```

CI (GitHub Actions, `.github/workflows/ci.yml`) runs the same fmt / clippy /
test sequence as `task check`. The default build and CI do not use the `ble`
cargo feature; deploy builds (`task dist:arm64`) enable it for BLE+Thread
commissioning.

## Manual E2E (with real devices; not in CI)

In practice the main path is **multi-admin join**: adding a device that is
already commissioned by another admin (such as Home Assistant) to `mat` as well.
The printed code does not work (the device left commissioning mode), so the
existing admin opens a commissioning window to issue a one-time code.

1. **Share from the other admin:** on the other controller, run "Share" for the
   target device and note the issued setup code (`MT:...` or 11-digit).
2. **Join with `mat`:**
   ```bash
   mat commission --setup-code "<issued setup code>" --node 5
   ```
   It returns `{ "node_id": 5, "status": "success" }` and records the ledger in
   `~/.config/mat/nodes.json`.
3. **Confirm:** `mat discover` now shows node 5 with `"state": "commissioned"`.

> For a factory-reset device, pass the printed setup code directly to
> `commission` (first commission).

### State operations E2E

Against a commissioned node (node 5 above), confirm read / describe / on / off
on a real device.

```bash
# Introspect what you can call (endpoints and numeric cluster IDs)
mat describe --node 5

# Read the OnOff attribute (for a light, its current on/off state)
mat read --node 5 --cluster onoff --attribute on-off

# Turn on -> off (invoke of the OnOff command, not an attribute write)
mat on --node 5
mat off --node 5

# Read-after-write check (confirm the value took effect)
mat on --node 5 && mat read --node 5 --cluster onoff --attribute on-off   # -> "value": true
```

### Share E2E (mat -> another admin)

Share `mat`-owned node 5 with another controller.

```bash
# Open a commissioning window (get the issued code)
mat open-window --node 5 --timeout 300
# -> { "node_id": 5, "manual_code": "...", "qr_payload": "MT:...", "expires_at": "..." }
```

Enter the returned `manual_code` (11-digit) or `qr_payload` (render the QR with
the receiving tool) into the other controller's "Add device" flow (Alexa / Apple
Home / Google Home). Finish before `expires_at`. After sharing, `mat` keeps its
fabric membership (multi-admin).

> Each one-shot run pays mDNS resolution plus a CASE handshake, so a single call
> is slow (hundreds of ms to seconds). Speed-sensitive use cases run `matd`,
> which keeps warm sessions (see ARCHITECTURE.md).

### Groupcast E2E (real devices)

With several commissioned lights (say nodes 5, 6, 7), burn a wire group and fire
one multicast send at it.

```bash
# Provision the group onto every node (controller-side state is set up too)
mat group provision --group 1 --nodes 5 6 7 --name living
# -> { "group_id": 1, "keyset_id": 42, "nodes": [5,6,7], "status": "provisioned", ... }

# One multicast send — all three should react together (no popcorn effect)
mat group invoke --group 1 --cluster onoff --command on
mat group invoke --group 1 --cluster onoff --command off
```

> Groupcast is **unacknowledged**, so `group invoke` only confirms the send, not
> delivery. If a light did not react, confirm it individually (`mat read --node 6 -c
> onoff --attribute on-off`) and re-provision that node. Multicast is **especially weak on
> Thread**; Wi-Fi / Ethernet lights are more reliable. The KVS records `mat`
> writes (keyset table, group table, GroupKeyMap) follow the connectedhomeip
> v1.4.2.0 `GroupDataProviderImpl` link discipline, so a real `chip-tool` on the
> same store can still read them — if a devices-side provisioning step regresses,
> the group-settings writer is the first place to check.
>
> If **no** device reacts although provision reported success, suspect the
> device ACL first: provisions made before the ACL step (or through an old
> `matd` ≤ 0.12) never granted the group permission, and devices silently drop
> unauthorized groupcast. `mat group grant --group 1 --nodes 5 6 7` adds the
> missing entries idempotently.

