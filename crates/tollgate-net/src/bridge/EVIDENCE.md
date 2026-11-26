# The v1→v2 bridge against real daemons

What was run, on 2026-11-26, and what it shows. Every value below is copied from
a live run on loopback, not from a mock.

## The stack

- `mintd` — a real Cashu mint, CDK-backed, keysets served at `127.0.0.1:3338`
  (public) and `127.0.0.1:3339` (private, quotes paid on creation).
  Started with `auto_accept: true`, which the daemon itself warns about:
  *"the public listener gives vouchers to anyone who asks, so service is free"*.
  That warning is the distinction the code documents: the **private** listener is
  where `merchantd` prints vouchers, and it belongs on loopback.
- `merchantd` — holds the wallet, serves `funding.sock` (for `tollgated`:
  `fund` and `deposit` only) and `merchant-control.sock` (for the operator).
- The bridge (`bridge::BridgeFunding`) — not involved in these two calls; it is
  the caller that will make them.

## Funding a channel (what a v2 session asks for)

```
{"command":"fund","params":{"mint":"http://127.0.0.1:3338","unit":"byte","amount":5000}}
```

answered:

```
{"status":"ok","data":{"token":"cashuBo2FtdWh0dHA6Ly8xMjcuMC4wLjE6MzMzOGF1ZGJ5dGVhdIGiYWlIAfvk0AGNCCFhcJgl..."}}
```

`merchantd`'s log shows it going through CDK's own send saga:

```
prepare_send{amount=Amount { value: 5000 ... }}: cdk::wallet::send::saga: Preparing send for 5000
confirm_send{...}: cdk::wallet::send::saga: Confirming prepared send
tollgate_net::merchant: funded a channel mint=http://127.0.0.1:3338 unit=byte amount=5000
```

So a real 5000-byte voucher token came out of a real mint, fee 0.

## Depositing a payment (what a v1 buyer produces)

The same token, handed back:

```
{"command":"deposit","params":{"token":"cashuBo2FtdWh0dHA6Ly8xMjcuMC4wLjE6MzMzOGF1ZGJ5dGVhdIGiYWlIAfvk0AGNCCFhcJgl..."}}
```

answered:

```
{"status":"ok","data":{"amount":3000,"kind":"prepaid_transit","mint":"http://127.0.0.1:3338","unit":"byte"}}
```

and `merchantd` logged `deposited mint=http://127.0.0.1:3338 unit=byte amount=3000`.

**This is the answer the bridge's `deposit_held` reads**, and the field names and
types match exactly — `mint`, `unit`, `amount`. That is why the bridge credits a
buyer at what the mint reports rather than at the token's advertised value.

## What this proves, and what it does not

Proven: the two calls the bridge depends on both work against real daemons, in
the direction the bridge uses them, with the answer shape the bridge parses. A v1
buyer's token can be deposited and its real value reported back; a v2 channel can
be funded with real vouchers.

Not proven, and it is the remaining work:

1. **The v1 HTTP surface does not exist yet.** Nothing calls `accept_payment` — a
   v1 buyer does not yet reach this stack at all.
2. **No v2 session was run against these daemons** with `BridgeFunding` attached,
   so "a deposited token funds a live channel and the session runs on it" is
   still a composition that has been tested in pieces, not end to end.
3. **`auto_accept: true` makes this mint free.** That was deliberate for the run
   — it isolates the funding path from a payment path — but it means no money
   changed hands here, and it must not be mistaken for a paid deployment.

## Reproducing

```sh
cd /tmp && rm -rf v1v2 && mkdir v1v2
# mint.yaml / merchant.yaml as used above; then
mintd -c /tmp/v1v2/mint.yaml
merchantd -c /tmp/v1v2/merchant.yaml
printf '{"command":"fund","params":{"mint":"http://127.0.0.1:3338","unit":"byte","amount":5000}}\n' \
  | socat - UNIX-CONNECT:/tmp/v1v2/funding.sock
```

The request JSON is externally tagged: `{"command":"fund","params":{...}}`. A
plain `{"Fund":{...}}` is refused with *"missing field `command`"*.
