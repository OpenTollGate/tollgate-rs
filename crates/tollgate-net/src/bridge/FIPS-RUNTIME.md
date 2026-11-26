# The FIPS transit-policy API does not exist in any build I can find

This file records a documentation-versus-code contradiction that blocks the FIPS half
of the paid exit. It replaces an earlier note that said only "this host's FIPS is too
old". That was wrong, and being wrong in that direction matters: it implied a version
bump would fix it.

## What `tollgate-rs` says exists

`docs/design/FIPS_FEATURE_REQUESTS.md` states:

> **Status**: available in the FIPS build TollGate targets, as the transit-policy
> commands: `set_transit_policy {npub, admitted, rate_bytes_per_sec}` per peer,
> `set_default_transit_policy` for peers not yet named, `clear_transit_policy` on
> removal, and `show_transit_policy` for the per-peer counters.

and `docs/guides/quickstart.md` repeats the consequence:

> `fipsd` running, built with per-peer transit policy (`set_transit_policy`); a FIPS
> daemon without it refuses the node at start.

`crates/tollgate-net/src/enforcer/fips.rs` implements exactly that: it sends
`set_transit_policy {npub, admitted, rate_bytes_per_sec}` and reads counters from
`show_transit_policy`.

## What the code actually contains

Four places checked, all negative.

**0. The fallback does not exist either.** Before concluding, the obvious retreat was
tried: if there is no rate, could the bridge at least use the boolean
`ForwardingPolicy` that the fork does have — admit a paying peer, drop an unpaid one,
and bill nothing? On this host's daemon:

```
set_peer_policy -> {"status":"error","message":"unknown command: set_peer_policy"}
show_peers      -> ok, and its per-peer objects carry authenticated_at_ms,
                   connectivity, mmp{...}, ipv6_addr, node_addr, link_id —
                   and no forwarding_policy field at all
```

So this build has **no per-peer admission surface of any kind**: not a rate, not a
boolean, not even a readable policy. A bridge cannot admit or refuse a peer through it,
only observe that one connected.

**1. This host's daemon** — `fips 0.5.3`, `fips0` up at MTU 1280, control socket
reachable:

```
set_transit_policy  -> {"status":"error","message":"unknown command: set_transit_policy"}
show_transit_policy -> {"status":"error","message":"unknown command: show_transit_policy"}
show_peers          -> {"status":"ok", ...}
show_links          -> {"status":"ok", ...}
```

**2. Upstream `jmcorgan/fips`, master** — no occurrence of `transit_policy` anywhere in
the tree. The control socket dispatches `connect`, `disconnect` and status queries;
the gateway module adds `show_gateway` and `show_mappings`. There is no transit-policy
command and no per-peer forwarding-rate concept.

**3. `Amperstrand/fips`, branch `archive/feat-tollgate-peer-policy`** — the only
TollGate-specific FIPS fork, and the closest thing to "the FIPS build TollGate
targets". It does have a peer policy, under a different name and with a different
shape:

```rust
"set_peer_policy" => set_peer_policy(node, params),
...
let policy = match policy_str {
    "full" => ForwardingPolicy::Full,
    "local_only" => ForwardingPolicy::LocalOnly,
    _ => return Response::error(...expected 'full' or 'local_only'),
};
```

That is a **two-valued enum: forwarded or not**. It carries no rate, no byte budget,
no per-peer counter reading, and there is no `show_transit_policy` on that branch
either. `rate_bytes_per_sec` appears only in the BLE transport's token bucket, which
is unrelated.

## What follows

- **A paid FIPS exit needs per-peer admission AND a rate**, because what is being sold
  is shaped transit. `set_peer_policy`'s two values can express "admitted" but not "at
  what speed", so it cannot bill or meter. The missing piece is not a version bump; it
  is the shaping half of the feature.
- **The `fips` enforcer kind in `tollgate-rs` cannot work against any FIPS I can
  find**, and would refuse to start, as the quickstart says it should.
- **`docs/design/FIPS_FEATURE_REQUESTS.md`'s "Status: available" is inaccurate as of
  this writing.** That is a documentation defect in a design document other code plans
  against. It should be corrected, or the feature built.

## What this does not change

The bridge's own logic is unaffected in principle and unproven in practice for FIPS.
`admit_fips` drives the `Enforcer` trait, and that trait's FIPS backend is the part
with no working counterpart. The address-based path needs no transit policy, so the
fixture-daemon evidence stays meaningful for the protocol and silent about FIPS.

## How to close it

Either:

1. build a FIPS carrying `set_transit_policy` with a real rate, then re-run the proof;
   or
2. accept `set_peer_policy` as the available primitive and shape elsewhere — which for
   transit is the problem the design document opens with: *"Transit never traverses the
   TUN… the enforcement point has to be the forwarding path itself."*

Option 2 is not a workaround this branch can perform. It is a decision about where
enforcement lives, and it belongs to whoever owns the FIPS build.
