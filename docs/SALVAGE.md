# Salvage note — Amperstrand/tollgate-rs-ai-research-and-experiments

This branch is a **salvage pointer**, not development history. It preserves the
exact git objects of the default branch (`experimental`) of
`Amperstrand/tollgate-rs-ai-research-and-experiments` — a design/ai-research
twin of this repo — immediately before that repository was archived
(2026-08-29/30, plan `amperstrand-nfc-mcu-dedup` todo 5c, user decision
2026-08-29 "archive after verify").

## Why this branch exists

Comparison at salvage time (fetched refs, not stale clones):

- Canonical: `OpenTollGate/tollgate-rs` `master` @ `76cb483d8b09ebe03eedc00e7c0f8695c2b4952b`
- Duplicate: `Amperstrand/tollgate-rs-ai-research-and-experiments` `experimental` @ `38de8534a7653f5f73d9c84dd5d4b566eb1a5c2e`
- Unique commits on the duplicate's default branch: **78** (`git log old/experimental ^origin/master`)
- Content diff vs canonical master: **83 files changed, 19386 insertions(+), 886 deletions(-)**

Diverged design-phase work (multi-mint CDK wallet, MIPS cross-compilation CI,
double-spend prevention, crash-recovery suite, OpenWrt packaging experiments,
Spilman channels, etc.). Archival proceeded only after this branch was pushed.

## Branch inventory of the archived repo (unique commits vs all canonical refs)

| Branch (archived repo) | Unique commits |
|---|---|
| experimental (default; = this branch's parent) | 78 |
| master | 78 |
| conformance-pay-token | 7 |
| copilot/finish-spilman-channel-correctness | 110 |
| dependabot/cargo/quinn-proto-0.11.16 | 76 |
| dependabot/cargo/serde_with-3.21.0 | 20 |
| feat/openwrt-packaging | 14 |
| feat/protocol-completeness | 3 |
| feat/v1-compat | 4 |
| feat/v1-rebase | 24 |
| feat/v2-rebase | 9 |
| feat/v3-rebase | 7 |
| m2/cashu-verify-endpoint | 161 |
| m3/spilman-channels | 34 |
| m4/ipk-physical-e2e | 133 |
| m4/v1-compat-on-upstream | 16 |
| m7/openwrt-packaging-v2 | 27 |
| test/v1-cors-contract | 8 |
| upstream/cddl-schema | 1 |
| upstream/codec-offsets | 1 |
| upstream/pricing-overflow | 1 |

All 21 branches remain fully clonable from the archived (read-only) repository.
Do not build on this branch; treat it as read-only provenance.
