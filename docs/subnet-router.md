# Subnet routing

A noeio node can act as a **subnet router**: it advertises one or more LAN
CIDRs to the network, and other nodes send traffic for those CIDRs through it.
Hosts in the LAN need no agent. This is the equivalent of Tailscale's
`--advertise-routes` / `--accept-routes`, or WireGuard `AllowedIPs`.

```
   Host-B (no agent)                        Noeio E
   192.168.10.7                             accept_routes = true
        |                                        |
   [ 192.168.10.0/24 ]                           |
        |                                        |
   Noeio A  (Linux)  ---- overlay (WG/UDP) ------+
   advertise_routes = ["192.168.10.0/24"]

   On E:  curl http://192.168.10.7      -> routed via A, SNATed to A's LAN address
```

## Roles

| Role | What it does | Platforms |
| --- | --- | --- |
| **Advertiser** | Declares CIDRs it routes for; forwards + SNATs traffic into its LAN | **Linux only** |
| **Consumer** | Installs routes for CIDRs advertised by others through its tunnel | Linux, macOS, Windows |

Roles are per CIDR, not per node: a Linux node can advertise `192.168.10.0/24`
and at the same time accept `10.0.0.0/8` from another node.

**macOS and Windows cannot advertise.** A non-empty `advertise_routes` in the
config or on the command line makes the daemon exit at start-up with a message
naming the platform and the rejected CIDRs; `noeio route advertise` returns
`FAILED_PRECONDITION` and the daemon keeps running. In neither case does the
CIDR enter the broadcast — this is deliberate. A node that warned but still
advertised would black-hole the subnet for every consumer, and nothing on the
consumer side would show why. Both platforms remain full consumers.

## Configuration

```toml
[router]
# Advertiser (Linux only). Prefix length must be /8 .. /32.
advertise_routes = ["192.168.10.0/24"]
# Consumer.
accept_routes = true
# Advertiser: manage ip_forward and the nftables table automatically.
auto_nat = true
# Advertiser: LAN egress interface; empty = the interface whose address is
# inside the first advertised subnet.
lan_interface = ""
```

Equivalent flags: `noeio boot --advertise-routes 192.168.10.0/24 --accept-routes`.

Runtime changes without restarting:

```
noeio route advertise 192.168.10.0/24
noeio route withdraw  192.168.10.0/24
noeio route list
```

`route list` shows every local and learned CIDR with the peer it came from,
the path currently in use (direct address or relay), its state (`active`,
`standby`, `rejected`) and the rejection reason. On macOS / Windows it
prints `consumer-only`.

### What cannot be advertised

Rejected with an explicit error on the advertiser, and silently not installed
on consumers (both sides validate):

- prefixes shorter than `/8` (including `0.0.0.0/0` — exit nodes are out of scope)
- anything containing this node's overlay address
- anything containing a configured derper or STUN server (would cut the control plane)
- `127.0.0.0/8`, `169.254.0.0/16`

A CIDR with host bits set (`192.168.10.7/24`) is normalized to its network
address with a warning.

### Consumer-side conflicts

When a consumer learns a CIDR, in priority order:

1. It overlaps a subnet one of the consumer's **physical interfaces** is on →
   rejected. The local LAN always wins.
2. The consumer advertises that CIDR itself → rejected (it *is* the exit).
3. It fails the blacklist above → rejected.
4. Several peers advertise the **same** prefix → the peer with the lowest
   `peer_id` is active, the others are standby. If the active one disappears
   the next takes over.
5. **Nested** prefixes from different peers (`10.0.0.0/8` and `10.1.0.0/16`)
   are both installed; longest-prefix match decides.

## How withdrawal works

Routes are never deleted imperatively. The daemon keeps a single desired set
computed from its peer table and a reconciler converges the kernel routing
table toward it — adding what is missing, deleting what is extra — on every
change and on a 30-second tick. That gives three withdrawal paths:

1. **Explicit**: a newer `PeerInfo` from the advertiser no longer lists the
   CIDR (config change, `route withdraw`). Full diff, so the route goes.
2. **Peer gone**: the derper notices a peer's reports stopped (1-minute TTL)
   and broadcasts a tombstone (`withdrawn = true`). Consumers drop the peer
   and everything it advertised. This covers crashes and power loss —
   nothing the consumer could detect itself, because a healthy peer with
   stable config sends *no* updates either (reports are deduplicated).
3. **Tick**: the periodic reconcile heals drift such as a manual
   `ip route del`.

The reconciler never uses "time since the last update" as a liveness signal.

## What survives a crash, and what is done about it

