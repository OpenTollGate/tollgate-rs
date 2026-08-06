# Packaging

Installable packages for the two places a node actually runs: a router that
sells transit, and a Mac that develops against one. Everything lands in
`deploy/`.

```sh
make -C packaging ipk              # OpenWrt .ipk, aarch64
make -C packaging ipk ARCH=x86_64  # ...or another architecture
make -C packaging pkg              # macOS .pkg
make -C packaging clean
```

## What gets installed

| | OpenWrt | macOS |
|---|---|---|
| Binaries | `/usr/bin/tollgated`, `/usr/bin/tolltop` | `/usr/local/bin/…` |
| Config | `/etc/tollgate/tollgate.yaml` | `/usr/local/etc/tollgate/tollgate.yaml` |
| Service | procd, `/etc/init.d/tollgate` | launchd, `com.tollgate.daemon` |
| Control socket | `/run/tollgate.sock` | `/usr/local/var/run/tollgate.sock` |
| Forwarding mode | `nftables` — the real thing | `loopback` — a socket of its own |

`tolltop` finds the socket without being told where it is, on both.

## The identity is generated once, at install

A node that generated a fresh key on every start would be a different node on
every start: its peers would not recognise it, and the vouchers they hold would
be claims on an issuer that no longer exists. So both packages generate one
during installation and write it into the config, which is then `0600` and
marked to survive an upgrade — a conffile under opkg, and copied only when
absent on macOS. `lib/upgrade/keep.d/tollgate` carries it across a sysupgrade.

Losing that file is not a reinstall. It is a new node that owes nothing to
anyone holding the old one's paper.

## OpenWrt

Cross-compiled with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)
rather than the OpenWrt SDK, and the `.ipk` is assembled directly — it is a
gzipped tar of three members, so nothing needs an SDK to make one.

```sh
cargo install cargo-zigbuild        # plus zig itself
make -C packaging ipk
scp -O deploy/tollgate_*.ipk root@192.168.1.1:/tmp/
ssh root@192.168.1.1 opkg install /tmp/tollgate_*.ipk
```

About 10 MB packaged, 22 MB installed — most of it the mint. Fine on anything
with a spare 32 MB of flash; too big for an 8 MB router without trimming.

Dependencies are what the adapter actually uses: `nftables` for the gate and
the counters, `tc-full` for the shaper, `kmod-sched-core` for the HTB class it
installs, and `kmod-nf-conntrack` because the forward chain matches on
established connections.

First boot generates the identity, rewrites the mint URL to the router's LAN
address — a peer funds its channel against that mint, so `127.0.0.1` would be
useless to it — opens 4747 and 3338 on the lan zone, and turns on forwarding.

The service starts at `START=96`, after the firewall. fw4 flushing on a later
start would take the adapter's own table and classes with it.

## macOS

```sh
make -C packaging pkg
sudo installer -pkg deploy/tollgate-*.pkg -target /
tolltop
sudo packaging/macos/uninstall.sh          # --purge to drop the identity too
```

The forwarding mode is `loopback`, because macOS has neither nftables nor tc
and the kernel forwarding path cannot be gated the way it is on a router. The
protocol, the payments and the shaping are all real; the traffic is the node's
own rather than somebody else's. That makes it right for developing against,
for watching with `tolltop`, and for a node that buys transit rather than
selling it — and wrong for actually selling a Mac's uplink.

## What is not packaged yet

No Debian, no Arch, no Windows. `tollgated` is an ordinary static binary on
Linux, so a systemd unit is a small addition when something needs one.
