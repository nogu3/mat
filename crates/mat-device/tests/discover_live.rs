//! Live E2E: spawn `MdnsAdvertiser`, then use production
//! `mat_controller::dnssd::browse_commissionable` (the same code path a real
//! controller uses) to find it over the wire. This is the end-to-end proof
//! that Task 11's advertiser answers real mDNS queries — the inline tests in
//! `core::mdns_records` only prove the RR bytes are shaped correctly, never
//! that a socket loop actually replies to a query in flight.
//!
//! Not run in CI: reliable multicast delivery needs a real NIC — a plain
//! loopback interface inside a CI network namespace often filters or drops
//! multicast even with `IPV6_MULTICAST_LOOP` set. Run via `task
//! e2e:device:m1` on real hardware/a real interface.
//!
//! Env: `MAT_E2E_IFACE` selects the interface (defaults to `lo`, useful for
//! a quick local smoke run where multicast loopback happens to work).
#![cfg(feature = "net")]

use std::time::Duration;

use mat_controller::dnssd::{browse_commissionable, iface_index};
use mat_device::core::mdns_records::CommissionableAdvert;
use mat_device::net::mdns::MdnsAdvertiser;

#[tokio::test]
#[ignore = "要: 実 NIC。task e2e:device:m1 で実行"]
async fn advertiser_is_discoverable_via_browse_commissionable() {
    let iface = std::env::var("MAT_E2E_IFACE").unwrap_or_else(|_| "lo".to_string());
    let scope_id = iface_index(&iface).expect("interface index (set MAT_E2E_IFACE?)");

    let advertiser = MdnsAdvertiser::spawn(scope_id)
        .await
        .expect("advertiser socket should bind");
    advertiser
        .set_commissionable(Some(CommissionableAdvert {
            instance: "AA11BB22CC33DD44".to_string(),
            hostname: "MATDEV01".to_string(),
            discriminator: 3840,
            vendor_id: 65521,
            product_id: 32768,
            port: 5540,
            addr_v6: "fe80::1234".parse().unwrap(),
        }))
        .await;

    let found = browse_commissionable(scope_id, Duration::from_secs(3))
        .await
        .expect("browse_commissionable should not error");

    let mine = found
        .iter()
        .find(|c| c.discriminator == Some(3840))
        .expect("should discover our own advertised commissionable instance");
    assert_eq!(mine.hostname.as_deref(), Some("MATDEV01"));
    assert_eq!(mine.port, Some(5540));
    assert_eq!(mine.vendor_id, Some(65521));
    assert_eq!(mine.product_id, Some(32768));
}
