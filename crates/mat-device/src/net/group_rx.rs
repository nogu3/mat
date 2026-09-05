//! groupcast 受信（spec §4.15 group session、§8.2.5 group 宛 Invoke）の
//! 純関数側: header 分類 → 復号候補 (GKH 一致 keyset) の試行復号 →
//! GroupKeyMap / membership / リプレイ検査 → group InvokeRequest デコード。
//! P ビット（privacy）付きの datagram は候補 keyset ごとに
//! `core::group_privacy` で header を復号してから同じ経路を通す。
//! ソケットと join は `GroupSocket`（同ファイル、Task 7）、Node への適用は
//! `runtime`。応答は送らない（全 drop は `GroupDrop` で理由を返し、runtime が
//! debug ログにする）。
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

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
use crate::core::group_privacy::deobfuscate_header;

/// security flags の session type（spec §4.4.1.4）: 下位 2 bit、0b01 = group。
pub const SESSION_TYPE_MASK: u8 = 0x03;
pub const SESSION_TYPE_GROUP: u8 = 0x01;
/// P フラグ（privacy 処理済み、spec §4.4.1.4）。
pub use crate::core::group_privacy::PRIVACY_FLAG;
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
    let (wire_header, _) = MessageHeader::decode(buf).map_err(|_| GroupDrop::HeaderDecode)?;
    if wire_header.security_flags & SESSION_TYPE_MASK != SESSION_TYPE_GROUP {
        return Err(GroupDrop::NotGroupSession);
    }
    let privacy = wire_header.security_flags & PRIVACY_FLAG != 0;
    if !privacy {
        // 平文 header なら復号前に形を検査できる（drop 理由の順序は従来どおり）。
        wire_header.source_node_id.ok_or(GroupDrop::NoSource)?;
        if !matches!(wire_header.destination, Destination::Group(_)) {
            return Err(GroupDrop::NotGroupDestination);
        }
    }

    // spec §4.15.3: GKH が一致する keyset を全 fabric から集めて順に試す。
    // P ビット付き（chip SDK の送信形）は候補ごとに privacy key で header を
    // 復号してから open_message に渡す（source / destination は復号後の値）。
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
        if derive_group_session_id(&operational) != wire_header.session_id {
            continue;
        }
        candidates += 1;
        let dg: Cow<'_, [u8]> = if privacy {
            match deobfuscate_header(buf, &operational) {
                Some(plain) => Cow::Owned(plain),
                None => continue,
            }
        } else {
            Cow::Borrowed(buf)
        };
        let Ok((header, _)) = MessageHeader::decode(&dg) else {
            continue;
        };
        let (Some(source), Destination::Group(group_id)) =
            (header.source_node_id, header.destination)
        else {
            continue;
        };
        if let Ok((_, proto, payload)) = open_message(&operational, &dg, source) {
            opened = Some((
                ks.fabric_index,
                ks.keyset_id,
                source,
                group_id,
                header.message_counter,
                proto,
                payload,
            ));
            break;
        }
    }
    let Some((fabric_index, keyset_id, source, group_id, counter, proto, payload)) = opened else {
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
    // 復号後（= 認証後）にリプレイ検査: 偽 datagram で窓を進められないように。
    if !replay.accept(fabric_index, source, counter) {
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

/// 失敗した join の再試行間隔（review round 1）: `sync_joins` は毎ループ
/// 反復（データグラム 1 通・タイマー tick 1 回ごと）呼ばれるので、`lo` の
/// ような恒久的に join 不可能なインタフェースでは無条件リトライだと
/// warn ログが同じ頻度で出続けてしまう——1 アドレスあたり高々この間隔に
/// 1 回だけ試行・warn する。
pub const JOIN_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// groupcast 受信ソケット: `[::]:port`（既定 5540）を SO_REUSEADDR +
/// SO_REUSEPORT で bind（同ポートを名乗る複数プロセスの共存用——
/// `DeviceConfig::group_port` の doc 参照。unicast 側には付けられない）、
/// multicast join は `sync_joins` で差分管理。
/// `mat_controller::transport::UdpTransport` に join API が無いので
/// mat-device で直接組む。
pub struct GroupSocket {
    socket: tokio::net::UdpSocket,
    iface_index: u32,
    joined: HashSet<Ipv6Addr>,
    /// アドレスごとの直近の join 失敗時刻（`JOIN_RETRY_INTERVAL` 未満は
    /// 再試行しない）。成功したら消える。
    failed: HashMap<Ipv6Addr, Instant>,
    /// 実際に `join_multicast_v6` を呼んだ回数（バックオフでスキップした
    /// 分は含まない）——テスト用の観測窓。
    join_attempts: u64,
}

impl GroupSocket {
    pub fn bind(port: u16, iface_index: u32) -> std::io::Result<Self> {
        let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_only_v6(true)?;
        sock.set_reuse_address(true)?;
        sock.set_reuse_port(true)?;
        sock.bind(&SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port).into())?;
        sock.set_nonblocking(true)?;
        let socket = tokio::net::UdpSocket::from_std(std::net::UdpSocket::from(sock))?;
        Ok(Self {
            socket,
            iface_index,
            joined: HashSet::new(),
            failed: HashMap::new(),
            join_attempts: 0,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    /// 実際に join を試行した累計回数（バックオフでスキップした分は
    /// 含まない）。テストが「同じ desired set で 2 回目は新規試行なし」を
    /// 確かめるための観測用。
    #[cfg(test)]
    pub(crate) fn join_attempts(&self) -> u64 {
        self.join_attempts
    }

    /// desired との差分だけ join/leave。直近 `JOIN_RETRY_INTERVAL` 以内に
    /// 失敗したアドレスは今回の試行をスキップする（`failed` に記録済みの
    /// まま）——`lo` のような恒久的に join 不可能なインタフェースで、毎
    /// ループ反復のたびに試行・warn し続けるのを避けるため。成功分は
    /// `joined` に記録し `failed` から消す（失敗分は `joined` に入れず
    /// `failed` に残し、次回以降のバックオフ判定に使う）。
    pub fn sync_joins(&mut self, desired: &HashSet<Ipv6Addr>) {
        for addr in desired.difference(&self.joined.clone()) {
            if let Some(last_failed) = self.failed.get(addr) {
                if last_failed.elapsed() < JOIN_RETRY_INTERVAL {
                    continue;
                }
            }
            self.join_attempts += 1;
            match self.socket.join_multicast_v6(addr, self.iface_index) {
                Ok(()) => {
                    self.joined.insert(*addr);
                    self.failed.remove(addr);
                    tracing::info!(%addr, iface_index = self.iface_index, "groupcast: joined");
                }
                Err(e) => {
                    self.failed.insert(*addr, Instant::now());
                    tracing::warn!(%addr, iface_index = self.iface_index, error = %e, "groupcast: join failed (will retry)");
                }
            }
        }
        for addr in self.joined.clone().difference(desired) {
            match self.socket.leave_multicast_v6(addr, self.iface_index) {
                Ok(()) => tracing::info!(%addr, "groupcast: left"),
                Err(e) => tracing::warn!(%addr, error = %e, "groupcast: leave failed"),
            }
            self.joined.remove(addr);
        }
    }
}

/// `runtime::run` が受け取る groupcast 一式（`Device` が組む）。
pub struct GroupRx {
    pub socket: Option<GroupSocket>,
    pub gk_store: GroupKeyStore,
    pub membership: GroupMembershipStore,
}

/// `select!` 用: ソケットが無ければ永遠に pending（`subscription_deadline`
/// と同じ流儀）。
pub async fn group_recv(
    socket: &Option<GroupSocket>,
    buf: &mut [u8],
) -> std::io::Result<(usize, SocketAddr)> {
    match socket {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
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
        gk.upsert_keyset(1, 42, EPOCH, 0).unwrap();
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

    /// P ビット付き datagram を作る（chip SDK と同じ送信形）: security flags
    /// に PRIVACY_FLAG を立てて封じ、header を難読化する。
    fn privacy_datagram(f: &FabricEntry, epoch: &[u8; 16], counter: u32, group: u16) -> Vec<u8> {
        use mat_controller::message::{MessageHeader, ProtocolHeader};
        let c = creds(f, epoch);
        let header = MessageHeader {
            session_id: c.session_id,
            security_flags: SESSION_TYPE_GROUP | PRIVACY_FLAG,
            message_counter: counter,
            source_node_id: Some(SOURCE),
            destination: Destination::Group(group),
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: im::OPCODE_INVOKE_REQUEST,
            exchange_id: 0x42,
            protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
            vendor_id: None,
        };
        let payload =
            im::encode_group_invoke_request(im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE, None);
        let mut dg = mat_controller::crypto::seal_message(
            &c.encryption_key,
            &header,
            &proto,
            &payload,
            SOURCE,
        )
        .unwrap();
        assert!(crate::core::group_privacy::obfuscate_header(
            &mut dg,
            &c.encryption_key
        ));
        dg
    }

    #[test]
    fn privacy_flagged_datagram_is_deobfuscated_and_classified_like_plain() {
        let (fabrics, gk, m) = provisioned();
        let deps = GroupRxDeps {
            fabrics: &fabrics,
            gk_store: &gk,
            membership: &m,
        };
        let mut replay = GroupReplayGuard::new();
        let dg = privacy_datagram(&fabrics[0], &EPOCH, 100, 10);
        let batch = classify_group_datagram(&dg, &deps, &mut replay).unwrap();
        assert_eq!(
            (batch.fabric_index, batch.group_id, batch.source_node_id),
            (1, 10, SOURCE)
        );
        assert_eq!(batch.endpoints, vec![2, 3]);
        assert_eq!(
            (batch.invokes[0].cluster, batch.invokes[0].command),
            (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE)
        );
        // 同じ counter の再送は復号後のリプレイ検査で落ちる
        assert_eq!(
            classify_group_datagram(&dg, &deps, &mut replay).unwrap_err(),
            GroupDrop::Replay
        );
    }

    #[test]
    fn privacy_flagged_datagram_with_wrong_or_missing_key_is_no_keyset() {
        let (fabrics, gk, m) = provisioned();
        let deps = GroupRxDeps {
            fabrics: &fabrics,
            gk_store: &gk,
            membership: &m,
        };
        let mut replay = GroupReplayGuard::new();
        // GKH 不一致（別 epoch）→ 候補ゼロ
        let other = privacy_datagram(&fabrics[0], &[3u8; 16], 1, 10);
        assert_eq!(
            classify_group_datagram(&other, &deps, &mut replay).unwrap_err(),
            GroupDrop::NoKeyset { candidates: 0 }
        );
        // P を立てたが難読化していない（= 受信側が復号すると header が壊れる）:
        // 難読化をもう一度当てて平文 header に戻す（CTR は対称）
        let c = creds(&fabrics[0], &EPOCH);
        let mut raw = privacy_datagram(&fabrics[0], &EPOCH, 2, 10);
        assert!(crate::core::group_privacy::obfuscate_header(
            &mut raw,
            &c.encryption_key
        ));
        assert_eq!(
            classify_group_datagram(&raw, &deps, &mut replay).unwrap_err(),
            GroupDrop::NoKeyset { candidates: 1 }
        );
        // 難読化区間を 1 バイト改竄 → 復号後の header/nonce が変わり MIC 不一致
        raw = privacy_datagram(&fabrics[0], &EPOCH, 3, 10);
        raw[5] ^= 0x01;
        assert_eq!(
            classify_group_datagram(&raw, &deps, &mut replay).unwrap_err(),
            GroupDrop::NoKeyset { candidates: 1 }
        );
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

    #[tokio::test]
    async fn group_socket_binds_ephemeral_and_survives_failed_joins() {
        let mut s = GroupSocket::bind(0, 1 /* lo */).unwrap();
        assert_ne!(s.local_addr().unwrap().port(), 0);
        let mut want = HashSet::new();
        want.insert(mat_controller::group::group_multicast_addr(FABRIC_ID, 10));
        s.sync_joins(&want); // lo は IFF_MULTICAST 無し → join 失敗でも panic しない
        s.sync_joins(&HashSet::new()); // leave 側も同様
    }

    #[tokio::test]
    async fn two_group_sockets_can_share_one_port() {
        let a = GroupSocket::bind(0, 1).unwrap();
        let port = a.local_addr().unwrap().port();
        let _b = GroupSocket::bind(port, 1)
            .expect("SO_REUSEPORT lets a second socket bind the same port");
    }

    /// `DeviceConfig::group_port`'s doc's constraint, pinned directly: a
    /// plain (non-reuseport) unicast-style socket already sitting on a
    /// port makes `GroupSocket::bind` on that same port fail — this is
    /// exactly why `Device::new` refuses `port == group_port` up front
    /// (review round 1, finding 1) rather than letting the bind race.
    #[tokio::test]
    async fn bind_fails_when_a_plain_socket_already_holds_the_port() {
        let taken = std::net::UdpSocket::bind("[::]:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        assert!(GroupSocket::bind(port, 1).is_err());
    }

    /// review round 1, finding 2: a failed join is retried at most once
    /// per `JOIN_RETRY_INTERVAL`, not on every `sync_joins` call — an
    /// immediate second call with the same desired set makes no new
    /// attempt, and aging the recorded failure past the interval lets the
    /// next call retry.
    ///
    /// `lo`'s own join behavior turns out to be environment-dependent (it
    /// lacks `IFF_MULTICAST` per `ip link`, yet some sandboxes' kernels let
    /// the join through anyway) — bogus (999999) `iface_index` is used
    /// instead of `lo`'s real one for a join failure that's deterministic
    /// everywhere (`ENODEV`, "No such device").
    #[tokio::test]
    async fn failed_join_is_retried_only_after_the_backoff_interval() {
        let mut s =
            GroupSocket::bind(0, 999_999 /* no such interface: join always fails */).unwrap();
        let addr = mat_controller::group::group_multicast_addr(FABRIC_ID, 10);
        let mut want = HashSet::new();
        want.insert(addr);

        s.sync_joins(&want);
        assert_eq!(s.join_attempts(), 1, "first call always attempts");

        s.sync_joins(&want);
        assert_eq!(
            s.join_attempts(),
            1,
            "immediate retry within the backoff window makes no new attempt"
        );

        let backdated = Instant::now() - JOIN_RETRY_INTERVAL - Duration::from_secs(1);
        *s.failed.get_mut(&addr).expect("recorded as failed") = backdated;
        s.sync_joins(&want);
        assert_eq!(
            s.join_attempts(),
            2,
            "past the backoff interval, the address is retried"
        );
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
