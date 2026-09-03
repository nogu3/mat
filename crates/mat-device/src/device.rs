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

use std::sync::atomic::AtomicBool;

use crate::core::commissioning::CommissioningServer;
use crate::core::datamodel::{DescriptorHandler, Node};
use crate::core::fabric_store::FabricStore;
use crate::net::store::{basic_info_in_dir, load_basic_info, store_in_dir};

/// spec §5.1.4.2 `DiscoveryCapabilitiesBitmask`: bit 2 = on-network. This
/// device advertises no other discovery capability (no BLE/SoftAP) — same
/// value `mat_controller::commissioning`'s own `build_window_qr` uses for
/// the same reason.
const DISCOVERY_CAPABILITY_ON_NETWORK: u8 = 0x04;

/// Which dev attestation chain `Device::new` builds. Task 10 (M2 Echo
/// checkpoint) addition: a from-scratch commissions fine with chip-tool but
/// Echo aborts after AttestationRequest (cloud-side validation) — the
/// working hypothesis is that Echo accepts the canonical connectedhomeip
/// test chain (the one HA Matter Hub / matter.js-based devices use
/// verbatim) but rejects our self-generated-every-boot one. `ChipTest`
/// switches to that vendored chain (`crate::chip_test_attestation`) so the
/// hypothesis can be tested against a real Echo without touching the
/// default `Self_` path chip-tool e2e gates rely on.
/// `serde::Deserialize` derived directly on this enum (rather than a
/// separate string field in `matv`'s `FileConfig`) so `matv.toml`'s
/// `attestation = "self" | "chip-test"` maps straight onto it — one enum,
/// one place that knows the two spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub enum AttestationMode {
    /// Fresh self-generated PAA/PAI/DAC chain every boot
    /// (`x509::generate_dev_attestation`) plus a locally-built CD signed
    /// with the public Matter test CD signing key. Default — unchanged
    /// M1/M2 behavior, what the chip-tool e2e gates exercise.
    #[default]
    #[serde(rename = "self")]
    Self_,
    /// The vendored canonical connectedhomeip test chain (VID FFF1 / PID
    /// 8000) — see `crate::chip_test_attestation`'s module doc.
    #[serde(rename = "chip-test")]
    ChipTest,
}

/// 設定ファイル 1 `[[device]]` 分 — Aggregator (EP1) 配下にぶら下がる
/// bridged endpoint 1 つ。`id` は endpoint 採番台帳
/// (`net::endpoint_ledger`) のキーで、一度使ったら改名してはいけない
/// （改名は「別デバイスの新規追加 + 旧デバイスの削除」として扱われ、
/// コントローラ側のアクセサリ対応が切れる）。`name` は Bridged Device
/// Basic Information の NodeLabel。
///
/// バリデーション（`id` の一意性・非空、`name` の 32 文字上限）は
/// `matv::load_config` の責務 — `Device::new` は渡されたものをそのまま
/// 組むだけで、設定ファイルの妥当性判断は持たない。
#[derive(Debug, Clone)]
pub struct VirtualDeviceConfig {
    pub id: String,
    pub kind: crate::core::bridge::DeviceKind,
    pub name: String,
}

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
    /// Which dev attestation chain to build (Task 10 addition). Defaults
    /// to [`AttestationMode::Self_`] — existing chip-tool e2e behavior is
    /// unchanged unless a caller opts into `ChipTest` explicitly.
    pub attestation: AttestationMode,
    /// M3: Aggregator (EP1) 配下に載せる bridged device 群、設定ファイルの
    /// `[[device]]` 宣言順。空でも `Device::new` は通る（EP1 の PartsList が
    /// 空の Aggregator になるだけ）— 「1 台以上必要」の判断は
    /// `matv::load_config` 側。
    pub devices: Vec<VirtualDeviceConfig>,
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

