# Why the FIPS path cannot be run on this host yet

Recorded because the bridge's FIPS admission is written, tested against the
enforcer protocol, and **cannot be exercised here** — and a future reader
deserves to know that from evidence rather than discovering it by trying.

## What the code requires

`tollgate-net`'s `fips` enforcer drives a mesh through two control commands:

- `set_transit_policy {npub, admitted, rate_bytes_per_sec}` — admission and rate
  for one peer, in one call;
- `show_transit_policy` — the per-peer counters.

`docs/guides/quickstart.md` states the consequence directly:

> `fipsd` running, built with per-peer transit policy (`set_transit_policy`); a
> FIPS daemon without it refuses the node at start.

## What this host has

```
$ /usr/bin/fips --version
fips 0.5.3 (rev f4a5b4a8e6)
$ ip link show fips0
417: fips0: <POINTOPOINT,MULTICAST,NOARP,UP,LOWER_UP> mtu 1280 ...
$ ls -l /run/fips/control.sock
srwxrwx--- 1 root fips 0 ... /run/fips/control.sock
```

A FIPS daemon is running, `fips0` is up, and the control socket is reachable by a
member of the `fips` group. The mesh exists.

Probing its control socket for the commands above:

```
show_transit_policy -> {"status":"error","message":"unknown command: show_transit_policy"}
set_transit_policy  -> {"status":"error","message":"unknown command: set_transit_policy"}
show_peers          -> {"status":"ok","data":{"peers":[...]}}
show_links          -> {"status":"ok","data":{"links":[...]}}
```

So this build answers `show_peers` and `show_links` and does **not** implement the
transit-policy pair. It is a FIPS that forwards by whatever policy it was built
with, not one that lets a node set per-peer admission at runtime.

## What follows

- **The FIPS admission path in this branch is unproven on a live mesh**, and no
  amount of local testing changes that. The trait path and the protocol encoding
  are proven (see `crates/tollgate-net/tests/bridge_enforcer_live.rs` and the
  fixture daemon); whether FIPS honours the resulting policy is not.
- **`tollgated` with `enforcer.kind: fips` would refuse to start against this
  daemon**, by the behaviour the quickstart describes. That is the correct
  behaviour and not a bug to work around.
- **The address-based path is unaffected.** `admit_enforcer` and the loopback
  enforcer do not need transit policy, which is why the fixture daemon run is
  meaningful evidence for the protocol and none at all for FIPS.

## What would close it

A FIPS build carrying `set_transit_policy`. The feature is documented as available
in the build TollGate targets (`docs/design/FIPS_FEATURE_REQUESTS.md`), so this is a
version or build-flag question, not a missing feature in the design — but it must be
answered by installing such a build and re-running, not by assuming.
