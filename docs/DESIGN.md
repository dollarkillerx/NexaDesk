# NexaDesk 设计文档

> 状态：草案 v0.1（2026-11-09）
> 参考快照：RustDesk `95cca3f`、hbb_common master、Avalonia `c93b0d6`（master，12.2 开发线）
> 模块接口、线上格式和状态机见 [DETAILED_DESIGN.md](DETAILED_DESIGN.md)

## 1. 结论先行

| 问题 | 决定 | 一句话理由 |
|---|---|---|
| 后台语言 | **Rust**，全部组件统一 | 采集/编码/渲染的每帧 FFI 都在热路径上，不能有 GC 和 cgo 成本；还能做成 cdylib 嵌入 UI 进程 |
| UI | **Avalonia 12 + .NET 10 NativeAOT** | 只做窗口、列表、设置；不碰每帧数据 |
| 视频显示 | **`NativeControlHost` 内嵌原生视图，Rust 直接 Present** | 绕过 Avalonia 合成器，才能关 VSync、允许撕裂 |
| 传输 | **单 UDP socket + QUIC（quinn）**：控制走可靠流，视频走 DATAGRAM + FEC | RustDesk 的三条路径都是可靠有序的，丢一个包会阻塞后续所有帧 |
| 打洞 | 自研 ICE 风格候选竞速，放在 socket 层之下 | QUIC 只看到一个虚拟地址，换路径不用重连 |
| Tailscale | **把 tailnet IP 当作普通候选**，LocalAPI 只作增强 | 不依赖其内部 API；装了 Tailscale 时连信令服务器都可以不要 |
| 键盘 | HID 物理码上线 + **控制端“组合键意图”改写** | 不做全局 Ctrl↔Cmd 互换，否则 Mac 上的 Ctrl+C 会变成 Win+C |
| 粘贴 | 代际号 + 确认 + **粘贴闸门** | 解决“刚复制就粘贴，粘到旧内容”的根本竞态 |
| 代码来源 | **只学思路，不复制代码** | RustDesk 是 AGPL-3.0；抄代码会让 NexaDesk 也必须 AGPL 开源 |

## 2. 目标

### 2.1 目标与验收指标

指标是**设计预算，尚未实测**，M0 起由内置 HUD 持续测量。

| 能力 | 指标 |
|---|---|
| 局域网端到端延迟（采集到显示，不含游戏自身渲染和屏幕扫描） | 游戏模式 ≤ 20 ms @ 120 fps；办公模式 ≤ 35 ms @ 60 fps |
| 广域网延迟 | 局域网值 + 单向网络时延，无额外抖动缓冲 |
| 清晰度 | 办公模式 4K 文字无彩边（有 4:4:4 编码能力时）；静止 1 秒内收敛到接近无损 |
| 丢包 | 2% 随机丢包下游戏模式无明显卡顿，不触发全 IDR |
| 建连 | 局域网/Tailscale < 1 秒；打洞 < 3 秒；失败时 5 秒内落到中继 |
| 粘贴 | “复制后立即粘贴”1000 次，0 次粘到旧内容 |
| 体积 | 安装包目标 < 40 MB，空闲 CPU ≈ 0、内存 < 80 MB |

### 2.2 平台范围

- **第一阶段**：Windows 10/11、macOS 13+（Apple Silicon 优先），双向互控。
- **第二阶段**：Linux 被控（X11，然后 PipeWire/Wayland）。
- **暂不做**：移动端、浏览器控制端、多人同时控制、HDR、绕过反作弊。

## 3. 从 RustDesk 学到什么

行号基于上述快照。

