//! Task M2-2, Step 8: self-closed-loop proof that the bridged endpoint's OnOff
//! cluster is reachable end-to-end — not just through `Node::handle_im`
//! directly (`core::datamodel`'s own tests) or `mat-controller`'s
//! `case_establish.rs` (which drives a hand-rolled responder, not the real
//! `Device`/`Node` wiring) — but through a fully commissioned, secured
//! session against the actual `Device` runtime, exactly the path a real
//! controller (e.g. Alexa via Echo, M2's target) would take.
//!
//! Reuses `self_commission_live.rs`'s direct-drive commissioning setup via
//! the shared `support` module (`tests/support/mod.rs`) rather than
//! duplicating it — see that module's doc comment.
#![cfg(feature = "net")]

use std::net::SocketAddr;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im;

use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config};

const ADMIN_NODE_ID: u64 = 445_566;

/// Commissions against a running `Device` (no mDNS, same direct-drive
/// pattern as `self_commission_live.rs`), then invokes
/// [`support::BRIDGED_EP`] / `CLUSTER_ON_OFF` / `CMD_ON_OFF_TOGGLE` over the
/// resulting secured session and confirms `STATUS_SUCCESS` — proof that
/// `device.rs`'s bridged-endpoint assembly (the `[[device]]` entry's
/// `build_bridged_endpoint` cluster set, under the EP1 Aggregator) is
/// actually reachable through the real commissioning + CASE + secured-IM
/// path, not just `Node::handle_im` called in isolation.
#[tokio::test]
async fn commissioned_session_can_toggle_bridged_endpoint_onoff() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    // Same `[::]` -> `[::1]` substitution as `self_commission_live.rs` (see
    // that file's comment): `local_addr()` is the wildcard bind address,
    // not a valid send destination.
    let addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        device.local_addr().port(),
    );
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der"))
        .expect("device should have written its PAA DER at Device::new");

    let device_task = tokio::spawn(async move {
        let _ = device.run().await;
    });

    let fabric =
        CommissioningFabric::generate(0x2233_4455, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(addr, &paa_der, &fabric).await;

    let resp = session
        .invoke_for_data(
            support::BRIDGED_EP,
            im::CLUSTER_ON_OFF,
            im::CMD_ON_OFF_TOGGLE,
            None,
            None,
            &support::fast_cfg(),
        )
        .await
        .expect("Toggle invoke over the commissioned session should succeed");
    assert_eq!(resp.status, im::STATUS_SUCCESS);

    device_task.abort();
    let _ = device_task.await;
}
