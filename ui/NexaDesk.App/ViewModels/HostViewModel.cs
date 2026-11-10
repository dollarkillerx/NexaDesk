using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Proto;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

public enum HostState { Stopped, Starting, Hosting }

public sealed partial class PairedDeviceViewModel(HostViewModel owner, PairedDevice device, string lastSeen) : ObservableObject
{
    public PairedDevice Device { get; } = device;
    public string Name => string.IsNullOrWhiteSpace(Device.Name) ? "(未命名设备)" : Device.Name;
    public string Fingerprint => Texts.ShortFingerprint(Device.DeviceKey);
    public string PermsText => Texts.Permissions(Device.Perms);
    public string LastSeen { get; } = lastSeen;
    public bool Unattended => Device.Unattended;
    public string ConsentText => Device.Unattended ? "连接时不再询问" : "每次连接都需确认";

    [RelayCommand]
    private void Revoke() => owner.Revoke(this);

    [RelayCommand]
    private void RequireConsent() => owner.RequireConsent(this);
}

/// <summary>A running session on this Mac: who, with what, and the way to end it (§9.3).</summary>
public sealed partial class BannerViewModel : ObservableObject
{
    readonly IAgentLink _link;
    readonly DateTimeOffset _startedAt;

    public BannerViewModel(HostSession session, IAgentLink link, DateTimeOffset startedAt)
    {
        _link = link;
        _startedAt = startedAt;
        _session = session;
        Transfers = new TransferListViewModel(new AgentTransfers(link));
    }

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(Headline), nameof(PermsText), nameof(Detail), nameof(HasInput), nameof(CanSendFiles), nameof(DropHint))]
    private HostSession _session;

    /// <summary>Files dropped here go to the viewer; files from it are listed here too.</summary>
    public TransferListViewModel Transfers { get; }
    public bool CanSendFiles => Session.Perms.File;
    public string DropHint => CanSendFiles
        ? $"把文件拖到这里发给对方 · 收到的文件在 {Texts.DownloadsFolder}"
        : "";

    [ObservableProperty] private string _durationText = "00:00";
    [ObservableProperty] private bool _isDisconnecting;

    public ulong SessionId => Session.Id;
    public bool HasInput => Session.Perms.Input;
    public string Headline => HasInput ? $"「{Session.DeviceName}」正在控制本机" : $"「{Session.DeviceName}」正在查看本机屏幕";
    public string PermsText => $"权限：{Texts.Permissions(Session.Perms)}";
    public string Detail => $"{Session.Remote} · 指纹 {Texts.ShortFingerprint(Session.DeviceKey)}";

    public void Tick(DateTimeOffset now) => DurationText = Texts.Duration(now - _startedAt);

    [RelayCommand]
    private void Disconnect()
    {
        IsDisconnecting = true;
        _link.Send(new UiRequest.Disconnect());
    }
}

/// <summary>The host's transfers go to the agent, which checks the session's permission.</summary>
sealed class AgentTransfers(IAgentLink link) : ITransferControl
{
    public void Send(IReadOnlyList<string> paths) => link.Send(new UiRequest.SendFiles(paths));
    public void Cancel(ulong id) => link.Send(new UiRequest.CancelTransfer(id));
    public void Answer(ulong id, bool accept) => link.Send(new UiRequest.AnswerTransfer(id, accept));
}

/// <summary>One way to reach this Mac while hosting: "Tailscale", "局域网", "设备 ID"…</summary>
public sealed record ConnectOption(string Kind, string Address, string Detail = "")
{
    public bool HasDetail => Detail.Length > 0;
}

/// <summary>"允许别人控制本机": the agent, its code, and the devices paired with it.</summary>
public sealed partial class HostViewModel : ObservableObject
{
    static readonly TimeSpan StartTimeout = TimeSpan.FromSeconds(60);
    static readonly TimeSpan RetryEvery = TimeSpan.FromMilliseconds(250);

    readonly IAgentLink _link;
    readonly IAgentProcess _process;
    readonly IPermissionProbe _permissions;
    readonly IWindowService _windows;
    readonly Func<DateTimeOffset> _now;
    readonly string _deviceName;
    readonly Func<string> _signalServer;
    readonly Func<bool> _announceOnLan;
    readonly ILocalAddresses _addresses;
    BannerViewModel? _banner;
    ConsentViewModel? _consent;
    CancellationTokenSource? _starting;
    bool _stopping;

