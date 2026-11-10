using Avalonia;
using Avalonia.Headless;
using NexaDesk.Native;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;

[assembly: AvaloniaTestApplication(typeof(NexaDesk.Tests.TestApp))]

namespace NexaDesk.Tests;

/// <summary>The real App (styles and theme), headless, drawn by Skia so frames can be saved.</summary>
public static class TestApp
{
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UseSkia()
            .UseHeadless(new AvaloniaHeadlessPlatformOptions { UseHeadlessDrawing = false });
}

public sealed class QueueDispatcher : IUiDispatcher
{
    readonly Queue<Action> _queue = new();
    readonly object _gate = new();
    public void Post(Action action) { lock (_gate) _queue.Enqueue(action); }
    public int Pending { get { lock (_gate) return _queue.Count; } }

    public void RunAll()
    {
        while (true)
        {
            Action a;
            lock (_gate)
            {
                if (_queue.Count == 0) return;
                a = _queue.Dequeue();
            }
            a();
        }
    }
}

public sealed class InlineDispatcher : IUiDispatcher
{
    public void Post(Action action) => action();
}

public sealed class FakeSession(ulong tag) : ISession
{
    public ulong Tag { get; } = tag;
    public bool IsClosed { get; private set; }
    public string LastError => "";
    public List<bool> InputActive { get; } = new();
    public List<SessionCommand> Commands { get; } = new();
    public List<(nint, uint, uint, float)> Attached { get; } = new();
    public int SetInputResult { get; set; }
    public int CloseCount { get; private set; }

    public int AttachView(nint nsView, uint widthPx, uint heightPx, float scale)
    {
        Attached.Add((nsView, widthPx, heightPx, scale));
        return 0;
    }

    public int ResizeView(uint widthPx, uint heightPx, float scale) => 0;
    public int DetachView() => 0;

    public int SetInputActive(bool active)
    {
        InputActive.Add(active);
        return active ? SetInputResult : 0;
    }

    public int Send(SessionCommand command)
    {
        Commands.Add(command);
        return 0;
    }

    public void Close()
    {
        CloseCount++;
        IsClosed = true;
    }
}

public sealed class FakeClient : IClientCore
{
    public List<ConnectRequest> Requests { get; } = new();
    public List<FakeSession> Sessions { get; } = new();
    public event Action<ClientEvent>? EventReceived;

    public ISession Connect(ConnectRequest request)
    {
        Requests.Add(request);
        var s = new FakeSession(request.Tag);
        Sessions.Add(s);
        return s;
    }

    public void Raise(ClientEvent e) => EventReceived?.Invoke(e);
    public void Dispose() { }
}

public sealed class FakeLink : IAgentLink
{
    public Queue<string?> ConnectResults { get; } = new();
    public string? DefaultConnectResult { get; set; } = "no agent";
    public List<UiRequest> Sent { get; } = new();
    public bool IsConnected { get; private set; }
    public int CloseCount { get; private set; }
    public event Action<AgentMessage>? MessageReceived;
    public event Action<string?>? Disconnected;

    public Task<string?> ConnectAsync(CancellationToken ct)
    {
        var r = ConnectResults.Count > 0 ? ConnectResults.Dequeue() : DefaultConnectResult;
        IsConnected = r == null;
        return Task.FromResult(r);
    }

    public void Send(UiRequest request) => Sent.Add(request);

    public void Close()
    {
        CloseCount++;
        IsConnected = false;
    }

    public void Raise(AgentMessage m) => MessageReceived?.Invoke(m);

    public void Drop(string? why = "gone")
    {
        IsConnected = false;
        Disconnected?.Invoke(why);
    }

    public T Last<T>() where T : UiRequest => Sent.OfType<T>().Last();
}

public sealed class FakeProcess : IAgentProcess
{
    public List<(string Listen, string Name)> Starts { get; } = new();
    public List<string> Servers { get; } = new();
    public string? StartResult { get; set; }
    public bool IsRunning { get; set; }
    public int Kills { get; private set; }
    public event Action<AgentExit>? Exited;

