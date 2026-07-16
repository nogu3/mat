//! M6a live E2E: 素の all-clusters-app に対する native commissioning。
//! 実行は scripts/e2e-m6.sh 経由（app 起動と PAA 取得を行う）。
//! 必須 env: MAT_E2E_PEER=[::1]:5540, MAT_E2E_PAA_DIR=<test PAA dir>
//! 任意 env: MAT_E2E_PASSCODE (既定 20202021)

use std::sync::Arc;

use mat_controller::commissioning::{
    self, CommissionError, CommissionParams, CommissionTarget, CommissioningFabric,
};
use mat_controller::exchange::MrpConfig;
use mat_controller::im::{ImValue, ATTR_ON_OFF, CLUSTER_ON_OFF, CMD_ON_OFF_ON};
use mat_controller::transport::UdpTransport;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

#[tokio::test]
#[ignore = "live: requires all-clusters-app (scripts/e2e-m6.sh)"]
async fn commission_control_multi_admin() {
    let peer: std::net::SocketAddr = env("MAT_E2E_PEER").expect("MAT_E2E_PEER").parse().unwrap();
    let paa_dir = std::path::PathBuf::from(env("MAT_E2E_PAA_DIR").expect("MAT_E2E_PAA_DIR"));
    let passcode: u32 = env("MAT_E2E_PASSCODE").map_or(20202021, |v| v.parse().unwrap());
    let cfg = MrpConfig::default();

    // ① 誤 passcode → Pase エラー（fresh device の window は失敗では閉じない）
    let transport = Arc::new(UdpTransport::bind().await.unwrap());
    let fab_a = CommissioningFabric::generate(0xFAB1, 0x1_0001).unwrap();
    // `CommissionedDevice`（Ok 側）は `Debug` を実装していない（`SecureSession`
    // を保持するため）ので `unwrap_err()` は使えない — `match` で取り出す。
    let result = commissioning::commission_on_network(
        Arc::clone(&transport),
        &fab_a,
        CommissionParams {
            passcode: passcode + 1,
            target: CommissionTarget::Addr(peer),
            device_node_id: 0x2_0001,
            paa_dir: Some(&paa_dir),
            cd_signer_dir: None,
            scope_id: 0,
        },
    )
    .await;
    let err = match result {
        Ok(_) => panic!("expected commissioning to fail with wrong passcode"),
        Err(e) => e,
    };
    assert!(
        matches!(err, CommissionError::Pase(_)),
        "expected Pase error, got {err}"
    );

    // ② 正しい passcode で native commission（初回 commissioner）
    let dev = commissioning::commission_on_network(
        Arc::clone(&transport),
        &fab_a,
        CommissionParams {
            passcode,
            target: CommissionTarget::Addr(peer),
            device_node_id: 0x2_0001,
            paa_dir: Some(&paa_dir),
            cd_signer_dir: None,
            scope_id: 0,
        },
    )
    .await
    .expect("commissioning A");
    let mut session = dev.session;

    // ③ 新 fabric で制御: on → read on-off == true
    session
        .invoke(1, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None, &cfg)
        .await
        .unwrap();
    let v = session
        .read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg)
        .await
        .unwrap();
    assert_eq!(v, ImValue::Bool(true));

    // ④ native open-window → 第二 admin (fabric B) として commission
    let window = commissioning::open_commissioning_window(
        &mut session,
        180,
        commissioning::random_discriminator(),
        1000,
        &cfg,
    )
    .await
    .expect("open window");
    eprintln!(
        "window: manual={} qr={}",
        window.manual_code, window.qr_payload
    );
    let fab_b = CommissioningFabric::generate(0xFAB2, 0x1_0002).unwrap();
    let dev_b = commissioning::commission_on_network(
        Arc::clone(&transport),
        &fab_b,
        CommissionParams {
            passcode: window.passcode,
            target: CommissionTarget::Addr(peer),
            device_node_id: 0x2_0002,
            paa_dir: Some(&paa_dir),
            cd_signer_dir: None,
            scope_id: 0,
        },
    )
    .await
    .expect("commissioning B (multi-admin)");
    let mut session_b = dev_b.session;

    // ⑤ fabric B からも制御でき、B を RemoveFabric で撤収 → A は生きている
    let v = session_b
        .read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg)
        .await
        .unwrap();
    assert_eq!(v, ImValue::Bool(true));
    let idx = dev_b.fabric_index.expect("fabric index from NOCResponse");
    let resp = session_b
        .invoke_for_data(
            0,
            commissioning::CLUSTER_OPERATIONAL_CREDENTIALS,
            commissioning::CMD_REMOVE_FABRIC,
            Some(&commissioning::encode_remove_fabric(idx)),
            None,
            &cfg,
        )
        .await
        .expect("remove fabric B");
    assert_eq!(resp.status, 0);
    // A のセッションは同一 socket 上で生存しているはず
    let v = session
        .read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg)
        .await
        .unwrap();
    assert_eq!(v, ImValue::Bool(true));
}
