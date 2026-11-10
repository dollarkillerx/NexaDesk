// The agent IPC (§5.3): crates/nexa-agent/proto/agent_ipc.proto. Kept in step by hand.

namespace NexaDesk.Proto;

public static class Ipc
{
    public const uint ProtocolVersion = 1;
    public const string SocketName = "agent.sock";
    public const int MaxFrame = 64 * 1024;
}

/// <summary>UI → agent.</summary>
public abstract record UiRequest
{
    protected abstract void Write(ProtoWriter w);

    public byte[] Encode()
    {
        var w = new ProtoWriter();
        Write(w);
        return w.ToArray();
    }

    public sealed record Hello(uint Version, string Client) : UiRequest
    {
        protected override void Write(ProtoWriter w) => w.Message(1, m => m.UInt32(1, Version).String(2, Client));
    }

    public sealed record GetStatus : UiRequest
    {
        protected override void Write(ProtoWriter w) => w.Message(2, _ => { });
    }

    /// <summary>Can only narrow what a paired device may do.</summary>
    public sealed record SetAccessPolicy(string DeviceKey, bool Unattended, Permissions Perms) : UiRequest
    {
        protected override void Write(ProtoWriter w) =>
            w.Message(3, m => m.String(1, DeviceKey).Bool(2, Unattended).Message(3, Perms.Write));
    }

    public sealed record ListPairings : UiRequest
    {
        protected override void Write(ProtoWriter w) => w.Message(4, _ => { });
    }

    public sealed record RevokePairing(string DeviceKey) : UiRequest
    {
        protected override void Write(ProtoWriter w) => w.Message(5, m => m.String(1, DeviceKey));
    }

    public sealed record Disconnect : UiRequest
    {
        protected override void Write(ProtoWriter w) => w.Message(6, _ => { });
    }

    public sealed record RotateCode : UiRequest
    {
        protected override void Write(ProtoWriter w) => w.Message(7, _ => { });
    }

    /// <summary>The answer to an IncomingRequest. Only <see cref="Allow"/> with View grants anything.</summary>
    public sealed record Decide(ulong ReqId, bool Allow, Permissions Perms, bool Save, bool SkipNextTime) : UiRequest
    {
        public static Decide Deny(ulong reqId) => new(reqId, false, Permissions.None, false, false);

        protected override void Write(ProtoWriter w) =>
            w.Message(8, m => m.UInt64(1, ReqId).Bool(2, Allow).Message(3, Perms.Write).Bool(4, Save).Bool(5, SkipNextTime));
    }

    public sealed record Shutdown : UiRequest
    {
        protected override void Write(ProtoWriter w) => w.Message(9, _ => { });
    }
}

public enum AuthKind { Unspecified = 0, Paired = 1, Code = 2, Password = 3 }

public enum NoticeKind { Warning = 0, ConnectionFailed = 1, Clipboard = 2 }

public sealed record HostSession(ulong Id, string DeviceName, string DeviceId, string DeviceKey, string Remote, Permissions Perms);

public sealed record PairedDevice(string DeviceKey, string DeviceId, string Name, Permissions Perms, ulong AddedAt, ulong LastSeen, bool Unattended);

/// <summary>Agent → UI.</summary>
public abstract record AgentMessage
{
    public sealed record Status(uint Version, string Listen, string Code, string DeviceName, string DeviceId,
        string DeviceKey, HostSession? Session, bool InputAvailable) : AgentMessage;

    public sealed record IncomingRequest(ulong ReqId, string DeviceName, string DeviceId, string DeviceKey,
        string Remote, AuthKind Auth, Permissions Perms, bool OfferSave, bool SaveDefault, bool OfferSkipNextTime,
        bool Takeover, uint TimeoutMs, bool InputAvailable) : AgentMessage;

    public sealed record SessionStarted(HostSession Session) : AgentMessage;
    public sealed record SessionEnded(ulong Id, string DeviceName, string Reason) : AgentMessage;
    public sealed record Pairings(IReadOnlyList<PairedDevice> Devices) : AgentMessage;
    public sealed record RequestClosed(ulong ReqId, string Reason) : AgentMessage;
    public sealed record Notice(NoticeKind Kind, string Text) : AgentMessage;
    public sealed record Refused(string Command, string Reason) : AgentMessage;
    public sealed record Unknown : AgentMessage;

