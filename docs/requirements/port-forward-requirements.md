# 端口转发需求文档

状态：草案
日期：2026-09-28
相关代码：`noeio`、`noeio-proto`
关联文档：[子网路由](./subnet-router-requirements.en.md)、[docs/subnet-router.md](../subnet-router.md)

---

## 1. 背景

子网路由已经解决了「整个网段的机器不装 agent 也能被访问」的问题：Advertiser 宣告一个 CIDR，Consumer 装路由，流量整段进隧道。代价是它只在 Linux 上可用（需要 nftables + `ip_forward`），并且只支持 overlay → LAN 一个方向，LAN 侧主动发起的连接被明确列为非目标（`docs/subnet-router.md` 最后一节）。

很多场景其实不需要整段网络，只需要一个端口：NAS 的 8080、打印机的 631、内网数据库的 3306、跑在 B 本机 loopback 上的服务。反过来，局域网里那台不装 agent 的机器也常常只需要访问 overlay 里的某一个服务。

端口转发覆盖这两个缺口。它的形态是一条前台命令 `noeio forward`：命令启动即开始转发，命令结束转发随之消失。这和 `ssh -L`、`kubectl port-forward`、`socat` 是同一类工具，用户已经有稳固的心智模型。它完全在用户态实现，不需要任何内核 NAT，三个平台一套代码，也不需要守护进程承载任何状态。

## 2. 目标与非目标

### 2.1 目标

- 用户在 B 节点上执行 `noeio forward <规则>...`，命令在前台运行并持续转发，直到被终止。
- **方向一（overlay → LAN）**：C 访问 `B_overlay_ip:8080`，B 转发到同局域网的 A `192.168.10.7:80`。A 不装 agent、不需要任何配置。
- **方向二（LAN → overlay）**：A 访问 `B_lan_ip:9090`，B 转发到 overlay 里的 C `110.20.0.9:22`。A 不装 agent、不需要路由。
- 目标地址也可以是 B 自己的 `127.0.0.1`，把只监听 loopback 的本机服务暴露给 overlay。
- **转发的生命周期与命令一致**：命令收到 SIGINT（Ctrl+C）、SIGTERM、SIGHUP 时优雅退出并释放所有监听器和连接；被 SIGKILL 或任何无法捕获的方式结束时，监听器和连接由内核随进程回收。任何情况下都不留残留状态。
- 一条命令对应一条转发。要开多条转发就开多个进程，每个进程独立启停，互不影响。
- macOS、Windows、Linux 三个平台**能力完全对等**。

### 2.2 非目标

- **守护进程承载转发**：`noeio boot` 不读取转发配置，不在后台维持转发，不跟踪转发进程。需要开机自启的转发，用 systemd / launchd 管理 `noeio forward` 进程本身。
- **UDP 转发**：见 FR-4，放到 M2。M1 阶段 `udp` 直接报错拒绝，不静默降级。
- **IPv6**：与子网路由保持一致，`noeio-net-route` 和 `smoltcp` 配置目前都是 IPv4-only（`Cargo.toml:33` features = `proto-ipv4`）。
- **转发规则的网络内广播 / 自动发现**：C 想知道 B 开了哪些转发，只能靠人告知。理由见 §10 Q4。
- **基于身份的 ACL**：与子网路由现状一致，只有基于源 IP 的粗粒度 `--allow-from`。
- **保留原始客户端源地址**（PROXY protocol / TPROXY）：见 §10 Q5。
- **连接优雅排空**：进程退出立刻关闭在途连接，不等它自然结束。
- **Linux 内核态（nftables DNAT/SNAT）模式**：见 §13。

## 3. 术语

| 术语 | 含义 |
| --- | --- |
| 转发进程 | 一次 `noeio forward ...` 命令对应的操作系统进程。它持有全部监听 socket 与代理任务，是本特性唯一的生命周期单位 |
| 转发规则（rule） | 一条 `监听地址:端口 → 目标地址:端口` 的映射，由 `noeio forward` 的 `--listen` 与 `--target` 给出，协议由 `--proto` 给出。一个转发进程只承载一条规则 |
| 监听地址（listen address） | `--listen` 的地址部分。关键字 `noeio` 展开为本节点全部 overlay 地址（即每张 noeio 虚拟网卡的地址），关键字 `lan` 展开为本节点全部物理网卡地址，也可以是一个具体的本机 IPv4 地址。展开后的每个地址各绑一个监听器 |
| 监听侧（listen side） | 由监听地址推出的归属：`overlay` 侧只有 noeio 对端能连；`lan` 侧只有物理局域网能连。具体 IP 属于哪一侧看它是 TUN 地址还是物理网卡地址 |
| 目标（target） | `--target` 给出的 `addr:port`，规则转发到的地方。`overlay` 侧监听时通常是 LAN 地址或 loopback，`lan` 侧监听时通常是对端的 overlay 地址 |
| 用户态转发 | 转发进程在监听侧 accept 连接，向目标另开一条连接，双向拷贝字节流。两条连接各自是独立的 TCP 连接 |
| 守护进程 | `noeio boot` 进程。转发进程只向它做一次只读查询（FR-6.2），数据面上两者没有任何耦合 |

## 4. 场景

```
   A  192.168.10.7                B  overlay 110.20.0.1            C  overlay 110.20.0.9
   不装 agent                      lan 192.168.10.3                  accept_routes 无关
        |                               |                                  |
        +------ 物理局域网 192.168.10.0/24 ------+                          |
                                        |                                  |
                                        +------ overlay (WireGuard/UDP) ---+

   B 上终端 1：
           $ noeio forward --listen noeio:8080 --target 192.168.10.7:80
           forwarding tcp noeio:8080 -> 192.168.10.7:80
             bound 110.20.0.1:8080
           press Ctrl+C to stop

   B 上终端 2：
           $ noeio forward --listen lan:9090 --target 110.20.0.9:22
           forwarding tcp lan:9090 -> 110.20.0.9:22
             bound 192.168.10.3:9090
           WARNING: this exposes overlay host 110.20.0.9:22 to the whole LAN without authentication
           press Ctrl+C to stop

   方向一：C 上 `curl http://110.20.0.1:8080`
           -> 转发进程的 overlay 侧监听器 accept
           -> 转发进程向 192.168.10.7:80 另开连接
           -> A 的访问日志里看到的源地址是 192.168.10.3（B 的 LAN 地址）

   方向二：A 上 `ssh -p 9090 192.168.10.3`
           -> 转发进程的 LAN 侧监听器 accept
           -> 转发进程向 110.20.0.9:22 另开连接（内核按 /32 主机路由送进 B 的 TUN，由守护进程封装）
           -> C 上看到的源地址是 110.20.0.1（B 的 overlay 地址）

   终端 1 按 Ctrl+C（终端 2 的转发不受影响）：
           ^C
           shutting down: 1 listener closed, 3 connections aborted
           total: 17 connections, 4.2 MiB in, 118.6 MiB out, 0 target failures, 0 rejected by allow-from
           $
