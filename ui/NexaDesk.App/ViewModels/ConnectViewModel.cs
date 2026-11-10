using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Native;
using NexaDesk.Proto;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

public sealed partial class RecentDeviceViewModel(ConnectViewModel owner, RecentDevice device, bool paired, string lastSeen)
    : ObservableObject
{
    public RecentDevice Device { get; } = device;
    public string Name => string.IsNullOrWhiteSpace(Device.Name) ? Address : Device.Name;
    /// <summary>As shown: an ID in groups of three, or the address.</summary>
    public string Address => Texts.NormalizeDeviceId(Device.Address) is { } id
        ? $"ID {Texts.GroupDeviceId(id)}"
        : Device.Address;
    public string HostKey => Device.HostKey;
    public string LastSeen { get; } = lastSeen;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(PairedText))]
    private bool _isPaired = paired;

    public string PairedText => IsPaired ? "已配对" : "未配对";

    [RelayCommand]
    private void Connect() => owner.ConnectRecent(this);

    [RelayCommand]
    private void Remove() => owner.RemoveRecent(this);
}

/// <summary>A NexaDesk host in this machine's tailnet (§4.7), from the Tailscale LocalAPI.</summary>
public sealed partial class TailnetDeviceViewModel(ConnectViewModel owner, TailnetDevice device) : ObservableObject
{
    public TailnetDevice Device { get; } = device;
    public string Name => string.IsNullOrWhiteSpace(Device.Name) ? Device.Address : Device.Name;
    public string Address => Device.Address;
    public string PathText => Device.Direct ? "直连" : "经 DERP 中继";
    public string Detail => string.IsNullOrEmpty(Device.Os) ? Device.Address : $"{Device.Address} · {Device.Os}";

    [RelayCommand]
    private void Connect() => owner.UseTailnet(this);
}

/// <summary>"连接到设备": address, one-time code, recent devices, and the attempt in flight.</summary>
public sealed partial class ConnectViewModel : ObservableObject
{
    readonly IClientCore _core;
    readonly IRecentStore _store;
    readonly IWindowService _windows;
    readonly Func<DateTimeOffset> _now;
    readonly Func<string> _signalServer;
    readonly Dictionary<ulong, SessionViewModel> _sessions = new();
    readonly HashSet<string> _paired = new(StringComparer.OrdinalIgnoreCase);
    List<RecentDevice> _recent;
    ulong _nextTag = 1;
    (ulong Tag, ISession Session, string Address)? _pending;

    public ObservableCollection<RecentDeviceViewModel> Recent { get; } = new();
    public ObservableCollection<TailnetDeviceViewModel> Tailnet { get; } = new();
    public bool HasTailnet => Tailnet.Count > 0;
    public IReadOnlyCollection<SessionViewModel> Sessions => _sessions.Values;

