# TollGate Guides

Practical guides for people who run TollGate or build it into their own
programs. The [design documents](../design/README.MD) are the specification;
these guides link to them for detail.

| Guide | What it covers |
|---|---|
| [getting-tollgate.md](getting-tollgate.md) | Clone from GitHub or over Nostr, build, depend on the crates from Rust pinned to a tag, which crate does what |
| [quickstart.md](quickstart.md) | Run two nodes on one machine, a Linux gateway, a FIPS node and an external enforcer; named instances; `tolltop`; what working looks like |
| [embedding.md](embedding.md) | Buy from your own program, write an external enforcer, run a node, read its control socket |

Before 1.0, the APIs, the configuration and the wire format may change
between releases. Pin a tag, and read [CHANGELOG.md](../../CHANGELOG.md) when
you move.
