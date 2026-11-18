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

    public SettingsViewModel(ISettingsStore store, ISavedPasswords? passwords = null, IServerDirectory? directory = null)
    {
        _store = store;
        _passwords = passwords;
        _directory = directory;
        var loaded = store.Load();
        _useServerList = loaded.UseServerList && directory != null;
        _savedServer = loaded.SignalServer;
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
    /// Offers the servers published for everyone (servers.json in the project's repository) to pick
    /// from. Off by default: while it is off the app never asks GitHub for the list. Picking one only
    /// fills in the two boxes; nothing is saved or used until Save, like a server typed by hand.
    /// </summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanUseServerList))]
    private bool _useServerList;

    /// <summary>False when this build has no list to read.</summary>
    public bool CanUseServerList => _directory != null;

    public ObservableCollection<ServerEntry> ListedServers { get; } = [];

    [ObservableProperty]
    private ServerEntry? _selectedListed;

    /// <summary>Loading, none published, or how many there are; null while the list is off.</summary>
    [ObservableProperty]
    private string? _listStatus;

    CancellationTokenSource? _listing;

    partial void OnUseServerListChanged(bool value)
    {
        Persist();
        _ = RefreshListAsync();
    }

    partial void OnSelectedListedChanged(ServerEntry? value)
    {
        if (value == null) return;
        ServerInput = value.Endpoint;
        SecretInput = value.Key;
    }

    /// <summary>Fetches the published list (when it is switched on) and shows it. Never throws.</summary>
    public async Task RefreshListAsync()
    {
        _listing?.Cancel();
        ListedServers.Clear();
        SelectedListed = null;
        if (!UseServerList || _directory == null)
        {
            ListStatus = null;
            return;
        }
        var mine = _listing = new CancellationTokenSource();
        ListStatus = L.Get("Main.ServerListLoading");
        var found = await _directory.LoadAsync(mine.Token);
        if (mine.IsCancellationRequested) return;
        foreach (var e in found) ListedServers.Add(e);
        ListStatus = found.Count == 0 ? L.Get("Main.ServerListEmpty") : L.Format("Main.ServerListCount", found.Count);
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
        var secret = value.Length == 0 ? "" : TypedSecret;
        SecretError = null;
        // The key first: a server is never saved without the key it needs.
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
