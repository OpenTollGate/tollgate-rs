# TollGate Guides

Practical guides for people who run TollGate or build it into their own
programs. The [design documents](../design/README.MD) are the specification;
these guides link to them for detail.

**The intended way to use a TollGate is to run a TollGate node of your own
that pays the gateway directly.** A customer's laptop, router, app or agent
runs `tollgated` set to buy, or embeds a buyer session, and pays over the
protocol. The captive portal and proxies of tollgate-suite-wrt (`proxyd`,
`exitd`) exist so that people without such a client can use a TollGate too;
they buy on the device's behalf. Start with
[running a buying node](quickstart.md#use-a-tollgate-as-a-customer-run-a-buying-node).

| Guide | What it covers |
|---|---|
| [getting-tollgate.md](getting-tollgate.md) | Clone from GitHub or over Nostr, build, depend on the crates from Rust pinned to a tag, which crate does what |
| [quickstart.md](quickstart.md) | Run a gateway and a buying node (a customer) on one machine; buy from a real gateway; run a Linux gateway, a FIPS node and an external enforcer; named instances; `tolltop`; what working looks like |
| [embedding.md](embedding.md) | Embed a buyer in your app or agent, write an external enforcer, run a node, read its control socket |

Before 1.0, the APIs, the configuration and the wire format may change
between releases. Pin a tag, and read [CHANGELOG.md](../../CHANGELOG.md) when
you move.
