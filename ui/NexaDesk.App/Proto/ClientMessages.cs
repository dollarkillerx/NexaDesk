// Messages crossing the libnexa_client C ABI: crates/nexa-client/proto/nexa_client.proto.
// Field numbers must stay in step with that file by hand.

namespace NexaDesk.Proto;

public enum StreamMode { Office = 0, Balanced = 1, Game = 2 }

public enum CaptureState { Inactive = 0, Active = 1, Degraded = 2, Unavailable = 3 }

public enum DisconnectReason
{
    Unspecified = 0,
    ClosedLocally = 1,
    HostEnded = 2,
    Replaced = 3,
    PipelineFailed = 4,
    Network = 5,
    PinMismatch = 6,
    NotPaired = 7,
    AuthFailed = 8,
    RateLimited = 9,
    ConsentDenied = 10,
    NoCommonCodec = 11,
    VersionMismatch = 12,
    TimedOut = 13,
    InvalidRequest = 14,
    Internal = 15,
    Offline = 16,
    NoPath = 17,
    SignalServer = 18,
}

/// <summary>How a session's packets travel (Stats.path).</summary>
public enum PathKind { Unknown = 0, Direct = 1, Punched = 2, Relay = 3, Tailscale = 4, RelayWss = 5, TailscaleDerp = 6 }

/// <summary>Where a connection attempt is (Connecting.stage).</summary>
public enum ConnectStage { Quic = 0, Resolving = 1, Gathering = 2, Checking = 3 }

public sealed record Permissions(bool View, bool Input, bool Clipboard, bool File = false, bool Audio = false)
{
    public static readonly Permissions None = new(false, false, false);

    internal void Write(ProtoWriter w) =>
        w.Bool(1, View).Bool(2, Input).Bool(3, Clipboard).Bool(4, File).Bool(5, Audio);

    internal static Permissions Read(ReadOnlySpan<byte> data)
    {
        bool v = false, i = false, c = false, f = false, a = false;
        var r = new ProtoReader(data);
        while (r.Next(out var field))
        {
            switch (field)
            {
                case 1: v = r.Bool(); break;
                case 2: i = r.Bool(); break;
                case 3: c = r.Bool(); break;
                case 4: f = r.Bool(); break;
                case 5: a = r.Bool(); break;
                default: r.Skip(); break;
            }
        }
        return new Permissions(v, i, c, f, a);
    }
}

/// <summary>An empty DownloadsDir means ~/Downloads/NexaDesk.</summary>
public sealed record ClientConfig(string DeviceName, string DataDir = "", string DownloadsDir = "")
{
    public byte[] Encode() => new ProtoWriter().String(1, DeviceName).String(2, DataDir).String(3, DownloadsDir).ToArray();
}

/// <summary>Where a file transfer is (§3.1 stream 4). Same numbers in the agent IPC and the client ABI.</summary>
public enum TransferState { Waiting = 0, NeedsConfirm = 1, Running = 2, Done = 3, Failed = 4, Cancelled = 5 }

public enum TransferFail
{
    None = 0, Declined = 1, TooLarge = 2, NotPermitted = 3, Invalid = 4, HashMismatch = 5, NoSpace = 6,
    Io = 7, ConnectionLost = 8, Timeout = 9, NotAFile = 10, Busy = 11,
}

/// <summary>One transfer as last reported. Outgoing: from this Mac to the other side.</summary>
public sealed record TransferInfo(ulong Id, bool Outgoing, string Name, ulong Size, ulong Done, TransferState State,
    TransferFail Reason = TransferFail.None, string Path = "", bool ByPeer = false)
{
    public bool IsFinal => State is TransferState.Done or TransferState.Failed or TransferState.Cancelled;

    internal static TransferInfo Of(ulong id, bool outgoing, string name, ulong size, ulong done, int state, int reason,
        string path, bool byPeer) =>
        new(id, outgoing, name, size, done,
            Enum.IsDefined((TransferState)state) ? (TransferState)state : TransferState.Failed,
            Enum.IsDefined((TransferFail)reason) ? (TransferFail)reason : TransferFail.Invalid, path, byPeer);
}

public abstract record ConnectAuth
{
    public sealed record KnownHost(string Key) : ConnectAuth;
    public sealed record Code(string Value) : ConnectAuth;
}