```

## 5. 可行性评估

### 5.1 结论

两个方向都可行，而且**不需要动任何协议、不需要动 derper、不需要动 `PeerInfo`**，守护进程只需要新增一个只读 RPC。端口转发是纯本地行为，C 只是在连一个普通的 `ip:port`，A 也只是在连一个普通的 `ip:port`，两边都不需要知道背后发生了什么。

转发进程与守护进程是**两个独立进程**，它们之间在数据面上没有任何耦合。这一点成立，是因为下面三件事都是主机协议栈层面的性质，与哪个进程持有 socket 无关：

1. **在 overlay 地址上监听是可行的，任何本机进程都可以。** TUN 用 `/32` 掩码配置了本节点的 overlay 地址（`noeio/src/interface/virtual_nic.rs:74`），它是一个正常的本机地址。守护进程解封装后把包写进 TUN（`noeio/src/daemon.rs:845` `write_to_nic`），目的地址是自己的包由主机协议栈正常交付给监听 socket，不管这个 socket 属于哪个进程。一个普通的 `TcpListener::bind((overlay_ip, port))` 就够了。
2. **从任何本机进程主动连 overlay 对端也是可行的。** reconciler 已经为每个对端装了 `/32` 主机路由（`noeio/src/daemon/reconciler.rs:70`），出口是 TUN。所以转发进程里一次普通的 `TcpStream::connect(C_noeio:22)` 会被内核路由进 TUN，被守护进程的 `process_outbound`（`daemon.rs:415`）读到、查表、封装发给 C。源地址由内核按出口网卡选中 TUN 的地址，也就是 B 的 overlay 地址。
3. **反欺骗检查不用改。** `Router::allowed_source`（`daemon/router.rs:133`）要求入向包的内层源地址属于对端自己的 overlay 地址或它宣告的网段。方向一里 C 发来的包源地址是 C 自己的 overlay 地址；方向二里 B 发给 C 的包源地址是 B 的 overlay 地址。两种情况都天然通过。

转发进程唯一需要从守护进程拿到的信息是「本节点有哪些 overlay 地址」以及「哪些地址属于 overlay」（用于 FR-1.3 的中继拒绝）。这是一次启动时的只读查询（FR-6.2），查询之后两个进程再无交互。守护进程之后退出、重启都不影响转发进程的存活（但会让 overlay 方向的连接失败，见 FR-3.6）。

### 5.2 为什么是前台进程

把转发做成一条前台命令而不是守护进程里的一张规则表，理由有三：

1. **使用形态贴近需求。** 端口转发的典型用法是「临时把这个东西暴露出去一会儿」。`ssh -L`、`kubectl port-forward`、`socat` 都没有规则表，开着就转发，关掉就停。
2. **生命周期由操作系统兜底。** 监听 socket 属于转发进程。进程以任何方式退出（正常返回、Ctrl+C、`kill -TERM`、`kill -9`、终端关闭、OOM）内核都会回收 fd。不需要收敛循环，不需要清扫残留，不需要「规则删了但连接还在」这类状态机。这条保证不依赖任何软件逻辑正确运行。
3. **守护进程保持精简。** 守护进程唯一新增的是一个只读查询（FR-6.2），告诉转发进程本节点的 overlay 地址是什么。规则表、RPC 增删、reconciler 分支、shutdown 分支都不需要。

代价是没有「运行中改规则」和「列出当前转发」：要改规则就结束进程重开一条；要看当前转发就看那个终端。这与 `ssh -L` 一致，可以接受。

### 5.3 为什么是用户态而不是 Linux 内核 DNAT/SNAT

内核态方案技术上可行，但有几条论据让它在这里站不住：

- **性能论据不成立。** 转发的每一个字节本来就已经过了一趟用户态：noeio 的 WireGuard 是 boringtun（`Cargo.toml:10`），加解密在用户态完成，包要经过 `process_outbound` / `handle_delivery` 读写 TUN。内核 DNAT 省掉的是代理的那一次 `copy_bidirectional`，而同一路径上还有 AEAD 加解密和 TUN 读写。省掉一次 memcpy，留着一次 ChaCha20-Poly1305，收益在噪声级别。
- **源地址论据不成立。** 方向一里 A 没有回到 overlay 的路由，方向二里 C 没有回到物理局域网的路由，所以两个方向都必须 SNAT。内核态下目标看到的源地址同样是 B 的地址，和用户态的可观测行为完全一致。
- **与生命周期目标不相容。** nftables 规则和 `ip_forward` 不随进程消失。`kill -9` 之后必然留下残留，就要重新引入子网路由那套原值保存（`/var/run/noeio/ip_forward.orig`）与启动清扫（`daemon/nat.rs:35` `sweep_leftovers`）。这直接违反 §2.1 的「任何情况下都不留残留状态」。

逐项对比：

| 维度 | 用户态代理 | Linux 内核 DNAT/SNAT |
| --- | --- | --- |
| 新增代码量 | 约 300 行，标准 tokio 代理 | 约 200 行 nftables 表达式编码 + 字节级测试，再加一套收敛/清理逻辑 |
| 平台覆盖 | 三平台一套代码 | 仅 Linux，另两个平台仍需用户态，等于维护两条路径 |
| 权限要求 | 绑端口 | `CAP_NET_ADMIN` + 全局 `net.ipv4.ip_forward` + `nft_masq`/`nf_nat`/`nft_ct` 模块 |
| 容器内可用 | 是 | 受限，NAT 模块在非特权容器里无法自动加载（`docs/subnet-router.md` 已记录） |
| MSS / MTU | **不需要处理**。代理两端各自是独立 TCP 连接，overlay 侧按 TUN 的 1411 协商，LAN 侧按 1500 协商 | 需要 MSS clamp mangle 链，否则出现「ping 通但大文件卡死」 |
| `kill -9` 后残留 | **无**。监听器随进程 fd 消失 | `ip_forward` 和 nftables 表都会残留 |
| 可观测性 | `ss -ltn` 能看到监听器，每条规则的连接数/字节数/错误数都是现成的 | 无监听器可看；已知的静默失败模式：conntrack 先绑定了别的 NAT 映射，规则变成空操作且无日志 |
| 目标为 `127.0.0.1` | 直接可用 | 需要额外打开 `route_localnet` |
| 协议覆盖 | TCP 完备；UDP 需要会话表（FR-4） | TCP/UDP 都由 conntrack 负责 |

唯一明确属于内核态的优势是 UDP：conntrack 免费提供会话跟踪，而用户态要自己写一张带 TTL 的会话表。但那张表是有界的、约 150 行的确定性工作（FR-4），换来的是三平台对等和无残留，仍然划算。

## 6. 命令行接口

### 6.1 在 `noeio` 命令树中的位置

```
noeio
├── boot          启动守护进程（常驻）
├── netcheck      测量各 derper 的 RTT
├── create vnic   创建虚拟网卡
├── route         子网路由管理
│   ├── advertise <CIDR>...
│   ├── withdraw  <CIDR>...
│   └── list
└── forward       端口转发（前台运行，本文档）
```

`forward` 与 `boot` 一样是前台长驻命令，与 `route` 这类「一次 RPC 就返回」的命令不同。它没有子命令：转发的启动就是执行这条命令，转发的停止就是结束这条命令。

### 6.2 `noeio forward` 用法

```
noeio forward --listen <ADDR>:<PORT> --target <ADDR>:<PORT> [OPTIONS]

