# NexaDesk 详细设计

> 状态：草案 v0.1（2026-11-09）。总体方案见 [DESIGN.md](DESIGN.md)，本文定义到模块接口、线上格式、状态机和参数。
>
> 标记约定：
> - **【已核实】**：已对照依赖的源码或 SDK 头文件确认。
> - **【待验证】**：需要运行才能确认，列入 §15 的 M0 验证。
> - **【初值】**：数值是起点，由测试调参，不是结论。

## 0. 基线与依赖

| 组件 | 版本 | 说明 |
|---|---|---|
| Rust | stable，`rustup` 安装（本机尚未安装） | workspace，edition 2024 |
| quinn | **锁定 0.11.x**（0.11.12） | master 已是 0.12，接口可能变化，升级需重新过 §15 S2 |
| rustls | 0.23 | 自定义证书校验 |
| tokio | 1.x | 只跑网络和控制，媒体不在 tokio 工作线程上做重活 |
| prost | 0.14 | 控制消息 |
| reed-solomon-simd | 3.1 | 分片长度必须为偶数；原始/恢复分片各 ≤ 32768 【已核实】 |
| spake2 | 0.4 | 发布前需做安全评审 |
| ed25519-dalek、rcgen | 当前稳定版 | 设备身份和证书 |
| windows / objc2-* | 0.62 / 0.3 | 平台绑定 |
| csbindgen | 1.9 | 生成 C# P/Invoke |
| .NET / Avalonia | 10 / 12.x | NativeAOT |
| NVENC 头文件 | nv-codec-headers（API 13.1） | 动态加载，运行时用 `NvEncodeAPIGetMaxSupportedVersion` 检查 |

**许可**：RustDesk 只作参考，不引入其任何 crate 或复制代码。

### 与总体设计的差异（已同步修订 DESIGN.md）

| 项 | 原方案 | 修订 | 依据 |
|---|---|---|---|
| quinn 拥塞窗口 | 窗口跟随目标码率 | 窗口恒大于 `u32::MAX`，彻底关闭 quinn 的阻塞和 pacing，全部由应用层控制 | quinn 在此条件下跳过 pacing；两个控制器叠加会互相干扰（§4.8） |
| MTU | 1200 起，PMTUD 上调 | **固定 1200，关闭 PMTUD** | 虚拟路径切换时 QUIC 不知情，MTU 大的路径切到 Tailscale 会黑洞（§4.8） |
| VideoToolbox 恢复 | 认为无参考帧失效 | 有 LTR 接口，纳入恢复阶梯 | SDK 头文件（§6.5） |
| macOS 编码 | 未区分编码格式 | 所有模式用低延迟码控 + LTR，HEVC 优先；无硬件 4:4:4 | M0 实测（§6.5） |
| Pacing | 按编码码率摊到半帧 | 按链路容量估计，否则 120 fps 固定多出 4 ms | M0 实测（§4.9） |

## 1. 进程与线程

### 1.1 被控端 `nexa-agent`

| 线程 | 职责 | 优先级 |
|---|---|---|
| `capture-encode-{n}`（每显示器一个） | 取帧 → 转换 → 同步编码，内联执行 | Windows：MMCSS `Capture`；macOS：`QOS_CLASS_USER_INTERACTIVE` |
| `media-tx` | 分片、FEC、pacing、发送 | 同上 |
| `net`（tokio，2 worker） | quinn 驱动、控制流、路径管理 | 高 |
| `input-inject` | 注入键鼠，隔离阻塞调用 | 高 |
| `clipboard` | 系统剪贴板所有者窗口 / 轮询 | 普通 |
| `audio` | 采集 + Opus | MMCSS `Pro Audio` |

macOS 上采集是推模式：`SCStream` 回调线程只把 IOSurface 放入单槽信箱，由 `capture-encode` 线程取走。

### 1.2 控制端 `libnexa_client`

| 线程 | 职责 |
|---|---|
| `net`（tokio，2 worker） | 同上 |
| `media-rx` | 循环读数据报，时间戳、重组、FEC 解码、发反馈 |
| `decode-{n}` | 硬解 |
| `render-{n}` | 取最新帧、Present |
| `input-capture` | Raw Input / CGEventTap，运行组合键改写 |
| `clipboard`、`audio` | 同被控端 |

### 1.3 背压铁律

媒体路径上**没有无界队列**，且遵守下面的丢弃位置规则：

```
采集 ──(内联)── 编码 ──[有界 2]── media-tx ──→ 网络 ──→ media-rx ──[有界 4 帧 FIFO]── 解码 ──[单槽，新帧覆盖]── 渲染
   ▲                      │
   └── 队列满：跳过本次编码 ┘
```

1. **只在编码前丢帧。** 编码后丢帧会破坏参考链，迫使恢复。`media-tx` 落后时，下一帧直接不编码。
2. **解码器必须按序收到每个完整帧**，可以丢的只有解码后、呈现前的帧。
3. 解码 FIFO 积压超过 2 帧时，上报 `DecoderSlow`，被控端降帧率。

## 2. Crate 边界与核心接口

```
nexa-wire  ←  nexa-net
    ↑            ↑
nexa-media   nexa-input   nexa-clip      （互不依赖，均只依赖 nexa-wire）
    ↑            ↑            ↑
    └── nexa-agent / nexa-client ──┘     （组装层，唯一知道全局的地方）
```

`nexa-input` 的改写器与 `nexa-clip` 的状态机是不依赖 OS 的纯逻辑，平台代码放在 `platform/` 子模块，放在 feature 后面。

```rust
// nexa-media
pub enum GpuFrame<'a> {
    D3d11 { tex: &'a ID3D11Texture2D, subresource: u32 },
    IoSurface(&'a IOSurfaceRef),
    Cpu(&'a CpuFrame),
}

pub enum CaptureEvent<'a> {
    Frame { frame: GpuFrame<'a>, captured_at: Instant, dirty: &'a [Rect] },
    Idle,                       // 超时内没有新内容
    ModeChanged(DisplayMode),   // 需重建编码器
    AccessLost,                 // 需重建采集器（桌面切换等）
}

pub trait Capturer: Send {
    fn next(&mut self, timeout: Duration) -> Result<CaptureEvent<'_>>;
}

pub trait Encoder: Send {
    fn caps(&self) -> &EncoderCaps;
    /// 同步：返回时 out 已写满本帧码流。
    fn encode(&mut self, f: &GpuFrame, o: &EncodeOpts, out: &mut Vec<u8>) -> Result<EncodedMeta>;
    fn set_bitrate(&mut self, bps: u32) -> Result<()>;
    fn invalidate_from(&mut self, first_lost: FrameId) -> Result<Recovery>;
    fn on_frame_acked(&mut self, frame: FrameId);  // LTR 用，其余后端空实现
    fn force_idr(&mut self);
}

pub enum Recovery { RefInvalidated, IntraRefresh, Idr }

pub trait Decoder: Send {
    fn decode(&mut self, au: &[u8], meta: &FrameMeta) -> Result<DecodedFrame>;
}

pub trait Presenter: Send {
    fn resize(&mut self, w_px: u32, h_px: u32, scale: f32) -> Result<()>;
    fn present(&mut self, f: &DecodedFrame, mode: PresentMode) -> Result<PresentStat>;
}

// nexa-net
pub trait RateObserver: Send + Sync {
    fn on_rate(&self, r: RateAllocation);  // 由组装层转给 Encoder/FEC/令牌桶
}

// nexa-input
pub struct Rewriter { /* §7.2 */ }
impl Rewriter {
    pub fn on_key(&mut self, ev: PhysKey, ctx: FocusClass) -> SmallVec<[Output; 6]>;
    pub fn on_button(&mut self, ev: PhysButton) -> SmallVec<[Output; 6]>;
    pub fn reset(&mut self) -> SmallVec<[Output; 16]>;  // 失焦：释放全部
}

// nexa-clip：平台后端
pub trait ClipboardBackend: Send {
    fn read_formats(&mut self) -> Result<Vec<FormatInfo>>;
    fn read(&mut self, kind: ClipKind) -> Result<Vec<u8>>;
    fn write_promise(&mut self, gen: Gen, formats: &[FormatInfo], inline: &[(ClipKind, Bytes)]) -> Result<()>;
}
```

## 3. 线上协议

### 3.1 流与数据报

QUIC ALPN：`nexa/1`。控制端是 QUIC 客户端，被控端是服务端。

可靠流的第一个字节是类型，之后每条消息为 `varint 长度 + protobuf`。

| 类型 | 名称 | 方向 | quinn 优先级 |
|---|---|---|---|
| 1 | Control | 双向 | 10 |
| 2 | Input | 控制端 → 被控端（单向） | 10 |
| 3 | Clipboard | 双向 | 0 |
| 4 | File（每次传输一条） | 双向 | -10 |

**数据报首字节为类型，多字节字段小端。**

| 值 | 类型 | 方向 |
|---|---|---|
| 0x01 | VideoShard | 被控 → 控制 |
| 0x02 | AudioPacket | 被控 → 控制 |
| 0x03 | Pointer | 控制 → 被控 |
| 0x04 | TransportFeedback | 控制 → 被控 |
| 0x05 | VideoReport | 控制 → 被控 |
| 0x06 | Nack | 控制 → 被控 |
| 0x07 | ClockPing / Pong | 双向 |

【已核实】quinn 组包时 DATAGRAM 帧排在 STREAM 帧之前。视频方向若积压，同向的控制消息要排队；这由 `media-tx` 的浅队列规避。输入方向基本只有小数据报，不受影响。

### 3.2 VideoShard（头 28 字节）

| 偏移 | 类型 | 字段 | 说明 |
|---|---|---|---|
| 0 | u8 | type | 0x01 |
| 1 | u8 | stream | 显示器流 |
| 2 | u8 | flags | bit0 IDR；bit1 恢复点 |
| 3 | u8 | ref_epoch | §6.3 |
| 4 | u32 | tx_seq | 全传输序号，视频和音频共用，用于拥塞控制 |
| 8 | u32 | frame_id | 单调递增 |
| 12 | u16 | shard_index | `< k` 为原始分片，否则为校验分片 |
| 14 | u16 | k | 原始分片数 |
| 16 | u16 | m | 校验分片数，可为 0 |
| 18 | u16 | shard_len | 偶数；同帧所有分片相同 |
| 20 | u32 | frame_len | 去填充前的编码帧长 |
| 24 | u32 | capture_ts_us | 被控端单调时钟低 32 位 |
| 28 | — | payload | 长度 `shard_len` |

