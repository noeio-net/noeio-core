# Port Forwarding Requirements

Status: draft
Date: 2026-09-28
Related code: `noeio`, `noeio-proto`
Related documents: [Subnet router](./subnet-router-requirements.en.md), [docs/subnet-router.md](../subnet-router.md)

---

## 1. Background

Subnet routing already solves "machines on a whole subnet can be reached without installing an agent": an Advertiser announces a CIDR, Consumers install a route, and the whole segment's traffic enters the tunnel. The price is that it is Linux-only (it needs nftables + `ip_forward`) and that it only covers the overlay → LAN direction; connections initiated from the LAN side are explicitly a non-goal (last section of `docs/subnet-router.md`).

Many scenarios do not need a whole network, just one port: the NAS on 8080, the printer on 631, the internal database on 3306, a service that runs on B's own loopback. Conversely, that agent-less machine on the LAN often only needs to reach one service inside the overlay.

Port forwarding covers both gaps. It takes the form of a foreground command, `noeio forward`: forwarding starts when the command starts and disappears when the command ends. This is the same class of tool as `ssh -L`, `kubectl port-forward` and `socat`, for which users already have a solid mental model. It is implemented entirely in user space, needs no kernel NAT, is one code path on all three platforms, and requires no daemon to hold any state.

## 2. Goals and non-goals

### 2.1 Goals

- The user runs `noeio forward <rule>...` on node B; the command runs in the foreground and keeps forwarding until it is terminated.
- **Direction one (overlay → LAN)**: C connects to `B_overlay_ip:8080`, B forwards to A at `192.168.10.7:80` on the same LAN. A runs no agent and needs no configuration.
- **Direction two (LAN → overlay)**: A connects to `B_lan_ip:9090`, B forwards to C at `110.20.0.9:22` inside the overlay. A runs no agent and needs no route.
- The target may also be B's own `127.0.0.1`, exposing a loopback-only local service to the overlay.
- **The forwarding's lifetime is the command's lifetime**: on SIGINT (Ctrl+C), SIGTERM or SIGHUP the command exits gracefully and releases every listener and connection; when it is ended by SIGKILL or any other uncatchable means, the kernel reclaims listeners and connections with the process. No residue is left in any case.
- One command is one forward. To open several forwards, start several processes; each starts and stops independently and none affects the others.
- macOS, Windows and Linux have **fully equal capabilities**.

### 2.2 Non-goals

- **Forwarding hosted by the daemon**: `noeio boot` reads no forwarding configuration, maintains no forwards in the background and tracks no forwarding processes. A forward that must start at boot is a `noeio forward` process managed by systemd / launchd.
- **UDP forwarding**: see FR-4, scheduled for M2. In M1 `udp` is refused with an error, never silently downgraded.
- **IPv6**: consistent with subnet routing; both `noeio-net-route` and the `smoltcp` configuration are currently IPv4-only (`Cargo.toml:33` features = `proto-ipv4`).
- **Announcing / auto-discovering forwarding rules across the network**: if C wants to know which forwards B has opened, somebody has to tell it. Rationale in §10 Q4.
- **Identity-based ACLs**: as with subnet routing today, only the coarse source-IP `--allow-from` exists.
- **Preserving the original client source address** (PROXY protocol / TPROXY): see §10 Q5.
- **Graceful connection draining**: in-flight connections are closed the moment the process exits; nothing waits for them to finish naturally.
- **A Linux kernel-mode (nftables DNAT/SNAT) path**: see §13.

## 3. Terminology

| Term | Meaning |
| --- | --- |
| Forwarding process | The operating-system process of one `noeio forward ...` invocation. It holds every listening socket and proxy task and is the only lifecycle unit of this feature |
| Forwarding rule (rule) | One `listen address:port → target address:port` mapping, given by `--listen` and `--target` of `noeio forward`, with the protocol given by `--proto`. One forwarding process carries exactly one rule |
| Listen address | The address part of `--listen`. The keyword `noeio` expands to every overlay address of this node (the address of each noeio virtual nic); the keyword `lan` expands to every physical interface address of this node; it may also be one specific local IPv4 address. Every address of the expansion gets its own listener |
| Listen side | Derived from the listen address: on the `overlay` side only noeio peers can connect; on the `lan` side only the physical LAN can connect. A specific IP belongs to whichever side owns it: TUN address or physical interface address |
| Target | The `addr:port` given by `--target`, where the rule forwards to. Usually a LAN address or loopback when listening on the `overlay` side, usually a peer's overlay address when listening on the `lan` side |
| User-space forwarding | The forwarding process accepts a connection on the listen side, opens a second connection to the target and copies the byte stream in both directions. The two connections are independent TCP connections |
| Daemon | The `noeio boot` process. The forwarding process makes one read-only query to it (FR-6.2); on the data plane the two are not coupled at all |

## 4. Scenario

