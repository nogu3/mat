use crate::*;
use mat_controller::cert::MatterCert;
use mat_controller::kvs::SelfIssueMaterials;
use mat_controller::test_support as case_ts;
use mat_controller::transport::UdpTransport;
use std::net::Ipv6Addr;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 呼び出し順に固定ポートを払い出す fake resolver。2 応答器が同一
/// fixture 識別（同一 node_id）なので、どちらの establish がどちらの
/// 応答器に着いても対称で問題ない。
struct FixedPortResolver {
    ports: Vec<u16>,
    next: AtomicUsize,
}

#[async_trait]
impl Resolver for FixedPortResolver {
    async fn resolve(
        &self,
        _scope_id: u32,
        _cfid: [u8; 8],
        _node_id: u64,
        _timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        Ok(dnssd::ResolvedNode {
            port: self.ports[i],
            addresses: vec![Ipv6Addr::LOCALHOST],
            session_idle_interval_ms: Some(50),
            session_active_interval_ms: Some(50),
        })
    }
}

/// 監査#3 の釘打ち: 異なるノードへの並行 op が互いの応答を吸わない。
/// ループバックに CASE 応答器を 2 つ立て、並行 establish + read が両方
/// 成功し、応答器の観測した initiator ソースポートが異なる（= ノード
/// ごとの専用ソケット）ことを assert する。共有ソケットに退行すると
/// ポートが一致して確実に落ちる。
#[tokio::test]
async fn concurrent_establishes_use_dedicated_sockets() {
    let noc = MatterCert::parse(case_ts::NODE01_NOC).expect("parse fixture NOC");
    let responder_node_id = noc.node_id().expect("node id");
    let fabric_id = noc.fabric_id().expect("fabric id");
    let op_priv: [u8; 32] = case_ts::NODE01_PRIV.try_into().unwrap();

    // 応答器 2 つ（同一識別・別ポート）。
    let mut handles = Vec::new();
    let mut ports = Vec::new();
    for _ in 0..2 {
        let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        ports.push(t.local_addr().unwrap().port());
        handles.push(tokio::spawn(case_ts::responder_task(
            t,
            case_ts::INITIATOR_NODE_ID,
            responder_node_id,
            case_ts::NODE01_NOC.to_vec(),
            case_ts::ICA01.to_vec(),
            op_priv,
            case_ts::ROOT01_CHIP.to_vec(),
        )));
    }

    let materials = SelfIssueMaterials {
        rcac: case_ts::ROOT01_CHIP.to_vec(),
        root_private_key: case_ts::ROOT01_PRIV.try_into().unwrap(),
        ipk_operational: case_ts::IPK,
        node_id: case_ts::INITIATOR_NODE_ID,
        fabric_id,
    };
    let creds = FabricCredentials::from_self_issued(materials).expect("creds");
    let est = CaseEstablisher {
        creds: std::sync::RwLock::new(Arc::new(creds)),
        scope_id: 0,
        resolver: Arc::new(FixedPortResolver {
            ports,
            next: AtomicUsize::new(0),
        }),
        cfg: NativeConfig {
            store: std::path::PathBuf::from("/nonexistent"),
            iface: "lo".into(),
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        },
    };

    let (a, b) = tokio::join!(
        est.establish(responder_node_id),
        est.establish(responder_node_id)
    );
    let mut a = a.expect("establish 1");
    let mut b = b.expect("establish 2");
    let (ra, rb) = tokio::join!(a.read_onoff(1), b.read_onoff(1));
    // 応答器は on-off=false を返す（clippy: bool_assert_comparison を避け assert! で）。
    assert!(!ra.expect("read 1"));
    assert!(!rb.expect("read 2"));

    let sa = handles.pop().unwrap().await.expect("responder 2");
    let sb = handles.pop().unwrap().await.expect("responder 1");
    assert_ne!(
        sa.port(),
        sb.port(),
        "op sockets must be dedicated per establish (audit #3)"
    );
}

/// reload の釘打ち: 間違った IPK で建てた確立器は CASE に失敗し、正しい
/// 資格情報へ swap した直後の establish は成功する（進行中セッション無し
/// の最小形 — swap が「次の確立から効く」ことを実 CASE で確認する）。
#[tokio::test]
async fn swapped_credentials_are_used_by_the_next_establish() {
    let noc = MatterCert::parse(case_ts::NODE01_NOC).expect("parse fixture NOC");
    let responder_node_id = noc.node_id().expect("node id");
    let fabric_id = noc.fabric_id().expect("fabric id");
    let op_priv: [u8; 32] = case_ts::NODE01_PRIV.try_into().unwrap();

    // 応答器 2 つ（1 回目の失敗で 1 つ目が終わっても 2 回目が着く先を持つ）。
    let mut handles = Vec::new();
    let mut ports = Vec::new();
    for _ in 0..2 {
        let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        ports.push(t.local_addr().unwrap().port());
        handles.push(tokio::spawn(case_ts::responder_task(
            t,
            case_ts::INITIATOR_NODE_ID,
            responder_node_id,
            case_ts::NODE01_NOC.to_vec(),
            case_ts::ICA01.to_vec(),
            op_priv,
            case_ts::ROOT01_CHIP.to_vec(),
        )));
    }

    let materials = |ipk: [u8; 16]| SelfIssueMaterials {
        rcac: case_ts::ROOT01_CHIP.to_vec(),
        root_private_key: case_ts::ROOT01_PRIV.try_into().unwrap(),
        ipk_operational: ipk,
        node_id: case_ts::INITIATOR_NODE_ID,
        fabric_id,
    };
    let wrong = FabricCredentials::from_self_issued(materials([0xDD; 16])).expect("creds");
    let right = FabricCredentials::from_self_issued(materials(case_ts::IPK)).expect("creds");
    let est = CaseEstablisher {
        creds: std::sync::RwLock::new(Arc::new(wrong)),
        scope_id: 0,
        resolver: Arc::new(FixedPortResolver {
            ports,
            next: AtomicUsize::new(0),
        }),
        cfg: NativeConfig {
            store: std::path::PathBuf::from("/nonexistent"),
            iface: "lo".into(),
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        },
    };

    // `expect_err` は `Box<dyn NodeConn>: Debug` を要求してしまう（未実装）
    // ので、既存の `fake_sub_conn_next_report_fails_when_injected_after_establish`
    // と同じ match で取り出す。
    let err = match est.establish(responder_node_id).await {
        Err(e) => e,
        Ok(_) => panic!("wrong IPK must not establish"),
    };
    assert_eq!(err.kind, ErrorKind::SessionFailed, "detail={}", err.detail);

    assert!(est.swap_credentials(right), "IPK differs → changed");
    let mut conn = est
        .establish(responder_node_id)
        .await
        .expect("establish with the swapped credentials");
    assert!(!conn.read_onoff(1).await.expect("read after swap"));

    for h in handles {
        h.abort();
    }
}
