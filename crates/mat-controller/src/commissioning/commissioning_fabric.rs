//! 使い捨て第二 fabric（CommissioningFabric: 自己発行 root と NOC 発行、KVS bootstrap）。
//!
//! --- 使い捨て第二 fabric ---

use crate::case;
use crate::cert::{self, MatterCert};
use crate::fabric::{self, FabricCredentials};
use crate::kvs::SelfIssueMaterials;

use super::CommissionError;

/// 使い捨て第二 fabric の素材（spec 決定 4: 永続化しない、呼び出し側が
/// 生成して持つ）。1 回のコミッショニングの寿命だけ生きる root 証明書 +
/// root 秘密鍵 + epoch IPK。`Drop` 時に KVS には一切書かれない——このフロー
/// が使う fabric は commissioning が終わった時点で controller のプロセス
/// メモリ上にしか存在しない使い捨てである。
pub struct CommissioningFabric {
    pub rcac_tlv: Vec<u8>,
    root_private_key: [u8; 32],
    pub fabric_id: u64,
    pub ipk_epoch: [u8; 16],
    pub admin_node_id: u64,
}

/// 手動 `Debug`: root 秘密鍵と epoch IPK はどちらも秘匿情報。
/// `fabric::FabricCredentials` の redaction 方針を踏襲する——このリポジトリ
/// は公開なので、エラー文脈やテスト失敗出力での `{:?}` 経由の意図しない
/// 漏洩を避ける。
impl std::fmt::Debug for CommissioningFabric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommissioningFabric")
            .field("rcac_tlv_len", &self.rcac_tlv.len())
            .field("root_private_key", &"[REDACTED]")
            .field("fabric_id", &self.fabric_id)
            .field("ipk_epoch", &"[REDACTED]")
            .field("admin_node_id", &self.admin_node_id)
            .finish()
    }
}

impl CommissioningFabric {
    /// 新しい self-signed RCAC と 16 バイトの epoch IPK を生成する。
    pub fn generate(fabric_id: u64, admin_node_id: u64) -> Result<Self, CommissionError> {
        let (rcac, root_private_key) = cert::generate_rcac()?;
        let mut ipk_epoch = [0u8; 16];
        getrandom::getrandom(&mut ipk_epoch).map_err(|_| CommissionError::Malformed {
            step: "commissioning_fabric_generate",
            detail: "os rng failure",
        })?;
        Ok(Self {
            rcac_tlv: rcac.to_tlv(),
            root_private_key,
            fabric_id,
            ipk_epoch,
            admin_node_id,
        })
    }

    /// chip-tool KVS の自己発行資材から、**既存 fabric** 上で commissioning
    /// するための CommissioningFabric を組む。`generate`（新規 fabric）と
    /// 対になる読み込み側。AddNOC でデバイスへ渡す IPK は **epoch** 側 —
    /// operational を渡すとデバイス側の KDF 導出が二重になり CASE が壊れる。
    /// epoch は呼び出し側が解決して渡す引数（M8c-3: `mat/f/<idx>/ipk-epoch`
    /// に永続される。`mat-native::commission::resolve_ipk_epoch` が、KVS に
    /// 既存の永続 epoch があればそれを、無ければ `fabric::verify_default_ipk_epoch`
    /// のガード通過後に `fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH` を採用して永続化する）。
    pub fn from_materials(m: crate::kvs::SelfIssueMaterials, ipk_epoch: [u8; 16]) -> Self {
        Self {
            rcac_tlv: m.rcac,
            root_private_key: m.root_private_key,
            fabric_id: m.fabric_id,
            ipk_epoch,
            admin_node_id: m.node_id,
        }
    }

    /// controller 自身の CASE 用 credentials（NOC 自己発行を再利用）。
    ///
    /// AddNOC でデバイスに渡す IPK は **epoch** 側（`self.ipk_epoch`、
    /// fabric 全体で共有される groupKeySet の鍵そのもの）。対して CASE の
    /// destination id 計算に使うのは **operational** 側——
    /// `fabric::derive_ipk_operational(&self.ipk_epoch, &cfid)` で epoch か
    /// ら導出する別物で、`FabricCredentials` にはこちらを積む。取り違える
    /// と CASE の宛先 id 計算がデバイス側と食い違う。
    pub fn admin_credentials(&self) -> Result<FabricCredentials, CommissionError> {
        let rcac = MatterCert::parse(&self.rcac_tlv)?;
        let cfid = fabric::compressed_fabric_id(&rcac.pub_key, self.fabric_id);
        let ipk_operational = fabric::derive_ipk_operational(&self.ipk_epoch, &cfid);
        let materials = SelfIssueMaterials {
            rcac: self.rcac_tlv.clone(),
            root_private_key: self.root_private_key,
            ipk_operational,
            node_id: self.admin_node_id,
            fabric_id: self.fabric_id,
        };
        Ok(FabricCredentials::from_self_issued(materials)?)
    }

