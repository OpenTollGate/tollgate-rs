# Getting TollGate

This guide says how to get the TollGate source, how to build it, and how to
use its crates from your own Rust project.

**You use a TollGate by running TollGate yourself.** A customer runs a node
of its own, or a program with a buyer session in it, and that pays the
gateway directly over the protocol. The same build serves both ends: a
gateway that sells and a customer that buys. (The captive portal in
tollgate-suite-wrt is for people who cannot run a client.) To run a node once you have it, see
[quickstart.md](quickstart.md). To build on the library, see
[embedding.md](embedding.md).

> **Before 1.0, everything here may change.** The Rust APIs, the wire format
> and the configuration keys can all change between releases, and a node on
> one release need not talk to a node on another. Pin a tag, and read
> [CHANGELOG.md](../../CHANGELOG.md) before you move to a newer one.

## What you get

The repository is a Rust workspace of three crates. Each one builds on the
one before it.

| Crate | What it is | Needs the standard library? |
|---|---|---|
| `tollgate-protocol` | The wire format: the messages two nodes exchange, their encoding (CBOR) and how they are framed on a TCP stream. Also the messages of the **enforcer protocol**, which `tollgated` speaks to a separate program that controls traffic. | No (`no_std` + `alloc`) |
| `tollgate-core` | The decisions: what a peer has paid for, when to buy, how much a peer may use, who may be carried. It does no input or output of its own and never reads the clock; the program around it hands it events and the time, and carries out what it returns. | No (`no_std` + `alloc`) |
| `tollgate-net` | A whole node for selling network access: the TCP connections, the shaper, the payment channels, the mint, the merchant, and the programs below. | Yes |

`tollgate-net` builds six programs:

| Program | What it does |
|---|---|
| `tollgated` | The node. It speaks the TollGate protocol with its peers, sells and buys, and tells an **enforcer** who may use how much. |
| `mintd` | The node's own mint, which issues the node's vouchers. One voucher is a claim on one unit (one byte, for network access). |
| `merchantd` | The node's money: it sells the node's vouchers, holds a wallet, and supplies the vouchers `tollgated` pays its own peers with. |
| `tolltop` | A live view of a running `tollgated`. |
| `minttop`, `merchanttop` | Live views of `mintd` and `merchantd`. |

Why three daemons, and what each one holds, is in
[tollgate-daemons.md](../design/core/tollgate-daemons.md).

**No `no_std` crate needs a feature flag.** None of the three crates defines
Cargo features, so there is nothing to turn on or off. `tollgate-protocol` and
`tollgate-core` are always `no_std`; `tollgate-net` always needs `std`, Tokio
and a Unix-like system. Some of `tollgate-net` builds only where it can work:
the `ip` enforcer only on Linux, the `fips` and `external` enforcers only on
Unix.

## Get the source

The repository lives in two places with the same history.

**From GitHub:**

```sh
git clone https://github.com/OpenTollGate/tollgate-rs.git
cd tollgate-rs
git checkout v0.1.0-rc.1
```

**Over Nostr, with [ngit](https://ngit.dev):** the primary home of the
repository is a `nostr://` remote, where pull requests and issues live. It
needs `ngit` and its `git-remote-nostr` helper installed
(`curl -fsSL https://ngit.dev/install.sh | bash`):

```sh
git clone nostr://npub1hw6amg8p24ne08c9gdq8hhpqx0t0pwanpae9z25crn7m9uy7yarse465gr/relay.ngit.dev/tollgate-rs
```

You can browse it on [gitworkshop.dev](https://gitworkshop.dev). Both places
carry `master` and the release tags.

## Build it

You need:

- **Rust 1.94.1.** The repository pins it in `rust-toolchain.toml`, and
  `rustup` installs it on the first build.
- **`protoc`**, the Protocol Buffers compiler. A library the mint uses runs
  it while building, and the build stops without it:
  `sudo apt install protobuf-compiler` on Debian or Ubuntu,
  `brew install protobuf` on macOS.

Then:

```sh
cargo build --release
```

The programs land in `target/release/`: `tollgated`, `mintd`, `merchantd`,
`tolltop`, `minttop` and `merchanttop`. To check that everything works:

```sh
cargo test --workspace
```

To build the two `no_std` crates for a microcontroller, add the target once
and build them alone:

```sh
rustup target add thumbv7em-none-eabihf
cargo build -p tollgate-protocol -p tollgate-core --target thumbv7em-none-eabihf
```

Packages for OpenWrt (`.ipk`) and macOS (`.pkg`) are built from the same
source; see [packaging/README.md](../../packaging/README.md).

## Depend on it from Rust

The crates are not on crates.io. Depend on them from Git, **pinned to a
tag**, so a new commit on `master` never changes your build:

```toml
[dependencies]
tollgate-protocol = { git = "https://github.com/OpenTollGate/tollgate-rs", tag = "v0.1.0-rc.1" }
tollgate-core     = { git = "https://github.com/OpenTollGate/tollgate-rs", tag = "v0.1.0-rc.1" }
tollgate-net      = { git = "https://github.com/OpenTollGate/tollgate-rs", tag = "v0.1.0-rc.1" }
```

Take only what you need:

- **Writing an external enforcer** (a program that controls traffic for
  `tollgated`)? `tollgate-protocol` alone is enough. It pulls in one small
  dependency, `minicbor`.
- **Running TollGate's decisions on your own hardware or runtime**, such as a
  microcontroller? `tollgate-core` and `tollgate-protocol`. You write the part
  that does the input and output.
- **Buying connectivity from your app or agent, or running a node inside
  your program?** `tollgate-net`. Its `client` module is a buyer session. It brings Tokio, the mint and wallet libraries, and their
  dependencies, so the first build takes a while and needs `protoc`.

Cargo fetches Git dependencies over HTTPS from GitHub. It cannot fetch a
`nostr://` URL, which is why the GitHub copy exists.

The crates' own version number is `0.1.0`, while the tag is `v0.1.0-rc.1`, a
release candidate. Cargo matches on the tag, so this does not matter for a Git
dependency.

### Checked

The three lines above were checked against the published tag on 2026-10-13:
a new crate depending on `tollgate-protocol`, `tollgate-core` and
`tollgate-net` by tag, from GitHub, passed `cargo check` with stable Rust 1.90
and `protoc` installed. Cargo resolved `tollgate-net` to commit
`95bf1b3980535201cbb5ba39b6a9b36516dcb8fe`. Your own crate does not inherit
the repository's `rust-toolchain.toml`. The crates declare
`rust-version = "1.85.0"`; their dependencies may need a newer one, and 1.90
is the oldest checked.

## Where next

- [quickstart.md](quickstart.md): run a gateway and a buying node on one
  machine, then buy from a real gateway.
- [embedding.md](embedding.md): embed a buyer in your app or agent, write
  an enforcer, run a node, read its state.
- [tollgate-intro.md](../design/core/tollgate-intro.md): what TollGate is and
  how it works, the start of the design documents.
