# Quickstart: run TollGate

This guide runs TollGate nodes and shows what a working node looks like.

**You use a TollGate by running TollGate yourself.** The customer runs a
TollGate node of its own, a **buying node**, and it pays the gateway
directly over the protocol: it opens a payment channel, buys a rate, and the
gateway carries it at that rate. A laptop, a home router, a phone app or a
software agent can all be the customer. The captive portal and proxies of
tollgate-suite-wrt (`proxyd`, `exitd`) exist so that people **without** such
a client can still use a TollGate; they buy on the device's behalf.

So the guide starts with the customer's side:
[run a buying node](#use-a-tollgate-as-a-customer-run-a-buying-node), and
[try it on one machine](#try-it-on-one-machine-a-gateway-and-a-buying-node)
against a gateway of your own, which needs nothing but the build. Then it
covers gateways: one on Linux with the `ip` enforcer, one on a FIPS mesh,
and one behind an external enforcer.

To get and build TollGate first, see [getting-tollgate.md](getting-tollgate.md).
For every configuration key, see
[tollgate-configuration.md](../design/core/tollgate-configuration.md); this
guide uses only a few. To buy from inside your own program instead of
running `tollgated`, see [embedding.md](embedding.md).

> **Before 1.0, configuration keys and behavior may change between releases.**
> The commands and files here match the tag `v0.1.0-rc.1`.

## The parts of a node

A node is three programs, each with its own YAML file
([tollgate-daemons.md](../design/core/tollgate-daemons.md)):

| Program | File | What it does |
|---|---|---|
| `tollgated` | `tollgate.yaml` | Speaks TollGate with peers. Sells and buys, and tells an **enforcer** who may use how much. |
| `mintd` | `mint.yaml` | The node's own mint. It issues the node's **vouchers**: one voucher is a claim on one byte of this node's capacity. |
| `merchantd` | `merchant.yaml` | The node's money. It holds a wallet, sells the node's vouchers, and gives `tollgated` the vouchers it pays its own peers with. |

The **enforcer** is what applies `tollgated`'s decisions to real traffic: it
lets a paying peer through, slows it to the rate it bought, and counts what it
carried. It is chosen with `enforcer.kind`:

| `kind` | Enforces | Runs on |
|---|---|---|
| `loopback` (default) | A TCP connection of its own that carries filler bytes. Nobody's real traffic. For trying TollGate out and for tests. | Linux and macOS |
| `ip` | The kernel's forwarding path, with nftables and `tc`. Sells internet access on a router. | Linux, as root or with `CAP_NET_ADMIN` |
| `fips` | Transit across a [FIPS](https://github.com/jmcorgan/fips) mesh, through the FIPS daemon. | Unix, beside `fipsd` |
| `external` | Whatever a separate program controls, over the enforcer protocol. | Unix |

**No real money moves in this guide.** `mintd` runs with `auto_accept` on by
default, so it gives its vouchers to anyone who asks. That lets a buyer open a
payment channel without paying anyone, and it means service is free to any
peer that can reach the mint. A node that really sells turns it off.

## Before you start

Build the programs and put them on your `PATH`:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

If your Cargo puts its builds elsewhere (`build.target-dir` in
`~/.cargo/config.toml`, or `CARGO_TARGET_DIR`), use that directory instead of
`target/`.

## Use a TollGate as a customer: run a buying node

A buying node is `tollgated` set up to buy from one gateway and sell to
nobody. It needs:

- **a key of its own** (`identity.secret_key`);
- **the gateway's public key and address**, under `peers`;
- **`no_charge: true` on that gateway.** This says the node will not charge
  the gateway, so the gateway opens no payment channel toward it, and the
  node needs **no mint and no `mintd`**;
- **`merchantd`**, which holds the money and supplies the vouchers the node
  pays with. It needs no market: `market: false`;
- **something it wants**: `buying.demand`, in bytes a second, or the
  `--demand` flag. A node buys only while it wants something.

The configuration document specifies `selling: false` for exactly this
([Selling](../design/core/tollgate-configuration.md#selling)), one switch
instead of `no_charge` on every peer. **It is not implemented yet:**
`tollgated` refuses to start with a `selling` key. Use `no_charge` per peer
until it is.

`tollgate.yaml` for a customer of a gateway on the LAN:

```yaml
identity: { secret_key: "…" }          # tollgated --show-identity prints a new pair
merchant: { socket: "/run/merchantd.sock" }
network: { listen: "0.0.0.0:4747" }
buying:
  demand: 2000000                      # want 2 MB/s from the gateway
peers:
  "02f383c916faa702f91b5e836ed6f12d5f80fb1752ff4db28e6e77997036befd74":   # the gateway's key
    endpoint: "192.168.1.1:4747"       # where its tollgated listens
    no_charge: true                    # buy only: no channel toward us, no mint
```

and `merchant.yaml`:

```yaml
socket: "/run/merchantd.sock"
market: false
wallet: { file: "/var/lib/tollgate/wallet.sqlite", seed_file: "/var/lib/tollgate/wallet.seed" }
```

Get the gateway's key from its operator, or from the gateway itself with
`tollgated -c /etc/tollgate/tollgate.yaml --show-identity`.

**Over FIPS**, name the gateway by its key and reach it over the mesh. The
endpoint can be its `fips0` address, or its `<npub>.fips` name where your
FIPS node resolves those names (`tollgated` resolves an endpoint's host name
like any program; this guide has not tried a `.fips` name):

```yaml
peers:
  "02f383c9…":
    endpoint: "[fde5:b78f:c4ef:a89c:82cd:b05b:8ee7:ed56]:4747"   # or "npub1….fips:4747"
    no_charge: true
enforcer:
  kind: fips          # checks that the gateway is who its key says
network:
  listen: "[::]:4747"
```

On a mesh the buying node must run FIPS with **the same key** as
`tollgated` (see [A FIPS node](#a-fips-node)).

**Paying with real money.** In this guide the gateway's mint gives its
vouchers away (`auto_accept`), so the customer's `merchantd` takes what it
needs for nothing. Against a gateway that sells, `merchantd` must buy the
gateway's vouchers with money, from the gateway's market. That step is not
on the funding path in this release (`merchantd` "funds" only from mints
that give vouchers away), so today a buying node works against gateways with
`auto_accept` on.

**What to check:**

- `tolltop` (on the customer) shows the gateway with a **bought** budget and
  a reserved rate, right of the divide.
- The customer's `link` lines show `down` at the rate it reserved: its
  demand plus a quarter (`buying.headroom_pct`, 125 by default).
- Traffic flows at that rate. With the `ip` gateway, that is your real
  traffic through the router.

The next section does all of this on one machine, with real output.

## Try it on one machine: a gateway and a buying node

Two nodes: a **gateway** that sells, and a **customer** that only buys from
it. Both use the `loopback` enforcer, so the bytes bought and sold are
filler on a local TCP connection, but the payments, the channels and the
shaping are real. The gateway runs `mintd` and `merchantd`; the customer runs
`merchantd` only.

All the files go in one directory. Keep its path short: Unix socket paths
are limited to about 100 characters, and the daemons refuse a longer one
("path must be shorter than SUN_LEN").

```sh
WORK=/tmp/tollgate-try
mkdir -p $WORK && cd $WORK
```

### 1. Make two keys

Every node needs a secret key, and a buyer names its seller by public key.
`tollgated --show-identity` prints a new pair when it has no config:

```sh
tollgated --show-identity > gateway.id
tollgated --show-identity > customer.id
GATEWAY_PUB=$(awk '/pubkey/ {print $2}' gateway.id)
GATEWAY_SEC=$(awk '/secret_key/ {print $2}' gateway.id)
CUSTOMER_SEC=$(awk '/secret_key/ {print $2}' customer.id)
```

```text
$ cat gateway.id
pubkey:     0331cb3e2e69d1802c79370f0deec4e60abb542dc1e7d9419e3a2c5526d47f7750
secret_key: …
```

### 2. Configure the gateway

Its mint, on 3338 for peers and 3337 for its merchant:

```sh
cat > gateway-mint.yaml <<EOF
url: "http://127.0.0.1:3338"
seed_file: "$WORK/gateway-mint.seed"
file: "$WORK/gateway-mint.sqlite"
public: { listen: "127.0.0.1:3338" }
private: { listen: "127.0.0.1:3337" }
control_socket: "$WORK/gateway-mint.sock"
EOF
```

Its merchant:

```sh
cat > gateway-merchant.yaml <<EOF
listen: "127.0.0.1:3340"
socket: "$WORK/gateway-merchant.sock"
control: "$WORK/gateway-merchant-control.sock"
mint: { url: "http://127.0.0.1:3338", private: "http://127.0.0.1:3337" }
wallet: { file: "$WORK/gateway-wallet.sqlite", seed_file: "$WORK/gateway-wallet.seed" }
EOF
```

And the node. It names its mint and its merchant, and gives every peer a
small free **minimum flow** of 4096 bytes a second, enough to reach the mint
before it has paid for anything:

```sh
cat > gateway.yaml <<EOF
identity: { secret_key: "$GATEWAY_SEC" }
mint: { url: "http://127.0.0.1:3338" }
merchant: { socket: "$WORK/gateway-merchant.sock" }
access:
  minimum_flow: { enabled: true, bytes_per_second: 4096 }
grants:
  from_payer_weight: 0
network: { listen: "127.0.0.1:4747" }
EOF
```

**`from_payer_weight: 0` is needed here, because of the loopback enforcer.**
It makes what the customer sends free. The loopback enforcer sends filler
bytes to every peer as fast as it is allowed to, and the customer does not
limit a peer it does not charge, so it sends the gateway filler as fast as
the machine can, over a gigabyte a second. With the default weight of 1 the
gateway draws all of that from the customer's budget: in a test run, each
1 GiB channel the customer opened was spent in about a second. A real
gateway carries real traffic instead of filler, and needs no such setting.

`mint.yaml` and `merchant.yaml` are described under
[mintd](../design/core/tollgate-configuration.md#mintd) and
[merchantd](../design/core/tollgate-configuration.md#merchantd). Everything
not written here keeps its default.

### 3. Configure the customer

A merchant with no market, and the buying node from the section above:

```sh
cat > customer-merchant.yaml <<EOF
listen: "127.0.0.1:3350"
socket: "$WORK/customer-merchant.sock"
control: "$WORK/customer-merchant-control.sock"
market: false
wallet: { file: "$WORK/customer-wallet.sqlite", seed_file: "$WORK/customer-wallet.seed" }
EOF
cat > customer.yaml <<EOF
identity: { secret_key: "$CUSTOMER_SEC" }
merchant: { socket: "$WORK/customer-merchant.sock" }
network: { listen: "127.0.0.1:4757" }
buying:
  demand: 1000000
peers:
  "$GATEWAY_PUB":
    endpoint: "127.0.0.1:4747"
    no_charge: true
EOF
```

Each node listens for TollGate on `network.listen` and carries the loopback
filler on the next port up (4748 and 4758).

### 4. Start them

The mint and the merchants first, then the gateway, then the customer:

```sh
mintd -c gateway-mint.yaml > gateway-mint.log 2>&1 &
merchantd -c gateway-merchant.yaml > gateway-merchant.log 2>&1 &
merchantd -c customer-merchant.yaml > customer-merchant.log 2>&1 &
sleep 3
tollgated -c gateway.yaml --instance gateway > gateway.log 2>&1 &
sleep 1
tollgated -c customer.yaml --instance customer > customer.log 2>&1 &
```

`--instance` gives each `tollgated` a name, so the two do not share a control
socket (see [Named instances](#named-instances)).

### 5. What working looks like

Every log line starts with the instance's name. At start, a node says where
everything is:

```text
[gateway] INFO tollgated: starting instance=gateway runtime_dir=/var/folders/…/T/tollgate-gateway pubkey=0331cb3e…
[gateway] INFO tollgated: mint at url=http://127.0.0.1:3338 local=http://127.0.0.1:3338
[gateway] INFO tollgated: funding from merchant=/tmp/tollgate-try/gateway-merchant.sock
[gateway] INFO tollgated: loopback data plane listen=127.0.0.1:4748
[gateway] INFO tollgated: payers' budgets kept at budgets=/tmp/tollgate-budgets-gateway.json
[gateway] INFO tollgated: control socket control=/var/folders/…/T/tollgate-gateway/control.sock
[gateway] INFO tollgate_net::node: node listening pubkey=0331cb3e.. control=127.0.0.1:4747 identity=address
```

The customer logs the same lines, including a `mint at` line with the
default URL, `http://127.0.0.1:3338`. It runs no mint and never uses it: the
line only reports the default.

Then, once a second, a `link` line per peer. `shaped` is the rate this node
carries the peer at, `down` and `up` what it received from and sent to the
peer in the last second, in bytes. On the customer:

```text
[customer] INFO tollgated: wanting demand=1000000
[customer] INFO tollgated: link peer=0331cb3e.. shaped=18446744073709551615 demand=0 down=3877 up=2111045632
[customer] INFO tollgated: link peer=0331cb3e.. shaped=18446744073709551615 demand=1000000 down=4089 up=1710948352
[customer] INFO tollgated: link peer=0331cb3e.. shaped=18446744073709551615 demand=1000000 down=1124154 up=1954545664
[customer] INFO tollgated: link peer=0331cb3e.. shaped=18446744073709551615 demand=1000000 down=1250000 up=2085355520
[customer] INFO tollgated: link peer=0331cb3e.. shaped=18446744073709551615 demand=1000000 down=1251250 up=2167537664
```

Read it like this:

1. **Before paying, the customer gets the minimum flow**: about 4 KB/s
   `down`. During these seconds its merchant gets vouchers from the gateway's
   mint and the customer opens a payment channel with them. Its merchant
   logs it:

   ```text
   INFO tollgate_net::merchant: funded a channel mint=http://127.0.0.1:3338 unit=byte amount=1073741824
   ```

2. **Then it buys.** It wants 1 MB/s and reserves a quarter more, so `down`
   jumps to 1 250 000 bytes a second, exactly the rate it reserved.
3. **It keeps buying.** A little before its budget runs out it buys again,
   and adds back only what was drawn.

`shaped=18446744073709551615` is the largest 64-bit number: the customer
does not charge the gateway, so it does not limit it. The huge `up` is the
loopback filler described under step 2; it costs CPU, not money.

The gateway's side of the same seconds:

```text
[gateway] INFO tollgated: link peer=02f5cff3.. shaped=4096 demand=0 down=1889009664 up=4081
[gateway] INFO tollgated: link peer=02f5cff3.. shaped=1250000 demand=0 down=1780350976 up=999766
[gateway] INFO tollgated: link peer=02f5cff3.. shaped=1250000 demand=0 down=2289631232 up=1252500
[gateway] INFO tollgated: link peer=02f5cff3.. shaped=1250000 demand=0 down=1926676324 up=1247500
```

### 6. Watch it with tolltop

In another terminal, on the customer:

```sh
tolltop --instance customer
```

The table has one row per peer. Left of the divide is what the peer bought
from this node (nothing: the customer sells nothing), right is what this
node bought from the peer:

```text
┌ peers — left of the divide is what they bought from us, right is what we bought from them ──────────────────────────────────┐
│  peer     access  speed      budget     ends    reserved     in            bought     ends    reserved     want         w  out…│
│▶ 0331cb3e free    1717986918 empty      —       —            —             1.66 MiB   1s      1.19 MiB/s   976.6 KiB/s  0  db8d…│
└─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ tolltop — instance customer (1/2) ──────────────────────────────────────────────────────────────────────────────────────────┐
│02f5cff3  selling byte  up 31s  1 peer(s)   ↑↓ selects   enter opens   tab next instance (or i)   q quits                    │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Enter** opens the gateway's detail. The part that matters to a customer is
at the bottom:

```text
│what we bought from this peer                                        │
│terms               accepted                                         │
│budget, our count   7.28 MiB                                         │
│deadline            in 6s                                            │
│reserved rate       1.19 MiB/s                                       │
│their weight        0x on what we send them                          │
│their last Balance  12.06 MiB left, deadline in 10s, reserved 1.19   │
│MiB/s — said 3s ago                                                  │
│demand we observe   976.6 KiB/s                                      │
│channel we pay on   db8de454 4% — 43.56 MiB of 1.00 GiB signed       │
```

Some of what `tolltop` shows for a buying node is not right yet:

- the status line says `selling`, though the node sells nothing;
- the `speed` column shows `1717986918` and the detail `17179869184.00
  GiB/s` for the gateway: the "no limit" value printed as a number;
- the customer's session with the gateway stays in phase `establishing`
  (the gateway's view says `established`), because no channel is opened
  toward the customer. Buying is not affected.

**Tab** moves to the gateway (`tolltop --instance gateway`), whose row shows
what the customer bought, left of the divide:

```text
│  peer     access  speed      budget     ends    reserved     in            bought     ends    reserved     want         w  out…│
│▶ 02f5cff3 active  1.19 MiB/s 5.37 MiB   5s      1.19 MiB/s   db8de454 5%   empty      —       —            —            1  —   …│
```

The same numbers are on the control socket as JSON, for scripts. The
`read_snapshot` example reads it (see
[embedding.md](embedding.md#4-the-control-socket-and-the-snapshot)):

```text
$ cargo run -p tollgate-net --example read_snapshot -- --instance customer
node 02f5cff3b4bca87d  unit byte  mint http://127.0.0.1:3338  up 36s  (/var/folders/…/T/tollgate-customer/control.sock)
peer 0331cb3e2e69d180  establishing  access free
  sold:   budget 0 byte, 0s left, reserved 0/s, carried at 18446744073709551615/s
  bought: budget 6565000 byte, 5s left, reserved 1250000/s
  balance from the peer: 12641250 byte, 10s left, reserved 1250000/s (4s ago)
  moved:  70502514688 to the peer, 38263550 from it
```

`minttop --socket $WORK/gateway-mint.sock` and
`merchanttop --socket $WORK/gateway-merchant-control.sock` show the
gateway's mint and merchant.

### 7. Stop

```sh
pkill -f 'instance customer'; pkill -f 'instance gateway'; pkill mintd; pkill merchantd
```

A payer's budget is kept on disk (`budgets-<instance>.json` in the state
directory: the first of `/var/lib/tollgate`, `/usr/local/var/lib/tollgate`
and `$XDG_DATA_HOME/tollgate` it can use, else `/tmp`). Start the gateway
again before the deadline, and the customer gets back what it had left.

### The demo script

[`demo/run.sh`](../../demo/run.sh) runs two nodes that both sell, with
traffic that ramps up, and prints one line a second. It also runs a sat mint
in Docker, so it needs Docker, and it looks for the programs in
`./target/release`. See [demo/README.md](../../demo/README.md) for how to
read its output.

## Named instances

One machine can run `tollgated` several times, once per task: say one selling
internet access on the LAN, one selling FIPS peering, one selling a FIPS
exit. Each is an **instance** with a name, set by `instance:` in the file or
`--instance` on the command line (which wins). Names are letters, digits and
`-`. With neither, the name is `default`.

Each instance keeps its sockets in its own **runtime directory**,
`tollgate-<instance>`:

| Where | Runtime directory |
|---|---|
| Linux and OpenWrt services | `/run/tollgate-<instance>/` |
| The macOS package | `/usr/local/var/run/tollgate-<instance>/` |
| By hand, with `$XDG_RUNTIME_DIR` set | `$XDG_RUNTIME_DIR/tollgate-<instance>/` |
| Otherwise | `tollgate-<instance>` in the temporary directory |

`tollgated` creates the directory if it can. It holds `control.sock`, which
`tolltop` reads, and, for `enforcer.kind: external`, `enforcer.sock`, where
the enforcer listens.

`tolltop` with no flags finds every running instance and shows the first;
Tab moves between them. `--instance NAME` starts on one, `--socket PATH`
watches one socket only. A stale socket left by a node that was killed can
make `tolltop` and `read_snapshot` see an instance that is not running;
remove its directory.

Two instances on one machine also need their own `network.listen` ports and,
if both sell, their own `mintd`, `merchantd`, seeds and databases. Set the
`file` and `seed_file` paths explicitly on a machine that runs more than one.

## An external enforcer

With `enforcer.kind: external`, a separate program controls the traffic
and `tollgated` tells it who has paid for what
([tollgate-enforcer-protocol.md](../design/core/tollgate-enforcer-protocol.md)).
The repository has a **stub enforcer** that controls nothing and logs what it
is told; with `--carry`, it pretends each open payer used its full rate.

Using the gateway and customer above, stop the gateway's `tollgated` and add an
`enforcer` block to a copy of its config:

```sh
pkill -f 'instance gateway'
cp gateway.yaml gateway-ext.yaml
cat >> gateway-ext.yaml <<EOF
enforcer:
  kind: external
  identity: address
  socket: "$WORK/enforcer.sock"
EOF
```

`identity: address` says a payer is the address it connects from. `external`
has no default and must say it. Without `socket`, the enforcer socket is
`enforcer.sock` in the instance's runtime directory.

Start the stub, then the gateway:

```sh
cargo run -p tollgate-protocol --example stub_enforcer -- \
    --socket $WORK/enforcer.sock --identity address --unit byte --carry > stub.log 2>&1 &
tollgated -c gateway-ext.yaml --instance gateway > gateway-ext.log 2>&1 &
```

`tollgated` waits for the enforcer's `hello`, and sells nothing before it:

```text
[gateway] INFO tollgated: waiting for the enforcer to say hello socket=/tmp/tollgate-try/enforcer.sock
[gateway] INFO tollgate_net::enforcer::external: the enforcer said hello; selling identity=address unit=byte delegated=false
[gateway] INFO tollgated: enforcing through the external enforcer socket=/tmp/tollgate-try/enforcer.sock identity=address
```

The stub logs what it is told. The customer is bound by its address
(`127.0.0.1`, written as IPv6), starts closed (`rate: Some(0)`), gets the
minimum flow, then the rate it paid for:

```text
stub_enforcer: listening on /tmp/tollgate-try/enforcer.sock, Hello { version: 1, identity: Address, delegated: false, unit: "byte" }
stub_enforcer: <- Bind(Bind { peer: 03df9622.., bindings: [Binding { subject: 00000000000000000000ffff7f000001, delegated: false }] })
stub_enforcer: <- Set(Set { peer: 03df9622.., rate: Some(0) })
stub_enforcer: <- Set(Set { peer: 03df9622.., rate: Some(4096) })
stub_enforcer: <- Set(Set { peer: 03df9622.., rate: Some(1250000) })
```

Here the customer came back to the budget it had left: the gateway kept it on
disk across its restart.

**An enforcer built for something else stops the node.** Start the stub with
`--unit ml` instead, and `tollgated` refuses to start:

```text
Error: refusing to start behind the enforcer at /tmp/tollgate-try/enforcer.sock: mint.unit is "byte", and the enforcer counts in "ml"; every rate and count between them would be read in the wrong unit
```

To write a real enforcer, see
[embedding.md](embedding.md#2-an-external-enforcer).

## A Linux gateway with the `ip` enforcer

This is the node that sells internet access: the router forwards its
customers' traffic, nftables lets a paying customer through, and `tc` shapes
it to the rate it bought. It needs Linux, nftables, `tc` and the full `ip`
command (`ip-full` on OpenWrt), and it runs as root or with `CAP_NET_ADMIN`.
How it gates, shapes and counts is in
[peering-ip.md](../design/network-peering/peering-ip.md).

The OpenWrt package installs and configures all of this; see
[packaging/README.md](../../packaging/README.md). By hand, `tollgate.yaml`
differs from the gateway above in three places:

```yaml
identity: { secret_key: "…" }
mint:
  # Customers fund their channels against this, so use the address they reach.
  url: "http://192.168.1.1:3338"
merchant: { socket: "/run/merchantd.sock" }
access:
  minimum_flow: { enabled: true, bytes_per_second: 4096 }
enforcer:
  kind: ip
  interface: "br-lan"     # the interface that faces the customers
network:
  listen: "0.0.0.0:4747"  # customers must reach it
```

`mint.yaml` then listens where customers reach it (`public.listen:
"0.0.0.0:3338"`, the default) and keeps `url` equal to `mint.url` above.
Leave `auto_accept` on only while trying it out.

```sh
mintd -c /etc/tollgate/mint.yaml &
merchantd -c /etc/tollgate/merchant.yaml &
tollgated -c /etc/tollgate/tollgate.yaml --instance ip
```

Working looks like this at start:

```text
[ip] INFO tollgated: gating and shaping the kernel forwarding path interface=br-lan
```

and then like the gateway above: a `link` line per customer, `shaped` rising
from the minimum flow to what each one bought. A customer that has not paid
is held at the minimum flow, or dropped if it is 0; its traffic to the
router's own `tollgated`, `mintd` and `merchantd` is never gated, so it can
always pay.

This guide's run was on macOS, where the `ip` enforcer is not built. The
`testing/forwarding` Docker topology runs a gateway like this one and checks
that every forwarded byte is gated, shaped and counted, IPv6 included; its
[`gateway.yaml`](../../testing/forwarding/gateway.yaml) is a working
config.

## A FIPS node

On a [FIPS](https://github.com/jmcorgan/fips) mesh, `tollgated` sells transit:
the FIPS daemon forwards, and `tollgated` sets each neighbor's rate in it over
FIPS's control socket. The details are in
[peering-fips.md](../design/network-peering/peering-fips.md). It needs:

- `fipsd` running, built with per-peer transit policy
  (`set_transit_policy`); a FIPS daemon without it refuses the node at
  start.
- **The same secret key** for `tollgated` and the FIPS node. A peer's mesh
  address is derived from its key, and `tollgated` checks that every peer
  connects from the address of the key it announces.
- `tollgated` and the mint reachable on the mesh: listen on `[::]`, and
  advertise the mint at this node's `fips0` address.

`tollgate.yaml`:

```yaml
identity: { secret_key: "…" }          # the FIPS node's key
mint:
  url: "http://[fde5:b78f:c4ef:a89c:82cd:b05b:8ee7:ed56]:3338"   # this node's fips0 address
access:
  minimum_flow: { enabled: true, bytes_per_second: 4096 }
enforcer:
  kind: fips          # identity defaults to pubkey: FIPS proves each peer's key
network:
  listen: "[::]:4747"
```

and in `mint.yaml`, `public: { listen: "[::]:3338" }`. A buyer on the mesh
names this node by key and by its `fips0` address:

```yaml
peers:
  "02f383c9…":
    endpoint: "[fde5:b78f:c4ef:a89c:82cd:b05b:8ee7:ed56]:4747"
```

At start, the gateway says it reached FIPS:

```text
[fips] INFO tollgated: setting transit policy on the FIPS node socket=/run/fips/control.sock allowance=4096
```

**Buying a FIPS exit.** tollgate-suite-wrt has a FIPS exit: a router that
sells a way out of the mesh to the internet. Native clients, buying nodes on
the mesh, buy from it through its own `tollgated` instance,
`tollgate-fips-exit`, which runs behind `exitd` as an external enforcer. A
buying node points at that instance like any other gateway. **This path has
not been tested end to end yet.**

`enforcer.fips_socket` moves the FIPS control socket if it is not where FIPS
puts it by default. The `testing/fips` Docker topology runs three such nodes
(it needs a FIPS checkout; see [testing/README.md](../../testing/README.md));
its configs are working examples. This guide's run did not include it.

## When something is wrong

| You see | It means |
|---|---|
| `path must be shorter than SUN_LEN` | A socket path is too long. Use a shorter directory. |
| `could not fund a channel … reach merchantd` | `merchantd` is not running, or `merchant.socket` in `tollgate.yaml` differs from `socket` in `merchant.yaml`. `tollgated` keeps trying. |
| `could not fund a channel … Unknown Keyset` | A mint was reset (a new seed) while the `merchantd` wallet that mints there remembers the old one. When trying things out, start that wallet afresh; with real money, never delete a wallet file, since the file is the money. |
| `shaped=4096` and nothing more | The node has not bought. Check that it wants something (`--demand` or `buying.demand`) and that funding works. |
| `several instances are running` | `tolltop` or `read_snapshot` found more than one control socket. Pass `--instance`. |
| `refusing to start behind the enforcer` | The external enforcer's identity or unit differs from `enforcer.identity` or `mint.unit`. |
| `enforcer.identity is pubkey, but this node runs no FIPS` | `pubkey` needs a network that proves keys. Use `address`, or run FIPS. |

For more detail in the logs, set `RUST_LOG=tollgate_net=debug,info`.

## Where next

- [embedding.md](embedding.md): use TollGate from your own program.
- [tollgate-configuration.md](../design/core/tollgate-configuration.md):
  every key in the three files.
- [tollgate-intro.md](../design/core/tollgate-intro.md): how TollGate works.
