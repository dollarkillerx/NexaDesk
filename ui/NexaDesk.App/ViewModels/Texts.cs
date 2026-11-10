using System.Text;
using System.Text.RegularExpressions;
using NexaDesk.Localization;
using NexaDesk.Proto;

namespace NexaDesk.ViewModels;

public sealed record ErrorText(string Title, string Detail);

/// <summary>Shared UI text helpers; the strings themselves are in Localization/Strings/*/texts.json.</summary>
public static partial class Texts
{
    public const int DefaultPort = 47800;

    /// <summary>What a Disconnected reason means for the person, and what to do about it.</summary>
    public static ErrorText Disconnect(DisconnectReason reason) => reason switch
    {
        DisconnectReason.ClosedLocally => new(L.Get("Texts.Disconnect.ClosedLocally.Title"), L.Get("Texts.Disconnect.ClosedLocally.Detail")),
        DisconnectReason.HostEnded => new(L.Get("Texts.Disconnect.HostEnded.Title"), L.Get("Texts.Disconnect.HostEnded.Detail")),
        DisconnectReason.Replaced => new(L.Get("Texts.Disconnect.Replaced.Title"), L.Get("Texts.Disconnect.Replaced.Detail")),
        DisconnectReason.PipelineFailed => new(L.Get("Texts.Disconnect.PipelineFailed.Title"), L.Get("Texts.Disconnect.PipelineFailed.Detail")),
        DisconnectReason.Network => new(L.Get("Texts.Disconnect.Network.Title"), L.Get("Texts.Disconnect.Network.Detail")),
        DisconnectReason.PinMismatch => new(L.Get("Texts.Disconnect.PinMismatch.Title"), L.Get("Texts.Disconnect.PinMismatch.Detail")),
        DisconnectReason.NotPaired => new(L.Get("Texts.Disconnect.NotPaired.Title"), L.Get("Texts.Disconnect.NotPaired.Detail")),
        DisconnectReason.AuthFailed => new(L.Get("Texts.Disconnect.AuthFailed.Title"), L.Get("Texts.Disconnect.AuthFailed.Detail")),
        DisconnectReason.RateLimited => new(L.Get("Texts.Disconnect.RateLimited.Title"), L.Get("Texts.Disconnect.RateLimited.Detail")),
        DisconnectReason.ConsentDenied => new(L.Get("Texts.Disconnect.ConsentDenied.Title"), L.Get("Texts.Disconnect.ConsentDenied.Detail")),
        DisconnectReason.NoCommonCodec => new(L.Get("Texts.Disconnect.NoCommonCodec.Title"), L.Get("Texts.Disconnect.NoCommonCodec.Detail")),
        DisconnectReason.VersionMismatch => new(L.Get("Texts.Disconnect.VersionMismatch.Title"), L.Get("Texts.Disconnect.VersionMismatch.Detail")),
        DisconnectReason.TimedOut => new(L.Get("Texts.Disconnect.TimedOut.Title"), L.Get("Texts.Disconnect.TimedOut.Detail")),
        DisconnectReason.InvalidRequest => new(L.Get("Texts.Disconnect.InvalidRequest.Title"), L.Get("Texts.Disconnect.InvalidRequest.Detail")),
        DisconnectReason.Internal => new(L.Get("Texts.Disconnect.Internal.Title"), L.Get("Texts.Disconnect.Internal.Detail")),
        DisconnectReason.Offline => new(L.Get("Texts.Disconnect.Offline.Title"), L.Get("Texts.Disconnect.Offline.Detail")),
        DisconnectReason.NoPath => new(L.Get("Texts.Disconnect.NoPath.Title"), L.Get("Texts.Disconnect.NoPath.Detail")),
        DisconnectReason.SignalServer => new(L.Get("Texts.Disconnect.SignalServer.Title"), L.Get("Texts.Disconnect.SignalServer.Detail")),
        _ => new(L.Get("Texts.Disconnect.Other.Title"), L.Get("Texts.Disconnect.Other.Detail")),
    };