    /// CSR の公開鍵にデバイス NOC を発行して TLV で返す。
    pub fn issue_device_noc(
        &self,
        op_public_key: &[u8; 65],
        node_id: u64,
    ) -> Result<Vec<u8>, CommissionError> {
        let rcac = MatterCert::parse(&self.rcac_tlv)?;
        let mut serial = [0u8; 8];
        getrandom::getrandom(&mut serial).map_err(|_| CommissionError::Malformed {
            step: "issue_device_noc",
            detail: "os rng failure",
        })?;
        serial[0] &= 0x7F; // BER INTEGER の最小正表現を維持
        let noc = cert::issue_noc(
            op_public_key,
            node_id,
            self.fabric_id,
            &rcac,
            &self.root_private_key,
            &serial,
        )?;
        Ok(noc.to_tlv())
    }

    /// 初回 fabric bootstrap（M8c-3）: この fabric を chip-tool INI 互換 KVS へ
    /// 新規永続する。書くもの:
    ///   alpha ini … ExampleOpCredsCAKey<issuer> = pub65||priv32（97B）
    ///   main ini  … f/<idx>/r = RCAC(TLV) / f/<idx>/n = admin NOC(TLV)
    ///               f/<idx>/k/0 = IPK keyset blob（3 スロット、終端 0xFFFF）
    ///               mat/f/<idx>/ipk-epoch = ランダム epoch（mat 専用キー）
    ///               g/gdc = Global Group Data Counter（spec 4.5.1 レンジの
    ///               ランダム初期値、u32 LE — 欠落だと groupcast 永久不可）
    /// 既に KVS があれば `KvsError::AlreadyExists`（上書きしない — 誤 store
    /// パスでのサイレント別 fabric 生成を防ぐ、spec ユーザー決定）。
    pub fn write_kvs_bootstrap(
        &self,
        store: &std::path::Path,
        fabric_index: u8,
        issuer_index: u8,
    ) -> Result<(), crate::kvs::KvsError> {
        use crate::kvs::KvsTxn;
        let alpha_path = store.join("chip_tool_config.alpha.ini");
        let main_path = store.join("chip_tool_config.ini");
        // どちらか一方でも実在したら拒否（中途半端な store を悪化させない）。
        if alpha_path.exists() || main_path.exists() {
            return Err(crate::kvs::KvsError::AlreadyExists);
        }

        let rcac = MatterCert::parse(&self.rcac_tlv).map_err(|_| crate::kvs::KvsError::BadNoc {
            fabric_index,
            reason: "generated rcac unparseable (bug)",
        })?;
        let cfid = fabric::compressed_fabric_id(&rcac.pub_key, self.fabric_id);
        let operational = fabric::derive_ipk_operational(&self.ipk_epoch, &cfid);

        // admin NOC: 使い捨て op 鍵で自己発行（f/<idx>/n はリーダが node_id /
        // fabric_id を読むためだけに使う — 実行時の CASE 用 NOC は毎回
        // FabricCredentials::from_self_issued が自己発行するので秘密鍵は捨てる）。
        let op_secret = case::random_p256_secret();
        let op_public = case::eph_pub_bytes(&op_secret);
        let admin_noc = self
            .issue_device_noc(&op_public, self.admin_node_id)
            .map_err(|_| crate::kvs::KvsError::BadNoc {
                fabric_index,
                reason: "admin noc issuance failed (bug)",
            })?;

        // alpha ini
        let mut ca_key = Vec::with_capacity(97);
        ca_key.extend_from_slice(&rcac.pub_key);
        ca_key.extend_from_slice(&self.root_private_key);
        let mut alpha = KvsTxn::create(&alpha_path)?;
        alpha.set(&format!("ExampleOpCredsCAKey{issuer_index}"), &ca_key);
        alpha.commit()?;

        // main ini（1 flock 区間 + 1 commit）
        let gkh = fabric::derive_group_session_id(&operational);
        let mut main = KvsTxn::create(&main_path)?;
        main.set(&format!("f/{fabric_index}/r"), &self.rcac_tlv);
        main.set(&format!("f/{fabric_index}/n"), &admin_noc);
        main.set(
            &format!("f/{fabric_index}/k/0"),
            &crate::group_settings::serialize_keyset(
                0,
                crate::group_settings::EPOCH_START_TIME,
                gkh,
                &operational,
                0xFFFF,
            ),
        );
        main.set(
            &crate::kvs::mat_ipk_epoch_key(fabric_index),
            &self.ipk_epoch,
        );
        // g/gdc（Global Group Data Counter）: これが無いと mat-native の
        // init_sender が groupcast を永久拒否する（低い counter で始めると
        // 受信側 replay 窓に全弾落とされるため欠落=拒否が正しい）。SDK の
        // GroupOutgoingCounters と同様、初回はランダム初期化して永続する。
        main.set("g/gdc", &crate::counter::random_initial().to_le_bytes());
        main.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_roundtrip_via_kvs_readers() {
        let dir = tempfile::tempdir().unwrap();
        let fab = CommissioningFabric::generate(1, 112233).unwrap();
        fab.write_kvs_bootstrap(dir.path(), 1, 0).unwrap();
        // 既存リーダで読み戻せる = chip-tool INI 互換形式の証明
        let m = crate::kvs::read_self_issue_materials(
            &dir.path().join("chip_tool_config.alpha.ini"),
            &dir.path().join("chip_tool_config.ini"),
            1,
            0,
        )
        .unwrap();
        assert_eq!(m.fabric_id, 1);
        assert_eq!(m.node_id, 112233);
        // epoch → operational の導出チェーンが KVS の中身と一致
        let creds = crate::fabric::FabricCredentials::from_self_issued(m.clone()).unwrap();
        let epoch = crate::kvs::read_mat_ipk_epoch(&dir.path().join("chip_tool_config.ini"), 1)
            .unwrap()
            .expect("epoch persisted");
        let cfid = crate::fabric::compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
        assert_eq!(
            crate::fabric::derive_ipk_operational(&epoch, &cfid),
            m.ipk_operational
        );
        // 二重 init は拒否
        assert!(matches!(
            fab.write_kvs_bootstrap(dir.path(), 1, 0),
            Err(crate::kvs::KvsError::AlreadyExists)
        ));
    }

    /// Task 11 (`mat fabric list`): bootstrap 直後の main KVS から index / NOC
    /// identity / RCAC 公開鍵が読めること。
    #[test]
    fn bootstrap_kvs_exposes_noc_identity_and_rcac_pubkey() {
        let dir = tempfile::tempdir().unwrap();
        let fab = CommissioningFabric::generate(7, 112233).unwrap();
        fab.write_kvs_bootstrap(dir.path(), 1, 0).unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        assert_eq!(crate::kvs::list_fabric_indices(&ini).unwrap(), vec![1]);
        assert_eq!(crate::kvs::read_noc_identity(&ini, 1).unwrap(), (112233, 7));
        let pk = crate::kvs::read_rcac_pubkey(&ini, 1).unwrap();
        assert_eq!(pk[0], 0x04, "SEC1 uncompressed");
    }

    /// 監査 Tier 3: bootstrap が `g/gdc` を書かないと、fabric init 由来の
    /// ストアでは `mat-native::group::init_sender` が「counter を低く始め
    /// られない」と groupcast / bump を永久拒否する。初期値は spec 4.5.1 の
    /// ランダムレンジ [1, 2^28]（`TxCounter::new_random` と同じ規律）。
    #[test]
    fn bootstrap_writes_group_data_counter() {
        let dir = tempfile::tempdir().unwrap();
        let fab = CommissioningFabric::generate(1, 112233).unwrap();
        fab.write_kvs_bootstrap(dir.path(), 1, 0).unwrap();
        let gdc = crate::kvs::read_group_data_counter(&dir.path().join("chip_tool_config.ini"))
            .unwrap()
            .expect("g/gdc persisted by bootstrap");
        assert!((1..=(1u32 << 28)).contains(&gdc), "gdc: {gdc}");
    }

    #[test]
    fn commissioning_fabric_issues_valid_credentials() {
        let fab = CommissioningFabric::generate(0xFAB1, 0x1_0001).unwrap();
        let creds = fab.admin_credentials().unwrap();
        assert_eq!(creds.fabric_id, 0xFAB1);
        assert_eq!(creds.node_id, 0x1_0001);
        // デバイス NOC も同じ root でチェーン検証が通る
        let dev = crate::case::random_p256_secret();
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let dev_pub: [u8; 65] = dev
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .unwrap();
        let noc_tlv = fab.issue_device_noc(&dev_pub, 0x2_0001).unwrap();
        let noc = crate::cert::MatterCert::parse(&noc_tlv).unwrap();
        let rcac = crate::cert::MatterCert::parse(&fab.rcac_tlv).unwrap();
        crate::cert::verify_noc_chain(&noc, None, &rcac).unwrap();
    }

    #[test]
    fn commissioning_fabric_from_materials_maps_fields() {
        let m = crate::kvs::SelfIssueMaterials {
            rcac: vec![0x15, 0x01, 0x02],
            root_private_key: [7u8; 32],
            ipk_operational: [8u8; 16],
            node_id: 0xAA55,
            fabric_id: 0xFAB2,
        };
        let f = CommissioningFabric::from_materials(m, [9u8; 16]);
        assert_eq!(f.rcac_tlv, vec![0x15, 0x01, 0x02]);
        assert_eq!(f.fabric_id, 0xFAB2);
        assert_eq!(f.ipk_epoch, [9u8; 16]);
        assert_eq!(f.admin_node_id, 0xAA55);
    }
}