| State | After `kill -9` | Cleanup |
| --- | --- | --- |
| Routes through the TUN | reclaimed by the kernel with the interface (non-persistent TUN) | mirror under `/var/run/noeio/routes` for belt-and-braces; removed on clean exit |
| `net.ipv4.ip_forward` (advertiser) | **stays modified** | original saved to `/var/run/noeio/ip_forward.orig`; restored at next start and on clean exit |
| nftables table `ip noeio` (advertiser) | **stays** | deleted at next start and on clean exit; whole-table replace makes it idempotent |

`/var/run` is tmpfs, so a reboot starts clean. The TUN must stay
non-persistent for the first row to hold — do not create it with
`ip tuntap add` or set `IFF_PERSIST`.

## Advertiser internals (Linux)

The advertiser installs, via netlink (no `nft` or `iptables` binary needed):

```
table ip noeio {
    chain postrouting { type nat hook postrouting priority 100;
        ip saddr <overlay range> oifname <lan_if> masquerade }
    chain forward { type filter hook forward priority 0;
        iifname <tun> oifname <lan_if> accept
        iifname <lan_if> oifname <tun> ct state established,related accept }
    chain mssclamp { type filter hook forward priority -150;
        iifname <tun> tcp flags syn tcp option maxseg size set rt mtu
        oifname <tun> tcp flags syn tcp option maxseg size set rt mtu }
}
```

and enables `net.ipv4.ip_forward`. Replies are un-NATed by conntrack and
reach the consumer through the existing `/32` route to its overlay address.

Requirements: Linux ≥ 4.18, `CAP_NET_ADMIN` in the daemon's own network
namespace, kernel options `NF_TABLES`, `NF_TABLES_IPV4`, `NF_TABLES_NAT`,
`NFT_MASQ`, `NFT_CT`. In a container the NAT modules cannot be auto-loaded;
run `modprobe nft_masq nf_nat nft_ct` on the host first. The error message
tells the two cases apart.

**`ip_forward` is host-wide.** Turning it on makes the host forward IPv4
between *all* its interfaces, not only noeio traffic. The daemon logs a
warning when it does so. Set `auto_nat = false` to manage this yourself.

### MTU

The TUN MTU is 1411. TCP SYNs crossing the advertiser get their MSS clamped
to the path MTU so a 1500-byte LAN host never sends a segment the tunnel
cannot carry — this is what keeps "ping works but large downloads hang" from
happening when ICMP fragmentation-needed is filtered somewhere. Non-TCP
traffic larger than 1411 bytes relies on fragmentation (DF clear) or on
ICMP reaching the sender.

### Coexisting with iptables

nftables and iptables-legacy share the same hooks, conntrack and NAT engine
and run in priority order; neither overrides the other. Two things to know:

- **NAT is stateful.** Once conntrack has bound a NAT mapping for a flow,
  later NAT chains do not translate it again. If an existing
  iptables-legacy `MASQUERADE` rule matches first, noeio's masquerade is a
  silent no-op for that flow (and vice versa). There is no log line for
  this; check `conntrack -L` / `nft list ruleset` when SNAT "does nothing".
- Systems running iptables-legacy **and** iptables-nft at once are the
  hardest to debug: each tool shows only its half of the rules.

noeio only touches its own table `ip noeio`. It never modifies `nat`,
`filter` or `mangle`.

## Trust model

- The RPC socket (`/var/run/noeio.sock`, mode `0600`, root) is
  unauthenticated. Anyone who can connect can rewrite the routing table and,
  on Linux, enable forwarding and NAT. File permissions are the boundary;
  the daemon refuses to serve if the mode is not `0600`.
- Route advertisements travel through the derper. **Trusting a derper means
  trusting it with write access to your routing table** — a malicious relay
  can inject a CIDR and pull that traffic to a node of its choice. This is
  the same trust already placed in it for overlay IP assignment. Both
  advertiser and consumer apply the blacklist above, so a derper cannot use
  this to cut a node off from its own control plane or LAN.
- Inbound packets are checked against WireGuard-style AllowedIPs: a peer may
  only source its own overlay address or an address inside a subnet it
  advertises. Violations are counted per peer and logged with rate limiting.

## Out of scope for now

IPv6 subnets; exit nodes (`0.0.0.0/0`); load balancing across several
advertisers of the same CIDR (active/standby only); LAN-initiated
connections toward overlay nodes (only overlay → LAN is set up; replies
ride on conntrack — for a single port use
`noeio forward --listen lan:<port> --target <overlay ip>:<port>`, see
[port-forward.md](./port-forward.md)); macOS as an advertiser (pf).
