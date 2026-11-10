# NexaDesk Web 端方案

> 状态：方案草案（2026-11-10），排在 macOS 完成之后。尚未验证，也尚未开发。

目标：不安装任何软件，用手机或 PC 浏览器控制一台 NexaDesk 被控端。

## 1. 结论

| 问题 | 决定 | 理由 |
|---|---|---|
| 传输 | **WebRTC** 为主 | 浏览器里唯一能打洞的方式；WebTransport 只能连接可直达的服务器 |
| 被控端实现 | Rust `str0m`，新增一个 `FrameSink` | 无需引入庞大的 libwebrtc；`FrameSink` 接口已预留 |
| 视频 | H.264 媒体轨道（RTP） | 手机上硬解最稳；丢包恢复用浏览器自带的 NACK/PLI |
| 输入 | DataChannel：按键可靠有序，鼠标不可靠无序 | 对应原生端的流和数据报 |
| 身份 | SPAKE2（WASM）并绑定 DTLS 指纹 | 与原生端同样的安全目标 |

**WebTransport 作为第二阶段优化**：只用于局域网和 Tailscale 这类可直达的场景。

## 2. 为什么不能直接复用原生协议

原生端用的是自定义 UDP socket、打洞和双向 TLS 证书。浏览器不开放这些能力：

| 能力 | 原生端 | 浏览器 |
|---|---|---|
| 原始 UDP / 打洞 | 有 | 只有 WebRTC ICE |
| 自定义 QUIC | 有 | 只有 WebTransport（基于 HTTP/3，需连到可直达的服务器） |
| 双向证书身份 | 有 | 无，改由应用层认证 |
| 全局快捷键 | CGEventTap | 受限，Cmd+Tab 等系统键拦不住 |
| 系统剪贴板 | 完全控制 | 需要权限和用户手势 |

## 3. 方案比较

| | WebRTC（媒体轨道） | WebRTC（DataChannel + WebCodecs） | WebTransport + WebCodecs |
|---|---|---|---|
| 穿越 NAT | ICE / TURN | ICE / TURN | 不能，需可直达或中继 |
| 浏览器覆盖 | 最广 | 广 | Safari/iOS 需 26.4 起（由系统版本决定） |
| 延迟 | 有浏览器抖动缓冲，需实测 | 可控，接近原生 | 可控，接近原生 |
| 手机解码 | 平台硬解 | WebCodecs | WebCodecs |
| 复用现有 FEC/重组 | 否 | 可以（移植到 WASM） | 可以（移植到 WASM） |
| 工作量 | 最小 | 中 | 中，还需 HTTP/3 + WebTransport 服务端 |

**推荐路线**：先做媒体轨道，最快可用；如果实测延迟不够，再换 DataChannel + WebCodecs，被控端的 `FrameSink` 和控制消息都不用变。

## 4. 架构

```
浏览器（PC / 手机）                         nexa-agent
┌────────────────────────┐                ┌──────────────────────────┐
│ <video> ← H.264 轨道    │ ←─ RTP/SRTP ── │ WebRtcSink（FrameSink）   │
│ 触屏/键鼠 → DataChannel │ ─ SCTP/DTLS ─→ │ 输入 → 现有 Injector      │
│ SPAKE2（WASM）          │                │ nexa-auth                │
└──────────┬─────────────┘                └────────────┬─────────────┘
           └────────────── nexa-signal（SDP/ICE）──────┘
                          TURN（直连失败时）
```

被控端的改动很小：

- 采集和编码完全不变。Web 会话协商 H.264 即可，Apple 被控端的低延迟码控支持它（M0 实测）。
- `WebRtcSink` 实现 `FrameSink`，把 Annex-B 帧交给 `str0m` 打包。码流形式在设计时就定为 Annex-B，正是为了这里。
- 浏览器的 PLI/NACK 映射到现有的 `force_idr` 或 `invalidate_from`。
- 拥塞控制使用 `str0m` 的 BWE，再喂给编码器的 `set_bitrate`。

依赖：`nexa-signal`（信令）必须先完成，原生端的公网连接也需要它。

## 5. 编解码

- **只用 H.264**。Safari 的 WebCodecs 对 HEVC 有过 `isConfigSupported` 返回 true 但解码失败的报告，H.264 最稳妥。
- 手机浏览器的硬解能力和低延迟表现需要在真机上测。

## 6. 手机交互

这部分是 Web 端和桌面端差别最大的地方，建议单独设计：

| 手势 | 映射 |
|---|---|
| 单指点按 | 左键点击 |
| 单指拖动 | 模式可切换：移动光标（触控板模式）或直接拖动（触摸模式） |
| 双指点按 | 右键 |
| 双指滑动 | 滚轮 |
| 双指捏合 | 本地缩放画面，不发给被控端 |
| 虚拟键盘 | 组合输入提交后走 `TextInput`，另设 Ctrl/Cmd/Esc/方向键工具条 |

## 7. 浏览器限制

| 限制 | 影响 | 应对 |
|---|---|---|
| 系统快捷键拦不住 | Cmd+Tab、Win 键等到不了被控端 | 工具条按钮；Chromium 全屏时用 Keyboard Lock API |
| 剪贴板需要用户手势和权限 | 自动同步受限 | 粘贴事件触发时同步；iOS 上退化为手动按钮 |
| 页面进入后台 | iOS 会暂停 | 回前台时重连并请求关键帧 |
| 没有指针锁（手机） | 游戏模式不可用 | Web 端只提供办公模式 |
| 组合键改写 | 浏览器能拿到 `KeyboardEvent.code` | 复用 `nexa-input` 规则（编译为 WASM） |

## 8. 安全

- 浏览器没有设备密钥存储，首次连接必须用一次性码；可用 WebCrypto 的不可导出密钥保存身份，但清除站点数据后需要重新配对。
- SPAKE2 必须绑定 DTLS 指纹，否则信令可以中间人。
- **Web 页面本身在信任链上**：托管页面的服务器被攻破，就能下发恶意脚本窃取一次性码。建议支持自托管，并使用 SRI 和严格的 CSP。
- 必须走 HTTPS；局域网场景需要解决证书问题。
- 被控端的确认弹窗和会话提示同样适用，并显示“来自浏览器”。

## 9. 里程碑

| 阶段 | 内容 | 退出标准 |
|---|---|---|
| W0 验证 | `str0m` 推 H.264 到 Chrome、Safari、iOS Safari | 真机延迟数据；决定媒体轨道还是 WebCodecs |
| W1 | 信令、认证、桌面浏览器看和控 | PC 浏览器可完成办公操作 |
| W2 | 手机手势、虚拟键盘、前后台恢复 | iOS 和 Android 真机可用 |
| W3 | 剪贴板、TURN、局域网 WebTransport（可选） | 对外试用 |

## 10. 待验证

- `str0m` 的 H.264 打包和 BWE 能否满足低延迟要求
- 浏览器 WebRTC 抖动缓冲的实际延迟，手机上尤其需要实测
- WebTransport `serverCertificateHashes` 的证书要求（调研资料说法不一，需核对规范）
- iOS 上 WKWebView / 应用内浏览器的 WebTransport 支持情况

## 参考

- [MDN：WebTransport 构造函数](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport/WebTransport)
- [caniuse：serverCertificateHashes](https://caniuse.com/mdn-api_webtransport_webtransport_options_servercertificatehashes_parameter)
- [WebKit Bug 262950：WebCodecs HEVC 解码失败](https://bugs.webkit.org/show_bug.cgi?id=262950)