必选：
      --listen <ADDR>:<PORT>
          监听地址与端口。<ADDR> 可以是：
            noeio          本节点全部 noeio 虚拟网卡的地址（即 overlay 地址），只有 noeio 对端能连
            lan            本节点全部物理网卡的 IPv4 地址，只有物理局域网能连
            <IPv4>         本机当前持有的某一个具体地址；属于 TUN 则视为 overlay 侧，
                           属于物理网卡则视为 lan 侧。用于多网卡机器只在一块网卡上开监听
          不允许 0.0.0.0（会同时暴露两侧）和 127.0.0.1（本机自己连自己没有意义）。
          <PORT> 为 1..=65535。

      --target <ADDR>:<PORT>
          转发目标，IPv4 字面量加端口，例如 192.168.10.7:80、127.0.0.1:5432。
          不接受域名。

可选：
      --proto <PROTO>
          传输协议。tcp（M1）或 udp（M2；M1 阶段报错拒绝）。 [default: tcp]

      --allow-from <CIDR>[,<CIDR>...]
          只接受来自这些 IPv4 网段的连接，其余在 accept 后立刻关闭并计数。
          默认不限制。这是基于源 IP 的粗过滤，不是认证。

  -h, --help
  -V, --version
```

一条命令只描述一条转发。需要多条就开多个进程，每个进程有自己的终端、日志和生命周期。

### 6.3 退出码

| 退出码 | 含义 |
| --- | --- |
| 0 | 收到终止信号（Ctrl+C / SIGTERM / SIGHUP，或 Windows 对应事件）后正常退出 |
| 1 | 运行环境错误：连不上守护进程、任一监听地址绑定失败 |
| 2 | 参数错误：规则解析或校验失败、选项非法（与 clap 自身的用法错误退出码一致） |

`kill -9` 等不可捕获的结束没有退出码可言，见 FR-3.4。

### 6.4 输出约定

| 流 | 内容 |
| --- | --- |
| stdout | 启动横幅（FR-5.2）、退出摘要（FR-5.4）。两者都是一次性的、面向人读的 |
| stderr | `tracing` 日志（FR-5.3），受 `RUST_LOG` 控制 |

这样 `noeio forward ... 2>forward.log` 时终端只剩横幅和摘要，`noeio forward ... >/dev/null` 时只剩日志。

### 6.5 示例

把局域网里 NAS 的 Web 界面暴露给 overlay：

```
$ sudo noeio forward --listen noeio:8080 --target 192.168.10.7:80
```

让局域网里不装 agent 的机器能 SSH 到 overlay 节点 C：

```
$ sudo noeio forward --listen lan:9090 --target 110.20.0.9:22
```

把本机只监听 loopback 的数据库暴露给 overlay，并只允许一个网段访问：

```
$ sudo noeio forward --listen noeio:5432 --target 127.0.0.1:5432 --allow-from 110.20.0.0/24
```

一次开一组端口：每条一个进程，各自独立启停。

```
$ sudo noeio forward --listen noeio:8080 --target 192.168.10.7:80 &
$ sudo noeio forward --listen noeio:631  --target 192.168.10.12:631 &
$ sudo noeio forward --listen lan:9090   --target 110.20.0.9:22 &
```

多网卡机器上只在有线网卡的地址上开 LAN 侧监听（写具体地址而不是 `lan`）：

```
$ sudo noeio forward --listen 192.168.10.3:9090 --target 110.20.0.9:22
```

作为 systemd 服务常驻（生命周期交给 systemd，`systemctl stop` 发 SIGTERM，转发随之停止）：

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

结束转发：在运行它的终端按 Ctrl+C，或对进程发 `kill -TERM <pid>`；没有其他方式，也不需要其他方式。

## 7. 功能需求

### FR-1 规则的书写与校验

- **FR-1.1** 一条命令描述一条规则：`--listen <ADDR>:<PORT>` 说监听在哪，`--target <ADDR>:<PORT>` 说转发到哪，`--proto`（默认 `tcp`）说走什么协议，完整说明见 §6.2。两个地址选项形状一致，读起来就是「从这里到那里」。`--listen` 的地址部分允许 `noeio` / `lan` 两个关键字，因为绝大多数用法是「绑到 noeio 网卡那一侧」或「绑到局域网那一侧」，用户不该被要求先查自己的 overlay 地址再抄进命令行；需要精确控制时写具体 IP。
- **FR-1.2** `--allow-from` 是这条规则的附属选项（§6.2），结构照 `cli.rs:15` 的 `Boot` 写。需要多条规则就开多个进程，每个进程带自己的一组选项。
- **FR-1.3** 校验全部在一个纯函数里完成（可单测，和 `daemon/routes.rs:134` `validate_advertisement` 的「单一闸门」模式一致），**校验失败命令不启动**，进程以退出码 2 结束并打印全部错误（不是遇到第一个就停）：
  - `--listen` 可解析为 `<ADDR>:<PORT>`，`ADDR` 是 `noeio`、`lan` 或一个 IPv4 字面量；
  - `--listen` 的端口在 `[1, 65535]`；
  - `--listen` 给具体 IP 时，它必须是本机**当前持有**的地址：在 FR-6.2 返回的 overlay 地址集里（判为 overlay 侧），或在 FR-2.2 枚举的物理地址集里（判为 lan 侧）；两边都不在则报「不是本机地址」。**拒绝 `0.0.0.0`**（FR-2.4）和 `127.0.0.1`（本机连本机不需要转发）；
  - `--proto` 目前只接受 `tcp`，`udp` 报「尚未实现」（FR-4.4），其他值报非法；
  - `--target` 可解析为 `IPv4:port`，端口非 0，不接受域名（域名解析结果会变，与「启动时一次校验」的模型不符）；
  - **拒绝自环**：`--target` 等于本进程将要绑定的任何一个 `addr:port`（overlay 侧绑定地址来自 FR-6.2，lan 侧来自 FR-2.2）；
  - **拒绝 overlay→overlay 中继**：监听侧为 overlay（关键字 `noeio` 或判为 overlay 侧的具体 IP）且 `--target` 的地址是本节点的 overlay 地址、或守护进程当前已知的任何对端 overlay 地址（FR-6.2 返回的两个集合）。这会让 B 变成两个对端之间的中继，是本轮范围外的策略问题，报专门的错误而不是静默允许；
  - `--allow-from` 每项可解析为 IPv4 CIDR；

- **FR-1.4** 需要守护进程信息的校验项（自环、中继拒绝）依赖 FR-6.2 的查询。守护进程不在时的行为见 FR-6.3。

### FR-2 监听地址的推导与绑定

- **FR-2.1** `--listen noeio:<port>` 展开为 FR-6.2 返回的本节点每一个 overlay 地址。一个节点通常只有一张虚拟网卡，多网络时每张都绑。
- **FR-2.2** `--listen lan:<port>` 展开为所有物理网卡的 IPv4 地址，排除 loopback 和本节点的 TUN（TUN 地址即 FR-6.2 返回的 overlay 地址）。枚举方式复用现成的 `pnet`（`daemon/routes.rs:284` `local_lans` 已这么做，本特性需要的是地址而不是网段，抽一个 `local_addrs` 出来共用）。
- **FR-2.3** `--listen <IPv4>:<port>` 不展开，只绑这一个地址；FR-1.3 已经校验过它属于哪一侧。
- **FR-2.4** **不允许绑 `0.0.0.0`**，即使显式配置。它会把 `lan` 规则同时暴露在 overlay 侧、把 `overlay` 规则同时暴露给物理局域网，是本特性最容易出的安全事故。两侧分离是这个设计的核心不变量。
- **FR-2.5** 地址集合在**启动时推导一次**。一条规则可能对应多个绑定地址（overlay 侧多张虚拟网卡、lan 侧多块物理网卡），**全部绑定成功后才进入服务状态**；任何一个失败（`EADDRINUSE`、地址不存在、`<1024` 权限不足），命令整体失败，已绑定的监听器随进程退出释放，退出码 1，错误信息指明哪个地址、什么原因。一条前台命令「一半地址在跑一半没跑」比直接失败更难发现，all-or-nothing 才是命令行工具的正确语义。
- **FR-2.6** 绑定之后地址集合发生变化（DHCP 续租、切 Wi-Fi、守护进程新注册虚拟网卡）**不做处理**。已绑定的 socket 在地址消失后 accept 会开始报错，按 FR-5 计数与日志；用户重启命令即可。理由见 §10 Q2。
- **FR-2.7** Windows 上 `pnet` 未链接（`routes.rs:301`），关键字 `lan` 无法展开，校验阶段报错并提示改写成具体的物理网卡地址（`--listen 192.168.10.3:9090`）。此时 FR-1.3 的「是否本机地址」检查退化为只查 overlay 集合，不在其中的 IP 一律按 lan 侧接受，交给 bind 失败兜底。`noeio` 关键字不受影响，地址来自 FR-6.2。

### FR-3 生命周期

这是本特性的核心不变量：**转发存在，当且仅当转发进程存在。**

- **FR-3.1 启动序列**，任一步失败即退出，不进入服务状态：
  1. 连接守护进程 RPC，查询 overlay 地址集（FR-6.2）；
  2. 枚举物理地址（FR-2.2）；
  3. 校验全部规则（FR-1.3）；
  4. 绑定全部监听器（FR-2.5）；
  5. 打印启动横幅（FR-5.2）；
  6. 每个监听器起一个 accept 循环，进入服务状态。
- **FR-3.2 优雅退出**。转发进程等待以下任一信号，收到后：停止所有 accept 循环，abort 所有在途连接任务，关闭所有监听器，打印退出摘要（FR-5.4），以退出码 0 结束。
  - Unix：`SIGINT`（Ctrl+C）、`SIGTERM`（`kill`、systemd stop）、`SIGHUP`（终端关闭、SSH 断开）。三者一律视为「结束」，`SIGHUP` **不**做 reload 语义，因为本命令没有可 reload 的东西。
  - Windows：`Ctrl+C`、`Ctrl+Break`、控制台关闭（`CTRL_CLOSE_EVENT`）、登出/关机（`CTRL_LOGOFF_EVENT` / `CTRL_SHUTDOWN_EVENT`）。tokio 的 `signal::windows::{ctrl_c, ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown}` 已覆盖。
  - 现有 `main.rs:118` `wait_for_shutdown_signal` 只处理 `SIGINT`/`SIGTERM`，需要扩展为上述集合并抽到公共位置，`noeio boot` 一并受益。
- **FR-3.3 退出时限**。优雅退出的全部工作有上界：abort 任务是同步的，关闭 socket 是同步的，正常情况下毫秒级完成。仍然给整个退出过程套一个 2 秒的超时，超时则直接 `process::exit(0)`，在途连接由内核回收，不存在需要等待的清理。这和 `noeio boot` 里 5 秒等路由清理的语义不同：那里有真正的系统状态要还原，这里没有。
- **FR-3.4 不可捕获的退出**。`SIGKILL`、panic、OOM、断电重启：内核关闭进程的全部 fd，监听器立刻释放，在途连接的对端收到 RST 或 FIN。**没有任何需要清理的残留状态，也没有任何启动时的清扫逻辑。** 这是用户态方案相对子网路由的实质优势（后者需要 `/var/run/noeio/ip_forward.orig` 和 `daemon/nat.rs:35` `sweep_leftovers`），也是 §13 排除内核态模式的直接原因。
- **FR-3.5 目标不可达时**：接受连接后连目标失败，立刻关闭客户端连接，按规则计数，日志限速（沿用 `daemon.rs:813` 的「首次 + 每 1000 次」写法）。**不因为目标挂了就退出进程或撤掉监听器**，目标恢复后不需要人工干预。
- **FR-3.6 守护进程退出时**：转发进程**不监视**守护进程，不因守护进程退出而退出。`lan` 侧规则连 overlay 目标会开始失败（TUN 与主机路由随守护进程消失），走 FR-3.5 的路径计数；`overlay` 侧监听器绑定的 TUN 地址消失后 accept 报错，同样计数并限速记日志。守护进程重新 `boot` 后，`lan → overlay` 方向自动恢复（路由重装），`overlay` 侧监听器则需要重启转发进程（地址重新出现但旧 socket 已失效）。理由见 §10 Q3。
- **FR-3.7 一进程一规则**。转发进程之间没有任何共享状态，也互不感知；一个进程退出不影响其他进程。多条转发就是多个进程，由用户或进程管理器（systemd template unit、launchd）各自管理。

### FR-4 UDP（M2）

TCP 靠连接语义天然有生命周期，UDP 没有，所以需要一张会话表：

- **FR-4.1** 会话键为客户端的 `(addr, port)`，值为一个朝目标打开的 socket 加最后活动时间。
- **FR-4.2** 空闲 TTL 默认 60 秒，到期回收。
- **FR-4.3** 会话数上限固定为 512，不提供命令行选项。UDP 源地址可伪造，无上限等于给了一条内存耗尽的路径，所以这个上界是安全属性而不是配置项。达到上限后丢弃新会话并计数，不驱逐活跃会话。TCP 不设连接数上限：每条连接有自己的生命周期，fd 数量由操作系统的进程限制兜底。
- **FR-4.4** M1 阶段 `udp` 在校验层被拒绝，错误信息明确说明「尚未实现」，而不是静默当成 TCP。
- **FR-4.5** 会话表随进程消失，进程退出时不需要主动清理。

### FR-5 可观测性

前台 stdout 就是全部界面，所以三段输出要各自完整：

- **FR-5.1 计数**。转发进程维护：当前连接数、累计连接数、双向字节数、目标连接失败数、被 `--allow-from` 拒绝数、accept 错误数。
- **FR-5.2 启动横幅**（stdout，进入服务状态时打印一次）。第一行是规则本身（协议、`--listen` 原文、目标），之后每个实际绑定的地址一行，再按 FR-8 的要求打印安全提示。横幅的最后一行固定是 `press Ctrl+C to stop`，因为这是用户唯一需要知道的「怎么停」。
- **FR-5.3 运行日志**（`tracing`，走 stderr）。规则级事件 `info`：accept 循环启动/停止、绑定地址失效。连接级事件 `debug`：建立与关闭各一条，带对端地址和传输字节数。connection 级别不上 `info`，否则一个扫描器就能刷爆日志。默认日志级别是 `info`，所以连接级日志默认不显示，需要时用 `RUST_LOG=debug` 打开。
- **FR-5.4 退出摘要**（stdout，优雅退出时打印一次）。第一行说明关闭了多少监听器、abort 了多少在途连接；第二行是 FR-5.1 的全部计数。`kill -9` 自然打印不出来，这是可接受的，因为摘要只是便利而不是正确性的一部分。
- **FR-5.5** 可选：Unix 上收到 `SIGUSR1` 时把当前计数打印到 stderr 而不退出。登记为 M3 的低优先级项。

### FR-6 与守护进程的交互

转发进程与守护进程只有**一次启动时的只读查询**，之后没有任何交互。

- **FR-6.1** CLI 子命令 `noeio forward`，加在 `cli.rs:13` `Command` 上，与 `Boot` / `Route` 同级。**没有子子命令**。`main.rs:23` 的 `match` 新增一个分支，结构上更接近 `Boot`（有 `tokio::select!` 等信号）而不是 `Route`（一次 RPC 就返回）。
- **FR-6.2** 守护进程新增只读 RPC。虚拟网卡的地址只存在守护进程内存里的 `NicManager`（`daemon/nic.rs:14`），转发进程是另一个进程，读不到；而靠枚举系统网卡去猜哪块是 noeio 的 TUN 不可靠：Linux 上名字固定为 `noeio0`（`interface/virtual_nic.rs:87`），但 macOS 强制用系统分配的 `utunN`，与其他 VPN 混在一起，Windows 同理。所以 `--listen overlay` 的展开必须问守护进程。在现有 `VirtualNicService`（`noeio-proto/protos/noeio/v1/virtual_nic.proto:15`）上加一个 `List`，与已有的 `CreateVirtualNic` 配对，不新建服务：

  ```proto
  service VirtualNicService {
    rpc CreateVirtualNic(CreateVirtualNicRequest) returns (CreateVirtualNicResponse);
    rpc ListVirtualNics(ListVirtualNicsRequest) returns (ListVirtualNicsResponse);
  }

  message ListVirtualNicsRequest {}

  message VirtualNicEntry {
    // 系统里的接口名：Linux 为 noeio0，macOS 为 utunN，Windows 为适配器名。
    string tun_name = 1;
    // 这张网卡的 overlay 地址（VirtualNic.ip）。
    string ip = 2;
    // 这张网卡加入的网络。
    string network_id = 3;
    // 本节点在该网络里的 peer id。
    uint32 peer_id = 4;
    // 该网络里当前已知的其他对端的 overlay 地址（Router::ips()，daemon/router.rs:163）。
    repeated string peer_ips = 5;
  }

  message ListVirtualNicsResponse {
    repeated VirtualNicEntry nics = 1;
  }
  ```

  转发进程从中取：`nics[].ip` 用于 overlay 侧绑定（FR-2.1）、lan 侧排除 TUN（FR-2.2）、判断具体 IP 属于哪一侧和自环检查；`nics[].peer_ips` 用于中继拒绝（FR-1.3）。`tun_name` / `network_id` / `peer_id` 是顺手带出来的，供横幅打印和将来 `noeio vnic list` 之类的命令复用，本特性不依赖。实现放 `noeio/src/rpc/service/nic.rs`，客户端方法放 `rpc/client.rs`。

- **FR-6.2a** 多张虚拟网卡时 `--listen noeio:<port>` **绑全部**，不默认取第一张。多张网卡意味着本节点加入了多个 noeio 网络，「暴露给 noeio」在没有进一步说明时最自然的含义是暴露给全部网络；而 `NicManager` 是一张 `DashMap`，迭代顺序不稳定，「第一张」不是一个有定义的概念，两次启动可能绑到不同的网卡。只想暴露给某一个网络的用户写具体 IP（`--listen 110.20.0.1:8080`）。理由见 §10 Q9。

- **FR-6.3** 守护进程不在时 `noeio forward` **拒绝启动**，错误信息与 `noeio route` 一致（`main.rs:101`：``Is `noeio boot` running?``）。不提供「不查守护进程、手工指定 overlay 地址」的旁路：没有守护进程就没有 TUN，overlay 侧监听绑不上、lan 侧规则连不到 overlay，转发本身没有意义，报错比给用户一个必然失败的进程更诚实（§10 Q8）。
- **FR-6.4** RPC socket 的信任模型不变，沿用 `docs/subnet-router.md` 的结论：`/var/run/noeio.sock` 是 `0600` root、无认证，文件权限就是边界（`rpc/mod.rs:7`）。这意味着 `noeio forward` 与 `noeio route` 一样需要 root（或对 socket 的等价权限）才能运行。顺带的结果是绑定 `<1024` 端口不再是问题（§10 Q7）。要在文档里写明。
- **FR-6.5** 守护进程**不**知道有转发进程存在，不跟踪、不列出、不管理它们。`noeio route list` 不显示转发。一旦守护进程开始跟踪转发进程，就要回答「转发进程死了守护进程怎么知道」，那是一套额外的状态管理，与 §5.2 的第二条理由相悖。

### FR-7 与子网路由的关系

- **FR-7.1** 两个特性正交，可同时启用，互不依赖。端口转发不需要 `accept_routes`，也不需要任何节点宣告网段。
- **FR-7.2** 当 B 既宣告了 `192.168.10.0/24` 又对同一网段里的地址开了 `overlay` 侧转发时，两条路径同时有效：C 可以直连 `192.168.10.7:80`（走子网路由），也可以连 `110.20.0.1:8080`（走端口转发）。不是冲突，不需要去重。启动横幅可以加一行提示，不是必须。
- **FR-7.3** 方向二（LAN → overlay）**填补的正是子网路由明确列为非目标的那个方向**（`docs/subnet-router.md:200` "LAN-initiated connections toward overlay nodes"）。两份文档的这一处描述需要同步更新，指向 `noeio forward --listen lan:<port> ...`。

### FR-8 安全要求

这一节独立成章，因为端口转发**创造了新的无认证网络入口**，这是它和子网路由在风险结构上最不一样的地方。

- **FR-8.1** 监听侧为 lan 时，把一个 overlay 服务暴露给整个物理局域网，**无任何认证**。A 不装 agent，也就不存在身份可验证。同一局域网里的任何机器都能连。监听侧为 lan 时启动横幅里必须有一行 `WARNING`，文档也要写清楚。
- **FR-8.2** 监听侧为 overlay 时，把一个 LAN 服务（或 B 的 loopback 服务）暴露给 noeio 网络里的**每一个**对端。当前没有 ACL 机制（与子网路由现状一致）。
- **FR-8.3** `--allow-from` 是本轮唯一的收窄手段，基于源 IP 的 CIDR 匹配，在 accept 之后、连目标之前判断。它不是认证，只是一道粗过滤，文档里不要写成安全边界。
- **FR-8.4** 目标为 loopback 时横幅要特别提示：`--listen noeio:5432 --target 127.0.0.1:5432` 这类规则绕过了「服务只监听 loopback」这个开发者本来依赖的保护假设。
- **FR-8.5** FR-1.3 的自环与 overlay→overlay 中继拒绝是硬性要求，不是建议：前者能让转发进程自己把自己打死，后者把 B 变成一个开放中继。
- **FR-8.6** 前台进程模型本身是一道安全收益：转发只在有人明确启动并保持它运行的时间段内存在，不会因为一条忘了删的配置在重启后悄悄复活。文档里值得点明这一点。

## 8. 平台支持矩阵

| 能力 | Linux | macOS | Windows |
| --- | --- | --- | --- |
| overlay 侧监听（方向一） | ✅ | ✅ | ✅ |
| LAN 侧监听（方向二），`--listen lan:<port>` | ✅ | ✅ | ❌ 需写具体地址（FR-2.7） |
| LAN 侧监听，`--listen <IPv4>:<port>` | ✅ | ✅ | ✅ |
| 目标为 LAN 地址 | ✅ | ✅ | ✅ |
| 目标为 overlay 对端 | ✅ | ✅ | ✅ |
| 目标为 `127.0.0.1` | ✅ | ✅ | ✅ |
| TCP | ✅ M1 | ✅ M1 | ✅ M1 |
| UDP | ✅ M2 | ✅ M2 | ✅ M2 |
| Ctrl+C / 终止信号优雅退出 | SIGINT/SIGTERM/SIGHUP | SIGINT/SIGTERM/SIGHUP | Ctrl+C/Break/Close/Logoff/Shutdown |
| `kill -9` / 强制结束后无残留 | ✅ 内核回收 | ✅ 内核回收 | ✅ 内核回收 |
| `SIGUSR1` 打印计数（FR-5.5） | ✅ | ✅ | ❌ 无对应信号 |

三平台对等是选用户态方案换来的直接结果。对照子网路由的同一张表：Advertiser 角色只有 Linux 可用，macOS/Windows 只能做 Consumer。

## 9. 验收标准

### 8.1 端到端

- **AC-1** 方向一：B 上 `noeio forward --listen noeio:8080 --target 192.168.10.7:80`，C 上 `curl http://<B_overlay>:8080` 返回 A 的页面；A 的访问日志源地址是 B 的 LAN 地址。
- **AC-2** 方向二：B 上 `noeio forward --listen lan:9090 --target <C_overlay>:22`，A 上 `ssh -p 9090 <B_lan_ip>` 连到 C；C 上 `who`/日志显示源地址是 B 的 overlay 地址。
- **AC-3** loopback 目标：B 上 `noeio forward --listen noeio:3000 --target 127.0.0.1:3000`，C 能访问 B 上只绑了 loopback 的服务。
- **AC-4** 大文件传输：方向一传一个 ≥100MB 的文件，校验和一致、不卡死。这条专门验证「代理两端各自协商 MSS，不需要 clamp」的结论。
- **AC-5** 侧隔离：`--listen noeio:...` 的转发，从物理局域网连不上；`--listen lan:...` 的转发，从 overlay 对端连不上。
- **AC-6 Ctrl+C**：转发进程运行中有一条在途连接（例如正在传大文件），按 Ctrl+C：进程在 2 秒内退出、退出码 0、打印退出摘要；在途连接的两端立刻收到 RST/FIN；`ss -ltn`（Linux）/ `netstat -an`（macOS/Windows）里监听端口消失；同一端口可以立刻被再次绑定。
- **AC-7 SIGTERM / SIGHUP**：对 AC-6 的场景分别发 `kill -TERM` 和 `kill -HUP`，结果与 AC-6 一致。
- **AC-8 SIGKILL**：对 AC-6 的场景发 `kill -9`：进程立刻消失（没有摘要）；在途连接两端收到 RST；监听端口消失且可立刻重绑。`nft list ruleset` 与 `sysctl net.ipv4.ip_forward` 与启动前一致（本特性从不碰它们）。
- **AC-9 终端关闭**：在 SSH 会话里启动转发进程后直接断开 SSH（触发 SIGHUP），结果与 AC-7 一致；Windows 上关闭控制台窗口，结果与 AC-6 一致。
- **AC-10 进程互相独立**：两个终端各起一条转发，两条同时可连；对其中一个按 Ctrl+C 或 `kill -9`，只有它的端口消失，另一条继续可连、计数不受影响。
- **AC-11 绑定失败 all-or-nothing**：`--listen lan:<port>` 且本机有两块物理网卡，其中一块地址上的目标端口已被占用：命令不启动、退出码 1、错误指明是哪个地址；另一块网卡的地址**也没有**被绑定。
- **AC-12** 目标挂掉再恢复：目标服务停掉期间连接被拒并计数，目标恢复后无需任何操作即可再次连通，转发进程全程存活。
- **AC-13** 守护进程不在：`noeio forward ...` 拒绝启动，退出码 1，提示 ``Is `noeio boot` running?``。
- **AC-14** 与子网路由共存：同时开启 `advertise_routes` 和端口转发，两条访问路径都通。
- **AC-15** `--allow-from` 生效：不在列表内的源地址被拒绝并计数，退出摘要里能看到拒绝数。

