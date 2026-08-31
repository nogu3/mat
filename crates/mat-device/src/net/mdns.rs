//! mDNS advertiser socket loop (spec §4.3; RFC 6762). Owns the `ff02::fb`
//! multicast socket, holds the current commissionable/operational adverts
//! behind a lock, answers incoming queries with
//! `core::mdns_records::encode_{commissionable,operational}_response`, and
//! proactively announces (RFC 6762 §8.3) or says goodbye (§10.1, TTL=0) on
//! its own initiative whenever the advert set changes — see `announce`,
//! `set_commissionable`, `add_operational`, `remove_operational` below.
//!
//! Thin by design: all RR framing lives in `core::mdns_records` (pure,
//! tested independently — see that module's `tests`); this file is just the
//! socket plumbing plus the "which advert(s) does this query name match"
//! dispatch, and, for the advert-mutating methods, the "which advert(s)
//! changed" send.

use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use mat_controller::sync::{read_locked, write_locked};
use tokio::net::UdpSocket;

use crate::core::mdns_records::{
    encode_commissionable_response, encode_goodbye, encode_operational_response,
    encode_unsolicited_announcement, parse_questions, CommissionableAdvert, OperationalAdvert,
};

const MDNS_GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_PORT: u16 = 5353;
/// RFC 6762 §8.3/§10.1: an unsolicited announcement or a goodbye SHOULD be
/// sent twice, about a second apart, to guard against a single lost packet.
const ANNOUNCE_REPEAT_DELAY: Duration = Duration::from_secs(1);
/// mDNS messages are conventionally bounded to the classic UDP-safe
/// payload; our own responses are small (a handful of records), but an
/// incoming query could in principle be larger (e.g. many known-answer PTR
/// records from a browsing peer) — size generously like
/// `mat_controller::dnssd`'s browse listener does.
const RECV_BUF: usize = 9000;

/// Binds the advertiser's mDNS socket: `[::]:5353` with address reuse,
/// joined to `ff02::fb` on `scope_id`.
///
/// Mirrors `mat_controller::dnssd::bind_mdns_socket` exactly on the
/// reuse-flag choice: `SO_REUSEADDR` only, deliberately **not**
/// `SO_REUSEPORT`. `dnssd.rs`'s doc comment (lines 135-148) records the
/// reason, and it applies here just as much as to the querier: on Linux,
/// `SO_REUSEPORT` puts same-port sockets into a load-balancing group that
/// hashes *each* incoming datagram — multicast included — to a single
/// member. A responder sharing port 5353 with a system mDNS daemon (avahi,
/// the default on the deployment target) would then have queries land on
/// only one of the two sockets at random, so this advertiser would
/// silently miss a fraction of the queries it's supposed to answer.
/// `SO_REUSEADDR` alone already delivers multicast to *every* socket bound
/// to the port that joined the group, which is what coexistence with avahi
/// (and, incidentally, multiple test-run instances) actually needs.
///
/// `set_multicast_loop_v6(true)` is set — off by the OS default — so a
/// query this socket itself sends (there are none from this struct today,
/// but the live/e2e test's *querier* runs in the same process/netns via
/// loopback) can be answered and looped back locally; see
/// `tests/discover_live.rs`.
fn bind_advertiser_socket(scope_id: u32) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.set_only_v6(true)?;
    sock.set_nonblocking(true)?;
    let bind = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, MDNS_PORT, 0, 0);
    sock.bind(&SocketAddr::V6(bind).into())?;
    sock.join_multicast_v6(&MDNS_GROUP, scope_id)?;
    sock.set_multicast_if_v6(scope_id)?;
    sock.set_multicast_hops_v6(255)?;
    sock.set_multicast_loop_v6(true)?;
    UdpSocket::from_std(sock.into())
}

