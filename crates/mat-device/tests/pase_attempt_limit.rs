//! The commissioning window's brute-force cap, end to end against a running
//! `Device`: 20 failed PASE attempts close the window (the Matter Core
//! Specification's rule for a ~27-bit passcode), the count is per window
//! (a newly opened ECM window starts from zero), and a legitimate
//! commissioning that happens after some failures is unaffected.
//!
//! The failing attempts are real `mat_controller::pase::establish` calls with
//! a wrong passcode. That initiator detects the mismatch on cB (in Pake2)
//! and aborts with a StatusReport instead of sending Pake3 — exactly the
//! "check the guess and walk away" pattern the device must count (see
//! `core::pase::PaseResponderCore::has_unconfirmed_guess`).
#![cfg(feature = "net")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::commissioning::{open_commissioning_window, CommissioningFabric};
use mat_controller::exchange::{ExchangeError, MrpConfig};
use mat_controller::pase::{self, PaseError};
use mat_controller::transport::{Transport, UdpTransport};

mod support;
use support::{
    commission_directly, device_config, fast_cfg, spawn_device, DEVICE_NODE_ID, PASSCODE,
};

/// Must match `mat_device::core::pase::PASE_MAX_FAILED_ATTEMPTS` (kept
/// literal here so a change to the constant has to be a deliberate edit
/// to this test too).
const MAX_FAILED: usize = 20;

/// `CLUSTER_ADMIN_COMMISSIONING`'s `WindowStatus` attribute (spec
/// §11.19.7.1): 0 = closed, 1 = enhanced window open.
const ATTR_AC_WINDOW_STATUS: u32 = 0;

/// Initiator retransmission budget for the attempts. After each failed
/// attempt the device spends a few hundred ms retransmitting its own
/// failure StatusReport to the initiator that already left (`fast_cfg`'s
/// MRP budget), and drops other peers' datagrams meanwhile; a patient
/// initiator's retransmitted PBKDFParamRequest lands after that instead of
/// racing it.
fn patient_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: Duration::from_millis(100),
        active_interval: Duration::from_millis(100),
        max_retries: 10,
        backoff: 1.0,
        jitter: 0.0,
    }
}

async fn pase_attempt(addr: SocketAddr, passcode: u32) -> Result<(), PaseError> {
    let transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));
    pase::establish(transport, addr, passcode, &patient_cfg())
        .await
        .map(|_| ())
}

async fn assert_wrong_passcode_rejected(addr: SocketAddr, passcode: u32, attempt: usize) {
    match pase_attempt(addr, passcode).await {
        Err(PaseError::ConfirmMismatch) => {}
        Ok(()) => panic!("attempt {attempt}: wrong passcode unexpectedly established PASE"),
        Err(e) => panic!("attempt {attempt}: expected ConfirmMismatch (window open), got: {e}"),
    }
}

/// The window is closed: the device answers nothing at all, so even the
/// correct passcode exhausts the MRP budget.
async fn assert_window_closed(addr: SocketAddr, passcode: u32) {
    match pase_attempt(addr, passcode).await {
        Err(PaseError::Exchange(ExchangeError::Timeout)) => {}
        Ok(()) => panic!("PASE succeeded although the window should be closed"),
        Err(e) => panic!("expected silence (MRP timeout) from a closed window, got: {e}"),
    }
}

fn wrong(passcode: u32) -> u32 {
    if passcode == 1 {
        2
    } else {
        passcode - 1
    }
}

#[tokio::test]
async fn twenty_failed_pase_attempts_close_the_boot_window() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let dev = spawn_device(device_config(store_dir.path().to_path_buf()));

    for attempt in 1..=MAX_FAILED {
        assert_wrong_passcode_rejected(dev.addr, wrong(PASSCODE), attempt).await;
    }
    // The 21st attempt — even with the right passcode — is dropped.
    assert_window_closed(dev.addr, PASSCODE).await;

    dev.task.abort();
    let _ = dev.task.await;
}

#[tokio::test]
async fn failures_below_the_cap_do_not_block_commissioning_and_a_new_window_starts_fresh() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let dev = spawn_device(device_config(store_dir.path().to_path_buf()));

    // Boot window: one short of the cap, then a normal commissioning.
    for attempt in 1..MAX_FAILED {
        assert_wrong_passcode_rejected(dev.addr, wrong(PASSCODE), attempt).await;
    }
    // `commission_directly`'s PASE uses the tight `fast_cfg`; let the
    // device finish its last failure StatusReport first (see `patient_cfg`).
    tokio::time::sleep(Duration::from_millis(600)).await;
    let fabric = CommissioningFabric::generate(0x5566_7788, 112_233).expect("fabric generate");
    let mut session = commission_directly(dev.addr, &dev.paa_der, &fabric).await;

    // ECM window opened over the operational CASE session.
    let opened = open_commissioning_window(&mut session, 180, 0x0123, 1000, &fast_cfg())
        .await
        .expect("open commissioning window");
    let status = session
        .read_attribute_json(
            0,
            mat_controller::commissioning::CLUSTER_ADMIN_COMMISSIONING,
            ATTR_AC_WINDOW_STATUS,
            &fast_cfg(),
        )
        .await
        .expect("read WindowStatus");
    assert_eq!(status, serde_json::json!(1), "ECM window should be open");

    // 19 more failures: had the boot window's 19 carried over, the very
    // first of these would have closed the window and the second would
    // time out instead of reaching ConfirmMismatch.
    for attempt in 1..MAX_FAILED {
        assert_wrong_passcode_rejected(dev.addr, wrong(opened.passcode), attempt).await;
    }
    // Still below the cap: the right passcode gets in.
    tokio::time::sleep(Duration::from_millis(600)).await;
    pase_attempt(dev.addr, opened.passcode)
        .await
        .expect("PASE with the ECM passcode after 19 failures should succeed");

    // A success does not reset the count: one more failure is the 20th.
    assert_wrong_passcode_rejected(dev.addr, wrong(opened.passcode), MAX_FAILED).await;
    assert_window_closed(dev.addr, opened.passcode).await;

    // The close went through the same path as expiry: the AC cluster's
    // WindowStatus is back to closed. (The successful PASE above replaced
    // the CASE session, so establish a fresh one to read it.)
    let creds = fabric.admin_credentials().expect("admin credentials");
    let transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));
    let mut session = mat_controller::case::establish(
        transport,
        dev.addr,
        &creds,
        DEVICE_NODE_ID,
        &patient_cfg(),
    )
    .await
    .expect("CASE after the window closed");
    let status = session
        .read_attribute_json(
            0,
            mat_controller::commissioning::CLUSTER_ADMIN_COMMISSIONING,
            ATTR_AC_WINDOW_STATUS,
            &fast_cfg(),
        )
        .await
        .expect("read WindowStatus");
    assert_eq!(
        status,
        serde_json::json!(0),
        "ECM window should be closed after 20 failures"
    );

    dev.task.abort();
    let _ = dev.task.await;
}