    /// <summary>Asks the view to put the cursor in the code box.</summary>
    public event Action? FocusCodeRequested;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ConnectCommand))]
    [NotifyPropertyChangedFor(nameof(AddressHint))]
    private string _address = "";

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ConnectCommand))]
    [NotifyPropertyChangedFor(nameof(CodeHint))]
    private string _code = "";

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ConnectCommand))]
    private bool _isConnecting;

    [ObservableProperty] private string _progressText = "";

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasError))]
    private ErrorText? _error;

    [ObservableProperty] private string _thisDeviceFingerprint = "—";
    [ObservableProperty] private string? _identityError;

    public bool HasError => Error != null;
    public bool HasRecent => Recent.Count > 0;

    public string? AddressHint =>
        Address.Length > 0 && Texts.NormalizeTarget(Address) == null
            ? "请输入 9 位设备 ID（例如 123 456 789），或 IP:端口（例如 192.168.1.20:47800）"
            : Texts.NormalizeDeviceId(Address) != null && _signalServer().Length == 0
                ? "用设备 ID 连接需要先在上方设置信令服务器"
                : null;

    public string? CodeHint =>
        Code.Length > 0 && Texts.NormalizeCode(Code) == null ? "一次性码是 8 位字母或数字（2–7），例如 ABCD-2345" : null;

    public ConnectViewModel(IClientCore core, IRecentStore store, IWindowService windows, Func<DateTimeOffset>? now = null,
        Func<string>? signalServer = null)
    {
        _core = core;
        _store = store;
        _windows = windows;
        _now = now ?? (() => DateTimeOffset.Now);
        _signalServer = signalServer ?? (() => "");
        _recent = store.Load().OrderByDescending(r => r.LastConnected).ToList();
        RebuildRecent();
        core.EventReceived += OnClientEvent;
    }

    void RebuildRecent()
    {
        Recent.Clear();
        var now = _now();
        foreach (var r in _recent)
            Recent.Add(new RecentDeviceViewModel(this, r, IsPaired(r.HostKey),
                Texts.Ago(DateTimeOffset.FromUnixTimeSeconds(r.LastConnected), now)));
        OnPropertyChanged(nameof(HasRecent));
    }

    bool IsPaired(string key) => key.Length > 0 && _paired.Contains(key);

    bool CanConnect() => !IsConnecting && Texts.NormalizeTarget(Address) != null && Texts.NormalizeCode(Code) != null;

    /// <summary>The server setting changed: an ID typed earlier may now be usable.</summary>
    public void OnServerChanged()
    {
        OnPropertyChanged(nameof(AddressHint));
        ConnectCommand.NotifyCanExecuteChanged();
    }

    [RelayCommand(CanExecute = nameof(CanConnect))]
    private void Connect()
    {
        var address = Texts.NormalizeTarget(Address);
        var code = Texts.NormalizeCode(Code);
        if (address == null || code == null) return;
        Start(address, new ConnectAuth.Code(code));
    }

    internal void ConnectRecent(RecentDeviceViewModel device)
    {
        if (IsConnecting) return;
        if (device.IsPaired)
        {
            Start(device.Device.Address, new ConnectAuth.KnownHost(device.HostKey));
            return;
        }
        Address = Texts.NormalizeDeviceId(device.Device.Address) is { } id ? Texts.GroupDeviceId(id) : device.Device.Address;
        Code = "";
        Error = new ErrorText("需要一次性码", $"尚未与 {device.Name} 配对。请输入对方屏幕上显示的一次性码。");
        FocusCodeRequested?.Invoke();
    }

    /// <summary>A tailnet device: a paired one connects at once, anything else needs its code.</summary>
    internal void UseTailnet(TailnetDeviceViewModel device)
    {
        if (IsConnecting) return;
        var known = _recent.FirstOrDefault(r => r.Address == device.Address && IsPaired(r.HostKey));
        if (known != null)
        {
            Start(known.Address, new ConnectAuth.KnownHost(known.HostKey));
            return;
        }
        Address = device.Address;
        Code = "";
        Error = null;
        FocusCodeRequested?.Invoke();
    }

    internal void RemoveRecent(RecentDeviceViewModel device)
    {
        _recent.RemoveAll(r => r.Address == device.Device.Address && r.HostKey == device.HostKey);
        _store.Save(_recent);
        RebuildRecent();
    }

    void Start(string address, ConnectAuth auth)
    {
        Error = null;
        var isId = Texts.NormalizeDeviceId(address) != null;
        var server = _signalServer();
        if (isId && server.Length == 0)
        {
            Error = new ErrorText("未设置信令服务器", "用设备 ID 连接需要信令服务器。请在窗口上方填写服务器地址，或改用 IP:端口。");
            return;
        }
        var tag = _nextTag++;
        ISession session;
        try
        {
            session = _core.Connect(new ConnectRequest(address, tag, StreamMode.Balanced, 0, auth, isId ? server : ""));
        }
        catch (NexaException e)
        {
            Error = new ErrorText("无法开始连接", e.Message);
            return;
        }
        _pending = (tag, session, address);
        IsConnecting = true;
        ProgressText = isId ? Texts.Stage(ConnectStage.Resolving, Shown(address)) : $"正在连接 {address}…";
    }

    [RelayCommand]
    private void Cancel()
    {
        if (_pending is not { } p) return;
        _pending = null;
        p.Session.Close();
        IsConnecting = false;
        ProgressText = "";
    }

    public void OnClientEvent(ClientEvent ev)
    {
        if (ev is ReadyEvent ready)
        {
            OnReady(ready);
            return;
        }
        if (ev is TailnetDevicesEvent tailnet)
        {
            Tailnet.Clear();
            foreach (var d in tailnet.Devices) Tailnet.Add(new TailnetDeviceViewModel(this, d));
            OnPropertyChanged(nameof(HasTailnet));
            return;
        }
        if (_pending is { } p && p.Tag == ev.Tag)
        {
            OnPendingEvent(p, ev);
            return;
        }
        if (_sessions.TryGetValue(ev.Tag, out var session)) session.OnEvent(ev);
    }

    void OnReady(ReadyEvent ready)
    {
        IdentityError = string.IsNullOrEmpty(ready.Error) ? null : $"无法加载本机身份：{ready.Error}";
        if (!string.IsNullOrEmpty(ready.ThisDeviceKey)) ThisDeviceFingerprint = Texts.Fingerprint(ready.ThisDeviceKey);
        _paired.Clear();
        foreach (var h in ready.Hosts) _paired.Add(h.Key);
        foreach (var r in Recent) r.IsPaired = IsPaired(r.HostKey);
    }

    void OnPendingEvent((ulong Tag, ISession Session, string Address) p, ClientEvent ev)
    {
        switch (ev)
        {
            case ConnectingEvent c:
                ProgressText = Texts.Stage(c.Stage, Shown(p.Address));
                break;
            case ConnectedEvent c:
                _pending = null;
                IsConnecting = false;
                ProgressText = "";
                Code = "";
                Remember(p.Address, c);
                var vm = new SessionViewModel(p.Session, c, Shown(p.Address), _now);
                vm.Ended += OnSessionEnded;
                _sessions[p.Tag] = vm;
                _windows.ShowSession(vm);
                break;
            case DisconnectedEvent d:
                _pending = null;
                IsConnecting = false;
                ProgressText = "";
                p.Session.Close();
                if (d.Reason != DisconnectReason.ClosedLocally) Error = Texts.Disconnect(d.Reason);
                break;
            case WarningEvent w:
                ProgressText = w.Text;
                break;
        }
    }

    static string Shown(string address) =>
        Texts.NormalizeDeviceId(address) is { } id ? Texts.GroupDeviceId(id) : address;

    void OnSessionEnded(SessionViewModel vm, DisconnectReason reason, string message)
    {
        _sessions.Remove(vm.Session.Tag);
        if (reason == DisconnectReason.ClosedLocally) return;
        var e = Texts.Disconnect(reason);
        Error = new ErrorText($"与 {vm.HostName} 的会话已结束：{e.Title}", e.Detail);
    }

    void Remember(string address, ConnectedEvent c)
    {
        if (c.PairedNow && c.HostKey.Length > 0) _paired.Add(c.HostKey);
        _recent.RemoveAll(r => r.Address == address || (c.HostKey.Length > 0 && r.HostKey == c.HostKey));
        _recent.Insert(0, new RecentDevice(address, c.HostName, c.HostKey, _now().ToUnixTimeSeconds()));
        if (_recent.Count > 20) _recent.RemoveRange(20, _recent.Count - 20);
        _store.Save(_recent);
        RebuildRecent();
    }

    /// <summary>Ends every session and any attempt in flight.</summary>
    public void CloseAll()
    {
        Cancel();
        foreach (var s in _sessions.Values.ToList()) s.DisconnectCommand.Execute(null);
    }
}
