//! Groupcast send support (M5): multicast destination address and the
//! persisted global group data counter.
//!
//! The counter shares one space with chip-tool (same source node id), so it
//! never restarts low: it persists ahead of use (SDK PersistedCounter
//! semantics) and boot-jumps past both its own file and chip-tool's `g/gdc`.

use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::crypto::{self, CryptoError};
use crate::im;
use crate::kvs::GroupCredentials;
use crate::message::{Destination, MessageHeader, ProtocolHeader};
use crate::transport::UdpTransport;

/// Persist-ahead window: the file always stores a value no counter below
/// which has been handed out, so a crash can never reuse a sent counter.
pub const COUNTER_EPOCH: u32 = 4096;

/// Matter site-local transient multicast group address (spec §2.5.9.2):
/// `FF35:0040:FD || fabric_id(8B BE) || 00 || group_id(2B BE)`.
pub fn group_multicast_addr(fabric_id: u64, group_id: u16) -> Ipv6Addr {
    let f = fabric_id.to_be_bytes();
    let g = group_id.to_be_bytes();
    Ipv6Addr::from([
        0xff, 0x35, 0x00, 0x40, 0xfd, f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], 0x00, g[0],
        g[1],
    ])
}

/// Global Group Data Counter with persist-ahead storage (decimal text file).
#[derive(Debug)]
pub struct PersistedGroupCounter {
    next: u32,
    ceiling: u32,
    path: PathBuf,
    /// プロセス間排他（advisory flock、`<path>.lock` に取る）。counter 本体は
    /// tmp+rename で置換されるため本体 fd への flock は rename 後に無効化される
    /// —— ロックは安定した別ファイルに取り、インスタンス生存中保持する
    /// （Drop で OS が解放。matd 常駐中は one-shot の load が WouldBlock になり、
    /// native 送信元の counter 混在を構造的に防ぐ）。
    _lock: std::fs::File,
}

impl PersistedGroupCounter {
    /// Starts from `max(own persisted ceiling, chip-tool g/gdc) + EPOCH` and
    /// persists the new ceiling before returning. A corrupt counter file is
    /// an error (starting low would get every send dropped by receivers).
    pub fn load(path: &Path, chip_tool_gdc: u32) -> io::Result<Self> {
        use rustix::fs::{flock, FlockOperation};
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lock_path))?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "group counter is locked by another process (matd running?)",
                )
            } else {
                io::Error::other(e)
            }
        })?;
        let persisted = match std::fs::read_to_string(path) {
            Ok(s) => s.trim().parse::<u32>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "corrupt group counter file")
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let start = persisted.max(chip_tool_gdc).wrapping_add(COUNTER_EPOCH);
        let mut c = Self {
            next: start,
            ceiling: start,
            path: path.to_path_buf(),
            _lock: lock,
        };
        c.persist(start.wrapping_add(COUNTER_EPOCH))?;
        Ok(c)
    }

    /// Returns the counter to send with and advances. Crossing the persisted
    /// ceiling persists the next window first.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<u32> {
        if self.next == self.ceiling {
            self.persist(self.ceiling.wrapping_add(COUNTER_EPOCH))?;
        }
        let v = self.next;
        self.next = self.next.wrapping_add(1);
        Ok(v)
    }

    /// counter 窓を「再起動 1 回相当」前方へジャンプする（Issue #14 応急処置）。
    /// 次回払い出し値を `ceiling + EPOCH`（= `load()` と同じ算術）へ進め、
    /// 新 ceiling を先に永続化してから返す。persist 失敗時は状態を変えない
    /// （旧 ceiling のまま — 送信継続は安全）。
    /// 返り値は (ジャンプ前の次回値 from, ジャンプ後の次回値 to)。
    pub fn jump(&mut self) -> io::Result<(u32, u32)> {
        let from = self.next;
        let to = self.ceiling.wrapping_add(COUNTER_EPOCH);
        self.persist(to.wrapping_add(COUNTER_EPOCH))?;
        self.next = to;
        Ok((from, to))
    }

    /// Atomic write (tmp + fsync + rename) so a crash never leaves a
    /// truncated value behind.
    fn persist(&mut self, ceiling: u32) -> io::Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(format!("{ceiling}\n").as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        self.ceiling = ceiling;
        Ok(())
    }
}

