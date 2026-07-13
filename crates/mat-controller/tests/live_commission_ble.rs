//! M6b 実機受け入れ: 工場リセット済みデバイスを BLE+Thread で使い捨て
//! fabric へ native commission → onoff 確認 → native open-window →
//! （オペレータが本番 `mat commission` を実行）→ RemoveFabric 撤収。
//! 実行は scripts/e2e-m6b-real.sh 経由（jarvis 上）。
//!
//! 必須 env:
//!   MAT_E2E_BLE_PASSCODE      デバイス印字の setup passcode
//!   MAT_E2E_BLE_DISCRIMINATOR 12-bit discriminator（10進）
//!   MAT_E2E_THREAD_DATASET    `ot-ctl dataset active -x` の hex
//!   MAT_E2E_IFACE             operational 発見用 iface（jarvis は eth0）
//!   MAT_E2E_PAA_DIR           本番 PAA ストア（<store>/paa-trust-store）
//!   MAT_E2E_NODE_ID           使い捨て fabric 上の新 node_id（例 200）

#![cfg(feature = "ble")]

use mat_controller::commissioning::{self, BleThreadParams, CommissioningFabric};
use mat_controller::exchange::MrpConfig;
use mat_controller::im::{CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} required"))
}

fn hex_bytes(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex dataset"))
        .collect()
}

#[tokio::test]
#[ignore = "requires jarvis BLE + factory-reset device + OTBR dataset (task e2e:m6b:real)"]
async fn commission_factory_device_over_ble_thread() {
    let passcode: u32 = env("MAT_E2E_BLE_PASSCODE").parse().unwrap();
    let discriminator: u16 = env("MAT_E2E_BLE_DISCRIMINATOR").parse().unwrap();
    let dataset = hex_bytes(&env("MAT_E2E_THREAD_DATASET"));
    let scope_id = mat_controller::dnssd::iface_index(&env("MAT_E2E_IFACE")).unwrap();
    let paa_dir = std::path::PathBuf::from(env("MAT_E2E_PAA_DIR"));
    let device_node_id: u64 = env("MAT_E2E_NODE_ID").parse().unwrap();

    // 使い捨て fabric（M6a と同じ機構、プロセスメモリのみ）
    let fabric = CommissioningFabric::generate(0xE2E2, 100).unwrap();

    // 1) BLE+Thread native commission
    eprintln!("== 1/4 BLE+Thread native commission");
    let device = commissioning::commission_ble_thread(
        &fabric,
        BleThreadParams {
            passcode,
            discriminator,
            thread_dataset: &dataset,
            device_node_id,
            paa_dir: Some(&paa_dir),
            cd_signer_dir: None,
            scope_id,
        },
    )
    .await
    .expect("ble+thread commissioning failed");
    let mut session = device.session;
    let cfg = MrpConfig::default();
    eprintln!(
        "== commissioned: node {} fabric_index {:?}",
        device.node_id, device.fabric_index
    );

    // 2) 使い捨て fabric で onoff toggle（動作確認）
    eprintln!("== 2/4 onoff toggle（使い捨て fabric）");
    session
        .invoke(1, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, &cfg)
        .await
        .expect("toggle over new fabric");
    eprintln!("== toggle OK — ライトが変化したこと目視");

    // 3) native open-window → 本番 join 用コードを表示
    eprintln!("== 3/4 open-commissioning-window (300s)");
    let window = commissioning::open_commissioning_window(&mut session, 300, &cfg)
        .await
        .expect("open window");
    eprintln!(
        "== 本番復帰: 別端末で `mat commission <target> {}` を 5 分以内に実行",
        window.manual_code
    );
    eprintln!("== 完了したら Enter:");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();

    // 4) 使い捨て fabric を撤収（RemoveFabric は fabric_index 必須）
    eprintln!("== 4/4 RemoveFabric で使い捨て fabric を撤収");
    let idx = device.fabric_index.expect("fabric index from AddNOC");
    let resp = session
        .invoke_for_data(
            0,
            commissioning::CLUSTER_OPERATIONAL_CREDENTIALS,
            commissioning::CMD_REMOVE_FABRIC,
            Some(&commissioning::encode_remove_fabric(idx)),
            None,
            &cfg,
        )
        .await
        .expect("remove fabric");
    eprintln!(
        "== RemoveFabric status {} — 使い捨て fabric 撤収完了",
        resp.status
    );
    eprintln!("== M6b 実機 E2E PASS");
}
