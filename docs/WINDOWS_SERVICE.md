# Windows 系统服务：在 UAC 弹窗和锁屏时也能控制

> 状态（2026-11-27）：第 1 期（跟随输入桌面）已写好，并在真机（Windows 11，build 26300）上验证：用计划任务以 SYSTEM 身份经 `run-in-session` 启动被控端，从 Mac 连上后 UAC 弹窗能看到、能点。第 2 期（§5，以用户身份做事）已写好：新 crate `nexa-user`（`is_system`、`UserToken::of_session`、`as_user`、`user_folder`），命令行、收发文件、DPAPI 密钥、配对记录、数据和下载目录都改为以会话用户身份进行；同一台机器上以 SYSTEM 运行时，数据目录和 `device-key` 的所有者是用户、用户能解开，收到的文件所有者是用户。第 3 期（§3，服务 `crates/nexa-service`）已写好并在真机上验证：安装到 Program Files、以 LocalSystem 运行，用户经控制管道请它在自己的会话里以 SYSTEM 启动被控端，UAC 弹窗能看到能点，查看端按 Ctrl+Alt+Del 由服务 `SendSAS` 发出，客户端断开后被控端随即结束，卸载后服务、目录、注册表都恢复。第 4、5 期未实现。对应 [WINDOWS.md](WINDOWS.md) 第 6 阶段、[DETAILED_DESIGN.md](DETAILED_DESIGN.md) §5.1。

## 1. 要解决的问题

UAC 弹窗和锁屏在 Windows 的**安全桌面**（Winlogon 桌面）上。只有以 SYSTEM 身份运行的进程能截取它的画面、向它注入输入；UAC 弹窗程序（consent.exe）本身也以 SYSTEM 运行，管理员身份发来的输入同样到不了。所以：

- 普通身份或管理员身份的被控端（0.1.43 的“以管理员身份运行”）在 UAC 弹窗时画面停住（0.1.39 起停在最后一帧等待）、点击无效。
- TeamViewer、AnyDesk、RustDesk 都靠一个 SYSTEM 服务，在用户会话里以 SYSTEM 身份运行被控端。

目标（第一期）：已登录的用户会话里，UAC 弹窗、锁屏（含解锁时输入密码）都能看到、能操作；远程发送 Ctrl+Alt+Del。
不在第一期：开机后还没人登录时的登录界面（服务的设计给它留好位置，见 §3）、拆分 SYSTEM 进程的网络处理（见 §8）。

参照了 RustDesk（`src/platform/windows.cc`、`windows.rs`，见 §9）：它用同样的结构，在大量机器上验证过，凡是两种做法都可以的地方，取它的做法。

## 2. 总体结构

```
nexa-service.exe（Windows 服务，LocalSystem，装在 C:\Program Files\NexaDesk Service\）
  ├─ 控制管道 \\.\pipe\nexadesk-service：只接受本机交互用户的连接，核对对方所在的会话
  ├─ 收到“在我的会话里启动被控端（参数…）”
  │    └─ 取该会话里 winlogon.exe 的令牌（SYSTEM，本就在这个会话）→ CreateProcessAsUser（winsta0\default）
  │         → nexa-agent.exe（服务目录里的那份，不是用户目录里的）
  ├─ 收到“发送 Ctrl+Alt+Del” → SendSAS
  ├─ 被控端退出时按需重启；用户注销、会话断开时结束它
  └─ 被控端的 stderr 写到该用户的 agent.log（以用户身份写）

NexaDesk.exe（用户身份，界面）
  ├─ 装了服务且版本一致：请服务启动被控端，然后照旧连被控端的 IPC 管道
  └─ 否则：照旧自己启动用户身份的被控端（现在的做法）

nexa-agent.exe（SYSTEM 身份，在用户会话里）
  ├─ 截屏、注入、指针线程跟随“当前输入桌面”（普通桌面 ↔ 安全桌面）
  └─ 凡是属于用户的东西都以用户身份做：命令行、收发文件、DPAPI 保存的密钥（§5）
```

服务只当“启动器”，被控端还是同一个程序，网络、会话、权限、确认窗口的逻辑都不变。

## 3. 服务（nexa-service）

