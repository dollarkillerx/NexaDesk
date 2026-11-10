using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
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
        LoginItemStatus.Enabled => "登录 macOS 后自动打开 NexaDesk（允许控制仍需你手动开启）",
        LoginItemStatus.RequiresApproval => "需要在“系统设置 → 通用 → 登录项”中允许 NexaDesk",
        LoginItemStatus.NotFound => "只有安装好的 NexaDesk.app 才能设为登录时启动",
        LoginItemStatus.Unsupported => "需要 macOS 13 或更新版本",
        _ => "关闭：登录后不会自动打开",
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
        Error = why == null ? null : (value ? "无法设为登录时启动：" : "无法取消登录时启动：") + why;
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
                HostState.Hosting => _host.HasDeviceId ? $"允许控制中 · ID {_host.DeviceIdDisplay}" : "允许控制中",
                HostState.Starting => "正在开启允许控制…",
                _ => "未允许别人控制本机",
            };
        ToolTip = $"NexaDesk — {StatusText}";
        HostingLabel = _host.State == HostState.Stopped ? "允许别人控制本机" : "停止允许控制";
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
