using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>A choice in the language menu; an empty code follows macOS.</summary>
public sealed record LanguageOption(string Code, string Name)
{
    public override string ToString() => Name;
}

/// <summary>
/// The optional self-hosted signaling server (§4.3), where device IDs are looked up. Empty, the
/// default, means addresses only: the LAN directly, and Tailscale across networks.
/// </summary>
public sealed partial class SettingsViewModel : ObservableObject
{
    readonly ISettingsStore _store;
    readonly ISavedPasswords? _passwords;

    readonly IServerDirectory? _directory;
    readonly IServerProbe _probe;

    readonly IElevation _elevation;
    readonly ISystemService _service;

    public SettingsViewModel(ISettingsStore store, ISavedPasswords? passwords = null, IServerDirectory? directory = null,
        IServerProbe? probe = null, IElevation? elevation = null, ISystemService? systemService = null)
    {
        _store = store;
        _elevation = elevation ?? new NoElevation();
        _service = systemService ?? new NoSystemService();
        _passwords = passwords;
        _directory = directory;
        _probe = probe ?? new NoProbe();
        var loaded = store.Load();
        _savedServer = loaded.SignalServer;
        _mode = loaded.UseServerList && directory != null ? ServerMode.Published
            : _savedServer.Length > 0 ? ServerMode.Own : ServerMode.None;
        // With no server or no Keychain there is nothing to wait for.
        if (_passwords == null || _savedServer.Length == 0) _secretLoaded.TrySetResult();
        _serverInput = _savedServer;
        _announceOnLan = loaded.AnnounceOnLan;
        _allowBrowsers = loaded.AllowBrowsers;
        _allowAgents = loaded.AllowAgents;
        _runAsAdministrator = loaded.RunAsAdministrator;
        _startupLanguage = loaded.Language;
        LanguageOptions = [new LanguageOption("", L.Get("Settings.LanguageSystem")), .. L.Languages.Select(l => new LanguageOption(l.Code, l.Name))];
        _selectedLanguage = LanguageOptions.FirstOrDefault(o => o.Code == loaded.Language) ?? LanguageOptions[0];
    }

    readonly string _startupLanguage;

    /// <summary>"Follow macOS", then each language in its own name.</summary>
    public IReadOnlyList<LanguageOption> LanguageOptions { get; }

    /// <summary>Saved at once; the text changes at the next start.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(LanguageRestartNeeded))]
    private LanguageOption _selectedLanguage;

    public bool LanguageRestartNeeded => SelectedLanguage.Code != _startupLanguage;

    partial void OnSelectedLanguageChanged(LanguageOption value) => Persist();

    /// <summary>Saves what this window shows; ResumeHosting, which the host keeps, is left as it is.</summary>
    void Persist() => _store.Save(_store.Load() with
    {
        SignalServer = SavedServer,
        AnnounceOnLan = AnnounceOnLan,
        Language = SelectedLanguage?.Code ?? "",
        AllowBrowsers = AllowBrowsers,
        UseServerList = UseServerList,
        AllowAgents = AllowAgents,
        RunAsAdministrator = RunAsAdministrator,
    });

    /// <summary>
    /// Which server the person uses: none (addresses, the LAN and Tailscale only), one from the list the
    /// project publishes, or their own. Choosing the published list is also what asks GitHub for it: in
    /// any other mode the app never does. The choice is saved at once.
    /// </summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsModeNone), nameof(IsModePublished), nameof(IsModeOwn), nameof(UseServerList))]
    private ServerMode _mode;

    public bool IsModeNone { get => Mode == ServerMode.None; set { if (value) Mode = ServerMode.None; } }
    public bool IsModePublished { get => Mode == ServerMode.Published; set { if (value) Mode = ServerMode.Published; } }
    public bool IsModeOwn { get => Mode == ServerMode.Own; set { if (value) Mode = ServerMode.Own; } }

    /// <summary>Whether the published list is what the person chose (saved in settings).</summary>
    public bool UseServerList => Mode == ServerMode.Published;

    /// <summary>False when this build has no list to read.</summary>
    public bool CanUseServerList => _directory != null;