每包都带帧级字段：多花几字节，换来任意子集都能重组，不必依赖某个“首包”。

### 3.3 其余数据报

```
TransportFeedback（每 25 ms 或累计 128 个包发一次）【初值】
 0 u8  type  | 1 u8 rsv | 2 u16 fb_seq
 4 u32 base_tx_seq
 8 u32 base_arrival_us  （接收端时钟）
12 u16 count
14 u8[] status：
    0x00        = 未收到
    0x01..0xFE  = 收到，与上一收到包间隔 (v-1)×250 µs
    0xFF + u16  = 收到，长间隔，单位 250 µs

VideoReport（未解决期间每 max(RTT, 5 ms) 重发一次）
 0 u8  type | 1 u8 stream | 2 u8 client_epoch | 3 u8 kind
       kind: 1 Loss；2 NeedIdr；3 FrameAck；4 DecoderSlow
 4 u32 last_good_frame | 8 u32 first_lost_frame | 12 u32 latest_seen_frame

Pointer（24 字节）
 0 u8  type | 1 u8 flags（bit0 相对）| 2 u16 seq
 4 i32 x    | 8 i32 y    绝对：虚拟桌面像素；相对：累计总量，回绕相减
12 i32 wheel_h_total | 16 i32 wheel_v_total   累计，单位 1/120 格
20 u8  display | 21 u8[3] rsv

ClockPing：{ t1 } → ClockPong：{ t1, t2, t3 }  每 250 ms 一次，同时是 §4.5 的活跃路径心跳
```

Pointer 的处理规则：
- 丢包不丢位移，因为被控端用累计值相减。
- 最后一次移动后，发送端在 20 ms、60 ms 重发最后一包，防止停在错误位置。
- 按键事件走可靠流，并携带当时的绝对坐标或累计量；被控端先把指针对齐到该值再按下，**点击位置永远正确**。

时钟偏移：取最近 30 个样本中 RTT 最小的一个计算，仅用于 HUD，不参与控制。

### 3.4 控制消息（节选）

```protobuf
message ClientHello {
  uint32 proto = 1;  string device_name = 2;  Os os = 3;
  Caps caps = 4;     Auth auth = 5;           // Known | Code | Password
}
message AuthStep  { bytes spake_msg = 1; bytes confirm = 2; bytes salt = 3; }
message SessionAccept {
  Permissions perms = 1;  repeated Display displays = 2;
  Caps caps = 3;          Os os = 4;
}
message StreamConfig {
  uint32 display = 1;  Codec codec = 2;  Mode mode = 3;
  uint32 w = 4; uint32 h = 5; uint32 fps = 6;  Chroma chroma = 7;
}
message Caps {
  repeated CodecCap encode = 1;  repeated CodecCap decode = 2;
  bool rfi = 3; bool ltr = 4;  uint32 max_fps = 5;
}
message CodecCap { Codec codec = 1; Chroma chroma = 2; uint32 max_w = 3; uint32 max_h = 4; }
message FocusContext { FocusClass cls = 1; string app_id = 2; }
message CursorShape  { uint64 id = 1; uint32 w = 2; uint32 h = 3; int32 hot_x = 4; int32 hot_y = 5; bytes png = 6; }
message CursorSet    { uint64 id = 1; bool hidden = 2; }
message CandidateUpdate { repeated Candidate c = 1; }
message PathSwitch   { uint32 path_id = 1; }

// Input 流
message KeyEvent { uint32 hid = 1; bool down = 2; bool repeat = 3; }
message ButtonEvent { uint32 button = 1; bool down = 2; sint32 x = 3; sint32 y = 4; uint32 display = 5; }
message TextInput { string utf8 = 1; }
message KeyStateSync { repeated uint32 pressed = 1; uint64 seq = 2; }
```

未知字段按 protobuf 语义忽略。协议主版本不同则拒绝连接；能力差异通过 `Caps` 协商。

### 3.5 编解码协商

被控端取 `encode ∩ decode`，按模式打分选一项：

| 模式 | 打分顺序 |
|---|---|
| 办公高清 | HEVC 4:4:4 > AV1 4:4:4 > AV1 4:2:0 > HEVC 4:2:0 > H.264 |
| 均衡 | AV1 > HEVC > H.264 |
| 游戏 | 优先支持 RFI 的硬编：HEVC > AV1 > H.264 |

能力探测在 agent 启动时做一次，按“驱动版本 + GPU”缓存到 `caps.json`：不只查询，还要实际创建一次编解码会话，避免驱动谎报。

### 3.6 实现修订

组装时对本章的修正，以 `crates/nexa-session` 为准：

- Control 和 Input 流上的消息用 `oneof` 信封（`ControlMsg`、`InputMsg`）区分。
- 新增 `SessionReject`、`Goodbye`、`StreamRequest`：控制端在 `SessionAccept` 后请求流，被控端以 `StreamConfig` 应答。
- `Pointer` 坐标是某一显示器的流像素，`Display.id` 为索引；混合 DPI 下不再使用虚拟桌面像素。
- 直连模式使用普通 UDP 端点，因为 `VirtualSocket` 只接受已验证路径，无法监听未知来源。
- 被控端在无 UI 且无终端时拒绝所有连接，包括已配对的无人值守设备，因为无法向本机显示会话状态。

## 4. nexa-net

### 4.1 虚拟 socket

实现 `quinn::AsyncUdpSocket`，用 `Endpoint::new_with_abstract_socket` 接入。【已核实】接口：`create_io_poller`、`try_send`、`poll_recv`、`local_addr`、`max_transmit_segments`、`may_fragment`。

```rust
struct VirtualSocket {
    udp4: quinn_udp::UdpSocketState, udp6: quinn_udp::UdpSocketState,
    relay: Option<RelayLink>,
    active: ArcSwap<PathTarget>,   // 发送路径，无锁读取
    paths: Mutex<PathTable>,
    quic_rx: ...,                  // 过滤后的 QUIC 包
}
```

- **发送**：QUIC 传来的目的地址恒为固定虚拟地址，`try_send` 改写为 `active` 指向的真实目标。
- **接收**：
  - 来自中继地址的包先剥中继头。
  - 首字节 `0x00–0x03` **且** 第 4–7 字节为 STUN magic cookie：STUN，交给路径管理器。只看首字节不够，quinn 默认开启 QUIC bit grease（RFC 9287），约 3% 的短头包首字节也落在这个范围，集成测试中曾因此丢包。
  - 其余：QUIC。**只接受来自已验证路径的源地址**，`RecvMeta.addr` 改写成虚拟地址。
- **GSO**：`max_transmit_segments` 取路径能力。直连时透传 quinn-udp 的 GSO/USO；中继路径上拆成单包。
- 每个会话一组 socket，`SO_SNDBUF/RCVBUF` 设 4 MiB【初值】，打 DSCP（Windows 上没有 QoS 策略时会被忽略，属尽力而为）。

### 4.2 候选

```rust
struct Candidate { addr: SocketAddr, kind: CandKind, foundation: u32 }
enum CandKind { Host, Ipv6, Srflx, Portmap, Tailnet, Relay }
```

| 来源 | 方法 |
|---|---|
| Host / IPv6 | 枚举网卡，排除回环和常见虚拟网卡 |
| Srflx | 向两个不同 IP 的 STUN 服务器发 Binding |
| Portmap | PCP → NAT-PMP → UPnP（`igd-next`），限时 800 ms |
| Tailnet | 网卡地址落在 `100.64.0.0/10` 或 `fd7a:115c:a1e0::/48` |
| Relay | 信令下发 |

**NAT 分类**：两个 STUN 返回的映射端口相同 → `Easy`（EIM）；不同 → `Hard`（EDM），并记录端口差用于增量预测。

STUN 手写最小子集（Binding、XOR-MAPPED-ADDRESS、MESSAGE-INTEGRITY-SHA256），约 200 行，不引入依赖。

### 4.3 信令

`nexa-signal` 通过 WSS（443）提供，消息为 protobuf。

```
→ Register   { pubkey, x25519_pub, sig(server_nonce) }
← Registered { short_id, stun_servers[], relays[] }
→ Connect    { target_short_id, session_id, sealed }
← Incoming   { from_pubkey, session_id, sealed }
→ Answer     { session_id, sealed }
→ Trickle    { session_id, sealed }   // 后续候选
← Error      { Offline | RateLimited | Unknown }
```

- `sealed`：用对端 X25519 公钥做 HPKE 封装，内含候选与 ufrag/pwd。X25519 公钥由 Ed25519 身份签名。
- 服务器被攻破时最多阻断或看到元数据，无法冒充（真正的身份校验在 §9）。
- 限速：每 IP、每 ID 的连接请求令牌桶。
- 不落库，全部内存状态；重启后客户端重新注册。

### 4.4 连通性检查

控制端发起，候选两两配对，同地址族才配。

| 参数 | 【初值】 |
|---|---|
| 配对上限 | 64 |
| 发送节奏 | 每 20 ms 一个检查 |
| 单配对重传 | 100、200、400 ms |
| 总时限 | 3 s，然后只保留中继 |

STUN Binding 携带 `USERNAME = remote_ufrag:local_ufrag`，用 `pwd` 做完整性校验，防止伪造。双向成功的配对标记为 `Validated`。

**连接状态机（控制端）**

```
Idle → Resolving → Gathering → Checking → QuicHandshake → Authenticating
     → AwaitConsent → Streaming ⇄ Reconnecting → Closed
```

| 状态 | 超时 | 超时后 |
|---|---|---|
| Resolving | 5 s | 报错 Offline |
| Gathering | 1 s | 用已有候选继续（后续以 Trickle 补充） |
| Checking | 3 s | 只用中继；仍失败报错 |
| QuicHandshake | 3 s | 换下一条已验证路径 |
| AwaitConsent | 60 s | 报错 Denied |
| Reconnecting | 30 s，指数退避 | Closed |

第一条 `Validated` 路径立刻启动 QUIC，**不等更好的路径**，其余继续探测。

### 4.5 路径管理