    public static AgentMessage Decode(ReadOnlySpan<byte> data)
    {
        var r = new ProtoReader(data);
        AgentMessage result = new Unknown();
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: result = DecodeStatus(r.Bytes()); break;
                case 2: result = DecodeIncoming(r.Bytes()); break;
                case 3: result = new SessionStarted(DecodeSessionField(r.Bytes(), 1) ?? EmptySession); break;
                case 4: result = DecodeEnded(r.Bytes()); break;
                case 5: result = DecodePairings(r.Bytes()); break;
                case 6: result = DecodeClosed(r.Bytes()); break;
                case 7: result = DecodeNotice(r.Bytes()); break;
                case 8: result = DecodeRefused(r.Bytes()); break;
                default: r.Skip(); break;
            }
        }
        return result;
    }

    static readonly HostSession EmptySession = new(0, "", "", "", "", Permissions.None);

    static HostSession? DecodeSessionField(ReadOnlySpan<byte> data, int field)
    {
        var r = new ProtoReader(data);
        HostSession? s = null;
        while (r.Next(out var f))
        {
            if (f == field) s = DecodeSession(r.Bytes());
            else r.Skip();
        }
        return s;
    }

    static HostSession DecodeSession(ReadOnlySpan<byte> data)
    {
        ulong id = 0;
        string name = "", did = "", key = "", remote = "";
        var perms = Permissions.None;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: id = r.UInt64(); break;
                case 2: name = r.String(); break;
                case 3: did = r.String(); break;
                case 4: key = r.String(); break;
                case 5: remote = r.String(); break;
                case 6: perms = Permissions.Read(r.Bytes()); break;
                default: r.Skip(); break;
            }
        }
        return new HostSession(id, name, did, key, remote, perms);
    }

    static Status DecodeStatus(ReadOnlySpan<byte> data)
    {
        uint version = 0;
        string listen = "", code = "", name = "", did = "", key = "";
        HostSession? session = null;
        var input = false;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: version = r.UInt32(); break;
                case 2: listen = r.String(); break;
                case 3: code = r.String(); break;
                case 4: name = r.String(); break;
                case 5: did = r.String(); break;
                case 6: key = r.String(); break;
                case 7: session = DecodeSession(r.Bytes()); break;
                case 8: input = r.Bool(); break;
                default: r.Skip(); break;
            }
        }
        return new Status(version, listen, code, name, did, key, session, input);
    }

    static IncomingRequest DecodeIncoming(ReadOnlySpan<byte> data)
    {
        ulong id = 0;
        string name = "", did = "", key = "", remote = "";
        var auth = AuthKind.Unspecified;
        var perms = Permissions.None;
        bool offerSave = false, saveDefault = false, offerSkip = false, takeover = false, input = false;
        uint timeout = 0;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: id = r.UInt64(); break;
                case 2: name = r.String(); break;
                case 3: did = r.String(); break;
                case 4: key = r.String(); break;
                case 5: remote = r.String(); break;
                case 6: auth = (AuthKind)r.Int32(); break;
                case 7: perms = Permissions.Read(r.Bytes()); break;
                case 8: offerSave = r.Bool(); break;
                case 9: saveDefault = r.Bool(); break;
                case 10: offerSkip = r.Bool(); break;
                case 11: takeover = r.Bool(); break;
                case 12: timeout = r.UInt32(); break;
                case 13: input = r.Bool(); break;
                default: r.Skip(); break;
            }
        }
        return new IncomingRequest(id, name, did, key, remote, auth, perms, offerSave, saveDefault, offerSkip, takeover, timeout, input);
    }

    static SessionEnded DecodeEnded(ReadOnlySpan<byte> data)
    {
        ulong id = 0;
        string name = "", reason = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: id = r.UInt64(); break;
                case 2: name = r.String(); break;
                case 3: reason = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new SessionEnded(id, name, reason);
    }

    static Pairings DecodePairings(ReadOnlySpan<byte> data)
    {
        var list = new List<PairedDevice>();
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            if (f == 1) list.Add(DecodePaired(r.Bytes()));
            else r.Skip();
        }
        return new Pairings(list);
    }

    static PairedDevice DecodePaired(ReadOnlySpan<byte> data)
    {
        string key = "", did = "", name = "";
        var perms = Permissions.None;
        ulong added = 0, seen = 0;
        var unattended = false;
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: key = r.String(); break;
                case 2: did = r.String(); break;
                case 3: name = r.String(); break;
                case 4: perms = Permissions.Read(r.Bytes()); break;
                case 5: added = r.UInt64(); break;
                case 6: seen = r.UInt64(); break;
                case 7: unattended = r.Bool(); break;
                default: r.Skip(); break;
            }
        }
        return new PairedDevice(key, did, name, perms, added, seen, unattended);
    }

    static RequestClosed DecodeClosed(ReadOnlySpan<byte> data)
    {
        ulong id = 0;
        var reason = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: id = r.UInt64(); break;
                case 2: reason = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new RequestClosed(id, reason);
    }

    static Notice DecodeNotice(ReadOnlySpan<byte> data)
    {
        var kind = NoticeKind.Warning;
        var text = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: kind = (NoticeKind)r.Int32(); break;
                case 2: text = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new Notice(kind, text);
    }

    static Refused DecodeRefused(ReadOnlySpan<byte> data)
    {
        string command = "", reason = "";
        var r = new ProtoReader(data);
        while (r.Next(out var f))
        {
            switch (f)
            {
                case 1: command = r.String(); break;
                case 2: reason = r.String(); break;
                default: r.Skip(); break;
            }
        }
        return new Refused(command, reason);
    }
}