| 领域 | RustDesk 现状 | NexaDesk 的处理 |
|---|---|---|
| 传输 | KCP 被封装成字节流（`src/kcp_stream.rs` `create_framed`）；WebRTC 数据通道强制可靠有序（`hbb_common/src/webrtc.rs:744`） | **改**：视频用不可靠数据报，丢包由 FEC 和参考帧恢复 |
| 建连竞速 | 多路并行，中继结果短暂持有、等直连（`src/client.rs:223` `race_transports_prefer_webrtc`） | **保留思路**，并增加建连后的持续升级 |
| 拥塞控制 | 每秒一次 `TestDelay`（`connection.rs:504`），与视频同流，超基线 150 ms 才算拥塞（`video_qos.rs`，注释承认测到的是队列） | **改**：逐包到达时间反馈，20–50 ms 周期，延迟梯度预测 |
| 客户端队列 | 每路显示 120 帧队列 + `force_push`（`client.rs:114`、`io_loop.rs:1723`） | **改**：单槽“最新帧优先”，无队列 |
| 键盘 | 按平台对逐一转换（`keyboard.rs:1349`）；Map / Translate / Legacy 三模式；`swap_modifier_key` 全局互换 Control↔Meta（`ui_session_interface.rs:699`） | **改**：HID 作为枢纽；组合键级改写（§8） |
| 粘按键兜底 | `release_remote_keys`、`fix_key_down_timeout` | **保留**，改为按下集合心跳 |
| 光标 | 形状和位置独立于视频发送（`input_service.rs` `run_cursor`/`run_pos`） | **保留**，办公模式用本地光标 |
| 剪贴板 | 监听变化 + owner 标记防回环；内容直接推送；协议无代际与确认（`message.proto:348`） | 防回环**保留**；增加代际、确认、闸门、大内容延迟拉取（§9） |
| 文件粘贴 | Windows cliprdr 按需拉取；macOS `item_data_provider`；Linux FUSE | **保留思路**（第二阶段） |
| 进程模型 | 服务 + 用户会话进程，DXGI 失败回退 GDI | **保留** |

## 4. 总体架构

```
 控制端                                            被控端
┌───────────────────────────────┐        ┌───────────────────────────────────┐
│ NexaDesk.App（Avalonia/AOT）  │        │ nexa-service（SYSTEM/LaunchDaemon）│
│  设备列表/设置/工具条          │        │  看门狗；按会话拉起 agent；SAS     │
│        │ P/Invoke（低频）      │        └───────────────┬───────────────────┘
│        ▼                       │                        │ 拉起
│ libnexa_client（Rust cdylib）  │        ┌───────────────▼───────────────────┐
│  ┌────────┐ ┌──────┐ ┌──────┐  │        │ nexa-agent（每用户会话一个）       │
│  │解码     │→│最新帧│→│Present│ │        │ 采集 → 编码 → FEC/封包             │
│  └────────┘ └──────┘ └──┬───┘  │        │ 输入注入 / 剪贴板 / 音频           │
│  输入采集 → 组合键改写    │      │        └───────────────┬───────────────────┘
│                原生视图 ◄┘      │                        │
└──────────────┬────────────────┘                        │
               │          nexa-net（虚拟路径 + QUIC）      │
               └────────────────┬─────────────────────────┘
          局域网 │ IPv6 │ 打洞 │ Tailscale │ 中继（任选其优）
                                │ 仅建连时
                     ┌──────────▼──────────┐
                     │ nexa-signal（可自建）│ 信令 + STUN + 中继，单二进制
                     └─────────────────────┘
```

**原则：每帧数据绝不进入 C#。** C# 只把窗口句柄交给 Rust，之后只收低频事件（连接状态、HUD 数据）。

### 仓库结构

```
crates/
  nexa-wire/      # protobuf 控制消息、媒体帧头
  nexa-net/       # 候选收集、打洞、路径管理、QUIC、拥塞控制
  nexa-media/     # Capture/Encoder/Decoder/Presenter 抽象与各平台实现
  nexa-input/     # HID 模型、组合键改写（纯逻辑）、注入、捕获
  nexa-clip/      # 剪贴板状态机（纯逻辑）与平台后端
  nexa-agent/     # bin：被控端
  nexa-service/   # bin：特权看门狗
  nexa-client/    # cdylib：控制端内核，C ABI
  nexa-signal/    # bin：信令 / STUN / 中继
ui/
  NexaDesk.App/   # Avalonia
  NexaDesk.Interop/ # csbindgen 生成的绑定
docs/
```

C ABI 用 `csbindgen` 生成，回调用 `[UnmanagedCallersOnly]` 函数指针，事件体为 protobuf 字节，兼容 NativeAOT。UI 与本机 agent 之间走命名管道 / Unix socket。

## 5. 连接：自己打洞 + Tailscale 辅助

### 5.1 虚拟路径层

借鉴 iroh MagicSock 的做法：实现 quinn 的 `AsyncUdpSocket`，**QUIC 只看到一个固定虚拟对端**，下层自行决定真实路径。好处是打洞、Tailscale、中继和路径升级全部对 QUIC 透明，切换时不重建会话。

