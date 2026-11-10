# NexaDesk

轻量的远程控制软件。目标是高清、低延迟，粘贴可靠，键盘对 Win / Mac 友好，能自己打洞，也能借助 Tailscale。界面用 Avalonia，后端用 Rust。

> **状态**：macOS ↔ macOS 的功能已经写完，自动化测试全部通过，但**还没在两台真实的 Mac、真实的网络上验证过**。Web 端（桌面浏览器查看和控制）已完成第一版，只用演示主机在浏览器里验证过；Windows 端尚未开始。暂时不建议用在重要场合。

## 特点

- **传输**：一个 UDP socket 上跑 QUIC。控制走可靠流，视频走 DATAGRAM，每帧带 Reed-Solomon 纠错。拥塞控制由应用层自己做，丢一个包不会卡住后面的帧。
- **连接，默认不用服务器**：
  - 局域网：mDNS 自动发现，直接连接。
  - 跨网络：**推荐 Tailscale**。两边登录同一个 Tailscale 账号即可互连，Tailscale 负责打洞，打不通时自动中转。NexaDesk 会列出 tailnet 里运行 NexaDesk 的设备，并标明是直连还是经 DERP 中转。
  - 可选的自建服务器：按 9 位设备 ID 连接，自己打洞（端口映射依次尝试 PCP、NAT-PMP、UPnP），打不通就走 UDP 中继，UDP 被封锁时走 WebSocket 中继。
- **画面**：macOS 用 ScreenCaptureKit 采集，VideoToolbox 硬件编码 HEVC / H.264，用低延迟码率控制和长期参考帧（LTR）。丢帧后只让失效的帧之后的画面重新参考，不必等关键帧。
- **键盘**：按物理键码传输，控制端改写组合键意图。例如在 Mac 上按 ⌘C 控制 Windows 时发出 Ctrl+C，而不是全局对调 Ctrl 和 Cmd。映射表可以编辑和预览。
- **剪贴板与粘贴**：按代际同步。远端粘贴前先等剪贴板到达对端，再放行粘贴键，避免粘贴出旧内容。
- **游戏模式**：锁定指针，发送相对移动；FEC 更强，延迟更低。
- **浏览器控制**：对方不用安装任何软件，用浏览器打开本机地址的 47801 端口，输入一次性码即可查看和控制。页面和 WebRTC 都由本机提供，不需要服务器；适合局域网和 Tailscale。默认关闭，在主窗口底部打开“允许浏览器连接”。
- **其他**：光标形状、声音（Opus）、文件收发、多显示器、断线 30 秒内自动重连、HUD 与连接诊断。
- **多语言**：简体中文、English、日本語。默认跟随 macOS 的语言设置，也可以在主窗口右上角切换（重启后生效）。
- **安全**：
  - 双向 TLS，身份就是设备公钥。
  - 首次连接用一次性码或口令做 SPAKE2 配对，并绑定到 TLS 会话；之后钉住对方公钥。
  - 每次被控都要本机确认（可对已配对设备设为无人值守），并显示提示条。
  - 配对猜测按来源限制。

## 仓库结构

| 路径 | 内容 |
|---|---|
| `crates/nexa-wire` | 线上格式：数据报、帧、控制消息、信令消息 |
| `crates/nexa-net` | 自定义 QUIC socket、拥塞与码率控制、FEC、STUN、打洞、路径管理、端口映射、Tailscale、mDNS |
| `crates/nexa-media` | 采集、编解码、呈现、光标、音频（macOS 实现） |
| `crates/nexa-input` | 组合键改写、键位表、输入捕获与注入 |
| `crates/nexa-clip` | 剪贴板同步与粘贴闸门 |
| `crates/nexa-auth` | 设备身份、配对、钉住、访问决策 |
| `crates/nexa-session` | 会话握手、被控端与控制端的会话逻辑、文件传输、重连、会合 |
| `crates/nexa-agent` | 被控端进程 `nexa-agent` |
| `crates/nexa-client` | 控制端动态库 `libnexa_client`（C ABI，供 UI 调用） |
| `crates/nexa-signal` | 信令 + STUN + 中继服务器 `nexa-signal` |
| `crates/nexa-web` | 浏览器访问：网页、信令和 WebRTC（str0m），由被控端直接提供 |
| `crates/nexa-web-pake` | 浏览器一侧的配对（SPAKE2），编译为 WebAssembly |
| `ui/` | Avalonia 桌面应用（.NET 10，NativeAOT）与测试 |
| `deploy/` | 服务器的 systemd 单元、Docker 镜像与 Compose、配置模板、证书续期钩子 |
| `docs/` | 设计文档、部署说明、M0 验证报告 |
| `spikes/` | M0 阶段的验证代码 |