/// <summary>Address is "ip:port" or a 9-digit device ID; SignalServer is used for IDs.</summary>
public sealed record ConnectRequest(string Address, ulong Tag, StreamMode Mode, uint Fps, ConnectAuth Auth,
    string SignalServer = "")
{
    public byte[] Encode()
    {
        var w = new ProtoWriter().String(1, Address).UInt64(2, Tag).Int32(3, (int)Mode).UInt32(7, Fps);
        switch (Auth)
        {
            case ConnectAuth.KnownHost k: w.String(4, k.Key); break;
            case ConnectAuth.Code c: w.String(5, c.Value); break;
        }
        w.String(8, SignalServer);
        return w.ToArray();
    }

    /// <summary>Never shows the code.</summary>
    public override string ToString() => $"ConnectRequest({Address}, tag {Tag})";
}

public abstract record SessionCommand
{
    public abstract byte[] Encode();

    public sealed record PointerMove(double X, double Y) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(1, w => w.Double(1, X).Double(2, Y)).ToArray();
    }

    /// <summary>0 left, 1 right, 2+ others.</summary>
    public sealed record PointerButton(uint Button, bool Down) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(2, w => w.UInt32(1, Button).Bool(2, Down)).ToArray();
    }

    /// <summary>1/120 of a notch; positive is right and up.</summary>
    public sealed record PointerWheel(int Dx, int Dy) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(3, w => w.SInt32(1, Dx).SInt32(2, Dy)).ToArray();
    }

    public sealed record SetMode(StreamMode Mode, uint Fps = 0) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(5, w => w.Int32(1, (int)Mode).UInt32(2, Fps)).ToArray();
    }

    public sealed record SetClipboardPolicy(bool Outbound, bool Inbound) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(6, w => w.Bool(1, Outbound).Bool(2, Inbound)).ToArray();
    }

    /// <summary>Mute (false) or unmute the host's sound. Needs Permissions.Audio.</summary>
    public sealed record SetAudio(bool Enabled) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(20, w => w.Bool(1, Enabled)).ToArray();
    }

    /// <summary>Stream another of the host's displays (DisplayInfo.Id).</summary>
    public sealed record SelectDisplay(uint Display) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(21, w => w.UInt32(1, Display)).ToArray();
    }

    /// <summary>Send this Mac's files to the host. Needs Permissions.File; absolute paths, at most 100.</summary>
    public sealed record SendFiles(IReadOnlyList<string> Paths) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(22, w =>
        {
            foreach (var p in Paths) w.String(1, p);
        }).ToArray();
    }

    public sealed record CancelTransfer(ulong Id) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(23, w => w.UInt64(1, Id)).ToArray();
    }

    public sealed record AnswerTransfer(ulong Id, bool Accept) : SessionCommand
    {
        public override byte[] Encode() => new ProtoWriter().Message(24, w => w.UInt64(1, Id).Bool(2, Accept)).ToArray();
    }
}

/// <summary>One of the host's displays, as listed in Connected.</summary>
public sealed record DisplayInfo(uint Id, string Name, uint Width, uint Height);

public sealed record KnownHost(string Key, string Name, ulong LastSeen);

/// <summary>§6.1 segments the library measures (Stats.latency).</summary>
public enum LatencySegment { HostAndNetwork = 0, Receive = 1, DecodeQueue = 2, Decode = 3, PresentQueue = 4, Present = 5, Total = 6 }

public sealed record SegmentLatency(LatencySegment Segment, uint P50Us, uint P99Us);

public sealed record StreamStats(
    ulong Complete, ulong Lost, ulong FecRecovered, ulong Decoded, ulong DecodeErrors,
    ulong Presented, ulong RttUs, uint GatePastes, uint GateHeld, uint GateTimeouts, ulong RxBytes,
    PathKind Path = PathKind.Unknown, string PathDetail = "", ulong PathRttUs = 0,
    ulong AudioPlayed = 0, ulong AudioConcealed = 0, uint AudioBufferUs = 0,
    IReadOnlyList<SegmentLatency>? Latency = null);

/// <summary>Everything the library sends to the callback. Tag 0 is not tied to a session.</summary>
public abstract record ClientEvent(ulong Tag);

public sealed record ReadyEvent(ulong Tag, string ThisDeviceKey, IReadOnlyList<KnownHost> Hosts, string Error) : ClientEvent(Tag);
public sealed record ConnectingEvent(ulong Tag, ConnectStage Stage = ConnectStage.Quic) : ClientEvent(Tag);
public sealed record ConnectedEvent(ulong Tag, string HostKey, string HostName, Permissions Perms, uint HostOs, bool PairedNow,
    IReadOnlyList<DisplayInfo>? Displays = null) : ClientEvent(Tag);