```
   A  192.168.10.7                B  overlay 110.20.0.1            C  overlay 110.20.0.9
   no agent                        lan 192.168.10.3                  accept_routes irrelevant
        |                               |                                  |
        +------ physical LAN 192.168.10.0/24 ------+                        |
                                        |                                  |
                                        +------ overlay (WireGuard/UDP) ---+

   Terminal 1 on B:
           $ noeio forward --listen noeio:8080 --target 192.168.10.7:80
           forwarding tcp noeio:8080 -> 192.168.10.7:80
             bound 110.20.0.1:8080
           press Ctrl+C to stop

   Terminal 2 on B:
           $ noeio forward --listen lan:9090 --target 110.20.0.9:22
           forwarding tcp lan:9090 -> 110.20.0.9:22
             bound 192.168.10.3:9090
           WARNING: this exposes overlay host 110.20.0.9:22 to the whole LAN without authentication
           press Ctrl+C to stop

   Direction one: on C, `curl http://110.20.0.1:8080`
           -> the forwarding process's overlay-side listener accepts
           -> the forwarding process opens a second connection to 192.168.10.7:80
           -> A's access log shows 192.168.10.3 (B's LAN address) as the source

   Direction two: on A, `ssh -p 9090 192.168.10.3`
           -> the forwarding process's LAN-side listener accepts
           -> the forwarding process opens a connection to 110.20.0.9:22 (the kernel sends it
              into B's TUN via the /32 host route; the daemon encapsulates it)
           -> C sees 110.20.0.1 (B's overlay address) as the source

   Ctrl+C in terminal 1 (terminal 2's forward is unaffected):
           ^C
           shutting down: 1 listener closed, 3 connections aborted
           total: 17 connections, 4.2 MiB in, 118.6 MiB out, 0 target failures, 0 rejected by allow-from
           $
```

## 5. Feasibility

### 5.1 Conclusion

Both directions are feasible, and **no protocol, no derper and no `PeerInfo` change is needed**; the daemon only gains one read-only RPC. Port forwarding is purely local behaviour: C is just connecting to an ordinary `ip:port`, and so is A; neither side needs to know what happens behind it.

The forwarding process and the daemon are **two independent processes** with no coupling on the data plane. This holds because the following three facts are properties of the host network stack, independent of which process owns the socket:

1. **Listening on an overlay address is possible for any local process.** The TUN carries this node's overlay address with a `/32` mask (`noeio/src/interface/virtual_nic.rs:74`); it is an ordinary local address. After decapsulation the daemon writes the packet into the TUN (`noeio/src/daemon.rs:845` `write_to_nic`), and a packet addressed to the host itself is delivered by the host stack to the listening socket, whichever process owns it. A plain `TcpListener::bind((overlay_ip, port))` is enough.
2. **Connecting to an overlay peer from any local process is possible too.** The reconciler already installs a `/32` host route for every peer (`noeio/src/daemon/reconciler.rs:70`) with the TUN as egress. So an ordinary `TcpStream::connect(C_noeio:22)` in the forwarding process is routed by the kernel into the TUN, read by the daemon's `process_outbound` (`daemon.rs:415`), looked up, encapsulated and sent to C. The kernel picks the source address by egress interface, i.e. the TUN's address, which is B's overlay address.
3. **The anti-spoofing check needs no change.** `Router::allowed_source` (`daemon/router.rs:133`) requires the inner source address of an inbound packet to be the peer's own overlay address or one of the subnets it advertises. In direction one the packets from C carry C's own overlay address; in direction two the packets B sends to C carry B's overlay address. Both pass naturally.

The only information the forwarding process needs from the daemon is "which overlay addresses does this node have" and "which addresses belong to the overlay" (for the relay refusal in FR-1.3). That is one read-only query at start-up (FR-6.2); after it the two processes never interact again. The daemon exiting or restarting later does not affect the forwarding process's survival (though it makes overlay-direction connections fail; see FR-3.6).

### 5.2 Why a foreground process

Making forwarding a foreground command rather than a rule table inside the daemon has three reasons:

1. **The usage shape matches the need.** The typical use of port forwarding is "expose this thing for a while". `ssh -L`, `kubectl port-forward` and `socat` have no rule table: they forward while open and stop when closed.
2. **The operating system guarantees the lifecycle.** The listening socket belongs to the forwarding process. However the process exits (normal return, Ctrl+C, `kill -TERM`, `kill -9`, terminal closed, OOM), the kernel reclaims its fds. No convergence loop, no residue sweep, no "rule deleted but connection still alive" state machine. This guarantee does not depend on any software logic running correctly.
3. **The daemon stays lean.** The only daemon addition is one read-only query (FR-6.2) telling the forwarding process this node's overlay addresses. No rule table, no add/remove RPCs, no reconciler branch, no shutdown branch.

The cost is that there is no "change a rule while running" and no "list current forwards": to change a rule, end the process and start a new one; to see current forwards, look at that terminal. This matches `ssh -L` and is acceptable.

### 5.3 Why user space rather than Linux kernel DNAT/SNAT

A kernel-mode design is technically possible, but several arguments make it untenable here:

- **The performance argument does not hold.** Every forwarded byte already passes through user space once: noeio's WireGuard is boringtun (`Cargo.toml:10`), encryption and decryption happen in user space, and packets go through `process_outbound` / `handle_delivery` reading and writing the TUN. Kernel DNAT saves the proxy's one `copy_bidirectional`, while the same path still contains AEAD crypto and TUN I/O. Saving one memcpy while keeping one ChaCha20-Poly1305 is a gain at noise level.
- **The source-address argument does not hold.** In direction one A has no route back to the overlay, in direction two C has no route back to the physical LAN, so both directions must SNAT. In kernel mode the target also sees B's address as the source; the observable behaviour is identical to user space.
- **It is incompatible with the lifecycle goal.** nftables rules and `ip_forward` do not disappear with the process. After `kill -9` there is inevitably residue, which brings back subnet routing's original-value saving (`/var/run/noeio/ip_forward.orig`) and start-up sweep (`daemon/nat.rs:35` `sweep_leftovers`). That directly violates §2.1's "no residue in any case".

Item-by-item comparison:

| Dimension | User-space proxy | Linux kernel DNAT/SNAT |
| --- | --- | --- |
| New code | About 300 lines, a standard tokio proxy | About 200 lines of nftables expression encoding + byte-level tests, plus a convergence/cleanup layer |
| Platform coverage | One code path on three platforms | Linux only; the other two platforms still need user space, i.e. two paths to maintain |
| Privileges | Binding the port | `CAP_NET_ADMIN` + global `net.ipv4.ip_forward` + the `nft_masq`/`nf_nat`/`nft_ct` modules |
| Usable in containers | Yes | Limited; NAT modules cannot be auto-loaded in unprivileged containers (already recorded in `docs/subnet-router.md`) |
| MSS / MTU | **Nothing to handle.** The two ends of the proxy are independent TCP connections: the overlay side negotiates against the TUN's 1411, the LAN side against 1500 | Needs an MSS clamp mangle chain, otherwise "ping works but large files hang" |
| Residue after `kill -9` | **None.** The listener vanishes with the process fds | `ip_forward` and the nftables table remain |
| Observability | `ss -ltn` shows the listener; per-rule connection/byte/error counts are available for free | No listener to look at; known silent failure mode: conntrack already bound another NAT mapping, the rule becomes a no-op with no log |
| Target `127.0.0.1` | Works directly | Needs `route_localnet` additionally |
| Protocol coverage | TCP complete; UDP needs a session table (FR-4) | Both TCP and UDP handled by conntrack |

The only clear kernel-mode advantage is UDP: conntrack gives session tracking for free, whereas user space has to write a TTL-bearing session table itself. But that table is bounded, deterministic work of about 150 lines (FR-4), and in exchange we get three-platform parity and no residue. Still worth it.

## 6. Command-line interface

### 6.1 Place in the `noeio` command tree

```
noeio
├── boot          start the daemon (long-running)
├── netcheck      measure RTT to each derper
├── create vnic   create a virtual nic
├── route         subnet-route management
│   ├── advertise <CIDR>...
│   ├── withdraw  <CIDR>...
│   └── list
└── forward       port forwarding (foreground; this document)
```

Like `boot`, `forward` is a long-running foreground command, unlike `route`, whose subcommands return after one RPC. It has no subcommands: starting a forward is running this command, stopping it is ending this command.

### 6.2 `noeio forward` usage

```
noeio forward --listen <ADDR>:<PORT> --target <ADDR>:<PORT> [OPTIONS]

Required:
      --listen <ADDR>:<PORT>
          Listen address and port. <ADDR> may be:
            noeio          every noeio virtual nic address of this node (the overlay
                           addresses); only noeio peers can connect
            lan            every IPv4 address of this node's physical interfaces; only
                           the physical LAN can connect
            <IPv4>         one specific address this machine currently holds; treated as
                           the overlay side if it is a TUN address, as the lan side if it
                           is a physical interface address. For multi-homed machines that
                           want to listen on one interface only
          0.0.0.0 (exposes both sides at once) and 127.0.0.1 (the machine connecting to
          itself needs no forwarding) are refused.
          <PORT> is 1..=65535.

      --target <ADDR>:<PORT>
          Forwarding target: an IPv4 literal plus port, e.g. 192.168.10.7:80 or
          127.0.0.1:5432. Hostnames are not accepted.

Optional:
      --proto <PROTO>
          Transport protocol: tcp (M1) or udp (M2; refused with an error in M1).
          [default: tcp]

      --allow-from <CIDR>[,<CIDR>...]
          Accept connections only from these IPv4 subnets; everything else is closed
          right after accept and counted. Unrestricted by default. A coarse source-IP
          filter, not authentication.

  -h, --help
  -V, --version
```

One command describes one forward. For several, start several processes, each with its own terminal, log and lifetime.

### 6.3 Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Exited normally after a termination signal (Ctrl+C / SIGTERM / SIGHUP, or the Windows equivalents) |
| 1 | Environment error: daemon unreachable, or any listen address failed to bind |
| 2 | Argument error: rule parsing or validation failed, illegal option (same code clap uses for its own usage errors) |

`kill -9` and other uncatchable endings have no exit code to speak of; see FR-3.4.

### 6.4 Output conventions

| Stream | Content |
| --- | --- |
| stdout | Start-up banner (FR-5.2) and exit summary (FR-5.4). Both are one-off and meant for humans |
| stderr | `tracing` log (FR-5.3), controlled by `RUST_LOG` |

So `noeio forward ... 2>forward.log` leaves only the banner and summary on the terminal, and `noeio forward ... >/dev/null` leaves only the log.

### 6.5 Examples

Expose the web UI of a NAS on the LAN to the overlay:

```
$ sudo noeio forward --listen noeio:8080 --target 192.168.10.7:80
```

Let agent-less machines on the LAN SSH to overlay node C:

```
$ sudo noeio forward --listen lan:9090 --target 110.20.0.9:22
```

Expose a loopback-only local database to the overlay, allowing one subnet only:

```
$ sudo noeio forward --listen noeio:5432 --target 127.0.0.1:5432 --allow-from 110.20.0.0/24
```

Open a group of ports at once: one process each, started and stopped independently.

```
$ sudo noeio forward --listen noeio:8080 --target 192.168.10.7:80 &
$ sudo noeio forward --listen noeio:631  --target 192.168.10.12:631 &
$ sudo noeio forward --listen lan:9090   --target 110.20.0.9:22 &
```

On a multi-homed machine, open the LAN-side listener on the wired interface only (write the specific address instead of `lan`):

```
$ sudo noeio forward --listen 192.168.10.3:9090 --target 110.20.0.9:22
```

Run as a systemd service (systemd owns the lifecycle; `systemctl stop` sends SIGTERM and the forward stops with it):

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

To stop a forward: press Ctrl+C in the terminal running it, or `kill -TERM <pid>` the process. There is no other way, and none is needed.

## 7. Functional requirements

### FR-1 Writing and validating a rule

- **FR-1.1** One command describes one rule: `--listen <ADDR>:<PORT>` says where to listen, `--target <ADDR>:<PORT>` says where to forward, `--proto` (default `tcp`) says which protocol; the full description is in §6.2. The two address options have the same shape and read as "from here to there". The address part of `--listen` accepts the keywords `noeio` / `lan`, because the vast majority of uses are "bind on the noeio nic side" or "bind on the LAN side", and users should not have to look up their overlay address and copy it into the command line; write a specific IP when precise control is needed.
- **FR-1.2** `--allow-from` is an auxiliary option of the rule (§6.2), structured like `Boot` in `cli.rs:15`. For several rules, start several processes, each with its own set of options.
- **FR-1.3** All validation happens in one pure function (unit-testable; the same "single gate" pattern as `validate_advertisement` in `daemon/routes.rs:134`). **If validation fails the command does not start**: the process exits with code 2 and prints every error (not just the first):
  - `--listen` parses as `<ADDR>:<PORT>`, where `ADDR` is `noeio`, `lan` or an IPv4 literal;
  - the `--listen` port is in `[1, 65535]`;
  - when `--listen` gives a specific IP, it must be an address this machine **currently holds**: either in the overlay address set returned by FR-6.2 (then it is overlay-side) or in the physical address set enumerated by FR-2.2 (then it is lan-side); in neither → "not a local address". **`0.0.0.0` is refused** (FR-2.4), and so is `127.0.0.1` (the machine reaching itself needs no forwarding);
  - `--proto` currently accepts only `tcp`; `udp` reports "not implemented yet" (FR-4.4); any other value is illegal;
  - `--target` parses as `IPv4:port` with a non-zero port; hostnames are not accepted (their resolution can change, which does not fit the "validate once at start-up" model);
  - **self-loops are refused**: `--target` equal to any `addr:port` this process is about to bind (overlay-side bind addresses come from FR-6.2, lan-side ones from FR-2.2);
  - **overlay → overlay relaying is refused**: the listen side is overlay (keyword `noeio` or a specific IP classified as overlay-side) and the `--target` address is one of this node's overlay addresses or any peer overlay address the daemon currently knows (the two sets returned by FR-6.2). This would turn B into a relay between two peers, a policy question outside this round's scope; a dedicated error is reported instead of silently allowing it;
  - every `--allow-from` item parses as an IPv4 CIDR;

- **FR-1.4** The checks that need daemon information (self-loop, relay refusal) depend on the FR-6.2 query. Behaviour when the daemon is absent is in FR-6.3.

### FR-2 Deriving and binding listen addresses

- **FR-2.1** `--listen noeio:<port>` expands to every overlay address of this node returned by FR-6.2. A node usually has one virtual nic; with several networks every nic is bound.
- **FR-2.2** `--listen lan:<port>` expands to the IPv4 addresses of all physical interfaces, excluding loopback and this node's TUNs (the TUN addresses are the overlay addresses returned by FR-6.2). Enumeration reuses the existing `pnet` (`local_lans` in `daemon/routes.rs:284` already does this; this feature needs addresses rather than networks, so a shared `local_addrs` is extracted).
- **FR-2.3** `--listen <IPv4>:<port>` is not expanded; only that one address is bound. FR-1.3 has already determined which side it belongs to.
- **FR-2.4** **Binding `0.0.0.0` is not allowed**, even when configured explicitly. It would expose a `lan` rule on the overlay side and an `overlay` rule to the physical LAN at the same time; it is the easiest security incident this feature could cause. Keeping the two sides apart is the core invariant of this design.
- **FR-2.5** The address set is **derived once at start-up**. One rule may map to several bind addresses (several virtual nics on the overlay side, several physical interfaces on the lan side); **the rule enters service only after every address has bound successfully**. Any failure (`EADDRINUSE`, address does not exist, insufficient privilege for `<1024`) fails the command as a whole; already-bound listeners are released as the process exits; exit code 1; the error names the address and the reason. A foreground command "running on half its addresses" is harder to notice than an outright failure; all-or-nothing is the correct semantics for a command-line tool.
- **FR-2.6** Changes to the address set after binding (DHCP renewal, Wi-Fi switch, the daemon registering a new virtual nic) are **not handled**. Once its address is gone, a bound socket starts failing on accept, which is counted and logged per FR-5; the user restarts the command. Rationale in §10 Q2.
- **FR-2.7** On Windows `pnet` is not linked (`routes.rs:301`), so the `lan` keyword cannot be expanded: validation reports an error suggesting a specific physical interface address instead (`--listen 192.168.10.3:9090`). In that case FR-1.3's "is this a local address" check degrades to the overlay set only: any IP not in it is accepted as lan-side and left to fail at bind. The `noeio` keyword is unaffected, since its addresses come from FR-6.2.

### FR-3 Lifecycle

This is the core invariant of the feature: **a forward exists if and only if its forwarding process exists.**

- **FR-3.1 Start-up sequence**; any failing step exits without entering service:
  1. connect to the daemon RPC and query the overlay address set (FR-6.2);
  2. enumerate physical addresses (FR-2.2);
  3. validate the whole rule (FR-1.3);
  4. bind every listener (FR-2.5);
  5. print the start-up banner (FR-5.2);
  6. start one accept loop per listener and enter service.
- **FR-3.2 Graceful exit.** The forwarding process waits for any of the signals below; on receipt it stops every accept loop, aborts every in-flight connection task, closes every listener, prints the exit summary (FR-5.4) and exits with code 0.
  - Unix: `SIGINT` (Ctrl+C), `SIGTERM` (`kill`, systemd stop), `SIGHUP` (terminal closed, SSH dropped). All three mean "exit"; `SIGHUP` is **not** a reload, because this command has nothing to reload.
  - Windows: `Ctrl+C`, `Ctrl+Break`, console close (`CTRL_CLOSE_EVENT`), logoff/shutdown (`CTRL_LOGOFF_EVENT` / `CTRL_SHUTDOWN_EVENT`). tokio's `signal::windows::{ctrl_c, ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown}` covers them.
  - The existing `wait_for_shutdown_signal` at `main.rs:118` handles only `SIGINT`/`SIGTERM`; it is extended to the set above and moved to a shared location so `noeio boot` benefits too.
- **FR-3.3 Exit deadline.** All the work of a graceful exit is bounded: aborting tasks is synchronous, closing sockets is synchronous, and it normally completes in milliseconds. The whole exit is still wrapped in a 2-second timeout; on timeout the process calls `process::exit(0)` directly and the kernel reclaims in-flight connections; there is no cleanup worth waiting for. This differs from the 5-second wait for route cleanup in `noeio boot`: there, real system state has to be restored; here, none.
- **FR-3.4 Uncatchable exits.** `SIGKILL`, panic, OOM, power loss and reboot: the kernel closes every fd of the process, listeners are released immediately, and the other ends of in-flight connections receive RST or FIN. **There is no residual state to clean up and no start-up sweep logic.** This is the substantive advantage of the user-space design over subnet routing (which needs `/var/run/noeio/ip_forward.orig` and `sweep_leftovers` in `daemon/nat.rs:35`) and the direct reason §13 excludes a kernel-mode path.
- **FR-3.5 Target unreachable**: when connecting to the target fails after accept, the client connection is closed immediately, the rule's counter is bumped and the log is rate-limited (the "first + every 1000th" pattern from `daemon.rs:813`). **The process does not exit and the listener is not withdrawn because the target is down**; once the target recovers, no manual intervention is needed.
- **FR-3.6 Daemon exit**: the forwarding process **does not watch** the daemon and does not exit when it exits. A `lan`-side rule's connections to overlay targets start failing (the TUN and host routes vanish with the daemon) and are counted via FR-3.5; an `overlay`-side listener bound to a TUN address starts failing on accept once the address is gone, likewise counted and logged with rate limiting. After the daemon `boot`s again, the `lan → overlay` direction recovers by itself (routes are reinstalled), while an `overlay`-side listener needs the forwarding process restarted (the address reappears but the old socket is dead). Rationale in §10 Q3.
- **FR-3.7 One process, one rule.** Forwarding processes share no state and are unaware of each other; one exiting does not affect the others. Several forwards are several processes, each managed by the user or a process manager (systemd template unit, launchd).

### FR-4 UDP (M2)

TCP flows have a natural lifetime thanks to connection semantics; UDP flows do not, so a session table is needed:

- **FR-4.1** The session key is the client's `(addr, port)`; the value is a socket opened toward the target plus the time of last activity.
- **FR-4.2** The idle TTL defaults to 60 seconds; expired sessions are reclaimed.
- **FR-4.3** The session cap is fixed at 512, with no command-line option. UDP source addresses are forgeable, and no cap means a memory-exhaustion path, so this bound is a safety property rather than a setting. When the cap is reached, new sessions are dropped and counted; active sessions are never evicted. TCP has no connection cap: each connection has its own lifetime and the fd count is bounded by the operating system's per-process limit.
- **FR-4.4** In M1 `udp` is refused at the validation layer with an error that clearly says "not implemented yet", rather than silently treating it as TCP.
- **FR-4.5** The session table vanishes with the process; nothing needs active cleanup at exit.

### FR-5 Observability

The foreground stdout is the whole interface, so each of the three outputs has to be complete on its own:

- **FR-5.1 Counters.** The forwarding process maintains: current connections, total connections, bytes in both directions, target connection failures, connections rejected by `--allow-from`, accept errors.
- **FR-5.2 Start-up banner** (stdout, printed once on entering service). The first line is the rule itself (protocol, `--listen` as written, target); then one line per actually bound address; then the security notices required by FR-8. The last line of the banner is always `press Ctrl+C to stop`, because that is the one thing the user needs to know: how to stop.
- **FR-5.3 Runtime log** (`tracing`, on stderr). Rule-level events at `info`: accept loop started/stopped, bound address gone. Connection-level events at `debug`: one line each for open and close, with the peer address and bytes transferred. Connection-level events are never `info`, otherwise one scanner can flood the log. The default log level is `info`, so connection-level logs are hidden by default; use `RUST_LOG=debug` when needed.
- **FR-5.4 Exit summary** (stdout, printed once on graceful exit). The first line says how many listeners were closed and how many in-flight connections aborted; the second line is every counter of FR-5.1. `kill -9` naturally prints nothing, which is acceptable, because the summary is a convenience and not part of correctness.
- **FR-5.5** Optional: on Unix, `SIGUSR1` prints the current counters to stderr without exiting. Registered as a low-priority M3 item.

### FR-6 Interaction with the daemon

The forwarding process and the daemon interact **exactly once, with a read-only query at start-up**, and never again.

- **FR-6.1** The CLI subcommand is `noeio forward`, added to `Command` at `cli.rs:13` alongside `Boot` / `Route`. **It has no sub-subcommands.** The `match` at `main.rs:23` gains one arm; structurally it is closer to `Boot` (a `tokio::select!` waiting for signals) than to `Route` (one RPC and return).
- **FR-6.2** The daemon gains one read-only RPC. Virtual nic addresses live only in the daemon's in-memory `NicManager` (`daemon/nic.rs:14`); the forwarding process is another process and cannot read them. Guessing which system interface is the noeio TUN by enumerating interfaces is unreliable: on Linux the name is fixed to `noeio0` (`interface/virtual_nic.rs:87`), but macOS forces the system-assigned `utunN`, mixed in with other VPNs, and Windows is similar. So expanding `--listen overlay` must ask the daemon. A `List` is added to the existing `VirtualNicService` (`noeio-proto/protos/noeio/v1/virtual_nic.proto:15`), paired with the existing `CreateVirtualNic`; no new service:

  ```proto
  service VirtualNicService {
    rpc CreateVirtualNic(CreateVirtualNicRequest) returns (CreateVirtualNicResponse);
    rpc ListVirtualNics(ListVirtualNicsRequest) returns (ListVirtualNicsResponse);
  }

  message ListVirtualNicsRequest {}

  message VirtualNicEntry {
    // Interface name in the OS: noeio0 on Linux, utunN on macOS, the adapter name on Windows.
    string tun_name = 1;
    // This nic's overlay address (VirtualNic.ip).
    string ip = 2;
    // The network this nic joined.
    string network_id = 3;
    // This node's peer id in that network.
    uint32 peer_id = 4;
    // Overlay addresses of the other peers currently known in that network
    // (Router::ips(), daemon/router.rs:163).
    repeated string peer_ips = 5;
  }

  message ListVirtualNicsResponse {
    repeated VirtualNicEntry nics = 1;
  }
  ```

  The forwarding process takes: `nics[].ip` for overlay-side binding (FR-2.1), for excluding TUNs on the lan side (FR-2.2), for deciding which side a specific IP belongs to, and for the self-loop check; `nics[].peer_ips` for relay refusal (FR-1.3). `tun_name` / `network_id` / `peer_id` come along for free, for the banner and for future commands like `noeio vnic list`; this feature does not depend on them. The implementation lives in `noeio/src/rpc/service/nic.rs`, the client method in `rpc/client.rs`.
- **FR-6.2a** With several virtual nics, `--listen noeio:<port>` **binds all of them**; it does not default to the first. Several nics mean this node has joined several noeio networks, and without further qualification the most natural meaning of "expose to noeio" is exposing to all of them. `NicManager` is a `DashMap` with unstable iteration order, so "the first one" is not a well-defined notion and two start-ups could bind different nics. Users who want to expose to one network only write the specific IP (`--listen 110.20.0.1:8080`). Rationale in §10 Q9.
- **FR-6.3** When the daemon is absent, `noeio forward` **refuses to start**, with the same error as `noeio route` (`main.rs:101`: ``Is `noeio boot` running?``). There is no bypass that skips the daemon and takes overlay addresses by hand: without the daemon there is no TUN, an overlay-side listener cannot bind and a lan-side rule cannot reach the overlay, so the forward itself is meaningless; an error is more honest than a process that is bound to fail (§10 Q8).
- **FR-6.4** The trust model of the RPC socket is unchanged, following the conclusion in `docs/subnet-router.md`: `/var/run/noeio.sock` is `0600` root, unauthenticated, and the file permission is the boundary (`rpc/mod.rs:7`). This means `noeio forward`, like `noeio route`, needs root (or equivalent access to the socket) to run. As a side effect, binding ports `<1024` is no longer a problem (§10 Q7). This has to be documented.
- **FR-6.5** The daemon does **not** know that forwarding processes exist; it does not track, list or manage them. `noeio route list` does not show forwards. As soon as the daemon starts tracking forwarding processes it has to answer "how does the daemon learn that a forwarding process died", which is an extra layer of state management and contradicts the second reason in §5.2.

### FR-7 Relationship to subnet routing

- **FR-7.1** The two features are orthogonal, can be enabled together, and do not depend on each other. Port forwarding needs neither `accept_routes` nor any node advertising a subnet.
- **FR-7.2** When B both advertises `192.168.10.0/24` and opens an `overlay`-side forward to an address in that subnet, both paths are valid at the same time: C can connect to `192.168.10.7:80` directly (subnet routing) or to `110.20.0.1:8080` (port forwarding). This is not a conflict and needs no deduplication. The start-up banner may add a hint line; not required.
- **FR-7.3** Direction two (LAN → overlay) **fills exactly the direction subnet routing lists as a non-goal** (`docs/subnet-router.md:200`, "LAN-initiated connections toward overlay nodes"). That passage in both documents has to be updated to point to `noeio forward --listen lan:<port> ...`.

### FR-8 Security requirements

This section stands on its own because port forwarding **creates a new unauthenticated network entry point**, which is where its risk structure differs most from subnet routing.

- **FR-8.1** A lan-side listener exposes an overlay service to the whole physical LAN **without any authentication**. A runs no agent, so there is no identity to verify. Any machine on the same LAN can connect. The start-up banner must contain a `WARNING` line for lan-side listeners, and the documentation must say so clearly.
- **FR-8.2** An overlay-side listener exposes a LAN service (or a loopback service of B) to **every** peer in the noeio network. There is currently no ACL mechanism (same as subnet routing today).
- **FR-8.3** `--allow-from` is the only narrowing tool in this round: a source-IP CIDR match evaluated after accept and before connecting to the target. It is not authentication, only a coarse filter; the documentation must not describe it as a security boundary.
- **FR-8.4** When the target is loopback, the banner must warn specifically: a rule such as `--listen noeio:5432 --target 127.0.0.1:5432` bypasses the "the service only listens on loopback" protection a developer may have been relying on.
- **FR-8.5** FR-1.3's self-loop and overlay→overlay relay refusals are hard requirements, not suggestions: the former lets the forwarding process kill itself, the latter turns B into an open relay.
- **FR-8.6** The foreground-process model is itself a security benefit: a forward exists only during the time somebody explicitly started it and keeps it running, and never quietly comes back after a reboot because of a forgotten configuration line. Worth stating in the documentation.

## 8. Platform support matrix

| Capability | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Overlay-side listener (direction one) | ✅ | ✅ | ✅ |
| LAN-side listener (direction two), `--listen lan:<port>` | ✅ | ✅ | ❌ write the specific address (FR-2.7) |
| LAN-side listener, `--listen <IPv4>:<port>` | ✅ | ✅ | ✅ |
| Target is a LAN address | ✅ | ✅ | ✅ |
| Target is an overlay peer | ✅ | ✅ | ✅ |
| Target is `127.0.0.1` | ✅ | ✅ | ✅ |
| TCP | ✅ M1 | ✅ M1 | ✅ M1 |
| UDP | ✅ M2 | ✅ M2 | ✅ M2 |
| Graceful exit on Ctrl+C / termination signal | SIGINT/SIGTERM/SIGHUP | SIGINT/SIGTERM/SIGHUP | Ctrl+C/Break/Close/Logoff/Shutdown |
| No residue after `kill -9` / forced termination | ✅ kernel reclaims | ✅ kernel reclaims | ✅ kernel reclaims |
| `SIGUSR1` prints counters (FR-5.5) | ✅ | ✅ | ❌ no equivalent signal |

Three-platform parity is the direct result of choosing user space. Compare the same table for subnet routing: the Advertiser role is Linux-only, macOS/Windows can only be Consumers.

## 9. Acceptance criteria

### 9.1 End to end

- **AC-1** Direction one: on B, `noeio forward --listen noeio:8080 --target 192.168.10.7:80`; on C, `curl http://<B_overlay>:8080` returns A's page; A's access log shows B's LAN address as the source.
- **AC-2** Direction two: on B, `noeio forward --listen lan:9090 --target <C_overlay>:22`; on A, `ssh -p 9090 <B_lan_ip>` reaches C; `who` / the log on C shows B's overlay address as the source.
- **AC-3** Loopback target: on B, `noeio forward --listen noeio:3000 --target 127.0.0.1:3000`; C can reach the service on B that is bound to loopback only.
- **AC-4** Large transfer: transfer a file of ≥100 MB in direction one; the checksum matches and nothing hangs. This specifically verifies the conclusion "each side of the proxy negotiates its own MSS, no clamp needed".
- **AC-5** Side isolation: a `--listen noeio:...` forward is unreachable from the physical LAN; a `--listen lan:...` forward is unreachable from overlay peers.
- **AC-6 Ctrl+C**: with one in-flight connection through the running forwarding process (e.g. a large file mid-transfer), press Ctrl+C: the process exits within 2 seconds with code 0 and prints the exit summary; both ends of the in-flight connection receive RST/FIN immediately; the listening port disappears from `ss -ltn` (Linux) / `netstat -an` (macOS/Windows); the same port can be bound again right away.
- **AC-7 SIGTERM / SIGHUP**: send `kill -TERM` and `kill -HUP` to the AC-6 scenario; the result equals AC-6.
- **AC-8 SIGKILL**: send `kill -9` to the AC-6 scenario: the process disappears immediately (no summary); both ends of the in-flight connection receive RST; the listening port disappears and can be rebound immediately. `nft list ruleset` and `sysctl net.ipv4.ip_forward` are identical to before start-up (this feature never touches them).
- **AC-9 Terminal closed**: start the forwarding process in an SSH session and drop the SSH connection (triggering SIGHUP); the result equals AC-7. On Windows, close the console window; the result equals AC-6.
- **AC-10 Independent processes**: start one forward in each of two terminals; both are reachable at the same time; press Ctrl+C or `kill -9` one of them: only its port disappears, the other stays reachable and its counters are unaffected.
- **AC-11 All-or-nothing binding**: `--listen lan:<port>` on a machine with two physical interfaces, where the target port on one interface's address is already taken: the command does not start, exits with code 1, and the error names which address; the other interface's address is **also not** bound.
- **AC-12** Target goes down and comes back: while the target service is stopped, connections are refused and counted; after the target returns, connectivity resumes with no action, and the forwarding process survives throughout.
- **AC-13** Daemon absent: `noeio forward ...` refuses to start, exits with code 1, and prints ``Is `noeio boot` running?``.
- **AC-14** Coexistence with subnet routing: enable `advertise_routes` and port forwarding at the same time; both access paths work.
- **AC-15** `--allow-from` takes effect: source addresses outside the list are refused and counted, and the refusal count appears in the exit summary.

### 9.2 Unit and integration tests

- **AC-16** Table-driven tests of rule parsing and validation: `--listen` missing the port, `--listen` address that is neither a keyword nor a valid IPv4, `--listen 0.0.0.0:...`, `--listen 127.0.0.1:...`, `--listen` with an IP that is not local, `udp` (in M1), port 0, port 65536, hostname target, self-loop, overlay→overlay relay (one case each for the keyword `noeio` and for a specific IP classified as overlay-side, one case each for a target that is this node's address and a peer's address), invalid CIDR. All are refused with the right error type. Written after the existing `validate_routes` tests in `config.rs:299`.
- **AC-17** Pure-function tests of bind derivation: given the three shapes of `--listen` (`noeio:p` / `lan:p` / `ip:p`), an overlay address set and a physical address set, the resulting `[SocketAddr]` and the derived listen side are correct; TUN addresses are excluded when `lan` is expanded; a specific IP is not expanded. No real sockets.
- **AC-18** Loopback test of the proxy: start a local echo server as the target, forward through a rule, verify byte integrity in both directions and correct propagation of connection close (a FIN on one end is seen on the other).
- **AC-19** Process-level lifecycle tests (an integration test under `tests/` that starts a `noeio forward` child via `std::process::Command`):
  - establish a connection through the forward and keep it open;
  - send `SIGINT` / `SIGTERM` / `SIGHUP` / `SIGKILL` to the child (on Windows, `TerminateProcess` for the last and `GenerateConsoleCtrlEvent` for the rest);
  - assert: the child exits within 2 seconds; the client socket reads EOF or `ECONNRESET`; the listening port can be rebound by the test process itself within 1 second.
  - This test needs either a fake daemon RPC (returning a fixed `ListVirtualNics`) or the "query overlay information" step extracted into an injectable trait so the test can skip it. The latter is preferred, to avoid starting a Unix socket in the test.
- **AC-20** UDP session table (M2): TTL expiry reclaims sessions, the session cap (512) holds, and different source addresses do not interfere.

## 10. Risks and open questions

| # | Question | Notes | Leaning |
| --- | --- | --- | --- |
| Q1 | User space vs. Linux kernel mode | Full analysis in §5.3. Beyond the performance and source-address arguments not holding, the decisive reason is that nftables rules do not vanish with the process: any kernel-mode implementation leaves residue after `kill -9`, directly violating the invariant "a forward exists iff its process exists" | **User space on all three platforms; kernel mode out of scope** (§13) |
| Q2 | What to do when the address set changes after binding | Wi-Fi switch, DHCP giving a new address, the daemon registering a new virtual nic. Options: (a) do nothing, the user restarts the command; (b) listen for address-change events and rebind; (c) re-enumerate periodically and diff | **(a) for M1**. A foreground command's user is at the terminal; restarting one command costs far less than an address-watching layer; `ssh -L` behaves as (a) in the same situation. If it comes up often in practice, consider (c) |
| Q3 | Should the forwarding process exit when the daemon exits | Exiting along: clear behaviour, but needs a long-lived connection or polling toward the daemon, coupling the two processes. Not exiting: the `lan → overlay` direction recovers by itself after the daemon restarts, but an `overlay`-side listener keeps failing on accept until the user restarts | **Do not exit** (FR-3.6). Make "bound address gone" explicit with FR-5.3's rule-level `info` log and let the user decide |
| Q4 | Should forwarding rules go into the `PeerInfo` broadcast | Broadcasting lets C know what B offers, but needs new fields, goes through derper, and raises the trust question "can a peer's advertised port be trusted". Forwards are also short-lived foreground processes; broadcast information may be stale seconds later | Not in this round |
| Q5 | The target cannot see the original client address | Both user space and kernel mode see B's address because SNAT is mandatory; inherent to the feature. A real impact for scenarios that audit via access logs | Document it; register an optional `--proxy-protocol` for HTTP/TCP targets as a follow-up |
| Q6 | Should connection / session caps exist and be exposed as options | TCP connections have a natural lifetime; a cap mainly guards against misuse. An unbounded UDP session table is a real memory-exhaustion path | No TCP cap, one option fewer; the UDP session cap is fixed at 512 (FR-4.3) as a safety property rather than a setting; revisit if it proves insufficient |
| Q7 | Ports <1024 | `noeio forward` needs root to reach the RPC socket (FR-6.4), so it can bind them; but that lets a mistyped rule occupy a system port | Allow it, with a `warn` at bind time |
| Q8 | Should there be a "skip the daemon" bypass | E.g. `--overlay-ip 110.20.0.1` so non-root users can run overlay-side forwards. Gain: one RPC and one root requirement fewer; cost: bypasses the relay refusal (no `nics[].peer_ips`), and without the daemon forwarding is meaningless anyway | **Not provided** (FR-6.3). For non-root scenarios the right fix is loosening the socket permission model, which is a separate requirement |
| Q9 | Which nic does the `noeio` keyword pick when there are several virtual nics | The first one: a short command, but `NicManager` is unordered and it silently exposes to one network only; all of them: matches the literal meaning of "expose to noeio", larger exposure on multi-network nodes | **Bind all of them** (FR-6.2a). To narrow, write a specific IP; the banner lists every bound address, so the exposure is visible at a glance |

## 11. Milestones

| Milestone | Content |
| --- | --- |
| **M1** | Rule parsing and validation (FR-1), address derivation and all-or-nothing binding (FR-2), full lifecycle and signal handling (FR-3), TCP user-space proxy, banner/log/exit summary (FR-5.1–5.4), the `ListVirtualNics` RPC (FR-6.2), security notices (FR-8). Acceptance: AC-1..AC-19 |
| **M2** | UDP forwarding and the session table (FR-4). Acceptance: AC-20 |
| **M3** | User documentation `docs/port-forward.md`; update `docs/subnet-router.md` on the LAN-initiated direction (FR-7.3); bilingual README Features; `SIGUSR1` counter dump (FR-5.5) |

## 12. Changes to existing code

| # | Location | Change | Milestone |
| --- | --- | --- | --- |
| 1 | `noeio/src/cli.rs:13` | `Command` gains `Forward { listen: ListenSpec, target: SocketAddrV4, proto: Proto, allow_from: Vec<String> }`. `ListenSpec` is `enum { Noeio(u16), Lan(u16), Addr(SocketAddrV4) }` implementing `FromStr` for clap's `value_parser`; `proto` is a `ValueEnum` defaulting to `tcp`; usage and exit codes in §6 | M1 |
| 2 | New `noeio/src/forward.rs` (or a `forward/` directory: `rule.rs` parsing/validation, `bind.rs` address derivation, `proxy.rs` TCP proxy, `stats.rs` counters) | All logic of the forwarding process. **Outside `daemon/`**: it is not part of the daemon | M1 |
| 3 | `noeio/src/main.rs:23` | New `Command::Forward` arm in the `match`: connect RPC → `ListVirtualNics` → validate → bind → banner → `select!` on signals → abort → summary | M1 |
| 4 | `noeio/src/main.rs:118` | Extend `wait_for_shutdown_signal` to SIGINT/SIGTERM/SIGHUP and the five Windows console events; move it to `noeio/src/signal.rs`, shared by `boot` and `forward` | M1 |
| 5 | `noeio-proto/protos/noeio/v1/virtual_nic.proto:15` | `VirtualNicService` gains the `ListVirtualNics` method and the three messages `VirtualNicEntry` etc. | M1 |
| 6 | `noeio/src/rpc/service/nic.rs` | Implement `list_virtual_nics`: iterate `NicManager` (`tun_name`, `ip`), take `network_id` from `host_info.peers` by `peer_id`, `peer_ips` from `Router::ips()` | M1 |
| 7 | `noeio/src/rpc/client.rs` | New `list_virtual_nics()` returning `Vec<VirtualNicEntry>` | M1 |
| 8 | `noeio/src/daemon/routes.rs:284` | Extract `local_addrs(exclude) -> Vec<Ipv4Addr>` from `local_lans` for FR-2.2 | M1 |
| 9 | `noeio/tests/forward_lifecycle.rs` (new) | The process-level signal tests of AC-19 | M1 |
| 10 | New `docs/port-forward.md`; edit `docs/subnet-router.md:200`, `README.md:9` Features, `README.zh-CN.md:16` | User documentation; the direction description mentioned in FR-7.3 | M3 |

**Places that do not change**, each confirmed:

- `noeio/src/config.rs`, `config.toml.example` — there is no forwarding configuration.
- `noeio/src/daemon.rs`, `daemon/reconciler.rs`, `daemon/nat.rs` — the daemon holds no forwarding state; `shutdown` needs no change.
- `noeio-proto/protos/common/v1/host_info.proto` — port forwarding is not broadcast.
- All of `noeio-derp` — no new control-plane messages.
- `noeio/src/daemon/router.rs:133` `allowed_source` — the inner source addresses of both directions are naturally valid (§5.1 point 3).
- `noeio/src/interface/virtual_nic.rs` — the TUN configuration is unchanged.
- All of `noeio-net-route` — the user-space design touches neither the routing table nor nftables.

## 13. Explicitly out of scope

- **A Linux kernel-mode path (nftables DNAT/SNAT).** §5.3 has shown that the performance gain is masked by boringtun, that the source address is no different, that MSS needs clamping, and that extra sysctls such as `route_localnet` are required; the decisive reason is its incompatibility with §2.1's lifecycle goal: nftables rules and `ip_forward` do not vanish with the process, `kill -9` inevitably leaves residue, and subnet routing's original-value saving and start-up sweep would have to be reintroduced. A design draft of the kernel-mode approach (prerouting nat chain, `nat`/`immediate`/`tcp dport` expression encoding, narrowing with `ip daddr` when masquerading toward the TUN) is preserved in commit `0ce6195` and can be retrieved if it is ever built as a separate feature with explicit cleanup semantics.
- IPv6 forwarding; daemon-hosted forwarding and a configuration-file entry point; announcing / auto-discovering forwarding rules across the network (Q4); identity-based ACLs; preserving the original client source address (Q5); graceful connection draining; automatic rebinding on address change (Q2); a bypass that skips the daemon (Q8); SOCKS/HTTP-proxy-style dynamic targets; turning a forwarding rule into Kubernetes-Service-style multi-target load balancing.