```
score = rtt_ms + 8 × loss_pct + penalty(kind)
penalty：直连 0、Tailscale 直连 2、自有中继 5、DERP 30、WSS 中继 60   【初值】
```

- **升级**：候选路径分数持续 3 s 低于当前路径 15%（且差值 ≥ 3 ms）才切换。
- **故障转移**：当前路径 1 s 无任何入站包，立即切到分数最优的存活路径。静止画面时不编码、QUIC 保活只有 2 s 一次，所以活跃路径靠 250 ms 的 ClockPing 保证入站流量，否则会误判。
- **决策权**：控制端决定，经 QUIC 控制流发 `PathSwitch`；被控端收到后跟随。STUN 的完整性校验只防止路径外的伪造探测，免信令模式下 pwd 甚至是公开默认值，因此 STUN 只用于连通性和打分，**不能触发切换**。
- **探测**：备用路径前 10 s 每秒一次 STUN，之后每 5 s。保活每 15 s。
- **网络变化**：监听系统路由事件（`NotifyIpInterfaceChange`、`NWPathMonitor`），重新收集候选，经 QUIC 控制流 `CandidateUpdate` 交换，不再经信令。
- **切换副作用**：`media-tx` 通知拥塞控制重置到上一速率的 70%，并主动发一个恢复点。

QUIC 的 RTT 估计在切换后短暂失准，只影响可靠流的超时重传，可接受。

### 4.6 中继

```
UDP：[0x7E][ver u8][session_id 8B] payload
→ Bind { session_id, token }     token = 服务器签名 {session, expiry}
```

- 双方绑定同一 `session_id` 后转发；中继不解析 payload。
- 来自中继地址的包在进入分流前先剥头，所以 `0x7E` 不会与 QUIC 冲突。
- 每会话限速，带宽配额可配置。
- **WSS 回退**：同样的帧放进二进制 WebSocket。游戏模式下给出明确警告。

### 4.7 Tailscale 模块

```rust
pub struct TailscaleInfo { pub self_ips: Vec<IpAddr>, pub peers: Vec<TsPeer> }
pub struct TsPeer { pub name: String, pub ips: Vec<IpAddr>, pub online: bool, pub direct: Option<bool> }
pub fn discover_local() -> Vec<IpAddr>;              // 只枚举网卡，永不失败
pub async fn query_localapi() -> Option<TailscaleInfo>; // 失败返回 None
```

- `direct`：`/localapi/v0/status` 中对端有 `CurAddr` 则直连，否则 DERP。
- **免信令模式**：被控端在 tailnet 地址上监听固定 UDP 端口（默认 21116，可配）。控制端以 `IP:端口` 作为唯一候选，跳过信令直接开始检查；此时用 pwd 的预共享值由配对记录派生，首次连接使用公开默认值，身份校验仍由 §9 保证。
- macOS App Store 版的 LocalAPI 使用 TCP + token，取不到就退回 `discover_local`，功能只损失设备列表。

**实现修订（无服务器优先）**，以代码和测试为准：

- **默认不用服务器**：同一局域网直接连接（§4.11 发现），跨网络推荐 Tailscale。Tailscale 自己打洞，打不通时经 DERP 中转，所以不需要自建信令和中继。`nexa-signal` 保留为可选项，App 里收在“高级：自建信令服务器（可选）”下，默认不显示。
- **为什么不改路径打分**：只有配置了自建服务器才会出现自建中继，此时仍按实测打分（§4.5）。自建 UDP 中继扣分（5）小于 DERP（30），因为 DERP 走 TCP，延迟和抗丢包都更差。“推荐 Tailscale”体现在默认配置和界面上，不是让它在测量中占便宜。
- **读取状态**：先试 LocalAPI socket，再试命令行 `status --json`。命令行只用固定路径：`/Applications/Tailscale.app/Contents/MacOS/Tailscale`、`/opt/homebrew/bin/tailscale`、`/usr/local/bin/tailscale`，不查 `PATH`，2 秒超时后结束子进程。命令行输出与 LocalAPI 是同一份文档，因此 App Store 版和官网独立版也能拿到设备列表、DERP 判断和本机名称。
- **本机状态**：`TailscaleInfo` 增加 `self_name`、`self_dns_name`（MagicDNS 名）。控制端事件 `TailnetDevices` 增加 `state`（ABSENT / ADDRESS_ONLY / RUNNING）和本机名称、地址，没有 Tailscale 时也上报一次，界面据此决定显示本机的 Tailscale 名称还是推荐安装。
- **监听所有网卡**：App 默认让被控端监听 `[::]:47800`（双栈），局域网和 Tailscale 同时可连，之后出现的网卡也可连。只监听一个地址收在“监听范围”里。被控端把 `::ffff:a.b.c.d` 形式的对端地址还原为 IPv4，确认窗口和按来源限速（§9.5）都按 IPv4 计。
- **界面**：被控端开启后列出对方可输入的每个地址，Tailscale（优先 MagicDNS 名）在前，局域网其次，设置了自建服务器时最后是设备 ID。连接失败（网络不通、超时、无路径）且目标不是 tailnet 地址时，错误说明附带 Tailscale 建议。菜单栏状态优先显示 Tailscale 名称。

### 4.8 QUIC 配置

| 参数 | 值 | 原因 |
|---|---|---|
| `congestion_controller_factory` | `MediaController`（下述） | 关闭 quinn 拥塞与 pacing |
| `initial_mtu` / `min_mtu` | 1200 | Tailscale MTU 1280，IPv6 最小 UDP 载荷 1232，中继有 12 字节头 |
| `mtu_discovery_config` | `None` | 虚拟路径切换时 QUIC 不知情，更大的 MTU 会黑洞 |
| `datagram_send_buffer_size` | 512 KiB【初值】 | 浅队列 |
| `datagram_receive_buffer_size` | 4 MiB【初值】 | 突发 |
| `max_idle_timeout` | 10 s | 覆盖故障转移 |
| `keep_alive_interval` | 2 s | |
| `ack_frequency_config` | 16 包或 10 ms【初值】 | 高码率下减少 ACK |
| `enable_segmentation_offload` | true | |
| 迁移 | 关闭 | 地址恒定 |

```rust
impl Controller for MediaController {
    fn window(&self) -> u64 { 1 << 40 }   // > u32::MAX
    // 其余回调只统计
}
```

【已核实】quinn 只在 `in_flight + bytes >= window` 时阻塞，且窗口大于 `u32::MAX` 时 pacing 直接放行。

**后果与应对**：quinn 不再限制任何流量，因此：
- 可靠流的批量写入必须走应用层令牌桶（§4.9）。
- `send_datagram` 在缓冲区满时**静默丢弃最旧数据报**【已核实】，`media-tx` 发送前必须查 `datagram_send_buffer_space()`，不足时等待或按背压规则处理。

### 4.9 码率控制

运行在被控端 `media-tx`，输入 TransportFeedback。

**延迟控制器**（GCC 风格）：按帧分组，计算相邻组的到达间隔减发送间隔，线性回归 20 个样本得斜率，与自适应阈值比较，得出 `Overuse / Normal / Underuse`。

| 状态 | 动作【初值】 |
|---|---|
| Normal | 乘性增长 8%/s；接近上次拥塞点时转加性 |
| Overuse | `target = 0.85 × 实测接收速率` |
| Underuse | 保持 |

**丢包控制器**：丢包 > 10% 时 `target ×= (1 − 0.5 × loss)`；< 2% 允许增长。取两者较小值。

**应用受限**：实际发送低于目标 80% 时（如办公静止画面）**禁止上调**，否则估值会虚高。

**启动探测**：用 FEC 校验包作为填充，前 1 s 以 2 倍速率发送，探测带宽且不浪费流量。

**分配**：

```
fec     = clamp(2 × loss_ewma + margin, 0, 0.25)   // margin：游戏 0.05，办公 0.02
audio   = 128 kbps
video   = (target − audio) / (1 + fec) × 0.95
bulk    = max(0, target − audio − 实际视频码率)    // 游戏模式为 0
```

编码器调整节流：至少间隔 100 ms 且变化 ≥ 5%。【已核实】NVENC 有 `NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE` 与 `NvEncReconfigureEncoder`。

**Pacer**：每帧在 `帧字节 × 8 ÷ pace_rate` 内发完，`pace_rate = max(1.5 × target, 0.8 × 链路容量估计)`，且窗口不超过半个帧间隔。链路容量由启动探测和接收速率峰值估计，未知时退回 `1.5 × target`。

M0 实测：若只按编码码率计算，120 fps 下每帧固定摊 4.17 ms，回环帧完成 P50 为 3.98 ms；按 800 Mbps 计算降到 2.39 ms。局域网上线速发送是否引发瞬时丢包，需要 M1 在真实网络上验证。【待验证】

链路容量估计有天然上限：接收速率不会超过实际发送速率，所以局域网上估计值接近每帧突发速率，而不是真实链路带宽。实现取单帧包列的峰值速率，不随时间衰减，只在 Overuse 或路径切换时下调。M1 需要在真实局域网上确认这样是否够用。【待验证】

按 1 ms 粒度分批，Windows 使用高精度可等待定时器。

### 4.10 实现修订

`crates/nexa-signal` 与打洞实现时对本章的修正，以代码和测试为准：

