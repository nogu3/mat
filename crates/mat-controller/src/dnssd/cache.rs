//! matd 常駐用の operational mDNS キャッシュ: `_matter._tcp` の SRV/TXT/AAAA
//! を受信し続けて `ResolvedNode` を鮮度順に保持する（`OperationalCache`）。
//! one-shot 解決（親モジュール）とは別経路 — `mat` 単発実行は使わない
//! （設計ルール 4: 状態を持たない）。

use std::collections::{HashMap, HashSet};
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::codec::{encode_query, parse_message, txt_u32, RData, Record};
use super::{
    bind_mdns_socket, is_link_local, ResolvedNode, MDNS_GROUP, MDNS_PORT, TYPE_SRV, TYPE_TXT,
};

/// キャッシュ上限（偽装 flood でメモリを伸ばさない — MAX_INSTANCES と同思想）。
const MAX_CACHE: usize = 256;

#[derive(Debug)]
struct CacheEntry {
    node: ResolvedNode,
    expiry: Instant,
}

#[derive(Debug)]
struct CacheInner {
    map: StdMutex<HashMap<String, CacheEntry>>,
    query_tx: mpsc::UnboundedSender<String>,
}

/// matd 常駐 mDNS キャッシュのハンドル。listener タスク（[`run_operational_cache`]）
/// と `CachingResolver` が `Arc` で共有する。設計ルール4: `mat` 一発は使わない
/// （matd 専用）。`Clone` は内部 `Arc` の複製。
#[derive(Clone, Debug)]
pub struct OperationalCache {
    inner: std::sync::Arc<CacheInner>,
}

impl OperationalCache {
    /// ハンドルと、listener が読む provoke-request 受信端を返す。
    pub fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                inner: std::sync::Arc::new(CacheInner {
                    map: StdMutex::new(HashMap::new()),
                    query_tx: tx,
                }),
            },
            rx,
        )
    }

    /// 鮮度のあるエントリのみ返す（期限切れ/不在は None）。キーは ASCII 小文字
    /// 正規化して照合する（`resolve_operational` の `eq_ignore_ascii_case` と
    /// 同規律 — 最終レビュー #2）。listener が insert する生の `r.name` と
    /// `CachingResolver` が渡す `operational_instance()`（大文字16進）の大小差
    /// で恒久ミスにならないようにする。
    pub fn get(&self, instance: &str) -> Option<ResolvedNode> {
        let key = instance.to_ascii_lowercase();
        let map = crate::sync::locked(&self.inner.map);
        map.get(&key)
            .filter(|e| Instant::now() < e.expiry)
            .map(|e| e.node.clone())
    }

    /// listener に instance の provoke クエリ送信を依頼する（listener 不在でも無害）。
    /// ここは正規化しない: provoke クエリはワイヤに出す質問名で、mDNS は
    /// ワイヤ上では大小文字非依存（呼び出し元の形のまま送って構わない）。
    pub fn request(&self, instance: String) {
        let _ = self.inner.query_tx.send(instance);
    }

    /// listener が呼ぶ: エントリを入れて期限を更新する。上限超過時、新規キーは
    /// 挿入しない（既存キーの更新は常に許可＝鮮度維持を止めない）。キーは
    /// `get` と同じく ASCII 小文字正規化して格納する（最終レビュー #2）。
    pub fn insert(&self, instance: String, node: ResolvedNode, ttl: Duration) {
        let key = instance.to_ascii_lowercase();
        let mut map = crate::sync::locked(&self.inner.map);
        if !map.contains_key(&key) && map.len() >= MAX_CACHE {
            return;
        }
        map.insert(
            key,
            CacheEntry {
                node,
                expiry: Instant::now() + ttl,
            },
        );
    }
}

/// operational instance を _matter._tcp のサービス名で判定する接尾辞。
const OPERATIONAL_SUFFIX: &str = "._matter._tcp.local";