/// The device-side mDNS advertiser: one multicast socket, a background
/// receive loop, and the current commissionable/operational adverts it
/// answers queries with. `Arc`-shared so `spawn`'s caller can update
/// adverts (`set_commissionable`/`add_operational`) while the background
/// task keeps answering queries with whatever is current at receive time.
pub struct MdnsAdvertiser {
    sock: UdpSocket,
    scope_id: u32,
    commissionable: RwLock<Option<CommissionableAdvert>>,
    operational: RwLock<Vec<OperationalAdvert>>,
}

impl MdnsAdvertiser {
    /// Binds the socket and spawns the background receive/answer loop.
    /// Must be called from within a tokio runtime (spawns onto it).
    pub async fn spawn(iface_scope: u32) -> Result<Arc<Self>, io::Error> {
        let sock = bind_advertiser_socket(iface_scope)?;
        let this = Arc::new(MdnsAdvertiser {
            sock,
            scope_id: iface_scope,
            commissionable: RwLock::new(None),
            operational: RwLock::new(Vec::new()),
        });
        let bg = Arc::clone(&this);
        tokio::spawn(async move { bg.serve().await });
        Ok(this)
    }

    /// Cloned-out snapshot of the current advert set — taken out from
    /// under both locks so a caller can build/send a message without
    /// holding either lock across an `.await` (a `RwLockReadGuard` isn't
    /// `Send`, and every send here happens on a socket, i.e. async).
    fn snapshot(&self) -> (Option<CommissionableAdvert>, Vec<OperationalAdvert>) {
        let commissionable = read_locked(&self.commissionable).clone();
        let operational = read_locked(&self.operational).clone();
        (commissionable, operational)
    }

