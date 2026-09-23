# Contributing to tollgate-rs

<!-- markdownlint-disable MD013 -->

tollgate-rs is the Rust implementation of TollGate: device-to-device
payment for metered resource delivery, built on Cashu ecash and Spilman
payment channels. The workspace is layered, bottom to top:

- **tollgate-protocol** — the wire format: messages, the CBOR codec and
  TCP framing. `no_std` + `alloc`.
- **tollgate-core** — the pure logic: grants, buying, metering and
  admission control. **Sans-IO**: it never does I/O, never reads the
  clock, never verifies a signature, and depends on no async runtime.
  `no_std` + `alloc`, so the same logic runs on an ESP32.
- **tollgate-net** — the node (`tollgated`) and its dashboard
  (`tolltop`). It turns real events into core `Event`s, executes the
  `Action`s core returns, and owns everything core refuses to touch:
  TCP, the shaper, the mint, the wallet, Spilman channels, and the
  nftables / FIPS adapters that enforce what a peer bought.

Most non-trivial changes affect behavior that only shows between two
nodes — what a peer is allowed to draw, when a purchase is refused, how
a channel rolls over, what money moves. A single-process `cargo test`
run is necessary but not sufficient for that class of change; the
docker topologies in [testing/](testing/) are where regressions
actually surface. This document covers the workflow assuming that
context. Protocol depth lives in [docs/design/](docs/design/), starting
with [tollgate-intro.md](docs/design/core/tollgate-intro.md).

## Quick start

```bash
git clone https://github.com/OpenTollGate/tollgate-rs.git
cd tollgate-rs
cargo build --workspace
cargo test --workspace
```

The pinned toolchain in [rust-toolchain.toml](rust-toolchain.toml) is
used for deterministic builds. A source build requires **`protoc`**
(`sudo apt install protobuf-compiler` on Debian/Ubuntu,
`brew install protobuf` on macOS): cdk's signatory reaches gRPC through
prost, whose build script shells out to it, and the build fails inside
that build script without it.

For multi-node integration runs, Docker is required. Each topology
under [testing/](testing/) starts containerized nodes and a fake
Lightning sat mint, and asserts against each node's control socket; see
[testing/README.md](testing/README.md) for the suite catalog.

## Choosing a branch to target

There is one long-lived branch, **`master`**. Target it with every PR.
There are no releases yet, so there is no maintenance branch to
backport to; this section will grow one when there is.

## Reporting bugs