    public ObservableCollection<ListedServer> ListedServers { get; } = [];

    /// <summary>The published server in use. Picking one uses it at once, with no Save.</summary>
    [ObservableProperty]
    private ListedServer? _selectedListed;

    /// <summary>Loading, none published, or how many there are; null outside the published mode.</summary>
    [ObservableProperty]
    private string? _listStatus;

    CancellationTokenSource? _listing;
    bool _preselecting;

    partial void OnModeChanged(ServerMode value) => _ = ApplyModeAsync(value);

    async Task ApplyModeAsync(ServerMode mode)
    {
        Persist();
        // Giving servers up is final: no server, and its key out of the Keychain.
        if (mode == ServerMode.None) await ApplyAsync("", "");
        await RefreshListAsync();
    }

    partial void OnSelectedListedChanged(ListedServer? value)
    {
        if (value == null || _preselecting || Mode != ServerMode.Published) return;
        _ = ApplyAsync(value.Entry.Endpoint, value.Entry.Key);
    }

    /// <summary>Fetches the published list (in the published mode only) and shows it. Never throws.</summary>
    public async Task RefreshListAsync()
    {
        _listing?.Cancel();
        ListedServers.Clear();
        SelectedListed = null;
        if (Mode != ServerMode.Published || _directory == null)
        {
            ListStatus = null;
            return;
        }
        var mine = _listing = new CancellationTokenSource();
        ListStatus = L.Get("Main.ServerListLoading");
        var found = await _directory.LoadAsync(mine.Token);
        if (mine.IsCancellationRequested) return;
        foreach (var e in found) ListedServers.Add(new ListedServer(e));
        ListStatus = found.Count == 0 ? L.Get("Main.ServerListEmpty") : L.Format("Main.ServerListCount", found.Count);
        // The server already in use shows as chosen, without choosing it again.
        _preselecting = true;
        SelectedListed = ListedServers.FirstOrDefault(e => e.Entry.Endpoint == SavedServer);
        _preselecting = false;
        // How far away each is, and whether it answers; they all load at once.
        var items = ListedServers.ToList();
        await Task.WhenAll(items.Select(i => i.TestAsync(_probe, mine.Token)));
    }

    // ---- "Test" for a server typed by hand ----

    /// <summary>What the last test of the typed server found; null before one.</summary>
    [ObservableProperty]
    private string? _testResult;

    bool CanTest() => ServerHint == null && TypedServer is { Length: > 0 };

    [RelayCommand(CanExecute = nameof(CanTest))]
    private async Task TestAsync()
    {
        var server = TypedServer;
        if (string.IsNullOrEmpty(server)) return;
        TestResult = L.Get("Main.ProbeTesting");
        TestResult = ListedServer.Describe(await _probe.ProbeAsync(server));
    }

    partial void OnServerInputChanged(string value)
    {
        TestResult = null;
        TestCommand.NotifyCanExecuteChanged();
    }

    /// <summary>
    /// Whether web browsers may connect (crates/nexa-web): the agent then also serves a page
    /// on port 47801 of the addresses it listens on. Off by default. Saved at once; applies
    /// on the next start.
    /// </summary>
    [ObservableProperty]
    private bool _allowBrowsers;

    /// <summary>Raised after AllowBrowsers is saved.</summary>
    public event Action<bool>? BrowsersChanged;

    partial void OnAllowBrowsersChanged(bool value)
    {
        Persist();
        BrowsersChanged?.Invoke(value);
    }

    /// <summary>
    /// Whether AI agents (nexa-mcp, docs/MCP.md §4.2) may ask to connect. Each one is still put to the
    /// person, and the banner says it is an AI. Off by default. Saved at once and applies at once: the
    /// running agent is told, no restart needed.
    /// </summary>
    [ObservableProperty]
    private bool _allowAgents;