public sealed record StreamChangedEvent(ulong Tag, uint Codec, StreamMode Mode, uint Width, uint Height, uint Fps, uint Display = 0) : ClientEvent(Tag);
public sealed record InputCaptureEvent(ulong Tag, CaptureState State) : ClientEvent(Tag);
public sealed record PasteGateEvent(ulong Tag, uint WaitedMs, bool TimedOut) : ClientEvent(Tag);
/// <summary>Clipboard status. Text is English; Code lets the UI word it. Kind is "text", "html", "rtf" or "png".</summary>
public enum ClipboardCode { Other = 0, SensitiveKeptLocal = 1, TooLarge = 2, PeerRejected = 3, WriteFailed = 4, FetchFailed = 5, Unavailable = 6 }
public sealed record ClipboardNoticeEvent(ulong Tag, string Text, ClipboardCode Code = ClipboardCode.Other, string Kind = "",
    ulong Size = 0, string Detail = "") : ClientEvent(Tag);
public sealed record StatsEvent(ulong Tag, StreamStats Stats) : ClientEvent(Tag);
public sealed record DisconnectedEvent(ulong Tag, DisconnectReason Reason, string Message) : ClientEvent(Tag);
public sealed record HostKeyEvent(ulong Tag) : ClientEvent(Tag);
/// <summary>What the core wants said. Text is English; Code lets the UI word it, Detail is the variable part.</summary>
public enum WarningCode { Other = 0, GameWssRelay = 1, GameDerp = 2, KeymapInvalid = 3, AudioOutput = 4, RememberHost = 5, AudioPlayback = 6, PointerLock = 7 }
public sealed record WarningEvent(ulong Tag, string Text, WarningCode Code = WarningCode.Other, string Detail = "") : ClientEvent(Tag);
/// <summary>§4.4: the transport dropped and attempt n is starting; a Connected for the tag ends it.</summary>
public sealed record ReconnectingEvent(ulong Tag, uint Attempt, string Reason, uint RemainingMs) : ClientEvent(Tag);
/// <summary>A Tailscale peer answering on the NexaDesk port. Address is "ip:port".</summary>
public sealed record TailnetDevice(string Name, string Address, bool Direct, string Os, string DnsName);
/// <summary>This machine's Tailscale (§4.7): no tailnet address, an address but no readable status, or running.</summary>
public enum TailscaleState { Absent = 0, AddressOnly = 1, Running = 2 }
/// <summary>Tag 0: this machine's Tailscale and the whole current list. SelfDnsName is the MagicDNS name, or empty.</summary>
public sealed record TailnetDevicesEvent(ulong Tag, IReadOnlyList<TailnetDevice> Devices,
    TailscaleState State = TailscaleState.Running, string SelfName = "", string SelfDnsName = "",
    IReadOnlyList<string>? SelfIps = null) : ClientEvent(Tag)
{
    public IReadOnlyList<string> Ips => SelfIps ?? [];
}
/// <summary>A host heard on the LAN over mDNS (§4.11): only an address to try; TLS and pairing still decide.</summary>
public sealed record LanDevice(string Name, string Address, string Os, string DeviceId);
/// <summary>Tag 0: the whole current list.</summary>
public sealed record LanDevicesEvent(ulong Tag, IReadOnlyList<LanDevice> Devices) : ClientEvent(Tag);
public sealed record TransferEvent(ulong Tag, TransferInfo Info) : ClientEvent(Tag);
public sealed record UnknownEvent(ulong Tag) : ClientEvent(Tag);

public static class ClientCodec
{
    public static ClientEvent DecodeEvent(ReadOnlySpan<byte> data)
    {
        ulong tag = 0;
        var body = 0;
        ReadOnlySpan<byte> payload = default;
        var r = new ProtoReader(data);
        while (r.Next(out var field))
        {
            if (field == 20) tag = r.UInt64();
            else if (field is >= 1 and <= 11 or 40 or 41 or 42 or 43) { body = field; payload = r.Bytes(); }
            else r.Skip();
        }
        return body switch
        {
            1 => Ready(tag, payload),
            2 => new ConnectingEvent(tag, Stage(FirstVarint(payload, 1))),
            3 => Connected(tag, payload),
            4 => Stream(tag, payload),
            5 => new InputCaptureEvent(tag, (CaptureState)FirstVarint(payload, 1)),
            6 => PasteGate(tag, payload),
            7 => ClipboardNotice(tag, payload),
            8 => Stats(tag, payload),
            9 => Disconnected(tag, payload),
            10 => new HostKeyEvent(tag),
            11 => Warning(tag, payload),
            40 => Reconnecting(tag, payload),
            41 => Tailnet(tag, payload),
            42 => Transfer(tag, payload),
            43 => Lan(tag, payload),
            _ => new UnknownEvent(tag),
        };
    }