/// BasicInformation's UniqueID (spec §11.1.6.15) — a per-install identifier
/// that must survive restarts (unlike `NodeLabel`'s in-memory-only value in
/// `core::datamodel::BasicInformationHandler` — see that struct's doc).
/// Reads `<store_dir>/unique_id` if it already exists; otherwise generates
/// 16 random bytes (`getrandom`, the same OS RNG every other dev-
/// attestation/nonce generation in this crate uses) hex-encoded to 32
/// characters, persists that to the same path, and returns it — so the
/// value picked on first boot is the one every later boot reuses.
fn load_or_create_unique_id(store_dir: &std::path::Path) -> std::io::Result<String> {
    let path = store_dir.join("unique_id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| std::io::Error::other(format!("os rng: {e}")))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, &hex)?;
    Ok(hex)
}

/// 1 つの bridged endpoint (`[[device]]` 1 件) の Bridged Device Basic
/// Information UniqueID。
///
/// 素直に `format!("{node_unique_id}-{device_id}")` と繋ぐと **spec の
/// `string32` 上限を必ず超える** — `node_unique_id` 自体が 32 hex 文字
/// (`load_or_create_unique_id`) なので、区切りの `-` と device id を足した
/// 時点で 34 文字以上になる。BDBI の UniqueID は BasicInformation
/// (spec §11.1.6.15) と同じ `string32` 制約を継ぐので、そのまま載せると
/// 上限超過の値がワイヤに出る（Apple Home の interview で落ちうる）。
///
/// そこで同じ合成文字列の SHA-256 を取り、先頭 16 バイト = 32 hex 文字を
/// UniqueID とする。node unique id と device id の両方に依存するので
/// node を跨いでも device を跨いでも衝突せず、どちらも変わらない限り
/// 再起動を跨いで同じ値になる（コントローラ側のアクセサリ対応が安定
/// する）。
fn bridged_unique_id(node_unique_id: &str, device_id: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(format!("{node_unique_id}-{device_id}").as_bytes());
    // 16 バイト = 32 hex 文字。`load_or_create_unique_id` の node 側と同じ
    // 「16 ランダムバイトの hex」形と揃う。
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

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
    /// 各 bridged device の `(設定ファイルの id, OnOff 状態ハンドル)` —
    /// 宣言順。`Device` 自身はまだ読まない（M4 で mando への転送/状態
    /// ログが消費する）。
    #[allow(dead_code)]
    onoff_states: Vec<(String, Arc<AtomicBool>)>,
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

        // CD（Certification Declaration）の vendor_id/product_id はここの
        // `config.vendor_id`/`config.product_id` と一致させる（コミッショナ
        // が CD の vendor_id/product_id を Basic Information と突き合わせる
        // ため）。`device_type_id` は CD 側では matter.js の正典値に固定
        // されている（`mat_controller::cd::DEVICE_TYPE_ID_IN_CD` の doc
        // 参照 — CD の突き合わせ対象ではないため、この node がアプリ
        // endpoint 1 に載せている device type と揃える意味が無くなった）。
        //
        // Task 10: `ChipTest` mode bypasses this entirely in favor of the
        // vendored canonical chain (fixed VID FFF1/PID 8000 — matv.toml's
        // vendor_id/product_id must match for the CD's baked-in VID/PID to
        // line up with Basic Information; see chip_test_attestation's
        // module doc).
        let dev = match config.attestation {
            AttestationMode::Self_ => {
                x509::generate_dev_attestation(config.vendor_id, config.product_id)
                    .map_err(DeviceError::Attestation)?
            }
            AttestationMode::ChipTest => crate::chip_test_attestation::dev_attestation(),
        };
        let paa_dir = config.store_dir.join("paa");
        std::fs::create_dir_all(&paa_dir).map_err(DeviceError::Io)?;
        std::fs::write(paa_dir.join("paa.der"), &dev.paa_der).map_err(DeviceError::Io)?;

        let fabric_store = FabricStore::with_persist(Box::new(store_in_dir(&config.store_dir)));
        let mut comm_server = CommissioningServer::new(dev, fabric_store);

        // AccessControl（spec §11.1）の共有ストア: AddNOC の自動 admin
        // エントリ / RemoveFabric・fail-safe rollback の purge は
        // `CommissioningServer` が書き、EP0 の `AccessControlHandler` が
        // 読み書きする — `set_acl_store` は `into_cluster_handlers` より
        // 前に呼ぶ必要がある（後続のコミッショニングコマンドがこのストア
        // を触れるようにするため）。永続化は `<store_dir>/acl.json`
        // （`FabricStore`と同じ「ディレクトリを渡して file-backed persist
        // を注入する」配線）。
        let acl_store = crate::core::access_control::AclStore::with_persist(Box::new(
            crate::net::store::acl_store_in_dir(&config.store_dir),
        ));
        comm_server.set_acl_store(acl_store.clone());

        // GroupKeyManagement（spec §11.2）の共有ストア: `AclStore` と同じ
        // 「RemoveFabric・fail-safe rollback の purge は
        // `CommissioningServer` が書き、EP0 のクラスタハンドラが読み書き
        // する」配線（`set_acl_store`の doc 参照）。`<store_dir>/
        // group_keys.json` に永続化（`AclStore` と同じ file-backed persist
        // 注入）。
        let gk_store = crate::core::group_key_management::GroupKeyStore::with_persist(Box::new(
            crate::net::store::group_key_store_in_dir(&config.store_dir),
        ));
        comm_server.set_group_key_store(gk_store.clone());

        let unique_id = load_or_create_unique_id(&config.store_dir).map_err(DeviceError::Io)?;
        // NodeLabel/Location (spec §11.1.6.2/§11.1.6.6) の永続化 — 前回
        // 保存値（無ければ spec default の ("", "XX")）を初期値として渡し、
        // 以降の write は `basic_info_in_dir` へ save される
        // （`FabricStore`/`AclStore` と同じ「ディレクトリを渡して
        // file-backed persist を注入する」配線）。
        let (node_label, location) = load_basic_info(&config.store_dir);
        let mut node = Node::with_root_endpoint_persisted(
            config.vendor_id,
            config.product_id,
            &unique_id,
            node_label,
            location,
            Box::new(basic_info_in_dir(&config.store_dir)),
        );
        // DataVersion のブート時乱数初期化 (spec §7.10.3) — `core` は乱数源
        // を持ち込まないので、`getrandom` はここ（呼び出し側）で引いて
        // `Node` に渡す。node 単位の共通 base で十分（`set_data_version_
        // base`のdoc参照）: 目的は前ブートのキャッシュ済み DataVersion との
        // 偶然一致の排除であり、クラスタごとに独立させる必要はない。
        let mut version_seed = [0u8; 4];
        getrandom::getrandom(&mut version_seed)
            .map_err(|e| DeviceError::Io(std::io::Error::other(format!("os rng: {e}"))))?;
        node.set_data_version_base(u32::from_le_bytes(version_seed));
        // ACL enforcement (spec §9.10) を有効化する唯一の呼び出し —
        // `Node::set_acl_store` を呼ばない `Node`（テストが組む素の Node）は
        // 全許可のまま（`Node::acl`の doc 参照）。クラスタ登録より前に
        // 置く必要は無いが、「この Node は enforcement する」という宣言を
        // 組み立ての先頭にまとめておく。
        node.set_acl_store(acl_store.clone());
        let (general_commissioning, operational_credentials, admin_commissioning) =
            comm_server.into_cluster_handlers();
        node.add_cluster(0, general_commissioning);
        node.add_cluster(0, operational_credentials);
        node.add_cluster(0, admin_commissioning);
        node.add_cluster(
            0,
            Box::new(crate::core::access_control::AccessControlHandler::new(
                acl_store,
            )),
        );

        // NetworkCommissioning / GeneralDiagnostics / GroupKeyManagement
        // (Task 4): RootNode デバイスタイプの必須クラスタ（Device Library
        // §9.2.2）— AccessControl と同じ「Apple Home の commissioning 直後
        // interview 対策」。`config.iface` を唯一の "network"/interface 名
        // としてそのまま渡す（mDNS が同じ interface で egress する実体と
        // 一致させる）。
        node.add_cluster(
            0,
            Box::new(
                crate::core::network_commissioning::NetworkCommissioningHandler::new(&config.iface),
            ),
        );
        node.add_cluster(
            0,
            Box::new(
                crate::core::general_diagnostics::GeneralDiagnosticsHandler::new(&config.iface),
            ),
        );
        node.add_cluster(
            0,
            Box::new(crate::core::group_key_management::GroupKeyManagementHandler::new(gk_store)),
        );

        // M3: endpoint 1 = Aggregator (spec §9.12)、その配下 EP2.. が
        // 設定ファイルの `[[device]]` 1 件ずつに対応する bridged endpoint。
        // M2 までの「EP1 に OnOff Light 直付け」は廃止 — matv は純粋な
        // bridge になった。
        //
        // 採番はまず全 device 分を宣言順に台帳から引き当ててから 1 回だけ
        // save する（device ごとに save すると途中で失敗したときに台帳と
        // 実際に生えた endpoint が食い違う）。台帳は既知 id に同じ endpoint
        // を返し続けるので、設定の増減を跨いで endpoint が安定する。
        let mut ledger = crate::net::endpoint_ledger::EndpointLedger::load(&config.store_dir)
            .map_err(DeviceError::Io)?;
        let bridged_eps: Vec<u16> = config
            .devices
            .iter()
            .map(|d| ledger.assign(&d.id))
            .collect();
        ledger.save().map_err(DeviceError::Io)?;

        // EP1 は bridged endpoint 群より先に登録する — EP0 の PartsList は
        // `Node` が登録順に registry から導出するので、この順序がそのまま
        // `[1, 2, 3, ...]` という昇順の composition tree になる。
        node.add_endpoint(
            1,
            vec![Box::new(
                DescriptorHandler::for_device(mat_controller::im::DEVICE_TYPE_AGGREGATOR)
                    .with_parts(bridged_eps.clone()),
            )],
        );

        let mut onoff_states = Vec::with_capacity(config.devices.len());
        for (device, endpoint) in config.devices.iter().zip(&bridged_eps) {
            let built = crate::core::bridge::build_bridged_endpoint(
                device.kind,
                &device.name,
                &bridged_unique_id(&unique_id, &device.id),
            );
            node.add_endpoint(*endpoint, built.clusters);
            onoff_states.push((device.id.clone(), built.onoff_state));
        }

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
            onoff_states,
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
    ///
    /// **Commissioning window policy** (Task 14, spec §5.4.2.3): PASE is
    /// only admitted while the window is open. The window's *starting*
    /// state is decided once, right here, from `self.comm_server.fabrics()`
    /// as it stands at the moment `run` is called — empty (never
    /// commissioned, or a wiped `store_dir`) opens it for 15 minutes;
    /// non-empty (this device already has a fabric from an earlier run)
    /// starts it closed, since a fresh commissioner has no legitimate
    /// reason to PASE into an already-commissioned device. From there the
    /// window only ever closes — on the 15-minute deadline or on
    /// `CommissioningComplete` succeeding — and never reopens within this
    /// process: **M2 has no Administrator Commissioning cluster**
    /// (`OpenCommissioningWindow`/`RevokeCommissioning` are out of scope),
    /// so the only way to admit PASE again is a full restart, which
    /// re-evaluates this same policy against whatever's on disk at that
    /// point. The window's live state (open-until-deadline vs. closed) is
    /// tracked entirely inside `net::runtime::run`'s loop
    /// (`CommissioningWindow`) — this doc only describes the policy, not
    /// where the state lives.
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

#[cfg(test)]
mod tests {
    use super::*;

    use mat_controller::im;

    use crate::core::bridge::DeviceKind;
    use crate::core::datamodel::{InvokeCtx, ReadCtx};

    /// Reads one attribute off a `Node` through the real IM dispatch and
    /// returns its decoded JSON value — the `handle_im` +
    /// `decode_report_data_message` idiom `core::datamodel`'s own tests use,
    /// applied to the `Node` a live `Device` assembled.
    fn read_attr(
        node: &mut Node,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> serde_json::Value {
        let req = im::encode_read_request(endpoint, cluster, attribute);
        let out = node
            .handle_im(
                im::OPCODE_READ_REQUEST,
                &req,
                &mut InvokeCtx::default(),
                &ReadCtx::default(),
            )
            .expect("read request should be answered");
        let msg = im::decode_report_data_message(&out.payload).expect("decode report data");
        msg.reports
            .first()
            .unwrap_or_else(|| panic!("no report for {endpoint}/{cluster:#x}/{attribute:#x}"))
            .data
            .clone()
            .unwrap_or_else(|| panic!("no data for {endpoint}/{cluster:#x}/{attribute:#x}"))
    }

    /// M3 の bridge トポロジと採番台帳の安定性を、実際に `Device::new` が
    /// 組んだ `Node` に対する IM read で確認する:
    ///
    /// - EP0 の PartsList が EP1(Aggregator) + bridged EP 群を宣言順に並べる
    /// - EP1 が Aggregator デバイスタイプ + bridged EP 群の静的 PartsList
    /// - bridged EP が On/Off Light + Bridged Node の 2 デバイスタイプを持ち、
    ///   BDBI NodeLabel が設定ファイルの name
    /// - 設定から外した id の endpoint は他の id に再利用されず（単調増加）、
    ///   再追加すれば旧 endpoint が復元される
    #[tokio::test]
    async fn bridge_topology_and_ledger_stability() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = |devices: Vec<VirtualDeviceConfig>| DeviceConfig {
            passcode: 20202021,
            discriminator: 0xF00,
            vendor_id: 0xFFF1,
            product_id: 0x8000,
            // Port 0 so several `Device`s in this one test never collide.
            port: 0,
            store_dir: dir.path().to_path_buf(),
            iface: "lo".into(),
            attestation: AttestationMode::default(),
            devices,
        };
        let dev = |id: &str, name: &str| VirtualDeviceConfig {
            id: id.into(),
            kind: DeviceKind::OnOffLight,
            name: name.into(),
        };

        let mut d1 = Device::new(cfg(vec![
            dev("a", "A Light"),
            dev("b", "B Light"),
            dev("c", "C Light"),
        ]))
        .unwrap();

        // EP0 PartsList: Aggregator + the three bridged endpoints, in
        // declaration order (EP1 is registered before the bridged ones so the
        // registry-derived list comes out ascending).
        assert_eq!(
            read_attr(&mut d1.node, 0, im::CLUSTER_DESCRIPTOR, im::ATTR_PARTS_LIST),
            serde_json::json!([1, 2, 3, 4])
        );
        // EP1 is the Aggregator, and its static PartsList names only the
        // bridged children.
        assert_eq!(
            read_attr(
                &mut d1.node,
                1,
                im::CLUSTER_DESCRIPTOR,
                im::ATTR_DEVICE_TYPE_LIST
            ),
            serde_json::json!([{"0": im::DEVICE_TYPE_AGGREGATOR, "1": 1}])
        );
        assert_eq!(
            read_attr(&mut d1.node, 1, im::CLUSTER_DESCRIPTOR, im::ATTR_PARTS_LIST),
            serde_json::json!([2, 3, 4])
        );
        // EP2 is device "a": On/Off Light + Bridged Node, NodeLabel = name.
        assert_eq!(
            read_attr(
                &mut d1.node,
                2,
                im::CLUSTER_DESCRIPTOR,
                im::ATTR_DEVICE_TYPE_LIST
            ),
            serde_json::json!([
                {"0": im::DEVICE_TYPE_ON_OFF_LIGHT, "1": 1},
                {"0": im::DEVICE_TYPE_BRIDGED_NODE, "1": 1},
            ])
        );
        assert_eq!(
            read_attr(
                &mut d1.node,
                2,
                im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                im::ATTR_BI_NODE_LABEL
            ),
            serde_json::json!("A Light")
        );
        assert_eq!(
            read_attr(
                &mut d1.node,
                4,
                im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                im::ATTR_BI_NODE_LABEL
            ),
            serde_json::json!("C Light")
        );
        drop(d1);

        // Restart against the same store with "b" removed and "d" added:
        // a/c keep their endpoints, d gets a brand-new one (5) rather than
        // b's freed 3.
        let mut d2 = Device::new(cfg(vec![
            dev("a", "A Light"),
            dev("c", "C Light"),
            dev("d", "D Light"),
        ]))
        .unwrap();
        assert_eq!(
            read_attr(&mut d2.node, 1, im::CLUSTER_DESCRIPTOR, im::ATTR_PARTS_LIST),
            serde_json::json!([2, 4, 5])
        );
        assert_eq!(
            read_attr(
                &mut d2.node,
                5,
                im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                im::ATTR_BI_NODE_LABEL
            ),
            serde_json::json!("D Light")
        );
        drop(d2);

        // Re-adding "b" restores its original endpoint 3 (tombstone), so the
        // controller's existing pairing for that accessory keeps working.
        let mut d3 = Device::new(cfg(vec![
            dev("a", "A Light"),
            dev("b", "B Light"),
            dev("c", "C Light"),
            dev("d", "D Light"),
        ]))
        .unwrap();
        assert_eq!(
            read_attr(&mut d3.node, 1, im::CLUSTER_DESCRIPTOR, im::ATTR_PARTS_LIST),
            serde_json::json!([2, 3, 4, 5])
        );
        assert_eq!(
            read_attr(
                &mut d3.node,
                3,
                im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                im::ATTR_BI_NODE_LABEL
            ),
            serde_json::json!("B Light")
        );
        drop(d3);
    }

    /// BDBI の UniqueID (spec §9.13 / §11.1.6.15 の `string32`) を、実際に
    /// `Device` が組んだ node から IM read で取り出して確認する:
    ///
    /// - ちょうど 32 文字の小文字 hex（`{node_unique_id}-{device_id}` の
    ///   素の連結だと必ず 32 文字を超える — [`bridged_unique_id`] の doc）
    /// - 同じ store で組み直せば同じ値（コントローラ側のアクセサリ対応が
    ///   再起動を跨いで生きる）
    /// - device が違えば違う値
    #[tokio::test]
    async fn bridged_unique_ids_are_string32_stable_and_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = || DeviceConfig {
            passcode: 20202021,
            discriminator: 0xF00,
            vendor_id: 0xFFF1,
            product_id: 0x8000,
            port: 0,
            store_dir: dir.path().to_path_buf(),
            iface: "lo".into(),
            attestation: AttestationMode::default(),
            devices: vec![
                VirtualDeviceConfig {
                    id: "e2e-light".into(),
                    kind: DeviceKind::OnOffLight,
                    name: "E2E Light".into(),
                },
                VirtualDeviceConfig {
                    id: "other-light".into(),
                    kind: DeviceKind::OnOffLight,
                    name: "Other Light".into(),
                },
            ],
        };
        let unique_id_at = |node: &mut Node, endpoint: u16| -> String {
            read_attr(
                node,
                endpoint,
                im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                im::ATTR_BI_UNIQUE_ID,
            )
            .as_str()
            .expect("UniqueID reads as a string")
            .to_string()
        };

        let mut d1 = Device::new(cfg()).unwrap();
        let first = unique_id_at(&mut d1.node, 2);
        let second = unique_id_at(&mut d1.node, 3);
        drop(d1);

        for id in [&first, &second] {
            assert_eq!(
                id.chars().count(),
                32,
                "BDBI UniqueID is a spec string32, got {id:?}"
            );
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "BDBI UniqueID should be lowercase hex, got {id:?}"
            );
        }
        assert_ne!(
            first, second,
            "two devices under the same bridge must not share a UniqueID"
        );

        // 同じ store で組み直しても同じ値（node の unique_id も device id も
        // 変わらないので決定的）。
        let mut d2 = Device::new(cfg()).unwrap();
        assert_eq!(unique_id_at(&mut d2.node, 2), first);
        assert_eq!(unique_id_at(&mut d2.node, 3), second);
        drop(d2);
    }

    /// First call with an empty `store_dir` generates a fresh UniqueID and
    /// persists it — 32 lowercase hex chars (16 random bytes).
    #[test]
    fn load_or_create_unique_id_generates_and_persists_on_first_boot() {
        let dir = tempfile::tempdir().unwrap();
        let id = load_or_create_unique_id(dir.path()).unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("unique_id")).unwrap(),
            id
        );
    }

    /// Restart stability: a second call against the same `store_dir` reads
    /// back the exact value the first call generated, rather than
    /// generating a new one — Apple Home's UniqueID-keyed pairing state
    /// would otherwise desync from a fresh random value on every restart.
    #[test]
    fn load_or_create_unique_id_is_stable_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_unique_id(dir.path()).unwrap();
        let second = load_or_create_unique_id(dir.path()).unwrap();
        assert_eq!(first, second);
    }
}
