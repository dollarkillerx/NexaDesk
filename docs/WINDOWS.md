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
| 终端视图 | MySSH 的原生终端控件（`ui/NexaDesk.Terminal`，XTerm.NET） | `nexa-client::terminal` 只负责数据流，macOS 和 Windows 共用 |
| 显示器唤醒 | IOKit 电源断言 | `nexa-media::macos::power` |
| 应用和被控端通信 | Unix socket | `nexa-agent::ipc` |
| 界面原生部分 | NSView 句柄、SMAppService 登录项、权限引导 | `ui/NexaDesk.App` |

## 2. 技术选择

| 部分 | Windows 上用 | 理由 |
|---|---|---|
| 截屏 | DXGI Desktop Duplication；Windows.Graphics.Capture 作备选 | 延迟最低、直接给 D3D11 纹理，所有 Windows 10/11 都有；WGC 用于 DXGI 失败的情况（部分多显卡笔记本） |
| 编码 | Media Foundation 硬件编码器（NVENC / Quick Sync / AMF 都通过它），零拷贝 D3D11 纹理（已做：`windows/hwenc.rs`、`windows/gpu.rs`） | 不用分别对接三家显卡 SDK；没有硬件编码器时退回 MF 软件 H.264 |
| 解码 | Media Foundation（DXVA），输出 D3D11 纹理 | 同上 |
| 显示 | D3D11 交换链，挂在界面给的 HWND 子窗口上 | 对应 Metal 的 CAMetalLayer 做法，渲染线程不经过界面 |
| 输入注入 | SendInput：按键用扫描码（HID → Set 1），文字用 KEYEVENTF_UNICODE，鼠标用虚拟桌面绝对坐标。被控端没以管理员身份运行时，Windows 会悄悄丢掉送往管理员程序的输入：管理员程序在前台时丢掉全部输入（指针、点击、按键），点在管理员程序窗口上的点击也会丢；SendInput 照样报“已发出”（Windows 11 上任务管理器在前台时实测，指针不动）。所以每次按下按键或鼠标时自己查一下前台窗口和点击处的窗口，查到就写进 agent.log；SendInput 报告没发出的输入也记。同类问题 10 秒一行 | 和 macOS 一样按物理键传 |
| 按键采集 | 低级键盘钩子 WH_KEYBOARD_LL | 才能截到 Win 键、Alt+Tab 这类系统组合键 |
| 游戏模式 | Raw Input + ClipCursor | 相对移动 |
| 剪贴板 | Win32 剪贴板：文字、HTML（CF_HTML）、PNG / DIB | 和 macOS 端互通的格式 |
| 系统声音 | WASAPI loopback | 系统自带 |
| Opus | libopus（`audiopus` crate，从源码编译） | Windows 没有系统自带的 Opus；需要新增一个依赖 |
| 终端视图 | 同一个原生终端控件（Avalonia） | 不需要 WebView2，和 macOS 共用一份 |
| 本地通信 | 命名管道（只允许当前用户） | 对应 Unix socket |
| 开机启动 | 注册表 Run 项 | 对应 SMAppService |
| 显示器唤醒 | SetThreadExecutionState | 对应 IOKit 断言 |
| Rust 绑定 | `windows` crate（微软官方） | 一个依赖覆盖 D3D11、DXGI、MF、Win32 |

## 3. 构建、测试和安装包

全部在这台 Mac 上完成：`scripts/package-windows.sh`（做法和 MySSH 一样）。

