# NexaDesk

轻量的远程控制软件。目标是高清、低延迟，粘贴可靠，键盘对 Win / Mac 友好，能自己打洞，也能借助 Tailscale。界面用 Avalonia，后端用 Rust。

> **状态**：macOS ↔ macOS 的功能已经写完，自动化测试全部通过，但**还没在两台真实的 Mac、真实的网络上验证过**。浏览器端（查看、控制、命令行，含手机、输入法、剪贴板，可经自建服务器跨网络）已在 Chrome 中做过自动化端到端测试，并经公网服务器实测过。Linux / Windows 的命令行被控端已能构建，Linux 版已在服务器上注册运行，Windows 版尚未实机运行。图形化的 Windows 端尚未开始。暂时不建议用在重要场合。

## 截图

主窗口：左边连接别的设备（局域网和 Tailscale 里的设备会自动列出），右边允许别人控制本机：最上面是放大显示的设备 ID 和一次性码，点一下就复制，设备 ID 可以重置；下面是固定密码和对方可输入的地址。自动跟随系统的浅色和深色。

<p>
  <img src="docs/images/main.png" width="49%" alt="主窗口（浅色）">
  <img src="docs/images/main-dark.png" width="49%" alt="主窗口（深色）">
</p>

有人连接时，本机弹出确认窗口：显示对方是谁、怎么验证的、密钥指纹，由你勾选允许的权限。“拒绝”是默认按钮，“允许”在窗口出现一秒后才能点。用固定密码连接的是机主本人，不弹窗。浏览器端不用安装任何软件，连接时可以选“屏幕”还是“终端”；连过的设备可以保存在浏览器里，下次点一下就能连上。

<p>
  <img src="docs/images/consent.png" width="36%" alt="连接确认">
  <img src="docs/images/web.png" width="62%" alt="浏览器端连接页">
</p>

浏览器里看到并操作对方的屏幕。顶栏显示连接方式、延迟和帧率；展开后可以看丢包和码率，切换画质和分辨率（自动时跟浏览器窗口一样清晰）。选“终端”时只打开对方的命令行，不传画面。

<p>
  <img src="docs/images/web-session.png" width="56%" alt="浏览器端会话与网络面板">
  <img src="docs/images/web-terminal.png" width="42%" alt="浏览器端命令行">
</p>

手机上同样可用：顶栏换成图标，横屏时给画面让出更多空间，弹出键盘时多一排 Esc、Tab、修饰键和方向键。对方的指针由浏览器自己画出来，不等画面传回，点哪里就立刻显示在哪里。

<p>
  <img src="docs/images/web-phone.png" width="26%" alt="手机上的连接页：已保存的连接，其中一个正在编辑">
  <img src="docs/images/web-phone-session.png" width="72%" alt="手机横屏控制，带按键栏">
</p>

控制别人时，画面上方是工具栏（模式、声音、文件、HUD、诊断）。被控制时，本机顶部一直显示提示条，每个连接的人一行，可以单独断开；挡住画面时可以收起成一个小标签，但不会完全隐藏。

<p>
  <img src="docs/images/toolbar.png" width="80%" alt="控制端工具栏"><br>
  <img src="docs/images/banner.png" width="60%" alt="被控端提示条（两人同时连接）">
  <img src="docs/images/banner-folded.png" width="14%" alt="收起后的提示条">
</p>