同一 UDP 端口上按首字节加 STUN magic cookie 分流 STUN 与 QUIC；只看首字节会误判被 grease 的 QUIC 包。

### 5.2 候选来源

| 优先级 | 类型 | 获取方式 | 说明 |
|---|---|---|---|
| 1 | 局域网 / IPv6 直连 | 枚举网卡；mDNS 发现 | 零服务器 |
| 2 | IPv4 打洞 | 两个 STUN 地址探测映射行为；UPnP/NAT-PMP/PCP | 主路径 |
| 3 | Tailscale 直连 | 网卡上的 `100.64.0.0/10`、`fd7a:115c:a1e0::/48` | 多一层 WireGuard，略低于自有直连 |
| 4 | 自有中继（UDP） | 信令下发 | 只转发密文 |
| 5 | Tailscale DERP | 同候选 3，由实测或 LocalAPI 识别 | 延迟和带宽受限，游戏模式给出警告 |
| 6 | 中继 over WSS/443 | UDP 被封时 | 兜底；把 UDP 包封进 WebSocket |

优先级只是初值，最终按实测 RTT、丢包率打分，并带滞回防止抖动切换。

### 5.3 建连流程

1. 双方收集候选，经信令交换（信令内容对服务器端到端加密）。
2. 双方同时发 STUN 连通性检查，20 ms 节奏重传，第一条通的路径立刻启动 QUIC。
3. 用户**先用上**，后台继续探测；更优路径验证通过后无缝切换。
4. 直连失败时 5 秒内落到中继，并每 30 秒重试打洞。

**对称 NAT**：第一版只做“端口增量预测”，失败走 Tailscale 或中继。生日悖论式多端口探测放到第二阶段，且仅在一方易穿透、一方难穿透时启用，限时 3 秒。

### 5.4 Tailscale 集成，三级递进

1. **零依赖（M2）**：发现本机 tailnet 网卡地址，作为候选发布。不调用任何 Tailscale API。
2. **免信令（M2）**：在 UI 里直接填对端 tailnet IP 或 MagicDNS 名。此时无需 `nexa-signal`，身份靠设备配对保证（§10）。
3. **LocalAPI 增强（M3）**：读 `/localapi/v0/status` 列出 tailnet 内的设备，判断当前是直连还是 DERP。
   - Linux：`/var/run/tailscale/tailscaled.sock`；macOS：`/var/run/tailscaled.socket`（App Store 版改用 TCP 端口 + token）；Windows：命名管道 `\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled`。
   - 读取失败时静默回退到第 1 级。这是内部接口，**不能作为连接的必需条件**。

**默认形态（修订）**：不用服务器。同一局域网直接连接；跨网络推荐 Tailscale，打洞和中转都交给它（DERP）。自建 `nexa-signal` 是可选项，只为“不能用 Tailscale、又想按设备 ID 连接”的场景保留。App Store 版 Tailscale 的状态通过它自带的命令行 `status --json` 读取（详见 DETAILED_DESIGN §4.7 实现修订）。

内嵌 `tsnet`（免安装 Tailscale）需要 Go 边车进程，增加体积和进程，暂不做。

**MTU**：Tailscale 接口 MTU 为 1280，QUIC 固定为 1200 字节并关闭 PMTUD。路径切换对 QUIC 透明，若按较大路径探测 MTU，切到 Tailscale 时会黑洞。

## 6. 传输协议

### 6.1 通道

一条 QUIC 连接（TLS 1.3 端到端）内：

| 通道 | 载体 | 优先级 | 内容 |
|---|---|---|---|
| Control | 可靠流 | 最高 | 协商、模式切换、光标形状、统计 |
| Keyboard / 鼠标按键 | 可靠流 | 最高 | 不能丢、不能乱序 |
| Pointer | DATAGRAM | 最高 | 鼠标移动，见 §8.5 |
| Feedback | DATAGRAM | 高 | 到达时间、丢帧、参考帧确认 |
| Video | DATAGRAM | 中 | 视频分片 + FEC |
| Audio | DATAGRAM | 中 | Opus 10 ms |
| Clipboard | 可靠流 | 低 | §9 |
| File | 可靠流 | 最低 | 受令牌桶限速，游戏模式暂停 |

