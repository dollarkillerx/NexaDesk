# NexaDesk MCP 方案

> 状态（2026-11-19）：A0（macOS，§9）、A1（被控端知情，§10）、A2 和 A3（§11）已实现；agent 会话 G0（§13、§14）已实现并推送，取代 A0 的视频截图；G1–G3（§15）已实现，未提交。
> - 已验证：用本机的演示主机（`demo_host`，127.0.0.1:47900，VideoToolbox 真编真解）按 MCP 协议走完一遍：连接、截图、点击、输入、按键、滚动、断开，以及配对后不用码重连。被控端不接受 AI 代理时被拒且不弹窗；接受时确认请求标明是 AI 代理。
> - A2/A3 已验证：拖动、显示器切换在演示主机上实测；命令行和剪贴板在集成测试里对真实的被控端代码验证（命令行开的是本机真实的 shell）；macOS 安装包里带上了 `nexa-mcp`，Developer ID 签名、hardened runtime 下能正常连接截图。
> - 尚未测试：真实的被控端（应用里的确认窗口和提示条只看过渲染图）、经信令服务器连接、在 Claude Code 里实际调用，以及 Windows 上的 `nexa-mcp.exe`（只在 Mac 上交叉编译通过，没有在 Windows 上运行过）。

目标：让 AI agent（Claude Code、Claude Desktop 等支持 MCP 的程序）经 NexaDesk 看到并操作**另一台**电脑。连接方式和人用的一样：局域网、Tailscale、设备 ID 经信令服务器、TURN 中转，端到端加密，被控端照常确认、照常显示会话提示。

