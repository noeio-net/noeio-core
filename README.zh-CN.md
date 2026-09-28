# Noeio Core

![Rust](https://img.shields.io/badge/Rust-1.94%2B-black?logo=rust&logoColor=white)

[English](README.md) | 简体中文

Noeio 是一个可私有化部署、节点无状态且轻量的三层（Layer-3）Mesh 组网系统。它通过虚拟网卡将分布在不同局域网、NAT 之后和云上的机器连成一个私有网络。所有数据面组件都运行在你自己的基础设施上，不依赖任何第三方协调服务。

## 特性

- **三层虚拟网卡** —— 节点通过本地 `noeio0` 接口加入私有子网，任意 IP 流量开箱即用
- **WireGuard 加密数据面** —— 任意两节点之间都有一条独立密钥的专属隧道，基于 boringtun
- **NAT 穿透** —— 基于 STUN 的地址发现与 UDP 打洞，跨 NAT 建立直连
- **最低延迟选路** —— 对每条候选路径（局域网与公网）做 ping/pong RTT 探测，流量始终走最快的一条，切换带防抖
- **自部署中继兜底** —— 无直连路径时回退到你自己的 derper 中继，由网络级 token 认证保护
- **子网路由** —— Linux 节点可宣告 LAN 网段并为未安装 agent 的主机转发与 SNAT；所有平台都可以接受这些路由。详见 [docs/subnet-router.md](docs/subnet-router.md)
- **端口转发** —— `noeio forward` 在前台、纯用户态地把一个端口跨 overlay/LAN 边界双向暴露，三平台一致。详见 [docs/port-forward.md](docs/port-forward.md)
- **跨平台** —— 支持 Linux、macOS 和 Windows

## 初衷

我正是在使用 [Tailscale](https://github.com/tailscale/tailscale) 之后，发现它功能强大且极其好用。出于学习的目的，我去了解了它的底层实现原理，发现恰好与我当时正在学习的 Kubernetes CNI 组件有相似之处。于是，为了更好地学习和补充网络知识，我决定自己动手写一个 Overlay Network。我知道这个项目不一定能比肩 Tailscale，但光是这个学习的过程就已让我收获颇丰。我希望把它作为我的学习成果分享出来，以向社区分享和讨论更多知识与有趣的想法。另外也受到 Sandbox 这种产品形态的启发，让我决定将 Noeio 的节点设计成一种无状态的方式，以符合 Sandbox 的生命周期。

## 与 Tailscale 的区别

Noeio 是一个纯数据面组件：只负责组网本身，不内置 Tailscale 那样的账户、身份认证和 ACL 体系。这使它非常适合私有化部署。如果需要支持多租户，则需在其之上搭配你自己的控制面。

此外，Noeio 的节点天然是无状态、非持久注册的（Tailscale 的节点在未开启 Ephemeral 时是有状态且持久注册的）。这让 Noeio 节点非常轻量，可以随时部署和释放，因此对 Sandbox 这类使用场景非常友好。

## 架构

![Noeio 架构图](docs/images/screenshot-20260811-152405.png)

每个节点运行一个 noeio daemon，向自部署的 derper 上报自己的地址和 NAT 类型；derper 通过 SyncRoute 把各节点的路径候选广播回去。节点间的流量在存在直连路径时走 WireGuard 加密的直连，否则回退到经 derper 中继转发。

## 安装

### noeio

在每个需要加入虚拟网络的节点上安装 noeio daemon：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://noeio.net/install.sh | sh
```

该方式安装的是预编译二进制，无需 Rust 工具链。Windows 用户，或希望从源码编译的，参见 [构建](#构建)。

### derper

derper 需要部署在一台所有节点都能访问到的机器上，一般是一台带公网 IP 的云厂商机器。最快的方式是使用 [快速开始](#快速开始) 中的 Docker 镜像；如需从源码安装二进制：

```bash
cargo install --git https://github.com/CeerDecy/noeio-core noeio-derp
```

从源码编译需要 [Rust 工具链](https://rustup.rs/) 和 `protoc`（protobuf 编译器）——Linux 用 `apt install protobuf-compiler`，macOS 用 `brew install protobuf`；Windows 可用 `choco install protoc` / `scoop install protobuf`，或从 [protobuf releases](https://github.com/protocolbuffers/protobuf/releases) 下载 `protoc-*-win64.zip` 并加入 `PATH`。

## 快速开始

1. 启动 derper 中继。derper 需要部署在一台所有节点都能访问到的机器上，一般是一台带公网 IP 的云厂商机器。

   Docker 方式：

   ```bash
   docker run -d --name noeio-derper -p 8080:8080/udp --rm noeio/noeio-derp:latest
   ```

   或二进制方式：

   ```bash
   cargo install --git https://github.com/CeerDecy/noeio-core noeio-derp && noeio-derp boot
   ```

2. 生成 derper 认证 token。`--network` 是一个用于标识虚拟网络的 UUID，可以根据自己的需要随意填写其他 UUID；持有同一 UUID token 的节点会加入同一个虚拟网络。

   Docker 方式：

   ```bash
   docker exec noeio-derper /usr/local/bin/noeio-derp token create --network 25fe8468-b310-43ed-96be-495641eececd --ttl 0
   ```

   或二进制方式：

   ```bash
   noeio-derp token create --network 25fe8468-b310-43ed-96be-495641eececd --ttl 0
   ```

3. 在每个需要加入虚拟网络的节点上安装并启动 noeio daemon。直接在命令行传入 derper 地址和第二步生成的 token——将 `192.168.0.1:8080` 换成你自己的 derper 地址，STUN 服务器选一个离你较近的：

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://noeio.net/install.sh | sh

   noeio boot \
     --stun stun.chat.bilibili.com:3478 \
     --derper-server 192.168.0.1:8080 \
     --derper-token <第二步生成的 token>
   ```

   这些参数是追加式的，且仅对本次运行生效：它们只会合并进内存中的配置，不会修改你的配置文件。如需指定多个 STUN 服务器或多个 derper，参见 `noeio boot --help`。

   > 非 Linux 宿主机不建议用 Docker 运行 noeio 本体：每个操作系统的虚拟网卡创建方式不同，默认 Docker 镜像提供的是 Linux 的部署。

   <details>
   <summary>更希望用配置文件？改为在 <code>~/.noeio/config.toml</code> 中配置。</summary>

   如果需要持久化配置，可以把同样的内容写进 `~/.noeio/config.toml`（Windows 为 `%USERPROFILE%\.noeio\config.toml`，例如 `C:\Users\<用户名>\.noeio\config.toml`），然后直接执行 `noeio boot`。注意将 `address` 替换为你自己的 derper 地址，将 `token` 替换为第二步生成的 token。可以参考项目根目录下的 [`config.toml.example`](config.toml.example)：

   ```toml
   [stun]
   servers = ["stun.chat.bilibili.com:3478"] # 换成离你较近的 STUN 服务器

   [[derper.servers]]
   address = "192.168.0.1:8080" # 换成你自己的 derper 地址
   token = "<填入第二步生成的 token>"
   ```

   每增加一个 derper 就追加一个 `[[derper.servers]]` 段。若该文件尚不存在，直接执行 `noeio boot` 会用空的默认值创建它。上面的命令行参数与配置文件可以共用：参数中的条目会追加到配置文件已声明的内容之后，而配置文件中已存在的地址会保留原有 token，除非参数提供了新的 token。

   </details>

4. 创建虚拟网卡并加入网络。`--ip` 可以自定义，建议使用 `100.64.0.0/10` 网段，同一个网络下每个节点的 IP 不能相同。`--network` 需要与第二步命令中的 network 参数保持一致。

   节点 A 上执行：

   ```bash
   noeio create vnic --ip 100.64.0.1 --network 25fe8468-b310-43ed-96be-495641eececd
   ```

   节点 B 上执行：

   ```bash
   noeio create vnic --ip 100.64.0.2 --network 25fe8468-b310-43ed-96be-495641eececd
   ```

5. 在节点 A 上通过虚拟网络 ping 节点 B：

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

   ping 通即表示两个节点已经连上，组网完成。

## 构建

### 在 Docker 中构建各平台二进制

只需本机安装并启动 Docker（Linux 容器），无需安装或修改本机 Rust 环境：

```bash
make binaries
# 也可只构建指定平台
make binaries PLATFORMS="linux-amd64 linux-arm64 windows-amd64"
```

仅构建 `noeio`。发布文件名中，x86_64 使用 `amd64`，Linux ARM64 使用 `arm64`，
macOS ARM64 使用 `aarch64`。两种系统的 Rust ARM64 编译目标均使用 `aarch64`。
默认在 `build/out/` 下生成：

- `noeio-linux-amd64`
- `noeio-linux-arm64`
- `noeio-macos-amd64`
- `noeio-macos-aarch64`
- `noeio-windows-amd64.exe`

每个二进制附带同名 `.sha256` 校验文件。Linux 使用 musl，Windows 使用 GNU/MinGW，
macOS 最低目标版本为 11.0。

构建 macOS 目标需要在 Mac 上安装 Xcode 或 Command Line Tools。
脚本统一通过以下命令自动获取 SDK 路径：

```bash
xcrun --sdk macosx --show-sdk-path
```

仅构建 Linux/Windows 目标时不需要 `xcrun` 或 Apple SDK。
SDK 和项目源码只读挂载，编译工具安装在 Docker 镜像内，依赖及编译缓存保存在
`noeio-cross-cache-v1` Docker volume 中，不使用本机的 Cargo 配置、Rust 工具链或 `target/`。
首次运行需要联网下载镜像和依赖，后续运行复用缓存。不要同时运行多个使用同一缓存卷的构建。
存在 `Cargo.lock` 时复用它，否则在容器内生成；本次构建的锁文件保存到 `build/out/Cargo.lock`。
可用 `BUILD_IMAGE`、`BUILD_CACHE` 指定镜像名和缓存卷名；这些变量可作为 `make` 参数传入；脚本选项见 `./scripts/build-binaries.sh --help`。
脚本仅生成二进制，不包含 macOS 签名/公证或 Windows TUN 驱动等运行时组件。

### 本机编译

构建 derper 二进制：

```bash
cargo build --release -p noeio-derp
```

或构建多平台 Docker 镜像并推送：

```bash
REGISTRY=<your image registry url> MODEL=noeio-derp make build
```

构建 noeio daemon 二进制：

```bash
cargo build --release -p noeio
```

## Roadmap

- [ ] `NoeioPacket` 的零拷贝改造
- [x] 子网路由支持（Linux 宣告方，见 [docs/subnet-router.md](docs/subnet-router.md)）
- [ ] 多虚拟网络支持

## License

本项目基于 [Apache License 2.0](LICENSE) 开源。
