//! establish の mDNS 解決を差し替え可能にする抽象。`mat`（一発）は
//! [`OneShotResolver`]（キャッシュ無し＝設計ルール4）、`matd` は
//! [`CachingResolver`]（常駐キャッシュ）を注入する。

use std::time::Duration;

use async_trait::async_trait;
use mat_controller::dnssd;

/// establish の mDNS 解決を差し替え可能にする抽象。`mat`（一発）は
/// [`OneShotResolver`]（キャッシュ無し＝設計ルール4）、`matd` は
/// `CachingResolver`（常駐キャッシュ、Task 5）を注入する。
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(
        &self,
        scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError>;
}

/// 既定のリゾルバ: 一発 legacy multicast resolve を毎回実行する（キャッシュ
/// を持たない）。`mat` 一発直経路が使う。
pub struct OneShotResolver;

#[async_trait]
impl Resolver for OneShotResolver {
    async fn resolve(
        &self,
        scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
        dnssd::resolve_operational(scope_id, &cfid, node_id, timeout).await
    }
}

/// matd 用リゾルバ: 常駐 mDNS キャッシュ（[`dnssd::OperationalCache`]）を参照し、
/// ヒットは即返し、ミス時は provoke してリスナの次アナウンスを
/// `CACHE_MISS_TIMEOUT` まで待つ。establish から渡される `timeout`（8s）ではなく
/// この内部定数を使う理由は spec 参照（`mat` 一発を無変更に保つため窓を分離）。
pub struct CachingResolver {
    cache: dnssd::OperationalCache,
}

/// cache miss 時にリスナの次アナウンス（周期~30s）を確実に跨ぐ待ち窓。op 予算設計の成分（Issue #16）。
pub const CACHE_MISS_TIMEOUT: Duration = Duration::from_secs(35);
/// キャッシュ充填の poll 間隔（Notify を使わず単純 poll で取りこぼしを防ぐ）。
const CACHE_POLL: Duration = Duration::from_millis(500);

impl CachingResolver {
    pub fn new(cache: dnssd::OperationalCache) -> Self {
        Self { cache }
    }
}

#[async_trait]
impl Resolver for CachingResolver {
    async fn resolve(
        &self,
        _scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        _timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
        let instance = format!(
            "{}._matter._tcp.local",
            dnssd::operational_instance(&cfid, node_id)
        );
        if let Some(n) = self.cache.get(&instance) {
            return Ok(n);
        }
        // ミス: listener に provoke クエリを依頼し、次アナウンス/応答を待つ。
        self.cache.request(instance.clone());
        let deadline = tokio::time::Instant::now() + CACHE_MISS_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(CACHE_POLL).await;
            if let Some(n) = self.cache.get(&instance) {
                return Ok(n);
            }
        }
        Err(dnssd::DnssdError::Timeout { instance })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_miss_timeout_is_pinned() {
        // Issue #16: op 予算設計（最悪 45〜60s の導出成分）の釘打ち。
        assert_eq!(CACHE_MISS_TIMEOUT.as_secs(), 35);
    }