参考：[vercel-labs/agent-desktop](https://github.com/vercel-labs/agent-desktop) 用系统无障碍树让 agent 操作**本机**的应用，并以 `agent-desktop mcp` 把命令作为 MCP 工具提供。NexaDesk 补的是它没有的那一段：跨网络、经对方同意地操作别人的电脑。

## 1. 结论

| 问题 | 决定 | 理由 |
|---|---|---|
| MCP 服务放在哪 | **控制端**，新命令 `nexa-mcp` | 被控端不用开新的监听口；agent 在哪台机器上，就从哪台发起连接，和人用应用一样 |
| 传输 | 只用 stdio | 不开 HTTP 端口，本机其他进程和网络上的人都碰不到它；Claude Code / Claude Desktop 都支持 |
| 内核 | 直接用 `nexa-client` 的 Rust API（`Core`、`Session`），不经 C ABI | 和桌面端、iOS 同一个内核：打洞、重连、配对、码率控制全都现成 |
| 第一阶段操作方式 | 截图 + 坐标（和 Claude 的 computer use 一样） | 协议几乎不用改；主流模型都会用 |
| 第二阶段 | 被控端读无障碍树，按元素操作 | 准、快、省带宽；但要改协议、分平台实现，是独立的大功能 |
| 身份 | `nexa-mcp` 有**自己的设备密钥和数据目录** | 不继承桌面应用的配对，否则 AI 能免确认连上本机应用配对过的所有主机 |
| 被控端知情 | 握手里声明“这是 AI 代理”，被控端默认不接受，接受时确认窗口和提示条都写明 | 被控端的人必须知道是 AI 在操作 |

## 2. 架构

```
Claude Code 等 ─ MCP（stdio，JSON-RPC）─▶ nexa-mcp
                                          │ nexa-client::Core（无窗口）
                                          │   Session ── 无头 Presenter：只保留最新一帧
                                          │          └─ 终端流（nexa_client::terminal）
                                          ▼
                          现有会话：QUIC + SPAKE2 配对，局域网 / Tailscale / 信令 + TURN
                                          ▼
                                     被控端 nexa-agent（确认窗口、提示条、权限）
```

`nexa-mcp` 是一个独立的二进制（`crates/nexa-mcp`），随桌面应用一起发布（macOS 放在 `NexaDesk.app/Contents/MacOS/`，Windows 放在 `NexaDesk.exe` 旁边）。应用里给一段可复制的配置：

```json
{ "mcpServers": { "nexadesk": { "command": "/Applications/NexaDesk.app/Contents/MacOS/nexa-mcp" } } }
```

`nexa-client` 目前只有 macOS、Windows、iOS 的平台实现（`Deps`），所以 `nexa-mcp` 先支持 macOS 和 Windows。

## 3. 第一阶段：截图 + 坐标

### 3.1 工具

| 工具 | 参数 | 做什么 | 用到的现有能力 |
|---|---|---|---|
| `nexa_hosts` | — | 列出 `nexa-mcp` 自己配对过的主机和局域网里发现的主机 | `Ready.hosts`、`LanDevices`、`TailnetDevices` |
| `nexa_connect` | `address`（`host:port` 或 9 位设备 ID），`code?` | 连接；返回会话号、主机名、系统、显示器列表、获得的权限 | `Core::connect`、`ConnectRequest`（`known_host` / `code` / `request`） |
| `nexa_disconnect` | `session` | 断开 | `Session::close` |
| `nexa_screenshot` | `session`，`display?` | 返回当前画面（PNG），以及它的宽高 | **新写**：无头 Presenter，见 §3.2 |
| `nexa_click` | `session`，`x`，`y`，`button?`，`count?` | 移过去再点（双击 = `count: 2`） | `PointerMove` + `PointerButton` |
| `nexa_move` / `nexa_drag` | 坐标 | 移动；按下、移过去、松开 | 同上 |
| `nexa_scroll` | `session`，`x`，`y`，`dx`，`dy` | 在某处滚动 | `PointerWheel` |
| `nexa_type` | `session`，`text` | 输入文字（按 Unicode，不受对方键盘布局影响） | `SendText` |
| `nexa_key` | `session`，`keys`（如 `"cmd+c"`、`"enter"`） | 按组合键 | `KeyPress`（HID 用法，iOS 的快捷键面板用的同一条路） |
| `nexa_display` | `session`，`display` | 切换显示器 | `SelectDisplay` |
| `nexa_clipboard_get` / `_set` | `session`，`text?` | 读写对方剪贴板的文字 | `nexa-clip`；需要 `Permissions.clipboard` |
| `nexa_terminal_run` | `session`，`command`，`timeout_ms?` | 在对方 shell 里执行一条命令，返回输出 | `nexa_client::terminal`；需要 `Permissions.terminal` |

坐标都是**截图里的像素**。`nexa-mcp` 把它换算成会话的视图像素（`SessionCommand` 要的就是视图像素），见 §3.2。

动作类工具默认在动作之后等画面稳定（§3.3），再附一张新截图返回，省掉 agent 一次调用。可以用 `screenshot: false` 关掉。

### 3.2 无头画面

现在解码出的帧经 `PresenterSlot` 交给窗口的 Presenter（`crates/nexa-client/src/presenter.rs`）。没有窗口时帧被丢掉（“no view yet”）。要补的：

1. **`Session::attach_presenter(presenter, w, h)`**（Rust API，不进 C ABI）：挂一个不需要原生句柄的 Presenter，并像 `attach_view` 一样设置视图尺寸。把视图尺寸设成和流一样大，视图像素和流像素就是 1:1，坐标换算最简单。流的大小变化（`StreamChanged`）时跟着调。
2. **`SnapshotPresenter`**：`present()` 只把最新的 `DecodedFrame` 存起来（替换旧的，不排队），并记下时间。截图时才转换：
   - macOS：解码器要输出 CPU 可读的 `420v`。默认输出是 GPU 压缩缓冲，CPU 读不了（§6.6）。`nexa-media` 里已有 `DecoderOutput::CpuReadable420v`，目前只给测试用，要开放给 `nexa-mcp`。
   - Windows：硬解出来的是显存里的纹理（`NativeImage::Texture`），要先拷到 staging 纹理再读，这一段要新写；软解出来的已经是 `NativeImage::Cpu`，可以直接用。
   - NV12 → RGB → PNG。需要时先缩小，见下条。
3. **截图尺寸**：长边超过 1568 像素就缩小，这是模型看图的常见上限，再大也只是费 token。`nexa-mcp` 记下缩放比例，换算坐标时用。
4. **流的参数**：agent 只是偶尔看一眼，用不着 60 fps。连接时用 `SetMode` 把帧率压到 10 fps 左右；`SetResolution` 限到 1080 行，省带宽，被控端编码也轻松。静止画面精修（§6.8）照常起作用，文字依然清晰。

### 3.3 等画面稳定

点击之后，对方的画面要过一会儿才变，而且编码器在画面不变时可能根本不发新帧。所以“动作后的截图”这样取：

- 等到动作发出后的第一帧到达，然后再等 300 ms 没有新帧，就取最新的一帧；
- 最多等 2 秒，到时间就取当时最新的，并在结果里注明“画面可能还在变化”；
- 画面一直没变（没有新帧）也算稳定。

### 3.4 连接与凭据

- **数据目录**：`nexa-mcp` 用自己的数据目录（macOS `~/Library/Application Support/NexaDesk-mcp`，Windows `%APPDATA%\NexaDesk-mcp`），设备密钥、配对、信令服务器设置都在里面，和桌面应用分开。被控端看到的是另一台设备，名称默认为“<本机名>（AI 代理）”。
- **第一次连接**：用被控端显示的一次性码（`code`），或只给设备 ID 请求对方允许（`request`）。被控端的人确认时选择是否保存配对；保存后再连就不用码（`known_host`）。
- **一次性码可以作为工具参数**：它用一次就作废，出现在 agent 的对话记录里问题不大。
- **固定密码不作为工具参数**：密码长期有效，一旦进入对话记录就算泄露。如果要用固定密码，只能写在 `nexa-mcp` 的配置里（macOS 存钥匙串，Windows 用 DPAPI），工具里按主机引用。
- **信令服务器**：和桌面应用一样三选一（不用 / 公开服务器列表 / 自己的服务器）。设置写在 `nexa-mcp` 的配置文件里，访问口令同样存在钥匙串或 DPAPI 里，不经过工具参数。
- **同时连接**：最多 4 个会话，每个会话一个编号，工具都带 `session` 参数。

### 3.5 命令行

`nexa_terminal_run` 在会话的终端流上开一个 shell（需要被控端给了 `terminal` 权限），把命令和一个结束标记写进去，读到标记为止，返回输出（超过 64 KB 截断并注明）。一个会话保留一个 shell，前后命令共享工作目录和环境变量。超时就发 Ctrl+C。

对 agent 来说，能用命令行解决的事（查文件、跑脚本、看日志）用命令行，比点界面可靠得多。但它的权限也最大：等于对方登录用户能做的一切。所以确认窗口里这一项默认不勾（和现在一样）。

## 4. 被控端的改动

### 4.1 声明“我是 AI 代理”

- `ClientHello` 加一个字段 `agent`（bool，字段 9），`nexa-mcp` 填 true。（原方案是枚举 `kind`；目前只有两种，用 bool 更简单，以后真有第三种再换。）
- 被控端把它带进 `IncomingRequest`（加 `agent: bool`，和现有的 `browser` 一样）和 `SessionInfo`。
- `SessionAccept` 加 `agent_ack: bool`：被控端明确表示“我知道这是 AI 代理，并且允许”。

**`nexa-mcp` 收不到 `agent_ack` 就断开**。旧版被控端不认识 `agent`，会把它当普通应用放进来，但不会回 `agent_ack`。这样 AI 不会在一台不能如实显示“AI 在操作”的电脑上动手。

这个字段是控制端**自己声明**的。恶意的程序完全可以不声明，冒充人用的应用。所以它的作用是让诚实的工具如实告知，**不是**安全边界。安全边界仍是现有的配对、一次性码、确认窗口和权限。

### 4.2 设置、确认和提示

| 地方 | 改动 |
|---|---|
| 设置 | 主窗口“允许别人控制本机”下加“允许 AI 代理连接”，**默认关闭**。关闭时 `agent` 的请求直接拒绝（`SessionReject`，原因“这台电脑不接受 AI 代理”），不弹窗 |
| 确认窗口 | 标题写明“AI 代理请求控制”，图标区别于普通设备。权限默认只勾“查看”和“键盘鼠标”，剪贴板、文件、命令行都不勾 |
| 配对 | 可以保存配对，但“下次无需确认”（无人值守）对 AI 代理**默认不提供**，要在配对列表里对这台设备单独打开 |
| 提示条 | 会话期间的提示条写明“AI 代理正在操作”，颜色和普通会话不同，照常一键断开（§9.3 规定提示不可关闭，这里同样不可关） |
| 日志 | agent.log 记下 AI 代理会话的开始、结束和权限，不记具体操作内容（§9.4：不记录按键和剪贴板内容） |

### 4.3 不需要改的

画面采集、编码、输入注入、剪贴板、终端、文件传输都不用改。对被控端来说，`nexa-mcp` 就是一个不显示画面的控制端。

## 5. 第二阶段：无障碍树（另立项目）

第一阶段跑通后再决定做不做。做法：

- 协议加一组控制消息：`UiTreeRequest{display, root?, interactive_only}` → `UiTree{json, snapshot_id}`，`UiAction{snapshot_id, ref, action, value?}` → `UiActionResult`。
- 被控端读界面：macOS 用 AX API（被控端已经有“辅助功能”权限，注入输入用的就是它），Windows 用 UI Automation。可以直接嵌入 agent-desktop 的 Rust 库（Apache-2.0，`libagent_desktop_ffi`），也可以自己写一份精简的。
- 新权限 `ui_tree`：无障碍树里的文字比画面上看到的多，例如滚出屏幕的内容。所以单独授权，默认不勾。
- MCP 侧加 `nexa_ui_snapshot` / `nexa_ui_act`，和截图工具并存。agent 能按元素操作时用元素，不行再退回坐标。

**要先验证的**：
- 被控端 nexa-agent 是后台进程、没有焦点，在这种进程里读别的应用的 AX/UIA 树是否完整。
- agent-desktop 的库在这种环境里能不能用。
- Windows 上被控端运行在哪个会话、以什么身份运行（见 docs/WINDOWS.md），UIA 能不能读到用户桌面的窗口。

## 6. 测试

- **单元**：坐标换算（截图像素 ↔ 视图像素，含缩放和多显示器）、组合键解析（`"cmd+shift+4"` → HID 用法）、终端结束标记、MCP 消息的解析与错误。
- **集成**：沿用 `crates/nexa-client/tests` 的做法，用假的 `Deps` 和被控端。验证：
  - 没有 `agent_ack` 时断开；
  - 被控端关闭“允许 AI 代理”时被拒；
  - 权限不够时工具返回明确的错误，而不是悄悄失败。
- **端到端**：本机跑 `demo_host`（`127.0.0.1:47900`），用 MCP Inspector 或 Claude Code 调用全部工具；经信令服务器再测一遍。不连本机正在运行的 NexaDesk（47800）。
- **真机**：Mac 控 Windows、Windows 控 Mac 各一遍，让 Claude Code 完成一个小任务（打开某个应用、改一项设置），记录每步耗时和截图大小。

## 7. 里程碑

| 阶段 | 内容 | 退出标准 |
|---|---|---|
| A0 | `attach_presenter`、`SnapshotPresenter`（macOS），`nexa-mcp` 外壳：`connect` / `screenshot` / `click` / `type` / `key` | 用 demo_host 在 Claude Code 里完成一次点击和输入 |
| A1 | 被控端：`agent` / `agent_ack`、设置开关、确认窗口和提示条 | 旧版被控端被拒；新版能看到“AI 代理正在操作” |
| A2 | 终端、剪贴板、显示器、动作后自动截图、等画面稳定 | 真机上让 agent 完成一个小任务 |
| A3 | Windows 控制端（staging 纹理读回）、随应用发布、应用里的配置说明 | 两个平台的安装包都带 `nexa-mcp` |
| B | 无障碍树 | 另写方案 |

## 8. 待定

- **MCP 实现**：用现成的 Rust SDK（`rmcp`）还是自己写？stdio + JSON-RPC 本身很小，自己写只需几百行，也少一个依赖。倾向自己写，先比较两者的工作量。
- **截图格式**：PNG 对文字清楚，但照片多的画面体积大。要不要按画面内容在 PNG 和 JPEG 之间选？
- **是否允许 agent 自己保存配对**：现在的设计是被控端的人决定是否保存。还要考虑：是否让 `nexa-mcp` 的配置能禁止保存配对，每次都要码？
- **一台被控端同时有人和 AI**：现在最多 4 个会话、互不顶替。人和 AI 同时操作同一个指针会互相干扰。要不要在有 AI 会话时提示，或者让人一动鼠标 AI 就暂停？

## 9. A0 实现（与方案的差异）

代码：`crates/nexa-mcp`（MCP 服务和工具），`nexa-client` 里的 `snapshot.rs`（无头画面）和 `headless.rs`（无窗口的设备实现）。

| 方面 | 实现 |
|---|---|
| 工具 | `nexa_connect`、`nexa_sessions`、`nexa_screenshot`、`nexa_click`（左、右、中键，单击、双击、三击）、`nexa_scroll`、`nexa_type`、`nexa_key`、`nexa_disconnect`。滚动本来排在 A2，顺手做了。`session` 参数在只有一个会话时可以省略 |
| MCP | 自己实现（stdio，每行一条 JSON-RPC 消息），依赖只有 `serde_json`。协议版本 2025-11-25、2025-06-18、2025-03-26、2024-11-05：客户端要的版本在列表里就用它，否则用最新的。工具失败返回 `isError: true` 和原因，协议错误才返回 JSON-RPC 错误 |
| 画面大小 | 连上后把“视图”设成对方显示器按比例缩到长边 1568 的大小（例如 2560×1600 → 1568×980），被控端直接按这个大小编码。截图就是解码出的帧，不用再缩放一次，文字更清楚。帧比 1568 大时（被控端没照做）才在本地缩小 |
| 截图格式 | PNG（`png` crate，快速压缩）。演示主机 1280×800 的渐变画面约 180 KB |
| 坐标 | 截图像素 → 流像素（按比例）→ 视图像素（`ViewGeometry::to_view`，和窗口里的鼠标走同一套换算）。超出截图范围直接报错，不会夹到边上 |
| 等画面稳定 | 每帧取亮度的稀疏采样（每 16 像素一个），和上一帧的平均差小于 1.5 就算“没变”，容忍编码器精修造成的细微变化。默认动作后连续 500 ms 没变就截图，最多等 3 秒。演示主机的画面一直有移动的竖条，所以输入和按键后常常等满 3 秒，并在结果里注明“画面可能还在变化”。小的动画（光标闪烁、转圈图标）可能落在采样点之间，被当作没变：这正是想要的效果，不会因为光标闪烁一直等 |
| 按键 | 新增 `Deps::rewrites_keys()`，无窗口的客户端返回 false：按键原样发出，不经过 keymap.toml 里 Mac 和 Windows 之间的对调规则。agent 按对方系统的键名下指令，例如 Windows 被控端用 `ctrl+c`，Mac 被控端用 `cmd+c` |
| 本机隔离 | `HeadlessDeps` 不捕获本机键盘，不读写本机剪贴板（`clipboard()` 返回错误，所以剪贴板不同步），没有本机光标和声音输出 |
| 身份 | 设备密钥是数据目录里的 `device.key`（0600 文件，`FileKeyStore`），没有用钥匙串：命令行程序每次重新编译、签名变了，钥匙串都会弹窗要求授权，而 MCP 进程没有人能点。配对记在 `known_hosts.json`（和应用同一格式）；`hosts.json` 只记“哪个地址对应哪把主机公钥”，用来第二次不带码连接。只有被控端确认时选了保存（`paired_now`），才记进 `hosts.json` |
| 信令服务器 | 写在 `config.json`：`signal_server`、`signal_secret`。文件里有口令而别人能读（权限不是 600）时拒绝使用。按方案应存进钥匙串，留到 A3 |
| 设备名 | 默认“<主机名> (AI agent)”，可以用 `--name` 改。在 A1 的 `agent` 字段之前，被控端只能靠这个名字看出是 AI |
| 日志 | 标准输出只走协议；诊断写 `mcp.log`。不记录输入的文字和按键 |
| 帧率 | 请求 10 fps。演示主机不理这个请求，照样按 30 fps 发 |

**测试**：
- 单元测试：键名解析、坐标换算、缩放、PNG、YUV→RGB、等画面稳定、配置文件权限。
- 集成测试 `crates/nexa-mcp/tests/mcp.rs`：用真实的被控端代码（假设备）和 MCP 消息，验证点击落点（截图中心 → 对方屏幕中心，误差 1 点以内）、中文文字、`ctrl+c` 原样到达、只读会话拒绝操作、配对后不用码重连、未配对时的提示、协议错误码。

**使用**：

```sh
cargo build -p nexa-mcp --release
claude mcp add nexadesk -- /path/to/target/release/nexa-mcp
```

## 10. A1 实现（与方案的差异）

| 方面 | 实现 |
|---|---|
| 协议 | `ClientHello.agent`（字段 9）、`SessionAccept.agent_ack`（字段 6）。查看端（`nexa-session` 的 `viewer_handshake`）在 `ViewerConfig.agent` 为真而对方没回 `agent_ack` 时，关闭连接并报 `VersionMismatch`：“这台电脑的 NexaDesk 太旧，无法显示 AI 代理正在连接，需要更新”。`nexa-client` 通过新的 `Deps::is_agent()` 决定，只有 `HeadlessDeps` 返回 true |
| 被控端判定 | 在 `host_handshake` 里，紧接着验证之后：不接受 AI 代理时以 `ConsentDenied`、“this computer does not accept AI agents” 拒绝，不弹窗，也不计入别的东西。接受时：固定密码不再免确认（AI 代理总要问人）；确认窗口里建议的权限只有“查看”和“键盘鼠标”；不提供“下次无需确认”。配对列表里已经设成无人值守的设备照旧直接放行，那是机主自己的选择 |
| 开关 | 被控端进程启动时一律不接受（`Agent::set_allow_agents`）。应用连上被控端后发 `SetAllowAgents`（IPC 新消息，字段 13），设置改动时也立即发，**不用重启**；`Status.allow_agents`（字段 15）回报当前值。命令行：`nexa-agent --allow-agents`；演示主机：`--agents` |
| 设置 | 主窗口底部“允许 AI 代理连接”，存在 settings.json 的 `allowAgents`，默认关闭 |
| 确认窗口 | 标题“AI 代理「…」请求连接本机”，下面一段黄色说明（是程序不是人；会自己操作；提示条会一直标明；只在自己启动或正在等它时允许）。指纹照常显示 |
| 提示条 | AI 代理的那一行用紫色（Carbon Purple 40，`#BE95FF`）的左边线和圆点，标题前加“AI”标签，文字是“AI 代理「…」正在控制本机 / 正在查看本机屏幕”。普通会话仍是橙色 |
| IPC | `IncomingRequest.agent`（字段 15）、`SessionInfo.agent`（字段 7）；`agent_ipc.proto` 同时补上了此前漏写的 `browser`、`web_fingerprint`、`web_cert_custom`、`password_set` 和 `AUTH_REQUEST` |
| 浏览器 | 网页端没有 AI 代理的说法，`agent` 恒为 false |
| 配对列表 | 还不能看出哪台配对设备是 AI 代理（配对记录里没存）。设备名默认带 “(AI agent)”，暂时靠它分辨 |

**测试**：
- `nexa-agent/tests/host.rs`：
  - 默认拒绝 AI 代理，且不问人；
  - 允许后，确认请求标明是 AI 代理，建议的权限只有查看和键盘鼠标，并回 `agent_ack`；
  - 普通应用不会收到 `agent_ack`；
  - 固定密码不能让 AI 代理免确认。
- `nexa-session` 单元测试：没有 `agent_ack` 时查看端不继续。
- `nexa-mcp/tests/mcp.rs`：被控端不接受时，返回的错误写明原因。
- 桌面端（`AgentAccessTests`、`ProtoTests`）：
  - 设置默认关闭并会保存；
  - 连上被控端时、改设置时都会发 `SetAllowAgents`，且不需要重启；
  - 确认窗口和提示条的文字正确；
  - 和 Rust 共用的 golden 字节一致。
- 渲染图：`ui/out/consent-agent-*.png`、`banner-agent-*.png`。
- 实测：新版演示主机不带 `--agents` 时被拒；带 `--agents` 时确认请求标明“an AI agent”，会话正常。

## 11. A2、A3 实现（与方案的差异）

### A2：命令行、剪贴板、显示器、拖动

| 工具 | 实现 |
|---|---|
| `nexa_drag` | 先移到起点、按下，再分 12 步移到终点（每步间隔 16 ms，让程序看得到拖动过程），最后松开 |
| `nexa_display` | 不带参数时列出显示器并标出当前显示的那块；带编号时切过去：视图先按新显示器的比例调整，等被控端回报新的流配置和第一帧，再返回新显示器的截图。切换后之前的截图坐标作废 |
| `nexa_run` | 方案里叫 `nexa_terminal_run`。每个会话保留一个 shell（Mac/Linux 是用户的登录 shell，Windows 是 PowerShell），工作目录和环境变量在命令之间保留。规则如下 |
| `nexa_clipboard_get` / `_set` | 剪贴板放在 `nexa-mcp` 进程的内存里（`nexa_client::memclip`），从不读写本机系统剪贴板。被控端复制的内容同步到这里，`set` 的内容同步到被控端。`set` 会等被控端回报“已放到剪贴板”（`CLIPBOARD_SENT`，最多 3 秒）再返回，这样紧接着按 ⌘V/Ctrl+V 粘贴的一定是新内容。原因是无窗口客户端不经过改键规则，粘贴键也就不会触发粘贴闸门（§8.3）。多个会话共用这一个剪贴板，和真实的剪贴板一样 |

`nexa_run` 的细节：

- **结束标记**：每条命令后面跟一行，打印“结束标记 + 退出码”。标记由几段拼成（`printf '%s_%s' __NEXA_END <随机数>`），完整的标记只会出现在输出里，不会出现在回显里。
- **Mac/Linux 被控端**：先关掉回显和行编辑器（`stty -echo`、`set +o emacs`、`unsetopt zle`），清空提示符，设 `TERM=dumb`。命令用 `{ …; } </dev/null` 包起来，`cd` 和变量留在同一个 shell 里；标准输入为空，等着输入的命令会直接结束，不会卡住。
- **Windows 被控端（PowerShell）**：PowerShell 会回显，所以按行去掉回显出来的命令和 `PS …>` 提示符。这一路径只有单元测试，没在 Windows 上运行过。
- **输出清理**：去掉终端控制序列和回车。超过 64 KB 时只保留结尾，并注明被截断。
- **超时**：默认 30 秒，最多 600 秒。超时就发 Ctrl+C，再等 3 秒让 shell 回来；shell 继续可用。
- **权限**：客户端协议里的权限没有 `terminal` 这一项（桌面端也是打开终端时才知道），所以这里不事先检查：被控端没给，shell 会立刻以 `denied` 结束，工具返回“对方没有允许命令行”。
- **确认窗口**：AI 代理的确认窗口现在也提供“命令行”选项，和浏览器一样默认不勾。人的应用照旧不提供。

### A3：Windows、发布、应用里的说明、口令存放

| 方面 | 实现 |
|---|---|
| Windows | `HeadlessDeps` 改为 macOS 和 Windows 共用，平台差异只有系统类型、解码能力和解码器三处。Windows 用新增的 `MfDecoder::new_cpu`：始终在 CPU 上解码，帧直接在内存里，截图不需要从显卡读回（原方案是拷到 staging 纹理）。截图的画面小、帧率低，CPU 解码足够。`nexa-mcp.exe` 在 Mac 上用 mingw 交叉编译通过，只依赖系统 DLL，clippy 无警告；**没有在 Windows 上运行过** |
| 发布 | `scripts/package-macos.sh` 把 `nexa-mcp` 放进 `NexaDesk.app/Contents/MacOS/`，单独签名（标识 `app.nexadesk.mcp`，hardened runtime）。`scripts/package-windows.sh` 把 `nexa-mcp.exe` 放在 `NexaDesk.exe` 旁边并去掉符号 |
| 应用里的说明 | 主窗口设置卡片里新增可展开的“AI 代理（MCP）”，里面有 Claude Code 的 `claude mcp add nexadesk -- "<路径>"` 命令和 JSON 配置，各带复制按钮，路径就是应用旁边的 `nexa-mcp`。没找到这个文件（开发版）时给出提示。最后一行提醒：要让 AI 控制本机，勾选“允许 AI 代理连接” |
| 访问口令 | 信令服务器的访问口令不再写在 `config.json` 里，写了会报错并提示改用 `nexa-mcp set-signal-secret`：从标准输入读，所以不会出现在命令行和 shell 历史里。macOS 存进钥匙串（`app.nexadesk` / `mcp-signal-secret`），Windows 用 DPAPI 加密存在数据目录的 `signal-secret` 文件里。`clear-signal-secret` 删除。`config.json` 只剩 `signal_server` |

**测试**：
- `nexa-mcp` 单元测试 15 个：结束标记、PowerShell 回显清理、控制序列、截断、口令存取等。
- `nexa-mcp` 集成测试 12 个，新增：
  - 拖动的按下、移动、松开位置；
  - 显示器列表和不存在的编号；
  - 命令行：输出、退出码、`cd` 保留、等待输入的命令会结束、超时中断后 shell 仍可用、未授权时的提示；
  - 剪贴板：双向同步、未授权时的提示。
- `nexa-client`：内存剪贴板只察觉会话开始后的变化，被控端写入的内容不会被发回去。
- 桌面端：MCP 区块的命令、JSON、复制，以及 AI 代理“命令行”选项默认不勾、勾了才给。渲染图 `ui/out/main-mcp-*.png`。
- 实测：
  - 演示主机两块屏，切换后截图是 1024×768 的第二块屏；拖动落在第二块屏的全局坐标上（原点 1280）。
  - 打包出的 `.app` 里的 `nexa-mcp` 能连接、截图、点击。

**还没做的**：配对列表里标出 AI 代理设备（§10）；第二阶段的无障碍树（§5）；在 Windows 上实际运行 `nexa-mcp.exe`。

## 12. 在 Claude Code 里实测（2026-11-18）

**怎么跑的**：`claude -p`（无界面模式）加 `--mcp-config` / `--strict-mcp-config`，只加载 `nexa-mcp`，只放行 `mcp__nexadesk__*`。被控端是本机演示主机：两块屏，允许 AI 代理。任务六步：连接并描述画面、点中心、拖动、切显示器、输入加回车、断开。每次约 12 轮、53 秒、0.33 美元。

**第 1 次**：六步都完成，被控端收到的点击、拖动、文字和回车全部准确。Claude 提出的问题和改法：

| 问题 | 改法 |
|---|---|
| 显示器从 0 编号，连接结果里没写 | 连接结果改为“display 0 = 名称 宽×高, shown now; display 1 = …”，工具说明也写明从 0 开始 |
| 几乎每个动作都提示“画面还在变化”，看不出等了多久 | 改为“等了 3.0 s 画面仍在变化（可能有东西一直在动），这是最新的画面”，不再建议“再截一张” |
| 不知道输入是否到达 | 结果改为“发给对方当前有键盘焦点的程序，看截图确认”；`nexa_type` 的说明加上“先点一下输入框” |
| 截图里看不到指针 | 点击、拖动、滚动的结果写明“指针停在那里”；说明里写明截图里不画对方的指针 |
| 切换显示器后截图尺寸变了，没有提示 | 截图尺寸和上一张不同时，写明原尺寸，并说明之前的坐标不再适用 |
| `nexa_drag` 的参数没说明；`session` 省略时的规则；调用是否按顺序执行 | 都补上说明；说明里写明调用按发送顺序逐个执行 |

**第 2 次**：对方**一个点击、按键都没收到**，各工具却都报告成功。原因是 `nexa-client` 内核里的竞态：
- 内核先发出“已连接”事件，再启动输入线程；输入线程启动时读一次“是否开启输入”，读完后才标记“已启动”。
- `nexa-mcp` 一收到“已连接”就开启输入，这个调用如果正好落在“读完”和“标记已启动”之间，就被 `to_input` 丢掉了，输入线程从此一直是关闭状态，之后所有输入都被忽略。
- 修复：`start_input` 标记“已启动”后再读一次期望状态，和启动时读到的不同就补发一次；这两个标志改用 SeqCst。桌面端和 iOS 走同一段代码，也一起修好了。
- 验证：每次连接、截图后立即点击，修复前 30 次送达 26 次，修复后 30 次全部送达。

**第 3 次**（修复后）：六步全部送达。Claude 认为拖动后的截图和点击后的一样，可能是旧画面。查时间戳：两张截图相隔 6.40 秒，正好是演示画面竖条转一圈的时间，两张 PNG 也不相同，所以截图是新的，判断有误。另一条反馈是“画面还在变化”时有时无：原因是 12 像素宽的竖条有时落在每 16 像素一个的采样点之间。这个演示画面比较极端，暂时不改。


## 13. Agent 会话（方案）

> 状态（2026-11-19）：G0 已实现（§14），G1–G3 已实现（§15）。取代原先“在视频流里截图”的做法（§3.2）。被控端还没有发布过，所以不考虑兼容旧版。

### 13.1 结论

| 问题 | 决定 |
|---|---|
| agent 主要读什么 | **界面元素树**：前台窗口里的可交互元素（按钮、输入框、复选框、链接、菜单……），每个带编号 `@e1`、`@e2`、角色、名字、值、状态和位置。思路参考 [agent-desktop](https://github.com/vercel-labs/agent-desktop)：读树 → 按编号操作 → 再读一次确认。代码自己写（它的 core 和 macOS 两个 crate 约 4.5 MB 源码，内部接口也不稳定，整体引入太重） |
| 截图 | **按需取**，被控端直接截的无损 PNG，指针由系统画进去。树读不到东西（Electron 关了无障碍、画布、游戏、远程窗口）、要看视觉效果、要确认结果时，agent 自己调 |
| 视频 | agent 会话**不开视频**：被控端不采集、不编码，空闲时没有任何开销 |
| 人想在旁边看 | 用 NexaDesk 应用另开一个只看的会话（被控端最多同时 4 个会话）。两个会话各自确认、各自断开，互不影响 |
| 动作之后 | 默认只回一段文字：画面有没有变、是否已稳定、前台应用和窗口、焦点在哪个元素。也可以让它顺带返回新的元素树或截图（`then: "ui"` / `"screenshot"`） |
| 动作和观察的先后 | 每个动作之后在输入流上发一个**标记**；观察请求带上这个标记，被控端等它被注入线程处理完，才开始判断画面是否稳定 |
| 坐标 | 每块显示器一个“agent 坐标系”：原始像素等比缩到长边不超过 1568。截图、元素位置、点击坐标都用它，agent 不用做任何换算 |

### 13.2 协议

- **会话**：`ClientHello.agent = true` 的会话就是 agent 会话（A1 已有）。被控端不建视频管线，`SessionAccept` 之后直接开始。还会发一个 `StreamConfig{codec: Unknown, w, h, display}`，告诉查看端 agent 坐标系的大小。切换显示器沿用 `StreamRequest.display`，被控端再回一个新的 `StreamConfig`。
- **标记**：`InputBody::Mark{id}`（字段 5）。注入线程按顺序处理到它时，记下 id 和时间。
- **观察**：新的 QUIC 流类型 `Agent`（6），每次请求开一条双向流：

```
查看端 → ObserveRequest {
  after_mark: u64,        // 0 = 不等
  quiet_ms: u32,          // 多久没变化算稳定（默认 400）；0 = 不等稳定
  max_ms: u32,            // 最多等多久（默认 3000）
  ui: bool,               // 要元素树
  interactive_only: bool, // 只要可交互元素（默认 true；false 时也列出文字）
  max_nodes: u32,         // 默认 300
  screenshot: bool,       // 要截图
}
被控端 → ObserveReply {
  display, w, h,          // agent 坐标系
  settled, changed, waited_ms,
  app, window, focused,   // 前台应用名、窗口标题、焦点元素的一句描述
  pointer: Option<(x, y)>,
  ui: String,             // 元素树（文字，见 13.3）；不可用时为空，并在 ui_error 里说明原因
  ui_error: String,
  png_len: u32,           // 之后跟着这么多字节的 PNG（分块发送）
}
```

### 13.3 元素树的文字格式

一行一个元素，按层级缩进（只保留有意义的层级）：

```
window "未命名 — 文本编辑" (0,0 1568×980)
  @e1 button "关闭" (14,10 16×16)
  @e2 textarea value="hello" focused (0,52 1568×900)
  @e3 checkbox "自动换行" checked (1200,20 100×20)
```

- 编号只在这一次观察里有效。`nexa-mcp` 记下每个编号对应的中心点，agent 用 `nexa_click {ref: "@e1"}` 点击。编号来自旧的观察时直接报错，让 agent 重新读树。
- 密码框（`AXSecureTextField`）不给出值。
- 超过 `max_nodes` 就截断，并在末尾注明。

### 13.4 被控端（先做 macOS）

- **元素树**：`ApplicationServices` 的 AX API。
  - 从系统级元素取焦点应用，再取它的焦点窗口，深度优先遍历 `AXChildren`。
  - 每个元素读 `AXRole`、`AXSubrole`、`AXTitle`、`AXDescription`、`AXValue`、`AXEnabled`、`AXFocused`、`AXPosition`、`AXSize`，换算成 agent 坐标；不在当前显示器上或大小为 0 的跳过。
  - 每个应用设 `AXUIElementSetMessagingTimeout`（1 秒），卡住的应用不会拖住被控端。
  - 被控端注入输入本来就要“辅助功能”权限，读界面树用的是同一项权限，不用另外授权。
- **截图**：ScreenCaptureKit 的 `SCScreenshotManager`（macOS 14 起，正好是最低版本）。直接按 agent 坐标系的大小输出 BGRA，带系统指针，编码成 PNG。
- **画面稳定**：先等标记处理完，再每 100 ms 截一张很小的缩略图（长边 160）比较。连续 `quiet_ms` 没有变化就算稳定，最多等 `max_ms`。只在有观察请求时才做，空闲时零开销。
- **Windows**：元素树改用 UI Automation，截图从 DXGI 读 BGRA，放到下一阶段。在此之前，Windows 被控端的 agent 会话只有输入、剪贴板和命令行，观察请求返回“这台电脑暂不支持”。

### 13.5 nexa-mcp 的工具

| 工具 | 说明 |
|---|---|
| `nexa_ui` | 读前台窗口的元素树（默认只列可交互元素） |
| `nexa_screenshot` | 无损截图，附带前台应用、窗口和焦点信息 |
| `nexa_click` | `ref` 或 `x`、`y` 二选一 |
| `nexa_type` | 可以带 `ref`：先点它，再输入 |
| `nexa_scroll`、`nexa_drag`、`nexa_key`、`nexa_display`、`nexa_run`、`nexa_clipboard_*` | 同前 |
| 动作后的 `then` | `"none"`（默认，只回文字）、`"ui"`、`"screenshot"` |

视频方式的代码（`nexa_client::snapshot`、无头 Presenter、CPU 解码）在 `nexa-mcp` 改完后就没有用处了，一并删掉。

### 13.6 阶段

| 阶段 | 内容 |
|---|---|
| G0 | 协议（agent 会话不开视频、标记、观察流）；macOS 被控端的元素树和截图；`nexa-mcp` 改用新工具。用演示主机和 Claude Code 实测 |
| G1 | 截图上标出元素编号（set-of-marks），树读不全时用 |
| G2 | Windows 被控端：UI Automation 和 DXGI 截图 |
| G3 | 按编号做无头操作（AXPress、直接设值），不移动指针；浏览器网页内容的树裁剪 |

## 14. G0 实现（与方案的差异）

> 状态（2026-11-19）：已实现，未提交。演示主机上全部工具走通，Claude Code 实测通过（见下）。**真实 macOS 的元素树和截图还没跑过**：为了不在这台 Mac 上弹出辅助功能、屏幕录制的授权，只用演示主机和测试里的假实现验证。

### 实现

| 位置 | 内容 |
|---|---|
| `nexa-wire` | `StreamType::Agent = 6` |
| `nexa-session` | `InputBody::Mark`（字段 5）；`agent.rs`：`ObserveRequest`、`ObserveReply`、`AgentMsg{Observe, Reply, Png, Refused}`、`agent_size`（长边 ≤ 1568）；`Marks`（注入线程记下处理到的标记，观察时等它）；Agent 流单帧上限 256 KiB，PNG 按 64 KiB 分块 |
| `nexa-agent` | `agent_session.rs`：agent 会话单独一套循环，不建视频管线，只开输入、剪贴板、命令行和观察流；`agent_view.rs`：平台接口 `AgentView`（截图、前台信息、元素树、指针位置）、树的文字渲染、缩略图比较、PNG 编码；`mac_view.rs`：macOS 实现（AX 原生 FFI，`AXManualAccessibility` 打开 Electron 的无障碍，1 秒消息超时，最多 6000 个元素、40 层、3 秒） |
| `nexa-media` | `macos::screenshot_rgba`：`SCScreenshotManager` 单张截图，按 agent 坐标系大小输出，带指针 |
| `nexa-client` | `Session::mark`、`Session::observe`；agent 会话收到 `codec = Unknown` 的 `StreamConfig` 时把视图大小设成 agent 坐标系，坐标原样通过 |
| `nexa-mcp` | 工具按 §13.5 重写；说明文字改成“先 `nexa_ui`，按编号操作，看不懂再截图” |
| 删掉的 | `nexa_client::snapshot`、`nexa-mcp` 的 `image.rs`、`attach_presenter`、`MfDecoder::new_cpu`、`PixelBuffer::with_planes`，以及 `nexa-mcp` 对 `png` 的依赖 |

### 与方案的差异和补充

- **“画面变了没有”不只看像素**。像素比较的门槛（缩略图 160 像素宽，差别超过 24 的像素多于 0.2%）是为了不被闪烁的光标干扰，可这样一来打一两个字也看不出变化。所以还比较前台应用、窗口、焦点元素和**焦点元素内容的指纹**（哈希；密码框只算字数；超过 2 万字的只算字数和光标/选区位置，不把整篇拷出来）。指纹只用来比较，不发给 agent。
- **标记之后再等 100 ms**：指针移动走的是数据报，不在有序的输入流上，标记到了不代表指针已经移过去。
- **元素不在当前显示器上**：没有坐标，也不给编号。指针不在当前显示器上时，不报告指针位置。
- **切换显示器**：清掉 `nexa-mcp` 记下的编号和上一次的画面。上一次的画面还记着是哪块显示器的，切换时正在进行的观察写进来的旧画面不会被拿来比较。
- **编号的坐标**：被控端在 `ObserveReply.refs` 里直接给出每个编号的中心点，`nexa-mcp` 不再从文字里解析。
- **读树失败时**：`nexa-mcp` 清掉旧编号，再用旧编号会报错并建议改用截图，不会点到上一个窗口里的位置。
- **截不了缩略图时**（没有屏幕录制权限、显示器没了）：`ObserveReply.watch_error` 说明原因，动作结果写 “Whether the screen changed is unknown: it could not be watched (…)”，不再误报“画面一直在变”。
- **显示器不休眠**：视频采集本来会阻止显示器因空闲而休眠，agent 会话没有采集，所以由 `Platform::keep_display_awake` 在整个会话期间阻止（macOS 用 IOPM 断言，Windows 在一个专门的线程上设执行状态）。
- **截图**：ScreenCaptureKit 列出的显示器缓存 5 秒，等画面稳定时不必每 100 ms 都查询一次；缓存过期或截图失败就重新查询一次。
- **元素树（macOS）**：
  - 除了前台窗口，也读打开的右键菜单（应用的 `AXMenu`）和菜单栏。菜单栏只列标题，只有展开的那个菜单才列出菜单项。
  - 表格、列表的行（`AXRow`）可以操作，名字取行里的第一段文字；有标题的图片（比如 Finder 里的文件图标）也可以操作。
  - 不在当前显示器上的元素不占 `max_nodes` 的名额（它们的子元素照样会看）。滚动到很后面的长列表，不会把屏幕上的按钮挤掉。
  - 遍历因上限停下（元素太多、太深、超过 3 秒）时，树的末尾会注明“可能有遗漏”。
- **上限**：`max_nodes` 默认 300、最多 2000（向平台要 4 倍，渲染时截断，末尾注明还有多少没列出）；`max_ms` 在被控端最多 10 秒。`nexa-mcp` 的默认值是 `quiet_ms` 400、`max_ms` 3000。
- **Windows 被控端**：还没有 `AgentView`，观察请求返回 `Refused`（“这台电脑暂不支持”）；输入、剪贴板、命令行照常可用。
- **动作的结果文字**：例如 `Double-clicked (180, 120). The screen changed and settled after 0.4 s. In front: TextEdit — "Untitled". Keyboard focus: textarea. Pointer at (180, 120).`
- **演示主机**（`--agents`）：画一个假的窗口，有“Click me”按钮、“Name”输入框和点击次数。截图里也画出这些状态：焦点时输入框变黄，每次点击加长绿条，每个字符一个黑块，全选时变蓝。输入框支持退格和 Cmd/Ctrl+A。

### 测试

- `nexa-agent/tests/host.rs`，对真实的被控端代码，平台用假实现：
  - agent 会话不开编码器、给出 agent 坐标系；
  - 元素树带编号，截图是无损 PNG；
  - 观察会等最后一个动作；
  - 观察会等画面稳定，并报告变化（包括只有输入框的值变了的情况）；
  - 没有 `AgentView` 的被控端拒绝观察；
  - 会话期间保持显示器不休眠，会话结束后放开；
  - 截不了缩略图时说明原因；
  - 回复里带每个编号的中心点。
- `nexa-mcp/tests/mcp.rs`：
  - 读树之前按编号点击会报错；
  - 按编号点击落在元素中心；
  - 按坐标点击加 `then: "screenshot"`；
  - 坐标超出屏幕时报错；
  - 拖动；
  - 切换显示器；
  - 读树失败后旧编号不能再用；
  - 截不了缩略图时，动作结果说明原因。
- 整个 workspace 的 `cargo test` 通过。`cargo clippy` 在 macOS、Windows（gnu 交叉）和 iOS 上都没有警告。

### 实测（2026-11-19，演示主机 127.0.0.1:47900）

- 用脚本按 MCP 协议调用全部工具：
  - 读树、按编号点击、按坐标双击；
  - 带 `ref` 输入，`then: "ui"` 和 `"screenshot"`；
  - 按键、滚动、拖动，坐标越界报错；
  - 切换显示器、断开。
  - 被控端打印的注入事件和结果文字一致。
- Claude Code（`claude -p`，只开放 `mcp__nexadesk__*`），任务是“输入框先打 draft，再整个换成 Hello NexaDesk，然后点按钮直到显示 7 次”：
  - 14 轮、25 秒、0.21 美元，一次截图都没用；
  - 读树，按编号输入，Cmd+A 后覆盖，点了 7 次，并用 `then: "ui"` 确认。
  - 上一轮输入框还不能删字，它试了几种办法后如实报告做不到，没有乱点。

### 待做

- **在真实 Mac 上跑一遍**：
  - 辅助功能树：普通应用、Electron、Safari 网页、系统设置；
  - 截图；
  - 稳定判断的门槛（0.2% / 24）。
  - 需要装了被控端的另一台 Mac，或者先在这台 Mac 上授权。
- 全选、高亮这类细小的变化，像素和焦点值都看不出来，会报告“没变”。实际中是否误导 agent，要看真实使用。
- G1–G3 见 §15。

## 15. G1–G3 实现（与方案的差异）

> 状态（2026-11-19）：已实现，未提交。macOS 部分在演示主机和测试里验证；**Windows 部分（G2）只在 Mac 上交叉编译、clippy 通过，没有在 Windows 上运行过**。真实 Mac 的辅助功能操作同样还没跑过。

### G1：截图上标出编号（set-of-marks）

- `ObserveRequest.marks`（字段 8）：截图时顺带读一次元素树，在截图上给每个编号画框，并在框的左上角（上方放得下就放在上方）贴一个同色的标签，写上编号。颜色按编号轮换，共 6 种。数字用内置的 3×5 点阵放大 3 倍（9×15 像素），不依赖字体。
- 树也一起返回，编号随之替换成这棵树的编号。
- `nexa_screenshot {marks: true}`：编号和树里的文字对不上号时用，比如图标按钮、名字重复的元素。

### G3：不移动指针，按编号操作

- 协议：Agent 流上新增 `ActRequest`（字段 5）和 `ActReply`（字段 6），一条流一个动作。动作有 `PRESS`、`FOCUS`、`SET_VALUE`。`ObserveReply.tree`（字段 19）给每棵树编号，`ActRequest` 带上它，不是最后一棵树就拒绝（“call nexa_ui again”）。
- 被控端记着最后一棵树里每个编号的元素句柄（`UiNode.element`）。读树失败也会作废旧句柄。动作要“键盘和鼠标”权限，先等 `after_mark` 之前的输入处理完再做。
- macOS：
  - `PRESS` 用 `AXPress`；表格的行没有 AXPress，改为设 `AXSelected`。
  - `FOCUS` 设 `AXFocused`，`SET_VALUE` 设 `AXValue`。
  - 错误码翻译成 agent 能照着做的话，比如“元素没了，重新 nexa_ui”“这个元素不能这样操作，改用点击”。
- `nexa-mcp`：
  - 新增 `nexa_press {ref, then}` 和 `nexa_set_value {ref, text, then}`；
  - 做完照常等画面稳定，报告变化；
  - 说明里写明什么时候用它们：点击点不中，不想移动指针，一次填好整个输入框。
- **网页的树裁剪**：
  - 整块不在当前显示器上的部分（大小不为 0）不再往下遍历。很长的网页、滚得很远的列表，只看屏幕上的那一段，不会因为遍历上限停在屏幕外。
  - 链接、按钮没有名字时，用里面的第一段文字当名字。

### G2：Windows 被控端

- `win_view.rs`：
  - **元素树**：用 UI Automation 读前台窗口的元素。缓存请求加控件视图，每个元素的子元素一次跨进程调用取回。应用超时 1.5 秒，遍历上限和 macOS 相同（6000 个元素、40 层、3 秒），也同样裁剪屏幕外的部分。
  - **打开的右键菜单**：是桌面顶层的单独窗口，也会读进来。
  - **控件类型的对应**：Button/SplitButton → button，Edit → textfield 或 password，ListItem/TreeItem/DataItem → row，Hyperlink → link，等等。密码框不读值。
  - **前台信息**：程序名取可执行文件名，窗口标题用 `GetWindowTextW`。焦点元素用 UIA 读，内容只比指纹。
  - **操作**：`PRESS` 依次尝试 Invoke、Toggle、SelectionItem.Select、ExpandCollapse、LegacyIAccessible 的默认动作；`FOCUS` 用 `SetFocus`；`SET_VALUE` 用 ValuePattern（只读的输入框会报错）。
  - 每次调用都加入多线程 COM 套间，所以保存下来的元素句柄可以在任何阻塞线程上使用。
- **截图**（`nexa_media::windows::screenshot_rgba`）：用 GDI 的 `StretchBlt`（HALFTONE，带 CAPTUREBLT）按 agent 坐标系的大小截取，再用 `DrawIconEx` 画上光标。不需要视频那套 DXGI 复制。
- 坐标：Windows 被控端的“点”就是物理像素，和 UIA、`GetCursorPos` 用的一致。

### 测试

- `nexa-agent/tests/host.rs`：
  - 按编号操作：最后一棵树的编号有效，旧树的编号被拒；平台的错误原样返回；整个过程不注入任何指针或按键输入。
  - 没有“键盘和鼠标”权限时拒绝操作。
  - 带编号的截图在元素框上画了标记，元素中间不动。
- `nexa-mcp/tests/mcp.rs`：读树前不能按编号操作；带编号的截图同时返回树；`nexa_press`、`nexa_set_value` 都能用，并且不移动指针。
- `agent_view` 单元测试：标记框、标签和数字的位置与颜色。
- 演示主机实测：带编号的截图（见上），`nexa_press` 和 `nexa_set_value` 都起作用，指针一直停在 (0, 0)。
- 整个 workspace 的测试通过。clippy 在 macOS、Windows（gnu 交叉编译，包括 `win_view` 和 GDI 截图）和 iOS 上都没有警告。

### 待做

- 在 Windows 上实际跑一遍：
  - 记事本、资源管理器、Edge 网页、设置的元素树；
  - 截图和光标；
  - 操作是否生效；
  - 高 DPI 多显示器下的坐标。
- 在真实 Mac 上跑一遍 G0–G3（同 §14 待做）。

## 16. 由本机 NexaDesk 应用登录，`nexa-mcp` 只转发（方案）

> 状态（2026-11-19）：M1、M2 已实现，未提交（见 16.5）。M3 还差真实的 MacBook，需要先在 Mac mini 上装好新版应用。参考 MySSH：MCP 服务在桌面应用里，`myssh-mcp` 只是一个转发程序，凭据和主机列表都由应用管。

### 16.1 为什么改

现在的 `nexa-mcp` 自己负责连接：一次性码由 AI 传进来，签名服务器的接入密钥存在它自己的钥匙串条目里。这样 AI 会碰到凭据，而且已经在应用里保存好密码、配对过的电脑，AI 也用不上。改成：**谁来登录、用什么登录，都由本机的 NexaDesk 应用决定**。AI 只说“连哪台”，看不到也传不了任何密码、一次性码或密钥。

### 16.2 结论

| 问题 | 决定 |
|---|---|
| MCP 服务在哪里 | 在 NexaDesk 应用的进程里，由 Rust 内核（libnexa_client）运行。macOS 用 `~/Library/Application Support/NexaDesk/mcp.sock`：目录 0700，套接字 0600，并校验对方的 uid。Windows 用只有当前用户能连的命名管道。做法照搬 nexa-agent 的本地 IPC |
| `nexa-mcp` | 改成转发程序：在 stdin/stdout 和上面的套接字之间原样搬运字节，没有任何凭据，也不建自己的内核。应用没有运行，或者 MCP 开关没开时，用一句话报错后退出。Claude、Codex 等的配置不用改，仍然指向应用旁边的 `nexa-mcp` |
| 总开关 | 设置 →“AI 代理（MCP）”里加“让本机的 AI 应用使用 NexaDesk”，默认关闭。关闭时停止监听，断开已连上的 AI 应用，并结束所有 AI 会话 |
| AI 能连哪些电脑 | 只能连应用里已有的设备（最近的设备、已配对的设备），而且每台都要单独打开“允许 AI 操作”，默认关闭。这个开关存在 `recent.json` 里该设备的记录上 |
| 怎么登录 | 和人在应用里点“连接”时的顺序相同：已保存的固定密码 → 已配对 → 设备 ID 请求对方允许（用应用里设置的信令服务器和它的接入密钥）。密码只在应用进程里从钥匙串取出来用，不出这个进程 |
| AI 用哪个身份 | AI 会话用应用里**单独的一个内核**，有自己的设备密钥（钥匙串里另一个条目）和自己的配对记录（`<数据目录>/agent/`）。这个内核标明自己是 AI（`is_agent`），被控端总会询问，并显示这是 AI。不和人共用设备密钥，是因为人把某台设备设成了“无人值守”时，AI 不能借这个身份跳过询问 |
| 工具 | `nexa_devices`：列出允许 AI 操作的设备，包括名字、设备 ID 或地址、系统，以及能用什么方式登录。`nexa_connect {device}`：设备的名字、设备 ID 或地址，只能从这个列表里选，**不再接受 `code`**。其余工具（`nexa_ui`、`nexa_click`、`nexa_press`……）不变 |
| 本机能看到什么 | 应用里显示 AI 会话：哪个 AI 客户端（MCP 的 clientInfo，比如 “claude-code”）、在操作哪台设备，可以一键全部停止 |
| 旧的独立模式 | `nexa-mcp` 不再自己连接，`config.json`、`hosts.json`、`set-signal-secret` 都去掉。测试仍然在进程内直接驱动 MCP 服务（用假的设备目录） |

### 16.3 结构

```
Claude Code / Codex ──stdio──▶ nexa-mcp（转发）──本地套接字──▶ NexaDesk 应用
                                                            ├─ C#：开关、每台设备的“允许 AI 操作”、AI 会话列表
                                                            └─ libnexa_client
                                                                 ├─ 人用的内核（RealDeps）
                                                                 └─ MCP 服务 + AI 用的内核（is_agent，单独的密钥和配对记录）
                                                                      └─ 设备目录：recent.json + 钥匙串里保存的密码
                                                                                + settings.json 的信令服务器和接入密钥
```

- MCP 的工具代码（现在的 `nexa-mcp/src/server.rs`、`viewer.rs`、`keys.rs`、`terminal.rs`）移进 nexa-client，成为 `nexa_client::mcp` 模块，只在 macOS 和 Windows 上编译。这样它和应用在同一个动态库里，避免 nexa-mcp ↔ nexa-client 的循环依赖。
- `Viewer` 的连接改为向一个 `Directory`（设备目录）要“这台设备怎么登录”。应用里的实现从上面那些文件和钥匙串读取，测试里用假的。
- 新增导出：`nexa_mcp_serve(on)`、`nexa_mcp_status()`（AI 会话列表，给 C# 显示）、`nexa_mcp_stop_all()`。
- 每个 MCP 客户端一条连接。连接断开（AI 退出）时，它开的会话一起结束。

### 16.4 阶段

| 阶段 | 内容 | 验证 |
|---|---|---|
| M1 | 把 MCP 搬进 nexa-client；`Directory`；本地套接字服务；`nexa-mcp` 改成转发程序；`nexa_devices` 和新的 `nexa_connect` | 集成测试：经套接字驱动，用假目录和假被控端，密码只出现在目录里 |
| M2 | C#：总开关、每台设备的“允许 AI 操作”、AI 会话列表和“全部停止”；三种语言的文字 | 界面测试 |
| M3 | 用演示主机，再用真实的 MacBook，在 Claude Code 和 Codex 里实测 | 用 MacBook 跑一次测速 |

### 16.5 M1、M2 实现

- **代码位置**：
  - `nexa_client::mcp`：`server`、`viewer`、`keys`、`terminal` 从 nexa-mcp 搬来，并新增 `listen`、`app_directory`，以及 `mod.rs` 里的 `Service`。只在 macOS 和 Windows 上编译，iOS 不受影响。
  - `nexa-mcp` 只剩 `main.rs`，是个转发程序，唯一的依赖是 nexa-client。`--socket` 可以指定套接字，测试时用。
- **设备目录**：
  - `viewer::Directory` 有两个方法：`devices()` 列出允许 AI 操作的设备；`sign_in(device)` 给出怎么登录（`SignIn`：认证方式、信令服务器、接入密钥）。
  - `AppDirectory` 的数据来源：
    - 允许 AI 的设备：`recent.json` 里 `aiAllowed` 为真的记录，名字优先用你起的标签；
    - 固定密码：钥匙串，经新增的 `abi::load_saved_password`，只在本进程内使用；
    - 信令服务器：`settings.json` 的 `signalServer`；
    - 接入密钥：钥匙串的 `signal-access:<服务器>`。
- **登录顺序**：
  1. 先看 AI 自己的配对记录（`<数据目录>/ai/hosts.json`），有就直接用，不需要任何秘密。被控端说“不认识”时，删掉这条记录，往下走。
  2. 再用设备目录给的方式：保存的固定密码；或者，对设备 ID 发请求，由对方点允许。
  3. 两者都没有时，报错并说明原因（“NexaDesk has no way to sign in to …”）。
- **会话归属**：
  - 每个 AI 应用的连接对应一个 `Server`，它开的会话只有它自己看得到，连接断开时一起结束。
  - `initialize` 里的 `clientInfo.name` 会记下来，给应用显示。
- **本地套接字**：
  - macOS：`<数据目录>/mcp.sock`。要求目录属于本人且其他人不可写；套接字权限 0600；用 `getpeereid` 校验对方的 uid。旧的套接字连不上就删掉；连得上，说明已有一个 NexaDesk 在服务。
  - Windows：命名管道 `nexadesk-mcp-<数据目录的 FNV-1a>`，只有所有者能连，拒绝远程客户端，首个实例独占。
  - 停止服务时，用 `hang_up` 主动断开每个连接。
- **导出**：`nexa_mcp_start(data_dir, name)`、`nexa_mcp_stop`、`nexa_mcp_stop_sessions`、`nexa_mcp_status`。状态是 JSON：`{serving, sessions: [{client, device}]}`。
- **工具**：
  - 新增 `nexa_devices`，列出名字、地址和登录方式；
  - `nexa_connect {device}` 按名字（不分大小写）、地址或设备 ID 连接，`code` 参数去掉了；
  - 说明文字改成先用 `nexa_devices`。
- **应用（C#）**：
  - `IAgentServer` / `NativeAgentServer`：开启服务后每 2 秒读一次状态，有变化时在 UI 线程通知。
  - `AppSettings.AiApps`：开关，保存后下次启动照旧。
  - `RecentDevice.AiAllowed`：在设备的“编辑”里勾选，列表上显示“允许 AI”标签，重新连接后保留。
  - MCP 区：开关、开启失败的原因、“AI 正在操作”列表（客户端 → 设备）和“全部停止”。英、中、日三种语言。
- **去掉的**：`nexa-mcp` 的 `config.json`、`hosts.json`、`set-signal-secret` / `clear-signal-secret`（`secret.rs`），以及 `NexaDesk-mcp` 数据目录。
- **测试**：
  - `nexa-client/tests/mcp.rs` 16 个：
    - 原有工具测试改用假的设备目录；
    - 没有允许的设备时拒绝，并说明；
    - AI 只能连允许的设备，名字不分大小写；
    - 没有登录方式时说明原因；
    - 用自己的配对记录重新连接；
    - 经真实套接字的端到端测试：权限 0600、按 clientInfo 显示、AI 应用断开后会话结束、停止后套接字被删除。
  - `app_directory` 的单元测试。
  - C#：开关的保存和启动、开启失败、会话列表和“全部停止”、设备的 AI 标记。
  - 整个 workspace 和 329 个桌面测试都通过；clippy 在 macOS、Windows 和 iOS 上都没有警告。
- **实测（演示主机）**：
  - `examples/mcp_demo` 按应用的方式提供服务：设备目录里只有演示主机，用它的演示密码。
  - Claude Code 经 `nexa-mcp --socket` 依次调用 `nexa_devices` → `nexa_connect {device: "NexaDesk demo"}` → 读树、输入、点击 3 次，结果正确。整个过程中，AI 没有接触任何密码。
  - 演示主机询问时标明“(an AI agent)”；Claude Code 退出后，会话随之结束。
- **注意**：macOS 套接字路径最长 104 字节。默认路径大约 60 字节，没有问题；家目录特别长时，开启会失败，并显示原因。


## 17. 文件收发（实现）

> 状态（2026-11-19）：已实现，随下一个版本发布。

### 17.1 结论

| 问题 | 决定 |
|---|---|
| 发文件给对方 | `nexa_send_file {path}`：本机文件的绝对路径。走现有的文件传输（流类型 4），落在对方的 `~/Downloads/NexaDesk`（Windows 是用户目录下的 `Downloads\NexaDesk`），同名加编号，不覆盖。超过 256 MB 仍要对方的人确认。需要“传输文件”权限 |
| 从对方取文件 | `nexa_receive_file {path, save_as?}`：对方机器上的绝对路径。协议原来只能由持有文件的一方发起，所以新增一条消息 `FileRequest {path}`（`FileMsg` 字段 6）：查看端开一条文件流发请求，被控端在同一条流上按原流程发送（角色对调）。SHA-256 校验通过才保留 |
| 谁能取 | 只有同时允许了“传输文件”和“命令行”的会话。命令行本来就能读对方的任何文件，所以这不增加 AI 的能力；没有命令行时，仍然没有任何办法读对方的文件。普通查看端（人用的应用）不对外提供文件（`serve_requests: false`） |
| 取到哪里 | 默认放在 AI 的下载目录 `<数据目录>/ai/downloads`，结果里写明路径；`save_as` 可以指定本机的绝对路径（必须不存在，所在目录必须存在，先检查再传，跨磁盘时复制） |
| agent 会话的文件权限 | 原来 agent 会话一律不开文件（§13 时的简化）。现在和普通会话一样：确认窗口勾了“传输文件”才有。被控端的提示条照常显示正在传的文件，对方的人也能把文件拖到提示条上发给 AI |

### 17.2 协议

```text
查看端                                   被控端
  FileRequest { path }            ──→    会话有命令行？是绝对路径、可读的普通文件？
                                  ←──    FileOffer { name, size }
                                         （否则 FileAnswer { NotFound(11) | NotPermitted }）
  FileAnswer { Ok }               ──→
                                  ←──    FileChunk ×n、FileEnd { sha256 }
  FileResult { status }           ──→
```

- 被控端：`TransferConfig.serve_requests = perms.terminal`（普通会话和 agent 会话都一样）。路径不是绝对路径、不存在、是目录时回 `NotFound`；没有命令行时回 `NotPermitted`；发送名额满时回 `Busy`。发送过程算作被控端的一次发出传输，界面上看得到。
- 查看端：`Transfers::request_file(path)` 立刻返回传输编号（方向是“收”，名字先取路径最后一段，收到 `FileOffer` 后改成对方给的名字）。查看端自己要的文件不再弹确认；大小上限和磁盘空间检查照旧。
- 旧版被控端不认识字段 6，会一直等 `FileOffer`，10 秒后断开这条流；查看端把它报成“没有给”，提示需要对方 0.1.28 或更新并允许命令行。
- `FileStatus::NotFound` 在两边的界面里沿用“不是文件”（`FailReason::NotAFile`），IPC 和应用不用改。

### 17.3 工具

- `nexa_send_file {session?, path, timeout?}`：等到对方收好才返回，默认最多 300 秒（1–3600），超时取消。结果写明文件名、大小和对方的落点。
- `nexa_receive_file {session?, path, save_as?, timeout?}`：同上，结果写明本机路径。
- 失败时说清原因：对方的人拒绝、太大、没有空间、校验不符、断线、超时、没有这个文件、没有权限（取文件时提示需要命令行和新版本）。
- `nexa_connect` 的权限说明里，允许了文件会写“files”。说明文字（instructions）也提到这两个工具。

### 17.4 顺带修的

- `nexa_run` 在 zsh 上的输出前面有一长串空格：zsh 的 `PROMPT_SP` 在提示符前把行补满。初始化命令里加了 `unsetopt prompt_sp prompt_cr`。

### 17.5 测试

- `nexa-session/tests/transfer.rs`：
  - 查看端按路径取文件（比确认阈值大，也不问人），内容一致，被控端记为一次完成的发出传输；
  - 没有命令行、没有文件权限时都拒绝；不存在的路径、相对路径、目录都回“不是文件”，什么都不写。
- `nexa-client/tests/mcp.rs`（经真实被控端）：
  - 发过去落在对方下载目录，再按路径取回到 `save_as`，内容一致；`save_as` 已存在时拒绝；不给 `save_as` 时放进 AI 的下载目录；不存在、目录、相对路径都报“没有这个文件”；
  - 只有文件没有命令行时取文件被拒，并说明；没有文件权限时两个工具都说明。
- 演示主机加了 `--grant-all`（再允许文件和命令行，命令行是本机的真 shell），并打印 `DOWNLOADS` 目录。经 `mcp_demo` + `nexa-mcp --socket` 实测：5 MB 随机文件发过去、取回来，三份 SHA-256 一致；不存在的路径报错清楚。
- macOS 和 Windows 目标的 clippy 都没有警告。

### 17.6 还没做

- Windows 被控端上的实测（代码共用，没在真机上跑过）。
- 一次传多个文件、传目录（现在一次一个文件；目录可以先用 `nexa_run` 打包）。
