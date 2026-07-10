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
}
