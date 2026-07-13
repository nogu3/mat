//! M6a 実機受け入れ: 本番 fabric の Nanoleaf に native open-window →
//! 使い捨て第二 fabric へ native commission → 制御 → RemoveFabric 撤収 →
//! 本番 fabric 無傷を確認。実行は scripts/e2e-m6-real.sh 経由。
//! 必須 env: MAT_E2E_NODE_ID(対象), MAT_E2E_IFACE, MAT_E2E_KVS_DIR,
//!           MAT_E2E_FABRIC_INDEX, MAT_E2E_ISSUER_INDEX, MAT_E2E_PAA_DIR
//!
//! 本番 fabric・本番 matd には触れない — controller が読むのは chip-tool の
//! KVS（相乗り identity の自己発行のみ、既存 identity の書き換えなし）。
//! 使い捨て第二 fabric は commissioning が終わるとプロセスメモリからしか
//! 存在せず、最終手順で RemoveFabric により対象デバイスからも撤収する。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::commissioning::{
    self, CommissionParams, CommissionTarget, CommissioningFabric,
};
use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::{compressed_fabric_id, FabricCredentials};
use mat_controller::im::{ImValue, ATTR_ON_OFF, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE};
use mat_controller::session::SecureSession;
use mat_controller::transport::{Transport, UdpTransport};
use mat_controller::{case, dnssd, kvs};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} required"))
}

fn env_u64(name: &str) -> u64 {
    let s = env(name);
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).expect("hex id"),
        None => s.parse().expect("decimal id"),
    }
}

fn env_u8(name: &str) -> u8 {
    env(name)
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be a u8"))
}

async fn read_onoff(s: &mut SecureSession, ep: u16, cfg: &MrpConfig) -> Result<bool, String> {
    match s
        .read_attribute(ep, CLUSTER_ON_OFF, ATTR_ON_OFF, cfg)
        .await
        .map_err(|e| format!("read on-off: {e}"))?
    {
        ImValue::Bool(b) => Ok(b),
        v => Err(format!("on-off not bool: {v:?}")),
    }
}

