# 端口转发需求文档

状态：草案
日期：2026-09-24
相关代码：`noeio`、`noeio-proto`、`noeio-net-route`
关联文档：[子网路由](./subnet-router-requirements.en.md)、[docs/subnet-router.md](../subnet-router.md)

---

## 1. 背景

子网路由已经解决了「整个网段的机器不装 agent 也能被访问」的问题：Advertiser 宣告一个 CIDR，Consumer 装路由，流量整段进隧道。代价是它只在 Linux 上可用（需要 nftables + `ip_forward`），并且只支持 overlay → LAN 一个方向，LAN 侧主动发起的连接被明确列为非目标（`docs/subnet-router.md` 最后一节）。

很多场景其实不需要整段网络，只需要一个端口：NAS 的 8080、打印机的 631、内网数据库的 3306、跑在 B 本机 loopback 上的服务。反过来，局域网里那台不装 agent 的机器也常常只需要访问 overlay 里的某一个服务。

端口转发正好覆盖这两个缺口，而且——这是本文档最重要的结论——它比子网路由**简单得多**，因为它可以完全在用户态实现，不需要任何内核 NAT，三个平台一套代码。

## 2. 目标与非目标

### 2.1 目标

- B 节点可以声明一组端口转发规则，每条规则把「某一侧的某个端口」映射到「另一侧的某个地址:端口」。
- **方向一（overlay → LAN）**：C 访问 `B_overlay_ip:8080`，B 转发到同局域网的 A `192.168.10.7:80`。A 不装 agent、不需要任何配置。
- **方向二（LAN → overlay）**：A 访问 `B_lan_ip:9090`，B 转发到 overlay 里的 C `110.20.0.9:22`。A 不装 agent、不需要路由。
- 目标地址也可以是 B 自己的 `127.0.0.1`，把只监听 loopback 的本机服务暴露给 overlay。
- 规则支持配置文件、CLI 参数、运行时 RPC 三种入口，改动不需要重启进程。
- macOS、Windows、Linux 三个平台**能力完全对等**。

### 2.2 非目标（本轮不做）

- **UDP 转发**：见 FR-4，放到 M2。M1 阶段 `proto = "udp"` 直接报错拒绝，不静默降级。
- **IPv6**：与子网路由保持一致，`noeio-net-route` 和 `smoltcp` 配置目前都是 IPv4-only（`Cargo.toml:33` features = `proto-ipv4`）。
- **转发规则的网络内广播 / 自动发现**：C 想知道 B 开了哪些转发，本轮只能靠人告知或看 B 上的 `noeio forward list`。理由见 §9 Q4。
- **基于身份的 ACL**：与子网路由现状一致，只有基于源 IP 的粗粒度 `allow_from`。
- **保留原始客户端源地址**（PROXY protocol / TPROXY）：见 §9 Q5。
- **连接优雅排空**：删除规则会立刻关闭在途连接，不等它自然结束。

## 3. 术语

| 术语 | 含义 |
| --- | --- |
| 转发规则（Forward rule） | 一条 `(监听侧, 协议, 监听端口) → (目标地址, 目标端口)` 的映射，只在本节点生效 |
| 监听侧（listen side） | `overlay`：监听在本节点的 noeio 虚拟地址上，只有 overlay 对端能连；`lan`：监听在物理网卡地址上，只有物理局域网能连 |
| 目标（target） | 规则转发到的 `addr:port`。`overlay` 侧监听时通常是 LAN 地址或 loopback，`lan` 侧监听时通常是对端的 overlay 地址 |
| 用户态转发 | B 在监听侧 accept 连接，向目标另开一条连接，双向拷贝字节流。两条连接各自是独立的 TCP 连接 |
| 内核态转发 | Linux 上用 nftables 的 DNAT + SNAT 在内核里改写包，不经用户态 socket |

## 4. 场景