quinn 的拥塞窗口和 pacing 会阻塞数据报，因此注册自定义 `Controller`，让窗口恒大于 `u32::MAX`，从而同时关闭这两者（已对照 quinn 0.11.12 源码确认）。全部限速由应用层完成，批量流量经令牌桶分配剩余带宽。运行效果由 M0 验证；若不理想，退路是同 socket 上开两条 QUIC 连接（媒体 / 可靠各一条）。

### 6.2 拥塞控制

接收端每 20–50 ms 回报逐包到达时间。发送端用 GCC 风格（`draft-ietf-rmcat-gcc`）的延迟梯度 + 丢包双控制器得出目标码率，**直接调编码器码率，不靠队列吸收**。帧按链路容量估计做 pacing（不超过半个帧间隔），既避免突发打爆路由器缓冲区，也不白白增加延迟。

### 6.3 丢包恢复阶梯

逐级升级，每级都比下一级便宜：

1. **FEC**：Reed-Solomon 按帧分组，冗余 0–25% 随丢包率自适应。零额外 RTT。
2. **NACK**：仅当 RTT 足够小、重传能赶上显示截止时间时使用。
3. **参考帧失效（RFI / LTR）**：客户端报告最后完好帧，编码器改参考它。不产生大 I 帧。
4. **帧内刷新**：渐进修复，码率平稳。
5. **IDR**：最后手段，限频。

NVENC 通过 `NvEncInvalidateRefFrames` 支持第 3 级；VideoToolbox 在低延迟模式下支持 LTR（M0 实测）；AMF、QSV 的能力待接入时确认。M0 只实现验证机显卡对应的后端，其余在 M4 适配。不支持时跳到第 4/5 级。

## 7. 视频管线

### 7.1 平台实现

| | Windows | macOS | Linux（第二阶段） |
|---|---|---|---|
| 采集 | DXGI Desktop Duplication（可采安全桌面）；WGC 备用 | ScreenCaptureKit，IOSurface 帧 | X11 XShm；PipeWire + portal |
| 编码 | 按显卡选择 NVENC、AMF、QSV，D3D11 纹理零拷贝；Media Foundation 作通用后备 | VideoToolbox 低延迟模式，IOSurface 零拷贝 | VAAPI |
| 解码 | D3D11VA | VideoToolbox | VAAPI |
| 呈现 | Flip-model 交换链 + `ALLOW_TEARING` | `CAMetalLayer`，`displaySyncEnabled=NO` | Vulkan |

软件编解码（x264 / dav1d）只作无硬件时的兜底，做成可选组件，保持主包轻量。

Mac 被控端（M0 实测）：所有模式用低延迟码控 + LTR，HEVC 优先；没有硬件 4:4:4，也没有 AV1 硬编。

编码参数：无 B 帧、无前瞻、单帧 VBV、无限 GOP + 帧内刷新、低延迟预设。屏幕无变化时不编码。

### 7.2 三种模式

| | 办公高清（默认） | 均衡 | 游戏 |
|---|---|---|---|
| 色度 | 优先 4:4:4 | 4:2:0 | 4:2:0 |
| 帧率 | 60 | 60 | 跟随屏幕，最高 120+ |
| 编解码优先 | HEVC 4:4:4 > AV1 > HEVC | AV1 > HEVC > H.264 | HEVC/AV1 硬编，H.264 兜底 |
| 静止画面 | 逐步提升到接近无损 | 适度提升 | 不处理 |
| 光标 | 本地 OS 光标 | 本地 | 相对模式，画在视频里 |
| 键盘 | 组合键改写 | 组合键改写 | 纯物理码，改写关闭 |
| 文件/大剪贴板 | 正常 | 正常 | 暂停 |
| 呈现 | 无撕裂 | 无撕裂 | 允许撕裂 / VRR |

**文字清晰度**：硬件不支持 4:4:4 时，第二阶段做 AVC444 式方案（两路 4:2:0 编码，第二路携带剩余色度）。

### 7.3 延迟预算（局域网，120 fps，待实测）

| 段 | 预算 |
|---|---|
| 采集 | 1–2 ms |
| 编码（零拷贝硬编） | 3–6 ms |
| 分片 / FEC / 发送 | 2–4 ms（M0 回环实测） |
| 网络 | 1–3 ms |
| 解码 | 2–4 ms |
| 呈现（允许撕裂） | 1–4 ms |
| **合计** | **约 10–23 ms** |

帧头携带采集时间戳，控制通道做时钟偏移估计，HUD 可分段显示以上每一项。

