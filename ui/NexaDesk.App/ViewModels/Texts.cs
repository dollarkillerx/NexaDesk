using System.Text;
using System.Text.RegularExpressions;
using NexaDesk.Proto;

namespace NexaDesk.ViewModels;

public sealed record ErrorText(string Title, string Detail);

/// <summary>Everything the UI says, in one place. The UI is Chinese.</summary>
public static partial class Texts
{
    public const int DefaultPort = 47800;

    /// <summary>What a Disconnected reason means for the person, and what to do about it.</summary>
    public static ErrorText Disconnect(DisconnectReason reason) => reason switch
    {
        DisconnectReason.ClosedLocally => new("已断开连接", "你结束了这次会话。"),
        DisconnectReason.HostEnded => new("对方结束了会话", "被控端断开了连接，或停止了允许控制。"),
        DisconnectReason.Replaced => new("会话已被接管", "另一台设备连接了对方的电脑，对方同意了接管。"),
        DisconnectReason.PipelineFailed => new("对方无法共享屏幕", "被控端的屏幕采集或视频编码出错。请让对方确认已授予 NexaDesk “屏幕录制”权限后重试。"),
        DisconnectReason.Network => new("网络连接失败", "无法连到对方。请检查地址和端口、对方是否已开启“允许别人控制本机”，以及两边的防火墙。"),
        DisconnectReason.PinMismatch => new("对方的身份与已保存的配对不一致", "可能是对方重装了 NexaDesk，也可能有人冒充。请先与对方核实，再用一次性码重新配对。"),
        DisconnectReason.NotPaired => new("尚未与这台设备配对", "对方没有保存与本机的配对。请输入对方屏幕上显示的一次性码重新连接。"),
        DisconnectReason.AuthFailed => new("一次性码不正确", "请核对对方屏幕上显示的 8 位代码。连续输错 5 次后代码会自动更换。"),
        DisconnectReason.RateLimited => new("尝试次数过多", "为防止猜码，对方暂时拒绝了新的尝试。请稍等一分钟再试。"),
        DisconnectReason.ConsentDenied => new("对方没有同意连接", "对方拒绝了请求、没有在限定时间内回应，或者对方电脑前当前没有可以确认的人。"),
        DisconnectReason.NoCommonCodec => new("没有双方都支持的视频编码", "两台设备的硬件编解码能力不匹配，无法建立视频流。"),
        DisconnectReason.VersionMismatch => new("NexaDesk 版本不兼容", "请把两边的 NexaDesk 更新到同一版本。"),
        DisconnectReason.TimedOut => new("连接超时", "对方没有及时响应。请确认地址正确、对方在线，然后重试。"),
        DisconnectReason.InvalidRequest => new("连接请求无效", "地址或一次性码的格式不对。"),
        DisconnectReason.Internal => new("NexaDesk 内部错误", "发生了意外错误。请重试；如果反复出现，请重启 NexaDesk。"),
        DisconnectReason.Offline => new("对方不在线", "这个设备 ID 现在没有连到信令服务器。请确认对方已开启“允许别人控制本机”，并且两边设置的是同一台信令服务器。"),
        DisconnectReason.NoPath => new("找不到能走通的网络路径", "直连、打洞和中继都没有成功。可能是防火墙拦截了 UDP，或者信令服务器没有配置中继。"),
        DisconnectReason.SignalServer => new("无法使用信令服务器", "连不上信令服务器，或服务器拒绝了请求。请检查设置里的服务器地址和网络。"),
        _ => new("连接已断开", "连接意外中断。"),
    };

    public static string Permissions(Permissions p)
    {
        var parts = new List<string>();
        if (p.View) parts.Add("查看屏幕");
        if (p.Input) parts.Add("键盘鼠标");
        if (p.Clipboard) parts.Add("剪贴板");
        if (p.File) parts.Add("文件");
        if (p.Audio) parts.Add("声音");
        return parts.Count == 0 ? "无" : string.Join("、", parts);
    }

