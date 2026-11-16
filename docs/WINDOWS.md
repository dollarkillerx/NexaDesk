# Windows 图形版计划

目标：Windows 上的 NexaDesk 和 macOS 版一样，既能被控（被控端），也能控制别人（控制端），并且有中文、English、日本語三种语言的安装包。

本文是开工前的计划，等确认后再动手。完成一项就在本文里勾掉，并写下和计划不一样的地方。

## 1. 现状

能直接复用的（跨平台，或者已经有 Windows 实现）：

- 协议和会话：`nexa-wire`、`nexa-session`、`nexa-net`（QUIC、打洞、中继、路径切换、TURN）、`nexa-auth`（配对、密码；Windows 的密钥用 DPAPI 保存，已经写好）、`nexa-clip` 的同步逻辑、`nexa-input` 的组合键改写（已经有“Windows 控制 Mac”的规则）。
- 命令行：`nexa-term`（ConPTY）和 `nexa-term-host` 已能在 macOS 上交叉编译出 Windows 版，但还没在真机上运行过。
- 界面：Avalonia 本身跨平台，视图模型和大部分窗口都能直接用。

要为 Windows 重写的（现在只有 macOS 实现，约 9 千行）：

| 部分 | macOS 现在用的 | 接口 |
|---|---|---|
| 截屏 | ScreenCaptureKit | 被控端 `Platform::capturer` |
| 视频编码 | VideoToolbox（HEVC / H.264，LTR） | `Platform::encoder` |
| 输入注入 | CGEvent | `Platform::input_sink` |
| 剪贴板 | NSPasteboard | `Platform::clipboard`、`Deps::clipboard` |
| 主机指针 | NSCursor | `Platform::cursor_source` |
| 系统声音 | ScreenCaptureKit 音频 + AudioToolbox Opus | `Platform::audio_capture`、`audio_encoder` |
| 视频解码 | VideoToolbox | 控制端 `Deps::decoder` |
| 画面显示 | Metal（CAMetalLayer） | `Deps::attach_view` |
| 按键采集 | CGEventTap | `Deps::key_source` |
| 游戏模式锁定指针 | CGAssociateMouse | `Deps::pointer_lock` |
| 本地指针 | NSCursor | `Deps::cursor_display` |
| 声音播放 | AudioUnit | `Deps::audio_output` |
| 终端视图 | WKWebView + xterm.js | `nexa-client::terminal` |
| 显示器唤醒 | IOKit 电源断言 | `nexa-media::macos::power` |
| 应用和被控端通信 | Unix socket | `nexa-agent::ipc` |
| 界面原生部分 | NSView 句柄、SMAppService 登录项、权限引导 | `ui/NexaDesk.App` |

## 2. 技术选择

| 部分 | Windows 上用 | 理由 |
|---|---|---|
| 截屏 | DXGI Desktop Duplication；Windows.Graphics.Capture 作备选 | 延迟最低、直接给 D3D11 纹理，所有 Windows 10/11 都有；WGC 用于 DXGI 失败的情况（部分多显卡笔记本） |
| 编码 | Media Foundation 硬件编码器（NVENC / Quick Sync / AMF 都通过它），零拷贝 D3D11 纹理 | 不用分别对接三家显卡 SDK；没有硬件编码器时退回 MF 软件 H.264 |
| 解码 | Media Foundation（DXVA），输出 D3D11 纹理 | 同上 |
| 显示 | D3D11 交换链，挂在界面给的 HWND 子窗口上 | 对应 Metal 的 CAMetalLayer 做法，渲染线程不经过界面 |
| 输入注入 | SendInput：按键用扫描码（HID → Set 1），文字用 KEYEVENTF_UNICODE，鼠标用虚拟桌面绝对坐标 | 和 macOS 一样按物理键传 |
| 按键采集 | 低级键盘钩子 WH_KEYBOARD_LL | 才能截到 Win 键、Alt+Tab 这类系统组合键 |
| 游戏模式 | Raw Input + ClipCursor | 相对移动 |
| 剪贴板 | Win32 剪贴板：文字、HTML（CF_HTML）、PNG / DIB | 和 macOS 端互通的格式 |
| 系统声音 | WASAPI loopback | 系统自带 |
| Opus | libopus（`audiopus` crate，从源码编译） | Windows 没有系统自带的 Opus；需要新增一个依赖 |
| 终端视图 | WebView2 + 同一份 xterm.js | Windows 11 自带 WebView2 运行时 |
| 本地通信 | 命名管道（只允许当前用户） | 对应 Unix socket |
| 开机启动 | 注册表 Run 项 | 对应 SMAppService |
| 显示器唤醒 | SetThreadExecutionState | 对应 IOKit 断言 |
| Rust 绑定 | `windows` crate（微软官方） | 一个依赖覆盖 D3D11、DXGI、MF、Win32 |

