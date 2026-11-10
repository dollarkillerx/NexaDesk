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

    public SettingsViewModel(ISettingsStore store)
    {
        _store = store;
        var loaded = store.Load();
        _savedServer = loaded.SignalServer;
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

    void Persist() => _store.Save(new AppSettings(SavedServer, AnnounceOnLan, SelectedLanguage?.Code ?? "", AllowBrowsers));

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
    [NotifyPropertyChangedFor(nameof(ServerHint), nameof(IsDirty))]
    [NotifyCanExecuteChangedFor(nameof(SaveCommand))]
    private string _serverInput;

    /// <summary>Raised after a change is saved.</summary>
    public event Action<string>? ServerChanged;

    public bool HasServer => SavedServer.Length > 0;

    public bool IsDirty => (ServerInput.Trim().Length == 0 ? "" : Texts.NormalizeServer(ServerInput)) != SavedServer;

    public string? ServerHint =>
        ServerInput.Trim().Length > 0 && Texts.NormalizeServer(ServerInput) == null
            ? L.Get("Main.ServerHint")
            : null;

    public string ServerStatus => HasServer
        ? L.Format("Main.ServerInUse", SavedServer)
        : L.Get("Main.ServerNotUsed");

    bool CanSave() => IsDirty && ServerHint == null;

    [RelayCommand(CanExecute = nameof(CanSave))]
    private void Save()
    {
        var value = ServerInput.Trim().Length == 0 ? "" : Texts.NormalizeServer(ServerInput);
        if (value == null) return;
        SavedServer = value;
        ServerInput = value;
        Persist();
        ServerChanged?.Invoke(value);
    }
}