- **中继头为 12 字节**：magic、版本、类型、保留、8 字节会话 ID；Bind 是一种帧类型。§4.6 的示意图少算了 2 字节。
- **新增信令消息** `Lookup/PeerInfo`（封装需要对端 X25519 公钥）和 `Routed`（携带中继授权）。注册签名覆盖 nonce、X25519 公钥和所连服务器名。短 ID 为公钥派生的 9 位数字。
- **路径标识**：STUN 检查增加一个受完整性保护的可选属性，携带应答方对该路径的编号，被控端据此把 `PathSwitch.path_id` 对应到真实路径。
- **被控端**在收到经认证的检查请求后也把该路径视为已验证，否则会丢掉控制端的第一个 QUIC 包；首次 `PathSwitch` 之前，在 QUIC 最后到达的已验证路径上应答。
- **首条已验证路径立即启动 QUIC**，中继有时会先于稍慢的直连胜出，之后升级到直连至少需要 3 秒。
- **暴露面**：默认情况下，任何知道 ID 的人都能让被控端回应候选地址，并向对方指定的地址发送最多 64×4 个检查包。`--paired-only` 和 `--relay-only` 可以收紧，默认值待产品决定。
- **连接可靠性（第二轮已实现）**：断线后控制端保留窗口，以 0.5/1/2/4/8 秒退避在 30 秒内重连，每次重走会合和握手、钉住首次会话的公钥；被控端若授予了不同权限则结束会话。网络变化由 NWPathMonitor 加 2 秒轮询发现，经控制流发 `CandidateUpdate`（**消息编号 24**，与光标消息的 20–23 区分）。UDP 被封时改用信令服务器 `/relay` 上的 WSS 中继。端口映射依次尝试 PCP、NAT-PMP、UPnP（800 ms 内）。读取 Tailscale LocalAPI 区分直连和 DERP，并列出运行 NexaDesk 的设备。会话 socket 为双栈。
- **被控端静默跟随**：活跃路径 1 秒没有 QUIC 时，被控端跟随到另一条**已验证**路径，否则控制端地址变化后会死锁。同一设备重连时不再弹出“接管”提示，访问决策本身不变。
- **仍未实现或有局限**：断线检测受 QUIC 10 秒空闲超时支配；用一次性码重连时若对方已换码会失败；端口映射不续期（2 小时）；UDP 中继与 WSS 中继不能并存；Tailscale 设备列表是启发式探测；App Store 版 Tailscale 的状态改由命令行读取（见 §4.7 实现修订）。mDNS 见 §4.11。
- NAT 测试使用进程内 NAT 模拟器，代替 §14 中的 Linux 网络命名空间。

### 4.11 局域网发现

被控端用 mDNS / DNS-SD 宣告自己，控制端把听到的主机列在连接卡片的“局域网中的 NexaDesk 设备”里（`crates/nexa-net/src/discovery.rs`，`mdns-sd` crate，自带线程，不依赖异步运行时）。

- **服务**：`_nexadesk._udp.local.`，端口为被控端的监听端口。实例名为 `DeviceId`（32 位十六进制），同名的两台机器不会冲突。TXT 记录：`v=1`、`id`、`n`（显示名）、`os`。不用 9 位短 ID：它由信令服务器分配，只在局域网用的主机没有。
- **只是提示**：听到的内容只用来填一个要拨的地址。对方是谁仍由 TLS 和配对判定，和手动输入的地址完全一样。伪造的宣告最多让一次连接失败：首次配对时，一次性码来自真正的主机，SPAKE2 会失败；已配对的设备则钉住了主机公钥。显示名会去掉控制字符和双向文本覆盖字符，并截断。
- **地址**：被控端监听具体 IP 时只宣告那一个地址，监听全部接口时宣告所有接口地址并随接口变化更新。回环地址不宣告。控制端优先拨 IPv4；去掉 IPv6 链路本地地址，因为 `SocketAddr` 带不上接口 scope。
- **本机**：控制端和被控端用不同的设备密钥，无法按指纹排除本机，所以宣告的地址全是本机地址的主机不列出。
- **开关**：`--relay-only` 时不宣告，因为它不能透露直连地址。`--no-lan-discovery` 关闭宣告。App 设置“在局域网中显示本机”默认开启，下次开始共享时生效。控制端每 2 秒把变化的列表推给 UI（事件 `Lan`，编号 43）。
- **macOS**：Info.plist 声明了 `NSBonjourServices`（`_nexadesk._udp`）和本地网络用途说明。真实组播测试（`discovery::tests::a_host_is_heard_and_then_forgotten`）默认忽略：它需要能承载组播的网络，并会触发“本地网络”权限提示。

## 5. 被控端进程

### 5.1 Windows

```
nexa-service（SYSTEM，Windows 服务）
  ├─ 监听 WTS 会话事件，获取 WTSGetActiveConsoleSessionId
  ├─ 取该会话 winlogon.exe 的令牌（SYSTEM，位于用户会话）
  ├─ CreateProcessAsUser → nexa-agent
  └─ agent 崩溃重启；SAS 请求转 SendSAS
```

- agent 以 SYSTEM 运行在用户会话中，才能采集 UAC / 登录桌面、向高完整性窗口注入。
- 桌面切换：`capture-encode` 和 `input-inject` 各自 `OpenInputDesktop` + `SetThreadDesktop`；`AccessLost` 时重绑。
- 不装服务时（便携模式）agent 以当前用户运行，功能降级：不能操作 UAC 和锁屏。
- **风险**：SYSTEM 进程处理网络输入，攻击面大。Rust 内存安全可缓解，第二阶段把网络和解析拆到低权限进程。

### 5.2 macOS

- `LaunchAgent` 每用户运行 agent；无人值守需要 `LaunchDaemon` 配合登录窗口场景（M5）。
- 权限：屏幕录制、辅助功能。必须 Developer ID 签名，否则更新后 TCC 授权失效。

### 5.3 本地 IPC

命名管道 / Unix socket，帧格式同可靠流。

| 方向 | 消息 |
|---|---|
| UI → agent | `GetStatus`、`SetAccessPolicy`、`ListPairings`、`RevokePairing`、`Disconnect`、`RotateCode` |
| agent → UI | `Status`、`IncomingRequest{peer, perms}`、`SessionStarted/Ended` |
| UI → agent | `Decide{req_id, allow, perms}` |

管道 ACL 仅允许交互用户和 SYSTEM；Unix socket 0600 并校验对端 uid。

第一阶段被控端同时只有**一个**活动控制会话，新请求需要用户确认接管。

## 6. 视频

### 6.1 单帧时间线

每帧携带以下时间点，HUD 据此分段展示：

```
capture_ready → convert_done → encode_done → tx_first → tx_last
 → rx_first → rx_complete → decode_start → decode_done → present_call → present_done
```

跨机段使用时钟偏移校正。

### 6.2 分片与重组

**发送端**

```
shard_len = (max_datagram_size − 28) 向下取偶   // 每帧重新取
k = ceil(frame_len / shard_len)
m = 见下
```

| k | m【初值】 |
|---|---|
| ≤ 4 | `fec > 0` 时固定 1 |
| > 4 | `ceil(k × fec)`，上限 25% |

单帧一个 RS 块。最后一个分片补零至 `shard_len`，接收端按 `frame_len` 截断。

**接收端**

```rust
struct FrameSlot {
    id: FrameId, k: u16, m: u16, shard_len: u16,
    got: BitVec, count: u16, buf: Vec<u8>,
    first_rx: Instant, epoch: u8, flags: u8,
}
```

- 固定 16 个槽的环形缓冲，按 `frame_id` 索引，避免逐帧分配。
- 收齐 `k` 个分片（原始或校验）即完成；原始分片已全部收到时跳过解码。
- **丢失判定**：`now > first_rx + deadline`，或后续帧已完成且超过 `reorder_grace`（2 ms【初值】）。
- `deadline`【初值】：游戏 `1.5 × 帧间隔`，办公 `3 × 帧间隔`。

### 6.3 丢帧恢复与 ref_epoch

接收端不能把缺参考的帧喂给解码器，因此需要知道何时恢复。

```
                  ┌───────────── 帧完整且 epoch 匹配 ─────────┐
                  ▼                                          │
              ┌────────┐  判定丢帧      ┌──────────┐         │
              │ Normal │ ─────────────→ │ Waiting  │ ────────┘
              └────────┘ 发 Loss 报告    └──────────┘ 收到 epoch > 等待值的帧
                                         丢弃旧 epoch 的帧
```

被控端处理 Loss 报告：

```
if report.client_epoch != current_epoch: 忽略    // 重复或过期
match encoder.invalidate_from(first_lost):
    RefInvalidated | IntraRefresh | Idr → current_epoch += 1，下一帧置 bit1 恢复点
```

- Waiting 期间每 `max(RTT, 5 ms)` 重发报告，超过 500 ms 升级为 `NeedIdr`。
- **恢复帧本身丢失**：只有带恢复点或 IDR 标记的帧才能结束 Waiting。收到新 epoch 但不是恢复点的帧，说明恢复帧丢了，后续帧都依赖它。此时接收端丢弃该帧、采用新 epoch 并立即重发报告，否则被控端会把旧 epoch 的报告当作过期忽略。集成测试曾因此把缺参考的帧送进解码器。
- `first_lost` 始终是最后解码帧的下一帧，语义是**从该帧起全部失效**。NVENC 需要对范围内每一帧调用 `NvEncInvalidateRefFrames`。
- IDR 限频：最多每秒 1 次【初值】。
- 校验包数量足够时大多数丢包在第一级就被吸收，走不到这里。
- **NACK**：仅当 `RTT + 2 ms < 距 deadline 剩余时间` 才发；重传分片使用新的 `tx_seq`。

**M0 实测（Apple 硬解）**：H.264 丢帧后继续送入后续帧，解码器**不报错，但画面损坏**（PSNR 15 dB，且不会自愈）。因此不能依赖解码错误来发现丢帧，必须由 `ref_epoch` 门控。D3D11VA 的行为待 Windows 验证。【待验证】

### 6.4 Windows 采集

- `IDXGIOutput5::DuplicateOutput1`，超时取 `2 × 帧间隔`。
- `AcquireNextFrame` 后立即 `CopyResource` 到内部纹理池（4 张），然后 `ReleaseFrame`。
- `LastPresentTime == 0` 表示只有鼠标更新，不编码。
- `DXGI_ERROR_ACCESS_LOST`：重建；连续失败回退 GDI（只保活，不追求性能）。
- **多 GPU**：采集和编码必须在驱动该显示器的适配器上；混合显卡笔记本上独显若不驱动屏幕，使用核显 QSV 或跨适配器拷贝。
- BGRA→NV12 用 `ID3D11VideoProcessor`（BT.709，走视频引擎，受游戏 3D 负载影响较小）。4:4:4 模式把 BGRA 直接送编码器。
- 游戏模式：`IDXGIDevice::SetGPUThreadPriority(7)`、`D3DKMTSetProcessSchedulingPriorityClass`，后者需验证权限要求。

### 6.5 编码参数

**NVENC**（【已核实】以下字段和 caps 存在于 API 13.1 头文件）