    static ConnectStage Stage(ulong v) =>
        v <= 3 ? (ConnectStage)(int)v : ConnectStage.Quic;

    static ulong FirstVarint(ReadOnlySpan<byte> data, int want)
    {
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            if (f == want) return r.UInt64();
            r.Skip();
        }
        return 0;
    }

    static string FirstString(ReadOnlySpan<byte> data, int want)
    {
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            if (f == want) return r.String();
            r.Skip();
        }
        return "";
    }

    static ReadyEvent Ready(ulong tag, ReadOnlySpan<byte> data)
    {
        string key = "", error = "";
        var hosts = new List<KnownHost>();
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: key = r.String(); break;
                case 2: hosts.Add(Host(r.Bytes())); break;
                case 3: error = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new ReadyEvent(tag, key, hosts, error);
    }

    static KnownHost Host(ReadOnlySpan<byte> data)
    {
        string key = "", name = "";
        ulong seen = 0;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: key = r.String(); break;
                case 2: name = r.String(); break;
                case 3: seen = r.UInt64(); break;
                default: r.Skip(); break;
            }
        }
        return new KnownHost(key, name, seen);
    }

    static ConnectedEvent Connected(ulong tag, ReadOnlySpan<byte> data)
    {
        string key = "", name = "";
        var perms = Permissions.None;
        uint os = 0;
        var paired = false;
        var displays = new List<DisplayInfo>();
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: key = r.String(); break;
                case 2: name = r.String(); break;
                case 3: perms = Permissions.Read(r.Bytes()); break;
                case 4: os = r.UInt32(); break;
                case 5: paired = r.Bool(); break;
                case 20: displays.Add(Display(r.Bytes())); break;
                default: r.Skip(); break;
            }
        }
        return new ConnectedEvent(tag, key, name, perms, os, paired, displays);
    }

    static DisplayInfo Display(ReadOnlySpan<byte> data)
    {
        uint id = 0, w = 0, h = 0;
        var name = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: id = r.UInt32(); break;
                case 2: name = r.String(); break;
                case 3: w = r.UInt32(); break;
                case 4: h = r.UInt32(); break;
                default: r.Skip(); break;
            }
        }
        return new DisplayInfo(id, name, w, h);
    }

    static StreamChangedEvent Stream(ulong tag, ReadOnlySpan<byte> data)
    {
        uint codec = 0, w = 0, h = 0, fps = 0, display = 0;
        var mode = StreamMode.Office;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: codec = r.UInt32(); break;
                case 2: mode = (StreamMode)r.Int32(); break;
                case 3: w = r.UInt32(); break;
                case 4: h = r.UInt32(); break;
                case 5: fps = r.UInt32(); break;
                case 20: display = r.UInt32(); break;
                default: r.Skip(); break;
            }
        }
        return new StreamChangedEvent(tag, codec, mode, w, h, fps, display);
    }

    static PasteGateEvent PasteGate(ulong tag, ReadOnlySpan<byte> data)
    {
        uint waited = 0;
        var timedOut = false;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: waited = r.UInt32(); break;
                case 2: timedOut = r.Bool(); break;
                default: r.Skip(); break;
            }
        }
        return new PasteGateEvent(tag, waited, timedOut);
    }

    static StatsEvent Stats(ulong tag, ReadOnlySpan<byte> data)
    {
        var v = new ulong[15];
        ulong audioPlayed = 0, audioConcealed = 0, audioBuffer = 0;
        var detail = "";
        var latency = new List<SegmentLatency>();
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            if (f is >= 1 and <= 12 or 14) v[f] = r.UInt64();
            else if (f == 13) detail = r.String();
            else if (f == 30) audioPlayed = r.UInt64();
            else if (f == 31) audioConcealed = r.UInt64();
            else if (f == 32) audioBuffer = r.UInt64();
            else if (f == 40) latency.Add(Segment(r.Bytes()));
            else r.Skip();
        }
        var path = v[12] <= 6 ? (PathKind)(int)v[12] : PathKind.Unknown;
        return new StatsEvent(tag, new StreamStats(v[1], v[2], v[3], v[4], v[5], v[6], v[7],
            (uint)v[8], (uint)v[9], (uint)v[10], v[11], path, detail, v[14],
            audioPlayed, audioConcealed, (uint)Math.Min(audioBuffer, uint.MaxValue), latency));
    }

    static TransferEvent Transfer(ulong tag, ReadOnlySpan<byte> data)
    {
        ulong id = 0, size = 0, done = 0;
        bool outgoing = false, byPeer = false;
        string name = "", path = "";
        int state = 0, reason = 0;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: id = r.UInt64(); break;
                case 2: outgoing = r.Bool(); break;
                case 3: name = r.String(); break;
                case 4: size = r.UInt64(); break;
                case 5: done = r.UInt64(); break;
                case 6: state = r.Int32(); break;
                case 7: reason = r.Int32(); break;
                case 8: path = r.String(); break;
                case 9: byPeer = r.Bool(); break;
                default: r.Skip(); break;
            }
        }
        return new TransferEvent(tag, TransferInfo.Of(id, outgoing, name, size, done, state, reason, path, byPeer));
    }

    static SegmentLatency Segment(ReadOnlySpan<byte> data)
    {
        int seg = 0;
        uint p50 = 0, p99 = 0;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: seg = r.Int32(); break;
                case 2: p50 = r.UInt32(); break;
                case 3: p99 = r.UInt32(); break;
                default: r.Skip(); break;
            }
        }
        return new SegmentLatency(Enum.IsDefined((LatencySegment)seg) ? (LatencySegment)seg : LatencySegment.Total, p50, p99);
    }

    static ReconnectingEvent Reconnecting(ulong tag, ReadOnlySpan<byte> data)
    {
        uint attempt = 0, remaining = 0;
        var reason = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: attempt = r.UInt32(); break;
                case 2: reason = r.String(); break;
                case 3: remaining = r.UInt32(); break;
                default: r.Skip(); break;
            }
        }
        return new ReconnectingEvent(tag, attempt, reason, remaining);
    }

    static TailnetDevicesEvent Tailnet(ulong tag, ReadOnlySpan<byte> data)
    {
        var list = new List<TailnetDevice>();
        var ips = new List<string>();
        var state = TailscaleState.Absent;
        string name = "", dns = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: list.Add(Device(r.Bytes())); break;
                case 2:
                    var v = r.UInt64();
                    state = v <= 2 ? (TailscaleState)v : TailscaleState.Absent;
                    break;
                case 3: name = r.String(); break;
                case 4: dns = r.String(); break;
                case 5: ips.Add(r.String()); break;
                default: r.Skip(); break;
            }
        }
        return new TailnetDevicesEvent(tag, list, state, name, dns, ips);
    }

    static ClipboardNoticeEvent ClipboardNotice(ulong tag, ReadOnlySpan<byte> data)
    {
        string text = "", kind = "", detail = "";
        var code = ClipboardCode.Other;
        ulong size = 0;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: text = r.String(); break;
                case 2:
                    var v = r.UInt64();
                    code = v <= 6 ? (ClipboardCode)v : ClipboardCode.Other;
                    break;
                case 3: kind = r.String(); break;
                case 4: size = r.UInt64(); break;
                case 5: detail = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new ClipboardNoticeEvent(tag, text, code, kind, size, detail);
    }

    static WarningEvent Warning(ulong tag, ReadOnlySpan<byte> data)
    {
        string text = "", detail = "";
        var code = WarningCode.Other;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: text = r.String(); break;
                case 2:
                    var v = r.UInt64();
                    code = v <= 7 ? (WarningCode)v : WarningCode.Other;
                    break;
                case 3: detail = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new WarningEvent(tag, text, code, detail);
    }

    static LanDevicesEvent Lan(ulong tag, ReadOnlySpan<byte> data)
    {
        var list = new List<LanDevice>();
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            if (f == 1) list.Add(LanEntry(r.Bytes()));
            else r.Skip();
        }
        return new LanDevicesEvent(tag, list);
    }

    static LanDevice LanEntry(ReadOnlySpan<byte> data)
    {
        string name = "", address = "", os = "", id = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: name = r.String(); break;
                case 2: address = r.String(); break;
                case 3: os = r.String(); break;
                case 4: id = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new LanDevice(name, address, os, id);
    }

    static TailnetDevice Device(ReadOnlySpan<byte> data)
    {
        string name = "", address = "", os = "", dns = "";
        var direct = false;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: name = r.String(); break;
                case 2: address = r.String(); break;
                case 3: direct = r.Bool(); break;
                case 4: os = r.String(); break;
                case 5: dns = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new TailnetDevice(name, address, direct, os, dns);
    }

    static DisconnectedEvent Disconnected(ulong tag, ReadOnlySpan<byte> data)
    {
        var reason = DisconnectReason.Unspecified;
        var message = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: reason = (DisconnectReason)r.Int32(); break;
                case 2: message = r.String(); break;
                default: r.Skip(); break;
            }
        }
        if (!Enum.IsDefined(reason)) reason = DisconnectReason.Unspecified;
        return new DisconnectedEvent(tag, reason, message);
    }
}

