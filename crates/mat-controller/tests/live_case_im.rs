//! Live E2E: self-issue an operational identity from chip-tool's persisted root
//! CA key, then CASE + IM against a commissioned chip-all-clusters-app.
//! Run via `task e2e:m2`. Not in CI. Requires MAT_E2E_KVS_DIR / MAT_E2E_NODE_ID.

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::FabricCredentials;
use mat_controller::im::{ImValue, ATTR_ON_OFF, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE};
use mat_controller::message::MATTER_PORT;
use mat_controller::transport::{Transport, UdpTransport};
use mat_controller::{case, kvs};
use std::path::PathBuf;

fn env_node_id() -> u64 {
    let s = std::env::var("MAT_E2E_NODE_ID").expect("MAT_E2E_NODE_ID required");
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).expect("hex node id"),
        None => s.parse().expect("decimal node id"),
    }
}

#[tokio::test]
#[ignore = "requires a commissioned device + chip-tool KVS (task e2e:m2)"]
async fn self_issued_case_read_toggle_read() {
    let dir = PathBuf::from(std::env::var("MAT_E2E_KVS_DIR").expect("MAT_E2E_KVS_DIR required"));
    let device_node_id = env_node_id();
    let peer = std::env::var("MAT_E2E_PEER")
        .unwrap_or_else(|_| format!("[::1]:{MATTER_PORT}"))
        .parse()
        .expect("socket addr");

    // 受け入れ 2: KVS から CA 材料
    let materials = kvs::read_self_issue_materials(
        &dir.join("chip_tool_config.alpha.ini"),
        &dir.join("chip_tool_config.ini"),
        1,
        0,
    )
    .expect("read CA materials");
    eprintln!(
        "controller node id 0x{:016X}, fabric id {}",
        materials.node_id, materials.fabric_id
    );

    // 受け入れ 3: 自前 NOC 自己発行
    let creds = FabricCredentials::from_self_issued(materials).expect("self-issue NOC");

    // 受け入れ 4: CASE 確立（我々の自己発行 NOC を実機が受理）
    let transport = std::sync::Arc::new(Transport::Udp(std::sync::Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));
    let cfg = MrpConfig::default();
    let mut session = case::establish(
        std::sync::Arc::clone(&transport),
        peer,
        &creds,
        device_node_id,
        &cfg,
    )
    .await
    .expect("CASE establishment");
    eprintln!(
        "CASE established with device 0x{:016X}",
        session.peer_node_id()
    );

    // 受け入れ 5/6: read → toggle → read（admin 権限で通る = ACL 継承の実証）
    let before = session
        .read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg)
        .await
        .expect("read");
    let ImValue::Bool(before) = before else {
        panic!("on-off not bool: {before:?}")
    };
    let outcome = session
        .invoke(1, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, &cfg)
        .await
        .expect("toggle");
    assert_eq!(outcome.status, 0);
    let after = session
        .read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg)
        .await
        .expect("read2");
    assert_eq!(after, ImValue::Bool(!before), "toggle must flip on-off");
    eprintln!("on-off {before} -> {after:?}");
}
