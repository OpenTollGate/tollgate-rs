# Quickstart: run TollGate

This guide runs TollGate nodes and shows what a working node looks like. It
starts with two nodes on one machine, which needs nothing but the build, and
then covers a real gateway: one on Linux with the `ip` enforcer, one on a FIPS
mesh, and one behind an external enforcer.

To get and build TollGate first, see [getting-tollgate.md](getting-tollgate.md).
For every configuration key, see
[tollgate-configuration.md](../design/core/tollgate-configuration.md); this
guide uses only a few.

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

## Two nodes on one machine

Two nodes: a **gateway** that sells, and a **client** that buys from it. Both
use the `loopback` enforcer, so the bytes bought and sold are filler on a
local TCP connection, but the payments, the channels and the shaping are
real. Each node runs its own `mintd` and `merchantd`.

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
tollgated --show-identity > client.id
GATEWAY_PUB=$(awk '/pubkey/ {print $2}' gateway.id)
GATEWAY_SEC=$(awk '/secret_key/ {print $2}' gateway.id)
CLIENT_SEC=$(awk '/secret_key/ {print $2}' client.id)
```

```text
$ cat gateway.id
pubkey:     0353d4fac76a992b04b492c25ba81abd143424f2a71b80fe4696371d7eb358d0
secret_key: …
```

### 2. Configure the mints and merchants

Each node's mint and merchant need their own ports and files. The gateway's
mint is on 3338 for peers and 3337 for its merchant; the client's on 3348 and
3347:

```sh
for node in gateway:3338:3337:3340 client:3348:3347:3350; do
  IFS=: read name public private market <<< "$node"
  cat > $name-mint.yaml <<EOF
url: "http://127.0.0.1:$public"
seed_file: "$WORK/$name-mint.seed"
file: "$WORK/$name-mint.sqlite"
public: { listen: "127.0.0.1:$public" }
private: { listen: "127.0.0.1:$private" }
control_socket: "$WORK/$name-mint.sock"
EOF
  cat > $name-merchant.yaml <<EOF
listen: "127.0.0.1:$market"
socket: "$WORK/$name-merchant.sock"
control: "$WORK/$name-merchant-control.sock"
mint: { url: "http://127.0.0.1:$public", private: "http://127.0.0.1:$private" }
wallet: { file: "$WORK/$name-wallet.sqlite", seed_file: "$WORK/$name-wallet.seed" }
EOF
done
```

`mint.yaml` and `merchant.yaml` are described under
[mintd](../design/core/tollgate-configuration.md#mintd) and
[merchantd](../design/core/tollgate-configuration.md#merchantd). Everything
not written here keeps its default.

### 3. Configure the two nodes

The gateway sells. It names its mint and its merchant, and gives every peer a
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

`from_payer_weight: 0` makes what the client sends free, so only what it
downloads is drawn from its budget. The loopback enforcer sends filler both
ways, and this keeps the numbers easy to read.

The client buys. It names the gateway by its public key and says where to
reach it:

```sh
cat > client.yaml <<EOF
identity: { secret_key: "$CLIENT_SEC" }
mint: { url: "http://127.0.0.1:3348" }
merchant: { socket: "$WORK/client-merchant.sock" }
access:
  minimum_flow: { enabled: true, bytes_per_second: 4096 }
network: { listen: "127.0.0.1:4757" }
peers:
  "$GATEWAY_PUB":
    endpoint: "127.0.0.1:4747"
EOF
```

Each node listens for TollGate on `network.listen` and carries the loopback
filler on the next port up (4748 and 4758).

### 4. Start them

The mints and merchants first, then the gateway, then the client.
`--demand 1000000` tells the client to want 1 MB/s from its peers, which is
what makes it buy:

```sh
for name in gateway client; do
  mintd -c $name-mint.yaml > $name-mint.log 2>&1 &
  merchantd -c $name-merchant.yaml > $name-merchant.log 2>&1 &