| 字段 | 游戏 | 办公 |
|---|---|---|
| `tuningInfo` | ULTRA_LOW_LATENCY | LOW_LATENCY |
| preset | P1 | P4 |
| 码控 | CBR | CBR，开 AQ |
| `vbvBufferSize` | `码率 / fps` | `1.5 × 码率 / fps` |
| `gopLength` / `idrPeriod` | INFINITE | INFINITE |
| `frameIntervalP` | 1 | 1 |
| 参考帧数 | 8【初值】 | 4 |
| `enableEncodeAsync` | 0 | 0 |
| 输入 | D3D11 NV12 | NV12，或 ARGB（4:4:4） |

- 恢复：`inputTimeStamp` 直接设为 `frame_id`，`NvEncInvalidateRefFrames(encoder, first_lost)`。头文件建议把参考帧数设大，以便有旧帧可用；无可用参考时编码器自动出帧内帧。
- 不支持 `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION` 时用 `enableIntraRefresh`（要求 INFINITE GOP）。
- 后续优化：`enableSubFrameWrite` + 切片，边编边发。

AMF、QSV 和 Media Foundation 走相同的 `Encoder` trait，恢复能力由 `caps` 声明，不支持 RFI/LTR 的后端由 `invalidate_from` 返回 `IntraRefresh` 或 `Idr`。M0 实现验证机对应的后端，其余在 M4 适配。这些后端的接口细节尚未对照 SDK 核实，接入前需确认。【待验证】

**VideoToolbox**（【已核实】SDK 头文件）

所有模式使用同一组配置（M0 在 Apple M6 上实测）：

| 属性 | 值 | 说明 |
|---|---|---|
| `EnableLowLatencyRateControl`（编码器规格） | 开 | H.264、HEVC 均可用；强制无限 GOP |
| `RealTime` | true | |
| `AllowFrameReordering` | false | |
| `AverageBitRate` | 目标码率 | |
| `EnableLTR` | 开 | **仅低延迟模式支持**，非低延迟模式返回 -12900 |
| 编码格式 | HEVC 优先，H.264 兜底 | 无 AV1 硬编 |

不要设置 `MaxFrameDelayCount`（所有模式都被拒绝）；低延迟模式也拒绝 `PrioritizeEncodingSpeedOverQuality`。

- LTR 流程：回调收到 `RequireLTRAcknowledgementToken` → 控制端确认该帧（`FrameAck`）→ 编码时传 `AcknowledgedLTRTokens`；丢帧时用 `ForceLTRRefresh`。实测丢 3 帧后无 IDR，恢复帧约 1.2–1.3 倍均值，而强制 IDR 为 56 倍。
- **无硬件 4:4:4**：输入 4:4:4 仍输出 `chroma_format_idc=1`，SDK 也没有对应档次。Mac 被控端办公模式只有 4:2:0，靠 §6.8 静止精修补偿；第二阶段可考虑 AVC444 式双流。

macOS 采集：`SCStreamConfiguration` 的 `queueDepth = 3`（默认 8，深队列增加延迟）、`minimumFrameInterval = 1/fps`、`pixelFormat` 用 `420v`（4:4:4 模式用 BGRA）、`showsCursor` 办公 NO / 游戏 YES；过滤掉 `SCFrameStatus` 非 `complete` 的帧。

### 6.6 解码与呈现

| | Windows | macOS |
|---|---|---|
| 解码 | D3D11VA → NV12 纹理数组 | VTDecompressionSession → CVPixelBuffer（IOSurface） |
| 绑定 | SRV | `CVMetalTextureCache` |
| 上屏 | NV12→RGB 着色器，Flip-Discard，缓冲 2–3 | Metal 着色器，`maximumDrawableCount = 2` |
| 限帧 | `FRAME_LATENCY_WAITABLE`，`SetMaximumFrameLatency(1)` | 信号量 |
| 办公 | `Present(1, 0)` | `displaySyncEnabled = YES` |
| 游戏 | `Present(0, ALLOW_TEARING)` | `displaySyncEnabled = NO` |

**4:4:4 解码是主要限制**：D3D11VA 是否暴露 HEVC 4:4:4 取决于 GPU 和驱动，须实测创建；不可用时协商自动降到 4:2:0。

macOS 解码输出不指定像素格式时，Apple Silicon 返回 GPU 压缩缓冲，效率高但 CPU 不能读取。正常显示路径保持默认，只有需要 CPU 读取时才指定 `420v`。

### 6.7 模式切换与分辨率变化

| 事件 | 动作 |
|---|---|
| 切换办公/游戏 | 控制流下发 `StreamConfig`；能热改的（码率、帧率）热改，否则重建编码器 + IDR |
| 控制端缩放窗口 | 默认只缩放显示；停止调整 500 ms 后可选择协商新分辨率 |
| 被控端分辨率变化 | `ModeChanged` → 重建编码器 → 新 `StreamConfig` → IDR |
| 切换显示器 | 停旧流、起新流；游戏模式仅单流 |

### 6.8 静止画面精修（办公模式）

```
最后一次内容变化后：
  +100 ms → 重编当前帧，QP 上限 28
  +300 ms → 24
  +600 ms → 20
  +1000 ms → 16，然后停止          【初值】
```

重复帧使用同一纹理，通过重配置编码器的 QP 上限实现，画面不变，所以只有细节增量，码流有限。任何新内容立刻回到正常码控。各编码器能否不出 IDR 地动态调整 QP，在 M3 实测。【待验证】

### 6.9 光标

- 被控端跟踪光标形状（Windows `GetCursorInfo`，macOS `NSCursor.currentSystemCursor`），按内容哈希去重，经 `CursorShape` / `CursorSet` 发送。
- 办公模式：控制端设置 OS 光标（`SetCursor` / `NSCursor`），鼠标移动零延迟。
- 被控程序主动移动光标时才反向同步位置。
- 游戏模式：光标画在视频里。

### 6.10 实现修订

光标、游戏模式、音频和多显示器实现时对本章的修正，以代码和测试为准：

- **`tx_seq` 在发送时分配**（视频和音频共用计数器），而不是分片时；否则仍在 pacing 中的分片会被反馈判为丢失。
- **AudioPacket**（0x02）：16 字节头，`tx_seq` 与 VideoShard 同偏移，布局见 `nexa-wire/src/datagram.rs`。Opus 走系统 AudioToolbox（无 libopus），64 kbps，10 ms 帧；接收端自适应抖动缓冲 2–10 帧。实测采集到播放约 47 ms（无丢包），其中缓冲约 28 ms；AudioToolbox 的 Opus 没有丢包隐藏。音频权限默认关闭。
- **新增控制消息** `CursorShape`、`CursorSet`、`CursorMoved`、`AudioControl`；`StreamRequest` 增加 `display`，`SessionAccept` 增加 `host_name`。
- **光标（§6.9）**：只有在控制端负责绘制光标时（办公/均衡模式、有输入权限）才从视频中去掉光标；**只读会话和游戏模式保留在视频里**。被控端本人移动鼠标也会同步到控制端，两人同时操作时指针会跳。
- **模式切换（§6.7）**：重建限频期间的请求会延后执行，不再丢弃；切换显示器失败时回退到原显示器。
- **静止精修（§6.8）**：`MaxAllowedFrameQP` 能在低延迟会话中实时生效且不出 IDR，但 H.264 上无法干净地恢复（恢复后 10 帧中丢 5–7 帧），因此**未启用**。临时把 `AverageBitRate` 提高 2–16 倍可将 PSNR 从 38.4 提到 44.3 dB，且无 IDR、无丢帧，是后续可采用的替代方案，代价是精修帧在慢速链路上形成突发。
- **游戏模式锁定（§7.4）**：宿主键释放锁定后，需由界面重新开启捕获才会再次锁定。

## 7. 输入

### 7.1 HID 与表格

线上 `hid = (page << 16) | usage`。修饰键遵循 HID（0xE0–0xE7）。

平台表在构建时由一份 CSV 生成，列为 `hid, win_scancode, win_extended, mac_keycode, linux_evdev, name`。JIS 键：`Intl1 (Ro) 0x87`、`Intl2 0x88`、`Intl3 (Yen) 0x89`、`Intl4 (変換) 0x8A`、`Intl5 (無変換) 0x8B`、`LANG1 (かな) 0x90`、`LANG2 (英数) 0x91`。

### 7.2 改写器

**规则文件**（`keymap.toml`，内置默认值，用户覆盖）

```toml
[[profile]]
from = "macos"
to = "windows"
deferred = ["LGui", "RGui", "LAlt", "RAlt"]   # 延迟决议的修饰键

[[profile.rule]]
when = "Cmd+Shift+Z"
then = "Ctrl+Y"

[[profile.rule]]
when = "Cmd+Tab"
then = "Alt+Tab"
hold = "Alt"          # Cmd 按住期间保持 Alt，支持连按 Tab

[[profile.rule]]
when = "Cmd+Backspace"
then = ["Shift+Home", "Delete"]

[[profile.rule]]
when = "Cmd+V"
then = "Ctrl+V"
tag = "paste"

[[profile.rule]]            # 通用规则，优先级最低
when = "Cmd+*"
then = "Ctrl+*"

[[profile]]
from = "windows"
to = "macos"

[[profile.rule]]
ctx = "terminal"
when = "Ctrl+C"
then = "Ctrl+C"

[[profile.rule]]
ctx = "terminal"
when = "Ctrl+Shift+V"
then = "Cmd+V"
tag = "paste"
```

**状态**

```rust
struct Rewriter {
    profile: Compiled,
    held_deferred: ModSet,                       // 按下但尚未决议
    used_as_chord: ModSet,                       // 期间触发过组合键
    phys_mods: ModSet,                           // 物理按下的全部修饰键
    active: HashMap<Hid, Active>,                // 物理键 → 实际效果
    out_mods: ModSet,                            // 远端当前按下的修饰键
    out_refs: HashMap<Hid, u32>,                 // 输出键引用计数
    hold: Option<(Hid, Hid)>,                    // (触发键, 被保持的输出修饰)
}
struct Active { emitted: SmallVec<[Hid; 2]>, mods_for_key: ModSet }
```

**按下**

```
on_down(k, repeat):
  if repeat: 按 active[k] 重发 down(repeat=true)，不重新匹配
  if k ∈ deferred: held_deferred += k; phys_mods += k; return
  if k 是其他修饰键: phys_mods += k; out_down(k); return
  rule = match(ctx, held_deferred ∪ phys_mods, k)  // 优先级：ctx 精确 > 无 ctx 精确 > 通配
  output = rule.then 或 透传
  used_as_chord += held_deferred
  差分修饰：out_mods → output.mods（按需抬起/按下）
  out_down(output.key); active[k] = ...
  if rule.tag == paste: 产出 PasteMarker
  if rule.hold: hold = (触发键, 输出修饰)
```