    public ObservableCollection<PairedDeviceViewModel> Paired { get; } = new();
    public IReadOnlyList<string> AddressSuggestions { get; }

    /// <summary>This Mac's Tailscale: how people outside the LAN reach it, with no server of ours.</summary>
    public TailscaleViewModel Tailscale { get; }
    public bool ShowTailscaleAdvice => Tailscale.IsAbsent;

    /// <summary>While hosting: every address the viewer may type, Tailscale first.</summary>
    public ObservableCollection<ConnectOption> ConnectOptions { get; } = new();

    /// <summary>
    /// Listen on every interface, dual-stack (the default): LAN and Tailscale at once, and
    /// interfaces that come later. Off: only <see cref="ListenAddress"/>.
    /// </summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ListenHint), nameof(ListenSummary))]
    private bool _listenEverywhere = true;

    /// <summary>The other radio button: only <see cref="ListenAddress"/>.</summary>
    public bool ListenOnOne
    {
        get => !ListenEverywhere;
        set => ListenEverywhere = !value;
    }

    partial void OnListenEverywhereChanged(bool value) => OnPropertyChanged(nameof(ListenOnOne));

    public string ListenSummary => ListenEverywhere
        ? $"所有网络（局域网和 Tailscale），端口 {Texts.DefaultPort}"
        : $"只在 {ListenAddress}";
    public ConsentViewModel? Consent => _consent;
    public BannerViewModel? Banner => _banner;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsStopped), nameof(IsStarting), nameof(IsHosting), nameof(StateText), nameof(RestartNeeded))]
    [NotifyCanExecuteChangedFor(nameof(StartCommand), nameof(StopCommand), nameof(RotateCodeCommand))]
    private HostState _state = HostState.Stopped;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ListenHint), nameof(ListenSummary))]
    private string _listenAddress;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CodeDisplay))]
    private string _code = "";

    [ObservableProperty] private string _activeListen = "";

    /// <summary>This Mac's device ID while registered with the signaling server, else empty.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(DeviceIdDisplay), nameof(HasDeviceId))]
    private string _deviceId = "";

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasSignalText))]
    private string _signalText = "";

    /// <summary>Only said when a server is in use: without one there is nothing to report.</summary>
    public bool HasSignalText => SignalText.Length > 0;
    [ObservableProperty] private bool _signalProblem;

    public bool HasDeviceId => DeviceId.Length > 0;
    public string DeviceIdDisplay => HasDeviceId ? Texts.GroupDeviceId(DeviceId) : "—";

    /// <summary>
    /// The server the running agent was started with, or null for an agent this app
    /// did not start (it cannot tell). A changed setting applies on the next start.
    /// </summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(RestartNeeded))]
    private string? _startedWithServer;

    /// <summary>The LAN announcement setting the running agent was started with; null as for the server.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(RestartNeeded))]
    private bool? _startedWithAnnounce;

    public bool RestartNeeded => IsHosting &&
        ((StartedWithServer != null && StartedWithServer != _signalServer()) ||
         (StartedWithAnnounce != null && StartedWithAnnounce != _announceOnLan()));

    public void OnServerChanged() => OnPropertyChanged(nameof(RestartNeeded));
    [ObservableProperty] private string _hostFingerprint = "—";
    [ObservableProperty] private bool _inputAvailable = true;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasError))]
    private string? _error;

    [ObservableProperty] private string? _notice;

    public HostViewModel(IAgentLink link, IAgentProcess process, IPermissionProbe permissions, IWindowService windows,
        ILocalAddresses addresses, string deviceName, Func<DateTimeOffset>? now = null, Func<string>? signalServer = null,
        Func<bool>? announceOnLan = null, TailscaleViewModel? tailscale = null)
    {
        Tailscale = tailscale ?? new TailscaleViewModel(windows);
        Tailscale.Changed += () =>
        {
            OnPropertyChanged(nameof(ShowTailscaleAdvice));
            RebuildOptions();
        };
        _addresses = addresses;
        _signalServer = signalServer ?? (() => "");
        _announceOnLan = announceOnLan ?? (() => true);
        _link = link;
        _process = process;
        _permissions = permissions;
        _windows = windows;
        _deviceName = deviceName;
        _now = now ?? (() => DateTimeOffset.Now);
        AddressSuggestions = addresses.Candidates().Select(a => $"{a}:{Texts.DefaultPort}").ToList();
        _listenAddress = AddressSuggestions.FirstOrDefault() ?? $"127.0.0.1:{Texts.DefaultPort}";
        _link.MessageReceived += OnMessage;
        _link.Disconnected += OnLinkLost;
        _process.Exited += OnProcessExited;
    }

    public bool IsStopped => State == HostState.Stopped;
    public bool IsStarting => State == HostState.Starting;
    public bool IsHosting => State == HostState.Hosting;
    public bool HasError => Error != null;
    public string CodeDisplay => Texts.GroupCode(Code);

    public string StateText => State switch
    {
        HostState.Starting => "正在启动…",
        HostState.Hosting => "已开启：对方输入下面任一地址和一次性码即可请求连接，每次都需要你确认",
        _ => "未开启：别人无法连接本机",
    };

    public string? ListenHint => !ListenEverywhere && Texts.NormalizeAddress(ListenAddress) == null ? "格式应为 IP:端口" : null;

    [RelayCommand]
    private void UseSuggestion(string address)
    {
        ListenAddress = address;
        ListenEverywhere = false;
    }

    /// <summary>
    /// What the viewer can type. The agent reports where it listens; on every interface that
    /// means each of this Mac's addresses. Tailscale comes first, as the way that works from
    /// anywhere, and by its MagicDNS name when there is one.
    /// </summary>
    void RebuildOptions()
    {
        ConnectOptions.Clear();
        if (!IsHosting || ActiveListen.Length == 0) return;
        var (host, port) = SplitListen(ActiveListen);
        var suffix = port == Texts.DefaultPort ? "" : $":{port}";
        var everywhere = host is "0.0.0.0" or "::";
        var ips = everywhere ? _addresses.Candidates().Where(ip => ip != "127.0.0.1").ToList() : [host];
        var tailnet = ips.Where(IsTailnet).ToList();
        if (tailnet.Count > 0 && Tailscale.DnsName.Length > 0)
            ConnectOptions.Add(new ConnectOption("Tailscale", Tailscale.DnsName + suffix, $"或 {tailnet[0]}{suffix}"));
        else
            foreach (var ip in tailnet) ConnectOptions.Add(new ConnectOption("Tailscale", Bracket(ip) + suffix));
        foreach (var ip in ips.Where(ip => !IsTailnet(ip)))
            ConnectOptions.Add(new ConnectOption(ip == "127.0.0.1" || ip == "::1" ? "仅本机" : "局域网", Bracket(ip) + suffix));
        if (HasDeviceId) ConnectOptions.Add(new ConnectOption("设备 ID", DeviceIdDisplay, "经自建信令服务器"));
    }

    partial void OnStateChanged(HostState value) => RebuildOptions();

    static bool IsTailnet(string ip) => System.Net.IPAddress.TryParse(ip, out var a) && Texts.IsTailnetIp(a);

    static string Bracket(string ip) => ip.Contains(':') ? $"[{ip}]" : ip;

    static (string Host, int Port) SplitListen(string listen)
    {
        var i = listen.LastIndexOf(':');
        if (i <= 0 || !int.TryParse(listen[(i + 1)..], out var port)) return (listen, Texts.DefaultPort);
        return (listen[..i].Trim('[', ']'), port);
    }

    [RelayCommand(CanExecute = nameof(IsStopped))]
    private async Task Start()
    {
        Error = null;
        Notice = null;
        var listen = ListenEverywhere ? $"[::]:{Texts.DefaultPort}" : Texts.NormalizeAddress(ListenAddress);
        if (listen == null)
        {
            Error = "监听地址格式不对，应为 IP:端口，例如 192.168.1.20:47800。";
            return;
        }
        if (!_permissions.Check().ScreenRecording)
        {
            Error = "需要先授予 NexaDesk“屏幕录制”权限，别人才能看到本机屏幕。";
            ShowPermissionGuide();
            return;
        }
        State = HostState.Starting;
        _stopping = false;
        using var cts = new CancellationTokenSource(StartTimeout);
        _starting = cts;
        try
        {
            // An agent left from an earlier run is reused rather than fought with.
            if (await _link.ConnectAsync(cts.Token) == null)
            {
                Attached();
                return;
            }
            var server = _signalServer();
            var announce = _announceOnLan();
            StartedWithServer = server;
            StartedWithAnnounce = announce;
            var why = _process.Start(listen, _deviceName, server, announce);
            if (why != null)
            {
                State = HostState.Stopped;
                Error = why;
                return;
            }
            while (!cts.IsCancellationRequested)
            {
                await Task.Delay(RetryEvery, cts.Token);
                if (State != HostState.Starting) return; // the process exited meanwhile
                if (await _link.ConnectAsync(cts.Token) == null)
                {
                    Attached();
                    return;
                }
            }
        }
        catch (OperationCanceledException)
        {
        }
        finally
        {
            _starting = null;
        }
        if (State == HostState.Starting)
        {
            _process.Kill();
            State = HostState.Stopped;
            Error ??= "被控服务启动超时。";
        }
    }

    /// <summary>At startup: picks up an agent that is still running from before.</summary>
    public async Task TryAttachExistingAsync()
    {
        if (State != HostState.Stopped) return;
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(3));
        if (await _link.ConnectAsync(cts.Token) == null) Attached();
    }

    void Attached()
    {
        State = HostState.Hosting;
        Error = null;
        _link.Send(new UiRequest.ListPairings());
    }

    [RelayCommand(CanExecute = nameof(IsHosting))]
    private void Stop()
    {
        _stopping = true;
        _link.Send(new UiRequest.Shutdown());
        _link.Close();
        Stopped(null);
        if (_process.IsRunning) _ = KillLaterAsync();
    }

    async Task KillLaterAsync()
    {
        await Task.Delay(TimeSpan.FromSeconds(3));
        if (_process.IsRunning && State == HostState.Stopped) _process.Kill();
    }

    [RelayCommand(CanExecute = nameof(IsHosting))]
    private void RotateCode() => _link.Send(new UiRequest.RotateCode());

    [RelayCommand]
    private void RefreshPairings() => _link.Send(new UiRequest.ListPairings());

    [RelayCommand]
    private void ShowPermissionGuide() =>
        _windows.ShowPermissionGuide(new PermissionGuideViewModel(_permissions, PermissionContext.Hosting));

    internal void Revoke(PairedDeviceViewModel d) => _link.Send(new UiRequest.RevokePairing(d.Device.DeviceKey));

    internal void RequireConsent(PairedDeviceViewModel d) =>
        _link.Send(new UiRequest.SetAccessPolicy(d.Device.DeviceKey, false, d.Device.Perms));

    void OnMessage(AgentMessage message)
    {
        switch (message)
        {
            case AgentMessage.Status s:
                Code = s.Code;
                ActiveListen = s.Listen;
                InputAvailable = s.InputAvailable;
                HostFingerprint = Texts.Fingerprint(s.DeviceKey);
                DeviceId = s.Signal == SignalState.Online ? s.ShortId : "";
                SignalText = s.Signal == SignalState.Off ? "" : Texts.Signal(s.Signal, s.ShortId, s.SignalDetail);
                RebuildOptions();
                SignalProblem = s.Signal == SignalState.Error;
                if (s.Session != null) ShowBanner(s.Session);
                else HideBanner();
                break;
            case AgentMessage.IncomingRequest r:
                // The agent asks one question at a time; a new one replaces anything stale.
                _consent?.Expire("被新的请求取代");
                var consent = new ConsentViewModel(r, _link, _now());
                consent.Finished += c =>
                {
                    if (_consent == c) _consent = null;
                };
                _consent = consent;
                _windows.ShowConsent(consent);
                break;
            case AgentMessage.RequestClosed c:
                if (_consent?.ReqId == c.ReqId) _consent.Expire(c.Reason);
                break;
            case AgentMessage.SessionStarted s:
                ShowBanner(s.Session);
                Notice = null;
                break;
            case AgentMessage.SessionEnded e:
                if (_banner?.SessionId == e.Id) HideBanner();
                Notice = $"与「{e.DeviceName}」的会话已结束。";
                _link.Send(new UiRequest.ListPairings());
                break;
            case AgentMessage.Pairings p:
                Paired.Clear();
                var now = _now();
                foreach (var d in p.Devices)
                    Paired.Add(new PairedDeviceViewModel(this, d, Texts.Ago(DateTimeOffset.FromUnixTimeSeconds((long)d.LastSeen), now)));
                break;
            case AgentMessage.Notice n:
                Notice = n.Text;
                break;
            case AgentMessage.Transfer t:
                if (_banner?.SessionId == t.SessionId) _banner.Transfers.OnUpdate(t.Info);
                break;
            case AgentMessage.Refused { Command: "files" } r when _banner != null:
                _banner.Transfers.Error = $"无法发送：{r.Reason}";
                break;
            case AgentMessage.Refused r:
                Error = r.Command == "policy"
                    ? "只能在连接确认时开启“下次无需确认”。"
                    : $"操作未完成：{r.Reason}";
                break;
        }
    }

    void ShowBanner(HostSession session)
    {
        if (_banner != null && _banner.SessionId == session.Id)
        {
            _banner.Session = session;
            return;
        }
        _banner = new BannerViewModel(session, _link, _now());
        OnPropertyChanged(nameof(Banner));
        _windows.ShowBanner(_banner);
    }

    void HideBanner()
    {
        if (_banner == null) return;
        _banner = null;
        OnPropertyChanged(nameof(Banner));
        _windows.HideBanner();
    }

    void Stopped(string? error)
    {
        _starting?.Cancel();
        State = HostState.Stopped;
        Code = "";
        ActiveListen = "";
        StartedWithServer = null;
        StartedWithAnnounce = null;
        DeviceId = "";
        SignalText = "";
        ConnectOptions.Clear();
        SignalProblem = false;
        Paired.Clear();
        // With no UI the agent itself ends the session and denies the open question.
        _consent?.Expire("被控服务已停止");
        HideBanner();
        if (error != null) Error = error;
    }

    void OnLinkLost(string? why)
    {
        if (State == HostState.Stopped) return;
        Stopped(_stopping ? null : "与被控服务的连接断开了，已停止允许控制。");
    }

    void OnProcessExited(AgentExit exit)
    {
        if (_stopping && State == HostState.Stopped) return;
        var text = exit.LastOutput;
        string message;
        if (text.Contains("Screen Recording", StringComparison.OrdinalIgnoreCase))
        {
            message = "缺少“屏幕录制”权限，被控服务无法启动。";
            ShowPermissionGuide();
        }
        else if (text.Contains("cannot listen", StringComparison.OrdinalIgnoreCase))
            message = $"无法在 {ListenAddress} 上监听：地址不属于本机，或端口已被占用。";
        else if (text.Contains("already running", StringComparison.OrdinalIgnoreCase))
            message = "另一个被控服务已经在运行。";
        else if (text.Contains("device key", StringComparison.OrdinalIgnoreCase))
            message = "无法读取本机设备密钥（钥匙串）。";
        else
            message = $"被控服务意外退出（代码 {exit.Code}）。";
        var detail = text.Length > 0 ? $"\n{text.Split('\n').Last()}" : "";
        if (State != HostState.Stopped) Stopped(message + detail);
        else Error = message + detail;
    }

    /// <summary>At exit: nobody would see the banner, so hosting stops with the app.</summary>
    public void ShutdownForExit()
    {
        if (!_link.IsConnected) return;
        _stopping = true;
        _link.Send(new UiRequest.Shutdown());
        _link.Close();
    }
}