### 8.2 单元与集成测试

- **AC-16** 规则解析与校验的表驱动测试：`--listen` 缺端口、`--listen` 地址既非关键字也非合法 IPv4、`--listen 0.0.0.0:...`、`--listen 127.0.0.1:...`、`--listen` 给了不属于本机的 IP、`udp`（M1 阶段）、端口 0、端口 65536、域名目标、自环、overlay→overlay 中继（关键字 `noeio` 与判为 overlay 侧的具体 IP 各一例，目标为本节点地址与对端地址各一例）、非法 CIDR，全部被拒且错误类型正确。照 `config.rs:299` 现有的 `validate_routes` 测试写法。
- **AC-17** 绑定地址推导的纯函数测试：给定 `--listen` 的三种形态（`noeio:p` / `lan:p` / `ip:p`）、overlay 地址集、物理地址集，产出的 `[SocketAddr]` 与推出的监听侧正确；`lan` 展开时 TUN 地址被排除；具体 IP 不展开。不碰真实 socket。
- **AC-18** 代理转发的回环测试：起一个本地 echo server 当目标，通过规则转发，验证双向字节完整、连接关闭正确传播（一端 FIN 后另一端也收到）。
- **AC-19** 生命周期的进程级测试（`tests/` 下的集成测试，通过 `std::process::Command` 启动 `noeio forward` 子进程）：
  - 建立一条穿过转发的连接并保持；
  - 分别对子进程发 `SIGINT` / `SIGTERM` / `SIGHUP` / `SIGKILL`（Windows 上用 `TerminateProcess` 代替最后一项，其余用 `GenerateConsoleCtrlEvent`）；
  - 断言：子进程在 2 秒内退出；客户端 socket 读到 EOF 或 `ECONNRESET`；监听端口能在 1 秒内被测试进程自己重新绑定。
  - 这条测试需要一个假的守护进程 RPC（返回固定的 `ListVirtualNics`），或者把「查询 overlay 信息」抽成可注入的 trait 让测试跳过。倾向后者，避免在测试里起 Unix socket。
