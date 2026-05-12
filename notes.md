pricing: I like the idea of a per-peer, per-product, per-mint price
sheet without negotiation. Especially the fact that we can have both
positive and/or negative fees. If there is agreement between peers,
the traffic flows. If there is no agreement, the traffic doesn't
flow. This is truly the TollGate principles applied to a mesh network
rather than a gateway based system.


Bootstrap: yes this is the way I told @SatsAndSports we would have to
bootstrap prior to having a flat mesh. I love that we can still keep
the principle of having to pay up front before receiving the transport.

Streaming payment: every 5 seconds --> it makes sense that we can't
make this a configurable / dynamic value, since the time windows need
to start and end at the same time on all peers. Can peers see what the
price of the next time window will be? What do we do if we are selling
megabytes/data? The period that we choose here will determine how
quickly we can change our price if we are running at a loss and our
peers are exploiting our cheap transport. Perhaps choosing the "right"
time window length is an unsolvable problem, a bit like choosing the
right block size. Are we using unix time to demarcate the start and
end of such periods? Do we have/need a guard time between the time
windows to account for clock drift and/or message propagation time
between peers?

Npub instead of MAC address: to what extent does TollGate-V2 depend on
FIPS? Can it work with legacy IPV4/IPV6, or does it strictly require
FIPS to work?

Data model: do these CBOR encoded messages still use the nostr format
internally?

Interface: Do devices need to run both tollgate V1 and TollGate V2 if
the TollGate operator wants a user facing captive portal or will there
be an addon that lets TollGate V2 implementations interact with
customers via a captive portal?

No identity beyond pubkey: How frequently can we rotate our pubkeys?
Keep in mind that our spilman channels can dox the new pubkey if they
remain open when we rotate pubkeys. Maybe we could consider rotating
the pubkey that we expose to a peer whenever we establish a new set of
spilman channels to that peer?

Price discovery without negotiation: to what extent can we see the
price before committing to a certain peer? I think @Arjen mentioned
that its possible to connect to multiple peers at once with WiFi -
something like WiFi direct but for OpenWRT routers. Having the ability
to see what the price of the competition is before committing to the
competition is really important, because it allows for price
discovery..

Routing metrics: to what extent do we want to/can we verify that the
transport we paid for actually got delivered?


Channel cap: Why cap the channel capacity at 10,000 SATs? If we have
more than 10,000 SATs with a certain mint, we can also make channels
that are larger than 10,000 SATs, because our counterparty doesn't
have the ability to rug us. I see how having a cap could be useful for
privacy if we want to rotate our npubs at a regular basis, but in that
case we might want t close the channel and open a new one at the time
of rotating npubs. Or we might want to have a field in a config file
where the user decides the max channel capacity and the npub is
rotated upon hitting the rollover threshhold.


Netting & balanced traffic: even if the traffic is only one
directional, we could have a different channel capacity for the
inbound channel and for the outbound channel, so we souldn't have
unnecessary channel rollovers on the outbound channel if all the
traffic is inbound.


Initial capacity: how about size relative to product pricing?

Wash trading risk: when making routing decisions we need to be careful
to avoid using node throughput as an input to our decision making
process, because TollGates that relay traffic could game this metric
by temporarily subsidizing traffic.

FIPS questions:

* can the FIPS config override routing policy decisions that it gets
  form TollGate? Can we have a config mode where it doesn't override
  any policy decisions from TollGate?

* FIPS computes link_cost internally. Will that information be passed
  up to TollGate? How will we expose this information to the logic
  that makes policy decisions in TollGate? How will we plug in the
  TollGate operator's business model?

* Do TollGates have economic information about their peers before
  establishing channels?

* In multi hop routing: if a relay accepts a packet at a certain
  price, but then finds that this packet is costing it more to
  transport than it received from the customer, it has the choice to
  drop the packet at the cost of getting a bad reputation or to up its
  price for the next packet in order to avoid making a bad
  reputation. Is this correct?

* What if this relay has two customers (Alice and Bob) who are both
  sending outboudn traffic through the same edge (Hop_Next) of the
  relay. Alice's traffic poses a high cost to the overall network,
  while Bob's traffic poses a low overall cost to the network. If we
  don't distinguish between traffic from Alice and Bob on that edge,
  the relay that transports that edge would raise prices due to Bob's
  traffic, thus negatively impacting the cost of Alice's traffic. Does
  this mean Alice needs to maintain a persistant identity for some
  time if Alice wants to get a lower price than Bob gets for her
  traffic? How would we distinguish between Alice and Bob's traffic?
  Could bloom filters be useful here?