    public static string Permissions(Permissions p)
    {
        var parts = new List<string>();
        if (p.View) parts.Add(L.Get("Texts.Perm.View"));
        if (p.Input) parts.Add(L.Get("Texts.Perm.Input"));
        if (p.Clipboard) parts.Add(L.Get("Texts.Perm.Clipboard"));
        if (p.File) parts.Add(L.Get("Texts.Perm.File"));
        if (p.Audio) parts.Add(L.Get("Texts.Perm.Audio"));
        return parts.Count == 0 ? L.Get("Texts.Perm.None") : string.Join(L.Get("Texts.Perm.Separator"), parts);
    }

    public static string Auth(AuthKind kind) => kind switch
    {
        AuthKind.Paired => L.Get("Texts.Auth.Paired"),
        AuthKind.Code => L.Get("Texts.Auth.Code"),
        AuthKind.Password => L.Get("Texts.Auth.Password"),
        _ => L.Get("Texts.Auth.Unknown"),
    };

    /// <summary>A public key in groups of four hex digits, eight groups per line.</summary>
    public static string Fingerprint(string hex)
    {
        if (string.IsNullOrEmpty(hex)) return "—";
        var sb = new StringBuilder();
        for (var i = 0; i < hex.Length; i += 4)
        {
            if (i > 0) sb.Append(i % 32 == 0 ? '\n' : ' ');
            sb.Append(hex.AsSpan(i, Math.Min(4, hex.Length - i)));
        }
        return sb.ToString().ToUpperInvariant();
    }

    /// <summary>The first four groups, for places with little room.</summary>
    public static string ShortFingerprint(string hex) =>
        string.IsNullOrEmpty(hex) ? "—" : Fingerprint(hex.Length > 16 ? hex[..16] : hex);

    /// <summary>ABCD2345 → ABCD-2345.</summary>
    public static string GroupCode(string code) =>
        code.Length == 8 ? $"{code[..4]}-{code[4..]}" : code;

    /// <summary>Strips separators and case; null unless it is eight RFC 4648 base32 characters.</summary>
    public static string? NormalizeCode(string? input)
    {
        if (input == null) return null;
        var s = new string(input.Where(c => !char.IsWhiteSpace(c) && c != '-').ToArray()).ToUpperInvariant();
        return CodeRegex().IsMatch(s) ? s : null;
    }

    [GeneratedRegex("^[A-Z2-7]{8}$")]
    private static partial Regex CodeRegex();

    /// <summary>host:port, [v6]:port, or a host alone (the default port is added). Null if unusable.</summary>
    public static string? NormalizeAddress(string? input)
    {
        var s = input?.Trim() ?? "";
        if (s.Length == 0 || s.Any(char.IsWhiteSpace)) return null;
        string host;
        string port;
        if (s.StartsWith('['))
        {
            var end = s.IndexOf(']');
            if (end < 2) return null;
            host = s[..(end + 1)];
            var rest = s[(end + 1)..];
            if (rest.Length == 0) port = DefaultPort.ToString();
            else if (rest.StartsWith(':')) port = rest[1..];
            else return null;
        }
        else
        {
            var colons = s.Count(c => c == ':');
            if (colons > 1) return null; // a bare IPv6 address needs brackets
            var i = s.LastIndexOf(':');
            host = i < 0 ? s : s[..i];
            port = i < 0 ? DefaultPort.ToString() : s[(i + 1)..];
            if (host.Length == 0) return null;
        }
        if (!int.TryParse(port, out var p) || p < 1 || p > 65535 || port.StartsWith('+')) return null;
        return $"{host}:{p}";
    }

    /// <summary>Whether a typed address goes into a tailnet: 100.64.0.0/10, fd7a:115c:a1e0::/48, or a MagicDNS name.</summary>
    public static bool IsTailnetTarget(string address)
    {
        var host = address;
        var i = address.LastIndexOf(':');
        if (i > 0 && !address.EndsWith(']') && address.Count(c => c == ':') == 1) host = address[..i];
        else if (address.StartsWith('[') && address.IndexOf(']') is > 0 and var end) host = address[1..end];
        if (host.EndsWith(".ts.net", StringComparison.OrdinalIgnoreCase) ||
            host.EndsWith(".ts.net.", StringComparison.OrdinalIgnoreCase)) return true;
        return System.Net.IPAddress.TryParse(host, out var ip) && IsTailnetIp(ip);
    }