**抬起**

```
on_up(k):
  if k ∈ deferred:
    phys_mods -= k
    if k ∉ used_as_chord: 发一次完整点按映射（Cmd → Win）
    held_deferred -= k; used_as_chord -= k
    if hold 的触发键 == k: out_up(被保持的修饰); hold = None
    return
  if k 是其他修饰键: phys_mods -= k; out_up(k); return
  按 active[k] 释放；移除；恢复修饰到“物理键隐含的状态”
```

**序列规则**（`then` 为数组）按下时依次点按，放进同一发送批次，抬起时无操作。

**点击**：`on_button` 按当前 `held_deferred` 查 `Cmd+Click → Ctrl+Click` 类规则，先差分修饰再发按键。

**不变式（用属性测试验证）**：任意按键序列结束后，所有物理键抬起 ⇒ `out_refs` 为空且 `out_mods` 为空。

### 7.3 FocusContext

| 平台 | 机制 |
|---|---|
| Windows | `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` → 进程名 |
| macOS | `NSWorkspace.didActivateApplicationNotification` → bundle id |

分类清单内置：Windows Terminal、conhost、PowerShell、mintty、Alacritty、WezTerm；Terminal.app、iTerm2、kitty、Ghostty、Alacritty、WezTerm。VS Code 内置终端无法从进程识别，默认不算，用户可把应用加入清单。

### 7.4 本地捕获

| | Windows | macOS |
|---|---|---|
| 键盘 | `WH_KEYBOARD_LL`（仅会话激活时安装） | `CGEventTap`（需辅助功能） |
| 鼠标（办公） | 窗口消息 | 视图事件 |
| 鼠标（游戏） | Raw Input + 光标裁剪 | `CGAssociateMouseAndMouseCursorPosition(false)` + 增量字段 |
| 无权限时 | — | `NSEvent` 本地监听，无法拦截 Cmd+Tab，并提示用户 |

是否捕获由 C# 通过 `nexa_session_set_input_active(bool)` 决定：窗口失活、工具条激活时关闭，并调用 `Rewriter::reset`。

**宿主键**默认 `Ctrl+Alt+Shift+Q`（Windows）/ `Ctrl+Option+Cmd+G`（macOS；不用 Q，`Ctrl+Cmd+Q` 是系统锁屏快捷键），可配置，用于退出游戏模式捕获和唤出工具条，且永不转发。

### 7.5 注入

| | Windows | macOS |
|---|---|---|
| 键 | `SendInput` + `KEYEVENTF_SCANCODE` | `CGEventCreateKeyboardEvent`，显式设置 flags |
| 绝对移动 | `MOUSEEVENTF_ABSOLUTE \| VIRTUALDESK` | `kCGEventMouseMoved` |
| 相对移动 | `MOUSEEVENTF_MOVE` | 设置 `kCGMouseEventDeltaX/Y` |
| 文本 | `KEYEVENTF_UNICODE` | `CGEventKeyboardSetUnicodeString` |

**防粘键对账**：每 1 s 及按下集合变化时发 `KeyStateSync`；被控端释放不在集合中的键。连接断开或 3 s 收不到同步（有键按下时）则全部释放。

### 7.6 布局与输入法

- **物理模式**（默认）：两端键盘布局相同时最自然。
- **字符模式**：布局不同时建议开启。无 Ctrl/Alt/Cmd 的可打印键发 `TextInput`，依赖 `ToUnicodeEx` / `UCKeyTranslate` 解析。
- **本地输入法**：组字在本地，提交后发 `TextInput`。
- **IME 键映射**：`英数 ↔ 無変換`（IME 关），`かな ↔ 変換`（IME 开），作为可选规则组。

### 7.7 实现修订

`crates/nexa-input` 实现时对上文的修正，以代码和测试为准：

- **PasteMarker 排在同批事件最前**，否则闸门扣不住 V 键本身（§8.3）。
- **修饰键由统一的目标状态差分得出**，替代伪代码中分散的 `out_down/out_up`。伪代码有三处缺陷：Shift 先于 Z 松开时会释放未按下的键；非延迟修饰键的 `hold` 不会释放（Windows 控制 Mac 的 Alt+Tab 会粘住）；repeat 不恢复修饰键。
- **`*` 通配范围**：产生字符的键和 Enter，不含 Tab、Esc、Space、Backspace、Delete、导航键和 F 键。否则 Ctrl+Tab 会变成 Cmd+Tab，Ctrl+F5 会变成 Cmd+F5（VoiceOver）。
- **同系统会话**：不改写任何键，但标记粘贴组合键（Mac 的 Cmd+V，Windows 的 Ctrl+V、Shift+Insert），否则闸门不生效。
- **滚轮**：增加 `on_wheel`。默认规则保持 Ctrl 直到 Cmd 松开；第一格滚动仍可能先于 Ctrl 到达。
- **macOS 平台层约定**：线上 `Pointer` 绝对坐标是像素，CoreGraphics 用点，组装层负责换算；滚轮正值为上/右；按键 0 左、1 右、2 起其他；宿主键的修饰键在识别前已发出，因此触发时补发释放。
- **新增默认规则**：Windows 控制 Mac 的 `Ctrl+V` 粘贴标记、终端内 `Ctrl+*` 保持、Cmd/Option+Shift+方向键的选择映射。

## 8. 剪贴板

### 8.1 状态

```rust
struct ClipSync {
    lamport: u64,
    local:  Option<LocalEntry>,   // 本端最新
    remote: Option<RemoteEntry>,  // 对端最新
    acked:  Gen,                  // 对端已确认的本端代际
    self_hashes: LruSet<Hash>,    // 防回环
}
type Gen = (u64 /*lamport*/, DeviceId);
```

### 8.2 事件处理

| 事件 | 动作 |
|---|---|
| 系统剪贴板变化 | 有 owner 标记或哈希命中 `self_hashes` → 忽略；策略判定（敏感、方向开关）；否则 `lamport += 1`，发 `ClipAnnounce` |
| 收到 `ClipAnnounce(g)` | `g <= 当前最新` → 丢弃；否则写系统剪贴板（内联内容直接写，其余写承诺），记录哈希，回 `ClipApplied(g, Ok)` |
| 收到 `ClipApplied` | 更新 `acked`，通知闸门 |
| 系统请求延迟格式 | `clipboard` 线程阻塞，发 `ClipFetch`，等 `ClipData`；超时 10 s 写空 |
| 收到 `ClipFetch` | 校验 `gen` 仍最新，否则返回 `Stale`；从本地读取，分块发送 |
| 重连 | 双方重新 Announce 当前内容 |

`lamport` 收到对端更大的值时跟进。

### 8.3 粘贴闸门

位于 `input-capture` → 发送之间的 `InputGate`：

```rust
enum Gate { Open, Holding { until: Gen, deadline: Instant, buf: VecDeque<Output> } }
```

- 改写器产出 `PasteMarker` 时，若 `acked < local.gen`，转入 `Holding`，随后所有输入事件（含 V 键本身）缓冲，**保持顺序**。
- `ClipApplied(gen ≥ until)` 或到达 deadline（800 ms【初值】）时放行；超时在工具条提示。
- 指针事件不进闸门；但 `Holding` 期间的按钮事件要进入，维持与键盘的相对顺序。
- 闸门命中率、等待时长写入 HUD。

### 8.4 平台要点

**Windows**
- 专用线程 + 消息窗口。`SetClipboardData(fmt, NULL)` 承诺，`WM_RENDERFORMAT` 兑现，`WM_RENDERALLFORMATS` 在退出前处理。
- `OpenClipboard` 失败时退避 10/20/40…ms，累计 1 s，失败回 `ClipApplied(Busy)`。

**macOS**
- 无变化通知，轮询 `changeCount`，100 ms 一次。
- `NSPasteboardItem` + `NSPasteboardItemDataProvider` 实现延迟。

**格式映射**

| 规范 | Windows | macOS |
|---|---|---|
| `text` (UTF-8) | `CF_UNICODETEXT` | `public.utf8-plain-text` |
| `html` | `HTML Format`（处理偏移头） | `public.html` |
| `rtf` | `Rich Text Format` | `public.rtf` |
| `png` | `PNG`、`CF_DIBV5` | `public.png`、`public.tiff` |
| owner 标记 | `NexaDesk.Owner` | `app.nexadesk.owner` |

文本换行按目标系统规范化（CRLF ↔ LF）。

**限制**：单格式默认上限 32 MiB，超出回 `TooLarge` 并提示用文件传输。内联阈值 64 KiB。

### 8.5 键入式粘贴

工具条触发，把本机剪贴板中的文本作为 `TextInput` 分批发送，默认 200 字符/秒，节奏可调。发送前弹窗显示字符数，避免误发大段文本。

### 8.6 实现修订

`crates/nexa-clip` 实现时对上文的修正，以代码和测试为准：

- **字段名**：`gen` 是 edition 2024 保留字，代码中为 `generation`。
- **`ClipData` 增加 `status`**：否则 `ClipFetch` 无法返回 `Stale`。`ClipApplied` 与之共用状态枚举：Ok、Busy、TooLarge、Stale、Denied、Invalid。
- **闸门条件**：不是简单的 `acked < local.gen`，还要求本地条目仍是最新、未被作废且会话已连接，否则同时复制落败后每次粘贴都会空等 800 ms。任何状态的 `ClipApplied` 都放行闸门，但只有 Ok 推进 `acked`。
- **防回环哈希**：每项只抵消一次回环，5 秒过期，否则“复制 X、Y、X”时第二个 X 会被吞掉。哈希基于规范化内容（LF、无 CF_HTML 头）。
- **隐私**：被策略过滤的复制也会作废当前条目，防止对端用旧代际 `ClipFetch` 读到新的敏感内容；读出的数据还要与公告的哈希比对。
- **`ClipApplied` 时机**：系统剪贴板写入完成后才发送，因此 Busy 能如实上报。
- **同代际的 Announce** 重新确认，更旧的回 `Stale`，不再静默丢弃，否则发送端会空等。
- **重连**：只有最新条目的来源方重新公告，并沿用原代际。
- **macOS 承诺数据**：data provider 回调大概率在主线程。控制端因此先下载再写入真实数据，不使用承诺，代价是本机剪贴板要等下载完成；被控端主线程没有 UI，保留延迟承诺，闸门不受影响。
- **Lamport 持久化**：进程重启后由组装层用持久化值或墙钟初始化，否则重启方的新复制会被判旧。