    public string? Start(string listen, string deviceName, string signalServer)
    {
        Starts.Add((listen, deviceName));
        Servers.Add(signalServer);
        IsRunning = StartResult == null;
        return StartResult;
    }

    public void Kill()
    {
        Kills++;
        IsRunning = false;
    }

    public void Exit(int code, string output)
    {
        IsRunning = false;
        Exited?.Invoke(new AgentExit(code, output));
    }
}

public sealed class FakePermissions : IPermissionProbe
{
    public PermissionState State { get; set; } = new(true, true);
    public List<PermissionKind> Requested { get; } = new();
    public List<PermissionKind> Opened { get; } = new();
    public PermissionState Check() => State;
    public void Request(PermissionKind kind) => Requested.Add(kind);
    public void OpenSettings(PermissionKind kind) => Opened.Add(kind);
}

public sealed class MemoryRecent(params RecentDevice[] initial) : IRecentStore
{
    public List<RecentDevice> Saved { get; private set; } = initial.ToList();
    public IReadOnlyList<RecentDevice> Load() => Saved;
    public void Save(IReadOnlyList<RecentDevice> devices) => Saved = devices.ToList();
}

public sealed class FakeAddresses(params string[] ips) : ILocalAddresses
{
    public IReadOnlyList<string> Candidates() => ips.Length == 0 ? ["192.168.1.20", "127.0.0.1"] : ips;
}

public sealed class RecordingWindows : IWindowService
{
    public List<SessionViewModel> Sessions { get; } = new();
    public List<ConsentViewModel> Consents { get; } = new();
    public List<BannerViewModel> Banners { get; } = new();
    public int BannerHides { get; private set; }
    public List<PermissionGuideViewModel> Guides { get; } = new();
    public BannerViewModel? Banner { get; private set; }

    public void ShowSession(SessionViewModel session) => Sessions.Add(session);
    public void ShowConsent(ConsentViewModel consent) => Consents.Add(consent);

    public void ShowBanner(BannerViewModel banner)
    {
        Banners.Add(banner);
        Banner = banner;
    }

    public void HideBanner()
    {
        BannerHides++;
        Banner = null;
    }

    public void ShowPermissionGuide(PermissionGuideViewModel guide) => Guides.Add(guide);
    public void ShowDiagnostics(DiagnosticsViewModel diagnostics) { }
    public List<KeyboardSettingsViewModel> KeyboardSettings { get; } = new();
    public void ShowKeyboardSettings(KeyboardSettingsViewModel settings) => KeyboardSettings.Add(settings);
}

/// <summary>Never touches SMAppService: records what the app asked for.</summary>
public sealed class FakeLoginItem : ILoginItem
{
    public LoginItemStatus Status { get; set; } = LoginItemStatus.NotRegistered;
    /// <summary>What a successful register leads to (approval may be needed).</summary>
    public LoginItemStatus AfterRegister { get; set; } = LoginItemStatus.Enabled;
    public string? Fail { get; set; }
    public int Registers { get; private set; }
    public int Unregisters { get; private set; }
    public int SettingsOpened { get; private set; }

    public string? Register()
    {
        Registers++;
        if (Fail != null) return Fail;
        Status = AfterRegister;
        return null;
    }

    public string? Unregister()
    {
        Unregisters++;
        if (Fail != null) return Fail;
        Status = LoginItemStatus.NotRegistered;
        return null;
    }

    public void OpenSettings() => SettingsOpened++;
}

