//! `matv` — virtual Matter device host CLI (M1: single self-contained node).
//!
//! Thin wrapper around `mat_device::device::{Device, DeviceConfig}`: parse
//! `--config <matv.toml>`, validate the passcode/discriminator, build the
//! `Device` (binds the UDP socket + generates/loads persistent state
//! synchronously), print one JSON line to stdout (mat 流儀: stdout=JSON,
//! ログ=stderr — see `matd`'s own main.rs for the same convention), then
//! serve forever until Ctrl-C.
//!
//! Task 13 replaces `mat-device`'s throwaway `examples/device_m1.rs` fixture
//! (Task 12's stand-in for `scripts/e2e-device-m1.sh`) with this real binary.

use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

use mat_controller::commissioning::INVALID_PASSCODES;
use mat_device::core::bridge::DeviceKind;
use mat_device::device::{AttestationMode, Device, DeviceConfig, VirtualDeviceConfig};

/// spec §5.1.3.1: valid setup passcode range is `1..=99_999_998` (0 and
/// `0x5F5E0FF`=99_999_999 are reserved), on top of the trivial/attack-prone
/// values in [`INVALID_PASSCODES`].
const MAX_PASSCODE: u32 = 99_999_998;
/// 12-bit onboarding discriminator upper bound (spec §5.1.4.2). Checked
/// here (rather than left to `Device::qr_payload`) because
/// `mat_controller::setup_code::encode_qr` enforces this with an `assert!`
/// (a panic, not a `Result`) — matv validates up front so an out-of-range
/// value is a clean stderr error + non-zero exit instead of a panic.
const MAX_DISCRIMINATOR: u16 = 0x0FFF;
/// `[[device]]` の `name` 上限。Bridged Device Basic Information の
/// NodeLabel は spec §11.1.6.2 の `string32` — バイト数ではなく**文字数**
/// で 32 まで。
const MAX_DEVICE_NAME_CHARS: usize = 32;

/// matv — virtual Matter device host (M1: single self-contained node).
#[derive(Parser, Debug)]
#[command(name = "matv", version)]
struct Cli {
    /// Device config TOML path (see `FileConfig` for the schema).
    #[arg(long)]
    config: PathBuf,
}

/// `matv.toml` schema (M1: all fields required, no defaults). Mirrors
/// `mat_device::device::DeviceConfig` field-for-field, plus `iface`
/// (Task 12 addition — not in the M1 plan's illustrative TOML sketch: a
/// multi-NIC host has no correct interface to auto-detect, same reasoning
/// as `mat-native::iface_select`'s own refusal to guess, so this is left
/// mandatory rather than defaulted to something like `"lo"`).
#[derive(Debug, Deserialize)]
struct FileConfig {
    passcode: u32,
    discriminator: u16,
    vendor_id: u16,
    product_id: u16,
    /// UDP port to bind. `0` = ephemeral (tests / multi-instance hosts).
    port: u16,
    store: PathBuf,
    /// mDNS/UDP egress interface name (e.g. `"eth0"`).
    iface: String,
    /// Dev attestation chain to use: `"self"` (default — fresh
    /// self-generated chain every boot, unchanged M1/M2 behavior) or
    /// `"chip-test"` (Task 10's Echo experiment: the vendored canonical
    /// connectedhomeip test chain — see `mat_device::device::AttestationMode`
    /// and `mat_device::chip_test_attestation`'s module docs). Optional,
    /// defaults to `"self"` so existing `matv.toml` files (and the
    /// chip-tool e2e gates) are unaffected.
    #[serde(default)]
    attestation: AttestationMode,
    /// M3: bridge がぶら下げるデバイス群（`[[device]]` の配列）。宣言順が
    /// そのまま endpoint 採番順になる。`serde(default)` は「未宣言 = 空
    /// ベクタ」をパースエラーではなく `load_config` のバリデーション
    /// エラー（"config must declare at least one [[device]]"）にするため
    /// — TOML の missing-field メッセージより設定者に伝わる。
    #[serde(default, rename = "device")]
    devices: Vec<FileDeviceConfig>,
}