- **Rust**：用 mingw 交叉编译（`x86_64-pc-windows-gnu`，`brew install mingw-w64`），只依赖系统自带的 DLL（UCRT，Windows 10 起都有）。
- **界面**：NativeAOT 不能在 macOS 上交叉编译到 Windows，所以 Windows 版是自带运行时的 JIT 版本加 ReadyToRun（代码和 AOT 版相同，体积大一些，第一次启动稍慢）。
- **CI**：暂不用。`.github/workflows/windows.yml` 只能手动触发，留着以后用。
- **真机测试**：在你的 Windows 电脑上装 `dist/windows/NexaDesk-win-Setup.exe`。
- **在 Windows 上直接跑测试**（装了 Rust 的 MSVC 工具链和 VS 2022 生成工具时）：把 VS 自带的 CMake 加进 PATH，`CMAKE_GENERATOR="Visual Studio 17 2022"`，再 `cargo --config 'profile.dev.package.audiopus_sys.opt-level=2' test --workspace`（libopus 用 Debug 编译会链接调试版 C 运行库，和 Rust 的对不上）。
- **安装包**：Velopack（`vpk "[win]" pack`）：
  - 按用户安装到 `%LocalAppData%\NexaDesk`，不需要管理员权限，开始菜单和桌面快捷方式；
  - 安装界面只有进度，没有文字，所以不分语言；应用本身按 Windows 显示语言选中文、日语或英语，设置里也能改；
  - “登录时启动”写在 `HKCU\...\Run`，带 `--background`：登录后只在通知区域出现图标并自动开启允许控制；卸载时删掉这一项；
  - 防火墙：按用户安装没有权限预先放行，第一次开启允许控制时 Windows 会弹窗询问，选允许即可；
  - 卸载：设置 → 应用。设置和配对记录在 `%APPDATA%\NexaDesk`，卸载后保留；
  - 不签名：SmartScreen 第一次会提示“Windows 已保护你的电脑”，点“更多信息 → 仍要运行”。
  - 以后可以加 Velopack 的自动更新（`releases.win.json` 已经生成）。

## 4. 阶段

每个阶段结束都能单独验收。

1. **能在 Windows 上编译**（完成）：整个项目能按 `x86_64-pc-windows-gnu` 编译；被控端和应用的本地通信在 Windows 上改用只允许本人打开的命名管道，名字由数据目录算出（`\\.\pipe\nexadesk-<路径的 FNV-1a>`）。
2. **被控端（从 Mac 控制 Windows）**（已写完，等真机验收）：

   **显卡硬件编码**：被控端启动时用一帧 4K 画面真的试一次显卡的 H.264 编码器（Media Foundation 列出的硬件 MFT 逐个试，NVENC / Quick Sync / AMF 都走这条路）。成功就声明最高 4096×2304、120 fps，截屏留在显存里：复制出的 BGRA 纹理由 D3D11 视频处理器转成 NV12 并缩放（BT.709 有限范围，关掉驱动的锐化降噪），编码器通过 DXGI 设备管理器直接读这块纹理。游戏模式要画进画面的指针，只回读指针那一小块，混合后写回。编码器是异步的：等 NeedInput 再送帧，40 ms 内没出来的帧算"跳过"，跟着下一帧出来。会话中硬件编码失败，就换成软件编码继续；显卡转换失败，就重建截屏，之后都走 CPU。

   **没有硬件编码器时**（最高 1080p、60 fps）：DXGI 截屏转 NV12（缩小时按面积平均，多线程，4K 到 1080p 约 6 ms；最初是取最近像素，细线和文字会断）、Media Foundation 软件 H.264（先到 1080p）、SendInput（扫描码和 Unicode 文字）、剪贴板文字和图片（图片用 WIC：写入时放 PNG 本身和一份 CF_DIBV5，系统据此给只认位图的程序生成 CF_DIB；读取时优先取 PNG，没有就把位图转成 PNG）、主机指针、显示器唤醒、按显示器的 DPI 感知；界面通过命名管道连被控端，“登录时启动”、通知区域图标、只运行一份。验收：用 Mac 版或浏览器连上 Windows 电脑，能看、能操作、能复制粘贴、能开 PowerShell 终端。这一阶段 Windows 版还不能去连别的电脑，界面上会说明。
