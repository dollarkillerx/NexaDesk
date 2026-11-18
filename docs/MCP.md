# NexaDesk MCP 方案

> 状态（2026-11-18）：A0（macOS，§9）、A1（被控端知情，§10）、A2 和 A3（§11）已实现。
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

