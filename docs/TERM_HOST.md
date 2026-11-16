# 命令行被控端（nexa-term-host）

没有图形界面、只要命令行的机器（Linux 服务器、Windows、也可以是 Mac）用它当被控端：
它在自建信令服务器上注册一个设备 ID，对方在浏览器里打开服务器的网页，选“终端”，填设备 ID，
就能用上这台机器的 shell。Linux 和 macOS 上是用户的登录 shell，Windows 上是 PowerShell。

## 构建

```sh
scripts/build-term-host.sh
# dist/term-host/nexa-term-host-linux-x86_64        静态链接（musl），任何发行版都能跑
# dist/term-host/nexa-term-host-windows-x86_64.exe  Windows 10 1809 及以上（需要 ConPTY）
```

在 macOS 上交叉编译：Linux 用 rustup 自带的 rust-lld，Windows 用 Homebrew 的 mingw-w64。
str0m 的 DTLS 依赖 dimpl 被打了一个小补丁，见 `third_party/README.md`。

## 登录方式

| 方式 | 被控端要做什么 |
|---|---|
| 固定密码 | 机主自己的入口：直接进入，不用确认。先运行 `nexa-term-host set-password`，从标准输入读密码（至少 8 位，不能长得像一次性码） |
| 一次性码 | 启动时打印在控制台；有人用它连接时，控制台问 `Allow? [y/N]` |
| 只填设备 ID | 同样在控制台问，并注明“只凭设备 ID，未经验证” |

作为后台服务运行（没有控制台）时，只有固定密码能登录，所以必须先设好密码，否则拒绝启动。

## 运行

```sh
nexa-term-host --signal wss://signal.example.com:8443 [--signal-ca ca.pem] [--max 4]
```

- `--signal-ca`：自签名服务器的 CA，默认读数据目录里的 `signal-ca.pem`
- `--max`：同时最多几个人（默认 4），每人一个独立的 shell
- 数据目录：Linux 是 `~/.config/nexadesk`，Windows 是 `%APPDATA%\NexaDesk`，macOS 是
  `~/Library/Application Support/NexaDesk-term`，可用 `--data-dir` 改
- `reset-device-id`：下次启动换一个新的设备 ID，旧的失效

shell 以运行 nexa-term-host 的那个用户身份运行。不要用 root 跑，给它一个普通用户。

### Linux：systemd 服务

```ini
# /etc/systemd/system/nexa-term-host.service
[Unit]
Description=NexaDesk command-line host
After=network-online.target
Wants=network-online.target

[Service]
User=nexadesk
ExecStart=/usr/local/bin/nexa-term-host --signal wss://signal.example.com:8443
Restart=always

[Install]
WantedBy=multi-user.target
```

```sh
sudo -u nexadesk nexa-term-host set-password     # 输入密码后回车
sudo systemctl enable --now nexa-term-host
journalctl -u nexa-term-host                      # 设备 ID 在这里
```

### Windows

在 PowerShell 里：

```powershell
.\nexa-term-host-windows-x86_64.exe set-password
.\nexa-term-host-windows-x86_64.exe --signal wss://signal.example.com:8443
```

密钥和密码校验值用 DPAPI 加密后存放，只有同一台机器上的同一个 Windows 用户能解开。

## 安全

- 会话结束（关闭终端、断开、关页面）时，shell 和它启动的所有进程一起结束：Linux/macOS 上对整个
  进程组先挂断（SIGHUP）、1.5 秒后强制结束；Windows 上整个进程树在一个作业对象（job object）里，
  随会话一起结束
- 固定密码只保存 Argon2id 校验值；输错次数有限制
- 画面、键盘这类权限在这里都不存在，只有命令行
- NexaDesk 应用（非浏览器）目前连不了命令行被控端