    /// <summary>
    /// Windows: the "Run as administrator" option is offered. The NexaDesk service does what it
    /// did and more (a SYSTEM host can use elevated windows, UAC prompts and the lock screen),
    /// so where the service is there, it shows only while still switched on from before, to
    /// be seen and switched off; <see cref="RefreshSystemService"/> moves it over by itself.
    /// </summary>
    public bool ShowRunAsAdministrator => _elevation.IsSupported && (!_service.IsSupported || RunAsAdministrator);

    /// <summary>
    /// Windows only: run NexaDesk as administrator, so the host can click and type into programs
    /// that run as administrator (<see cref="IElevation"/>). Switched on, it restarts NexaDesk
    /// elevated at once, which asks Windows (the UAC prompt); a no leaves it off. Opened by hand
    /// later, NexaDesk asks again; started at login it does not, as nobody may be there to answer.
    /// Switched off, it applies at the next start.
    /// </summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(RunAsAdministratorStatus), nameof(ShowRunAsAdministrator))]
    private bool _runAsAdministrator;

    /// <summary>Why turning it on did not work (the UAC prompt was declined, say); null otherwise.</summary>
    [ObservableProperty] private string? _runAsAdministratorError;

    /// <summary>Whether this copy runs elevated, and what the setting will do.</summary>
    public string? RunAsAdministratorStatus =>
        !_elevation.IsSupported ? null
        : _elevation.IsElevated ? L.Get(RunAsAdministrator ? "Main.RunAsAdminActive" : "Main.RunAsAdminOffLater")
        : RunAsAdministrator ? L.Get("Main.RunAsAdminNotYet")
        : null;

    /// <summary>The elevated copy has started: this one should quit for it.</summary>
    public event Action? QuitForElevatedCopy;

    bool _reverting;

    partial void OnRunAsAdministratorChanged(bool value)
    {
        Persist();
        if (_reverting) return;
        RunAsAdministratorError = null;
        if (!value || !_elevation.IsSupported || _elevation.IsElevated) return;
        if (_elevation.StartElevatedCopy() is { } why)
        {
            RunAsAdministratorError = why;
            _reverting = true;
            RunAsAdministrator = false;
            _reverting = false;
            return;
        }
        QuitForElevatedCopy?.Invoke();
    }

    // ---- The NexaDesk service (Windows, docs/WINDOWS_SERVICE.md §6) ----

    /// <summary>Windows, with the service beside the app.</summary>
    public bool ShowSystemService => _service.IsSupported;

    /// <summary>What is installed, as last looked at.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(SystemServiceStatus), nameof(SystemServiceNeedsUpdate))]
    private SystemServiceState _systemServiceState = SystemServiceState.NotInstalled;