## 8. 输入与键盘

### 8.1 三层模型

```
物理按键 → [捕获] HID Usage → [组合键改写] 意图 → [发送] 物理码 或 文本 → [注入]
```

线上统一用 **USB HID Usage**：N 个平台只需 N 张表，而不是 N×N。JIS 等键盘的 `LANG1/2`、`International1–5` 也能直接表示。

### 8.2 为什么不做全局 Ctrl↔Cmd 互换

全局互换下，Mac 控制 Windows 时物理 Ctrl+C 会变成 Win+C，终端里没法中断程序。NexaDesk 的规则：**只改写 Cmd 引导的组合键，物理 Ctrl 保持原样**。

### 8.3 默认映射（按“本机系统 → 对端系统”自动启用，可逐条编辑）

**Mac 控制 Windows**

| 按下 | 发送 |
|---|---|
| Cmd + 字母/数字（通用规则） | Ctrl + 同键 |
| Ctrl + 任意键 | 原样 |
| Cmd + Shift + Z | Ctrl + Y |
| Cmd + Tab | Alt + Tab（Cmd 按住期间保持 Alt） |
| Cmd + Q | Alt + F4 |
| Cmd + ←/→ | Home / End |
| Cmd + ↑/↓ | Ctrl + Home / End |
| Option + ←/→ | Ctrl + ←/→ |
| Option + Backspace | Ctrl + Backspace |
| Cmd + Backspace | Shift+Home，再 Delete |
| Cmd + Option + Esc | Ctrl + Shift + Esc |
| Cmd + Shift + 4 | Win + Shift + S |
| 单独点按 Cmd | Win |
| Cmd + 点击 / 滚轮 | Ctrl + 点击 / 滚轮 |

**Windows 控制 Mac**

| 按下 | 发送 |
|---|---|
| Ctrl + 字母/数字（通用规则） | Cmd + 同键 |
| Ctrl + Y | Cmd + Shift + Z |
| Alt + Tab | Cmd + Tab |
| Alt + F4 | Cmd + Q |
| Home / End | Cmd + ←/→ |
| Ctrl + Home / End | Cmd + ↑/↓ |
| Ctrl + ←/→ | Option + ←/→ |
| Ctrl + Backspace | Option + Backspace |
| Ctrl + Shift + Esc | Cmd + Option + Esc |
| Win + Shift + S | Cmd + Shift + 4 |
| PrintScreen | Cmd + Shift + 3 |
| Ctrl + 点击 | Cmd + 点击 |

同平台互控时全部透传。

### 8.4 改写的关键细节

- **终端例外**：被控端在前台应用变化时推送 `FocusContext`（terminal / ide / default）。Windows 控制 Mac 且焦点在终端时，Ctrl+C 保持 Ctrl+C，改用 Ctrl+Shift+C/V 映射 Cmd+C/V。
- **按下时决议，抬起时回放**：维护 `物理键 → 实际发出的键` 表。抬起时按表释放，即使修饰键中途变化也不会粘键。
- **修饰键延迟决议**：Cmd 按下时先不发，等下一个键或点击再决定变成什么；单独抬起才发 Win。修饰键本身无可见效果，用户感觉不到延迟。
- **防粘键**：窗口失焦时释放全部键；按下集合随心跳同步，被控端对账；断线全部释放。
- **本机快捷键拦截**：Windows 用 `WH_KEYBOARD_LL`，macOS 用 `CGEventTap`（需辅助功能权限），让 Alt+Tab、Cmd+Tab 发给对端。保留一个可配置的“宿主键”唤出工具条和退出捕获。
- **输入法**：默认**远端输入法**（发物理键）。可切**本地输入法**：本地完成组字，提交文本后以 Unicode 注入。Mac 的 `英数/かな` 与 Windows 的 `無変換/変換` 按 IME 关/开语义互映。
- **纯函数**：改写器不依赖 OS，用金标表做穷举单测。

### 8.5 鼠标

- **办公模式**：绝对坐标 + 本地 OS 光标，手感零延迟。被控端推送光标形状。
- **游戏模式**：锁定指针，Windows Raw Input / macOS `CGAssociateMouseAndMouseCursorPosition(false)`，收到即发，不做聚合。
- **抗丢包**：相对移动发送**累计坐标**（总 x、总 y），被控端取差值，丢包不丢位移。
- **按键**走可靠流，与移动分离。
- **注入**：Windows `SendInput`；macOS `CGEventPost`。部分反作弊游戏会拒绝注入，这在非目标内，不做绕过。