done
sleep 3
tollgated -c gateway.yaml --instance gateway > gateway.log 2>&1 &
sleep 1
tollgated -c client.yaml --instance client --demand 1000000 > client.log 2>&1 &
```

`--instance` gives each `tollgated` a name, so the two do not share a control
socket (see [Named instances](#named-instances)).

### 5. What working looks like

Every log line starts with the instance's name. At start, the gateway says
where everything is:

```text
[gateway] INFO tollgated: starting instance=gateway runtime_dir=/var/folders/…/T/tollgate-gateway pubkey=0353d4fa…
[gateway] INFO tollgated: mint at url=http://127.0.0.1:3338 local=http://127.0.0.1:3338
[gateway] INFO tollgated: funding from merchant=/tmp/tollgate-try/gateway-merchant.sock
[gateway] INFO tollgated: loopback data plane listen=127.0.0.1:4748
[gateway] INFO tollgated: payers' budgets kept at budgets=/tmp/tollgate-budgets-gateway.json
[gateway] INFO tollgated: control socket control=/var/folders/…/T/tollgate-gateway/control.sock
[gateway] INFO tollgate_net::node: node listening pubkey=0353d4fa.. control=127.0.0.1:4747 identity=address
```

Then, once a second, a `link` line per peer. `shaped` is the rate this node
carries the peer at, `down` and `up` what it received from and sent to the
peer in the last second, in bytes. On the client:

```text
[client] INFO tollgated: link peer=0353d4fa.. shaped=4096 demand=0 down=3846 up=3988
[client] INFO tollgated: link peer=0353d4fa.. shaped=5100 demand=1000000 down=4080 up=5129
[client] INFO tollgated: link peer=0353d4fa.. shaped=1531862 demand=1000000 down=813928 up=311621
[client] INFO tollgated: link peer=0353d4fa.. shaped=1531862 demand=1000000 down=1250000 up=1531856
[client] INFO tollgated: link peer=0353d4fa.. shaped=1531862 demand=1000000 down=1250000 up=1530323
```

Read it like this:

1. **Before paying, the client gets the minimum flow**: about 4 KB/s down.
   During these seconds the client's merchant mints vouchers at the gateway's
   mint, and the client opens a payment channel with them.
2. **Then it buys.** It wants 1 MB/s and reserves a quarter more
   (`buying.headroom_pct`, 125 by default), so `down` jumps to 1 250 000 bytes
   a second, exactly the rate it reserved. The gateway shapes it to that.
3. **It keeps buying.** A little before its budget runs out, it buys again
   and adds back what was drawn. Nothing already paid for is lost.

The gateway's side of the same seconds:

```text
[gateway] INFO tollgated: link peer=03df9622.. shaped=4096 demand=0 down=5118 up=4076
[gateway] INFO tollgated: link peer=03df9622.. shaped=1250000 demand=0 down=235288 up=752882
[gateway] INFO tollgated: link peer=03df9622.. shaped=1250000 demand=0 down=1530325 up=1248750
```

Both nodes sell here, so the gateway also buys from the client: on the
loopback enforcer, what a node receives counts as something it wants. That is
the client's `shaped` above, the rate the client carries the gateway at. A
node that should only buy says `no_charge: true` on each peer it buys from.

`merchantd`'s log shows each channel it funds:

```text
INFO tollgate_net::merchant: funded a channel mint=http://127.0.0.1:3338 unit=byte amount=1073741824
```

and the gateway's log shows a settled channel's vouchers being destroyed at
its own mint, once delivered:

```text
[gateway] INFO tollgate_net::channel::spilman: burned what a channel paid channel="9b6bbb6c…" mint=http://127.0.0.1:3338 amount=1073741824
```

### 6. Watch it with tolltop

In another terminal:

```sh
tolltop --instance client
```

The table has one row per peer. Left of the divide is what the peer bought
from this node, right is what this node bought from the peer:

```text
┌ peers — left of the divide is what they bought from us, right is what we bought from them ──────────────────────────────────┐
│  peer     access  speed      budget     ends    reserved     in            bought     ends    reserved     want         w  out…│
│▶ 03df9622 active  1.19 MiB/s 9.57 MiB   8s      1.19 MiB/s   d3e6b82e 7%   14.40 MiB  9s      1.52 MiB/s   —            1  b2b0…│
└─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ tolltop — instance gateway (2/2) ───────────────────────────────────────────────────────────────────────────────────────────┐
│0353d4fa  selling byte  up 1m00s  1 peer(s)   ↑↓ selects   enter opens   tab next instance (or i)   q quits                  │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

