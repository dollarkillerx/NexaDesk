using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
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

    public string PairedText => IsPaired ? L.Get("Connect.Paired") : L.Get("Connect.NotPaired");

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
    public string PathText => Device.Direct ? L.Get("Connect.PathDirect") : L.Get("Connect.PathDerp");
    public string Detail => string.IsNullOrEmpty(Device.Os) ? Device.Address : $"{Device.Address} · {Device.Os}";

    [RelayCommand]
    private void Connect() => owner.UseTailnet(this);
}

/// <summary>A NexaDesk host heard on the LAN (§4.11). Unverified: connecting checks it as for a typed address.</summary>
public sealed partial class LanDeviceViewModel(ConnectViewModel owner, LanDevice device) : ObservableObject
{
    public LanDevice Device { get; } = device;
    public string Name => string.IsNullOrWhiteSpace(Device.Name) ? Device.Address : Device.Name;
    public string Address => Device.Address;
    public string Detail => string.IsNullOrEmpty(Device.Os) ? Device.Address : $"{Device.Address} · {Device.Os}";

    [RelayCommand]
    private void Connect() => owner.UseAddress(Device.Address);
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
    public ObservableCollection<LanDeviceViewModel> Lan { get; } = new();
    public bool HasLan => Lan.Count > 0;
    public IReadOnlyCollection<SessionViewModel> Sessions => _sessions.Values;

    /// <summary>This Mac's Tailscale; recommended for anything beyond the LAN.</summary>
    public TailscaleViewModel Tailscale { get; }

    /// <summary>The Tailscale recommendation under the device lists: shown once the core says it is absent.</summary>
    public bool ShowTailscaleAdvice => Tailscale.IsAbsent;

    bool HasServer => _signalServer().Length > 0;

    public string Intro => HasServer
        ? L.Get("Connect.IntroWithId")
        : L.Get("Connect.Intro");

    public string AddressPlaceholder => HasServer
        ? L.Get("Connect.AddressPlaceholderWithId")
        : L.Get("Connect.AddressPlaceholder");

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
            ? L.Get("Connect.AddressHint")
            : Texts.NormalizeDeviceId(Address) != null && !HasServer
                ? IdNeedsServer
                : null;

    static string IdNeedsServer => L.Get("Connect.IdNeedsServer");

    public string? CodeHint =>
        Code.Length > 0 && Texts.NormalizeCode(Code) == null ? L.Get("Connect.CodeHint") : null;

    public ConnectViewModel(IClientCore core, IRecentStore store, IWindowService windows, Func<DateTimeOffset>? now = null,
        Func<string>? signalServer = null, TailscaleViewModel? tailscale = null)
    {
        Tailscale = tailscale ?? new TailscaleViewModel(windows);
        Tailscale.Changed += () => OnPropertyChanged(nameof(ShowTailscaleAdvice));
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
        OnPropertyChanged(nameof(Intro));
        OnPropertyChanged(nameof(AddressPlaceholder));
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
        Error = new ErrorText(L.Get("Connect.CodeNeeded"), L.Format("Connect.NotPairedDetail", device.Name));
        FocusCodeRequested?.Invoke();
    }

    /// <summary>A tailnet device: a paired one connects at once, anything else needs its code.</summary>
    internal void UseTailnet(TailnetDeviceViewModel device) => UseAddress(device.Address);

    /// <summary>A listed ip:port: a paired one connects at once, anything else needs its code.</summary>
    internal void UseAddress(string address)
    {
        if (IsConnecting) return;
        var known = _recent.FirstOrDefault(r => r.Address == address && IsPaired(r.HostKey));
        if (known != null)
        {
            Start(known.Address, new ConnectAuth.KnownHost(known.HostKey));
            return;
        }
        Address = address;
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
            Error = new ErrorText(L.Get("Connect.IdNeedsServerTitle"), IdNeedsServer);
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
            Error = new ErrorText(L.Get("Connect.CannotStart"), e.Message);
            return;
        }
        _pending = (tag, session, address);
        IsConnecting = true;
        ProgressText = isId ? Texts.Stage(ConnectStage.Resolving, Shown(address)) : L.Format("Connect.Connecting", address);
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
            Tailscale.Update(tailnet);
            Tailnet.Clear();
            foreach (var d in tailnet.Devices) Tailnet.Add(new TailnetDeviceViewModel(this, d));
            OnPropertyChanged(nameof(HasTailnet));
            return;
        }
        if (ev is LanDevicesEvent lan)
        {
            Lan.Clear();
            foreach (var d in lan.Devices) Lan.Add(new LanDeviceViewModel(this, d));
            OnPropertyChanged(nameof(HasLan));
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
        IdentityError = string.IsNullOrEmpty(ready.Error) ? null : L.Format("Connect.IdentityError", ready.Error);
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
                if (d.Reason != DisconnectReason.ClosedLocally) Error = Failure(d.Reason, p.Address);
                break;
            case WarningEvent w:
                ProgressText = Messages.Warning(w);
                break;
        }
    }

    /// <summary>A failure to get through to an address outside any tailnet points at Tailscale.</summary>
    static ErrorText Failure(DisconnectReason reason, string address)
    {
        var e = Texts.Disconnect(reason);
        var unreachable = reason is DisconnectReason.Network or DisconnectReason.TimedOut or DisconnectReason.NoPath;
        return unreachable && !Texts.IsTailnetTarget(address) ? new ErrorText(e.Title, $"{e.Detail}\n{Texts.TailscaleAdvice}") : e;
    }

    static string Shown(string address) =>
        Texts.NormalizeDeviceId(address) is { } id ? Texts.GroupDeviceId(id) : address;

    void OnSessionEnded(SessionViewModel vm, DisconnectReason reason, string message)
    {
        _sessions.Remove(vm.Session.Tag);
        if (reason == DisconnectReason.ClosedLocally) return;
        var e = Texts.Disconnect(reason);
        Error = new ErrorText(L.Format("Connect.SessionEnded", vm.HostName, e.Title), e.Detail);
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