/// <summary>
/// Keyboard rules in memory, with just enough rewriting to tell the view-model's
/// calls apart. The real rewriter behind the ABI is tested in Rust.
/// </summary>
public sealed class FakeKeymap : IKeymapService
{
    public Dictionary<RemoteOs, List<KeymapRule>> Saved { get; } = new()
    {
        [RemoteOs.Windows] =
        [
            new KeymapRule(true, "", "Cmd+Shift+Z", ["Ctrl+Y"], "", "", true),
            new KeymapRule(true, "", "Cmd+V", ["Ctrl+V"], "", "paste", true),
            new KeymapRule(true, "terminal", "Ctrl+C", ["Ctrl+C"], "", "", true),
            new KeymapRule(true, "", "Cmd+*", ["Ctrl+*"], "", "", true),
        ],
        [RemoteOs.Macos] = [new KeymapRule(true, "", "Cmd+V", ["Cmd+V"], "", "paste", true)],
    };
    public string FileError { get; set; } = "";
    public List<(RemoteOs, string, KeyContext, int)> Previews { get; } = new();
    public int Saves { get; private set; }

    public KeymapView Rules(RemoteOs remote) => new(Saved[remote].ToList(), FileError);

    static bool Valid(string chord) => chord.Split('+').All(p => p.Length > 0 && !p.Contains("Nope"));

    public string? Save(RemoteOs remote, IReadOnlyList<KeymapRule> rules)
    {
        if (rules.Any(r => !Valid(r.When) || r.Then.Any(t => !Valid(t)))) return "unknown key";
        Saves++;
        Saved[remote] = rules.ToList();
        return null;
    }

    public KeyPreview Preview(RemoteOs remote, string chord, KeyContext ctx, IReadOnlyList<KeymapRule> rules)
    {
        Previews.Add((remote, chord, ctx, rules.Count));
        if (!Valid(chord) || rules.Any(r => !Valid(r.When))) return new KeyPreview([], [], false, "unknown key in chord");
        var rule = rules.FirstOrDefault(r => r.Enabled && r.When == chord && (r.Ctx == "" || (r.Ctx == "terminal" && ctx == KeyContext.Terminal)));
        var sent = rule?.Then.ToList() ?? [chord.Replace("Cmd", "Ctrl")];
        var events = sent.SelectMany(s => s.Split('+').Select(k => new KeyOut(k, true))).ToList();
        return new KeyPreview(events, sent, rule?.Tag == "paste", "");
    }
}

public static class Samples
{
    public const string ViewerKey = "3f2a9c1b0d4e77aa12345678deadbeef00112233445566778899aabbccddeeff";
    public const string HostKey = "aa00bb11cc22dd33ee44ff5566778899aabbccddeeff00112233445566778899";

    public static readonly DateTimeOffset T0 = new(2026, 11, 10, 12, 0, 0, TimeSpan.Zero);

    public static AgentMessage.IncomingRequest Request(ulong id = 41, bool input = true, bool offerSave = true,
        bool takeover = false, bool inputAvailable = true, uint timeoutMs = 60_000) =>
        new(id, "王小明的 MacBook Pro", "6f1c2d3e4a5b6c7d8e9f0a1b2c3d4e5f", ViewerKey, "192.168.1.35:53122",
            AuthKind.Code, new Permissions(true, input, false), offerSave, false, false, takeover, timeoutMs, inputAvailable);

    public static HostSession Session(ulong id = 7, bool input = true) =>
        new(id, "王小明的 MacBook Pro", "6f1c2d3e4a5b6c7d8e9f0a1b2c3d4e5f", ViewerKey, "192.168.1.35:53122",
            new Permissions(true, input, true));

    public static AgentMessage.Status Status(HostSession? session = null, string code = "K7QM2XPA", bool input = true,
        string shortId = "", SignalState signal = SignalState.Off, string detail = "") =>
        new(1, "192.168.1.20:47800", code, "工作室 Mac mini", "0123456789abcdef0123456789abcdef", HostKey, session, input,
            shortId, signal, detail);

    public static ConnectedEvent Connected(ulong tag, bool input = true, bool clipboard = true) =>
        new(tag, HostKey, "工作室 Mac mini", new Permissions(true, input, clipboard), 2, true);
}
