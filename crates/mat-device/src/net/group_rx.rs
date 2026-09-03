//! groupcast 受信（spec §4.15 group session、§8.2.5 group 宛 Invoke）の
//! 純関数側: header 分類 → 復号候補 (GKH 一致 keyset) の試行復号 →
//! GroupKeyMap / membership / リプレイ検査 → group InvokeRequest デコード。
//! ソケットと join は `GroupSocket`（同ファイル、Task 7）、Node への適用は
//! `runtime`。応答は送らない（全 drop は `GroupDrop` で理由を返し、runtime が
//! debug ログにする）。
use std::collections::{HashSet, VecDeque};
use std::net::Ipv6Addr;

use mat_controller::crypto::open_message;
use mat_controller::fabric::{
    compressed_fabric_id, derive_group_session_id, derive_ipk_operational,
};
use mat_controller::group::group_multicast_addr;
use mat_controller::im;
use mat_controller::message::{Destination, MessageHeader, PROTOCOL_ID_INTERACTION_MODEL};

use crate::core::fabric_store::FabricEntry;
use crate::core::group_invoke::{decode_group_invoke_request, GroupInvokeIn};
use crate::core::group_key_management::GroupKeyStore;
use crate::core::group_membership::GroupMembershipStore;

/// security flags の session type（spec §4.4.1.4）: 下位 2 bit、0b01 = group。
pub const SESSION_TYPE_MASK: u8 = 0x03;
pub const SESSION_TYPE_GROUP: u8 = 0x01;
/// P フラグ（privacy 処理済み）— 未対応で drop。
pub const PRIVACY_FLAG: u8 = 0x80;
/// リプレイ表の上限 `(fabric, source)` 数。超えたら最古を退去。
pub const REPLAY_TABLE_CAPACITY: usize = 64;

/// spec §4.5.4.2 の group data message counter 検査の簡略版: `(fabric,
/// source node)` ごとに最終 counter を持ち、それ以下は drop。bitmap window
/// は持たず順序逆転は捨てる側に倒す（mat は各 egress に同一 datagram を
/// 流すので、実用上の役目は重複排除）。初見の source は trust-first で受理。
pub struct GroupReplayGuard {
    seen: VecDeque<((u8, u64), u32)>,
}

impl GroupReplayGuard {
    pub fn new() -> Self {
        Self {
            seen: VecDeque::new(),
        }
    }

    pub fn accept(&mut self, fabric_index: u8, source_node_id: u64, counter: u32) -> bool {
        let key = (fabric_index, source_node_id);
        if let Some(pos) = self.seen.iter().position(|(k, _)| *k == key) {
            if counter <= self.seen[pos].1 {
                return false;
            }
            self.seen[pos].1 = counter;
            return true;
        }
        if self.seen.len() >= REPLAY_TABLE_CAPACITY {
            self.seen.pop_front();
        }
        self.seen.push_back((key, counter));
        true
    }
}

impl Default for GroupReplayGuard {
    fn default() -> Self {
        Self::new()
    }
}

pub struct GroupRxDeps<'a> {
    pub fabrics: &'a [FabricEntry],
    pub gk_store: &'a GroupKeyStore,
    pub membership: &'a GroupMembershipStore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupDrop {
    HeaderDecode,
    NotGroupSession,
    Privacy,
    NoSource,
    NotGroupDestination,
    NoKeyset { candidates: usize },
    NotMapped,
    NoMembers,
    Replay,
    NotInvoke,
    Malformed,
}

#[derive(Debug)]
pub struct GroupInvokeBatch {
    pub fabric_index: u8,
    pub group_id: u16,
    pub source_node_id: u64,
    pub endpoints: Vec<u16>,
    pub invokes: Vec<GroupInvokeIn>,
}