- **位置**：`C:\Program Files\NexaDesk Service\`，里面有 `nexa-service.exe` 和 `nexa-agent.exe` 的副本。只有管理员能写这里。SYSTEM 服务绝不能运行用户目录（`%LocalAppData%\NexaDesk`，Velopack 的安装位置）里的程序，否则任何用户程序替换掉它就能拿到 SYSTEM。
- **安装/卸载**：由应用以管理员身份运行 `nexa-service.exe install|uninstall` 完成（UAC 确认一次）：复制文件、`CreateService`（自动启动、失败后重启）、写 `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration = 1`（允许服务发送 Ctrl+Alt+Del，RustDesk 同样在安装时写）、启动服务。卸载时停止并删除服务和目录，`SoftwareSASGeneration` 是安装时写的就删掉，原来就有则保留。
- **控制管道**：DACL 只允许 `INTERACTIVE`（本机交互登录的用户）连接，拒绝远程。每个请求先用 `GetNamedPipeClientProcessId` + `ProcessIdToSessionId` 确认请求方所在的会话，只能为**自己的会话**启动被控端。
- **启动被控端**：在目标会话里找 `winlogon.exe`（`WTSEnumerateProcessesW`，核对 `SessionId`），`OpenProcessToken` → `DuplicateTokenEx`（主令牌）→ `CreateProcessAsUserW`，桌面 `winsta0\default`，`CREATE_NO_WINDOW`。这是 RustDesk 的 `GetSessionUserTokenWin`/`LaunchProcessWin` 的做法，也是 DETAILED_DESIGN §5.1 原来写的；winlogon 在没人登录时也在控制台会话里，以后做登录界面时不用换。找不到 winlogon 时退回“复制服务自己的令牌并 `SetTokenInformation(TokenSessionId)`”（需要 SE_TCB，LocalSystem 有）。参数和环境（信令服务器的接入密钥）由应用经管道传来，不上命令行。
- **参数白名单**：服务只转发已知的参数（`--listen`、`--name`、`--signal`、`--web`、`--no-lan-discovery`、`--allow-agents`、`--consent ui`）。`--downloads` 这类路径参数不收：路径由被控端按该用户的身份自己算（§5）。
- **生命周期**（实现时改了）：被控端跟着请求它的那条管道连接走。应用开着连接，被控端就一直运行；连接断开（应用退出、崩溃、用户注销）时服务立即结束它；服务停止时结束所有被控端。被控端的标准输出和错误由服务逐行转给应用（`{"line":…}`），退出时发 `{"exited":{"code":…}}`，所以应用照旧写 agent.log、照旧在被控端退出时处理（重启或报错），服务自己不重启被控端。
- **Ctrl+Alt+Del**：只有服务能发（`SendSAS(FALSE)`，sas.dll）。被控端收到查看端的 Ctrl+Alt+Del（工具条按钮或组合键）后，经控制管道请服务发送。`SoftwareSASGeneration` 不是 1 或 3 时（被组策略改了），发送前临时设为 1、发完恢复，同 RustDesk 的 `send_sas`。
- **版本**：管道只交换协议版本（`"hello"` → `{"hello":{"protocol":1}}`）；程序是否一致由应用直接比较 Program Files 里的 `nexa-agent.exe` 和自己那份（用户可读）。应用和服务版本不一致时，应用不用服务（退回现在的用户身份被控端），设置里显示“系统服务需要更新”和一个按钮（UAC 确认后更新服务目录里的文件）。以管理员身份打开的应用会自动更新它。

## 4. 被控端跟随输入桌面

- `display-capture`（DXGI 复制）、`input-inject`、`cursor` 三个线程在开始工作前 `OpenInputDesktop` + `SetThreadDesktop`（线程不能有窗口和钩子，这三个都没有）。
- 截屏：桌面切换时 DXGI 报 `ACCESS_LOST`；以 SYSTEM 运行时在新桌面上重建能成功（0.1.39 的重试逻辑已在，届时不再是 E_ACCESSDENIED）。重建前先重新绑定到当前输入桌面。
- 注入：每批输入前比较当前输入桌面的名字（`GetUserObjectInformationW(UOI_NAME)`），变了就重新 `SetThreadDesktop`，然后再 `SendInput`。
- 指针：`GetCursorInfo` 同样要在输入桌面上调用。
- 不以 SYSTEM 运行时，`OpenInputDesktop` 打不开安全桌面，行为和现在一样（停在最后一帧、输入无效）。所以这部分可以先做、先合入。
- **实现**（第 1 期）：`nexa-media/src/windows/desktop.rs` 的 `follow_input_desktop(now)`：`OpenInputDesktop(GENERIC_ALL)`，用 `UOI_NAME` 比较它和本线程当前桌面的名字，不同就 `SetThreadDesktop` 过去，并关掉本线程上一次打开的那个桌面句柄（句柄要一直开着，线程才能留在那个桌面上）。每个线程最多 200 毫秒查一次，`now` 时立即查。调用的地方：
  - `Duplication::open` 之前（立即查）：复制的是本线程所在桌面；桌面切换后 DXGI 报 `ACCESS_LOST`，重建时走到这里，跟到新桌面上。
  - `WinCursorSource::sample` 之前：指针线程和截屏线程读的都是输入桌面的指针。
  - 被控端的输入（`real_windows.rs` 的 `InjectorSink` 每个方法）之前：切换后最多约 200 毫秒的输入会落到旧桌面上被丢掉。
  - 每次线程换桌面，`agent.log` 记一行 `desktop: thread "…" moved from Default to Winlogon`；换不过去（线程有窗口或钩子）也记一行。
- **真机结果**：取帧线程（`display-capture`，调 `AcquireNextFrame`）不换桌面也能取到安全桌面的帧：复制对象建在哪个桌面就取哪个桌面的帧，所以只在 `Duplication::open` 前跟随就够了。

## 5. 以用户身份做的事（防止权限提升）

SYSTEM 身份的被控端，凡是会让远程的人“借 SYSTEM 做事”的地方，都必须换回会话用户的身份。用户令牌由被控端自己用 `WTSQueryUserToken(自己的会话)` 取得（SYSTEM 有 SE_TCB）；失败时取该会话 `explorer.exe` 的令牌，再不行取 `sihost.exe` 的（RustDesk `GetSessionUserTokenWin` 的 as_user 分支）。会话里没人登录（锁屏不算，登录界面算）时没有用户令牌，这些功能一律不可用，而不是退回 SYSTEM。

| 功能 | 现在 | 改成 |
|---|---|---|
| 命令行（终端、AI 的 `nexa_run`） | `CreateProcessW`，继承被控端身份 | `CreateProcessAsUserW(用户令牌)`，环境块用 `CreateEnvironmentBlock`（同 RustDesk 的 `run_as_user`） |
| 收到的文件 | 写到 `Downloads\NexaDesk` | 路径由用户的 `FOLDERID_Downloads` 算出，写文件时 `ImpersonateLoggedOnUser` |
| 发出的文件（拖到提示条上） | 被控端读文件 | 读文件时模拟用户身份 |
| 设备密钥、固定密码（DPAPI，用户范围） | 被控端直接读写 | 模拟用户身份后调用 `CryptProtectData`/`CryptUnprotectData` |
| 数据目录 `%APPDATA%\NexaDesk` | 读环境变量 | 用 `SHGetKnownFolderPath(FOLDERID_RoamingAppData, 用户令牌)` |
| 剪贴板 | 当前窗口站的剪贴板 | SYSTEM 进程在 `winsta0` 里可以直接用，不变；剪贴板里的文件路径按用户身份读 |
| agent.log | 写到用户目录 | 服务以用户身份打开文件，把句柄交给被控端的 stderr |

还要逐一检查：键盘规则文件、信令服务器证书文件等所有读用户目录的地方，读文件都在模拟用户身份时进行。

## 6. 应用（NexaDesk.exe）

- 设置里新增“在 UAC 弹窗和锁屏时也能控制（安装系统服务）”。开启时以管理员身份运行 `nexa-service.exe install`（UAC 确认）；关闭时同样确认后卸载。
- 开始“允许控制”时：服务在、版本一致 → 请服务启动被控端；否则照旧自己启动。停止时请服务结束它（现在是 `Kill`，SYSTEM 进程用户结束不了）。
- 和被控端的 IPC 管道：被控端以 SYSTEM 创建时，管道 DACL 允许“该会话的用户”而不是“所有者”（现在是 `D:P(A;;GA;;;OW)`），并加中等完整性标签，否则普通身份的应用连不上。
- 卸载 NexaDesk（Velopack 的卸载钩子以用户身份运行）删不掉服务：钩子里尝试以管理员身份运行 `nexa-service.exe uninstall`，被拒绝时在说明里写明怎样手动删除。
- 0.1.43 的“以管理员身份运行”在装了服务后就没必要了，设置里提示二选一。

## 7. 分期

| 期 | 内容 | 验证 |
|---|---|---|
| 1 | 被控端跟随输入桌面（§4） | 单元测试；在 Windows 机器上用 SYSTEM 身份手动运行被控端（计划任务以 SYSTEM 运行），弹出 UAC 看画面和点击 |
| 2 | 用户身份的事（§5）：命令行、文件、DPAPI、数据目录 | 单元测试；SYSTEM 身份下命令行里 `whoami` 是用户、收到的文件属于用户 |
| 3 | 服务程序（§3）：安装、控制管道、启动、重启、日志、Ctrl+Alt+Del | 在 Windows 机器上安装、启动、杀掉被控端看重启、注销看结束；锁屏时从查看端发 Ctrl+Alt+Del |
| 4 | 应用接入（§6）：设置、版本、更新、卸载、IPC 管道权限 | 界面测试；真机从 Mac 连 Windows：UAC 弹窗点“是”、锁屏输入密码解锁 |
| 5 | 文档、发版 | |

每期都能单独发版，前一期不依赖后一期才有用（第 1、2 期在没有服务时行为不变）。

## 8. 风险和以后

- **攻击面**：SYSTEM 进程处理网络数据。Rust 的内存安全降低了风险，但协议解析里的逻辑错误仍可能被利用。以后可以把网络和协议解析放进低权限进程，SYSTEM 进程只留截屏、编码、注入和一个很窄的本机接口（§5.1 的“第二阶段”）。
- **未签名**：Windows 版没有代码签名证书，服务无法用签名校验程序，只能靠“服务目录只有管理员能写”。
- **登录界面**：开机后还没人登录时，控制台会话里只有 winlogon，没有应用来显示确认窗口。服务已经按 winlogon 令牌启动被控端，下一期只要让服务在没人登录时也启动它，并且只接受固定密码（不需要确认窗口）；数据目录和设备密钥这时没有用户可用，要改存到机器范围（`%ProgramData%` + DPAPI 机器范围），这是下一期的主要工作。
- **杀毒软件**：SYSTEM 服务加远程控制，可能被一些杀毒软件拦截或报毒，需要在常见的杀毒软件下试。

## 9. 参照：RustDesk 的做法

读的是 `rustdesk/rustdesk` master 的 `src/platform/windows.cc`（全文）和 `src/platform/windows.rs`（前半逐字，后半只看了摘要）。

| 方面 | RustDesk | 本方案 |
|---|---|---|
| 进程 | 同一个程序：`--service`（服务）、`--server`（被控端）、无参数（界面） | `nexa-service.exe`（服务）、`nexa-agent.exe`（被控端）、`NexaDesk.exe`（界面），已经是分开的程序 |
| 安装位置 | 整个程序装在 `C:\Program Files\RustDesk`，`sc create … --service`，自动启动 | 应用仍用 Velopack 装在用户目录；只把服务和被控端复制到 `C:\Program Files\NexaDesk Service` |
| 启动被控端的令牌 | 目标会话 `winlogon.exe` 的令牌（`GetLogonPid`、`GetSessionUserTokenWin`），`CreateProcessAsUserW` | 同 |
| 服务循环 | `run_service`：定时检查活动会话和被控端是否退出，退出就重启；管道接收关闭、SAS 等指令 | 同，另加按会话的请求和参数白名单 |
| 桌面切换 | `inputDesktopSelected` 比较线程桌面和输入桌面的名字，`selectInputDesktop` 切换，`try_change_desktop` 在需要时调用 | 同（§4） |
| 以用户身份做事 | `run_as_user`：`explorer.exe`（或 `sihost.exe`）的令牌 + `CreateEnvironmentBlock` | `WTSQueryUserToken`，退回 explorer/sihost |
| Ctrl+Alt+Del | 安装时写 `SoftwareSASGeneration=1`；`send_sas` 必要时临时改策略后调用 `SendSAS` | 同 |
| 网络 | 据所知在 SYSTEM 的 `--server` 里（读的两个文件里没有网络代码，未从源码证实） | 第一期同样在 SYSTEM 的被控端里，以后再拆（§8） |
| 更新 | 整个程序在 Program Files，更新要管理员权限（重新运行安装脚本） | 服务目录的文件在版本不一致时经 UAC 更新，期间退回用户身份被控端（§3） |

