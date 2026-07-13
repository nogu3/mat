//! jarvis preflight: Matter BLE advertisement scanner.
//! 実行: cargo run -p mat-controller --features ble --example ble-scan
//! 30 秒スキャンして 0xFFF6 service data を持つデバイスを列挙する。

#[cfg(feature = "ble")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use futures_util::StreamExt;
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    eprintln!("adapter {} — scanning 30s…", adapter.name());
    let mut events = adapter.discover_devices().await?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, events.next()).await {
        let bluer::AdapterEvent::DeviceAdded(addr) = ev else {
            continue;
        };
        let device = adapter.device(addr)?;
        let Ok(Some(sd)) = device.service_data().await else {
            continue;
        };
        if let Some(bytes) = sd.get(&mat_controller::ble::MATTER_BLE_SERVICE) {
            match mat_controller::ble::parse_matter_service_data(bytes) {
                Some(adv) => println!(
                    "{addr}  disc={:#05x}({})  vid={:#06x} pid={:#06x} rssi={:?}",
                    adv.discriminator,
                    adv.discriminator,
                    adv.vendor_id,
                    adv.product_id,
                    device.rssi().await.ok().flatten()
                ),
                None => println!("{addr}  matter service data (unparsed): {bytes:02x?}"),
            }
        }
    }
    Ok(())
}

#[cfg(not(feature = "ble"))]
fn main() {
    eprintln!("build with --features ble");
}
