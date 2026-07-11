//! Live E2E (M3): ride chip-tool's production fabric on a real device —
//! self-issued identity from the KVS, our own mDNS resolution, CASE, then
//! onoff round-trip and a colorcontrol change. Run via `task e2e:m3`
//! (cross-build → transfer → run on the controller host). Not in CI.
//!
//! Required env: MAT_E2E_KVS_DIR, MAT_E2E_NODE_ID, and MAT_E2E_IFACE
//! (or MAT_E2E_PEER to bypass mDNS when isolating failures).
//! Optional: MAT_E2E_FABRIC_INDEX (1), MAT_E2E_ENDPOINT (1),
//! MAT_E2E_ISSUER_INDEX (0).

use std::path::PathBuf;
use std::time::Duration;

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::{compressed_fabric_id, FabricCredentials};
use mat_controller::im::{
    encode_move_to_hue_and_saturation_fields, ImValue, ATTR_CURRENT_HUE, ATTR_CURRENT_SATURATION,
    ATTR_ON_OFF, CLUSTER_COLOR_CONTROL, CLUSTER_ON_OFF, CMD_MOVE_TO_HUE_AND_SATURATION,
    CMD_ON_OFF_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_TOGGLE,
};
use mat_controller::session::SecureSession;
use mat_controller::transport::UdpTransport;
use mat_controller::{case, dnssd, kvs};

fn env_u64(name: &str) -> u64 {
    let s = std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).expect("hex id"),
        None => s.parse().expect("decimal id"),
    }
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(s) => s
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number")),
        Err(_) => default,
    }
}

async fn read_bool(s: &mut SecureSession<'_>, ep: u16, cfg: &MrpConfig) -> bool {
    match s
        .read_attribute(ep, CLUSTER_ON_OFF, ATTR_ON_OFF, cfg)
        .await
        .expect("read on-off")
    {
        ImValue::Bool(b) => b,
        v => panic!("on-off not bool: {v:?}"),
    }
}

async fn read_color_u8(s: &mut SecureSession<'_>, ep: u16, attr: u32, cfg: &MrpConfig) -> u8 {
    match s
        .read_attribute(ep, CLUSTER_COLOR_CONTROL, attr, cfg)
        .await
        .expect("read colorcontrol attr")
    {
        ImValue::Uint(v) => u8::try_from(v).expect("u8 attr"),
        v => panic!("colorcontrol attr not uint: {v:?}"),
    }
}