截图里的设备、地址、代码和已保存的连接都是虚构的。界面改动后运行 `scripts/screenshots.sh` 重新生成：应用界面由 `ui/NexaDesk.Screenshots` 用真实的界面代码离屏渲染；浏览器页面由 `scripts/web-screenshots.mjs` 在无界面的 Google Chrome 里对演示主机走一遍真实的会话截取（电脑和手机尺寸各一遍），终端用的是一个干净的 `/bin/sh`。

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
- **浏览器控制**：对方不用安装任何软件。
  - 局域网和 Tailscale 里，用浏览器打开 `https://本机地址:47801`，页面和 WebRTC 都由本机提供，不经过服务器。
  - 有自建服务器时，从任何网络打开服务器的网址、输入设备 ID 连接。先经服务器的 TURN 中转；本机点了允许之后，才把自己的局域网和 Tailscale 地址告诉浏览器，同一网络里自动切到直连。
  - 指针在浏览器本地画：对方只发指针的形状和位置，不画进画面，所以鼠标移动没有延迟。别人（对方本人、另一个连接者、程序）移动的指针照样显示在对方说的位置。
  - 连过的设备可以保存在浏览器里，可以存多台，点一下直接连：保存了固定密码的直接登录，没保存的向对方发连接请求。每台可以改名、更新或不再保存密码、删除。密码先用浏览器生成的、页面也导不出的密钥加密再存；一次性码从不保存。
  - 手机可用触控手势和屏幕键盘，页面按手机屏幕排版（顶栏换成图标，横屏时更紧凑）；输入法照常可用，文字剪贴板双向同步，非 Mac 键盘的 Ctrl+C/V 自动对应 ⌘C/V。页面支持中文、English、日本語。
  - 顶栏显示连接方式、延迟和帧率；画质可选自动、清晰、均衡、弱网，分辨率可选自动（按浏览器窗口的实际像素）、原生、1080p、720p。
  - 默认关闭，在主窗口底部打开“允许浏览器连接”。
- **登录方式**：一次性码（对方点允许）；固定密码（机主自己的入口，直接进入）；经服务器时也可以只填设备 ID，对方会看到标明“无法核实身份”的请求。设备 ID 可以随时重置。
- **多人同时连接**：最多 4 人同时连接同一台 Mac（应用和浏览器合计），每人单独确认，都能操作，互不顶替；谁离开就只结束谁的会话。本机提示条每人一行，可以单独断开，也可以收起。
- **命令行**：浏览器连接时可以选“屏幕”或“终端”。终端打开被控端的 shell（xterm.js），是单独一项权限，默认不勾；会话结束时 shell 和它启动的所有进程一起结束。没有图形界面的 Linux / Windows 机器用命令行被控端 `nexa-term-host`（bash / PowerShell），见 [docs/TERM_HOST.md](docs/TERM_HOST.md)。
- **其他**：光标形状、声音（Opus）、文件收发、多显示器、断线 30 秒内自动重连、HUD 与连接诊断。
- **多语言**：简体中文、English、日本語。默认跟随 macOS 的语言设置，也可以在主窗口右上角切换（重启后生效）。
- **安全**：
  - 双向 TLS，身份就是设备公钥。
  - 首次连接用一次性码或固定密码做 SPAKE2 配对，并绑定到 TLS 会话；之后钉住对方公钥。固定密码在“允许别人控制本机”下设置，只保存 Argon2id 的结果，和一次性码同时有效，原生端和浏览器都能用。
  - 被控时本机要确认（可对已配对设备设为无人值守；固定密码直接进入），并一直显示提示条。
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
| `crates/nexa-web` | 浏览器访问：HTTPS、信令和 WebRTC（str0m），由被控端提供；经服务器的会话走 TURN |
| `crates/nexa-web-page` | 浏览器端网页，由被控端和信令服务器共用 |
| `crates/nexa-turn` | TURN 服务器与客户端（纯状态机），只在同一服务器的分配之间中转 |
| `crates/nexa-web-pake` | 浏览器一侧的配对（SPAKE2），编译为 WebAssembly |
| `crates/nexa-term` | 伪终端上的 shell（Linux / macOS 用 PTY，Windows 用 ConPTY），随会话结束整个进程树 |
| `crates/nexa-term-host` | 只有命令行的被控端 `nexa-term-host`，用于 Linux、Windows 服务器 |
| `ui/` | Avalonia 桌面应用（.NET 10，NativeAOT）、测试和 README 截图工具 |
| `deploy/` | 服务器的 systemd 单元、Docker 镜像与 Compose、配置模板、证书续期钩子 |
| `docs/` | 设计文档、部署说明、M0 验证报告 |
| `spikes/` | M0 阶段的验证代码 |
| `third_party/` | 打了小补丁的第三方代码（dimpl，让 Linux 版不依赖 aws-lc），说明见其中的 README |

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