/// Security flags for a group session data message (spec §4.4.1.4:
/// session type = 1, no privacy).
pub const GROUP_SECURITY_FLAGS: u8 = 0x01;

/// Multicast hop limit for groupcast sends (Matter SDK default).
pub const MULTICAST_HOP_LIMIT: u32 = 64;

/// Groupcast send failure: encryption (caller bug / oversized payload) or
/// socket I/O.
#[derive(Debug)]
pub enum GroupSendError {
    Crypto(CryptoError),
    Io(std::io::Error),
}

impl std::fmt::Display for GroupSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Crypto(e) => write!(f, "group message encryption: {e}"),
            Self::Io(e) => write!(f, "group multicast send: {e}"),
        }
    }
}

impl std::error::Error for GroupSendError {}

/// Builds the encrypted groupcast datagram (pure; unit-testable without
/// sockets): group-session plain header + CCM-sealed group InvokeRequest.
#[allow(clippy::too_many_arguments)]
pub fn build_group_datagram(
    creds: &GroupCredentials,
    source_node_id: u64,
    counter: u32,
    exchange_id: u16,
    group_id: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Result<Vec<u8>, CryptoError> {
    let header = MessageHeader {
        session_id: creds.session_id,
        security_flags: GROUP_SECURITY_FLAGS,
        message_counter: counter,
        source_node_id: Some(source_node_id),
        destination: Destination::Group(group_id),
    };
    let proto = ProtocolHeader {
        initiator: true,
        needs_ack: false, // groupcast is unreliable by spec — no MRP
        acked_counter: None,
        opcode: im::OPCODE_INVOKE_REQUEST,
        exchange_id,
        protocol_id: im::PROTOCOL_ID_IM,
        vendor_id: None,
    };
    let payload = im::encode_group_invoke_request(cluster, command, fields_tlv);
    crypto::seal_message(
        &creds.encryption_key,
        &header,
        &proto,
        &payload,
        source_node_id,
    )
}

/// groupcast の送出先 1 本分。transport は egress 専用（unicast と共有しない）。
#[derive(Clone)]
pub struct GroupEgress {
    pub iface: String,
    pub transport: Arc<UdpTransport>,
    pub scope_id: u32,
}

/// Send-only groupcast path. Holds no per-group key state: credentials are
/// passed per call (the caller re-reads the KVS so re-provisioned keys are
/// picked up immediately).
pub struct GroupSender {
    egress: Vec<GroupEgress>,
    dest_port: u16,
    fabric_id: u64,
    source_node_id: u64,
    counter: PersistedGroupCounter,
}

impl GroupSender {
    /// Configures each egress socket's multicast hop limit and assembles the
    /// sender. `dest_port` is `message::MATTER_PORT` in production (tests
    /// point it at an ephemeral receiver). `egress` must have at least one
    /// entry; the first is the operating iface (previous single-egress
    /// behavior), later entries (e.g. a Thread TUN) receive the same
    /// datagram in addition.
    ///
    /// per-egress setsockopt 失敗の扱い（監査 I-1）: `new()` はどの egress が
    /// explicit 指定でどれが auto 検出由来かを知らない（そこは呼び出し側 —
    /// `mat-native::Engine::build` — の責務で、explicit 指定の thread iface の
    /// 解決/bind 失敗は build 側で既にハードエラー化されている）。ここでは
    /// 由来を問わず一律「setsockopt に失敗した egress は warn ログを出して
    /// スキップ（脱落）」とし、生存 egress が 1 本でも残れば `Ok` で継続する。
    /// 単一 egress 構成（従来）で失敗すれば生存ゼロになるので従来どおり
    /// `Err`（絶対互換）。全 egress が失敗した場合も `Err`。
    pub fn new(
        egress: Vec<GroupEgress>,
        dest_port: u16,
        fabric_id: u64,
        source_node_id: u64,
        counter: PersistedGroupCounter,
    ) -> std::io::Result<Self> {
        // 各 egress socket に hop limit と IPV6_MULTICAST_IF を焼く
        //（宛先 sockaddr の sin6_scope_id だけでは egress iface を選べない環境が
        // ある（VPN 系の広い v6 経路が multicast の経路解決を勝ち、実機で
        // tailscale0 へ流出 → LAN に出ず 0/7 不達）。IPV6_MULTICAST_IF で明示
        // 固定する。multicast 送信専用オプションなので共有 socket の unicast
        // には影響しない）。
        let mut survivors = Vec::with_capacity(egress.len());
        let mut last_err: Option<std::io::Error> = None;
        for e in egress {
            let result = e
                .transport
                .set_multicast_hops_v6(MULTICAST_HOP_LIMIT)
                .and_then(|()| e.transport.set_multicast_if_v6(e.scope_id));
            match result {
                Ok(()) => survivors.push(e),
                Err(err) => {
                    tracing::warn!(iface = %e.iface, error = %err,
                        "groupcast egress multicast socket option setup failed; dropping this egress");
                    last_err = Some(err);
                }
            }
        }
        if survivors.is_empty() {
            return Err(last_err.unwrap_or_else(|| std::io::Error::other("no egress configured")));
        }
        Ok(Self {
            egress: survivors,
            dest_port,
            fabric_id,
            source_node_id,
            counter,
        })
    }

    /// Fire-and-forget groupcast InvokeRequest: the same encrypted datagram
    /// is sent once per configured egress (no response, no retransmit).
    /// Returns the message counter used and the iface names that accepted
    /// the send, for logging. Only fails (returning the first error) if
    /// every egress fails — a partial failure (e.g. the Thread TUN egress
    /// down while LAN succeeds) still counts as delivered.
    pub async fn send_invoke(
        &mut self,
        creds: &GroupCredentials,
        group_id: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
    ) -> Result<(u32, Vec<String>), GroupSendError> {
        let counter = self.counter.next().map_err(GroupSendError::Io)?;
        let mut ex = [0u8; 2];
        getrandom::getrandom(&mut ex).expect("os rng");
        let datagram = build_group_datagram(
            creds,
            self.source_node_id,
            counter,
            u16::from_le_bytes(ex),
            group_id,
            cluster,
            command,
            fields_tlv,
        )
        .map_err(GroupSendError::Crypto)?;
        let mut sent = Vec::new();
        let mut first_err: Option<std::io::Error> = None;
        for e in &self.egress {
            let dest = SocketAddr::V6(SocketAddrV6::new(
                group_multicast_addr(self.fabric_id, group_id),
                self.dest_port,
                0,
                // multicast 宛先では sin6_scope_id が送出 iface を選ぶ
                e.scope_id,
            ));
            match e.transport.send_to(&datagram, dest).await {
                Ok(_) => sent.push(e.iface.clone()),
                Err(err) => {
                    tracing::warn!(iface = %e.iface, error = %err, "groupcast egress send failed");
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }
        if sent.is_empty() {
            return Err(GroupSendError::Io(
                first_err.unwrap_or_else(|| std::io::Error::other("no egress")),
            ));
        }
        Ok((counter, sent))
    }

    /// 内包 counter の窓ジャンプ（Issue #14 応急処置）。matd 常駐中は counter の
    /// 実体（in-memory + flock）が GroupSender 内にあるため、ここを経由する。
    pub fn bump_counter(&mut self) -> std::io::Result<(u32, u32)> {
        self.counter.jump()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multicast_addr_packs_fabric_and_group() {
        // FF35:0040:FD || fabric_id(8B BE) || 00 || group_id(2B BE)
        assert_eq!(
            group_multicast_addr(0x1122334455667788, 0xaabb),
            std::net::Ipv6Addr::new(0xff35, 0x0040, 0xfd11, 0x2233, 0x4455, 0x6677, 0x8800, 0xaabb)
        );
        assert_eq!(
            group_multicast_addr(1, 10),
            std::net::Ipv6Addr::new(0xff35, 0x0040, 0xfd00, 0, 0, 0, 0x0100, 0x000a)
        );
    }

    fn tmp_counter_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("mat-group-counter-{}-{tag}", std::process::id()))
    }

    #[test]
    fn counter_starts_above_both_sources_plus_epoch() {
        let p = tmp_counter_path("fresh");
        let _ = std::fs::remove_file(&p);
        let mut c = PersistedGroupCounter::load(&p, 1000).unwrap();
        assert_eq!(c.next().unwrap(), 1000 + COUNTER_EPOCH);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_reload_never_reuses_values() {
        let p = tmp_counter_path("reload");
        let _ = std::fs::remove_file(&p);
        let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
        let mut last = 0;
        for _ in 0..10 {
            last = c.next().unwrap();
        }
        drop(c);
        // 再起動相当: chip-tool 側が 0 でも、自前永続値から必ず上へ跳ぶ。
        let mut c2 = PersistedGroupCounter::load(&p, 0).unwrap();
        assert!(c2.next().unwrap() > last);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_gdc_wins_when_larger_than_own_file() {
        let p = tmp_counter_path("gdcwins");
        let _ = std::fs::remove_file(&p);
        drop(PersistedGroupCounter::load(&p, 0).unwrap()); // 小さい自前値を永続化
        let mut c = PersistedGroupCounter::load(&p, 900_000).unwrap();
        assert!(c.next().unwrap() >= 900_000 + COUNTER_EPOCH);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_persists_ahead_across_epoch_boundary() {
        let p = tmp_counter_path("epoch");
        let _ = std::fs::remove_file(&p);
        let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
        let mut prev = None;
        for _ in 0..(COUNTER_EPOCH + 5) {
            let v = c.next().unwrap();
            if let Some(p) = prev {
                assert_eq!(v, p + 1, "strictly sequential across the persist boundary");
            }
            prev = Some(v);
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_corrupt_file_is_an_error() {
        let p = tmp_counter_path("corrupt");
        std::fs::write(&p, "not a number").unwrap();
        assert!(PersistedGroupCounter::load(&p, 0).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_load_is_exclusive_across_handles() {
        let p = tmp_counter_path("flock");
        let _ = std::fs::remove_file(&p);
        let first = PersistedGroupCounter::load(&p, 0).unwrap();
        // 保持中の 2 度目の load は WouldBlock（別プロセスの matd/one-shot 相当。
        // flock は open file description 単位なので同一プロセスでも競合する）。
        let err = PersistedGroupCounter::load(&p, 0).expect_err("second load must fail while held");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        // 解放後は再取得できる。
        drop(first);
        let _again = PersistedGroupCounter::load(&p, 0).expect("load after release");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.lock", p.display())));
    }

    #[test]
    fn counter_jump_advances_one_restart_window_and_persists_ahead() {
        let p = tmp_counter_path("jump");
        let _ = std::fs::remove_file(&p);
        let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
        let first = c.next().unwrap(); // = 4096 (EPOCH)。ceiling = first + EPOCH
        let (from, to) = c.jump().unwrap();
        // from = ジャンプしなかった場合の次回値、to = 旧 ceiling + EPOCH。
        assert_eq!(from, first + 1);
        assert_eq!(to, first + 2 * COUNTER_EPOCH);
        // ジャンプ後の払い出しは to から連番。
        assert_eq!(c.next().unwrap(), to);
        assert_eq!(c.next().unwrap(), to + 1);
        // persist-ahead 不変条件: drop → reload しても値が重ならない。
        drop(c);
        let mut c2 = PersistedGroupCounter::load(&p, 0).unwrap();
        assert!(c2.next().unwrap() > to + 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_jump_twice_is_monotonic() {
        let p = tmp_counter_path("jump2");
        let _ = std::fs::remove_file(&p);
        let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
        let (_, to1) = c.jump().unwrap();
        let (from2, to2) = c.jump().unwrap();
        // 2 回目の from は 1 回目の to（間で next() していないので）。
        assert_eq!(from2, to1);
        assert!(to2 > to1);
        let _ = std::fs::remove_file(&p);
    }

    use crate::im::{CLUSTER_ON_OFF, CMD_ON_OFF_ON, OPCODE_INVOKE_REQUEST, PROTOCOL_ID_IM};
    use crate::kvs::GroupCredentials;
    use crate::message::{Destination, MessageHeader};

    fn test_creds() -> GroupCredentials {
        GroupCredentials {
            session_id: 0x855f,
            encryption_key: [0xDD; 16],
        }
    }

    #[test]
    fn group_datagram_roundtrips_with_group_header() {
        let dg = build_group_datagram(
            &test_creds(),
            0x0001_0001,
            5000,
            0x42,
            10,
            CLUSTER_ON_OFF,
            CMD_ON_OFF_ON,
            None,
        )
        .unwrap();
        // 平文ヘッダ: DSIZ=group(2) + S flag、session type = group。
        let (header, _) = MessageHeader::decode(&dg).unwrap();
        assert_eq!(header.session_id, 0x855f);
        assert_eq!(header.security_flags, GROUP_SECURITY_FLAGS);
        assert_eq!(header.message_counter, 5000);
        assert_eq!(header.source_node_id, Some(0x0001_0001));
        assert_eq!(header.destination, Destination::Group(10));
        // 復号して protocol header / payload を確認（nonce・AAD が正しい証拠）。
        let (h2, proto, payload) =
            crate::crypto::open_message(&test_creds().encryption_key, &dg, 0x0001_0001).unwrap();
        assert_eq!(h2, header);
        assert!(proto.initiator);
        assert!(!proto.needs_ack);
        assert_eq!(proto.opcode, OPCODE_INVOKE_REQUEST);
        assert_eq!(proto.protocol_id, PROTOCOL_ID_IM);
        assert_eq!(
            payload,
            crate::im::encode_group_invoke_request(CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)
        );
    }

    /// A network interface eligible to try as the multicast join/egress
    /// interface for the test below.
    struct McastCandidate {
        name: String,
        index: u32,
    }

    /// Enumerates interfaces that advertise `IFF_UP | IFF_MULTICAST` via
    /// `/sys/class/net/*/flags`, excluding `lo` (which lacks the MULTICAST
    /// flag on Linux — IPv6 multicast never delivers through it, in any
    /// environment). Interfaces reporting `operstate == "up"` are tried
    /// first, since flags alone (as seen on some bridge/veth interfaces)
    /// don't guarantee delivery.
    fn multicast_capable_interfaces() -> Vec<McastCandidate> {
        const IFF_UP: u32 = 0x1;
        const IFF_MULTICAST: u32 = 0x1000;
        let mut up_first = Vec::new();
        let mut rest = Vec::new();
        let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
            return Vec::new();
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "lo" {
                continue;
            }
            let base = entry.path();
            let flags = std::fs::read_to_string(base.join("flags"))
                .ok()
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            if flags & IFF_UP == 0 || flags & IFF_MULTICAST == 0 {
                continue;
            }
            let Some(index) = std::fs::read_to_string(base.join("ifindex"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
            else {
                continue;
            };
            let operstate = std::fs::read_to_string(base.join("operstate")).unwrap_or_default();
            let candidate = McastCandidate { name, index };
            if operstate.trim() == "up" {
                up_first.push(candidate);
            } else {
                rest.push(candidate);
            }
        }
        up_first.sort_by_key(|c| c.index);
        rest.sort_by_key(|c| c.index);
        up_first.extend(rest);
        up_first
    }

    #[tokio::test]
    async fn group_sender_pins_multicast_egress_interface() {
        use crate::transport::UdpTransport;

        // 実機で sin6_scope_id だけでは egress を選べず tailscale0 へ流出した
        // 回帰の防止: new() が IPV6_MULTICAST_IF を scope_id に固定すること。
        let transport = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
        let p = tmp_counter_path("mcastif");
        let _ = std::fs::remove_file(&p);
        let counter = PersistedGroupCounter::load(&p, 0).unwrap();
        let egress = vec![GroupEgress {
            iface: "test".into(),
            transport: std::sync::Arc::clone(&transport),
            scope_id: 1,
        }];
        let _s = GroupSender::new(egress, 5540, 1, 2, counter).unwrap();
        assert_eq!(transport.multicast_if_v6().unwrap(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn group_sender_multicast_loops_back_locally() {
        use crate::transport::UdpTransport;

        let addr = group_multicast_addr(1, 10);
        let mut tried = Vec::new();

        for cand in multicast_capable_interfaces() {
            // Fresh receiver per candidate: a socket can only join a given
            // multicast group once per interface, and candidates that fail
            // to join must not poison the next attempt.
            let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
            let port = recv.local_addr().unwrap().port();
            if recv.join_multicast_v6(&addr, cand.index).is_err() {
                tried.push(format!("{}(idx={}): join failed", cand.name, cand.index));
                continue;
            }

            let p = tmp_counter_path(&format!("sender-{}", cand.index));
            let _ = std::fs::remove_file(&p);
            let counter = PersistedGroupCounter::load(&p, 0).unwrap();
            let transport = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
            let egress = vec![GroupEgress {
                iface: cand.name.clone(),
                transport,
                scope_id: cand.index,
            }];
            let mut s = GroupSender::new(egress, port, 1, 0x0001_0001, counter).unwrap();
            // Send failures must not poison the run either: docker0 / veth* /
            // WSL2's loopback0 advertise IFF_UP|IFF_MULTICAST but carry no
            // IPv6 source address, so the ff35::/16 send fails with
            // EADDRNOTAVAIL — the next candidate (a real NIC) can still
            // deliver.
            let sent_counter = match s
                .send_invoke(&test_creds(), 10, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)
                .await
            {
                Ok((c, _sent)) => c,
                Err(e) => {
                    let _ = std::fs::remove_file(&p);
                    tried.push(format!(
                        "{}(idx={}): send failed: {e:?}",
                        cand.name, cand.index
                    ));
                    continue;
                }
            };

            let mut buf = [0u8; 1280];
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                recv.recv_from(&mut buf),
            )
            .await;
            let _ = std::fs::remove_file(&p);

            match result {
                Ok(Ok((n, _))) => {
                    let (header, _) = MessageHeader::decode(&buf[..n]).unwrap();
                    assert_eq!(header.destination, Destination::Group(10));
                    assert_eq!(header.message_counter, sent_counter);
                    return; // first delivering interface is enough — PASS.
                }
                _ => tried.push(format!("{}(idx={}): no delivery", cand.name, cand.index)),
            }
        }

        panic!(
            "no multicast-capable interface delivered a loopback groupcast \
             datagram (lo excluded — it lacks IFF_MULTICAST on Linux); \
             tried: {tried:?}"
        );
    }

    /// egress リスト方式の中核: 1 本の GroupSender::send_invoke が複数
    /// egress へ同一 datagram を送出する。`lo` は multicast join できないため、
    /// 2 egress とも同じ iface（同じ scope_id）を使い、独立ソケット 1 本で
    /// 同一バイト列が 2 回届くことで「egress ごとに 1 回送出された」ことを
    /// 固定する。join できる候補は `multicast_capable_interfaces` の走査で
    /// 探す（`group_sender_multicast_loops_back_locally` と同じ流儀 —
    /// docker0/veth* 等 join はできても send が ENETUNREACH/EADDRNOTAVAIL で
    /// 落ちる候補があるため、送出失敗も次候補へ送る）。
    #[tokio::test]
    async fn send_invoke_emits_identical_datagram_on_each_egress() {
        let addr = group_multicast_addr(1, 10);
        let mut tried = Vec::new();

        for cand in multicast_capable_interfaces() {
            let recv1 = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
            if recv1.join_multicast_v6(&addr, cand.index).is_err() {
                tried.push(format!("{}(idx={}): join failed", cand.name, cand.index));
                continue;
            }
            let port = recv1.local_addr().unwrap().port();

            let e1 = GroupEgress {
                iface: "egress-a".into(),
                transport: Arc::new(UdpTransport::bind().await.unwrap()),
                scope_id: cand.index,
            };
            let e2 = GroupEgress {
                iface: "egress-b".into(),
                transport: Arc::new(UdpTransport::bind().await.unwrap()),
                scope_id: cand.index,
            };
            let p = tmp_counter_path(&format!("dual-egress-{}", cand.index));
            let _ = std::fs::remove_file(&p);
            let counter = PersistedGroupCounter::load(&p, 0).unwrap();
            let creds = test_creds();
            let mut s = GroupSender::new(vec![e1, e2], port, 1, 0x0001_0001, counter).unwrap();
            let (counter_used, sent) = match s.send_invoke(&creds, 10, 6, 1, None).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = std::fs::remove_file(&p);
                    tried.push(format!(
                        "{}(idx={}): send failed: {e:?}",
                        cand.name, cand.index
                    ));
                    continue;
                }
            };
            assert_eq!(sent, vec!["egress-a".to_string(), "egress-b".to_string()]);
            assert!(counter_used > 0);

            // 同一バイト列が 2 回届く（egress ごとに 1 回）。
            let mut buf1 = [0u8; 1280];
            let r1 = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                recv1.recv_from(&mut buf1),
            )
            .await;
            let mut buf2 = [0u8; 1280];
            let r2 = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                recv1.recv_from(&mut buf2),
            )
            .await;
            let _ = std::fs::remove_file(&p);
            match (r1, r2) {
                (Ok(Ok((n1, _))), Ok(Ok((n2, _)))) => {
                    assert_eq!(&buf1[..n1], &buf2[..n2], "同一 counter の同一 datagram");
                    return; // 配達できる iface が見つかった時点で PASS。
                }
                _ => tried.push(format!("{}(idx={}): no delivery", cand.name, cand.index)),
            }
        }

        panic!(
            "no multicast-capable interface delivered dual-egress groupcast \
             datagrams (lo excluded — it lacks IFF_MULTICAST on Linux); \
             tried: {tried:?}"
        );
    }

    /// 監査 I-1 の中核: egress 2 本のうち 1 本が setsockopt に失敗しても
    /// `GroupSender::new` は Err にならず、生存 egress だけで送信を継続する
    /// （その egress の scope_id への setsockopt 失敗を「不正 ifindex」で
    /// シミュレートする — 例えば otbr-agent 再起動で wpan0 の ifindex が
    /// stale 化した状況に相当）。不正 ifindex への setsockopt が本当に失敗
    /// する保証は環境依存（コンテナ等ではカーネルが緩い場合がある）ため、
    /// 各候補で先に「本当に失敗するか」を確認し、失敗しない環境ではその
    /// 候補をスキップして次を試す。どの候補でも固定できなければ確認不能
    /// として（panic せず）ログのみ残す。
    #[tokio::test]
    async fn new_drops_failing_egress_and_send_invoke_reports_survivors_only() {
        use crate::transport::UdpTransport;

        let addr = group_multicast_addr(1, 10);
        let mut tried = Vec::new();
        const BOGUS_SCOPE_ID: u32 = 0x7fff_fffe;

        for cand in multicast_capable_interfaces() {
            let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
            if recv.join_multicast_v6(&addr, cand.index).is_err() {
                tried.push(format!("{}(idx={}): join failed", cand.name, cand.index));
                continue;
            }
            let port = recv.local_addr().unwrap().port();

            let bad_transport = Arc::new(UdpTransport::bind().await.unwrap());
            if bad_transport.set_multicast_if_v6(BOGUS_SCOPE_ID).is_ok() {
                // この環境は bogus ifindex への setsockopt を許してしまう —
                // 失敗を再現できないので次候補へ（skip、fail 扱いにしない）。
                tried.push(format!(
                    "{}(idx={}): bogus scope_id {BOGUS_SCOPE_ID} accepted by this \
                     environment, cannot exercise the failure path",
                    cand.name, cand.index
                ));
                continue;
            }

            let good = GroupEgress {
                iface: "egress-good".into(),
                transport: Arc::new(UdpTransport::bind().await.unwrap()),
                scope_id: cand.index,
            };
            let bad = GroupEgress {
                iface: "egress-bad".into(),
                transport: bad_transport,
                scope_id: BOGUS_SCOPE_ID,
            };

            let p = tmp_counter_path(&format!("bad-egress-drop-{}", cand.index));
            let _ = std::fs::remove_file(&p);
            let counter = PersistedGroupCounter::load(&p, 0).unwrap();
            let mut s = GroupSender::new(vec![good, bad], port, 1, 0x0001_0001, counter)
                .expect("生存 egress が1本ある限り Ok（監査 I-1）");
            assert_eq!(
                s.egress.len(),
                1,
                "setsockopt に失敗した egress は脱落しているはず"
            );
            assert_eq!(s.egress[0].iface, "egress-good");

            let creds = test_creds();
            let (_, sent) = match s.send_invoke(&creds, 10, 6, 1, None).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = std::fs::remove_file(&p);
                    tried.push(format!(
                        "{}(idx={}): send failed: {e:?}",
                        cand.name, cand.index
                    ));
                    continue;
                }
            };
            assert_eq!(
                sent,
                vec!["egress-good".to_string()],
                "sent には生存 egress 名だけが入る"
            );

            let mut buf = [0u8; 1280];
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                recv.recv_from(&mut buf),
            )
            .await;
            let _ = std::fs::remove_file(&p);
            match result {
                Ok(Ok(_)) => return, // 配達確認まで取れた時点で PASS。
                _ => tried.push(format!("{}(idx={}): no delivery", cand.name, cand.index)),
            }
        }
        eprintln!(
            "no candidate could exercise the bad-egress-drop path in this environment \
             (skip, not a failure); tried: {tried:?}"
        );
    }

    /// 監査 I-1: 全 egress の setsockopt が失敗する場合は従来どおり `Err`
    /// のまま（単一 egress 構成の絶対互換維持）。
    #[tokio::test]
    async fn new_returns_err_when_all_egress_fail() {
        use crate::transport::UdpTransport;
        const BOGUS_SCOPE_ID: u32 = 0x7fff_fffe;

        let transport = Arc::new(UdpTransport::bind().await.unwrap());
        if transport.set_multicast_if_v6(BOGUS_SCOPE_ID).is_ok() {
            eprintln!(
                "environment accepts bogus scope_id {BOGUS_SCOPE_ID}; cannot exercise \
                 the all-fail path, skipping"
            );
            return;
        }

        let egress = vec![GroupEgress {
            iface: "egress-bad".into(),
            transport,
            scope_id: BOGUS_SCOPE_ID,
        }];
        let p = tmp_counter_path("all-egress-fail");
        let _ = std::fs::remove_file(&p);
        let counter = PersistedGroupCounter::load(&p, 0).unwrap();
        let result = GroupSender::new(egress, 5540, 1, 1, counter);
        assert!(
            result.is_err(),
            "全 egress が setsockopt 失敗なら Err のまま（後方互換）"
        );
        let _ = std::fs::remove_file(&p);
    }
}
