# Noeio Core

![Rust](https://img.shields.io/badge/Rust-1.94%2B-black?logo=rust&logoColor=white)

English | [简体中文](README.zh-CN.md)

Noeio is a self-hostable layer-3 mesh networking system with stateless, lightweight nodes. It connects machines across LANs, NATs, and clouds into one private network over a virtual NIC. All data-plane components run on your own infrastructure; no third-party coordination service is involved.

## Features

- **Layer-3 virtual NIC** — peers join a private subnet through a local `noeio0` interface; any IP traffic just works
- **WireGuard-encrypted data plane** — a dedicated tunnel with per-peer keys between every pair of nodes, powered by boringtun
- **NAT traversal** — STUN-based address discovery and UDP hole punching for direct connections across NATs
- **Lowest-latency path selection** — every candidate path (LAN and public) is probed with ping/pong RTT sampling, and traffic always takes the fastest one, with debounced switching
- **Self-hosted relay fallback** — when no direct path exists, traffic falls back to your own derper relay, guarded by network-scoped token auth
- **Subnet routing** — a Linux node can advertise LAN CIDRs and forward/SNAT for hosts that run no agent; every platform can accept them. See [docs/subnet-router.md](docs/subnet-router.md)
- **Port forwarding** — `noeio forward` exposes one port across the overlay/LAN boundary in either direction, in the foreground and in user space, on every platform. See [docs/port-forward.md](docs/port-forward.md)
- **Cross-platform** — runs on Linux, macOS, and Windows

## Motivation

It was after using [Tailscale](https://github.com/tailscale/tailscale) that I discovered how powerful and remarkably easy to use it is. Out of a desire to learn, I dug into the principles behind its implementation, and found they happened to resemble the Kubernetes CNI plugins I was studying at the time. So, to learn more and round out my networking knowledge, I decided to write an overlay network of my own. I know this project may never rival Tailscale, but the learning process alone has been richly rewarding. I hope to share it as the outcome of that learning, to exchange and discuss more knowledge and interesting ideas with the community. Noeio is also inspired by sandbox-style products: their short-lived nature led me to design Noeio's nodes as stateless, so they fit naturally into a sandbox's lifecycle.

## Differences from Tailscale

Noeio is a pure data-plane component: it only handles networking itself, and ships none of the account, identity, or ACL machinery that Tailscale builds in. This makes it well suited for self-hosted deployment. If you need multi-tenancy, pair it with your own control plane on top.

In addition, Noeio nodes are stateless and non-persistently registered by nature (Tailscale nodes are stateful and persistently registered unless ephemeral mode is enabled). This keeps Noeio nodes extremely lightweight — they can be deployed and released at any time, which makes Noeio a great fit for sandbox use cases.

## Architecture

![Noeio architecture](docs/images/screenshot-20260811-152405.png)

Each node runs a noeio daemon that reports its addresses and NAT type to the self-hosted derper; the derper broadcasts every peer's route candidates back via SyncRoute. Traffic between nodes takes a WireGuard-encrypted direct path whenever one exists, and falls back to relaying through the derper otherwise.

## Install

### noeio

Install the noeio daemon on every node that joins the virtual network:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://noeio.net/install.sh | sh
```

This installs a prebuilt binary, so no Rust toolchain is required. On Windows, or to build from source instead, see [Build](#build).

### derper

The derper must be deployed on a machine reachable by every node — typically a cloud instance with a public IP. The quickest way to run one is the Docker image shown in [Quick start](#quick-start); to install the binary from source instead:

```bash
cargo install --git https://github.com/CeerDecy/noeio-core noeio-derp
```

Building from source needs the [Rust toolchain](https://rustup.rs/) and `protoc` (the protobuf compiler) — `apt install protobuf-compiler` on Linux, `brew install protobuf` on macOS; on Windows use `choco install protoc` / `scoop install protobuf`, or download a `protoc-*-win64.zip` from the [protobuf releases](https://github.com/protocolbuffers/protobuf/releases) and add it to your `PATH`.

## Quick start

1. Start a derper relay. The derper must be deployed on a machine reachable by every node — typically a cloud instance with a public IP.

   With Docker:

   ```bash
   docker run -d --name noeio-derper -p 8080:8080/udp --rm noeio/noeio-derp:latest
   ```

   Or with the binary:

   ```bash
   cargo install --git https://github.com/CeerDecy/noeio-core noeio-derp && noeio-derp boot
   ```

2. Create a derper auth token for your network. `--network` takes a UUID that identifies a virtual network — feel free to use any UUID of your own; nodes holding a token for the same UUID join the same network.

   With Docker:

   ```bash
   docker exec noeio-derper /usr/local/bin/noeio-derp token create --network 25fe8468-b310-43ed-96be-495641eececd --ttl 0
   ```

   Or with the binary:

   ```bash
   noeio-derp token create --network 25fe8468-b310-43ed-96be-495641eececd --ttl 0
   ```

3. Install and start the noeio daemon on every node that should join the virtual network. Pass your derper address and the token from step 2 directly on the command line — replace `192.168.0.1:8080` with your own derper address, and pick a STUN server near you:

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://noeio.net/install.sh | sh

   noeio boot \
     --stun stun.chat.bilibili.com:3478 \
     --derper-server 192.168.0.1:8080 \
     --derper-token <token from step 2>
   ```

   These flags are additive and per-run: they are merged into the configuration in memory and never modify your config file. See `noeio boot --help` for how to pass multiple STUN servers or derpers.

   > Running the noeio daemon itself in Docker is not recommended on non-Linux hosts: virtual NIC creation differs per operating system, and the default Docker image ships the Linux flavor.

   <details>
   <summary>Prefer a config file? Configure it in <code>~/.noeio/config.toml</code> instead.</summary>

   For a persistent setup, put the same values in `~/.noeio/config.toml` (on Windows: `%USERPROFILE%\.noeio\config.toml`, e.g. `C:\Users\<username>\.noeio\config.toml`) and run a bare `noeio boot`. Make sure to replace `address` with your own derper's address and `token` with the token created in step 2. You can use [`config.toml.example`](config.toml.example) in the project root as a reference:

   ```toml
   [stun]
   servers = ["stun.chat.bilibili.com:3478"] # change to a STUN server near you

   [[derper.servers]]
   address = "192.168.0.1:8080" # change to your own derper address
   token = "<replace with the token from step 2>"
   ```

   Add another `[[derper.servers]]` block per extra derper. A bare `noeio boot` creates this file with empty defaults on first run if it does not exist yet. The boot flags above still work alongside a config file: entries they add are appended to what the file already declares, and an address already present keeps its configured token unless the flag supplies a new one.

   </details>

4. Create a virtual NIC and join the network. `--ip` is up to you — the `100.64.0.0/10` range is recommended, and every node in the same network must use a different IP. `--network` must match the UUID used in step 2.

   On node A:

   ```bash
   noeio create vnic --ip 100.64.0.1 --network 25fe8468-b310-43ed-96be-495641eececd
   ```

   On node B:

   ```bash
   noeio create vnic --ip 100.64.0.2 --network 25fe8468-b310-43ed-96be-495641eececd
   ```

5. Try pinging node B from node A over the virtual network:

   ```bash
   ping 100.64.0.2
   ```

   ```text
   64 bytes from 100.64.0.2: icmp_seq=1 ttl=64 time=34.155 ms
   64 bytes from 100.64.0.2: icmp_seq=2 ttl=64 time=28.900 ms
   64 bytes from 100.64.0.2: icmp_seq=3 ttl=64 time=23.171 ms
   64 bytes from 100.64.0.2: icmp_seq=4 ttl=64 time=31.013 ms
   64 bytes from 100.64.0.2: icmp_seq=5 ttl=64 time=26.343 ms
   64 bytes from 100.64.0.2: icmp_seq=6 ttl=64 time=29.072 ms
   64 bytes from 100.64.0.2: icmp_seq=7 ttl=64 time=22.765 ms
   ```

   If the ping succeeds, the two nodes are connected — you're all set.

## Build

### Build binaries in Docker

Start Docker with Linux containers; no host Rust installation or changes are needed:

```bash
make binaries
# Or select platforms
make binaries PLATFORMS="linux-amd64 linux-arm64 windows-amd64"
```

Only `noeio` is built. Release names use `amd64` for x86_64, `arm64` for
Linux ARM64, and `aarch64` for macOS ARM64. Rust ARM64 compilation targets
use `aarch64` on both operating systems. The default outputs in `build/out/` are:

- `noeio-linux-amd64`
- `noeio-linux-arm64`
- `noeio-macos-amd64`
- `noeio-macos-aarch64`
- `noeio-windows-amd64.exe`

Each binary has a matching `.sha256` checksum file. Linux uses musl, Windows
uses GNU/MinGW, and the macOS deployment target is 11.0.

macOS targets require a Mac with Xcode or Command Line Tools installed.
The script always locates the SDK automatically using:

```bash
xcrun --sdk macosx --show-sdk-path
```

Building only Linux/Windows targets does not require `xcrun` or an Apple SDK.
The SDK and sources are mounted read-only. Tools live in the Docker image;
dependencies and build artifacts are cached in the `noeio-cross-cache-v1` Docker
volume, without using the host Cargo configuration, Rust toolchain, or `target/`.
The first build downloads the image and dependencies; later builds reuse caches.
An existing `Cargo.lock` is reused; otherwise one is generated inside the container.
The lockfile used for the build is saved to `build/out/Cargo.lock`.
Do not run concurrent builds sharing the same cache volume. Override `BUILD_IMAGE`
or `BUILD_CACHE` as Make variables to change their names; see
`./scripts/build-binaries.sh --help` for script details.
The script produces binaries only; macOS signing/notarization and runtime components
such as Windows TUN drivers are not included.

### Native build

Build the derper binary:

```bash
cargo build --release -p noeio-derp
```

Or build and push a multi-platform Docker image:

```bash
REGISTRY=<your image registry url> MODEL=noeio-derp make build
```

Build the noeio daemon binary:

```bash
cargo build --release -p noeio
```

## Roadmap

- [ ] Zero-copy refactor of `NoeioPacket`
- [x] Subnet routing support (Linux advertisers; see [docs/subnet-router.md](docs/subnet-router.md))
- [ ] Multiple virtual network support

## License

This project is licensed under the [Apache License 2.0](LICENSE).