(Above is the gateway's view, `--instance gateway`.) **Tab** (or `i`) moves
to the next running instance. **Enter** opens a peer's detail, here the
client's view of the gateway:

```text
┌ peer 0353d4fa ───────────────────────────────────────────┐
│access              active   phase established            │
│                                                          │
│what this peer bought from us                             │
│budget              5.55 MiB                              │
│deadline            in 7s                                 │
│reserved rate       1.48 MiB/s                            │
│from-payer weight   1x on what they send us               │
│speed we allow      1.48 MiB/s                            │
│authorized          141.96 MiB                            │
│consumed            136.41 MiB                            │
│channels on                                               │
│  b2b05617 13% — 141.96 MiB of 1.00 GiB signed            │
│                                                          │
│what we bought from this peer                             │
│terms               accepted                              │
│budget, our count   7.65 MiB                              │
```

`minttop --socket $WORK/gateway-mint.sock` and
`merchanttop --socket $WORK/gateway-merchant-control.sock` show the mint and
the merchant.

The same numbers are on the control socket as JSON, for scripts. The
`read_snapshot` example reads it (see
[embedding.md](embedding.md#4-the-control-socket-and-the-snapshot)):

```text
$ cargo run -p tollgate-net --example read_snapshot -- --instance gateway
node 0353d4fac76a992b  unit byte  mint http://127.0.0.1:3338  up 41s  (/var/folders/…/T/tollgate-gateway/control.sock)
peer 03df962210869faa  established  access active
  sold:   budget 11267500 byte, 9s left, reserved 1250000/s, carried at 1250000/s
  bought: budget 13004151 byte, 7s left, reserved 2083332/s
  balance from the peer: 20076959 byte, 10s left, reserved 2083332/s (2s ago)
  moved:  44503053 to the peer, 64602482 from it
```

### 7. Stop

```sh
kill %1 %2 %3 %4 %5 %6      # or: pkill tollgated; pkill mintd; pkill merchantd
```

A payer's budget is kept on disk (`budgets-<instance>.json`, in the state
directory: the first of `/var/lib/tollgate`, `/usr/local/var/lib/tollgate`
and `$XDG_DATA_HOME/tollgate` it can use, else `/tmp`). Start the gateway again before the deadline, and the
client gets back what it had left.

### The demo script

[`demo/run.sh`](../../demo/run.sh) runs the same two nodes with traffic that
ramps up, and prints one line a second. It also runs a sat mint in Docker, so
it needs Docker, and it looks for the programs in `./target/release`. See
[demo/README.md](../../demo/README.md) for how to read its output.

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

Using the two nodes above, stop the gateway's `tollgated` and add an
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

The stub logs what it is told. The client is bound by its address
(`127.0.0.1`, written as IPv6), starts closed (`rate: Some(0)`), gets the
minimum flow, then the rate it paid for:

```text
stub_enforcer: listening on /tmp/tollgate-try/enforcer.sock, Hello { version: 1, identity: Address, delegated: false, unit: "byte" }
stub_enforcer: <- Bind(Bind { peer: 03df9622.., bindings: [Binding { subject: 00000000000000000000ffff7f000001, delegated: false }] })
stub_enforcer: <- Set(Set { peer: 03df9622.., rate: Some(0) })
stub_enforcer: <- Set(Set { peer: 03df9622.., rate: Some(4096) })
stub_enforcer: <- Set(Set { peer: 03df9622.., rate: Some(1250000) })
```

Here the client came back to the budget it had left: the gateway kept it on
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
