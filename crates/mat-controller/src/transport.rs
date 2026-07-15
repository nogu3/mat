//! UDP transport (IPv6, Matter port 5540).

use std::io;
use std::net::SocketAddr;

use tokio::net::UdpSocket;

/// Matter caps UDP payloads at 1280 bytes (IPv6 minimum MTU).
pub const MAX_DATAGRAM: usize = 1280;

pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Binds an ephemeral IPv6 port for controller use.
    pub async fn bind() -> io::Result<Self> {
        Self::bind_addr("[::]:0".parse().expect("static addr")).await
    }

    pub async fn bind_addr(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(addr).await?,
        })
    }

    pub async fn send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        let n = self.socket.send_to(buf, dest).await?;
        if n != buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short datagram send",
            ));
        }
        Ok(())
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Sets the hop limit for multicast sends. The OS default of 1 never
    /// crosses the border router, so groupcast callers must raise it
    /// (Matter SDK uses 64). Unicast sends are unaffected.
    pub fn set_multicast_hops_v6(&self, hops: u32) -> io::Result<()> {
        socket2::SockRef::from(&self.socket).set_multicast_hops_v6(hops)
    }

    /// Pins the egress interface for multicast sends. Relying on the
    /// destination's `sin6_scope_id` is not enough: with overlapping IPv6
    /// routes (e.g. a VPN/tailscale device) the kernel can route a
    /// site-scope ff35:: datagram out the wrong interface (observed live —
    /// the groupcast left via tailscale0 and never reached the LAN).
    /// Unicast sends are unaffected.
    pub fn set_multicast_if_v6(&self, ifindex: u32) -> io::Result<()> {
        socket2::SockRef::from(&self.socket).set_multicast_if_v6(ifindex)
    }

    /// Reads back the multicast egress interface (0 = kernel default).
    pub fn multicast_if_v6(&self) -> io::Result<u32> {
        socket2::SockRef::from(&self.socket).multicast_if_v6()
    }
}

use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Reliable 経路（BTP 等）で recv_from が返す固定の擬似 peer アドレス。
/// exchange 層の from==peer スクリーニングを素通しするための marker で、
/// 実在の宛先ではない。
pub const RELIABLE_PEER: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5541);

/// 順序・信頼性を transport 自身が保証する経路（BTP）のメッセージ土管。
/// チャネルの 1 要素 = Matter メッセージ 1 通（データグラム等価）。
pub struct ReliableChannel {
    tx: mpsc::Sender<Vec<u8>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl ReliableChannel {
    pub fn new(tx: mpsc::Sender<Vec<u8>>, rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            tx,
            rx: Mutex::new(rx),
        }
    }

    /// クロス接続されたループバック対（テスト用）。
    pub fn pair() -> (Transport, Transport) {
        let (atx, brx) = mpsc::channel(8);
        let (btx, arx) = mpsc::channel(8);
        (
            Transport::Reliable(Self::new(atx, arx)),
            Transport::Reliable(Self::new(btx, brx)),
        )
    }
}

/// セッション層が使うメッセージ transport。Udp は MRP あり、Reliable
/// （BTP）は transport 自身が信頼性を持つため MRP なし（spec §4.12.2）。
pub enum Transport {
    Udp(Arc<UdpTransport>),
    Reliable(ReliableChannel),
}

impl Transport {
    pub fn is_reliable(&self) -> bool {
        matches!(self, Transport::Reliable(_))
    }

    pub async fn send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        match self {
            Transport::Udp(u) => u.send_to(buf, dest).await,
            Transport::Reliable(c) => {
                c.tx.send(buf.to_vec()).await.map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "reliable channel closed")
                })
            }
        }
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Transport::Udp(u) => u.recv_from(buf).await,
            Transport::Reliable(c) => {
                let msg = c.rx.lock().await.recv().await.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "reliable channel closed")
                })?;
                let n = msg.len().min(buf.len());
                buf[..n].copy_from_slice(&msg[..n]);
                Ok((n, RELIABLE_PEER))
            }
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Transport::Udp(u) => u.local_addr(),
            Transport::Reliable(_) => Ok(RELIABLE_PEER),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrips_datagram_over_loopback() {
        let a = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let b = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        a.send_to(b"ping", b.local_addr().unwrap()).await.unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = b.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(from, a.local_addr().unwrap());
    }

    #[tokio::test]
    async fn reliable_pair_roundtrips_messages() {
        let (a, b) = ReliableChannel::pair();
        a.send_to(b"ping", RELIABLE_PEER).await.unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = b.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(from, RELIABLE_PEER);
        assert!(a.is_reliable());
    }
}