    /// resolve が実際に multicast 送受信できる iface の index を1つ探す。
    /// `crate::iface_select`（M8c-3 iface 自動検出）と同じ適格条件 — up・
    /// MULTICAST・非 loopback・非 POINTOPOINT・IPv6 link-local 保有 — を使う
    /// が、こちらは複数候補でも先頭を採用する（本番の autodetect は曖昧なら
    /// ハードエラーだが、このテストは delegation の検証に使える iface が
    /// 1つあれば十分）。単純に `flags`/`lo` だけで判定すると、この sandbox
    /// のような環境で `docker0` / `loopback0`（`lo` とは別名の仮想 NIC）/
    /// `tailscale0` を拾って `bind_mdns_socket` の send が `ENETUNREACH` で
    /// 即死し、意図した Timeout 経路を検証できなくなる。
    fn multicast_capable_iface_index() -> Option<u32> {
        const IFF_UP: u32 = 0x1;
        const IFF_LOOPBACK: u32 = 0x8;
        const IFF_POINTOPOINT: u32 = 0x10;
        const IFF_MULTICAST: u32 = 0x1000;
        let mut ll_names = std::collections::HashSet::new();
        for line in std::fs::read_to_string("/proc/net/if_inet6").ok()?.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 6 && cols[3] == "20" {
                ll_names.insert(cols[5].to_string());
            }
        }
        let mut entries: Vec<_> = std::fs::read_dir("/sys/class/net")
            .ok()?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !ll_names.contains(&name) {
                continue;
            }
            let base = entry.path();
            let flags = std::fs::read_to_string(base.join("flags"))
                .ok()
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            let operstate_up = std::fs::read_to_string(base.join("operstate"))
                .map(|s| s.trim() == "up")
                .unwrap_or(false);
            let eligible = operstate_up
                && flags & IFF_UP != 0
                && flags & IFF_MULTICAST != 0
                && flags & IFF_LOOPBACK == 0
                && flags & IFF_POINTOPOINT == 0;
            if !eligible {
                continue;
            }
            if let Ok(idx) = std::fs::read_to_string(base.join("ifindex"))
                .unwrap_or_default()
                .trim()
                .parse::<u32>()
            {
                return Some(idx);
            }
        }
        None
    }

    #[tokio::test]
    async fn oneshot_resolver_times_out_without_responder() {
        // 応答者のいない iface で resolve すると Timeout（委譲先
        // resolve_operational の契約）。無応答→Timeout は不変。
        let Some(scope) = multicast_capable_iface_index() else {
            eprintln!(
                "skipping oneshot_resolver test: no eligible multicast-capable IPv6 interface"
            );
            return;
        };
        let r = OneShotResolver;
        let out = r
            .resolve(scope, [0u8; 8], 5, std::time::Duration::from_millis(300))
            .await;
        assert!(matches!(
            out,
            Err(mat_controller::dnssd::DnssdError::Timeout { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn caching_resolver_returns_cached_hit_immediately() {
        use mat_controller::dnssd;
        let (cache, _rx) = dnssd::OperationalCache::new();
        let inst = dnssd::operational_instance(&[0xAB; 8], 5) + "._matter._tcp.local";
        cache.insert(
            inst,
            dnssd::ResolvedNode {
                port: 5540,
                addresses: vec!["fd00::1".parse().unwrap()],
                session_idle_interval_ms: None,
                session_active_interval_ms: None,
            },
            std::time::Duration::from_secs(60),
        );
        let r = CachingResolver::new(cache);
        let n = r
            .resolve(1, [0xAB; 8], 5, std::time::Duration::from_secs(8))
            .await
            .expect("hit");
        assert_eq!(n.port, 5540);
    }

    #[tokio::test(start_paused = true)]
    async fn caching_resolver_awaits_listener_fill_then_returns() {
        use mat_controller::dnssd;
        let (cache, mut rx) = dnssd::OperationalCache::new();
        let inst = dnssd::operational_instance(&[0xAB; 8], 7) + "._matter._tcp.local";
        let filler = cache.clone();
        let inst2 = inst.clone();
        // 別タスクが少し後に埋める（リスナ相当）。
        tokio::spawn(async move {
            // provoke request が届くはず。
            let got = rx.recv().await.unwrap();
            assert_eq!(got, inst2);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            filler.insert(
                inst2,
                dnssd::ResolvedNode {
                    port: 5541,
                    addresses: vec!["fd00::2".parse().unwrap()],
                    session_idle_interval_ms: None,
                    session_active_interval_ms: None,
                },
                std::time::Duration::from_secs(60),
            );
        });
        let r = CachingResolver::new(cache);
        let n = r
            .resolve(1, [0xAB; 8], 7, std::time::Duration::from_secs(8))
            .await
            .expect("fill");
        assert_eq!(n.port, 5541);
    }

    #[tokio::test(start_paused = true)]
    async fn caching_resolver_times_out_when_never_filled() {
        use mat_controller::dnssd;
        let (cache, _rx) = dnssd::OperationalCache::new();
        let r = CachingResolver::new(cache);
        let out = r
            .resolve(1, [0xAB; 8], 9, std::time::Duration::from_secs(8))
            .await;
        assert!(matches!(out, Err(dnssd::DnssdError::Timeout { .. })));
    }
}
