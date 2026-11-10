# 部署信令、STUN 和中继服务器

`nexa-signal` 一个进程提供三项服务：

| 服务 | 协议与端口 | 作用 |
|---|---|---|
| 信令 | TCP 443，`wss://` | 设备按 9 位 ID 注册和查找，交换加密的打洞信息（§4.3） |
| WebSocket 中继 | 同上，路径 `/relay` | UDP 被封锁时的最后退路（§4.6） |
| STUN | UDP 3478（建议两个地址） | 告诉客户端它的公网地址，判断 NAT 类型（§4.2） |
| UDP 中继 | UDP 3480 | 两端都无法直连时转发流量（§4.6） |

只在局域网或 Tailscale 内使用时**不需要**这台服务器：直接填 `IP:端口` 即可连接。

服务器不受信任：画面、输入和剪贴板都是端到端加密的，服务器看不到内容，也无法冒充设备（§9）。它能看到的只有谁在什么时候连接谁、流量多大。所有状态都在内存里，重启后客户端会自动重新注册。

## 1. 准备

- 一台有公网 IPv4 的 Linux 服务器。有 IPv6 更好。
- 一个域名，例如 `signal.example.com`，解析到这台服务器。
- **最好有两个公网 IP**。STUN 在两个不同 IP 上应答，客户端才能区分容易打洞和难打洞的 NAT，并在难打洞时预测端口。只有一个 IP 也能用，只是两端都在严格 NAT 后面时更常走中继。
- 防火墙放行：TCP 443；UDP 3478（第二个 STUN 地址的端口也要放行）；UDP 3480。

带宽：直连的会话不经过服务器。每个中继会话最多占用 `bandwidth_bps`（默认 50 Mbit/s）。

## 2. 证书

TLS **必须由 nexa-signal 自己终止**，不要放在 nginx、Caddy、Cloudflare 这类 HTTP 反向代理后面。服务器按客户端 IP 限速，同时把发起方的 IP 转给被控端，让被控端按来源限制配对尝试（§9.5）。经过代理后，所有客户端的 IP 都会变成代理的 IP：限速对所有人生效，配对尝试也会被所有人共用。

用 certbot 申请证书，例如 standalone 模式（申请时会临时占用 80 端口）：

```bash
certbot certonly --standalone -d signal.example.com
```

Let's Encrypt 的私钥只有 root 可读，而服务以 uid 10001 运行。安装部署钩子后，钩子会在每次签发或续期后做两件事：把证书复制到 `/etc/nexa-signal/tls/`，然后重启服务（证书只在启动时加载）。

```bash
install -m 0755 deploy/certbot-deploy-hook.sh /etc/letsencrypt/renewal-hooks/deploy/nexa-signal.sh
```

```bash
RENEWED_LINEAGE=/etc/letsencrypt/live/signal.example.com /etc/letsencrypt/renewal-hooks/deploy/nexa-signal.sh
```

第二条命令是手动跑一次钩子，生成第一份证书副本。

## 3. 配置

```bash
mkdir -p /etc/nexa-signal && cp deploy/nexa-signal.toml /etc/nexa-signal/
```

编辑 `/etc/nexa-signal/nexa-signal.toml`，把示例域名和 IP 换成自己的：

- `signal.server_names`：客户端设置里填写的域名。为其他服务器生成的注册会被拒绝。
- `stun.listen`：两个公网 IP 上的 3478 端口。
- `relay.advertise`：客户端访问中继用的公网地址。

**云服务器的公网 IP 不在网卡上**（AWS、GCP、阿里云等用 1:1 NAT）时，监听地址要写内网地址，再用 `advertise` 告诉客户端对应的公网地址：

```toml
[stun]
listen    = ["10.0.0.5:3478", "10.0.0.6:3478"]
advertise = ["203.0.113.10:3478", "203.0.113.11:3478"]
```

检查配置和证书：

```bash
nexa-signal --check-config /etc/nexa-signal/nexa-signal.toml
```

输出 `OK` 即可。如果配置会降低连接成功率（只有一个 STUN 地址、没有中继、没设 `server_names`），这里会给出警告。

## 4. 运行

两种方式任选其一。

### systemd

在服务器上编译，或者在同架构的 Linux 机器上编译后复制过去：

```bash
cargo build --release --locked -p nexa-signal
```

```bash
install -m 0755 target/release/nexa-signal /usr/local/bin/
```

```bash
useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin nexa-signal
```

```bash
install -m 0644 deploy/nexa-signal.service /etc/systemd/system/ && systemctl daemon-reload && systemctl enable --now nexa-signal
```

单元文件以非 root 用户运行，只授予绑定 443 端口的权限，文件系统只读。每次启动前会先运行 `--check-config`。日志查看方法：

```bash
journalctl -u nexa-signal -f
```

### Docker Compose

```bash
cd deploy && docker compose up -d --build
```

使用宿主机网络（`network_mode: host`）。STUN 需要看到客户端的真实地址，服务器按客户端 IP 限速，而 Docker 的端口映射会把这些地址都换掉。镜像以 uid 10001 运行，读取宿主机的 `/etc/nexa-signal`。

## 5. 让设备使用它

- **NexaDesk 应用**：在主窗口顶部的“信令服务器”填 `wss://signal.example.com` 并保存。被控端需要先停止再重新开始“允许别人控制本机”，设置才会生效。
- **命令行被控端**：`nexa-agent --listen 0.0.0.0:47800 --signal wss://signal.example.com`。

被控端注册成功后，窗口里会显示 9 位设备 ID。控制端输入这个 ID 和一次性码即可连接。

自签证书只适合测试：把 CA 证书以 `signal-ca.pem` 为名放进两端的数据目录（macOS 默认为 `~/Library/Application Support/NexaDesk`）。命令行被控端也可以用 `--signal-ca` 指定。正式使用请用公开 CA 签发的证书，否则每台设备都要手动放置这个文件。

## 6. 运维

- **停止**：服务收到 SIGTERM（systemd 停止服务或 `docker stop` 时发出）后正常关闭。
- **升级**：替换二进制或镜像后重启。被控端会在几秒内重新注册。直连的会话不经过服务器，不受影响。中继令牌的签名密钥只在内存里，重启后旧令牌全部失效，所以**走中继的会话会断开**，控制端会在 30 秒的重连窗口内尝试恢复（§4.4）。最好在没人使用时升级。
- **限速**：`[limits]` 的默认值能防住常见的扫描和滥用。用 `nexa-signal --example-config` 可以看到全部选项。
- **未验证**：这些部署文件还没在真实的 Linux 服务器和 Docker 上运行过。已经验证的只有两项：Linux x86_64 的类型检查，以及 macOS 上的 `--check-config` 和 SIGTERM 处理。