- **AC-20** UDP 会话表（M2）：TTL 到期回收、会话数上限（512）、不同源地址互不干扰。

## 10. 风险与待决问题

| # | 问题 | 说明 | 倾向 |
| --- | --- | --- | --- |
| Q1 | 用户态 vs Linux 内核态 | §5.3 的完整分析。除性能与源地址两条论据不成立外，决定性理由是 nftables 规则不随进程消失，任何内核态实现都会让 `kill -9` 留下残留，直接违反「转发存在当且仅当进程存在」这条不变量 | **三平台统一用户态，内核态移出范围**（§13） |
| Q2 | 绑定后地址集合变化怎么办 | 切 Wi-Fi、DHCP 换地址、守护进程新注册虚拟网卡。可选方案：（a）不处理，用户重启命令；（b）监听地址变化事件重绑；（c）定时重新枚举并 diff | **M1 选 (a)**。前台命令的用户就在终端前，重启一条命令的成本远低于一套地址监听逻辑；`ssh -L` 在同样场景下的行为也是 (a)。如果实际使用中频繁遇到，再考虑 (c) |
| Q3 | 守护进程退出后转发进程要不要跟着退 | 跟着退：行为清楚，但要维持一条到守护进程的长连接或轮询，引入两进程耦合。不跟着退：`lan → overlay` 方向在守护进程重启后自动恢复，但 `overlay` 侧监听器会一直报 accept 错误直到用户重启 | **不跟着退**（FR-3.6）。用 FR-5.3 的规则级 `info` 日志把「绑定地址失效」说清楚，让用户自己决定 |
| Q4 | 转发规则要不要进 `PeerInfo` 广播 | 广播能让 C 知道 B 提供了什么，但要新增字段、走 derper、并回答「能否信任对端宣告的端口」这个信任问题。而且转发是短命前台进程，广播出去的信息几秒后可能就失效 | 本轮不做 |
| Q5 | 目标看不到原始客户端地址 | 用户态和内核态都因为必须 SNAT 而看到 B 的地址，是特性固有行为。对依赖访问日志做审计的场景是实质影响 | 文档写明；给 HTTP/TCP 目标提供可选 `--proxy-protocol` 登记为后续项 |
| Q6 | 要不要给连接数 / 会话数设上限并暴露为选项 | TCP 连接有自然生命周期，上限主要防误用；UDP 会话表无上限是真实的内存耗尽路径 | TCP 不设上限，少一个选项；UDP 会话上限固定 512（FR-4.3），是安全属性不是配置项，实测不够再考虑放开 |
| Q7 | 端口 <1024 | `noeio forward` 因 FR-6.4 需要 root 才能连 RPC socket，所以能绑；但这让一条写错的规则能占掉系统端口 | 允许，绑定时打一条 `warn` |
| Q8 | 要不要提供「不查守护进程」的旁路 | 例如 `--overlay-ip 110.20.0.1` 让非 root 用户也能跑 overlay 侧转发。收益是少一次 RPC、少一个 root 要求；代价是绕过中继拒绝检查（拿不到 `nics[].peer_ips`），且没有守护进程时转发本来就没意义 | **不提供**（FR-6.3）。需要非 root 运行的场景，正确做法是放宽 socket 权限模型，那是另一个需求 |
| Q9 | 多张虚拟网卡时 `noeio` 关键字取哪张 | 取第一张：命令短，但 `NicManager` 无序，且静默只暴露给一个网络；绑全部：符合「暴露给 noeio」的字面含义，多网络节点上暴露面更大 | **绑全部**（FR-6.2a）。要收窄就写具体 IP，横幅会把每个绑定地址都列出来，暴露面一眼可见 |