## 构建（macOS）

需要：

- macOS 14 或更高；
- Xcode 命令行工具；
- Rust stable（edition 2024）；
- .NET 10 SDK。

打包成一个应用：

```bash
scripts/package-macos.sh
```

产物是 `dist/NexaDesk.app`，ad-hoc 签名（自签）。每次重新打包，macOS 都会把它当成新应用，“屏幕录制”和“辅助功能”权限需要重新授予。

应用图标在 `assets/icon/`（`AppIcon.icns` 和 1024 px 原图），已提交到仓库，打包时直接使用。图标由 `scripts/make-app-icon.swift` 用代码绘制；修改后运行 `scripts/make-app-icon.sh` 重新生成。

只编译 Rust 部分：

```bash
cargo build --release -p nexa-agent -p nexa-client
```

## 使用

1. 两台 Mac 都打开 NexaDesk.app，按提示授予“屏幕录制”和“辅助功能”权限，首次在局域网使用时还要允许“本地网络”。
2. 不在同一个局域网时，两台都安装 [Tailscale](https://tailscale.com/download) 并登录同一个账号。同一局域网内不需要。
3. 被控的一台点“允许别人控制本机”。默认监听所有网络，窗口里会列出对方可以输入的地址（Tailscale 名称、局域网 IP）和一次性码。
4. 控制的一台选择对方，然后输入一次性码：
   - 在“局域网中的 NexaDesk 设备”或“Tailscale 网络中的 NexaDesk 设备”里点“连接”；
   - 也可以手动输入对方的地址，例如 `192.168.1.20` 或 `studio.tail1234.ts.net`（默认端口 47800）。
5. 被控端确认后开始控制。配对过的设备下次可以直接连接。

不能用 Tailscale 时，可以自建信令服务器，按 9 位设备 ID 连接：部署方法见 [docs/DEPLOY.md](docs/DEPLOY.md)，然后在主窗口的“高级：自建信令服务器（可选）”里填写地址。

被控端也可以从命令行运行，在终端里确认连接：

```bash
nexa-agent --listen '[::]:47800'
```

`nexa-agent --help` 列出全部选项，例如 `--paired-only`、`--relay-only`、`--no-lan-discovery`。

## 测试

```bash
cargo test --workspace
```

```bash
dotnet test ui/NexaDesk.Tests
```

需要真实硬件或网络的测试默认忽略，例如真实组播、屏幕采集，以及会触发系统权限提示的测试。

试用浏览器端，不需要屏幕录制权限（合成画面，经真实 H.264 编码器；收到的鼠标和按键会画进画面）：

```bash
cargo run -p nexa-web --example demo
```

然后在另一台设备的浏览器，或本机浏览器，打开本机的局域网地址（不要用 127.0.0.1）的 47801 端口，输入终端里显示的一次性码。改了 `crates/nexa-web-pake` 之后，运行 `scripts/build-web-pake.sh` 重新生成 `crates/nexa-web/web/pake.wasm`。

## 文档

- [docs/DESIGN.md](docs/DESIGN.md)：总体设计与里程碑
- [docs/DETAILED_DESIGN.md](docs/DETAILED_DESIGN.md)：详细设计，各章末尾的“实现修订”记录了实际实现与设计的差异
- [docs/DEPLOY.md](docs/DEPLOY.md)：部署信令、STUN 和中继服务器
- [docs/WEB_CLIENT.md](docs/WEB_CLIENT.md)：Web 控制端的方案与第一版实现
- [docs/spikes/M0-report.md](docs/spikes/M0-report.md)：M0 验证报告

## 尚未完成

- **验证**：两台 Mac 的实机联调；真实 NAT 环境下的打洞测试。
- **macOS 发布**：Developer ID 签名与公证、安装包、自动更新，以及登录界面和锁屏前的远程访问。
- **其他平台**：Windows 端（被控与控制）。
- **Web 端后续**：手机手势和虚拟键盘、输入法、剪贴板、跨公网（TURN）、HTTPS；真实浏览器和手机上的延迟实测。
- **产品决定**：无人值守机器的首次配对方式；知道 ID 的人默认能做什么；文件权限是否按方向拆开。
