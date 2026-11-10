using NexaDesk.Proto;

namespace NexaDesk.Services;

/// <summary>Runs work on the UI thread. Callbacks from Rust threads go through this (§10.1).</summary>
public interface IUiDispatcher
{
    void Post(Action action);
}

/// <summary>The viewer library (libnexa_client). Events arrive on the UI thread.</summary>
public interface IClientCore : IDisposable
{
    event Action<ClientEvent>? EventReceived;
    ISession Connect(ConnectRequest request);
}

/// <summary>One session handle. Every call returns at once; status codes are NativeMethods.NEXA_*.</summary>
public interface ISession
{
    ulong Tag { get; }
    bool IsClosed { get; }
    int AttachView(nint nsView, uint widthPx, uint heightPx, float scale);
    int ResizeView(uint widthPx, uint heightPx, float scale);
    int DetachView();
    int SetInputActive(bool active);
    int Send(SessionCommand command);
    string LastError { get; }
    void Close();
}

/// <summary>The IPC connection to the local agent. Events arrive on the UI thread.</summary>
public interface IAgentLink
{
    bool IsConnected { get; }
    event Action<AgentMessage>? MessageReceived;
    /// <summary>The connection ended; the argument says why, if known.</summary>
    event Action<string?>? Disconnected;
    /// <summary>Connects and says hello. Returns null once attached, or why it could not.</summary>
    Task<string?> ConnectAsync(CancellationToken ct);
    void Send(UiRequest request);
    void Close();
}

public sealed record AgentExit(int Code, string LastOutput);

/// <summary>The nexa-agent child process.</summary>
public interface IAgentProcess
{
    bool IsRunning { get; }
    /// <summary>Raised on the UI thread when a process this started exits.</summary>
    event Action<AgentExit>? Exited;
    /// <summary>Null on success, else why it could not start. An empty signalServer means not reachable by ID.</summary>
    /// <param name="announceOnLan">Announce this Mac to viewers on the LAN over mDNS (§4.11).</param>
    /// <param name="web">Where browsers connect (<c>--web</c>), or null to keep browser access off.</param>
    string? Start(string listen, string deviceName, string signalServer, bool announceOnLan, string? web = null);
    void Kill();
}

public sealed record PermissionState(bool ScreenRecording, bool Accessibility);

public enum PermissionKind { ScreenRecording, Accessibility }

/// <summary>macOS privacy permissions. Checking never prompts; only Request does.</summary>
public interface IPermissionProbe
{
    PermissionState Check();
    void Request(PermissionKind kind);
    void OpenSettings(PermissionKind kind);
}

public sealed record RecentDevice(string Address, string Name, string HostKey, long LastConnected);

public interface IRecentStore
{
    IReadOnlyList<RecentDevice> Load();
    void Save(IReadOnlyList<RecentDevice> devices);
}

public interface ILocalAddresses
{
    /// <summary>This machine's addresses worth listening on, best first.</summary>
    IReadOnlyList<string> Candidates();
}

/// <summary>Preferences kept in settings.json. SignalServer is already normalized, or empty.
/// AnnounceOnLan: whether the host is announced to viewers on the LAN (§4.11).
/// Language: a supported language code, or empty to follow macOS; applies at the next start.</summary>
public sealed record AppSettings(string SignalServer = "", bool AnnounceOnLan = true, string Language = "",
    bool AllowBrowsers = false);

public interface ISettingsStore
{
    AppSettings Load();
    void Save(AppSettings settings);
}

public sealed class MemorySettings(AppSettings? initial = null) : ISettingsStore
{
    public AppSettings Current { get; private set; } = initial ?? new AppSettings();
    public AppSettings Load() => Current;
    public void Save(AppSettings settings) => Current = settings;
}

/// <summary>Keyboard rules (§11.2) through libnexa_client's nexa_keymap_* calls. This Mac is the local side.</summary>
public interface IKeymapService
{
    KeymapView Rules(RemoteOs remote);
    /// <summary>Null when saved, else why not (nothing is written then).</summary>
    string? Save(RemoteOs remote, IReadOnlyList<KeymapRule> rules);
    /// <summary>Runs the real rewriter on a chord with <paramref name="rules"/> in place of the file. Sends nothing.</summary>
    KeyPreview Preview(RemoteOs remote, string chord, KeyContext ctx, IReadOnlyList<KeymapRule> rules);
}

/// <summary>SMAppService.mainApp's status (macOS 13+); Unsupported elsewhere.</summary>
public enum LoginItemStatus { NotRegistered = 0, Enabled = 1, RequiresApproval = 2, NotFound = 3, Unsupported = 4 }

/// <summary>"登录时启动". Only the real app registers anything; tests use a fake.</summary>
public interface ILoginItem
{
    LoginItemStatus Status { get; }
    /// <summary>Null on success, else why not.</summary>
    string? Register();
    string? Unregister();
    /// <summary>System Settings → General → Login Items.</summary>
    void OpenSettings();
}
