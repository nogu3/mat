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
use mat_device::device::{Device, DeviceConfig};

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

    Ok(cfg)
}

/// Builds the `Device`, prints the setup-payload JSON line, then serves
/// until `Device::run` errors or Ctrl-C is received.
async fn run(cfg: FileConfig) -> Result<(), String> {
    let store = cfg.store.clone();
    let device = Device::new(DeviceConfig {
        passcode: cfg.passcode,
        discriminator: cfg.discriminator,
        vendor_id: cfg.vendor_id,
        product_id: cfg.product_id,
        port: cfg.port,
        store_dir: cfg.store,
        iface: cfg.iface,
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