```
   A  192.168.10.7                B  overlay 110.20.0.1            C  overlay 110.20.0.9
   不装 agent                      lan 192.168.10.3                  accept_routes 无关
        |                               |                                  |
        +------ 物理局域网 192.168.10.0/24 ------+                          |
                                        |                                  |
                                        +------ overlay (WireGuard/UDP) ---+

   方向一：C 上 `curl http://110.20.0.1:8080`
           -> B 的 overlay 侧监听器 accept
           -> B 向 192.168.10.7:80 另开连接
           -> A 的访问日志里看到的源地址是 192.168.10.3（B 的 LAN 地址）

   方向二：A 上 `ssh -p 9090 192.168.10.3`
           -> B 的 LAN 侧监听器 accept
           -> B 向 110.20.0.9:22 另开连接（走 B 自己的 TUN）
           -> C 上看到的源地址是 110.20.0.1（B 的 overlay 地址）
```

## 5. 可行性评估

### 5.1 结论

两个方向都可行，而且**不需要动任何协议、不需要动 derper、不需要动 `PeerInfo`**。这是它和子网路由最大的区别：子网路由必须让全网知道「谁代理哪个网段」，所以有 FR-2 一整章的广播、撤销、墓碑逻辑；端口转发是纯本地行为，C 只是在连一个普通的 `ip:port`，A 也只是在连一个普通的 `ip:port`，两边都不需要知道背后发生了什么。

三件现成的能力让它几乎是白拿的：

1. **在 overlay 地址上监听是可行的。** TUN 用 `/32` 掩码配置了本节点的 overlay 地址（`noeio/src/interface/virtual_nic.rs:74`），它是一个正常的本机地址。守护进程解封装后把包写进 TUN（`noeio/src/daemon.rs:845` `write_to_nic`），目的地址是自己的包由主机协议栈正常交付给监听 socket。一个普通的 `TcpListener::bind((overlay_ip, port))` 就够了。
2. **从 B 主动连 overlay 对端也是可行的。** reconciler 已经为每个对端装了 `/32` 主机路由（`noeio/src/daemon/reconciler.rs:70`），出口是 TUN。所以 B 上一次普通的 `TcpStream::connect(C_overlay:22)` 会被内核路由进 TUN，被 `process_outbound`（`daemon.rs:415`）读到、查表、封装发给 C。源地址由内核按出口网卡选中 TUN 的地址，也就是 B 的 overlay 地址。
3. **反欺骗检查不用改。** `Router::allowed_source`（`daemon/router.rs:133`）要求入向包的内层源地址属于对端自己的 overlay 地址或它宣告的网段。方向一里 C 发来的包源地址是 C 自己的 overlay 地址；方向二里 B 发给 C 的包源地址是 B 自己的 overlay 地址。两种情况都天然通过，不像子网路由那样需要把检查放宽成 AllowedIPs 语义。

### 5.2 关于「Mac/Windows 用户态、Linux 走 SNAT/DNAT」的评估

这个方案技术上完全可行，FR-8 给出了完整的内核态规则设计。但评估下来，**建议三个平台统一走用户态，把 Linux 内核态降级为后续可选的加速开关**，理由如下。

最关键的一条是性能论据本身站不住：**转发的每一个字节本来就已经过了一趟用户态。** noeio 的 WireGuard 是 boringtun（`Cargo.toml:10`），加解密在用户态完成，包要经过 `process_outbound` / `handle_delivery` 读写 TUN。内核 DNAT 省掉的是代理的那一次 `copy_bidirectional`，而这条路径上本来就有 AEAD 加解密和 TUN 读写。省掉一次 memcpy，留着一次 ChaCha20-Poly1305，收益在噪声级别。真正的瓶颈不在这里。

其次，源地址这条通常支持 DNAT 的论据在这里也不成立。方向一里 A 没有回到 overlay 的路由，方向二里 C 没有回到物理局域网的路由，所以**两个方向都必须做 SNAT**。也就是说内核态下目标看到的源地址同样是 B 的地址，和用户态的可观测行为完全一致——没有任何区别。

逐项对比：

| 维度 | 用户态代理 | Linux 内核 DNAT/SNAT |
| --- | --- | --- |
| 新增代码量 | 约 300 行，标准 tokio 代理 | 约 200 行 nftables 表达式编码 + 字节级测试，再加一套收敛/清理逻辑 |
| 平台覆盖 | 三平台一套代码 | 仅 Linux，另两个平台仍需用户态，等于维护两条路径 |
| 权限要求 | 绑端口（<1024 需要 root，守护进程已有） | `CAP_NET_ADMIN` + 全局 `net.ipv4.ip_forward` + `nft_masq`/`nf_nat`/`nft_ct` 模块 |
| 容器内可用 | 是 | 受限，NAT 模块在非特权容器里无法自动加载（`docs/subnet-router.md` 已记录） |
| MSS / MTU | **不需要处理**。代理两端各自是独立 TCP 连接，overlay 侧按 TUN 的 1411 协商，LAN 侧按 1500 协商 | 需要 MSS clamp mangle 链，否则出现「ping 通但大文件卡死」 |
| 残留状态 | **无**。监听器随进程消失，比子网路由依赖的「TUN 非持久化」更彻底 | `ip_forward` 和 nftables 表都会残留，需要启动清扫 + 原值保存（§12 已验证） |
| 可观测性 | `ss -ltn` 能看到监听器，每条规则的连接数/字节数/错误数都是现成的 | 无监听器可看；已知的静默失败模式：conntrack 先绑定了别的 NAT 映射，noeio 的规则变成空操作且无日志 |
| 吞吐 | 多一次 memcpy | 理论更优，但被同路径的 boringtun 加解密掩盖 |
| 目标为 `127.0.0.1` | 直接可用 | 需要额外打开 `route_localnet` |
| 协议覆盖 | TCP 完备；UDP 需要会话表（FR-4） | TCP/UDP 都由 conntrack 负责 |

唯一明确属于内核态的优势是 UDP：conntrack 免费提供会话跟踪，而用户态要自己写一张带 TTL 的会话表。但那张表是有界的、约 150 行的确定性工作（FR-4），换来的是三平台对等，仍然划算。

因此本文档把用户态定为默认且唯一的 M1 实现，内核态写成 FR-8 的可选模式（`mode = "kernel"`），排在 M3 之后，并且只在实测证明确有吞吐瓶颈时才启用。§9 Q1 把这个取舍登记为待决问题，保留改主意的空间。

## 6. 功能需求

### FR-1 规则配置

- **FR-1.1** 配置文件新增 `[[forward]]` 数组段，落在 `noeio/src/config.rs:5` 的 `Config` 上（与现有 `stun` / `derper` / `router` 同级）：

  ```toml
  [[forward]]
  # 规则名，用于日志、CLI 和 RPC 中标识这条规则；同一节点内唯一。
  name = "nas-web"
  # 监听侧："overlay"（只有 noeio 对端能连）或 "lan"（物理局域网能连）。
  listen = "overlay"
  proto = "tcp"
  # 监听端口。
  port = 8080
  # 转发目标。
  to = "192.168.10.7:80"
  # 可选：覆盖默认的绑定地址。留空时按 FR-2.2 推导。
  bind_addr = ""
  # 可选：只接受来自这些 CIDR 的连接，空表示不限制。
  allow_from = []
  # 可选：本规则同时在途的最大连接数，0 表示用默认值 256。
  max_conns = 0
  ```

- **FR-1.2** CLI 参数遵循现有 `--advertise-routes` 的风格（`noeio/src/cli.rs:37`），支持在 `noeio boot` 上追加：`--forward overlay/tcp/8080/192.168.10.7:80`，可用逗号分隔多条。合并逻辑照 `config.rs:148` 的 `append_routes` 写。
- **FR-1.3** 运行时增删不重启进程，见 FR-6。
- **FR-1.4** 校验规则，全部在一个函数里，和 `daemon/routes.rs:134` `validate_advertisement` 的「单一闸门」模式保持一致：
  - `name` 非空、同节点内唯一；
  - `port` 在 `[1, 65535]`；
  - `to` 可解析为 `addr:port`，且端口非 0；
  - `proto` 目前只接受 `tcp`（FR-4 之前 `udp` 报明确错误）；
  - **拒绝自环**：`to` 解析后等于这条规则自己的监听 `addr:port`；
  - **拒绝 overlay→overlay 中继**：`listen = "overlay"` 且 `to` 的地址落在已知的 overlay 范围内。这会让 B 变成两个对端之间的中继，是本轮范围外的策略问题，报专门的错误而不是静默允许；
  - `allow_from` 每项可解析为 CIDR。
- **FR-1.5** 校验失败的处理方式和子网路由一致：启动阶段（配置文件 / CLI）失败则进程带明确错误退出；RPC 阶段失败返回 `INVALID_ARGUMENT`，守护进程继续运行。

### FR-2 监听器的建立

- **FR-2.1** `listen = "overlay"` 时，绑定到本节点每个已注册虚拟网卡的 IP（`NicManager::ips()`，`daemon/nic.rs:55`）。一个节点通常只有一张，多网络时每张都绑。
- **FR-2.2** `listen = "lan"` 时，默认绑定所有物理网卡的 IPv4 地址，排除 loopback 和本节点的 TUN。枚举方式复用现成的 `pnet`（`daemon/routes.rs:284` `local_lans` 和 `daemon/nat.rs:201` 都已这么做）。`bind_addr` 非空时只绑那一个地址。
  - **不允许绑 `0.0.0.0`**，即使显式配置。它会把 `lan` 规则同时暴露在 overlay 侧、把 `overlay` 规则同时暴露给物理局域网，是本特性最容易出的安全事故。两侧分离是这个设计的核心不变量。
- **FR-2.3** 绑定地址集合会变（DHCP 续租、切 Wi-Fi、虚拟网卡注册晚于配置加载）。因此监听器集合必须是**收敛**出来的，不是一次性建立的，见 FR-3。
- **FR-2.4** 绑定失败（`EADDRINUSE`、地址还不存在、`<1024` 权限不足）按规则粒度记错误日志并计数，**不影响其他规则**，也不让守护进程退出。下一个收敛周期会重试。`noeio forward list` 要能看到这条规则处于未绑定状态和失败原因。

### FR-3 生命周期与收敛

- **FR-3.1** 复用 `daemon/reconciler.rs` 已经建立的模式，不另起一套：

  ```text
  desired   := f(规则集, 当前可绑定地址集)      // 纯函数，可单测
  installed := 当前实际持有的监听器
  converge  := diff → 建立缺失的，关闭多余的
  ```

  收敛入口挂到现有的 reconciler 循环里（`daemon/reconciler.rs:280` `spawn`，`:288` 已经在同一位置调 `nat::converge`），同时由 `notify` 触发和 30 秒 tick 驱动。
- **FR-3.2** 一条规则被删除或改动时，立刻停止 accept 并关闭该规则在途的所有连接（abort 它的任务）。这是明确选择的行为，不做排空。
- **FR-3.3** 进程退出（含 `kill -9`）时监听器随 fd 消失，**没有任何需要清理的残留状态**。这是用户态方案相对子网路由的实质优势：后者需要 `/var/run/noeio/ip_forward.orig` 和启动清扫来处理 sysctl 与 nftables 表的残留（`daemon/nat.rs:35` `sweep_leftovers`），端口转发完全不需要这套东西。
- **FR-3.4** 目标不可达时：接受连接后立刻失败，按规则计数，日志限速（沿用 `daemon.rs:813` 的「首次 + 每 1000 次」写法）。**不因为目标挂了就撤掉监听器**——目标恢复后不需要人工干预。

### FR-4 UDP（M2）

TCP 靠连接语义天然有生命周期，UDP 没有，所以需要一张会话表：

- **FR-4.1** 会话键为客户端的 `(addr, port)`，值为一个朝目标打开的 socket 加最后活动时间。
- **FR-4.2** 空闲 TTL 默认 60 秒，到期回收。
- **FR-4.3** 每条规则的会话数上限（默认 512）。UDP 源地址可伪造，无上限等于给了一条内存耗尽的路径。达到上限后丢弃新会话并计数，不驱逐活跃会话。
- **FR-4.4** M1 阶段 `proto = "udp"` 在校验层被拒绝，错误信息明确说明「尚未实现」，而不是静默当成 TCP。

### FR-5 可观测性

- **FR-5.1** 每条规则维护：当前连接数、累计连接数、双向字节数、目标连接失败数、被 `allow_from` 拒绝数、绑定状态。
- **FR-5.2** 规则建立/关闭/绑定失败各一条 `tracing` 日志，带 `name`、`listen`、`bind_addr`、`port`、`to`。
- **FR-5.3** 每条连接在建立和关闭时各一条 `debug` 日志，带对端地址和传输字节数。connection 级别不上 `info`，否则一个扫描器就能刷爆日志。

### FR-6 控制面（RPC 与 CLI）

- **FR-6.1** 新增 `noeio-proto/protos/noeio/v1/forward.proto`，`ForwardService` 提供 `AddForward` / `RemoveForward` / `ListForwards`，实现放 `noeio/src/rpc/service/forward.rs`，注册在 `noeio/src/rpc/service.rs:16`。结构照 `route.proto` 与 `rpc/service/route.rs` 写。
- **FR-6.2** CLI 子命令，结构照 `cli.rs:58` 的 `RouteCommand`：

  ```
  noeio forward add nas-web --listen overlay --proto tcp --port 8080 --to 192.168.10.7:80
  noeio forward rm  nas-web
  noeio forward list
  ```

- **FR-6.3** `forward list` 输出每条规则的：名字、监听侧、实际绑定地址列表、协议、端口、目标、绑定状态、FR-5.1 的计数。
- **FR-6.4** RPC socket 的信任模型不变，沿用 `docs/subnet-router.md` 的结论：`/var/run/noeio.sock` 是 `0600` root、无认证，文件权限就是边界。能连上它的人本来就能改路由表，现在还能开端口转发——这不扩大攻击面，但要在文档里写明。

### FR-7 与子网路由的关系

- **FR-7.1** 两个特性正交，可同时启用，互不依赖。端口转发不需要 `accept_routes`，也不需要任何节点宣告网段。
- **FR-7.2** 当 B 既宣告了 `192.168.10.0/24` 又对同一网段里的地址开了 `overlay` 侧转发时，两条路径同时有效：C 可以直连 `192.168.10.7:80`（走子网路由），也可以连 `110.20.0.1:8080`（走端口转发）。不是冲突，不需要去重，但 `forward list` 里加一行提示更友好。
- **FR-7.3** 方向二（LAN → overlay）**填补的正是子网路由明确列为非目标的那个方向**（`docs/subnet-router.md` 末节 "LAN-initiated connections toward overlay nodes"）。两份文档的这一处描述需要同步更新。

### FR-8 Linux 内核态模式（可选，M3 之后）

保留完整设计以备后用，默认不启用。开关为规则级 `mode = "kernel"`，仅 Linux 接受，其他平台报 `FAILED_PRECONDITION`。

- **FR-8.1** 方向一（overlay → LAN），在 `ip noeio` 表里新增 nat prerouting 链：

  ```
  chain prerouting { type nat hook prerouting priority -100;
      iifname <tun> ip daddr <B_overlay> tcp dport 8080 dnat to 192.168.10.7:80 }
  chain postrouting { ... oifname <lan_if> ip daddr 192.168.10.7 tcp dport 80 masquerade }
  ```

- **FR-8.2** 方向二（LAN → overlay）：

  ```
  chain prerouting { iifname <lan_if> ip daddr <B_lan_ip> tcp dport 9090 dnat to 110.20.0.9:22 }
  chain postrouting { oifname <tun> ip daddr <overlay range> tcp dport 22 masquerade }
  ```

  注意这里的 masquerade 出口是 **TUN**，而子网路由现有的 masquerade 出口是 LAN 网卡（`noeio-net-route/src/nftables.rs:507`）。往 TUN 上做 SNAT 意味着 conntrack 开始接管 B 自己去 overlay 的流，所以规则必须用 `ip daddr <overlay range>` 加具体 dport 收窄，不能只匹配 `oifname <tun>`。
- **FR-8.3** 两个方向都需要 `net.ipv4.ip_forward`，复用 `noeio-net-route/src/forwarding.rs` 的原值保存/恢复。
- **FR-8.4** 需要 MSS clamp（`nftables.rs:427` 已有），因为内核模式下不再有两段独立 TCP 连接各自协商 MSS。
- **FR-8.5** 编码层新增：`nat` 表达式（`NFTA_NAT_*`，dnat 类型、地址与端口寄存器）、`immediate` 装载目标地址与端口、`tcp dport` 匹配（transport base offset 2 len 2 + cmp）、`ip daddr` 匹配。按 `nftables.rs` 现有约定，每个都要有字节级 pin 测试并与 `nft --debug=netlink` 的抓包对齐，还要有读回内核实际规则内容的集成测试（对应现有的 AC-18 / NFAC-2）。
- **FR-8.6** 目标为 `127.0.0.1` 时需要 `net.ipv4.conf.<if>.route_localnet=1`，这是又一个全局 sysctl，又一份需要保存恢复的原值。这条单独拿出来说，是因为它让「暴露本机 loopback 服务」这个常见用法在内核模式下明显更贵。

### FR-9 安全要求

这一节独立成章，因为端口转发**创造了新的无认证网络入口**，这是它和子网路由在风险结构上最不一样的地方。

- **FR-9.1** `listen = "lan"` 把一个 overlay 服务暴露给整个物理局域网，**无任何认证**。A 不装 agent，也就不存在身份可验证。同一局域网里的任何机器都能连。这必须在文档、`forward list` 输出和规则建立时的日志里都说清楚。
- **FR-9.2** `listen = "overlay"` 把一个 LAN 服务（或 B 的 loopback 服务）暴露给 noeio 网络里的**每一个**对端。当前没有 ACL 机制（与子网路由现状一致：网络内任何节点都能用任何已接受的子网路由）。
- **FR-9.3** `allow_from` 是本轮唯一的收窄手段，基于源 IP 的 CIDR 匹配，在 accept 之后、连目标之前判断。它不是认证，只是一道粗过滤，文档里不要写成安全边界。
- **FR-9.4** 把 loopback 目标暴露到 overlay 要特别提示：`to = "127.0.0.1:5432"` 这类规则绕过了「服务只监听 loopback」这个开发者本来依赖的保护假设。
- **FR-9.5** FR-1.4 的自环与 overlay→overlay 中继拒绝是硬性要求，不是建议：前者能让守护进程自己把自己打死，后者把 B 变成一个开放中继。

## 7. 平台支持矩阵

| 能力 | Linux | macOS | Windows |
| --- | --- | --- | --- |
| overlay 侧监听（方向一） | ✅ | ✅ | ✅ |
| LAN 侧监听（方向二） | ✅ | ✅ | ✅ |
| 目标为 LAN 地址 | ✅ | ✅ | ✅ |
| 目标为 overlay 对端 | ✅ | ✅ | ✅ |
| 目标为 `127.0.0.1` | ✅ | ✅ | ✅ |
| TCP | ✅ M1 | ✅ M1 | ✅ M1 |
| UDP | ✅ M2 | ✅ M2 | ✅ M2 |
| 内核态模式 | 可选，M3 之后 | ❌ 不适用 | ❌ 不适用 |

三平台对等是选用户态方案换来的直接结果。对照子网路由的同一张表：Advertiser 角色只有 Linux 可用，macOS/Windows 只能做 Consumer。

## 8. 验收标准

### 8.1 端到端

- **AC-1** 方向一：B 配 `listen=overlay, port=8080, to=192.168.10.7:80`，C 上 `curl http://<B_overlay>:8080` 返回 A 的页面；A 的访问日志源地址是 B 的 LAN 地址。
- **AC-2** 方向二：B 配 `listen=lan, port=9090, to=<C_overlay>:22`，A 上 `ssh -p 9090 <B_lan_ip>` 连到 C；C 上 `who`/日志显示源地址是 B 的 overlay 地址。
- **AC-3** loopback 目标：B 配 `to=127.0.0.1:3000`，C 能访问 B 上只绑了 loopback 的服务。
- **AC-4** 大文件传输：方向一传一个 ≥100MB 的文件，校验和一致、不卡死。这条专门验证「代理两端各自协商 MSS，不需要 clamp」的结论。
- **AC-5** 侧隔离：`listen=overlay` 的规则，从物理局域网连不上；`listen=lan` 的规则，从 overlay 对端连不上。
- **AC-6** 运行时增删：`noeio forward add` 后立刻可连，`noeio forward rm` 后立刻拒连且在途连接被关闭，全程不重启进程。
- **AC-7** 目标挂掉再恢复：目标服务停掉期间连接被拒并计数，目标恢复后无需任何操作即可再次连通。
- **AC-8** `kill -9` 之后：`ss -ltn` 没有残留监听，`nft list ruleset` 与 `sysctl net.ipv4.ip_forward` 与启动前一致（用户态模式下不应有任何改动）。
- **AC-9** 与子网路由共存：同时开启 `advertise_routes` 和端口转发，两条访问路径都通。
- **AC-10** `allow_from` 生效：不在列表内的源地址被拒绝并计数。