#[tokio::test]
#[ignore = "requires production fabric KVS + a commissioned real device + PAA store (task e2e:m6:real)"]
async fn commission_second_fabric_and_remove() {
    let device_node_id = env_u64("MAT_E2E_NODE_ID");
    let iface = env("MAT_E2E_IFACE");
    let kvs_dir = PathBuf::from(env("MAT_E2E_KVS_DIR"));
    let fabric_index = env_u8("MAT_E2E_FABRIC_INDEX");
    let issuer_index = env_u8("MAT_E2E_ISSUER_INDEX");
    let paa_dir = PathBuf::from(env("MAT_E2E_PAA_DIR"));
    let endpoint: u16 = 1;
    let transport = Arc::new(UdpTransport::bind().await.unwrap());
    // case::establish (直接呼び出し、2/7) は Arc<Transport> を取る一方、
    // commission_on_network (4/7) の公開シグネチャは Arc<UdpTransport> のまま
    // （M6a 呼び出し側互換）— 同じ UDP ソケットを両方から使うため、Transport
    // 側は clone を wrap するだけ。
    let session_transport = Arc::new(Transport::Udp(Arc::clone(&transport)));

    // 1/7: 本番 fabric credentials（live_jarvis.rs と同じ読み方 — 相乗り
    // identity の自己発行のみ、KVS への書き込みは一切しない）。
    eprintln!("== 1/7 本番 fabric credentials 読み取り");
    let materials = kvs::read_self_issue_materials(
        &kvs_dir.join("chip_tool_config.alpha.ini"),
        &kvs_dir.join("chip_tool_config.ini"),
        fabric_index,
        issuer_index,
    )
    .expect("read CA materials");
    eprintln!(
        "controller node id 0x{:016X}, fabric id 0x{:016X} (from f/{}/n)",
        materials.node_id, materials.fabric_id, fabric_index
    );
    let prod_creds = FabricCredentials::from_self_issued(materials).expect("self-issue NOC");

    // 2/7: 自前 mDNS 解決 → 対象ノードへ CASE → 事前状態を read。
    eprintln!("== 2/7 対象ノードへ CASE 確立（本番 fabric 相乗り）");
    let scope = dnssd::iface_index(&iface).expect("iface index");
    let cfid = compressed_fabric_id(&prod_creds.root_public_key, prod_creds.fabric_id);
    let node = dnssd::resolve_operational(scope, &cfid, device_node_id, Duration::from_secs(8))
        .await
        .expect("mDNS resolve (cross-check: avahi-browse -rtp _matter._tcp)");
    let peers = node.socket_addrs(scope);
    let mrp = node.mrp_config();
    let mut prod_session = None;
    for peer in &peers {
        match case::establish(
            Arc::clone(&session_transport),
            *peer,
            &prod_creds,
            device_node_id,
            &mrp,
        )
        .await
        {
            Ok(s) => {
                eprintln!("CASE established via {peer}");
                prod_session = Some(s);
                break;
            }
            Err(e) => eprintln!("CASE via {peer} failed: {e}"),
        }
    }
    let mut prod_session =
        prod_session.expect("CASE establishment failed on all resolved addresses");
    let before = read_onoff(&mut prod_session, endpoint, &mrp)
        .await
        .expect("read on-off (pre-commissioning)");
    eprintln!("pre-commissioning on-off = {before}");

    // 3/7: native open-commissioning-window（本番 fabric セッション上）。
    eprintln!("== 3/7 open-commissioning-window (180s)");
    let window = commissioning::open_commissioning_window(&mut prod_session, 180, &mrp)
        .await
        .expect("open commissioning window");
    eprintln!(
        "window opened: discriminator={} passcode={} manual={} qr={}",
        window.discriminator, window.passcode, window.manual_code, window.qr_payload
    );

    // 4/7: 使い捨て第二 fabric を生成し、実 _matterc browse 経路で native
    // commission（本物 DAC の厳格 attestation を通過することがここの主眼）。
    eprintln!("== 4/7 使い捨て第二 fabric へ native commission（実 browse 経路）");
    let fabric = CommissioningFabric::generate(0xFAB1, 0x1_0001).expect("generate second fabric");
    let dev = commissioning::commission_on_network(
        Arc::clone(&transport),
        &fabric,
        CommissionParams {
            passcode: window.passcode,
            target: CommissionTarget::Discriminator(window.discriminator),
            device_node_id,
            paa_dir: Some(&paa_dir),
            cd_signer_dir: None,
            scope_id: scope,
        },
    )
    .await
    .expect("commission on second fabric");
    let mut second_session = dev.session;
    eprintln!(
        "commissioned on second fabric: node_id=0x{:016X} fabric_index={:?}",
        dev.node_id, dev.fabric_index
    );

    // 5/7・6/7: 新 fabric での制御確認 + 本番 fabric 無傷確認。
    //
    // commission_on_network が Ok を返した時点で CommissioningComplete 済み
    // = 対象デバイスの fail-safe は解除され、使い捨てのはずの第二 fabric は
    // 恒久エントリになっている。ここから先で assert/panic すると
    // RemoveFabric（撤収）が走らないまま test が落ち、実機に fabric が
    // 残留し続ける（fabric slot は数個しかなく、再実行で更に１つ増える）。
    // そのため制御・検証は panic しない Result 化した exercise() に閉じ込め、
    // 呼び出し側は成否に関わらず必ず RemoveFabric を試みてから、最後に
    // まとめて結果を expect する（live_jarvis.rs の exercise/復元パターンと
    // 同じ考え方）。
    let result = exercise(
        &mut second_session,
        &mut prod_session,
        endpoint,
        &mrp,
        before,
    )
    .await;

    // 7/7: 成否に関わらず RemoveFabric で使い捨て第二 fabric を対象デバイスから
    // 撤収する（best-effort — 失敗はログのみ、exercise() 側の元の失敗は握り
    // つぶさない）。
    eprintln!("== 7/7 RemoveFabric で第二 fabric を撤収（best-effort）");
    match dev.fabric_index {
        Some(idx) => match second_session
            .invoke_for_data(
                0,
                commissioning::CLUSTER_OPERATIONAL_CREDENTIALS,
                commissioning::CMD_REMOVE_FABRIC,
                Some(&commissioning::encode_remove_fabric(idx)),
                None,
                &mrp,
            )
            .await
        {
            Ok(resp) if resp.status == 0 => {
                eprintln!("second fabric removed (fabric_index={idx})");
            }
            Ok(resp) => {
                eprintln!(
                    "WARNING: RemoveFabric returned non-zero status {} (fabric_index={idx}) \
                     — manual cleanup on the device may be required",
                    resp.status
                );
            }
            Err(e) => {
                eprintln!(
                    "WARNING: RemoveFabric failed: {e} (fabric_index={idx}) \
                     — manual cleanup on the device may be required"
                );
            }
        },
        None => {
            eprintln!(
                "WARNING: no fabric_index from NOCResponse — cannot send RemoveFabric, \
                 manual cleanup on the device may be required"
            );
        }
    }

    result.expect("live E2E failed (fabric B removal was attempted regardless)");

    eprintln!("== M6a 実機 E2E PASS");
}

/// 5/7・6/7 本体: 新 fabric での onoff toggle 往復 + 本番 fabric セッション
/// の無傷確認。途中で失敗しても panic せず `Err` を返し、呼び出し側が
/// RemoveFabric による撤収を確実に実行できるようにする。
async fn exercise(
    second_session: &mut SecureSession,
    prod_session: &mut SecureSession,
    endpoint: u16,
    mrp: &MrpConfig,
    before: bool,
) -> Result<(), String> {
    // 5/7: 新 fabric セッションで onoff toggle 往復（反転を確認して元に戻す）。
    eprintln!("== 5/7 新 fabric から onoff toggle 往復");
    second_session
        .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, mrp)
        .await
        .map_err(|e| format!("toggle 1: {e}"))?;
    let toggled = read_onoff(second_session, endpoint, mrp).await?;
    if toggled == before {
        return Err(format!(
            "toggle must flip on-off: expected {}, got {toggled}",
            !before
        ));
    }
    second_session
        .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, mrp)
        .await
        .map_err(|e| format!("toggle 2: {e}"))?;
    let restored = read_onoff(second_session, endpoint, mrp).await?;
    if restored != before {
        return Err(format!(
            "second toggle must restore on-off: expected {before}, got {restored}"
        ));
    }
    eprintln!("onoff toggle round-trip OK (was {before})");

    // 6/7: 本番 fabric セッション（手順 2 のもの）で read が通る = 本番無傷確認。
    eprintln!("== 6/7 本番 fabric セッションで read → 無傷確認");
    let after = read_onoff(prod_session, endpoint, mrp)
        .await
        .map_err(|e| format!("read on-off (production session): {e}"))?;
    eprintln!("production fabric intact: on-off = {after} (pre-commissioning was {before})");

    Ok(())
}
