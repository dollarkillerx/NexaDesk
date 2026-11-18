# 部署信令、STUN 和中继服务器

`nexa-signal` 一个进程提供三项服务：

| 服务 | 协议与端口 | 作用 |
|---|---|---|
| 信令 | TCP 443，`wss://` | 设备按 9 位 ID 注册和查找，交换加密的打洞信息（§4.3） |
| WebSocket 中继 | 同上，路径 `/relay` | UDP 被封锁时的最后退路（§4.6） |
| STUN | UDP 3478（建议两个地址） | 告诉客户端它的公网地址，判断 NAT 类型（§4.2） |
| UDP 中继 | UDP 3480 | 两端都无法直连时转发流量（§4.6） |
| 浏览器网页（可选） | 同信令端口，`https://` | 打开 `[web]` 后提供浏览器端网页和 `/web`，让浏览器从任何网络按设备 ID 连接（WEB_CLIENT.md §12） |
| TURN（可选） | 同第一个 STUN 端口，UDP | 打开 `[web]` 后提供，浏览器会话的画面经此中转 |

**这台服务器是可选的，大多数人不需要它。** NexaDesk 默认不用服务器：同一局域网会自动发现并直接连接；不在同一网络时，推荐两边都安装 Tailscale 并登录同一个账号，Tailscale 会自动打洞，打不通时自动中转。只有不能用 Tailscale、又想按 9 位设备 ID 连接时，才需要按本文自建。

服务器不受信任：画面、输入和剪贴板都是端到端加密的，服务器看不到内容，也无法冒充设备（§9）。它能看到的只有谁在什么时候连接谁、流量多大。所有状态都在内存里，重启后客户端会自动重新注册。

## 0. 一键安装（推荐）

在一台 Linux 服务器（x86_64、systemd，有 root）上，一条命令装好并启动：

```bash
curl -fsSL https://raw.githubusercontent.com/dollarkillerx/NexaDesk/main/deploy/install.sh | sudo sh
```

脚本会：下载最新发布的程序并校验 SHA-256（没有校验文件就不安装）；自动找到本机公网 IP；生成一个随机的信令口令；做一份私有 CA 签发的证书（IP 或域名都可以）；写好配置、创建 `nexa-signal` 用户和 systemd 服务并启动；给正在运行的 ufw / firewalld 放行端口；最后打印**服务器地址、口令和证书指纹**，同样的内容保存在 `/etc/nexa-signal/INFO`（只有 root 可读）。客服把地址和口令填进应用的“信令服务器”和“访问口令”即可，应用第一次连接会弹出证书指纹，和脚本打印的核对一致后点信任。

常用选项（管道方式要写在 `sh -s --` 后面）：

```bash
curl -fsSL https://raw.githubusercontent.com/dollarkillerx/NexaDesk/main/deploy/install.sh | sudo sh -s -- --host signal.example.com --web
```

- `--host <IP 或域名>`：客户端填写的地址；默认是本机公网 IP。写域名时，它必须已经解析到这台服务器。
- `--key <口令>`：自己指定口令（至少 10 位，只能用字母、数字和 `. _ ~ + = @ % : -`）；默认随机生成。
- `--web [密码]`：同时打开浏览器网页版；密码可不写（不设密码）。
- `--port <端口>`：信令端口，默认 443。
- `--binary <文件>`：安装自己编译的程序，不下载。
- `sh install.sh --show` 重新显示地址和口令；`--uninstall` 卸载服务（保留配置和证书），再加 `--purge` 连配置一起删除。

可以重复运行：已有的口令、证书、`[web]` 设置会保留，程序和服务单元更新到最新；证书只有在快到期（不足 30 天）或地址变了才重新签发，签发时沿用原来的 CA，所以用户信任过的指纹不变。证书有效期 398 天，一年内再运行一次脚本即可续期。

脚本**不会**替你改云厂商控制台里的安全组：阿里云、腾讯云、AWS 等需要自己放行 TCP 443（或所选端口）、UDP 3478、UDP 3480，装完后从另一台机器 `nc -z <地址> 443` 确认。私有证书只适合自己人用；要给公众用，请按 §2 换成域名和公开 CA 的证书（把 `tls_cert`、`tls_key` 指向它，重启服务）。

下面的 §1 起是手工部署，想自己控制每一步时用。

## 1. 准备

