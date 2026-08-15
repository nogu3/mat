//! Device runtime assembly: `DeviceConfig` (what to advertise/answer as)
//! plus `Device` (the running instance — bind, generate/load persistent
//! state synchronously in `new`, then `run` forever). This is where all of
//! Task 4-11's parts (PASE/CASE cores, commissioning server, fabric store,
//! mDNS advertiser, data model) get wired into one thing `mat commission`
//! can actually talk to — the async loop itself lives in
//! `net::runtime::run` (tokio, kept out of this file per the brief's file
//! plan).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use mat_controller::setup_code::{self, SetupPayload};
use mat_controller::transport::{Transport, UdpTransport};
use mat_controller::x509::{self, X509Error};

use crate::core::commissioning::CommissioningServer;
use crate::core::datamodel::Node;
use crate::core::fabric_store::FabricStore;
use crate::net::store::store_in_dir;

/// spec §5.1.4.2 `DiscoveryCapabilitiesBitmask`: bit 2 = on-network. This
/// device advertises no other discovery capability (no BLE/SoftAP) — same
/// value `mat_controller::commissioning`'s own `build_window_qr` uses for
/// the same reason.
const DISCOVERY_CAPABILITY_ON_NETWORK: u8 = 0x04;

/// What this device advertises/answers as. Loaded once at `Device::new` and
/// otherwise immutable for the process lifetime (M1 scope: no runtime
/// reconfiguration).
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    pub passcode: u32,
    /// 12-bit onboarding discriminator (spec §5.1.4.2).
    pub discriminator: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    /// UDP port to bind (`0` = ephemeral — the actual bound port is then
    /// available via `Device::local_addr`).
    pub port: u16,
    /// Fabric persistence root — `<store_dir>/fabrics.json`
    /// (`net::store::store_in_dir`) plus `<store_dir>/paa/paa.der` (the dev
    /// PAA DER a commissioner's `--paa-dir`/`MAT_PAA_TRUST_STORE` needs to
    /// verify this device's attestation chain).
    pub store_dir: PathBuf,
    /// mDNS/UDP egress interface name (e.g. `"eth0"`). **Not** in the
    /// brief's illustrative `DeviceConfig` field list — added because
    /// `net::mdns::MdnsAdvertiser::spawn` needs a concrete interface scope
    /// id to join the `ff02::fb` multicast group on, and there is no
    /// correct default to guess on a multi-interface host (this
    /// workspace's own `mat-native::iface_select` refuses to guess for the
    /// same reason — see its doc comment). Documented deviation; see
    /// task-12-report.md.
    pub iface: String,
}