Search [open issues](https://github.com/OpenTollGate/tollgate-rs/issues)
before filing a new one — duplicates are common in a young project.

When you open a bug report, please include:

- **Commit** you built from (`git rev-parse HEAD`), or the package
  version if you installed one.
- **Rust toolchain version** (`rustc --version`) if you built from
  source.
- **Platform** — Linux distro and kernel, macOS version, or OpenWrt
  release and router model.
- **What you expected to happen** — your mental model of the behavior,
  ideally referencing the relevant design doc or config field.
- **What actually happened** — the observed behavior, including the
  surprise.
- **Reproduction steps** — minimal and deterministic if you can.
  Multi-node bugs should include the topology (who sells, who buys,
  over the kernel path or FIPS) and per-node config excerpts. Leave
  secret keys and wallet tokens out.
- **Evidence** — log excerpts from both sides of the peering, with
  `RUST_LOG=tollgate_net=debug,info`. On OpenWrt that is `logread`; on
  macOS `/usr/local/var/log/tollgate/tollgate.log`. Add what `tolltop`
  shows for the peer if it is relevant — the grant in force, the rate
  being shaped to, the wallet balance.

One issue per bug. Don't bundle unrelated symptoms even if you suspect
they share a root cause — the maintainer will link them if they turn
out to be related.

## Submitting pull requests

### Scope discipline

Every PR should make one logical change. The reviewer should be able
to read the whole diff and trace every line back to the PR's stated
purpose.

- No drive-by reformatting of unrelated files.
- No unrelated refactors folded into a bug fix or a feature PR.
- No "while I was in there" cleanups in files outside the change's
  natural footprint. Send them as separate PRs; they'll usually land
  faster on their own.
- Pre-existing lint warnings in files you didn't touch are not yours
  to fix in this PR.

### Respect the layering

The crate boundaries are the design, not an accident of it.

- **Nothing in `tollgate-core` does I/O, reads the clock or verifies a
  signature.** A decision that needs the time takes `now_ms` from the
  host. A change that needs any of these belongs in `tollgate-net`,
  which hands core the result.
- **`tollgate-protocol` and `tollgate-core` stay `no_std` + `alloc`.**
  CI builds them for `thumbv7em-none-eabihf`; a dependency that pulls
  in `std` fails there.
- **Core trusts what it is handed.** The host verifies a message's
  signature before wrapping it in `Event::MessageReceived`. Core
  decides what is owed; it never decides whether a peer is who it
  claims to be.
- **The channel backend is a trait.** Code above `ChannelBackend`
  should not know whether it is paying over Spilman or anything else.

### Required before opening any PR

Run these locally and confirm they all pass:

```bash
cargo fmt --all --check
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p tollgate-protocol -p tollgate-core --target thumbv7em-none-eabihf
```

The last line needs the target installed once:
`rustup target add thumbv7em-none-eabihf`.

`fmt`, `clippy -D warnings` and the `no_std` build are CI gates — PRs
that fail any of them will fail CI and be sent back.

Then run the topology that exercises your change:

```bash
testing/scripts/build.sh               # build tollgate-test:latest once
SKIP_BUILD=1 testing/<suite>/test.sh
```

See [testing/README.md](testing/README.md) for the available suites
and what each covers: peering, purchase, refusal, rollover, allowance,
forwarding (the kernel path through nftables and `tc`), and fips (the
same over a FIPS mesh, which needs a second image). Pick the narrowest
one that touches your code path.

**Recommended before opening**: every suite CI runs.

```bash
testing/scripts/build.sh
for suite in peering purchase refusal rollover allowance forwarding; do
  SKIP_BUILD=1 testing/$suite/test.sh || break
done
```

This is the same matrix that runs on GitHub Actions. Catching a
regression locally is much cheaper than catching it in CI. The `fips`
suite is not in CI, because it needs a FIPS checkout this repository
does not carry; run it by hand if you touched the FIPS adapter.

### Self-review against the project review checklist

The 13-criteria checklist the maintainer runs on every incoming PR is
published at [PR-REVIEW.md](PR-REVIEW.md). Run your own change through
it before opening — or hand the document to your coding agent with
"review my branch against this checklist" and let it do the pass. The
checklist covers PR hygiene (body, commit shape, base freshness), diff
content (does the change do what the description says, does it fit the
codebase as a natural extension), and cross-cutting concerns (tests,
docs, dependencies, security, contributor-conventional Rust patterns).

This is the first thing the maintainer does on any submission, so
running it yourself saves a review round trip.

### Additional requirements for feature PRs

- **New test coverage.** Features added without a test that exercises
  them won't be reviewed. Logic in `tollgate-core` gets unit tests in
  the module's `tests.rs`, written against the design documents'
  worked examples where one exists. Behavior between nodes gets a
  topology under `testing/`, or an extension of an existing one.
  Coverage of just the happy path is fine for an initial PR; edge
  cases can land as follow-ups.
- **Documentation updated alongside the code.** Protocol changes
  update the relevant [docs/design/](docs/design/) page — and if a
  test in `tollgate-core` fails, check whether the doc or the code is
  right before changing either. Config changes update the example
  configs under [packaging/](packaging/) and [testing/](testing/).
  Behavior visible to operators updates [README.md](README.md) where it
  describes it.
- **A CHANGELOG entry** under `[Unreleased]` in
  [CHANGELOG.md](CHANGELOG.md).

### Additional requirements for bug-fix PRs

- **A regression test** where practical. If a regression test isn't
  tractable (some bugs only surface under timing or load that's hard
  to encode), say so in the PR description with a one-paragraph
  explanation.
- **Commit message references the bug**: the symptom, the root cause
  in one sentence, and the fix shape.

### Merge mechanics

PRs are merged via **squash-merge**. One logical change per PR becomes
one commit on `master`, which keeps `git bisect` useful across the
integration suite. Your in-PR commit history doesn't matter for the
final landed history — the maintainer rewrites the commit message at
merge time.

## AI coding assistant policy

Use of AI coding assistants (Claude Code, Copilot, Cursor, Aider, and
similar) in preparing a contribution is welcome. These tools are force
multipliers and we have no objection in principle to their use in
writing code, tests, documentation, or PR descriptions.

What we require is that the contributor does a thorough manual review
and editorial pass over the output before submission. Concretely:

- Verify that the code does what it claims, not just that it compiles.
- Verify that any tests the agent wrote actually test something
  useful, not just that they pass.
- Verify that any documentation matches the behavior.
- Spot-check the diff for nothing-surprising: no unrelated files
  modified, no fabricated APIs, no references to symbols that don't
  exist, no version bumps you didn't intend, no churn outside the
  change's natural footprint.
- Be ready to discuss the design choices in the PR as if you wrote
  every line, because for the purposes of accountability you did.

The coding agent is a tool. The contributor is the author of record
and is accountable for whatever they submit. PRs are reviewed on what
they contain, not on who or what wrote them.

**Review effort scales with submission effort.** A submission that
shows signs of being unreviewed agent output — irrelevant edits
scattered across the tree, hallucinated function names, mismatched
test/behavior pairs, fabricated API references, ChatGPT-style summary
prose in comments — will receive an AI-coding-agent reply in turn,
without human review. If you want a human reviewer's attention, do the
editorial pass yourself first.

Repeated submissions of unreviewed AI output will result in the
contributor being asked to step back and may result in account
restrictions.

## Where the conversation happens

- **GitHub issues** — bugs, feature requests, design discussions that
  don't fit on a specific PR.
- **GitHub PRs** — design discussion specific to a change in flight.
  Comment threads on the diff are the right place to push back on a
  decision.

For implementation questions specific to your PR, ask in the PR
itself. For design or roadmap questions that don't have a clear PR
home yet, file a GitHub issue with the `design` label.

## Further reading

- [PR-REVIEW.md](PR-REVIEW.md) — the 13-criteria PR review checklist
  the maintainer runs on every incoming PR; run it yourself before
  opening to save a round trip.
- [docs/design/core/tollgate-intro.md](docs/design/core/tollgate-intro.md)
  — goals, architecture, payment model; the start of the design tree.
- [testing/README.md](testing/README.md) — integration suite catalog.
- [packaging/README.md](packaging/README.md) — building the OpenWrt
  and macOS packages.
