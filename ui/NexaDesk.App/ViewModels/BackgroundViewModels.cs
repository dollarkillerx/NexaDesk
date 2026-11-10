using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>"登录时启动": SMAppService.mainApp behind <see cref="ILoginItem"/>.</summary>
public sealed partial class LoginItemViewModel : ObservableObject
{
    readonly ILoginItem _item;
    bool _syncing;

    public LoginItemViewModel(ILoginItem item)
    {
        _item = item;
        Refresh();
    }

    [ObservableProperty] private bool _isEnabled;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsSupported), nameof(NeedsApproval), nameof(StatusText))]
    private LoginItemStatus _status;

    [ObservableProperty] private string? _error;

    public bool IsSupported => Status != LoginItemStatus.Unsupported;
    public bool NeedsApproval => Status == LoginItemStatus.RequiresApproval;

    public string StatusText => Status switch
    {
        LoginItemStatus.Enabled => L.Get("LoginItem.Enabled"),
        LoginItemStatus.RequiresApproval => L.Get("LoginItem.RequiresApproval"),
        LoginItemStatus.NotFound => L.Get("LoginItem.NotFound"),
        LoginItemStatus.Unsupported => L.Get("LoginItem.NeedsMacOS13"),
        _ => L.Get("LoginItem.Off"),
    };

    public void Refresh()
    {
        _syncing = true;
        Status = _item.Status;
        IsEnabled = Status is LoginItemStatus.Enabled or LoginItemStatus.RequiresApproval;
        _syncing = false;
    }

    partial void OnIsEnabledChanged(bool value)
    {
        if (_syncing) return;
        var why = value ? _item.Register() : _item.Unregister();
        Error = why == null ? null : (value ? L.Format("LoginItem.CannotEnable", why) : L.Format("LoginItem.CannotDisable", why));
        Refresh();
    }

    [RelayCommand]
    private void OpenSettings() => _item.OpenSettings();
}

/// <summary>
/// The menu-bar item: closing the main window keeps NexaDesk running here, with the
/// agent, consent prompts and the banner. Quitting from here ends hosting, since
/// nobody would see the banner any more (§9.3).
/// </summary>
public sealed partial class TrayViewModel : ObservableObject
{
    readonly HostViewModel _host;

    public TrayViewModel(HostViewModel host)
    {
        _host = host;
        host.PropertyChanged += (_, e) =>
        {
            if (e.PropertyName is nameof(HostViewModel.State) or nameof(HostViewModel.DeviceId) or nameof(HostViewModel.Banner))
                Update();
        };
        host.Tailscale.Changed += Update;
        Update();
    }

    public event Action? ShowRequested;
    public event Action? QuitRequested;

    [ObservableProperty] private string _statusText = "";
    [ObservableProperty] private string _toolTip = "NexaDesk";
    [ObservableProperty] private string _hostingLabel = "";
    [ObservableProperty] private bool _hasSession;

    void Update()
    {
        var banner = _host.Banner;
        HasSession = banner != null;
        StatusText = banner != null
            ? banner.Headline
            : _host.State switch
            {
                HostState.Hosting when _host.Tailscale.IsPresent && _host.Tailscale.ConnectName.Length > 0 =>
                    L.Format("Tray.HostingAt", _host.Tailscale.ConnectName),
                HostState.Hosting => _host.HasDeviceId ? L.Format("Tray.HostingId", _host.DeviceIdDisplay) : L.Get("Tray.Hosting"),
                HostState.Starting => L.Get("Tray.Starting"),
                _ => L.Get("Tray.Stopped"),
            };
        ToolTip = $"NexaDesk — {StatusText}";
        HostingLabel = _host.State == HostState.Stopped ? L.Get("Tray.AllowControl") : L.Get("Tray.StopControl");
        DisconnectCommand.NotifyCanExecuteChanged();
    }

    [RelayCommand]
    private void Show() => ShowRequested?.Invoke();

    [RelayCommand]
    private async Task ToggleHosting()
    {
        if (_host.State == HostState.Stopped)
        {
            if (_host.StartCommand.CanExecute(null)) await _host.StartCommand.ExecuteAsync(null);
            // A missing permission or a bad address is shown in the main window.
            if (_host.HasError) ShowRequested?.Invoke();
        }
        else if (_host.StopCommand.CanExecute(null))
        {
            _host.StopCommand.Execute(null);
        }
    }

    [RelayCommand(CanExecute = nameof(HasSession))]
    private void Disconnect() => _host.Banner?.DisconnectCommand.Execute(null);

    [RelayCommand]
    private void Quit() => QuitRequested?.Invoke();
}