- 一台有公网 IPv4 的 Linux 服务器。有 IPv6 更好。
- 一个域名，例如 `signal.example.com`，解析到这台服务器。
- **最好有两个公网 IP**。STUN 在两个不同 IP 上应答，客户端才能区分容易打洞和难打洞的 NAT，并在难打洞时预测端口。只有一个 IP 也能用，只是两端都在严格 NAT 后面时更常走中继。
- 防火墙放行：TCP 443；UDP 3478（第二个 STUN 地址的端口也要放行）；UDP 3480。
  - 服务器上可能不止一层防火墙：firewalld 之外，还可能有自己写的 nftables 表（`nft list tables`）、fail2ban 的表，以及云厂商控制台里的安全组。每一层都要放行。
  - 放行后从**另一台机器**确认能连通，不要只在服务器本机测：`nc -z <服务器> 443`。
- 443 已被别的服务占用时，信令可以用任意端口，例如 `listen = "0.0.0.0:8443"`，客户端填 `wss://signal.example.com:8443`。

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

### 信令口令（必填）

信令服务器**必须**设置口令：没有口令，配置检查会失败，服务器不会启动。在 `[signal]` 里写：

```toml
access_secrets = ["一段够长、别人猜不到的口令"]
```

- 至少 10 个字符，首尾不能有空格或换行。`--example-config` 里的示例口令会被拒绝，必须换成自己的。
- 口令的作用是只让知道它的设备使用这台服务器：注册、按 ID 查找、领取中继，都要先证明自己知道口令。
- 口令不会在网络上传输。设备发送的是一个 HMAC 证明，绑定了服务器刚发来的随机数、设备拨的服务器名和设备自己的公钥，所以截获的证明不能用在别的连接、别的服务器或别的设备上。服务器日志里也看不到口令。
- 可以同时写两个，用来换口令而不让所有人同时掉线：先加入新口令，发给用户，等大家都换了再删旧口令，重启服务。
- 它只管“谁能用这台服务器”。谁能连到哪台被控端，仍然由两端之间的配对、一次性码和确认决定（§9），与口令无关。服务器被人白用最多是占带宽，看不到画面、输入和剪贴板。
- 不挡的是 STUN（回应公网地址的公开服务，有按 IP 限速）。浏览器网页版有它自己的密码，见下。
- 口令短时，如果有人把设备指向一台不可信的服务器，那台服务器能拿到证明去离线猜口令：用长一点的。口令写在配置文件里，文件只让运行服务的用户可读（`chmod 600`）。

设备那边：应用的“信令服务器”旁边必须填同一个口令（存在钥匙串里，不写进设置文件）；命令行被控端用环境变量，不要写在命令行里，否则同机其他进程能看到：

```bash
NEXA_SIGNAL_SECRET='一段够长、别人猜不到的口令' nexa-agent --listen 0.0.0.0:47800 --signal wss://signal.example.com
```

口令不对时，被控端会在状态里显示“the server needs its access password”，每 30 秒才重试一次；控制端按 ID 连接会收到同样的提示。

### 浏览器的密码（可选）

打开 `[web]` 后，网页版和 TURN 对浏览器开放。浏览器没法持有上面的信令口令，所以网页版有**另一个、单独的**密码，可设可不设：

```toml
[web]
password = "人们能输入的密码"
```

- 设了：网页在设备 ID 旁边多出“访问密码”一栏，服务器只在密码正确时才让浏览器通过、才发 TURN 凭据，并且先于告诉对方“这个 ID 在不在线”。每次尝试都计入按 IP 的限速。
- 不设：浏览器在这里不被问任何东西。什么能让浏览器进入某台设备，仍然是那台设备自己的密码、一次性码或确认（§9）。启动时会提醒这一点。
- 这个密码在 TLS 里随网页的第一条消息发送，不是 HMAC 证明，所以它只适合“拦住路过的人”，不要和别处共用。

检查配置和证书：

```bash
nexa-signal --check-config /etc/nexa-signal/nexa-signal.toml
```

输出 `OK` 即可，并且会写明信令口令和浏览器密码的情况。如果配置会降低连接成功率（只有一个 STUN 地址、没有中继、没设 `server_names`），或 `[web]` 没有密码，这里会给出提醒。

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

小内存的服务器（2 GB 以下）编译时可能内存不足，影响上面已经在跑的服务。这时可以在 Mac 上编译一个静态链接的 Linux x86_64 程序，任何发行版都能直接运行，不需要 Docker：

```bash
scripts/build-signal-linux.sh
```

产物是 `target/x86_64-unknown-linux-musl/release/nexa-signal`，复制到服务器后同样用 `install` 安装。

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

- **NexaDesk 应用**：在主窗口的“信令服务器”填 `wss://signal.example.com`，并在旁边的“访问口令”填服务器的口令（必填），然后保存。被控端需要先停止再重新开始“允许别人控制本机”，设置才会生效。
- **命令行被控端**：`nexa-agent --listen 0.0.0.0:47800 --signal wss://signal.example.com`。