### 8.2 单元与集成测试

- **AC-11** 规则校验的表驱动测试：自环、overlay→overlay 中继、端口 0、重名、非法 CIDR、`udp`（M1 阶段）全部被拒，错误类型正确。照 `config.rs:299` 现有的 `validate_routes` 测试写法。
- **AC-12** `desired` 纯函数测试：给定规则集与可绑定地址集，产出的监听器集合正确；地址集变化时 diff 正确。不碰真实 socket，照 `reconciler.rs:63` `desired` 的可测性设计。
- **AC-13** 代理转发的回环测试：起一个本地 echo server 当目标，通过规则转发，验证双向字节完整、连接关闭正确传播（一端 FIN 后另一端也收到）。
- **AC-14** 连接数上限：超过 `max_conns` 后新连接被拒且已有连接不受影响。
- **AC-15** 收敛测试：模拟绑定地址消失再出现，监听器相应关闭与重建。
- **AC-16** UDP 会话表（M2）：TTL 到期回收、会话数上限、不同源地址互不干扰。

## 9. 风险与待决问题

| # | 问题 | 说明 | 倾向 |
| --- | --- | --- | --- |
| Q1 | 用户态 vs Linux 内核态 | §5.2 的完整分析。决定性的一点是转发字节本来就要过一趟 boringtun 的用户态加解密，内核 DNAT 省下的那次 memcpy 被同路径的 AEAD 掩盖；而「保留源地址」这个通常支持 DNAT 的论据在两个方向都因为必须 SNAT 而不成立 | **M1 三平台统一用户态**；内核态作为规则级 `mode = "kernel"` 排在 M3 之后，且只在实测出吞吐瓶颈时才做 |
| Q2 | `listen = "lan"` 默认绑哪些地址 | 绑全部物理地址最符合直觉但暴露面最大；要求显式 `bind_addr` 更安全但多一步配置 | 默认绑全部物理地址（排除 TUN 与 loopback），靠 FR-9.1 的显式警告和 `allow_from` 兜住，不允许 `0.0.0.0` |
| Q3 | 规则改动时在途连接怎么办 | 立刻断开实现简单、行为可预测；排空更友好但需要超时、计数和一套半关闭状态机 | M1 立刻断开，排空登记为后续优化 |
| Q4 | 转发规则要不要进 `PeerInfo` 广播 | 广播能让 C 上的 `noeio forward list` 看到 B 提供了什么，但要新增字段、走 derper、并回答「能否信任对端宣告的端口」这个信任问题，等于把一个纯本地特性拉进控制面 | 本轮不做。项目尚未上线，线格式随时可改，这个决定日后翻盘成本很低 |
| Q5 | 目标看不到原始客户端地址 | 用户态和内核态都因为必须 SNAT 而看到 B 的地址，所以这不是选型差异，而是特性固有行为。对依赖访问日志做审计的场景是实质影响 | 文档写明；给 HTTP/TCP 目标提供可选 `proxy_protocol = true` 登记为后续项 |
| Q6 | 单条规则的连接数上限该是多少 | 太低会误伤正常使用，太高等于没有上限 | TCP 默认 256、UDP 会话默认 512，都可按规则覆盖 |
| Q7 | 端口 <1024 | 守护进程已以 root 运行，所以能绑，但这让一条配置错误的规则能占掉系统端口 | 允许，绑定时打一条 `warn` |