pub fn classify_group_datagram(
    buf: &[u8],
    deps: &GroupRxDeps<'_>,
    replay: &mut GroupReplayGuard,
) -> Result<GroupInvokeBatch, GroupDrop> {
    let (header, _) = MessageHeader::decode(buf).map_err(|_| GroupDrop::HeaderDecode)?;
    if header.security_flags & SESSION_TYPE_MASK != SESSION_TYPE_GROUP {
        return Err(GroupDrop::NotGroupSession);
    }
    if header.security_flags & PRIVACY_FLAG != 0 {
        return Err(GroupDrop::Privacy);
    }
    let source = header.source_node_id.ok_or(GroupDrop::NoSource)?;
    let Destination::Group(group_id) = header.destination else {
        return Err(GroupDrop::NotGroupDestination);
    };

    // spec §4.15.3: GKH が一致する keyset を全 fabric から集めて順に試す。
    let mut candidates = 0usize;
    let mut opened = None;
    for ks in deps.gk_store.keysets() {
        let Some(f) = deps
            .fabrics
            .iter()
            .find(|f| f.fabric_index == ks.fabric_index)
        else {
            continue;
        };
        let operational = derive_ipk_operational(
            &ks.epoch_key0,
            &compressed_fabric_id(&f.root_public_key, f.fabric_id),
        );
        if derive_group_session_id(&operational) != header.session_id {
            continue;
        }
        candidates += 1;
        if let Ok((_, proto, payload)) = open_message(&operational, buf, source) {
            opened = Some((ks.fabric_index, ks.keyset_id, proto, payload));
            break;
        }
    }
    let Some((fabric_index, keyset_id, proto, payload)) = opened else {
        return Err(GroupDrop::NoKeyset { candidates });
    };
    if !deps
        .gk_store
        .map_entries_for(fabric_index)
        .contains(&(group_id, keyset_id))
    {
        return Err(GroupDrop::NotMapped);
    }
    let endpoints = deps.membership.endpoints_for(fabric_index, group_id);
    if endpoints.is_empty() {
        return Err(GroupDrop::NoMembers);
    }
    if !replay.accept(fabric_index, source, header.message_counter) {
        return Err(GroupDrop::Replay);
    }
    if proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL
        || proto.opcode != im::OPCODE_INVOKE_REQUEST
    {
        return Err(GroupDrop::NotInvoke);
    }
    let invokes = decode_group_invoke_request(&payload).map_err(|_| GroupDrop::Malformed)?;
    Ok(GroupInvokeBatch {
        fabric_index,
        group_id,
        source_node_id: source,
        endpoints,
        invokes,
    })
}