    /// Multicast destination for announcements/goodbyes — the same group
    /// and port `serve`'s non-QU replies already target.
    fn multicast_dest(&self) -> SocketAddr {
        SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, self.scope_id))
    }

    /// Sends `msg` to the multicast group once now, then again after
    /// `ANNOUNCE_REPEAT_DELAY` via `tokio::spawn` (RFC 6762 §8.3/§10.1's
    /// "send twice, ~1s apart" cadence — see that const's doc). The second
    /// send is fire-and-forget: callers (an advert-mutating method
    /// returning to `serve_secured_message`, ultimately) must not block an
    /// extra second on every advert change, and losing the repeat to a
    /// send error is no worse than losing the first send to packet loss —
    /// either way the advertiser is still correct, just less redundant.
    async fn send_doubled(self: &Arc<Self>, msg: Vec<u8>) {
        let dest = self.multicast_dest();
        let _ = self.sock.send_to(&msg, dest).await;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(ANNOUNCE_REPEAT_DELAY).await;
            let dest = this.multicast_dest();
            let _ = this.sock.send_to(&msg, dest).await;
        });
    }

    /// Sends an unsolicited multicast announcement (RFC 6762 §8.3) of the
    /// current advert set — every record for whichever commissionable/
    /// operational adverts are set right now, doubled per
    /// `send_doubled`. Called after every advert-mutating method below,
    /// and once more right after `bring_up_mdns` finishes its initial
    /// setup (`net::runtime`) — so a device joining the network
    /// proactively tells the LAN about itself instead of waiting to be
    /// asked.
    pub async fn announce(self: &Arc<Self>) {
        let (commissionable, operational) = self.snapshot();
        let msg = encode_unsolicited_announcement(commissionable.as_ref(), &operational);
        self.send_doubled(msg).await;
    }

    /// Sets (or clears, with `None`) the commissionable advert, then
    /// announces the resulting advert set. Clearing first sends a goodbye
    /// (TTL=0, RFC 6762 §10.1) for the *outgoing* commissionable advert
    /// alone (the operational adverts aren't changing here, so they're
    /// left out of this particular goodbye) — so peers purge it from
    /// cache immediately instead of waiting out its TTL — before the
    /// state actually changes.
    pub async fn set_commissionable(self: &Arc<Self>, ad: Option<CommissionableAdvert>) {
        if ad.is_none() {
            let old = read_locked(&self.commissionable).clone();
            if let Some(old) = old {
                let goodbye = encode_goodbye(Some(&old), &[]);
                self.send_doubled(goodbye).await;
            }
        }
        *write_locked(&self.commissionable) = ad;
        self.announce().await;
    }

    /// Adds one operational advert (e.g. one per commissioned fabric),
    /// then announces the resulting advert set.
    pub async fn add_operational(self: &Arc<Self>, ad: OperationalAdvert) {
        write_locked(&self.operational).push(ad);
        self.announce().await;
    }

    /// Removes the operational advert matching `(compressed_fabric_id,
    /// node_id)` — e.g. a fail-safe expiry (`net::runtime`'s deadline
    /// timer, Task 8) rolling back an uncommitted `AddNOC`'s fabric.
    /// Sends a goodbye for *that* advert alone first (same
    /// goodbye-before-state-change ordering as `set_commissionable(None)`
    /// above), then removes it. A no-op if no advert currently matches —
    /// defensive; not expected to happen given the only caller computes
    /// the identity from the very entry it just removed from the fabric
    /// store.
    pub async fn remove_operational(self: &Arc<Self>, compressed_fabric_id: u64, node_id: u64) {
        let cfid = compressed_fabric_id.to_be_bytes();
        let matches =
            |ad: &&OperationalAdvert| ad.compressed_fabric_id == cfid && ad.node_id == node_id;
        let target = read_locked(&self.operational).iter().find(matches).cloned();
        let Some(target) = target else {
            return;
        };
        let goodbye = encode_goodbye(None, std::slice::from_ref(&target));
        self.send_doubled(goodbye).await;
        write_locked(&self.operational)
            .retain(|ad| !(ad.compressed_fabric_id == cfid && ad.node_id == node_id));
    }

    /// Receive loop: reads queries, matches each distinct question name
    /// against the current adverts, and replies. I/O errors on `recv_from`
    /// are not fatal (mirrors `mat_controller::dnssd`'s listener loops) —
    /// this task runs for the process lifetime.
    async fn serve(self: Arc<Self>) {
        let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, self.scope_id));
        let mut buf = vec![0u8; RECV_BUF];
        loop {
            let Ok((n, from)) = self.sock.recv_from(&mut buf).await else {
                continue;
            };
            let questions = parse_questions(&buf[..n]);
            if questions.is_empty() {
                continue;
            }
            self.answer(&questions, from, dest).await;
        }
    }

    /// Answers each distinct question name in `questions` (a query commonly
    /// asks e.g. SRV+TXT for the same name in one message — matching by
    /// name alone, once, already returns everything relevant; see
    /// `core::mdns_records`' per-name dispatch). QU (top bit of the
    /// question's class) routes the reply unicast to `from`; otherwise it
    /// goes to the multicast group `dest`.
    async fn answer(&self, questions: &[(String, u16, bool)], from: SocketAddr, dest: SocketAddr) {
        let mut answered: Vec<&str> = Vec::new();
        for (name, _qtype, qu) in questions {
            if answered.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                continue;
            }
            answered.push(name.as_str());
            let target = if *qu { from } else { dest };

            // Clone the (small) advert snapshot out from under the lock
            // before any `.await` — a `RwLockReadGuard` isn't `Send`, and
            // this task must stay `Send` for `tokio::spawn`.
            let commissionable = read_locked(&self.commissionable).clone();
            let operational = read_locked(&self.operational).clone();

            if let Some(ad) = &commissionable {
                if let Some(msg) = encode_commissionable_response(name, ad, *qu) {
                    let _ = self.sock.send_to(&msg, target).await;
                }
            }
            for ad in &operational {
                if let Some(msg) = encode_operational_response(name, ad, *qu) {
                    let _ = self.sock.send_to(&msg, target).await;
                }
            }
        }
    }
}
