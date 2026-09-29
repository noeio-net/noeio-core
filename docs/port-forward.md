# Port forwarding

`noeio forward` forwards one TCP or UDP port between the two sides of a
node: the overlay (its noeio virtual nic) and the physical LAN. It runs in the
foreground and the forwarding lives exactly as long as the command does, like
`ssh -L`, `kubectl port-forward` or `socat`. It is implemented entirely in
user space, so it works the same way on Linux, macOS and Windows and needs no
kernel NAT.

Two directions are covered:

- **overlay → LAN**: a peer connects to `<this node's overlay ip>:8080` and
  reaches `192.168.10.7:80` on the LAN behind this node. The LAN host runs no
  agent and needs no configuration. The target may also be `127.0.0.1`, which
  exposes a loopback-only local service to the overlay.
- **LAN → overlay**: a LAN machine connects to `<this node's LAN ip>:9090`
  and reaches an overlay node at `110.20.0.9:22`. The LAN machine runs no
  agent and needs no route. This is the direction subnet routing does not
  provide (see [subnet-router.md](./subnet-router.md)).

## Usage

```
noeio forward --listen <ADDR>:<PORT> --target <IPv4>:<PORT> [--proto tcp|udp] [--allow-from <CIDR>[,<CIDR>...]]
```

`--listen` decides which side the listener faces:

| `<ADDR>` | Binds | Reachable from |
| --- | --- | --- |
| `noeio` | every overlay address of this node (one per virtual nic) | noeio peers only |
| `lan` | every IPv4 address of the physical interfaces | the physical LAN only |
| a specific IPv4 | that address only; it must currently belong to this machine | whichever side that address is on |

`0.0.0.0` is refused because it would expose the rule on both sides at once.
`127.0.0.1` is refused because nothing needs forwarding to reach the machine
from itself. `--target` must be an IPv4 literal; hostnames are not accepted
because their resolution could change after the rule was validated.

Examples:

```
# expose a LAN host's web UI to the overlay
sudo noeio forward --listen noeio:8080 --target 192.168.10.7:80

# let LAN machines without an agent SSH to an overlay node
sudo noeio forward --listen lan:9090 --target 110.20.0.9:22

# expose a loopback-only database to one overlay subnet
sudo noeio forward --listen noeio:5432 --target 127.0.0.1:5432 --allow-from 110.20.0.0/24

# multi-homed machine: LAN-side listener on one interface only
sudo noeio forward --listen 192.168.10.3:9090 --target 110.20.0.9:22

# a UDP service (here a LAN DNS resolver) for the overlay
sudo noeio forward --listen noeio:53 --target 192.168.10.1:53 --proto udp
```

One command is one rule. For several ports start several processes; each one
has its own terminal, log and lifetime, and stopping one does not affect the
others.

## Lifecycle

The forwarding exists if and only if the process exists.

- `Ctrl+C`, `SIGTERM` (`kill`, `systemctl stop`) and `SIGHUP` (terminal
  closed, SSH dropped) end the process gracefully: listeners are closed,
  in-flight connections are aborted, a summary is printed, exit code 0. On
  Windows the same applies to Ctrl+C, Ctrl+Break, closing the console, logoff
  and shutdown. `SIGHUP` is not a reload; there is nothing to reload.
- `kill -9`, a panic, OOM or a power cut: the kernel closes the process's
  sockets, the listener disappears, the other ends of in-flight connections
  see a reset. There is no residue to clean up and no start-up sweep.
- The daemon (`noeio boot`) is asked once, at start-up, which overlay
  addresses this node has. After that the two processes do not interact. If
  the daemon exits, the forwarder keeps running: LAN → overlay connections
  fail until the daemon is back (then they recover by themselves), an
  overlay-side listener keeps reporting accept errors until the forwarder is
  restarted.
- If the target is down, connections are closed and counted; the forwarder
  does not exit. Once the target is back, no action is needed.
- Address changes after start-up (DHCP, Wi-Fi switch, a new virtual nic) are
  not tracked. Restart the command.

Every address of a rule must bind, otherwise the command fails as a whole
(exit code 1) and nothing stays bound.

Exit codes: `0` stopped by a signal, `1` environment error (daemon not
running, bind failed), `2` invalid rule (every problem is listed).

To keep a forward across reboots, let the service manager own the process:

```ini
[Unit]
Description=noeio port forward: NAS web
After=noeio.service
Requires=noeio.service

[Service]
ExecStart=/usr/local/bin/noeio forward --listen noeio:8080 --target 192.168.10.7:80
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

## UDP

TCP flows have a lifetime of their own; UDP flows do not, so the forwarder
keeps one for them: a **session** per client `(address, port)`, holding the
socket that faces the target. Datagrams from a known client reuse its
session; replies from the target go back to that client through the
listener they arrived on.

- A session that has seen no datagram in either direction for **60 seconds**
  is reclaimed.
- A process holds at most **512** sessions. When the table is full,
  datagrams from *new* clients are dropped and counted; existing sessions
  are never evicted. UDP source addresses are trivially forged, so an
  unbounded table would be a memory-exhaustion path. The cap is therefore a
  safety property, not a setting.
- The session table is process memory: it disappears with the process, and
  there is nothing to clean up.
- The target sees this node's address as the client, as for TCP.

## Output

stdout carries the start-up banner (rule, every bound address, warnings,
`press Ctrl+C to stop`) and the exit summary (listeners closed, connections
aborted, totals). stderr carries the `tracing` log, controlled by `RUST_LOG`;
connection- and session-level events are `debug`, so they are silent by
default.

## Security

Port forwarding creates a new, unauthenticated network entry point.

- A `lan`-side listener exposes an overlay service to **every machine on the
  LAN**. There is no identity on that side to check. The banner prints a
  `WARNING` for this case.
- An `noeio`-side listener exposes a LAN or loopback service to **every peer**
  of the overlay network(s) the node has joined. A target of `127.0.0.1`
  defeats the "only listens on loopback" assumption a service may rely on;
  the banner warns about that too.
- `--allow-from` is a coarse source-IP filter applied after `accept`, not
  authentication.
- The target's logs show this node's address as the client, never the
  original one: both directions must source-NAT, because the target has no
  route back to the other side.
- Overlay → overlay relaying (an overlay-side listener with an overlay
  target) and self-loops are refused at validation time.
- Because `noeio forward` reads the daemon's RPC socket (`/var/run/noeio.sock`,
  mode 0600), it needs the same privileges as `noeio route`.
- The foreground model is itself a safety property: a forward exists only
  while somebody deliberately keeps it running, and never comes back after a
  reboot because of a forgotten configuration line.

## Platform matrix

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| `--listen noeio:<port>` | ✅ | ✅ | ✅ |
| `--listen lan:<port>` | ✅ | ✅ | ❌ write the interface address instead |
| `--listen <IPv4>:<port>` | ✅ | ✅ | ✅ |
| TCP | ✅ | ✅ | ✅ |
| UDP (512 sessions, 60 s idle) | ✅ | ✅ | ✅ |
| graceful stop | SIGINT/SIGTERM/SIGHUP | SIGINT/SIGTERM/SIGHUP | Ctrl+C/Break/Close/Logoff/Shutdown |
| no residue after a hard kill | ✅ | ✅ | ✅ |

## Out of scope for now

IPv6; announcing forwards to peers; identity-based ACLs; preserving the client's source
address (PROXY protocol); draining connections on exit; re-binding when
addresses change; a kernel-mode (nftables DNAT/SNAT) path, which cannot meet
the "no residue after `kill -9`" requirement.
