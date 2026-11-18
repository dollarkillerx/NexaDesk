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

    public SettingsViewModel(ISettingsStore store, ISavedPasswords? passwords = null, IServerDirectory? directory = null,
        IServerProbe? probe = null)
    {
        _store = store;
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
