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
    /// <summary>The host's command line, <paramref name="cols"/> × <paramref name="rows"/> cells to start
    /// with; disposing it ends the shell. Null, with the status, if it could not open.</summary>
    (NexaDesk.Terminal.ITerminalBackend? Terminal, int Status) StartTerminal(int cols, int rows);
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
    /// <summary>Sets the fixed password (null clears it) with the agent, which keeps only an
    /// Argon2id verifier in the Keychain. Null on success, else why not. Takes a moment.</summary>
    Task<string?> SetPassword(string? password) => Task.FromResult<string?>(null);
    /// <summary>Gives this Mac a new device ID from the next start on; the old one stops
    /// working. Null on success, else why not.</summary>
    Task<string?> ResetDeviceId() => Task.FromResult<string?>(null);
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

/// <summary>
/// A host connected to before. <paramref name="Label"/> is the person's own name for it
/// (empty: the host's); <paramref name="HasPassword"/> says its fixed password is in the
/// Keychain (see <see cref="ISavedPasswords"/>), never the password itself.
/// </summary>
public sealed record RecentDevice(string Address, string Name, string HostKey, long LastConnected,
    string Label = "", bool HasPassword = false)
{
    /// <summary>What the Keychain item is filed under: the host's key once known, else its address.</summary>
    public string PasswordAccount => HostKey.Length > 0 ? HostKey : Address;
}

/// <summary>Saved connections' fixed passwords, in the login Keychain. Nothing else keeps them.
/// The UI uses the Async forms: the Keychain may ask the person (an unlock, or "allow access"
/// after an update) and waits for the answer, which must not freeze the window.</summary>
public interface ISavedPasswords
{
    /// <summary>Null when kept, else why not.</summary>
    string? Save(string account, string password);
    /// <summary>The kept password, or null when there is none or it cannot be read.</summary>
    string? Load(string account);
    void Delete(string account);

    Task<string?> SaveAsync(string account, string password) => Task.FromResult(Save(account, password));
    Task<string?> LoadAsync(string account) => Task.FromResult(Load(account));
    Task DeleteAsync(string account)
    {
        Delete(account);
        return Task.CompletedTask;
    }
}

public sealed class MemorySavedPasswords : ISavedPasswords
{
    public Dictionary<string, string> Kept { get; } = new();
    public string? Refusal { get; set; }
    public string? Save(string account, string password)
    {
        if (Refusal != null) return Refusal;
        Kept[account] = password;
        return null;
    }
    public string? Load(string account) => Kept.GetValueOrDefault(account);
    public void Delete(string account) => Kept.Remove(account);
}

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
/// Language: a supported language code, or empty to follow macOS; applies at the next start.
/// ResumeHosting: remote control is turned on at start. On by default (a fresh install, or a
/// settings.json from before this field); the person stopping it switches it off until they
/// start it again, and quitting or restarting to update leaves it as it was.</summary>
public sealed record AppSettings(string SignalServer = "", bool AnnounceOnLan = true, string Language = "",
    bool AllowBrowsers = false, bool ResumeHosting = true);

/// <summary>The viewer's picture choices, remembered for the next session.</summary>
public sealed record ViewerPrefs(string Quality = "auto", string Resolution = "auto");

/// <summary>viewer-prefs.json: apart from settings.json, which the settings window saves whole.</summary>
public interface IViewerPrefs
{
    ViewerPrefs Load();
    void Save(ViewerPrefs prefs);
}

public sealed class MemoryViewerPrefs(ViewerPrefs? initial = null) : IViewerPrefs
{
    public ViewerPrefs Current { get; private set; } = initial ?? new ViewerPrefs();
    public ViewerPrefs Load() => Current;
    public void Save(ViewerPrefs prefs) => Current = prefs;
}

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
    /// <summary>Whether symbol keys type what this keyboard prints on them (sent as text) rather than going by position.</summary>
    bool SymbolsAsPrinted();
    /// <summary>Null when kept and applied (to a session running now too), else why not.</summary>
    string? SetSymbolsAsPrinted(bool on);
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