public enum PermissionContext { Hosting, Viewing }

/// <summary>Where there is no SMAppService (and in tests that do not care).</summary>
sealed class NoLoginItem : ILoginItem
{
    public LoginItemStatus Status => LoginItemStatus.Unsupported;
    public string? Register() => "需要 macOS 13 或更新版本";
    public string? Unregister() => null;
    public void OpenSettings() { }
}

/// <summary>Where to switch on Screen Recording and Accessibility, and whether they are on.</summary>
public sealed partial class PermissionGuideViewModel : ObservableObject
{
    readonly IPermissionProbe _probe;

    public PermissionContext Context { get; }

    public event Action? CloseRequested;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ScreenRecordingStatus), nameof(AllGranted))]
    private bool _screenRecording;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(AccessibilityStatus), nameof(AllGranted))]
    private bool _accessibility;

    public PermissionGuideViewModel(IPermissionProbe probe, PermissionContext context)
    {
        _probe = probe;
        Context = context;
        Recheck();
    }

    public string Title => "需要 macOS 权限";

    public string Intro => Context == PermissionContext.Hosting
        ? "要让别人看到并控制这台 Mac，需要在“系统设置 → 隐私与安全性”中为 NexaDesk 打开下面的开关。"
        : "要把 Cmd+Tab 等全部按键发给对方，需要为 NexaDesk 打开“辅助功能”。没有它也能控制，但系统快捷键会留在本机。";

    public bool ShowScreenRecording => Context == PermissionContext.Hosting;
    public string ScreenRecordingStatus => ScreenRecording ? "已授予" : "未授予";
    public string AccessibilityStatus => Accessibility ? "已授予" : "未授予";
    public string ScreenRecordingUse => "屏幕录制（必需）：让对方看到你的屏幕";
    public string AccessibilityUse => Context == PermissionContext.Hosting
        ? "辅助功能：让对方操作你的键盘和鼠标；不授予则只能查看"
        : "辅助功能：控制别人时捕获全部按键";
    public bool AllGranted => Accessibility && (ScreenRecording || Context != PermissionContext.Hosting);
    public string RestartNote => "打开开关后，macOS 可能要求退出并重新打开 NexaDesk 才会生效。";

    [RelayCommand]
    private void Recheck()
    {
        var s = _probe.Check();
        ScreenRecording = s.ScreenRecording;
        Accessibility = s.Accessibility;
    }

    [RelayCommand]
    private void RequestScreenRecording() => _probe.Request(PermissionKind.ScreenRecording);

    [RelayCommand]
    private void OpenScreenRecording() => _probe.OpenSettings(PermissionKind.ScreenRecording);

    [RelayCommand]
    private void OpenAccessibility() => _probe.OpenSettings(PermissionKind.Accessibility);

    [RelayCommand]
    private void Close() => CloseRequested?.Invoke();
}