/// `Device::new`/`Device::run` failure.
#[derive(Debug)]
pub enum DeviceError {
    Io(std::io::Error),
    /// Dev attestation chain generation failed (never expected in
    /// practice — `x509::generate_dev_attestation` only fails on OS RNG
    /// exhaustion; kept as a real variant rather than `.expect()` since
    /// `Device::new` is a fallible constructor per the brief).
    Attestation(X509Error),
    /// `mat_controller::dnssd::iface_index(&config.iface)` failed (no such
    /// interface).
    IfaceIndex(std::io::Error),
    /// The named interface has no IPv6 link-local address to advertise
    /// (`/proc/net/if_inet6` had no scope-0x20 entry for it).
    Iface(String),
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceError::Io(e) => write!(f, "device: io error: {e}"),
            DeviceError::Attestation(e) => write!(f, "device: dev attestation generation: {e}"),
            DeviceError::IfaceIndex(e) => write!(f, "device: resolve interface: {e}"),
            DeviceError::Iface(msg) => write!(f, "device: {msg}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// A running (or about-to-run) device instance. Construct with `new`
/// (synchronous — binds the socket and loads/creates all persistent state
/// eagerly, so `local_addr`/`qr_payload`/`manual_code` are all usable
/// before `run` is ever called), then hand it to `run` to serve forever.
pub struct Device {
    config: DeviceConfig,
    transport: Arc<Transport>,
    local_addr: SocketAddr,
    node: Node,
    comm_server: CommissioningServer,
}

impl Device {
    /// Builds a device: generates a fresh dev DAC/PAI/PAA attestation chain
    /// (spec-irrelevant after commissioning completes — see
    /// `net::runtime`'s module doc — so regenerating it every boot is
    /// fine), writes the PAA DER to `<store_dir>/paa/paa.der`, loads any
    /// already-persisted fabrics from `<store_dir>/fabrics.json`, and binds
    /// the UDP socket.
    ///
    /// **Must be called from within a running tokio runtime.** `new` is
    /// synchronous per the brief's interface (`bind`/`bind_addr` need
    /// `.await`; `UdpTransport::from_std` sidesteps that by binding via
    /// `std::net::UdpSocket` and handing the already-bound socket to the
    /// current tokio reactor — same requirement as
    /// `tokio::net::UdpSocket::from_std` itself).
    pub fn new(config: DeviceConfig) -> Result<Self, DeviceError> {
        std::fs::create_dir_all(&config.store_dir).map_err(DeviceError::Io)?;

        let dev = x509::generate_dev_attestation(config.vendor_id, config.product_id)
            .map_err(DeviceError::Attestation)?;
        let paa_dir = config.store_dir.join("paa");
        std::fs::create_dir_all(&paa_dir).map_err(DeviceError::Io)?;
        std::fs::write(paa_dir.join("paa.der"), &dev.paa_der).map_err(DeviceError::Io)?;

        let fabric_store = FabricStore::with_persist(Box::new(store_in_dir(&config.store_dir)));
        let comm_server = CommissioningServer::new(dev, fabric_store);

        let mut node = Node::with_root_endpoint();
        let (general_commissioning, operational_credentials) = comm_server.into_cluster_handlers();
        node.add_cluster(0, general_commissioning);
        node.add_cluster(0, operational_credentials);

        let bind_addr: SocketAddr = format!("[::]:{}", config.port)
            .parse()
            .expect("well-formed IPv6 wildcard address");
        let std_socket = std::net::UdpSocket::bind(bind_addr).map_err(DeviceError::Io)?;
        let udp = UdpTransport::from_std(std_socket).map_err(DeviceError::Io)?;
        let local_addr = udp.local_addr().map_err(DeviceError::Io)?;
        let transport = Arc::new(Transport::Udp(Arc::new(udp)));

        Ok(Self {
            config,
            transport,
            local_addr,
            node,
            comm_server,
        })
    }

    /// The socket's actual bound address (resolves `DeviceConfig::port ==
    /// 0` to the ephemeral port the OS picked) — a direct-drive test (no
    /// mDNS) uses this to reach the device without discovery.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// QR onboarding payload (`MT:...`, spec §5.1.4) — `mat commission
    /// --setup-code`'s input for the QR path.
    pub fn qr_payload(&self) -> String {
        setup_code::encode_qr(&SetupPayload {
            version: 0,
            vendor_id: self.config.vendor_id,
            product_id: self.config.product_id,
            custom_flow: 0,
            discovery_capabilities: DISCOVERY_CAPABILITY_ON_NETWORK,
            discriminator: self.config.discriminator,
            passcode: self.config.passcode,
        })
    }

    /// 11-digit manual pairing code (spec §5.1.3) — the non-QR alternative.
    pub fn manual_code(&self) -> String {
        setup_code::encode_manual_code(self.config.passcode, (self.config.discriminator >> 8) as u8)
    }

    /// Serves the device forever: PASE commissioning, CASE, and secured
    /// Interaction Model traffic, sequentially (see `net::runtime`'s module
    /// doc for the full wire-classification contract). **Never returns on
    /// its own** during normal operation — the caller cancels/aborts the
    /// future (e.g. on Ctrl-C) to stop the device. mDNS bring-up is
    /// best-effort: if it fails (bad interface name, no link-local address
    /// yet, socket bind failure), the device still serves PASE/CASE/IM to
    /// any peer that already has its address, and the runtime retries
    /// bringing mDNS up in the background on a bounded backoff (see
    /// `net::runtime`'s `MdnsRetry`) rather than giving up or returning an
    /// error.
    pub async fn run(self) -> Result<(), DeviceError> {
        crate::net::runtime::run(
            self.transport,
            self.local_addr,
            self.config,
            self.node,
            self.comm_server,
        )
        .await
    }
}