## 3. 构建、测试和安装包

- **Rust**：Windows x64，目标 `x86_64-pc-windows-msvc`。这台 Mac 上先用 mingw（`x86_64-pc-windows-gnu`）检查能否编译，正式包在 Windows 上构建。如果在 Mac mini 的虚拟机里跑 Windows 11 ARM，就再加 `aarch64-pc-windows-msvc`。
- **界面**：NativeAOT 不能在 macOS 上交叉编译到 Windows，所以 Windows 版的界面要在 Windows 上编译（GitHub Actions 的 Windows 机器，或者你的 Windows 电脑）。
- **CI**：新增 GitHub Actions：Windows 上编译 Rust 和界面、跑全部单元测试、打安装包，产物可以下载。CI 机器没有显卡和真实桌面，截屏、编码这类测试要在真机上跑。
- **真机测试**：必须有一台 Windows 机器，见第 6 节的问题。
- **安装包**：NSIS，三种语言（安装时按系统语言自动选，也可以手动选）：
  - 装到 Program Files，开始菜单和桌面快捷方式，可选开机启动；
  - 防火墙放行 UDP 47800（被控端用）；
  - 完整卸载（可选保留设置和配对记录）；
  - 代码签名：没有证书时 SmartScreen 会提示“未知发布者”，需要你决定是否购买证书。

## 4. 阶段

每个阶段结束都能单独验收。

1. **能在 Windows 上编译**（进行中：整个项目已能按 `x86_64-pc-windows-gnu` 编译；被控端和应用的本地通信在 Windows 上改用只允许本人打开的命名管道，名字由数据目录算出（`\\.\pipe\nexadesk-<路径的 FNV-1a>`）；`.github/workflows/windows.yml` 在 Windows 上编译、测试、上传产物，还没在 GitHub 上跑过）：给所有 macOS 专用代码加平台条件；Windows 下先用“不支持”的占位实现；加 CI；`cargo test` 和界面测试在 Windows 上跑通。
2. **被控端（从 Mac 控制 Windows）**：截屏、编码、输入注入、剪贴板、主机指针、命名管道、开机启动、显示器唤醒。验收：用 Mac 版或浏览器连上 Windows 电脑，能看、能操作、能复制粘贴、能开 PowerShell 终端。
3. **控制端（从 Windows 控制 Mac）**：解码、D3D11 显示、键盘钩子、指针、游戏模式、WebView2 终端。验收：Windows 上的 NexaDesk 能连 Mac，组合键按“Windows 控制 Mac”的规则改写，延迟和 Mac 版相当。
4. **声音**：WASAPI 采集和播放、libopus。
5. **安装包**：NSIS 三语言安装包、开始菜单、防火墙规则、卸载、（可选）签名。
6. **以后再做**：作为 Windows 服务运行，才能在登录界面、锁屏和 UAC 弹窗时也能控制（普通用户进程看不到安全桌面）；ARM64 版。

阶段 2 和 3 互不依赖，可以按你更需要的方向先做。

## 5. 风险

- **没有 Windows 机器就无法验收**：截屏、编码、输入这些只能在真机上确认。
- **硬件编码器差异**：各家显卡驱动的 MF 编码器对低延迟参数的支持不一样，可能需要按厂商调参，最后退回软件编码。
- **UAC 和锁屏**：第 6 阶段之前，遇到 UAC 弹窗或锁屏时画面会停住、操作无效，这是 Windows 的安全限制。
- **安装包签名**：不签名也能装，但会被 SmartScreen 拦一下。

## 6. 已经决定的

- 测试：在你的 Windows 电脑（x64）上装 CI 打出来的包验收。
- 顺序：被控端和控制端都做；先做被控端（做完就能从 Mac 和浏览器控制 Windows），接着做控制端。
- 签名：先不签名，以后需要时再加。