产物是 `dist/NexaDesk.app`。默认是 ad-hoc 签名：每次重新打包，macOS 都会把它当成新应用，“屏幕录制”“辅助功能”权限和钥匙串的“始终允许”都要重新给。先运行一次下面的脚本，在登录钥匙串里建一个本机签名证书（“NexaDesk Local Signing”），之后每次打包都用它签名，macOS 会认为是同一个应用，给过的权限不会再丢：

```bash
scripts/make-signing-identity.sh
```

这个证书的私钥只应留在本机：拿到它的人可以签出被 macOS 当作 NexaDesk、带着同样权限的程序。给别人用的版本请用 Developer ID 签名（`NEXA_SIGN_IDENTITY="Developer ID Application: …" scripts/package-macos.sh`）。

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

被控端可以在“固定密码”里设一个密码：用它连接时直接进入，不弹确认窗口，适合连自己的电脑。

没有图形界面的 Linux 或 Windows 机器，用命令行被控端：`scripts/build-term-host.sh` 构建，`nexa-term-host --signal wss://你的服务器:8443` 运行，然后在浏览器里打开服务器网址、选“终端”、输入它打印的设备 ID。详见 [docs/TERM_HOST.md](docs/TERM_HOST.md)。

被控端也可以从命令行运行，在终端里确认连接：

```bash
nexa-agent --listen '[::]:47800'
```

`nexa-agent --help` 列出全部选项，例如 `--paired-only`、`--relay-only`、`--no-lan-discovery`。

连接断开时想知道原因：NexaDesk.app 把被控端的日志写在 `~/Library/Logs/NexaDesk/agent.log`，每个会话的开始、结束和结束原因都在里面（超过 1 MB 时旧的移到 `agent.log.1`）。

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

然后在另一台设备的浏览器，或本机浏览器，打开 `https://本机局域网地址:47801`（不要用 127.0.0.1），接受自签证书的提示，输入终端里显示的一次性码。按 F9 会让演示主机“复制”一行文字，可以用来试剪贴板。改了 `crates/nexa-web-pake` 之后，运行 `scripts/build-web-pake.sh` 重新生成 `crates/nexa-web-page/web/pake.wasm`。

## 文档

- [docs/DESIGN.md](docs/DESIGN.md)：总体设计与里程碑
- [docs/DETAILED_DESIGN.md](docs/DETAILED_DESIGN.md)：详细设计，各章末尾的“实现修订”记录了实际实现与设计的差异
- [docs/DEPLOY.md](docs/DEPLOY.md)：部署信令、STUN 和中继服务器
- [docs/WEB_CLIENT.md](docs/WEB_CLIENT.md)：Web 控制端的方案与第一版实现
- [docs/TERM_HOST.md](docs/TERM_HOST.md)：Linux / Windows 命令行被控端
- [docs/spikes/M0-report.md](docs/spikes/M0-report.md)：M0 验证报告

## 尚未完成

- **验证**：两台 Mac 的实机联调；真实 NAT 环境下的打洞测试（经自建服务器中继已在公网实测，见 docs/DEPLOY.md §6）。
- **macOS 发布**：Developer ID 签名与公证、安装包、自动更新，以及登录界面和锁屏前的远程访问。
- **其他平台**：Windows 的图形被控端与控制端（命令行被控端已有，见 docs/TERM_HOST.md）。
- **Web 端后续**：iPhone / Safari、Firefox 上的实测（Android 上的 Chrome 已用过）；TURN over TCP/TLS（UDP 被封的网络）；声音和文件传输；应用（非浏览器）里的终端。
- **命令行被控端**：Windows 版的实机运行；作为 Windows 服务运行。
- **产品决定**：无人值守机器的首次配对方式；知道 ID 的人默认能做什么；文件权限是否按方向拆开。