#[tokio::test]
#[ignore = "requires chip-tool KVS + a commissioned real device (task e2e:m3)"]
async fn fabric_ride_along_onoff_and_color() {
    let dir = PathBuf::from(std::env::var("MAT_E2E_KVS_DIR").expect("MAT_E2E_KVS_DIR required"));
    let device_node_id = env_u64("MAT_E2E_NODE_ID");
    let endpoint: u16 = env_parse("MAT_E2E_ENDPOINT", 1);
    let fabric_index: u8 = env_parse("MAT_E2E_FABRIC_INDEX", 1);
    let issuer_index: u8 = env_parse("MAT_E2E_ISSUER_INDEX", 0);

    // 受け入れ 1: KVS から CA 材料 + NOC 由来の node/fabric id
    let materials = kvs::read_self_issue_materials(
        &dir.join("chip_tool_config.alpha.ini"),
        &dir.join("chip_tool_config.ini"),
        fabric_index,
        issuer_index,
    )
    .expect("read CA materials");
    eprintln!(
        "controller node id 0x{:016X}, fabric id 0x{:016X} (from f/{}/n)",
        materials.node_id, materials.fabric_id, fabric_index
    );

    // 受け入れ 2: 本番 fabric への相乗り identity を自己発行
    let creds = FabricCredentials::from_self_issued(materials).expect("self-issue NOC");

    // 受け入れ 3: 自前 mDNS 解決（MAT_E2E_PEER は障害切り分け用バイパス）
    let (peers, mrp): (Vec<std::net::SocketAddr>, MrpConfig) = match std::env::var("MAT_E2E_PEER") {
        Ok(p) => (vec![p.parse().expect("socket addr")], MrpConfig::default()),
        Err(_) => {
            let iface =
                std::env::var("MAT_E2E_IFACE").expect("MAT_E2E_IFACE or MAT_E2E_PEER required");
            let scope = dnssd::iface_index(&iface).expect("iface index");
            let cfid = compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
            let node =
                dnssd::resolve_operational(scope, &cfid, device_node_id, Duration::from_secs(8))
                    .await
                    .expect("mDNS resolve (cross-check: avahi-browse -rtp _matter._tcp)");
            eprintln!(
                "resolved {} addr(s), port {}, SII {:?} ms, SAI {:?} ms",
                node.addresses.len(),
                node.port,
                node.session_idle_interval_ms,
                node.session_active_interval_ms
            );
            (node.socket_addrs(scope), node.mrp_config())
        }
    };

    // 受け入れ 4: CASE 確立（解決したアドレスを順に試す）
    let transport = UdpTransport::bind().await.unwrap();
    let mut session = None;
    for peer in &peers {
        match case::establish(&transport, *peer, &creds, device_node_id, &mrp).await {
            Ok(s) => {
                eprintln!("CASE established via {peer}");
                session = Some(s);
                break;
            }
            Err(e) => eprintln!("CASE via {peer} failed: {e}"),
        }
    }
    let mut session = session.expect("CASE establishment failed on all resolved addresses");

    // 受け入れ 5: onoff toggle 往復（元の状態に戻して終わる）
    let before = read_bool(&mut session, endpoint, &mrp).await;
    session
        .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, &mrp)
        .await
        .expect("toggle 1");
    assert_eq!(
        read_bool(&mut session, endpoint, &mrp).await,
        !before,
        "toggle must flip on-off"
    );
    session
        .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, &mrp)
        .await
        .expect("toggle 2");
    assert_eq!(
        read_bool(&mut session, endpoint, &mrp).await,
        before,
        "second toggle must restore on-off"
    );
    eprintln!("onoff toggle round-trip OK (was {before})");

    // 受け入れ 6: 色変更（ライト on で実施し、hue/sat とも元へ復元）
    if !before {
        session
            .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None, &mrp)
            .await
            .expect("on for color");
    }
    let hue0 = read_color_u8(&mut session, endpoint, ATTR_CURRENT_HUE, &mrp).await;
    let sat0 = read_color_u8(&mut session, endpoint, ATTR_CURRENT_SATURATION, &mrp).await;
    // CurrentHue は 0..=254 の円環。確実に離れた目標を選ぶ。
    let target_hue = ((u16::from(hue0) + 80) % 254) as u8;
    let fields = encode_move_to_hue_and_saturation_fields(target_hue, 200, 0);
    session
        .invoke(
            endpoint,
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(&fields),
            &mrp,
        )
        .await
        .expect("move-to-hue-and-saturation");
    // transition 0 でも装置内の属性反映に猶予を置く
    tokio::time::sleep(Duration::from_millis(500)).await;
    let hue1 = read_color_u8(&mut session, endpoint, ATTR_CURRENT_HUE, &mrp).await;
    let d = (i32::from(hue1) - i32::from(target_hue)).abs();
    let d = d.min(254 - d); // 円環距離
    assert!(d <= 8, "current-hue {hue1} not near target {target_hue}");
    eprintln!("color change OK: hue {hue0} -> {hue1} (target {target_hue})");

    // 後始末: 色と電源状態を復元
    let fields = encode_move_to_hue_and_saturation_fields(hue0, sat0, 0);
    session
        .invoke(
            endpoint,
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(&fields),
            &mrp,
        )
        .await
        .expect("restore color");
    if !before {
        session
            .invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_OFF, None, &mrp)
            .await
            .expect("restore off");
    }
    eprintln!("restored original state");
}