## 9. 安全

### 9.1 身份与存储

| 项 | 内容 |
|---|---|
| 设备身份 | Ed25519，首次启动生成 |
| 私钥存储 | Windows：DPAPI；macOS：Keychain |
| 配对库 | `pairings.json`：`pubkey、name、perms、added_at、last_seen`，原子写，HMAC 防篡改 |
| TLS 证书 | rcgen 自签，由设备密钥派生 |

### 9.2 认证流程

```
1. TLS 1.3 双向认证。自定义 verifier 只检查证书形态和签名，提取对端公钥。
   已配对设备：控制端另外校验公钥与配对记录一致。
2. Control 流：ClientHello{auth}
3a. Known：被控端查配对库，公钥匹配 → 跳到 4。
3b. Code / Password：
    salt ← 被控端
    pw = Code：一次性码；Password：Argon2id(password, salt)
    双方运行 SPAKE2(pw, 身份 = 两端公钥)
    K = HKDF(spake_key, exporter("EXPORTER-nexa-pair", 32))
    互发 HMAC(K, transcript) 确认
4. 策略判定（§9.3）
5. SessionAccept
```

绑定 TLS exporter 后，即使信令和中继恶意替换公钥，SPAKE2 确认也会失败（集成测试已验证，含中间人持有双方真实设备密钥的情形）。

**一次性码**：8 位 base32（约 40 比特），5 次失败后轮换。**口令**：被控端只存 Argon2id 输出。

### 9.3 访问决策

| 情况 | 动作 |
|---|---|
| 已配对 + 该设备允许无人值守 | 直接允许 |
| 已配对 | 弹窗确认，可勾选“下次无需确认” |
| 一次性码正确 | 弹窗确认，可选保存配对 |
| 无人值守口令正确 | 按策略，默认仍保存为配对 |
| 其余 | 拒绝 |

被控端在会话期间常驻显示连接方和权限，并能一键断开。此提示**不可通过配置关闭**。

### 9.4 威胁

| 威胁 | 应对 |
|---|---|
| 信令/中继中间人 | SPAKE2 + exporter 绑定；配对公钥钉扎 |
| 暴力猜码 | SPAKE2 只能在线尝试；失败轮换；信令限速 |
| 通过 ID 获取 IP | “仅已配对可发起”与“仅中继”选项；信令限速 |
| 伪造打洞包切换路径 | 路径切换只经认证的 QUIC 流；QUIC 包只接受已验证路径 |
| 恶意剪贴板（巨型/畸形） | 大小上限；解析在受限代码路径 |
| 本机其他进程调用 IPC | ACL 与对端身份校验 |
| 日志泄露 | **禁止记录按键和剪贴板内容**，诊断包只含元数据 |

### 9.5 实现修订

`crates/nexa-auth` 实现时对上文的修正，以代码和测试为准：

- **未配对设备用口令连接时始终需要被控端确认**，因此无人值守的机器无法只靠口令完成首次配对。这是一个待定的产品决定。
- 消息顺序：被控端发 salt 和 SPAKE → 控制端发 SPAKE 和确认 → 被控端发确认。transcript 含方法、salt、双方公钥和双方 SPAKE 消息，并区分角色。
- 口令失败 5 次后锁定 30 秒，翻倍至 1 小时。**锁定按来源计算**，见下文“按来源限制猜测”。Argon2 参数固定为 64 MiB、t=3、p=1，修改需要升协议版本。
- **按来源限制猜测**（`nexa_auth::secrets::Source`）：来源是 IPv4 地址，或 IPv6 地址所在的 /64（通常就是一个用户分到的网段）。
  - **一次性码**：同一来源最多用掉一个码 5 次机会中的 3 次。单个来源既不能靠失败让码作废，也不能把机会用光，让真正拿到码的人无法使用。换码后每个来源的额度恢复。
  - **口令**：每个来源单独锁定（5 次失败）。所有来源合计失败 25 次（`PASSWORD_FAILURES_GLOBAL`，从上次成功起算）后，所有人一起锁定，时长同样从 30 秒起翻倍。这样能约束分散在多个地址上的猜测。
  - **握手次数**：此外每个来源每分钟最多发起 8 次配对握手（`PairLimiter`），与所用的秘密无关。
  - **经信令服务器来的连接**：信令服务器在 `Incoming.from_ip`（字段 7）里附上它看到的控制端地址，被控端按这个地址计数。否则所有经同一中继来的控制端会共用一份额度。服务器不可信，但它谎报地址最多把额度分错；它本来就能拒绝转发。为此 TLS 必须由 nexa-signal 自己终止，不能放在 HTTP 反向代理后面（docs/DEPLOY.md）。
- `DeviceId` 为公钥的域分离 SHA-256 前 128 位，不用于授权。配对记录增加 `unattended`，新配对默认只有查看权限。
- **已知局限**：被控端保存的 Argon2 输出等同口令，可用于登录该主机，后续考虑增强型 PAKE；`pairings.json` 回滚会撤销吊销；掌握 2 个以上来源（例如两个 IPv4 地址）的攻击者仍能让一次性码作废，掌握 5 个以上来源的攻击者仍能把口令登录对所有人锁住，这两种情况都只影响可用性；`spake2` crate 发布前需要外部安全评审。

## 10. 控制端 C ABI 与 Avalonia

### 10.1 C ABI

```c
typedef struct NexaClient NexaClient;
typedef struct NexaSession NexaSession;
typedef void (*nexa_event_cb)(void *user, const uint8_t *pb, size_t len);

int32_t nexa_init(const uint8_t *cfg, size_t len, nexa_event_cb cb, void *user, NexaClient **out);
void    nexa_shutdown(NexaClient *c);

int32_t nexa_connect(NexaClient *c, const uint8_t *req, size_t len, NexaSession **out);
void    nexa_session_close(NexaSession *s);

/* macOS 必须在主线程调用；内部完成 CAMetalLayer 绑定 */
int32_t nexa_session_attach_view(NexaSession *s, void *handle, uint32_t kind,
                                 uint32_t w_px, uint32_t h_px, float scale);
int32_t nexa_session_resize_view(NexaSession *s, uint32_t w_px, uint32_t h_px, float scale);
int32_t nexa_session_detach_view(NexaSession *s);

int32_t nexa_session_set_input_active(NexaSession *s, bool active);
int32_t nexa_session_command(NexaSession *s, const uint8_t *cmd, size_t len);
int32_t nexa_last_error(uint8_t *buf, size_t cap, size_t *written);
```

规则：
- 除 `attach_view` 外均线程安全，且都不阻塞（耗时操作投递到 Rust 线程）。
- Rust 内部任何 panic 在 FFI 边界捕获，转为错误码，绝不跨越 ABI。
- 回调运行在 Rust 线程上：C# 必须先复制字节，再 `Dispatcher.UIThread.Post`。
- `[UnmanagedCallersOnly]` 静态函数 + `GCHandle` 传 `user`，兼容 NativeAOT。

### 10.2 VideoHost 控件

【已核实】`NativeControlHost.CreateNativeControlCore` 可重写；默认创建的 Windows 子窗口是 `WS_CHILD`，macOS 句柄描述符为 `"NSView"`。

```csharp
public sealed class VideoHost : NativeControlHost
{
    protected override IPlatformHandle CreateNativeControlCore(IPlatformHandle parent)
    {
        var h = base.CreateNativeControlCore(parent);       // 复用默认子窗口
        var (w, h2, s) = CurrentPixelSize();
        Native.nexa_session_attach_view(_session, h.Handle, KindOf(h), w, h2, s);
        return h;
    }

    protected override void DestroyNativeControlCore(IPlatformHandle control)
    {
        Native.nexa_session_detach_view(_session);
        base.DestroyNativeControlCore(control);
    }
    // SizeChanged / 缩放变化 → nexa_session_resize_view
}
```

注意：
- 【已核实】控件离开视觉树时子窗口被重新挂到离屏父窗口，因此 `Detach` 后交换链可保留，不必重建。
- Windows 上若使用分层窗口，宿主会走 `_useLayeredWindow` 路径。会话窗口**不要设置透明**。
- 【已核实】像素尺寸由布局四舍五入，C# 传给 Rust 的尺寸必须与 `RenderScaling` 一致。

### 10.3 窗口结构

```
SessionWindow（无透明）
 └─ VideoHost（填满）

ToolbarWindow（Owner = SessionWindow，无边框，Topmost，跟随位置）
HudWindow（同上，鼠标穿透）
```

- `OverlayPopups = false`，让工具条里的菜单也成为独立窗口。
- macOS 全屏（独立 Space）下，owned 窗口是否跟随需要 S1 验证。【待验证】

### 10.4 工程

- MVVM：`CommunityToolkit.Mvvm` 源生成器；全部 `x:CompileBindings="True"`，满足裁剪。
- 打包：`PublishAot=true`；`libnexa_client` 作为原生依赖一起发布。
- 页面：`DevicesView`、`SessionWindow`、`KeyboardSettingsView`、`DiagnosticsView`、`ConsentDialog`。

### 10.5 多语言

界面支持简体中文、English、日本語，以代码和测试为准：

- **文案存放**：`ui/NexaDesk.App/Localization/Strings/<语言>/<区域>.json`，每个文件是一个扁平的 `"区域.名称": "文本"` 对象，作为内嵌资源编进程序集。不用 resx 卫星程序集，NativeAOT 下无需额外处理。C# 用 `L.Get` / `L.Format`（固定文化格式化），XAML 用 `{l:T 键名}`，也可以放在 `StringFormat` 里。
- **语言选择**：启动时决定一次。优先用设置里的语言；没有设置时，取 macOS 首选语言列表（CoreFoundation `CFLocaleCopyPreferredLanguages`，因为程序开启了固定文化全球化）中第一个受支持的语言。繁体中文也用简体中文，都不支持时用英文。主窗口右上角可切换，重启后生效，不做运行时切换。
- **缺失兜底**：当前语言缺键时依次退回英文、中文，最后显示键名本身。格式字符串损坏时显示原文，不抛异常。
- **字体**：中日文共用的汉字码位在两种字体里字形不同，日文界面把 Hiragino Sans 放在最前，中英文保持 PingFang SC。
- **核心发来的消息**：Rust 端和被控端发给界面的提示带“消息代码”，界面按代码选择当前语言的措辞，原有的文本字段改为英文兜底：
  - `Warning`：增加 `code` 和 `detail`；
  - `ClipboardNotice`：增加 `code`、`kind`、`size` 和 `detail`；
  - `KeymapView`：增加 `file_error`；
  - 被控端 `Notice`：增加 `remote` 和 `reason`。
  
  错误原因等可变部分（来自系统或网络库）按原文显示。