// ---------------------------------------------------------------------------------
// Keyboard settings (§11.2): nexa_keymap_rules / _save / _preview.
// ---------------------------------------------------------------------------------

/// <summary>OS numbers as in Connected.host_os.</summary>
public enum RemoteOs : uint { Windows = 1, Macos = 2, Linux = 3 }

/// <summary>Where a chord is pressed: rules with a ctx apply only there.</summary>
public enum KeyContext : uint { Default = 0, Terminal = 1, Ide = 2 }

public sealed record KeymapRule(bool Builtin, string Ctx, string When, IReadOnlyList<string> Then, string Hold, string Tag, bool Enabled)
{
    internal void Write(ProtoWriter w)
    {
        w.Bool(1, Builtin).String(2, Ctx).String(3, When);
        foreach (var t in Then) w.String(4, t);
        w.String(5, Hold).String(6, Tag).Bool(7, Enabled);
    }

    internal static KeymapRule Read(ReadOnlySpan<byte> data)
    {
        bool builtin = false, enabled = false;
        string ctx = "", when = "", hold = "", tag = "";
        var then = new List<string>();
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: builtin = r.Bool(); break;
                case 2: ctx = r.String(); break;
                case 3: when = r.String(); break;
                case 4: then.Add(r.String()); break;
                case 5: hold = r.String(); break;
                case 6: tag = r.String(); break;
                case 7: enabled = r.Bool(); break;
                default: r.Skip(); break;
            }
        }
        return new KeymapRule(builtin, ctx, when, then, hold, tag, enabled);
    }
}