/// `matv.toml` の `[[device]]` 1 件。
#[derive(Debug, Deserialize)]
struct FileDeviceConfig {
    /// endpoint 採番台帳のキー。一度使ったら改名しない（改名すると
    /// コントローラ側では別アクセサリの新規追加として見える）。
    id: String,
    /// デバイス種別。綴りは `mat_device::core::bridge::DeviceKind` の
    /// serde rename が正本（未知の綴りは serde が弾く）。
    kind: DeviceKind,
    /// Bridged Device Basic Information の NodeLabel。
    name: String,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        // stdout is reserved for the single JSON setup-payload line (mat
        // 流儀) — logs always go to stderr, matd's main.rs does the same.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let file_cfg = match load_config(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("matv: {e}");
            std::process::exit(1);
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("matv: failed to start tokio runtime: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = runtime.block_on(run(file_cfg)) {
        eprintln!("matv: {e}");
        std::process::exit(1);
    }
}

/// Reads and validates `matv.toml`. Kept synchronous (and separate from
/// `run`) so `main` can exit non-zero on a config error before ever
/// starting a tokio runtime.
fn load_config(path: &std::path::Path) -> Result<FileConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    let cfg: FileConfig = toml::from_str(&text)
        .map_err(|e| format!("failed to parse config {}: {e}", path.display()))?;

    if cfg.passcode == 0 || cfg.passcode > MAX_PASSCODE {
        return Err(format!(
            "passcode must be in 1..={MAX_PASSCODE}, got {}",
            cfg.passcode
        ));
    }
    if INVALID_PASSCODES.contains(&cfg.passcode) {
        return Err(format!(
            "passcode {} is a trivial/attack-prone value disallowed by spec §5.1.3.1",
            cfg.passcode
        ));
    }
    if cfg.discriminator > MAX_DISCRIMINATOR {
        return Err(format!(
            "discriminator must fit in 12 bits (<= {MAX_DISCRIMINATOR}), got {}",
            cfg.discriminator
        ));
    }

    // `[[device]]` 群（M3）。`Device::new` は渡されたものをそのまま組む
    // だけなので、設定ファイルの妥当性はここで全部見る。
    if cfg.devices.is_empty() {
        return Err("config must declare at least one [[device]]".to_string());
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for device in &cfg.devices {
        if device.id.is_empty() {
            return Err("[[device]] id must not be empty".to_string());
        }
        if !seen.insert(device.id.as_str()) {
            return Err(format!("duplicate [[device]] id {:?}", device.id));
        }
        let chars = device.name.chars().count();
        if chars > MAX_DEVICE_NAME_CHARS {
            return Err(format!(
                "[[device]] {:?} name must be at most {MAX_DEVICE_NAME_CHARS} characters, got {chars}",
                device.id
            ));
        }
    }

    Ok(cfg)
}

/// Builds the `Device`, prints the setup-payload JSON line, then serves
/// until `Device::run` errors or Ctrl-C is received.
async fn run(cfg: FileConfig) -> Result<(), String> {
    let store = cfg.store.clone();
    let devices = cfg
        .devices
        .into_iter()
        .map(|d| VirtualDeviceConfig {
            id: d.id,
            kind: d.kind,
            name: d.name,
        })
        .collect();
    let device = Device::new(DeviceConfig {
        passcode: cfg.passcode,
        discriminator: cfg.discriminator,
        vendor_id: cfg.vendor_id,
        product_id: cfg.product_id,
        port: cfg.port,
        store_dir: cfg.store,
        iface: cfg.iface,
        attestation: cfg.attestation,
        devices,
    })
    .map_err(|e| format!("failed to start device: {e}"))?;

    let payload = serde_json::json!({
        "qr_payload": device.qr_payload(),
        "manual_code": device.manual_code(),
        // Resolves `port == 0` (ephemeral) to the OS-assigned port — the
        // config's literal value would just report back 0 in that case.
        "port": device.local_addr().port(),
        "store": store.display().to_string(),
    });
    println!("{payload}");

    tokio::select! {
        result = device.run() => {
            result.map_err(|e| format!("device runtime exited with error: {e}"))
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("matv: ctrl-c, shutting down");
            Ok(())
        }
    }
}