## 11. 里程碑

| 里程碑 | 内容 |
| --- | --- |
| **M1** | 规则解析与校验（FR-1）、地址推导与 all-or-nothing 绑定（FR-2）、完整生命周期与信号处理（FR-3）、TCP 用户态代理、横幅/日志/退出摘要（FR-5.1–5.4）、`ListVirtualNics` RPC（FR-6.2）、安全提示（FR-8）。验收 AC-1..AC-19 |
| **M2** | UDP 转发与会话表（FR-4）。验收 AC-20 |
| **M3** | 用户文档 `docs/port-forward.md`；同步更新 `docs/subnet-router.md` 关于 LAN 侧发起方向的描述（FR-7.3）；README 双语 Features；`SIGUSR1` 打印计数（FR-5.5） |

## 12. 现有代码改动点

| # | 位置 | 改动 | 里程碑 |
| --- | --- | --- | --- |
| 1 | `noeio/src/cli.rs:13` | `Command` 新增 `Forward { listen: ListenSpec, target: SocketAddrV4, proto: Proto, allow_from: Vec<String> }`。`ListenSpec` 是 `enum { Noeio(u16), Lan(u16), Addr(SocketAddrV4) }`，实现 `FromStr` 供 clap `value_parser` 使用；`proto` 用 `ValueEnum`，默认 `tcp`；用法与退出码见 §6 | M1 |
| 2 | 新建 `noeio/src/forward.rs`（或 `forward/` 目录：`rule.rs` 解析校验、`bind.rs` 地址推导、`proxy.rs` TCP 代理、`stats.rs` 计数） | 转发进程的全部逻辑。**放在 `daemon/` 之外**，它不属于守护进程 | M1 |
| 3 | `noeio/src/main.rs:23` | `match` 新增 `Command::Forward` 分支：连 RPC → `ListVirtualNics` → 校验 → 绑定 → 横幅 → `select!` 等信号 → abort → 摘要 | M1 |
| 4 | `noeio/src/main.rs:118` | `wait_for_shutdown_signal` 扩展为 SIGINT/SIGTERM/SIGHUP 与 Windows 五种控制事件，抽到 `noeio/src/signal.rs` 供 `boot` 与 `forward` 共用 | M1 |
| 5 | `noeio-proto/protos/noeio/v1/virtual_nic.proto:15` | `VirtualNicService` 新增 `ListVirtualNics` 方法与 `VirtualNicEntry` 等三个 message | M1 |
| 6 | `noeio/src/rpc/service/nic.rs` | 实现 `list_virtual_nics`：遍历 `NicManager`（`tun_name`、`ip`），按 `peer_id` 从 `host_info.peers` 取 `network_id`，`peer_ips` 来自 `Router::ips()` | M1 |
| 7 | `noeio/src/rpc/client.rs` | 新增 `list_virtual_nics()`，返回 `Vec<VirtualNicEntry>` | M1 |
| 8 | `noeio/src/daemon/routes.rs:284` | 从 `local_lans` 抽出 `local_addrs(exclude) -> Vec<Ipv4Addr>` 供 FR-2.2 复用 | M1 |
| 9 | `noeio/tests/forward_lifecycle.rs`（新建） | AC-19 的进程级信号测试 | M1 |
| 10 | 新建 `docs/port-forward.md`；改 `docs/subnet-router.md:200`、`README.md:9` Features、`README.zh-CN.md:16` | 用户文档；同步 FR-7.3 提到的方向描述 | M3 |