    public static string Auth(AuthKind kind) => kind switch
    {
        AuthKind.Paired => "已配对的设备",
        AuthKind.Code => "输入了正确的一次性码",
        AuthKind.Password => "输入了正确的口令",
        _ => "未知方式",
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

    /// <summary>Where a direct connection goes, from its address.</summary>
    public static string PathOf(string address)
    {
        var host = address;
        var i = address.LastIndexOf(':');
        if (i > 0 && !address.EndsWith(']')) host = address[..i];
        host = host.Trim('[', ']');
        if (!System.Net.IPAddress.TryParse(host, out var ip)) return $"直连 · {address}";
        if (System.Net.IPAddress.IsLoopback(ip)) return $"直连（本机回环）· {address}";
        var b = ip.GetAddressBytes();
        if (b.Length == 4)
        {
            if (b[0] == 100 && b[1] >= 64 && b[1] < 128) return $"直连（Tailscale）· {address}";
            if (b[0] == 10 || (b[0] == 172 && b[1] >= 16 && b[1] < 32) || (b[0] == 192 && b[1] == 168))
                return $"直连（局域网）· {address}";
            return $"直连（公网）· {address}";
        }
        return ip.IsIPv6LinkLocal || (b[0] & 0xFE) == 0xFC ? $"直连（局域网）· {address}" : $"直连（公网）· {address}";
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
            PathKind.Direct => "直连",
            PathKind.Punched => "打洞",
            PathKind.Relay => "中继",
            PathKind.Tailscale => "Tailscale",
            _ => "正在建立",
        };
        return string.IsNullOrEmpty(detail) ? what : $"{what} · {detail}";
    }

    public static string Stage(ConnectStage stage, string target) => stage switch
    {
        ConnectStage.Resolving => $"正在通过信令服务器查找 {target}…",
        ConnectStage.Gathering => "正在收集本机网络地址…",
        ConnectStage.Checking => "正在尝试直连和打洞…",
        _ => $"已连上 {target}，正在验证并等待对方确认…",
    };

    public static string Signal(SignalState state, string shortId, string detail) => state switch
    {
        SignalState.Online => string.IsNullOrEmpty(detail)
            ? "已连接信令服务器：对方可以用上面的 ID 连接本机"
            : $"已连接信令服务器（{detail}）",
        SignalState.Connecting => "正在连接信令服务器…",
        SignalState.Error => $"无法连接信令服务器，正在重试：{detail}",
        _ => "未设置信令服务器：对方只能用地址连接本机",
    };

    public static string Ago(DateTimeOffset then, DateTimeOffset now)
    {
        var d = now - then;
        if (then.ToUnixTimeSeconds() <= 0) return "从未";
        if (d < TimeSpan.FromMinutes(1)) return "刚刚";
        if (d < TimeSpan.FromHours(1)) return $"{(int)d.TotalMinutes} 分钟前";
        if (d < TimeSpan.FromDays(1)) return $"{(int)d.TotalHours} 小时前";
        if (d < TimeSpan.FromDays(30)) return $"{(int)d.TotalDays} 天前";
        return then.ToLocalTime().ToString("yyyy-MM-dd");
    }

    public static string Duration(TimeSpan d) =>
        d.TotalHours >= 1 ? $"{(int)d.TotalHours}:{d.Minutes:00}:{d.Seconds:00}" : $"{d.Minutes:00}:{d.Seconds:00}";

    public static string Capture(CaptureState state, bool inputGranted) => !inputGranted
        ? "仅查看：对方没有允许键盘鼠标"
        : state switch
        {
            CaptureState.Active => "键盘已捕获：按键都发往对方",
            CaptureState.Degraded => "键盘部分捕获：缺少辅助功能权限，Cmd+Tab 等系统快捷键仍在本机生效",
            CaptureState.Unavailable => "无法捕获键盘：请授予辅助功能权限",
            _ => "键盘未捕获：点击画面开始控制",
        };

    public static string Mode(StreamMode m) => m switch
    {
        StreamMode.Office => "办公",
        StreamMode.Game => "游戏",
        _ => "均衡",
    };

    public static string Codec(uint codec) => codec switch
    {
        1 => "H.264",
        2 => "HEVC",
        3 => "AV1",
        _ => "—",
    };
}
