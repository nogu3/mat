//! mDNS advertiser socket loop (spec §4.3; RFC 6762). Owns the `ff02::fb`
//! multicast socket, holds the current commissionable/operational adverts
//! behind a lock, and answers incoming queries with
//! `core::mdns_records::encode_{commissionable,operational}_response`.
//!
//! Thin by design: all RR framing lives in `core::mdns_records` (pure,
//! tested independently — see that module's `tests`); this file is just the
//! socket plumbing plus the "which advert(s) does this query name match"
//! dispatch.

use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::{Arc, RwLock};

use tokio::net::UdpSocket;

use crate::core::mdns_records::{
    encode_commissionable_response, encode_operational_response, parse_questions,
    CommissionableAdvert, OperationalAdvert,
};

const MDNS_GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_PORT: u16 = 5353;
/// mDNS messages are conventionally bounded to the classic UDP-safe
/// payload; our own responses are small (a handful of records), but an
/// incoming query could in principle be larger (e.g. many known-answer PTR
/// records from a browsing peer) — size generously like
/// `mat_controller::dnssd`'s browse listener does.
const RECV_BUF: usize = 9000;

/// Binds the advertiser's mDNS socket: `[::]:5353` with address reuse,
/// joined to `ff02::fb` on `scope_id`.
///
/// Mirrors `mat_controller::dnssd::bind_mdns_socket` (see that function's
/// doc comment for the SO_REUSEADDR-not-SO_REUSEPORT rationale — sharing
/// port 5353 with a system mDNS daemon like avahi must still deliver
/// multicast to us). One addition here: `SO_REUSEPORT` is also attempted
/// (best-effort, brief allows "if available") purely so multiple advertiser
/// instances (e.g. tests run concurrently) don't fail to bind outright —
/// production only ever runs one `MdnsAdvertiser` per device process.
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
    let _ = sock.set_reuse_port(true); // best-effort; see doc comment above
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

    /// Sets (or clears, with `None`) the commissionable advert. Takes
    /// effect for the next query the background loop receives — no
    /// unsolicited announcement is sent on change (see module doc: this
    /// driver is query-answering only, matching the brief's minimal scope;
    /// `core::mdns_records::encode_unsolicited_announcement` exists and is
    /// tested but this loop doesn't call it proactively yet).
    pub fn set_commissionable(&self, ad: Option<CommissionableAdvert>) {
        *self
            .commissionable
            .write()
            .expect("mdns commissionable lock poisoned") = ad;
    }

    /// Adds one operational advert (e.g. one per commissioned fabric).
    /// There is no corresponding remove — fabrics are removed by process
    /// restart in this M1 scope (matches the brief's interface, which only
    /// lists `add_operational`).
    pub fn add_operational(&self, ad: OperationalAdvert) {
        self.operational
            .write()
            .expect("mdns operational lock poisoned")
            .push(ad);
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
            let commissionable = self
                .commissionable
                .read()
                .expect("mdns commissionable lock poisoned")
                .clone();
            let operational = self
                .operational
                .read()
                .expect("mdns operational lock poisoned")
                .clone();

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