### 8.6 线程

输入采集运行在 Rust 专用线程（Windows 用消息专用窗口接 Raw Input），**UI 线程卡顿不影响输入**。

## 9. 可靠的粘贴

### 9.1 粘贴闸门（核心）

问题：用户在本地复制后马上按远端粘贴，剪贴板和按键走不同通道，按键可能先到，粘到旧内容。

```
本地复制 → gen=42 → ClipAnnounce(42) ─────→ 被控端写入系统剪贴板
                                     ←───── ClipApplied(42, ok)
Cmd+V → 改写器识别为粘贴
      → 若 已确认 gen < 本地 gen：扣住 V 键
      → 收到 ClipApplied(42) 后放行；超时 800 ms 放行并提示
```

组合键改写器已经知道哪次按键是粘贴，因此闸门几乎没有额外成本。大内容用延迟渲染：只需公告到达并占位，目标应用读取时再拉取，闸门不等传输完成。

### 9.2 同步协议

```
ClipAnnounce { gen, origin, formats[{kind, size, hash}], inline? }
ClipApplied  { gen, status }
ClipFetch    { gen, kind, offset, len }
ClipData     { gen, kind, offset, bytes, eof }
```

- **代际**：`(Lamport 计数, 设备 ID)`，大者胜，解决两端同时复制的冲突。
- **内联阈值**：≤ 64 KiB 的文本随公告发送，常见场景不走拉取。
- **防回环**：写入时附带私有 owner 格式；剪贴板管理器可能剥掉它，故同时比对内容哈希。

### 9.3 平台

| | Windows | macOS | Linux |
|---|---|---|---|
| 变化检测 | `AddClipboardFormatListener` | 轮询 `changeCount`（100 ms，成本极低） | XFixes / wl-data-control |
| 延迟渲染 | `WM_RENDERFORMAT` | `NSPasteboardItemDataProvider` | selection owner |
| 占用处理 | `OpenClipboard` 指数退避，总计 1 秒 | — | — |

### 9.4 格式

文本（换行按目标平台规范化）、HTML（处理 CF_HTML 头）、RTF、图片（PNG 为规范格式）。文件在第二阶段走虚拟文件，第一阶段走传输面板。

### 9.5 粘贴控制功能

- **方向开关**：本机→远端、远端→本机独立控制，可按设备设置。
- **隐私**：默认不同步标记为敏感的内容（Windows `ExcludeClipboardContentFromMonitorProcessing`、macOS `org.nspasteboard.ConcealedType`）。
- **键入式粘贴**：对禁止粘贴的场景（BIOS、UAC、远程 KVM、虚拟机控制台），把文本按节奏模拟键入。
- **状态反馈**：工具条显示同步状态，失败时告知原因。

## 10. 安全

被控软件等于远程执行权限，以下为硬性要求：

- **身份**：每台设备一个 Ed25519 密钥对。QUIC 用它派生的自签证书做双向认证，校验对端公钥。
- **首次配对**：对方屏幕显示一次性码，用 SPAKE2 绑定 TLS exporter。**信令和中继被攻破也无法中间人。**
- **无人值守**：默认关闭，按设备显式开启，口令用 Argon2 存储，支持 2FA。
- **被控端知情**：默认弹窗确认；会话期间常驻可见指示，且被控方可一键断开。**不提供隐藏模式。**
- **权限分级**：查看 / 输入 / 剪贴板 / 文件 / 音频，按设备授予。
- **审计**：本机记录每次连接。
- **服务端最小化**：只见公钥、候选（加密）和密文。
- **Tailscale 身份不替代配对**，仅作附加信号。

## 11. Avalonia UI

### 11.1 视频显示方案比较

| 方案 | 延迟 | 叠加 UI | 结论 |
|---|---|---|---|
| `WriteableBitmap` | 差，每帧经 CPU 与 UI 线程 | 容易 | 否 |
| `CompositionDrawingSurface` + GPU interop（D3D 句柄 / `IOSurfaceRef`，见 `samples/GpuInterop`） | 中，至少受合成器节拍限制，不能撕裂 | 容易 | 备选，用于 Linux 或降级 |
| **`NativeControlHost` + Rust 直接呈现** | 最低 | 需独立窗口 | **采用** |