/// 各マップの「異なるキー」件数の上限（偽装 flood でメモリを伸ばさない —
/// MAX_INSTANCES/MAX_CACHE と同思想）。既存キーの更新は常に許可する（鮮度
/// 維持を止めない）ので、新規キーだけがこの上限で弾かれる。
const MAX_FOLD_ENTRIES: usize = 256;
/// ホスト 1 件あたりの AAAA 保持上限（dedup 済み）。旧実装のグローバル 64
/// プール（`MAX_BROWSE_AAAA`）は満杯になると新規ホストを一切学習できず恒久
/// Unreachable を招いた（最終レビュー #1a）。per-host にすることでホスト数
/// が MAX_FOLD_ENTRIES 未満である限り starve しない。
const MAX_ADDRS_PER_HOST: usize = 8;

/// RFC 6762 §10.2: cache-flush 受信時に破棄してよいのは「1 秒より古い」記録
/// だけ。この猶予が、同一アナウンス burst が複数データグラムに分割された
/// 場合の相互破壊を防ぐ。
const CACHE_FLUSH_GRACE: Duration = Duration::from_secs(1);

/// 常駐 mDNS listener（[`run_operational_cache`]）がプロセス寿命で保持する、
/// 有界・自己更新型の operational レコード蓄積。旧実装（[`InstAcc`] 相当を
/// 1個の HashMap + 1個のグローバル共有 `aaaa: Vec` に貯める設計）には2つの
/// 欠陥があった（最終レビュー #1）:
/// (a) 共有 AAAA プールが `MAX_BROWSE_AAAA`(64) で頭打ちになり、以後どんな
///     新規/再アドレス化ノードも AAAA を学習できず恒久 Unreachable になる。
/// (b) 完成 instance を「毎 datagram、蓄積された全部」を re-insert していた
///     ため、広告をやめた（goodbye 無し）ノードでも無関係な multicast が
///     expiry を延命し続け、TTL/goodbye 失効が機能しなかった。
///
/// この構造体はキーを全て ASCII 小文字化して保持し（大小文字非依存）、
/// [`fold_operational_into_cache`] が「当該 datagram に現れた instance のみ」
/// を touched として cache insert することで (b) を解消し、AAAA を
/// host→addrs の per-host プールにすることで (a) を解消する。
#[derive(Default)]
struct OperationalFold {
    /// instance_lower → (port, target_lower, ttl)。
    srv: HashMap<String, (u16, String, u32)>,
    /// instance_lower → TXT 文字列群。
    txt: HashMap<String, Vec<Vec<u8>>>,
    /// host_lower(SRV target) → (アドレス, last_seen)。dedup、
    /// [`MAX_ADDRS_PER_HOST`] で頭打ち。last_seen は鮮度順ソートと
    /// cache-flush / 満杯 evict の判定材料（監査⑤）。
    addrs: HashMap<String, Vec<(Ipv6Addr, Instant)>>,
}

