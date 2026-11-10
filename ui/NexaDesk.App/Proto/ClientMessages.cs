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
}

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

public sealed record ClientConfig(string DeviceName, string DataDir = "")
{
    public byte[] Encode() => new ProtoWriter().String(1, DeviceName).String(2, DataDir).ToArray();
}

public abstract record ConnectAuth
{
    public sealed record KnownHost(string Key) : ConnectAuth;
    public sealed record Code(string Value) : ConnectAuth;
}

public sealed record ConnectRequest(string Address, ulong Tag, StreamMode Mode, uint Fps, ConnectAuth Auth)
{
    public byte[] Encode()
    {
        var w = new ProtoWriter().String(1, Address).UInt64(2, Tag).Int32(3, (int)Mode).UInt32(7, Fps);
        switch (Auth)
        {
            case ConnectAuth.KnownHost k: w.String(4, k.Key); break;
            case ConnectAuth.Code c: w.String(5, c.Value); break;
        }
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
}

public sealed record KnownHost(string Key, string Name, ulong LastSeen);

public sealed record StreamStats(
    ulong Complete, ulong Lost, ulong FecRecovered, ulong Decoded, ulong DecodeErrors,
    ulong Presented, ulong RttUs, uint GatePastes, uint GateHeld, uint GateTimeouts, ulong RxBytes);

/// <summary>Everything the library sends to the callback. Tag 0 is not tied to a session.</summary>
public abstract record ClientEvent(ulong Tag);

public sealed record ReadyEvent(ulong Tag, string ThisDeviceKey, IReadOnlyList<KnownHost> Hosts, string Error) : ClientEvent(Tag);
public sealed record ConnectingEvent(ulong Tag) : ClientEvent(Tag);
public sealed record ConnectedEvent(ulong Tag, string HostKey, string HostName, Permissions Perms, uint HostOs, bool PairedNow) : ClientEvent(Tag);
public sealed record StreamChangedEvent(ulong Tag, uint Codec, StreamMode Mode, uint Width, uint Height, uint Fps) : ClientEvent(Tag);
public sealed record InputCaptureEvent(ulong Tag, CaptureState State) : ClientEvent(Tag);
public sealed record PasteGateEvent(ulong Tag, uint WaitedMs, bool TimedOut) : ClientEvent(Tag);
public sealed record ClipboardNoticeEvent(ulong Tag, string Text) : ClientEvent(Tag);
public sealed record StatsEvent(ulong Tag, StreamStats Stats) : ClientEvent(Tag);
public sealed record DisconnectedEvent(ulong Tag, DisconnectReason Reason, string Message) : ClientEvent(Tag);
public sealed record HostKeyEvent(ulong Tag) : ClientEvent(Tag);
public sealed record WarningEvent(ulong Tag, string Text) : ClientEvent(Tag);
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
            else if (field is >= 1 and <= 11) { body = field; payload = r.Bytes(); }
            else r.Skip();
        }
        return body switch
        {
            1 => Ready(tag, payload),
            2 => new ConnectingEvent(tag),
            3 => Connected(tag, payload),
            4 => Stream(tag, payload),
            5 => new InputCaptureEvent(tag, (CaptureState)FirstVarint(payload, 1)),
            6 => PasteGate(tag, payload),
            7 => new ClipboardNoticeEvent(tag, FirstString(payload, 1)),
            8 => Stats(tag, payload),
            9 => Disconnected(tag, payload),
            10 => new HostKeyEvent(tag),
            11 => new WarningEvent(tag, FirstString(payload, 1)),
            _ => new UnknownEvent(tag),
        };
    }

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
                default: r.Skip(); break;
            }
        }
        return new ConnectedEvent(tag, key, name, perms, os, paired);
    }

    static StreamChangedEvent Stream(ulong tag, ReadOnlySpan<byte> data)
    {
        uint codec = 0, w = 0, h = 0, fps = 0;
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
                default: r.Skip(); break;
            }
        }
        return new StreamChangedEvent(tag, codec, mode, w, h, fps);
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
        var v = new ulong[12];
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            if (f is >= 1 and <= 11) v[f] = r.UInt64();
            else r.Skip();
        }
        return new StatsEvent(tag, new StreamStats(v[1], v[2], v[3], v[4], v[5], v[6], v[7],
            (uint)v[8], (uint)v[9], (uint)v[10], v[11]));
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