/// Unit tests for `load_config`'s validation logic — pure function, no
/// process spawn needed (the spawn-based `tests/cli.rs` only exercises the
/// happy path end-to-end; these cover each rejection reason directly, per
/// review round 1: "no automated tests for the config-validation error
/// paths").
#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `body` as `matv.toml` under a fresh tempdir and returns its
    /// path (dir kept alive by the caller holding the `TempDir`).
    fn write_config(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("matv.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// The scalar half of a valid `matv.toml` — everything that must come
    /// *before* the `[[device]]` array-of-tables (TOML puts every following
    /// key inside the last table header, so top-level keys go first).
    const VALID_BASE: &str = "passcode = 20202021\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\nstore = \"x\"\niface = \"lo\"\n";

    /// The standard e2e `[[device]]` block — the same id/kind/name
    /// `mat-device`'s integration tests and `scripts/e2e-*` use.
    const DEVICE_BLOCK: &str =
        "\n[[device]]\nid = \"e2e-light\"\nkind = \"onoff-light\"\nname = \"E2E Light\"\n";

    /// `VALID_BASE` + any extra top-level scalar lines + one valid
    /// `[[device]]`, in the order TOML requires.
    fn valid_config(extra_scalars: &str) -> String {
        format!("{VALID_BASE}{extra_scalars}{DEVICE_BLOCK}")
    }

    #[test]
    fn accepts_a_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &valid_config(""));
        let cfg = load_config(&path).expect("valid config should load");
        assert_eq!(cfg.passcode, 20_202_021);
        assert_eq!(cfg.discriminator, 3840);
        assert_eq!(cfg.iface, "lo");
    }

    /// A pure bridge with nothing to bridge is a config mistake, not a
    /// degenerate-but-valid device — `[[device]]` is what `matv` exists to
    /// serve, so an empty list is rejected up front rather than booting an
    /// Aggregator with an empty PartsList.
    #[test]
    fn rejects_config_with_no_devices() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), VALID_BASE);
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("[[device]]"),
            "error should point at the missing [[device]] section: {err}"
        );
    }

    /// The device id is the endpoint ledger's key — two entries sharing one
    /// id would map two accessories onto a single endpoint.
    #[test]
    fn rejects_duplicate_device_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            &format!(
                "{VALID_BASE}{DEVICE_BLOCK}\n[[device]]\nid = \"e2e-light\"\nkind = \"onoff-light\"\nname = \"Another\"\n"
            ),
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("e2e-light"),
            "error should name the duplicated id: {err}"
        );
        assert!(
            err.contains("duplicate"),
            "error should say what's wrong: {err}"
        );
    }

    #[test]
    fn rejects_empty_device_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            &format!("{VALID_BASE}\n[[device]]\nid = \"\"\nkind = \"onoff-light\"\nname = \"X\"\n"),
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("id"),
            "error should point at the empty id: {err}"
        );
    }

    /// `name` becomes the Bridged Device Basic Information NodeLabel, a
    /// spec `string32` (§11.1.6.2) — 33 characters is over the line.
    #[test]
    fn rejects_device_name_over_32_chars() {
        let dir = tempfile::tempdir().unwrap();
        let name = "x".repeat(33);
        let path = write_config(
            dir.path(),
            &format!(
                "{VALID_BASE}\n[[device]]\nid = \"a\"\nkind = \"onoff-light\"\nname = \"{name}\"\n"
            ),
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("32"),
            "error should mention the 32-character limit: {err}"
        );
    }

    /// Exactly 32 characters is still fine (the boundary is inclusive), and
    /// the count is in characters, not bytes.
    #[test]
    fn accepts_a_32_char_multibyte_device_name() {
        let dir = tempfile::tempdir().unwrap();
        let name = "あ".repeat(32);
        let path = write_config(
            dir.path(),
            &format!(
                "{VALID_BASE}\n[[device]]\nid = \"a\"\nkind = \"onoff-light\"\nname = \"{name}\"\n"
            ),
        );
        let cfg = load_config(&path).expect("a 32-character name should be accepted");
        assert_eq!(cfg.devices[0].name, name);
    }

    #[test]
    fn accepts_several_devices_and_keeps_declaration_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            &format!(
                "{VALID_BASE}\n[[device]]\nid = \"a\"\nkind = \"onoff-light\"\nname = \"A\"\n\n[[device]]\nid = \"b\"\nkind = \"onoff-light\"\nname = \"B\"\n\n[[device]]\nid = \"c\"\nkind = \"onoff-light\"\nname = \"C\"\n"
            ),
        );
        let cfg = load_config(&path).expect("valid config should load");
        assert_eq!(cfg.devices.len(), 3);
        let ids: Vec<&str> = cfg.devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(cfg.devices[0].kind, DeviceKind::OnOffLight);
    }

    #[test]
    fn rejects_unknown_device_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            &format!("{VALID_BASE}\n[[device]]\nid = \"a\"\nkind = \"toaster\"\nname = \"A\"\n"),
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("toaster"),
            "error should name the unknown kind: {err}"
        );
    }

    /// `attestation` is optional (Task 10) — an existing `matv.toml` with
    /// no `attestation` line (like `VALID_BASE`) must still load, and must
    /// default to `Self_` (unchanged behavior for chip-tool e2e gates).
    #[test]
    fn attestation_defaults_to_self_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &valid_config(""));
        let cfg = load_config(&path).expect("valid config should load");
        assert_eq!(cfg.attestation, AttestationMode::Self_);
    }

    /// `attestation = "chip-test"` parses to `AttestationMode::ChipTest`.
    #[test]
    fn attestation_parses_chip_test() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &valid_config("attestation = \"chip-test\"\n"));
        let cfg = load_config(&path).expect("valid config should load");
        assert_eq!(cfg.attestation, AttestationMode::ChipTest);
    }

    /// `attestation = "self"` parses explicitly too (not just via omission).
    #[test]
    fn attestation_parses_self() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &valid_config("attestation = \"self\"\n"));
        let cfg = load_config(&path).expect("valid config should load");
        assert_eq!(cfg.attestation, AttestationMode::Self_);
    }

    #[test]
    fn rejects_passcode_in_deny_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "passcode = 11111111\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\nstore = \"x\"\niface = \"lo\"\n",
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("11111111"),
            "error should mention the offending passcode: {err}"
        );
        assert!(
            err.contains("disallowed"),
            "error should explain why it's rejected: {err}"
        );
    }

    #[test]
    fn rejects_discriminator_at_or_above_4096() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "passcode = 20202021\ndiscriminator = 4096\nvendor_id = 65521\nproduct_id = 32768\nport = 0\nstore = \"x\"\niface = \"lo\"\n",
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("4096"),
            "error should mention the offending discriminator: {err}"
        );
        assert!(
            err.contains("12 bits"),
            "error should explain the 12-bit limit: {err}"
        );
    }

    #[test]
    fn rejects_passcode_out_of_valid_range() {
        let dir = tempfile::tempdir().unwrap();
        // 100_000_000 is out of range but not itself in INVALID_PASSCODES —
        // isolates the range check from the deny-list check (which runs
        // second in `load_config`).
        let path = write_config(
            dir.path(),
            "passcode = 100000000\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\nstore = \"x\"\niface = \"lo\"\n",
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("100000000"),
            "error should mention the offending passcode: {err}"
        );
        assert!(
            err.contains("1..="),
            "error should mention the valid range: {err}"
        );
    }

    #[test]
    fn rejects_config_missing_required_field() {
        let dir = tempfile::tempdir().unwrap();
        // `iface` omitted entirely (the Task 12 addition not in the brief's
        // illustrative TOML — deliberately mandatory, see `FileConfig`'s
        // doc comment).
        let path = write_config(
            dir.path(),
            "passcode = 20202021\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\nstore = \"x\"\n",
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("iface"),
            "error should mention the missing field: {err}"
        );
    }

    #[test]
    fn rejects_unreadable_config_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let err = load_config(&missing).unwrap_err();
        assert!(
            err.contains("does-not-exist.toml"),
            "error should mention the offending path: {err}"
        );
    }
}