/// `records`（1 データグラム分）を `fold` へ畳み込み、その datagram に現れた
/// instance のうち SRV + 一致 AAAA が揃っているものだけを `cache` へ insert
/// する（TTL は SRV レコード値を尊重）。commissionable (`_matterc._udp`) など
/// operational でない名前は無視する。
///
/// SRV が先の datagram、AAAA が後の datagram で届く分割到着でも、AAAA 到着時
/// に `fold.srv` の target と一致する既知 instance を touched にする（この
/// target→touched 走査がクロス datagram 完成を保つ）。だが「当該 datagram に
/// 一切登場しない instance」は re-insert しない — 立ち去ったノードの expiry
/// が無関係な multicast で延命されるのを防ぐ。
fn fold_operational_into_cache(
    records: &[Record],
    fold: &mut OperationalFold,
    cache: &OperationalCache,
    now: Instant,
) {
    let mut touched: HashSet<String> = HashSet::new();

    for r in records {
        match &r.rdata {
            RData::Srv { port, target } if r.name.ends_with(OPERATIONAL_SUFFIX) => {
                let inst = r.name.to_ascii_lowercase();
                let is_new = !fold.srv.contains_key(&inst);
                if is_new && fold.srv.len() >= MAX_FOLD_ENTRIES {
                    continue;
                }
                fold.srv
                    .insert(inst.clone(), (*port, target.to_ascii_lowercase(), r.ttl));
                touched.insert(inst);
            }
            RData::Txt(strings) if r.name.ends_with(OPERATIONAL_SUFFIX) => {
                let inst = r.name.to_ascii_lowercase();
                let is_new = !fold.txt.contains_key(&inst);
                if is_new && fold.txt.len() >= MAX_FOLD_ENTRIES {
                    continue;
                }
                fold.txt.insert(inst.clone(), strings.clone());
                touched.insert(inst);
            }
            RData::Aaaa(addr) => {
                let host = r.name.to_ascii_lowercase();
                let is_new_host = !fold.addrs.contains_key(&host);
                if !(is_new_host && fold.addrs.len() >= MAX_FOLD_ENTRIES) {
                    let list = fold.addrs.entry(host.clone()).or_default();
                    if r.cache_flush {
                        // cache-flush: 1 秒より古い既存アドレスは「現在の広告に
                        // 含まれない」ものとして物理削除する（stale を先に試す
                        // 79s 浪費と恒久 unreachable の元 — 監査⑤）。
                        list.retain(|(a, seen)| {
                            let keep = now.duration_since(*seen) <= CACHE_FLUSH_GRACE;
                            if !keep {
                                tracing::debug!(host = %host, addr = %a, "aaaa evicted (cache-flush)");
                            }
                            keep
                        });
                    }
                    if let Some(entry) = list.iter_mut().find(|(a, _)| a == addr) {
                        entry.1 = now;
                    } else {
                        if list.len() >= MAX_ADDRS_PER_HOST {
                            // 満杯: 最古 last_seen を追い出して新アドレスを学習する。
                            // 旧来の「新規拒否」は全 stale 時に恒久 unreachable を
                            // 招いた（監査⑤の欠陥 2）。
                            if let Some(i) = list
                                .iter()
                                .enumerate()
                                .min_by_key(|(_, (_, seen))| *seen)
                                .map(|(i, _)| i)
                            {
                                let (evicted, _) = list.remove(i);
                                tracing::debug!(host = %host, addr = %evicted, "aaaa evicted (pool full)");
                            }
                        }
                        list.push((*addr, now));
                    }
                }
                // この host を SRV target に持つ既知 instance を touched に。
                // これがクロス datagram 完成（SRV が先、AAAA が後）を支える。
                for (inst, (_, target, _)) in &fold.srv {
                    if target.eq_ignore_ascii_case(&host) {
                        touched.insert(inst.clone());
                    }
                }
            }
            _ => {}
        }
    }

    for inst in touched {
        let Some((port, target, ttl)) = fold.srv.get(&inst) else {
            continue;
        };
        let Some(addrs) = fold.addrs.get(target) else {
            continue;
        };
        if addrs.is_empty() {
            continue;
        }
        // 非 LL 優先はそのまま、同群内は last_seen の新しい順（監査⑤）。現役
        // アドレスは約 30s 周期の再広告で常に最新なので、確立ループ（最初の
        // 成功で早期リターン）は常に現アドレスから試す。
        let mut entries = addrs.clone();
        entries.sort_by_key(|(a, seen)| (is_link_local(a), std::cmp::Reverse(*seen)));
        let addresses: Vec<Ipv6Addr> = entries.into_iter().map(|(a, _)| a).collect();
        let txt = fold.txt.get(&inst).map(Vec::as_slice).unwrap_or(&[]);
        let node = ResolvedNode {
            port: *port,
            addresses,
            session_idle_interval_ms: txt_u32(txt, "SII"),
            session_active_interval_ms: txt_u32(txt, "SAI"),
        };
        // TTL 0（goodbye）は即時失効相当なので短く。通常は広告 TTL を尊重。
        cache.insert(inst.clone(), node, Duration::from_secs(u64::from(*ttl)));
    }
}

/// operational レコードを常駐で受信・畳み込みキャッシュを温める。matd が起動時に
/// spawn する。provoke リクエスト受信時はその instance の SRV+TXT クエリを送出。
/// I/O エラー・パース失敗では落とさず継続する（listener はプロセス寿命）。
async fn run_operational_cache(
    sock: UdpSocket,
    cache: OperationalCache,
    mut requests: mpsc::UnboundedReceiver<String>,
    scope_id: u32,
) {
    let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));
    let mut fold = OperationalFold::default();
    // browse と同様、複数 instance の additional 同梱に備え広めに取る。
    let mut buf = vec![0u8; 9000];
    loop {
        tokio::select! {
            recv = sock.recv_from(&mut buf) => {
                let Ok((n, _)) = recv else { continue; };
                let Ok(records) = parse_message(&buf[..n]) else { continue; };
                fold_operational_into_cache(&records, &mut fold, &cache, Instant::now());
            }
            req = requests.recv() => {
                match req {
                    // instance は "<CFID>-<NodeId>._matter._tcp.local"。
                    Some(instance) => {
                        let q = encode_query(0, &[(instance.as_str(), TYPE_SRV), (instance.as_str(), TYPE_TXT)]);
                        let _ = sock.send_to(&q, dest).await;
                    }
                    // 全 sender が drop（= 実質プロセス終了時のみ）。
                    None => return,
                }
            }
        }
    }
}