- **系统权限提示**：macOS 的屏幕录制、辅助功能、本地网络权限说明来自应用包里各语言的 `InfoPlist.strings`（源文件在 `Localization/InfoPlist/`，打包脚本复制），`Info.plist` 本身为英文。
- **测试**：`LocalizationTests` 检查以下各项：
  - 三种语言的键完全一致，占位符一致；
  - 代码和 XAML 用到的键都存在；
  - 视图和视图模型里除注释外不残留中日文；
  - 首选语言选择；
  - 设置的保存；
  - 消息代码的措辞；
  - 三种语言的界面渲染。

## 11. 配置与持久化

| 文件 | 位置 | 内容 |
|---|---|---|
| `config.toml` | 用户配置目录 | 服务器地址、默认模式、方向开关 |
| `keymap.toml` | 用户配置目录 | 用户规则 |
| `pairings.json` | Windows：`%ProgramData%\NexaDesk`（ACL）；macOS：`~/Library/Application Support/NexaDesk` | 配对 |
| `caps.json` | 缓存目录 | 编解码能力 |
| `logs/` | 缓存目录 | 轮转 |

所有写入采用“写临时文件 + 原子改名”。

### 11.1 第三轮实现补充

以代码和测试为准：

- **文件传输**（流类型 4）：每个文件一条双向流，只能由持有文件的一方发起，没有任何列出或索取对方文件的消息。消息顺序 `FileOffer → FileAnswer → FileChunk(16 KiB) → FileEnd(sha256) → FileResult`；流终止码 1 取消、2 拒绝、3 无效。接收方先写隐藏的 `.nexadesk-part` 临时文件，SHA-256 校验通过后用硬链接放到 `~/Downloads/NexaDesk`，永不覆盖，并加 `com.apple.quarantine` 属性。单文件上限 8 GiB，超过 256 MiB 需接收方确认。新增 `file` 权限，确认弹窗中默认不勾选，一个权限覆盖双向（是否需要控制端单独同意接收，待产品决定）。
- **批量限速（§4.9 修订）**：剪贴板与文件共用每会话一个令牌桶。被控端取码率控制器的 `bulk_bps`；控制端上行没有传输反馈，改用基于 RTT 的 LEDBAT 式让路算法。保留 1 Mbit/s 下限，避免游戏模式下传输完全停滞。写入块降为 16 KiB，大块突发会被误判为拥塞。
- **后台运行**：菜单栏常驻，关闭主窗口不退出；“登录时启动”使用 `SMAppService.mainApp`。登录启动不会自动开始托管，无人值守规则不变。
- **键位设置**：用户 `keymap.toml` 支持 `disable` 关闭内置规则、`enabled = false` 暂停自定义规则。C ABI 新增 `nexa_keymap_rules`、`nexa_keymap_save`、`nexa_keymap_preview`，预览运行真实改写器但不发送任何事件。
- **延迟分段（§6.1、§12）**：控制端统计“被控端+网络、接收、解码排队、解码、呈现排队、呈现、合计”的 P50/P99；被控端时钟应答改用与 `capture_ts_us` 相同的时基。HUD 为置顶、鼠标穿透的小窗。
- **能力缓存**：`~/Library/Caches/NexaDesk/caps.json`，按系统版本、GPU 名称和探测版本区分。

## 12. 可观测性

| 指标 | 来源 |
|---|---|
| 分段延迟（§6.1）P50/P99 | 帧时间线 |
| 路径类型、RTT、丢包 | 路径管理 |
| 目标码率、实际码率、FEC 比例 | 码率控制 |
| FEC 恢复数、RFI 数、IDR 数 | 恢复阶梯 |
| 编码跳帧、解码积压 | 背压 |
| 闸门等待次数/时长 | InputGate |

- 日志：`tracing`，按会话 ID 关联，轮转。
- 协议调试：可开关的 qlog。
- 诊断包：用户手动导出，内容不含按键与剪贴板数据。

## 13. 错误处理

| 场景 | 行为 |
|---|---|
| 编码器失败 | 依次回退：同后端重建 → 下一个硬编 → 软编；通知 HUD |
| 解码器失败 | 请求 IDR；连续失败则重新协商编解码 |
| 交换链丢失（设备移除） | 重建 D3D 设备，解码器重置，请求 IDR |
| 采集 `AccessLost` | 重绑桌面，重建采集，请求 IDR |
| QUIC 断开 | 进入 Reconnecting，保留窗口，释放远端按键 |
| 被控端退出会话 | 正常关闭帧 + 原因 |

## 14. 测试策略

| 层 | 方法 |
|---|---|
| 改写器 | 金标表 + 属性测试（§7.2 不变式） |
| 剪贴板状态机 | 确定性模拟，乱序、丢失、同时复制 |
| FEC / 重组 | 随机丢包、乱序的模糊测试 |
| 码率控制 | 网络模拟器回放，验证收敛和突变响应 |
| 协议解析 | `cargo-fuzz`，覆盖所有数据报与消息解码 |
| 弱网 | macOS `dnctl`/`pfctl`，Linux `tc netem` |
| NAT | Linux 网络命名空间 + iptables，覆盖易/难 NAT 组合 |
| 延迟真值 | 240 fps 摄像机拍两台屏幕；或点击后变色的测试窗口，用 HUD 交叉校验 |
| 粘贴 | 自动脚本：写本地剪贴板后立即发粘贴键，对比远端内容，1000 次 |

## 15. M0 验证计划

代码放在 `spikes/`，不进入正式 crate。每项都有明确的通过标准和退路。macOS 部分的结果见 [spikes/M0-report.md](spikes/M0-report.md)。

**环境前提**
- 本机 macOS 可完成 S1（macOS 部分）、S2 全部、S3（macOS 部分）。
- **S1、S3 的 Windows 部分需要一台 Windows 机器，显卡不限**（NVIDIA、AMD、Intel 均可），编码后端按显卡选择，见 S3。
- 需先安装 `rustup`。

### S1 呈现（`spikes/s1-present`）

Avalonia 窗口 + Rust cdylib，Rust 线程在内嵌视图上以 120 Hz 绘制带帧计数的移动条。

| 检查 | 通过标准 |
|---|---|
| UI 线程人为卡 500 ms | 视频不停顿 |
| Windows 窗口 / 无边框全屏 | 观察到撕裂行为生效，或测得 Present 到扫描延迟 ≤ 1 帧 |
| macOS `displaySyncEnabled = NO` | 帧率不被 VSync 限制 |
| 工具条窗口覆盖视频 | 显示正常，能点击，全屏 Space 内也一样 |
| 调整大小、跨屏改变 DPI | 无黑屏、无尺寸错误 |
| NativeAOT 发布 | 回调正常工作，统计包大小 |

退路：子窗口拿不到独立翻转时，游戏模式改为 owned 顶层无边框窗口承载交换链（`Presenter` 接口不变）。

### S2 传输（`spikes/s2-transport`）

quinn 0.11 + `VirtualSocket` + `MediaController`，回环及 `dnctl` 模拟弱网。

| 检查 | 通过标准 |
|---|---|
| 120 帧/s、100 Mbps 数据报 | 不被 quinn 阻塞；quinn 层增加的 P99 延迟 < 1 ms |
| 同一连接上并发可靠流 | 控制消息 P99 不超过 RTT + 2 ms |
| 2% 丢包 + RS 25% | 帧完整率 > 99.9% |
| 中途切换底层 UDP 路径 | 连接不断，中断 < 100 ms |
| 关闭 PMTUD、固定 1200 | 实测有效载荷与 `max_datagram_size` 符合 |
| 收包时间戳抖动 | 在 tokio 线程读取的抖动足以支撑延迟梯度；否则改为媒体专用线程 |

退路：同 socket 开两条 QUIC 连接；再退用自研 UDP + Noise。

### S3 零拷贝管线（`spikes/s3-pipeline`）

单进程内跑完 采集 → 编码 → 分片 → 回环 → 重组 → 解码 → 呈现，并输出 §6.1 时间线。

**Windows 编码后端**按实际显卡选择，顺序为 NVENC → AMF → QSV → Media Foundation 硬件编码器。M0 只需实现验证机对应的一个后端。没有可用硬件编码时用 x264 跑通流程，但延迟数据不作为通过依据。

| 检查 | 通过标准 |
|---|---|
| Windows 1080p120（硬件编码） | 总和 ≤ 20 ms（P50），P99 ≤ 30 ms；编码器达不到 120 fps 时，记录实际最高帧率，并按该帧率评估 |
| macOS 1080p60 | 总和 ≤ 35 ms（P50） |
| 丢帧恢复（按后端能力） | 支持 RFI 或 LTR 的后端：人为丢帧后不出 IDR，解码器恢复，画面无破损；只支持帧内刷新或 IDR 的后端：恢复时间和码率尖峰有记录，并评估对游戏模式的影响 |
| 后端能力清单 | 记录验证机上的 RFI、LTR、帧内刷新、动态码率、4:4:4 支持情况 |
| VT 低延迟 + HEVC | 明确是否可用 |
| VT LTR 与低延迟同开 | 明确是否可用，恢复是否生效 |
| H.264 丢帧后 frame_num 跳变 | 目标硬解是否接受 |
| HEVC 4:4:4 编解码 | 列出实测可用的平台/GPU |
| 游戏占满 GPU | 编码帧率维持目标的 ≥ 90% |

### M0 产出

`docs/spikes/M0-report.md`：每项结果、实测数据、对本文的修订清单。任何项失败都先走退路并修订设计，再进入 M1。