    /// <summary>
    /// The checkbox: installed or not. Ticking it installs the service and unticking removes it,
    /// both asking Windows for administrator rights (the UAC prompt); a no, or a failure, puts the
    /// box back. Not a saved setting: the installed service is the setting.
    /// </summary>
    [ObservableProperty] private bool _useSystemService;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(SystemServiceStatus), nameof(CanChangeSystemService))]
    [NotifyCanExecuteChangedFor(nameof(UpdateSystemServiceCommand))]
    private bool _isChangingSystemService;

    /// <summary>Why installing, updating or removing did not work; null otherwise.</summary>
    [ObservableProperty] private string? _systemServiceError;

    public bool CanChangeSystemService => !IsChangingSystemService;

    public bool SystemServiceNeedsUpdate => SystemServiceState == SystemServiceState.NeedsUpdate;

    public string? SystemServiceStatus => IsChangingSystemService ? L.Get("Main.SystemServiceWorking")
        : SystemServiceState switch
        {
            SystemServiceState.Running => L.Get("Main.SystemServiceRunning"),
            SystemServiceState.Stopped => L.Get("Main.SystemServiceStopped"),
            SystemServiceState.NeedsUpdate => L.Get("Main.SystemServiceNeedsUpdate"),
            _ => null,
        };

    /// <summary>Raised once the service was installed, updated or removed: the host should be
    /// started again, through the service or not.</summary>
    public event Action? SystemServiceChanged;

    bool _syncingService;

    void ShowServiceState(SystemServiceState state)
    {
        SystemServiceState = state;
        _syncingService = true;
        UseSystemService = state != SystemServiceState.NotInstalled;
        _syncingService = false;
    }

    /// <summary>
    /// Looks at the service (off the UI thread: it reads both programs). A copy running as
    /// administrator needs no prompt to install, so it updates an outdated service at once,
    /// and moves "Run as administrator", if still on from before, over to the service: once
    /// that runs, the option is switched off and NexaDesk stops asking for elevation.
    /// </summary>
    public async Task RefreshSystemService()
    {
        if (!_service.IsSupported) return;
        ShowServiceState(await Task.Run(_service.Check));
        if (!_elevation.IsElevated) return;
        var moveOver = RunAsAdministrator && SystemServiceState != SystemServiceState.Running;
        if (SystemServiceState == SystemServiceState.NeedsUpdate || moveOver)
            await ChangeSystemService(true);
        if (RunAsAdministrator && SystemServiceState == SystemServiceState.Running)
            RunAsAdministrator = false;
    }

    partial void OnUseSystemServiceChanged(bool value)
    {
        if (_syncingService) return;
        _ = ChangeSystemService(value);
    }

    bool CanUpdateSystemService() => !IsChangingSystemService;

    [RelayCommand(CanExecute = nameof(CanUpdateSystemService))]
    private Task UpdateSystemService() => ChangeSystemService(true);

    async Task ChangeSystemService(bool install)
    {
        IsChangingSystemService = true;
        SystemServiceError = null;
        var why = install ? await _service.Install() : await _service.Uninstall();
        var state = await Task.Run(_service.Check);
        IsChangingSystemService = false;
        ShowServiceState(state);
        if (why != null)
        {
            SystemServiceError = why;
            return;
        }
        SystemServiceChanged?.Invoke();
    }

    /// <summary>Raised after AllowAgents is saved.</summary>
    public event Action<bool>? AgentsChanged;

    partial void OnAllowAgentsChanged(bool value)
    {
        Persist();
        AgentsChanged?.Invoke(value);
    }

    /// <summary>Whether this Mac's host is announced on the LAN (§4.11). Saved at once; applies on the next start.</summary>
    [ObservableProperty]
    private bool _announceOnLan;

    /// <summary>Raised after AnnounceOnLan is saved.</summary>
    public event Action<bool>? AnnounceChanged;

    partial void OnAnnounceOnLanChanged(bool value)
    {
        Persist();
        AnnounceChanged?.Invoke(value);
    }

    /// <summary>What is in effect: normalized, or empty.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasServer), nameof(ServerStatus))]
    private string _savedServer;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ServerHint), nameof(SecretHint), nameof(IsDirty))]
    [NotifyCanExecuteChangedFor(nameof(SaveCommand))]
    private string _serverInput;

    /// <summary>Raised after a change is saved.</summary>
    public event Action<string>? ServerChanged;

    /// <summary>
    /// The server's access key, in effect ("" with no server). A signaling server always asks for
    /// one, so a server is only saved with it. Kept in the login Keychain under the server it
    /// belongs to, never in settings.json; <see cref="LoadSecretAsync"/> reads it back at start.
    /// </summary>
    [ObservableProperty]
    private string _savedSecret = "";

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(SecretHint), nameof(IsDirty))]
    [NotifyCanExecuteChangedFor(nameof(SaveCommand))]
    private string _secretInput = "";

    /// <summary>Why the key could not be kept in the Keychain, after a Save.</summary>
    [ObservableProperty]
    private string? _secretError;

    static string AccountFor(string server) => "signal-access:" + server;

    readonly TaskCompletionSource _secretLoaded = new(TaskCreationOptions.RunContinuationsAsynchronously);

    /// <summary>Completes once the saved server's key has been read back (at once when there is none to read).
    /// What starts the agent waits for it, so the agent never starts without the key.</summary>
    public Task SecretLoaded => _secretLoaded.Task;

    /// <summary>Reads the saved server's key back from the Keychain. Run before anything uses it.
    /// Raises no <see cref="ServerChanged"/>: the server did not change, and the key is read when used.</summary>
    public async Task LoadSecretAsync()
    {
        if (_passwords == null || SavedServer.Length == 0) return;
        try
        {
            var kept = await _passwords.LoadAsync(AccountFor(SavedServer)) ?? "";
            // What the person typed while the Keychain answered wins.
            if (SecretInput.Length == 0 && SavedSecret.Length == 0) SecretInput = kept;
            SavedSecret = kept;
        }
        finally
        {
            _secretLoaded.TrySetResult();
        }
    }

    public bool HasServer => SavedServer.Length > 0;

    string TypedSecret => SecretInput.Trim();

    string? TypedServer => ServerInput.Trim().Length == 0 ? "" : Texts.NormalizeServer(ServerInput);

    public bool IsDirty => TypedServer != SavedServer || (TypedServer?.Length > 0 && TypedSecret != SavedSecret);

    /// <summary>A server needs its key: it cannot be saved without one.</summary>
    public string? SecretHint =>
        ServerInput.Trim().Length > 0 && TypedSecret.Length == 0 ? L.Get("Main.SecretHint") : null;

    public string? ServerHint =>
        ServerInput.Trim().Length > 0 && Texts.NormalizeServer(ServerInput) == null
            ? L.Get("Main.ServerHint")
            : null;

    public string ServerStatus => HasServer
        ? L.Format("Main.ServerInUse", SavedServer)
        : L.Get("Main.ServerNotUsed");

    bool CanSave() => IsDirty && ServerHint == null && SecretHint == null;

    [RelayCommand(CanExecute = nameof(CanSave))]
    private async Task SaveAsync()
    {
        var value = TypedServer;
        if (value == null) return;
        await ApplyAsync(value, value.Length == 0 ? "" : TypedSecret);
    }

    /// <summary>Makes <paramref name="value"/> and its key the server in use: the Keychain first, so a
    /// server is never kept without the key it needs; then settings, then everything that listens.</summary>
    async Task ApplyAsync(string value, string secret)
    {
        if (value == SavedServer && secret == SavedSecret) return;
        SecretError = null;
        if (value.Length > 0 && _passwords != null)
        {
            var why = await _passwords.SaveAsync(AccountFor(value), secret);
            if (why != null)
            {
                SecretError = L.Format("Main.SecretNotKept", why);
                return;
            }
        }
        // A server given up, or replaced by another, takes its key with it.
        if (SavedServer.Length > 0 && SavedServer != value && _passwords != null)
            await _passwords.DeleteAsync(AccountFor(SavedServer));
        SavedServer = value;
        SavedSecret = secret;
        ServerInput = value;
        SecretInput = secret;
        Persist();
        ServerChanged?.Invoke(value);
    }
}