/// matd 用: mDNS socket を bind し常駐 cache タスクを spawn する。bind 失敗は
/// `Err`（matd は OneShotResolver に degrade する）。tokio ランタイム内で呼ぶこと。
pub fn spawn_operational_cache(scope_id: u32) -> std::io::Result<OperationalCache> {
    let sock = bind_mdns_socket(scope_id)?;
    let (cache, requests) = OperationalCache::new();
    tokio::spawn(run_operational_cache(
        sock,
        cache.clone(),
        requests,
        scope_id,
    ));
    Ok(cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dnssd::codec::push_name;
    use crate::dnssd::test_util::{
        synth_aaaa_class, synth_commissionable_response, synth_response,
    };
    use crate::dnssd::{iface_index, CLASS_IN};
    use std::net::Ipv6Addr;
    use std::time::Duration;

    #[test]
    fn fold_operational_populates_cache_from_one_message() {
        let (cache, _rx) = OperationalCache::new();
        let addr: Ipv6Addr = "fd00:1111:2222:1::b92a".parse().unwrap();
        // operational instance の完成応答（SRV+TXT+AAAA を 1 メッセージに）。
        let msg = synth_response(
            "AB7DE08802E0CD54-0000000000000005._matter._tcp.local",
            "12B41A22758B788A.local",
            5540,
            &["SII=5000", "SAI=300", "T=0"],
            addr,
        );
        let records = parse_message(&msg).unwrap();
        let mut fold = OperationalFold::default();
        fold_operational_into_cache(&records, &mut fold, &cache, Instant::now());

        let node = cache
            .get("AB7DE08802E0CD54-0000000000000005._matter._tcp.local")
            .expect("operational instance should be cached");
        assert_eq!(node.port, 5540);
        assert_eq!(node.addresses, vec![addr]);
        assert_eq!(node.session_idle_interval_ms, Some(5000));
    }

    #[test]
    fn fold_operational_ignores_non_matter_and_incomplete() {
        let (cache, _rx) = OperationalCache::new();
        // commissionable(_matterc._udp) は operational ではないので無視。
        let msg = synth_commissionable_response(
            "_L3840._sub._matterc._udp.local",
            "ABCD1234._matterc._udp.local",
            "dev.local",
            5540,
            &["D=3840"],
            "fd00::1".parse().unwrap(),
        );
        let records = parse_message(&msg).unwrap();
        let mut fold = OperationalFold::default();
        fold_operational_into_cache(&records, &mut fold, &cache, Instant::now());
        assert!(cache.get("ABCD1234._matterc._udp.local").is_none());
    }

    #[tokio::test]
    async fn spawn_operational_cache_binds_on_loopback() {
        // lo の ifindex。取得できない CI もあるため取得可否で分岐。
        let Ok(scope) = iface_index("lo") else {
            return; // lo が無い環境ではスキップ（bind の型検証は他テストで担保）。
        };
        // 二重 bind（REUSEADDR）で失敗しないこと＝常駐 socket が確立できる。
        let a = spawn_operational_cache(scope);
        assert!(a.is_ok(), "operational cache should bind on lo: {a:?}");
    }

    fn sample_node(port: u16) -> ResolvedNode {
        ResolvedNode {
            port,
            addresses: vec!["fd00::1".parse().unwrap()],
            session_idle_interval_ms: Some(5000),
            session_active_interval_ms: Some(300),
        }
    }

    #[test]
    fn opcache_insert_get_and_expiry() {
        let (cache, _rx) = OperationalCache::new();
        let inst = "AABB-0005._matter._tcp.local".to_string();
        cache.insert(inst.clone(), sample_node(5540), Duration::from_secs(60));
        assert_eq!(cache.get(&inst).map(|n| n.port), Some(5540));
        assert!(cache.get("nope._matter._tcp.local").is_none());
        // 期限切れは None。
        cache.insert(inst.clone(), sample_node(5540), Duration::from_millis(0));
        assert!(cache.get(&inst).is_none());
    }

    #[test]
    fn opcache_caps_new_instances_but_updates_existing() {
        let (cache, _rx) = OperationalCache::new();
        for i in 0..MAX_CACHE {
            cache.insert(
                format!("i{i}._matter._tcp.local"),
                sample_node(1),
                Duration::from_secs(60),
            );
        }
        // 上限到達後の新規は無視。
        cache.insert(
            "overflow._matter._tcp.local".into(),
            sample_node(1),
            Duration::from_secs(60),
        );
        assert!(cache.get("overflow._matter._tcp.local").is_none());
        // 既存キーの更新は上限後も許可。
        cache.insert(
            "i0._matter._tcp.local".into(),
            sample_node(9999),
            Duration::from_secs(60),
        );
        assert_eq!(
            cache.get("i0._matter._tcp.local").map(|n| n.port),
            Some(9999)
        );
    }

    #[test]
    fn opcache_request_does_not_panic_and_is_received() {
        let (cache, mut rx) = OperationalCache::new();
        cache.request("x._matter._tcp.local".into());
        assert_eq!(rx.try_recv().unwrap(), "x._matter._tcp.local");
    }

    /// `resolve_operational` の `eq_ignore_ascii_case` 規律とキャッシュを揃える
    /// (最終レビュー #2)。listener はワイヤ上の生の `r.name`（大小混在し得る）
    /// を insert し、`CachingResolver` は `operational_instance()`（大文字16進）
    /// で get する — 正規化しないと非大文字hex広告者で恒久ミスになる。
    #[test]
    fn opcache_get_is_case_insensitive() {
        let (cache, _rx) = OperationalCache::new();
        let lower = "ab7de08802e0cd54-0000000000000005._matter._tcp.local".to_string();
        cache.insert(lower, sample_node(5540), Duration::from_secs(60));
        let upper = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        assert_eq!(cache.get(upper).map(|n| n.port), Some(5540));
    }

    /// SRV+TXT が 1 データグラム、AAAA が別データグラムで届いても、後段の
    /// AAAA 到着時に target 一致で touched になり完成する (最終レビュー #1,
    /// step 4 の target→touched 走査)。
    fn synth_srv_txt_only(service: &str, target: &str, port: u16, txt: &[&str]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&[0, 0, 0x84, 0x00]); // id 0, QR|AA
        m.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 0]); // qd 0, an 2 (SRV+TXT)
        push_name(&mut m, service);
        m.extend_from_slice(&TYPE_SRV.to_be_bytes());
        m.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 120]); // cache-flush|IN, ttl 120
        let mut rdata = vec![0, 0, 0, 0]; // priority, weight
        rdata.extend_from_slice(&port.to_be_bytes());
        let mut tname = Vec::new();
        push_name(&mut tname, target);
        rdata.extend_from_slice(&tname);
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&rdata);
        push_name(&mut m, service);
        m.extend_from_slice(&TYPE_TXT.to_be_bytes());
        m.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 120]);
        let mut rdata = Vec::new();
        for s in txt {
            rdata.push(s.len() as u8);
            rdata.extend_from_slice(s.as_bytes());
        }
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&rdata);
        m
    }

    fn synth_aaaa_only(name: &str, ttl: u32, addr: Ipv6Addr) -> Vec<u8> {
        synth_aaaa_class(name, ttl, addr, 0x8000 | CLASS_IN) // cache-flush|IN
    }

    /// アドレスローテーション（監査⑤の欠陥 1）: cache-flush 付きの新 AAAA が
    /// 届いたら、1 秒より古い旧アドレスは物理削除され、キャッシュは新アドレス
    /// のみになる。
    #[tokio::test(start_paused = true)]
    async fn fold_cache_flush_evicts_stale_addresses() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "hosta.local";
        let old: Ipv6Addr = "fd00::1".parse().unwrap();
        let new: Ipv6Addr = "fd00::2".parse().unwrap();

        let msg = synth_response(service, target, 5540, &["SII=5000"], old);
        fold_operational_into_cache(
            &parse_message(&msg).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );
        assert_eq!(cache.get(service).unwrap().addresses, vec![old]);

        tokio::time::advance(Duration::from_secs(2)).await;

        // 再アドレス化: cache-flush 付き AAAA（synth_aaaa_only は cache-flush|IN）。
        let msg2 = synth_aaaa_only(target, 120, new);
        fold_operational_into_cache(
            &parse_message(&msg2).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );
        assert_eq!(
            cache.get(service).unwrap().addresses,
            vec![new],
            "stale address must be physically removed on cache-flush"
        );
    }

    /// RFC 6762 §10.2 の 1 秒猶予: 同一アナウンス burst の分割データグラム
    /// （≤1s 間隔）は cache-flush でも互いを消さない。
    #[tokio::test(start_paused = true)]
    async fn fold_cache_flush_grace_keeps_same_burst() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "hosta.local";
        let a1: Ipv6Addr = "fd00::1".parse().unwrap();
        let a2: Ipv6Addr = "fd00::2".parse().unwrap();

        let m1 = synth_aaaa_only(target, 120, a1);
        fold_operational_into_cache(
            &parse_message(&m1).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );
        tokio::time::advance(Duration::from_millis(500)).await;
        let m2 = synth_aaaa_only(target, 120, a2);
        fold_operational_into_cache(
            &parse_message(&m2).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );

        let msg = synth_srv_txt_only(service, target, 5540, &["SII=5000"]);
        fold_operational_into_cache(
            &parse_message(&msg).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );
        let node = cache.get(service).unwrap();
        assert_eq!(
            node.addresses.len(),
            2,
            "both burst datagrams must survive the 1s grace"
        );
        assert!(node.addresses.contains(&a1) && node.addresses.contains(&a2));
    }

    #[test]
    fn fold_cross_datagram_srv_then_aaaa_completes() {
        let (cache, _rx) = OperationalCache::new();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "12b41a22758b788a.local";
        let addr: Ipv6Addr = "fd00:1111:2222:1::b92a".parse().unwrap();
        let mut fold = OperationalFold::default();

        let msg1 = synth_srv_txt_only(service, target, 5540, &["SII=5000"]);
        let records1 = parse_message(&msg1).unwrap();
        fold_operational_into_cache(&records1, &mut fold, &cache, Instant::now());
        assert!(
            cache.get(service).is_none(),
            "SRV without AAAA must not cache yet"
        );

        let msg2 = synth_aaaa_only(target, 120, addr);
        let records2 = parse_message(&msg2).unwrap();
        fold_operational_into_cache(&records2, &mut fold, &cache, Instant::now());

        let node = cache
            .get(service)
            .expect("should complete once the AAAA for the SRV target arrives");
        assert_eq!(node.port, 5540);
        assert_eq!(node.addresses, vec![addr]);
        assert_eq!(node.session_idle_interval_ms, Some(5000));
    }

    /// 旧実装は AAAA プールが `MAX_BROWSE_AAAA`(64) グローバル共有だったため、
    /// 65 個目以降の新規ホストは学習不能で恒久 Unreachable になった
    /// (最終レビュー #1a)。per-host 方式ならホスト数が MAX_FOLD_ENTRIES(256)
    /// 未満である限り starve しない。
    #[test]
    fn fold_no_global_aaaa_starvation() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let mut last_addr = Ipv6Addr::UNSPECIFIED;
        for i in 0..70u32 {
            let host = format!("host{i}.local");
            let addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, i as u16);
            last_addr = addr;
            let msg = synth_aaaa_only(&host, 120, addr);
            let records = parse_message(&msg).unwrap();
            fold_operational_into_cache(&records, &mut fold, &cache, Instant::now());
        }
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "host69.local"; // 70 番目 (旧 64 上限を上回る)
        let msg = synth_srv_txt_only(service, target, 5540, &["SII=5000"]);
        let records = parse_message(&msg).unwrap();
        fold_operational_into_cache(&records, &mut fold, &cache, Instant::now());

        let node = cache
            .get(service)
            .expect("the 70th distinct host's AAAA must not be starved by a global cap");
        assert_eq!(node.addresses, vec![last_addr]);
    }

    /// バックストップ（cache-flush を立てない実装向け・監査⑤の欠陥 1）:
    /// 旧アドレスは残るが、後から見たアドレスが先頭に並び先に試される。
    #[tokio::test(start_paused = true)]
    async fn fold_freshness_orders_latest_first() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "hosta.local";
        let a1: Ipv6Addr = "fd00::1".parse().unwrap();
        let a2: Ipv6Addr = "fd00::2".parse().unwrap();

        // cache-flush 無し（class=IN のみ）なので物理削除は起きない。
        let m1 = synth_aaaa_class(target, 120, a1, CLASS_IN);
        fold_operational_into_cache(
            &parse_message(&m1).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let m2 = synth_aaaa_class(target, 120, a2, CLASS_IN);
        fold_operational_into_cache(
            &parse_message(&m2).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );

        let msg = synth_srv_txt_only(service, target, 5540, &["SII=5000"]);
        fold_operational_into_cache(
            &parse_message(&msg).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );
        assert_eq!(
            cache.get(service).unwrap().addresses,
            vec![a2, a1],
            "freshest address must be tried first"
        );
    }

    /// 満杯 8 本での学習拒否の反転（監査⑤の欠陥 2 = 成功基準 2）: 9 本目は
    /// 最古を追い出して学習される。恒久 unreachable の根絶。
    #[tokio::test(start_paused = true)]
    async fn fold_full_pool_evicts_oldest_not_newest() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "hosta.local";
        let mut addrs = Vec::new();
        for i in 0..9u16 {
            let a = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, i + 1);
            addrs.push(a);
            let m = synth_aaaa_class(target, 120, a, CLASS_IN);
            fold_operational_into_cache(
                &parse_message(&m).unwrap(),
                &mut fold,
                &cache,
                Instant::now(),
            );
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        let msg = synth_srv_txt_only(service, target, 5540, &["SII=5000"]);
        fold_operational_into_cache(
            &parse_message(&msg).unwrap(),
            &mut fold,
            &cache,
            Instant::now(),
        );
        let node = cache.get(service).unwrap();
        assert_eq!(node.addresses.len(), MAX_ADDRS_PER_HOST);
        assert!(
            !node.addresses.contains(&addrs[0]),
            "oldest must be evicted"
        );
        assert_eq!(node.addresses[0], addrs[8], "newest must be first");
    }

    /// 立ち去ったノード(A)の完成後、A に触れない無関係な datagram を挟んでも
    /// A の expiry は延命されない — TTL(120s)通り +120s で失効する
    /// (最終レビュー #1b)。旧実装は fold 内の全累積 instance を毎 datagram
    /// 再 insert していたため、無関係な multicast が expiry を延命し続け、
    /// goodbye/TTL 失効が事実上機能しなかった。
    #[tokio::test(start_paused = true)]
    async fn fold_departed_instance_expires_and_is_not_refreshed() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service_a = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target_a = "hosta.local";
        let addr_a: Ipv6Addr = "fd00:1111:2222:1::a".parse().unwrap();

        let msg_a = synth_response(service_a, target_a, 5540, &["SII=5000"], addr_a);
        let records_a = parse_message(&msg_a).unwrap();
        fold_operational_into_cache(&records_a, &mut fold, &cache, Instant::now());
        assert!(
            cache.get(service_a).is_some(),
            "A should be cached initially"
        );

        tokio::time::advance(Duration::from_secs(119)).await;
        assert!(
            cache.get(service_a).is_some(),
            "A should still be fresh at +119s (ttl=120s)"
        );

        // 無関係な datagram: A に一切触れない (別ホストの AAAA のみ)。
        let addr_b: Ipv6Addr = "fd00:1111:2222:1::b".parse().unwrap();
        let msg_unrelated = synth_aaaa_only("unrelated.local", 120, addr_b);
        let records_unrelated = parse_message(&msg_unrelated).unwrap();
        fold_operational_into_cache(&records_unrelated, &mut fold, &cache, Instant::now());

        tokio::time::advance(Duration::from_secs(2)).await; // 合計 +121s
        assert!(
            cache.get(service_a).is_none(),
            "departed A must expire, not be refreshed by unrelated multicast"
        );
    }
}
