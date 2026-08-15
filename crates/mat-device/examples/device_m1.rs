//! M1 self-commissioning E2E fixture: runs a `Device` with a fixed
//! passcode/discriminator, prints its QR payload and manual code (the e2e
//! script parses the QR line to feed `mat commission --setup-code`), and
//! serves until Ctrl-C.
//!
//! Task 13 swaps this for the real `matv` binary — this example exists
//! only so Task 12's E2E harness (`scripts/e2e-device-m1.sh`) has a real
//! process to drive `mat commission` against before that binary exists.
//!
//! Run: `cargo run -p mat-device --example device_m1`
//!
//! Env:
//! - `MAT_DEVICE_STORE` — fabric/PAA persistence directory (default: a
//!   fresh `mktemp -d`-style tempdir, printed on stdout so the e2e script
//!   can find it for `MAT_PAA_TRUST_STORE`).
//! - `MAT_DEVICE_IFACE` — mDNS/UDP interface name (default: `lo`, which
//!   mDNS can't actually use — see `Device`'s doc comment on why that's
//!   non-fatal; set this to a real NIC for on-network discovery).
//! - `MAT_DEVICE_PORT` — UDP port (default: `5540`, the Matter default).

use std::path::PathBuf;

use mat_device::device::{Device, DeviceConfig};

/// chip-tool's conventional default passcode — used throughout this
/// workspace's own fixtures/tests (`net::pase`, `core::pase`'s tests) and
/// documented in the spec's own example onboarding payloads.
const PASSCODE: u32 = 20_202_021;
/// Matches `discover_live.rs`'s fixture discriminator.
const DISCRIMINATOR: u16 = 3840;
/// Matter test vendor/product range (spec §2.5.2) — same values
/// `core::datamodel`'s `BasicInformationHandler` reports.
const VENDOR_ID: u16 = 0xFFF1;
const PRODUCT_ID: u16 = 0x8000;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let store_dir = match std::env::var_os("MAT_DEVICE_STORE") {
        Some(p) => PathBuf::from(p),
        None => {
            let dir = std::env::temp_dir().join(format!("mat-device-m1-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            dir
        }
    };
    let iface = std::env::var("MAT_DEVICE_IFACE").unwrap_or_else(|_| "lo".to_string());
    let port: u16 = std::env::var("MAT_DEVICE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(mat_controller::message::MATTER_PORT);

    let device = Device::new(DeviceConfig {
        passcode: PASSCODE,
        discriminator: DISCRIMINATOR,
        vendor_id: VENDOR_ID,
        product_id: PRODUCT_ID,
        port,
        store_dir: store_dir.clone(),
        iface,
    })?;

    // Machine-parseable lines the e2e script greps for (kept on their own
    // lines with a stable `key=value` prefix rather than a full JSON blob —
    // this is a throwaway fixture, not `mat`'s own output contract).
    println!("store={}", store_dir.display());
    println!("paa_dir={}", store_dir.join("paa").display());
    println!("addr={}", device.local_addr());
    println!("qr={}", device.qr_payload());
    println!("manual_code={}", device.manual_code());

    tokio::select! {
        result = device.run() => {
            if let Err(e) = result {
                eprintln!("device runtime exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("device_m1: ctrl-c, shutting down");
        }
    }

    Ok(())
}