被控端注册成功后，窗口里会显示 9 位设备 ID。控制端输入这个 ID 和一次性码即可连接。

**浏览器从任何网络连接（可选）。**在配置里加上：

```toml
[web]
max_allocations = 200   # 同时存在的 TURN 分配；每个浏览器会话占两个
max_per_ip = 8
```

不需要新的端口：网页在信令端口上（`https://signal.example.com/`），TURN 和第一个 STUN 地址共用一个 UDP 端口。被控端那边要注册在这台服务器上，并打开“允许浏览器连接”；应用里会列出这个网址和设备 ID。对方用浏览器打开网址，输入设备 ID 和一次性码即可。所有浏览器会话的画面都经过服务器，按需要预留带宽。

自签证书只适合测试。应用第一次连接自签服务器时（保存服务器地址、按设备 ID 连接、或开启允许控制时）会弹窗显示证书指纹，问是否信任；信任后保存为数据目录里的 `signal-ca.pem`（macOS 为 `~/Library/Application Support/NexaDesk`，Windows 为 `%APPDATA%\NexaDesk`），控制端和被控端都会用。请先在服务器上核对指纹：`openssl x509 -noout -fingerprint -sha256 -in <CA 证书>`。也可以手动把 CA 证书以 `signal-ca.pem` 为名放进数据目录；命令行被控端用 `--signal-ca` 指定。正式使用请用公开 CA 签发的证书。

## 6. 公开服务列表（服务发现）

想把自己的服务免费给大家用，可以在 GitHub 上放一个 JSON 文件，列出服务地址和口令；应用读取这个文件，就不用每个人手工填。项目仓库根目录的 [`servers.json`](../servers.json) 就是这样一个文件：

```json
{
  "endpoints": [
    { "endpoint": "wss://signal.example.com", "key": "服务器配置里 access_secrets 的一项" }
  ]
}
```

- `endpoint` 必须是 `wss://`；`key` 不能为空，也不能含控制字符，否则这一项会被忽略，其余照常使用。信令服务始终要口令（§3），所以没有“免口令”的条目。
- 放在公开文件里的口令是**公开的**，它只能挡住不知道这个文件的人，不是保密。浏览器的密码是另一回事，见上文，不在这个文件里。
- 要换口令：先在服务器的 `access_secrets` 里同时放新旧两个，更新文件，等一段时间后再去掉旧的。
- 应用读取时：只接受 https，8 秒超时，文件不超过 64 KB，最多取 16 项，地址重复的取第一项；读取成功后存在数据目录的 `servers.json`，下次读取失败（离线、GitHub 打不开）就用它；读到的内容不是这种文件时，保持原来的不变。
- 官方服务的地址和口令在发布时才写进这个文件；在此之前列表是空的。

## 7. 运维

- **停止**：服务收到 SIGTERM（systemd 停止服务或 `docker stop` 时发出）后正常关闭。
- **升级**：替换二进制或镜像后重启。被控端会在几秒内重新注册。直连的会话不经过服务器，不受影响。中继令牌的签名密钥只在内存里，重启后旧令牌全部失效，所以**走中继的会话会断开**，控制端会在 30 秒的重连窗口内尝试恢复（§4.4）。最好在没人使用时升级。
- **限速**：`[limits]` 的默认值能防住常见的扫描和滥用。用 `nexa-signal --example-config` 可以看到全部选项。
- **验证情况**（2026-11-11）：
  - 已在一台 Rocky Linux 9（x86_64，1.7 GB 内存）上按本文的 systemd 方式部署：程序用 `scripts/build-signal-linux.sh` 在 Mac 上编译；证书用自签 CA，签给服务器的 IP。单元文件原样可用。
  - 被控端用 `--relay-only` 运行，控制端按 9 位 ID 跨公网连接：UDP 中继连续 12 次全部成功，画面 60 fps，往返约 21 ms；被控端绑在 127.0.0.1、UDP 出不去时，会自动改走 WebSocket 中继，会话照常建立。
  - 这次实测发现并修复了一个问题：Linux 内核会把同一来源的一串 UDP 包合并后一次交给程序（GRO），旧版中继把合并后的整块当成一个包，导致视频全部丢失。**升级前部署的服务器请换成新版本。**
  - 2026-11-15 又在同一台服务器上实测了 `[web]`：浏览器打开 `https://服务器:8443/`，按设备 ID 连到演示主机，画面和输入经服务器的 TURN 中转（往返约 20 ms），17 项自动化检查全部通过。
  - Docker Compose 方式仍未在真实服务器上运行过。