**不需要改动的地方**，逐条确认过：

- `noeio/src/config.rs`、`config.toml.example` —— 没有转发配置。
- `noeio/src/daemon.rs`、`daemon/reconciler.rs`、`daemon/nat.rs` —— 守护进程不持有任何转发状态，`shutdown` 无需改动。
- `noeio-proto/protos/common/v1/host_info.proto` —— 端口转发不进广播。
- `noeio-derp` 全部 —— 没有新的控制面消息。
- `noeio/src/daemon/router.rs:133` `allowed_source` —— 两个方向的内层源地址都天然合法（§5.1 第 3 点）。
- `noeio/src/interface/virtual_nic.rs` —— TUN 配置不变。
- `noeio-net-route` 全部 —— 用户态方案不碰路由表与 nftables。

## 13. 明确不在范围内

- **Linux 内核态模式（nftables DNAT/SNAT）**。§5.3 已论证性能收益被 boringtun 掩盖、源地址无差异、MSS 需 clamp、`route_localnet` 等额外 sysctl；决定性的原因是它与 §2.1 的生命周期目标不相容：nftables 规则和 `ip_forward` 不随进程消失，`kill -9` 必然留下残留，就需要重新引入子网路由那套原值保存与启动清扫。一份内核态方案的设计草稿（prerouting nat 链、`nat`/`immediate`/`tcp dport` 表达式编码、往 TUN 做 masquerade 时用 `ip daddr` 收窄）保留在提交 `0ce6195` 中，如果将来做成一个独立的、有明确清理语义的特性可以取回。
- IPv6 转发；守护进程承载转发与配置文件入口；转发规则的网络内广播与自动发现（Q4）；基于身份的 ACL；保留原始客户端源地址（Q5）；连接优雅排空；绑定后地址变化的自动重绑（Q2）；不经守护进程的旁路（Q8）；SOCKS/HTTP 代理形态的动态目标；把转发规则做成 Kubernetes Service 那样的多目标负载均衡。