    public static bool IsTailnetIp(System.Net.IPAddress ip)
    {
        var b = ip.GetAddressBytes();
        return b.Length == 4
            ? b[0] == 100 && b[1] >= 64 && b[1] < 128
            : b[0] == 0xfd && b[1] == 0x7a && b[2] == 0x11 && b[3] == 0x5c && b[4] == 0xa1 && b[5] == 0xe0;
    }

    /// <summary>Added to a failure across networks: Tailscale gets through, relaying when it must.</summary>
    public static string TailscaleAdvice => L.Get("Texts.TailscaleAdvice");

    /// <summary>Where a direct connection goes, from its address.</summary>
    public static string PathOf(string address)
    {
        var host = address;
        var i = address.LastIndexOf(':');
        if (i > 0 && !address.EndsWith(']')) host = address[..i];
        host = host.Trim('[', ']');
        if (!System.Net.IPAddress.TryParse(host, out var ip)) return L.Format("Texts.PathOf.Direct", address);
        if (System.Net.IPAddress.IsLoopback(ip)) return L.Format("Texts.PathOf.Loopback", address);
        var b = ip.GetAddressBytes();
        if (b.Length == 4)
        {
            if (b[0] == 100 && b[1] >= 64 && b[1] < 128) return L.Format("Texts.PathOf.Tailscale", address);
            if (b[0] == 10 || (b[0] == 172 && b[1] >= 16 && b[1] < 32) || (b[0] == 192 && b[1] == 168))
                return L.Format("Texts.PathOf.Lan", address);
            return L.Format("Texts.PathOf.Public", address);
        }
        return ip.IsIPv6LinkLocal || (b[0] & 0xFE) == 0xFC ? L.Format("Texts.PathOf.Lan", address) : L.Format("Texts.PathOf.Public", address);
    }

    /// <summary>Nine digits, with or without spaces or dashes: a device ID. Null otherwise.</summary>
    public static string? NormalizeDeviceId(string? input)
    {
        if (input == null) return null;
        var s = new string(input.Where(c => c != ' ' && c != '-' && c != '\u00A0').ToArray());
        return s.Length == 9 && s.All(c => c is >= '0' and <= '9') ? s : null;
    }

    /// <summary>123456789 → 123 456 789.</summary>
    public static string GroupDeviceId(string id) =>
        NormalizeDeviceId(id) is { } d ? $"{d[..3]} {d[3..6]} {d[6..]}" : id;

    /// <summary>What the connect box holds: a device ID, or host:port. Null if neither.</summary>
    public static string? NormalizeTarget(string? input) =>
        NormalizeDeviceId(input) ?? NormalizeAddress(input);