The problem:

Alice and Bob are both buying from Carrol
Both are sending traffic through carrol to Dave (Hop_Next)
Bob's traffic is taking a slow LoRa connection from Dave to a server
Alice's traffic is taking a fast Fiber connection from Dave to a different server

Dave might want to raise the price per megabyte that he presents to
carrol because of the high cost of sending Bob's limited (1 Byte per
minute) traffic over LoRa, but that would negatively impact Alice's
ability to pay for and fully saturate the capacity of the fast
connection between Carrol and Dave with Alice's traffic.


Bug or a feature?

* Feature: market signal that Hop_Next is congested

Someone else (say Erric) can step in and connect Carrol to the server
that Alice wants to reach, thus providing an alternative to the
overpriced route from Carrol to the server via Dave.

* Bug: Traffic classes pricing based on destination

What if the link between Carrol and Dave is a fiber link and the cost
for Eric to spin up compettion is prohibitively high?

Solution we came up with:

* What if Erric and Dave are just two instances of TollGate running on
  the same server? Carrol buys fast+cheap access to Alice's
  destination from Erric for routing Alice's fast traffic and Carrol
  buys slow + expensive access to Bob's destination from Dave for
  routing via the LoRa interface. Now both Alice and Bob can use the
  link between Carrol and Erric/Dave without negatively impacting
  eachother's ability to saturate the link between Carrol and
  Erric/Dave.


Open questions: do we have one TollGate instance per interface? At
which point does it make sense to split off from a single TollGate
service to two separate TollGate services that run on the same device?

What happens if we have multiple hops in the physical mesh and
multiple routers in the path between client and server have parralel
TollGate services like Erric and Dave?

* Note that Carrol doesn't need to expose a price per destination to
  Alice and Bob, just a price per traffic class that Carrol can access
  via Erric/Dave. This is because from Carrol's perspective Alice and
  Bob don't have any overlap in their paths, even though they are
  sharing the same physical link between Carrol and the router that
  hosts Erric / Dave. Hence, we don't need to worry about Carrol
  needing to expose and exponentially growing number of prices to
  Alice and Bob if the number of hops between Alice/Bob and their
  respective servers increases. Carrol makes two independent TollGate
  connections to the router that hosts Erric/Dave. One connection is
  to Erric and the other is to Dave. The price for traffic that flows
  via Erric is independent from the price that flows via Dave.

Note: we need a diagram for this..

Note: each TollGate instance will probably also have its own FIPS
instance.

* Payment aware routing: do you mean traffic shaping / packet
  prioritization or do you mean routing policy being passed from the
  TollGate to FIPS?

* ChannelSync isn't trustless right? Even if the node that lost its
  state signs messages about its current state, the node that helps it
  recover could give it an older message which is outdated? Am I
  missing something? Does peer A know at what time it went offline? If
  so, it could probably detect a really old channel state if B gives
  it a really old message that A signed. Are we doing this in order to
  avoid frequently persisting information to the file system which
  could increase wear and tear? We might want to consider some sort of
  persisted message once the cost of persisting a message is lower
  than the cost of losing the channel state. Even if its just a time
  stamp that we persist..

* 






-----------------------

Note on selling megabytes or minutes: I think I understand @Arjen's
concern about this now. In a multi hop scenario, the reseller of that
one megabyte would have zero margin if they want to actually deliver
one megabyte for each 1MB ecash note. I was thinking primarily in
terms of the 1st hop, because I was concerned about the lack of
initial adoption - which is a challenge we currently face. In a two
hop setting, its best to use SATs and it will quickly become apparent
that money is suprerior to megabyte shitcoins. Perhaps it will help
people to communicate what they are selling if we call it an `Alice
Megabyte` or `Bob Megabyte`. If Alice is reselling an Bob Megabyte, he
might deliver 0.9 actual megabytes for each Bob megabyte that he is
paid, so that he can still take a cut...

Is this the concern you were trying to convey?

-----------------------



(base) c03rad0r@CobradorWave:~/tollgate-rs$ opencode -s ses_1f97a0180ffezshdIwRHlWac2X
                                   ▄     
  █▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀█ █▀▀█
  █  █ █  █ █▀▀▀ █  █ █    █  █ █  █ █▀▀▀
  ▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀

  Session   TollGate Rust docs vs TIPs comparison
  Continue  opencode -s ses_1f97a7289ffe3Hiz3iG4Qd006b