/// <summary>How the person gets a signaling server.</summary>
public enum ServerMode { None, Published, Own }

/// <summary>A server of the published list, and how it tested.</summary>
public sealed partial class ListedServer(ServerEntry entry) : ObservableObject
{
    public ServerEntry Entry { get; } = entry;

    public string Endpoint => Entry.Endpoint;

    /// <summary>"Available, 38 ms", "cannot connect", ...; empty until it has been looked at.</summary>
    [ObservableProperty]
    private string _health = "";

    public async Task TestAsync(IServerProbe probe, CancellationToken ct)
    {
        Health = L.Get("Main.ProbeTesting");
        var result = await probe.ProbeAsync(Entry.Endpoint, ct);
        if (!ct.IsCancellationRequested) Health = Describe(result);
    }

    public static string Describe(ProbeResult r) => r switch
    {
        { Available: true, Milliseconds: { } ms } => L.Format("Main.ProbeOk", ms),
        { Problem: "notsignal" } => L.Get("Main.ProbeNotSignal"),
        _ => L.Get("Main.ProbeUnreachable"),
    };
}

/// <summary>Where nothing can be tested (tests, a build without it).</summary>
sealed class NoProbe : IServerProbe
{
    public Task<ProbeResult> ProbeAsync(string endpoint, CancellationToken ct = default) =>
        Task.FromResult(new ProbeResult(false, null, "unreachable"));
}