/// <summary>The main window: both halves and what they share.</summary>
public sealed partial class MainViewModel : ObservableObject
{
    readonly IClientCore _core;

    public ConnectViewModel Connect { get; }
    public HostViewModel Host { get; }
    public SettingsViewModel Settings { get; }
    /// <summary>Shared by both halves.</summary>
    public TailscaleViewModel Tailscale { get; }

    public MainViewModel(IClientCore core, IRecentStore recent, IAgentLink link, IAgentProcess process,
        IPermissionProbe permissions, IWindowService windows, ILocalAddresses addresses, string deviceName,
        Func<DateTimeOffset>? now = null, ISettingsStore? settings = null, ILoginItem? loginItem = null,
        IKeymapService? keymap = null)
    {
        _keymap = keymap;
        _core = core;
        DeviceName = deviceName;
        Settings = new SettingsViewModel(settings ?? new MemorySettings());
        Tailscale = new TailscaleViewModel(windows);
        Connect = new ConnectViewModel(core, recent, windows, now, () => Settings.SavedServer, Tailscale);
        Host = new HostViewModel(link, process, permissions, windows, addresses, deviceName, now, () => Settings.SavedServer,
            () => Settings.AnnounceOnLan, Tailscale);
        Settings.ServerChanged += _ =>
        {
            Connect.OnServerChanged();
            Host.OnServerChanged();
        };
        Settings.AnnounceChanged += _ => Host.OnServerChanged();
        _permissions = permissions;
        _windows = windows;
        LoginItem = new LoginItemViewModel(loginItem ?? new NoLoginItem());
        Tray = new TrayViewModel(Host);
    }

    readonly IKeymapService? _keymap;
    readonly IPermissionProbe _permissions;

    /// <summary>"登录时启动".</summary>
    public LoginItemViewModel LoginItem { get; }
    /// <summary>The menu-bar item that keeps NexaDesk running with its window closed.</summary>
    public TrayViewModel Tray { get; }
    public bool HasKeyboardSettings => _keymap != null;

    [RelayCommand]
    private void ShowKeyboardSettings()
    {
        if (_keymap != null) _windows.ShowKeyboardSettings(new KeyboardSettingsViewModel(_keymap));
    }
    readonly IWindowService _windows;

    public string DeviceName { get; }

    [RelayCommand]
    private void ShowViewerPermissions() =>
        _windows.ShowPermissionGuide(new PermissionGuideViewModel(_permissions, PermissionContext.Viewing));

    public void OnExit()
    {
        Connect.CloseAll();
        Host.ShutdownForExit();
    }
}
