using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>The signaling server (§4.3): where device IDs are looked up. Empty means address only.</summary>
public sealed partial class SettingsViewModel : ObservableObject
{
    readonly ISettingsStore _store;

    public SettingsViewModel(ISettingsStore store)
    {
        _store = store;
        _savedServer = store.Load().SignalServer;
        _serverInput = _savedServer;
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
            ? "格式应为 wss://服务器[:端口]，例如 wss://signal.example.com"
            : null;

    public string ServerStatus => HasServer
        ? $"用 {SavedServer} 按设备 ID 连接"
        : "未设置：只能用 IP:端口 连接";

    bool CanSave() => IsDirty && ServerHint == null;

    [RelayCommand(CanExecute = nameof(CanSave))]
    private void Save()
    {
        var value = ServerInput.Trim().Length == 0 ? "" : Texts.NormalizeServer(ServerInput);
        if (value == null) return;
        SavedServer = value;
        ServerInput = value;
        _store.Save(new AppSettings(value));
        ServerChanged?.Invoke(value);
    }
}