/// <summary>One request type for all three calls; Rules/Edited/Chord/Ctx only where they apply.</summary>
public sealed record KeymapRequest(string DataDir, RemoteOs LocalOs, RemoteOs RemoteOs,
    IReadOnlyList<KeymapRule>? Rules = null, bool Edited = false, string Chord = "", KeyContext Ctx = KeyContext.Default)
{
    public byte[] Encode()
    {
        var w = new ProtoWriter().String(1, DataDir).UInt32(2, (uint)LocalOs).UInt32(3, (uint)RemoteOs);
        foreach (var rule in Rules ?? []) w.Message(4, rule.Write);
        return w.Bool(5, Edited).String(6, Chord).UInt32(7, (uint)Ctx).ToArray();
    }
}

/// <summary>Error is English; FileError, when set, is the reason keymap.toml could not be used, for the UI to word.</summary>
public sealed record KeymapView(IReadOnlyList<KeymapRule> Rules, string Error, string FileError = "")
{
    public static KeymapView Decode(ReadOnlySpan<byte> data)
    {
        var rules = new List<KeymapRule>();
        string error = "", fileError = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            if (f == 1) rules.Add(KeymapRule.Read(r.Bytes()));
            else if (f == 2) error = r.String();
            else if (f == 3) fileError = r.String();
            else r.Skip();
        }
        return new KeymapView(rules, error, fileError);
    }
}

public sealed record KeyOut(string Key, bool Down);

public sealed record KeyPreview(IReadOnlyList<KeyOut> Events, IReadOnlyList<string> Sent, bool Paste, string Error)
{
    public static KeyPreview Decode(ReadOnlySpan<byte> data)
    {
        var events = new List<KeyOut>();
        var sent = new List<string>();
        var paste = false;
        var error = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1:
                {
                    var inner = new ProtoReader(r.Bytes());
                    string key = "";
                    var down = false;
                    while (inner.Next(out var g))
                    {
                        if (g == 1) key = inner.String();
                        else if (g == 2) down = inner.Bool();
                        else inner.Skip();
                    }
                    events.Add(new KeyOut(key, down));
                    break;
                }
                case 2: sent.Add(r.String()); break;
                case 3: paste = r.Bool(); break;
                case 4: error = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new KeyPreview(events, sent, paste, error);
    }
}