    /// <summary>
    /// The signaling server as the agent and library take it: wss://host[:port], or
    /// ws:// only to this machine. A bare host:port means wss. Null if unusable.
    /// </summary>
    public static string? NormalizeServer(string? input)
    {
        var s = input?.Trim() ?? "";
        if (s.Length == 0 || s.Any(char.IsWhiteSpace)) return null;
        var plain = false;
        if (s.StartsWith("wss://", StringComparison.OrdinalIgnoreCase)) s = s[6..];
        else if (s.StartsWith("ws://", StringComparison.OrdinalIgnoreCase)) { s = s[5..]; plain = true; }
        else if (s.Contains("://")) return null;
        var slash = s.IndexOf('/');
        if (slash >= 0) s = s[..slash];
        if (s.Length == 0 || s.Contains('@')) return null;
        string host;
        var port = "";
        if (s.StartsWith('['))
        {
            var end = s.IndexOf(']');
            if (end < 2) return null;
            host = s[1..end];
            var rest = s[(end + 1)..];
            if (rest.Length > 0 && !rest.StartsWith(':')) return null;
            if (rest.Length > 0) port = rest[1..];
        }
        else
        {
            var i = s.LastIndexOf(':');
            host = i < 0 ? s : s[..i];
            if (i >= 0) port = s[(i + 1)..];
            if (host.Contains(':')) return null;
        }
        if (host.Length == 0 || !host.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '-' or ':')) return null;
        if (port.Length > 0 && (!int.TryParse(port, out var p) || p < 1 || p > 65535 || port.StartsWith('+'))) return null;
        if (plain)
        {
            var loopback = host == "localhost" || (System.Net.IPAddress.TryParse(host, out var ip) && System.Net.IPAddress.IsLoopback(ip));
            if (!loopback || port.Length == 0) return null;
        }
        var shownHost = host.Contains(':') ? $"[{host}]" : host;
        return $"{(plain ? "ws" : "wss")}://{shownHost}{(port.Length > 0 ? ":" + port : "")}";
    }

    /// <summary>How the session's packets travel, for diagnostics (§11).</summary>
    public static string Path(PathKind kind, string detail)
    {
        var what = kind switch
        {
            PathKind.Direct => L.Get("Texts.Path.Direct"),
            PathKind.Punched => L.Get("Texts.Path.Punched"),
            PathKind.Relay => L.Get("Texts.Path.Relay"),
            PathKind.Tailscale => "Tailscale",
            PathKind.RelayWss => L.Get("Texts.Path.RelayWss"),
            PathKind.TailscaleDerp => L.Get("Texts.Path.TailscaleDerp"),
            _ => L.Get("Texts.Path.Unknown"),
        };
        return string.IsNullOrEmpty(detail) ? what : $"{what} · {detail}";
    }

    /// <summary>§4.4 Reconnecting, as the session window shows it.</summary>
    public static string Reconnecting(uint attempt, uint remainingMs) =>
        L.Format("Texts.Reconnecting", attempt, (remainingMs + 999) / 1000);

    public static string Stage(ConnectStage stage, string target) => stage switch
    {
        ConnectStage.Resolving => L.Format("Texts.Stage.Resolving", target),
        ConnectStage.Gathering => L.Get("Texts.Stage.Gathering"),
        ConnectStage.Checking => L.Get("Texts.Stage.Checking"),
        _ => L.Format("Texts.Stage.Verifying", target),
    };

    public static string Signal(SignalState state, string shortId, string detail) => state switch
    {
        SignalState.Online => string.IsNullOrEmpty(detail)
            ? L.Get("Texts.Signal.Online")
            : L.Format("Texts.Signal.OnlineDetail", detail),
        SignalState.Connecting => L.Get("Texts.Signal.Connecting"),
        SignalState.Error => L.Format("Texts.Signal.Error", detail),
        _ => L.Get("Texts.Signal.Off"),
    };

    public static string Ago(DateTimeOffset then, DateTimeOffset now)
    {
        var d = now - then;
        if (then.ToUnixTimeSeconds() <= 0) return L.Get("Texts.Ago.Never");
        if (d < TimeSpan.FromMinutes(1)) return L.Get("Texts.Ago.JustNow");
        if (d < TimeSpan.FromHours(1))
        {
            var m = (int)d.TotalMinutes;
            return m == 1 ? L.Format("Texts.Ago.Minute", m) : L.Format("Texts.Ago.Minutes", m);
        }
        if (d < TimeSpan.FromDays(1))
        {
            var h = (int)d.TotalHours;
            return h == 1 ? L.Format("Texts.Ago.Hour", h) : L.Format("Texts.Ago.Hours", h);
        }
        if (d < TimeSpan.FromDays(30))
        {
            var days = (int)d.TotalDays;
            return days == 1 ? L.Format("Texts.Ago.Day", days) : L.Format("Texts.Ago.Days", days);
        }
        return then.ToLocalTime().ToString("yyyy-MM-dd");
    }

    public static string Duration(TimeSpan d) =>
        d.TotalHours >= 1 ? $"{(int)d.TotalHours}:{d.Minutes:00}:{d.Seconds:00}" : $"{d.Minutes:00}:{d.Seconds:00}";

    public static string Capture(CaptureState state, bool inputGranted) => !inputGranted
        ? L.Get("Texts.Capture.ViewOnly")
        : state switch
        {
            CaptureState.Active => L.Get("Texts.Capture.Active"),
            CaptureState.Degraded => L.Get("Texts.Capture.Degraded"),
            CaptureState.Unavailable => L.Get("Texts.Capture.Unavailable"),
            _ => L.Get("Texts.Capture.Inactive"),
        };

    public static string Mode(StreamMode m) => m switch
    {
        StreamMode.Office => L.Get("Texts.Mode.Office"),
        StreamMode.Game => L.Get("Texts.Mode.Game"),
        _ => L.Get("Texts.Mode.Balanced"),
    };

    public static string Codec(uint codec) => codec switch
    {
        1 => "H.264",
        2 => "HEVC",
        3 => "AV1",
        _ => "—",
    };
}