/// join すべき multicast アドレス集合 = 各 fabric × その fabric の membership
/// にある group（fabric が無い membership は無視）。
pub fn desired_group_addrs(
    fabrics: &[FabricEntry],
    membership: &GroupMembershipStore,
) -> HashSet<Ipv6Addr> {
    membership
        .groups_by_fabric()
        .into_iter()
        .filter_map(|(fabric_index, group_id)| {
            fabrics
                .iter()
                .find(|f| f.fabric_index == fabric_index)
                .map(|f| group_multicast_addr(f.fabric_id, group_id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::group_key_management::GroupKeyStore;
    use crate::core::group_membership::GroupMembershipStore;
    use mat_controller::fabric::{
        compressed_fabric_id, derive_group_session_id, derive_ipk_operational,
    };
    use mat_controller::group::build_group_datagram;
    use mat_controller::im;
    use mat_controller::kvs::GroupCredentials;

    const FABRIC_ID: u64 = 0x2233_4455;
    const EPOCH: [u8; 16] = [9u8; 16];
    const SOURCE: u64 = 660_033;

    /// テスト fabric（root 公開鍵は形だけ — cfid は決定的なら何でもよい）。
    fn fabric(fabric_index: u8) -> FabricEntry {
        FabricEntry {
            fabric_index,
            root_tlv: Vec::new(),
            noc_tlv: Vec::new(),
            icac_tlv: None,
            op_private_key: [1u8; 32],
            ipk_operational: [2u8; 16],
            node_id: 1,
            fabric_id: FABRIC_ID,
            root_public_key: [4u8; 65],
            admin_subject: SOURCE,
            admin_vendor_id: 0xFFF1,
            label: String::new(),
        }
    }

    fn creds(f: &FabricEntry, epoch: &[u8; 16]) -> GroupCredentials {
        let op = derive_ipk_operational(
            epoch,
            &compressed_fabric_id(&f.root_public_key, f.fabric_id),
        );
        GroupCredentials {
            session_id: derive_group_session_id(&op),
            encryption_key: op,
        }
    }

    fn provisioned() -> (Vec<FabricEntry>, GroupKeyStore, GroupMembershipStore) {
        let gk = GroupKeyStore::new();
        gk.upsert_keyset(1, 42, EPOCH).unwrap();
        gk.replace_fabric_map(1, vec![(10, 42)]);
        let m = GroupMembershipStore::new();
        m.add(1, 10, 2).unwrap();
        m.add(1, 10, 3).unwrap();
        (vec![fabric(1)], gk, m)
    }

    fn datagram(f: &FabricEntry, epoch: &[u8; 16], counter: u32, group: u16) -> Vec<u8> {
        build_group_datagram(
            &creds(f, epoch),
            SOURCE,
            counter,
            0x42,
            group,
            im::CLUSTER_ON_OFF,
            im::CMD_ON_OFF_TOGGLE,
            None,
        )
        .unwrap()
    }

    #[test]
    fn a_provisioned_group_datagram_yields_the_member_endpoints_and_invoke() {
        let (fabrics, gk, m) = provisioned();
        let mut replay = GroupReplayGuard::new();
        let dg = datagram(&fabrics[0], &EPOCH, 100, 10);
        let batch = classify_group_datagram(
            &dg,
            &GroupRxDeps {
                fabrics: &fabrics,
                gk_store: &gk,
                membership: &m,
            },
            &mut replay,
        )
        .unwrap();
        assert_eq!(
            (batch.fabric_index, batch.group_id, batch.source_node_id),
            (1, 10, SOURCE)
        );
        assert_eq!(batch.endpoints, vec![2, 3]);
        assert_eq!(batch.invokes.len(), 1);
        assert_eq!(
            (batch.invokes[0].cluster, batch.invokes[0].command),
            (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE)
        );
    }

    #[test]
    fn drops_are_classified() {
        let (fabrics, gk, m) = provisioned();
        let deps = GroupRxDeps {
            fabrics: &fabrics,
            gk_store: &gk,
            membership: &m,
        };
        let mut replay = GroupReplayGuard::new();
        // 別 epoch = GKH 不一致（高確率）or 復号失敗 → NoKeyset
        let other = datagram(&fabrics[0], &[3u8; 16], 1, 10);
        assert!(matches!(
            classify_group_datagram(&other, &deps, &mut replay),
            Err(GroupDrop::NoKeyset { .. })
        ));
        // map に無い group
        let unmapped = datagram(&fabrics[0], &EPOCH, 2, 11);
        assert_eq!(
            classify_group_datagram(&unmapped, &deps, &mut replay).unwrap_err(),
            GroupDrop::NotMapped
        );
        // mapped だが member なし
        gk.append_map_entry(1, 12, 42);
        let nomember = datagram(&fabrics[0], &EPOCH, 3, 12);
        assert_eq!(
            classify_group_datagram(&nomember, &deps, &mut replay).unwrap_err(),
            GroupDrop::NoMembers
        );
        // リプレイ: 同 counter の 2 通目
        let dg = datagram(&fabrics[0], &EPOCH, 50, 10);
        assert!(classify_group_datagram(&dg, &deps, &mut replay).is_ok());
        assert_eq!(
            classify_group_datagram(&dg, &deps, &mut replay).unwrap_err(),
            GroupDrop::Replay
        );
        // unicast 形の header（session type 0）
        let mut uni = dg.clone();
        uni[3] &= !SESSION_TYPE_MASK; // security flags byte: header layout は message.rs（flags, session id(2), security flags）
        assert_eq!(
            classify_group_datagram(&uni, &deps, &mut replay).unwrap_err(),
            GroupDrop::NotGroupSession
        );
        // privacy ビット
        let mut priv_dg = datagram(&fabrics[0], &EPOCH, 51, 10);
        priv_dg[3] |= PRIVACY_FLAG;
        assert_eq!(
            classify_group_datagram(&priv_dg, &deps, &mut replay).unwrap_err(),
            GroupDrop::Privacy
        );
        // ゴミ
        assert_eq!(
            classify_group_datagram(&[0u8; 3], &deps, &mut replay).unwrap_err(),
            GroupDrop::HeaderDecode
        );
    }

    #[test]
    fn replay_guard_is_strictly_increasing_per_source_and_bounded() {
        let mut g = GroupReplayGuard::new();
        assert!(g.accept(1, 7, 10));
        assert!(!g.accept(1, 7, 10));
        assert!(!g.accept(1, 7, 9));
        assert!(g.accept(1, 7, 11));
        assert!(g.accept(2, 7, 1), "another fabric is another window");
        for src in 100..(100 + REPLAY_TABLE_CAPACITY as u64) {
            assert!(g.accept(1, src, 1));
        }
        // 最古（fabric 1, source 7）が退去 → 同じ counter がまた通る
        assert!(g.accept(1, 7, 11));
    }

    #[test]
    fn desired_addrs_follow_membership_times_fabric() {
        let (fabrics, _gk, m) = provisioned();
        let set = desired_group_addrs(&fabrics, &m);
        assert_eq!(set.len(), 1);
        assert!(set.contains(&mat_controller::group::group_multicast_addr(FABRIC_ID, 10)));
        m.add(1, 11, 2).unwrap();
        assert_eq!(desired_group_addrs(&fabrics, &m).len(), 2);
        // fabric が無い membership は join しない
        m.add(9, 5, 2).unwrap();
        assert_eq!(desired_group_addrs(&fabrics, &m).len(), 2);
    }
}