3. **控制端（从 Windows 控制 Mac）**（画面和操作已写完，等真机验收）：Media Foundation 解码 H.264（低延迟模式，NV12 回到内存）、D3D11 交换链显示（着色器把 BT.709 转成 RGB，按比例留黑边），画面放在一个 STATIC 子窗口里，鼠标穿透给会话窗口。Avalonia 在它外面还套了一层承载窗口（`AvaloniaDumbWindow`），这层原本把鼠标截走（命中测试返回 HTCLIENT），会话窗口在画面上收不到指针事件，Windows 控制端于是不发移动、点击和滚轮，只有走键盘钩子的按键能发过去（issue #5，0.1.37 及以前）；现在这层也子类化，命中测试返回 HTTRANSPARENT。不用窗口样式（WS_EX_TRANSPARENT），因为 Avalonia 会把样式改回去；低级键盘钩子捕获按键（Win、Alt+Tab 也能捕获，主机键是 Ctrl+Alt+Win+G）；游戏模式用 Raw Input 加 ClipCursor。终端用 MySSH 的原生终端控件（两个平台共用，见上表）。对方的指针形状：主机发来的 PNG 用 WIC 解码成 `HCURSOR`，会话窗口子类化后在画面上方响应 `WM_SETCURSOR`（`nexa-client/src/real_windows_cursor.rs`）。还没做：HEVC 解码。验收：Windows 上的 NexaDesk 能连 Mac，组合键按“Windows 控制 Mac”的规则改写，延迟和 Mac 版相当。
4. **声音**（已写完，等真机验收）：被控端用 WASAPI loopback 录默认输出设备（让 Windows 转成 48 kHz 立体声浮点；老系统不支持时按设备格式录、自己转换），什么都没在播放时补静音；控制端用 WASAPI 事件驱动播放。换了默认设备（插拔耳机）会自动重新打开。Opus 用 libopus 1.3.1（`crates/nexa-opus`，`audiopus_sys` 从源码静态编译，`.cargo/config.toml` 为 CMake 4 设了 `CMAKE_POLICY_VERSION_MINIMUM`），不多带 DLL。

   切换模式、换屏幕、分辨率变化、UAC 或锁屏之后，流水线都要重建：DXGI 每个进程对每块屏幕只能有一个复制，所以要先释放旧的截屏和编码器，再建新的（`nexa-session/src/host.rs` 的 `Released`）。以前顺序反了，Windows 被控端一切到游戏模式就断开。浏览器会话在所有平台上都没有声音。

   **几个人同时看同一块屏幕**（最多 4 人，应用和浏览器一起算）：以前每个会话各建一个 DXGI 复制，第二个会话的 `DuplicateOutput` 失败（E_INVALIDARG，日志里是 `DuplicateOutput: OS status -2147024809`），第二个人一连上就断开。现在每块屏幕只有一个复制，在自己的线程上取画面，复制到一张共用的桌面纹理（`nexa-media/src/fanout.rs`，`windows/capture.rs` 的 `Duplication`）；每个会话从这里拿最新的桌面，再按自己的大小、帧率、要不要画指针，各自走 GPU 或 CPU 路径缩放和编码。最后一个会话离开时复制才释放，释放完才返回，所以重建时“先释放旧的”照旧有效；只是自己重建的会话会接着用别人还在用的那个复制。模式变化、UAC、锁屏让复制失效时，每个会话都收到 AccessLost、各自重建，再共用新建的那一个。取画面的线程不在 `AcquireNextFrame` 里等（超时 0，没有新画面就睡 2 ms）：在和硬件编码器共用的设备上反复等，编码器会饿死（“the hardware encoder stopped asking for frames”，NVIDIA 显卡上实测）。显示器保持唤醒也改由这个线程持有。macOS 不受影响（ScreenCaptureKit 允许多个流）。在 Windows 11 + NVIDIA 显卡的电脑上验证过：一个人、两个人同时（各自尺寸）、一个人反复重建而另一个人一直在看，GPU 编码都正常（`cargo test -p nexa-media --test windows_capture -- --ignored --nocapture`，需要真的屏幕）。
5. **安装包**（完成，见第 3 节）：Velopack 安装包和免安装 zip；以后加自动更新和（可选）签名。
6. **以后再做**：作为 Windows 服务运行，才能在登录界面、锁屏和 UAC 弹窗时也能控制（普通用户进程看不到安全桌面）；ARM64 版。

阶段 2 和 3 互不依赖，可以按你更需要的方向先做。

## 5. 风险

- **没有 Windows 机器就无法验收**：截屏、编码、输入这些只能在真机上确认。
- **硬件编码器差异**：各家显卡驱动的 MF 编码器对低延迟参数的支持不一样，可能需要按厂商调参，最后退回软件编码。
- **UAC 和锁屏**：第 6 阶段之前，遇到 UAC 弹窗或锁屏时画面会停住、操作无效，这是 Windows 的安全限制。
- **安装包签名**：不签名也能装，但会被 SmartScreen 拦一下。

## 6. 已经决定的

- 测试：在你的 Windows 电脑（x64）上装这台 Mac 打出来的包验收，不用 CI。
- 安装包：参考 MySSH，用 Velopack。
- 顺序：被控端和控制端都做；先做被控端（做完就能从 Mac 和浏览器控制 Windows），接着做控制端。
- 签名：先不签名，以后需要时再加。
