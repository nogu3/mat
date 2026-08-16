# chip-test-attestation

Task 10 (M2 Echo attestation checkpoint) experiment material: the
**canonical connectedhomeip test attestation chain** for VID `0xFFF1` /
PID `0x8000`, vendored so `matv` can offer `attestation = "chip-test"` in
`matv.toml` as an alternative to `x509::generate_dev_attestation`'s
self-generated (fresh-every-boot) chain.

## What this is (and isn't)

This is **public Matter TEST material**, not secrets and never for
production device identity — the same caveat `mat-controller/src/cd.rs`
documents for `TEST_CD_SIGNING_KEY`. The DAC private key below is
published in plaintext in the connectedhomeip repository; anyone can sign
with it. It proves nothing about CSA certification. It exists only so a
commissioner that pins/prefers the well-known chip test identity (VID
FFF1 test vendor, `Chip-Test-*` cert names) has something to match against
— the working hypothesis being that Echo's cloud-side attestation
validation accepts the canonical chain but rejects our self-generated one.

## Source: PAA / PAI / DAC cert + key

Fetched via `raw.githubusercontent.com` from
`project-chip/connectedhomeip`, `master` branch, commit
`0f267927e02ce234ec75a7a4970104a73bcc06dc` (2026-08-16), from
`credentials/test/attestation/`:

| file | upstream path | sha256 |
| --- | --- | --- |
| `Chip-Test-PAA-FFF1-Cert.der` | `credentials/test/attestation/Chip-Test-PAA-FFF1-Cert.der` | `369eef40616645e5bd3e1bc9db70815d6cf93e9a611973ac20c9cd14c8d60c37` |
| `Chip-Test-PAI-FFF1-8000-Cert.der` | `credentials/test/attestation/Chip-Test-PAI-FFF1-8000-Cert.der` | `4f910bbf0e8a0208ea29dba71c549e9da375e792c6b8f271119dfacc1b5ab780` |
| `Chip-Test-DAC-FFF1-8000-0000-Cert.der` | `credentials/test/attestation/Chip-Test-DAC-FFF1-8000-0000-Cert.der` | `f766d18a73b2ee86aef2609e1093eed1a5d46e17f63d0bbcd8a61a6de36e7e83` |
| `Chip-Test-DAC-FFF1-8000-0000-Key.der` | `credentials/test/attestation/Chip-Test-DAC-FFF1-8000-0000-Key.der` | `cccec4ffa986623f40ecd4323d4e10f3b927e3028916816fdba2f60ce8ed0463` |

**Deviation from the task brief**: the brief guessed
`Chip-Test-PAA-NoVID-Cert.der` as the PAA. That file exists upstream but
is the *wrong* issuer for this DAC/PAI pair — its subject DN has no VID
RDN, while `Chip-Test-PAI-FFF1-8000-Cert.der`'s issuer DN carries
`1.3.6.1.4.1.37244.2.1 = FFF1`, so `openssl verify` fails against
NoVID (`unable to get local issuer certificate`) and succeeds against
`Chip-Test-PAA-FFF1-Cert.der`:

```
$ openssl verify -no_check_time \
    -CAfile Chip-Test-PAA-FFF1-Cert.der \
    -untrusted Chip-Test-PAI-FFF1-8000-Cert.der \
    Chip-Test-DAC-FFF1-8000-0000-Cert.der
Chip-Test-DAC-FFF1-8000-0000-Cert.der: OK
```

Chain is `Chip-Test-PAA-FFF1-Cert.der` → `Chip-Test-PAI-FFF1-8000-Cert.der`
→ `Chip-Test-DAC-FFF1-8000-0000-Cert.der`.

## Source: Certification Declaration

**`Chip-Test-CD-FFF1-8000.der` is *not* fetched from connectedhomeip** —
`credentials/test/certification-declaration/` upstream only ships
pre-built CD blobs for VID/PID pairs `FFF2`/`FFF3` (used for the
"second/third test vendor" cross-vendor scenarios), never for the default
`FFF1`/`8000` pair the example apps and this chain use. This was
discovered while implementing Task 10 (the task brief assumed the file
existed by that name; it doesn't — see the repo directory listing).

Instead this CD is **locally built** with the exact same public
"Matter Test CD Signing Authority" key connectedhomeip publishes at
`credentials/test/certification-declaration/Chip-Test-CD-Signing-Key.pem`
— the identical key `mat-controller/src/cd.rs`'s `TEST_CD_SIGNING_KEY`
already embeds (see that file's module doc for the full provenance/
security note: the key is not secret, chip SDK builds trust it by
default via `gTestCdPubkeyKid`/`gTestCdPubkeyBytes`). Generation command
(one-off, via a throwaway `mat-controller` example that was deleted after
use):

```rust
mat_controller::cd::generate_dev_certification_declaration(
    0xFFF1, 0x8000, mat_controller::im::DEVICE_TYPE_ON_OFF_LIGHT, // 0x0100
)
```

sha256 of the result: `89854838450139e50cc1bb72d61b0eb14354b551236ee8680ec9cf8fcd0320e7`.

This makes the CD *cryptographically* canonical (signed by the same
well-known test authority key any chip-based verifier already trusts,
with the correct VID/PID/device_type), but it is **not byte-identical to
anything upstream ships** — that's the one place this "chip-test" mode
still generates rather than replays a vendored blob, because there is
nothing to replay. If Echo's validation is sensitive to this CD's exact
bytes (as opposed to just "signed by the known test authority, VID/PID
match"), this is a residual difference from a real Home Assistant/matter.js
pairing and worth re-checking first if the chip-test experiment still
fails against Echo.

## Source: DAC private key (embedded as a Rust const)

Per Task 10's constraint (no new DER-parsing dependency), the DAC private
key is **not loaded from `Chip-Test-DAC-FFF1-8000-0000-Key.der`
at runtime** — its raw 32-byte P-256 scalar is embedded as a documented
`const` in `mat-device`'s chip-test attestation module, extracted with:

```
$ openssl ec -inform der -in Chip-Test-DAC-FFF1-8000-0000-Key.der -text -noout
```

and reading the `priv:` field. A test verifies the embedded scalar's
derived public key matches `Chip-Test-DAC-FFF1-8000-0000-Cert.der`'s
`SubjectPublicKey` byte-for-byte (`mat_controller::x509::parse_x509`).
The `.der`/`.pem` key file is kept in this directory anyway, for
provenance/audit — it is not read by any code path.
