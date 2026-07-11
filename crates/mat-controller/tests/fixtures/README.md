# Test certificate fixtures

connectedhomeip v1.4.2.0 `src/credentials/tests/CHIPCert_test_vectors.cpp`
(Apache-2.0) から抽出した公開テスト証明書。実デバイス・実 fabric とは無関係の
ダミー証明書（チェーン: Root01 → ICA01 → Node01_01）。

- `*_chip.bin`: Matter TLV 形式 / `*_der.bin`: 同一証明書の X.509 DER 形式
- `*_pubkey.bin`: P-256 公開鍵 (65B uncompressed) / `*_privkey.bin`: 秘密鍵 (32B)

再抽出手順は `docs/superpowers/plans/2026-07-11-phase5-m2-case-im.md` Task 4 参照。