**空域问题**：Avalonia 控件无法画在原生视图上方，因此工具条、HUD 都是独立的无边框置顶窗口，并设置 `OverlayPopups = false`，让菜单同样成为独立窗口。

原生视图由 UI 线程创建；`Present` 在 Rust 渲染线程调用，避免 UI 线程阻塞视频。

### 11.2 界面

- **主窗口**：我的 ID 与临时码；设备列表（标明局域网 / Tailscale / 公网）；输入框支持 ID、IP、MagicDNS 名。
- **会话窗口**：全屏视频；悬浮工具条（模式切换、显示器、键盘方案、剪贴板状态、断开）。
- **键盘设置**：映射表可编辑，实时按键预览“按下 → 发送”。
- **连接诊断**：当前路径、候选结果、分段延迟、丢包、码率。
- **被控提示条**：谁在控制、权限、断开。

macOS 需引导授予屏幕录制与辅助功能权限，且必须 Developer ID 签名，否则更新后权限失效。

## 12. 里程碑

先做风险最大的验证，不通过就在写大量代码前调整方案。

| 阶段 | 内容 | 退出标准 |
|---|---|---|
| **M0 验证** | ① Avalonia 内嵌 Metal/DXGI 无撕裂限制呈现 ② quinn 数据报 + 自定义拥塞控制 + 自定义 socket ③ 零拷贝硬编到显示 | 三项跑通；实测延迟接近 §7.3 |
| **M1 局域网** | Windows ↔ macOS 画面 + 基础输入 + HUD | 手动控制一小时无异常 |
| **M2 连接** | 信令、打洞、Tailscale 1/2 级、中继、路径升级 | 多种 NAT 组合矩阵测试通过 |
| **M3 办公** | 组合键改写、粘贴闸门、配对、4:4:4、LocalAPI | §2.1 粘贴指标达标；改写金标全过 |
| **M4 游戏** | FEC、RFI、相对鼠标、撕裂呈现、音频、GPU 优先级 | 2% 丢包指标达标 |
| **M5 产品化** | 服务、无人值守、UAC/登录界面、安装包、签名、自动更新 | 可对外试用 |
| **后续** | Linux 被控、文件剪贴板、虚拟显示器、手柄、AVC444 | — |

## 13. 风险

| 风险 | 影响 | 应对 |
|---|---|---|
| 子窗口交换链拿不到独立翻转，撕裂/低延迟失效 | 游戏模式延迟变高 | M0 验证；游戏模式全屏时可改用顶层窗口呈现 |
| 关闭 quinn 拥塞控制后实际表现不理想 | 传输层要调整 | 双连接退路；再退用 str0m 或自研 UDP + Noise |
| 游戏占满 GPU，采集/编码被饿死 | 帧率骤降 | 提升 GPU 调度优先级、MMCSS |
| Mac 被控端没有硬件 4:4:4 | Mac 被控时文字可能有彩边 | 静止精修；第二阶段考虑 AVC444 式双流 |
| 手柄：ViGEmBus 已于 2023 年归档 | 手柄支持受阻 | 不进入 MVP，届时重新选型 |
| HEVC 专利 | 商业分发风险 | 只调用系统/硬件编解码器，不内置 HEVC 软编；发布前法务确认 |
| Windows 代码签名、驱动签名成本 | 被杀软拦截 | M5 预算证书；不写内核驱动 |
| 远控软件常被滥用，安全厂商和云服务可能封禁 | 分发受阻 | 坚持可见指示、配对、审计（§10） |

## 14. 待决问题（已按括号内默认值设计）

1. 产品是否开源？（默认闭源或宽松许可，因此不使用 RustDesk 代码。）
2. 是否提供官方信令 / 中继服务？（默认只提供可自建的单二进制。）
3. Linux 被控是否要提前？（默认第二阶段。）
4. 手柄是否为游戏模式的硬需求？（默认否。）

## 15. 参考

- RustDesk：<https://github.com/rustdesk/rustdesk>（AGPL-3.0，仅参考）
- Avalonia GPU interop 示例：`samples/GpuInterop`
- iroh 打洞：<https://iroh.computer/docs/protocols/net/holepunching>
- Tailscale safesocket：<https://pkg.go.dev/tailscale.com@v1.98.6/safesocket>
- GCC：`draft-ietf-rmcat-gcc`