## 10. 里程碑

| 里程碑 | 内容 |
| --- | --- |
| **M1** | 配置 + CLI 参数 + 校验（FR-1）、两侧监听与绑定推导（FR-2）、收敛与生命周期（FR-3）、TCP 用户态代理、可观测计数（FR-5）、安全约束（FR-9）。验收 AC-1..AC-5、AC-7..AC-15 |
| **M2** | UDP 转发与会话表（FR-4）。验收 AC-16 |
| **M3** | `ForwardService` RPC 与 `noeio forward` CLI（FR-6）、文档（含同步更新 `docs/subnet-router.md` 关于 LAN 侧发起方向的描述，FR-7.3）。验收 AC-6 |
| **M4（可选）** | Linux 内核态模式（FR-8）。前置条件是实测证明用户态吞吐不够 |

## 11. 现有代码改动点

| # | 位置 | 改动 | 里程碑 |
| --- | --- | --- | --- |
| 1 | `noeio/src/config.rs:5` | `Config` 新增 `#[serde(default)] pub forward: Vec<ForwardConfig>`；新增 `ForwardConfig` 结构与 `validate_forwards`，照 `:171` 的 `validate_routes` 写 | M1 |
| 2 | `noeio/src/cli.rs:15` | `Boot` 新增 `--forward`（`value_delimiter = ','`）；合并逻辑照 `config.rs:148` | M1 |
| 3 | 新建 `noeio/src/daemon/forward.rs` | 规则类型、`desired` 纯函数、收敛、TCP 代理任务、计数。模块结构参照 `daemon/nat.rs` 的 `converge` + 已应用状态的写法 | M1 |
| 4 | `noeio/src/daemon.rs:51` | `NoeioDaemon` 新增 `forwards: RwLock<Vec<ForwardRule>>` 与 `forward_state`，与现有 `advertised` / `nat_state` 同构 | M1 |
| 5 | `noeio/src/daemon/reconciler.rs:288` | 在已有的 `nat::converge` 旁边加 `forward::converge`，复用同一个 notify + 30 秒 tick | M1 |
| 6 | `noeio/src/daemon.rs:312` | `shutdown` 里关闭监听器与代理任务 | M1 |
| 7 | `config.toml.example` | 新增 `[[forward]]` 示例段与注释 | M1 |
| 8 | 新建 `noeio-proto/protos/noeio/v1/forward.proto`、`noeio/src/rpc/service/forward.rs`；改 `noeio/src/rpc/service.rs:16`、`noeio/src/rpc/client.rs`、`noeio/src/cli.rs`、`noeio/src/main.rs:97` | `ForwardService` 与 `noeio forward` 子命令，结构照 `route.proto` / `rpc/service/route.rs` | M3 |
| 9 | 新建 `docs/port-forward.md`；改 `docs/subnet-router.md` 末节、`README.md:9` Features、`README.zh-CN.md` | 用户文档；同步 FR-7.3 提到的方向描述 | M3 |
| 10 | `noeio-net-route/src/nftables.rs:489` 及表达式区 `:388` 附近 | 仅内核态模式需要：prerouting nat 链、`nat`/`immediate`/`tcp dport`/`ip daddr` 表达式及其字节级测试 | M4 |

**不需要改动的地方**，逐条确认过：

- `noeio-proto/protos/common/v1/host_info.proto` —— 端口转发不进广播。
- `noeio-derp` 全部 —— 没有新的控制面消息。
- `noeio/src/daemon/router.rs:133` `allowed_source` —— 两个方向的内层源地址都天然合法（§5.1 第 3 点）。
- `noeio/src/daemon/reconciler.rs` 的 `desired` / `diff` —— 路由集合不变，端口转发不装任何系统路由。
- `noeio/src/interface/virtual_nic.rs` —— TUN 配置不变。

## 12. 明确不在范围内

IPv6 转发；转发规则的网络内广播与自动发现（Q4）；基于身份的 ACL；保留原始客户端源地址（Q5）；连接优雅排空（Q3）；SOCKS/HTTP 代理形态的动态目标；把转发规则做成 Kubernetes Service 那样的多目标负载均衡。
