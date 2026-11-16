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
    public string Name => Device.Label.Length > 0 ? Device.Label
        : string.IsNullOrWhiteSpace(Device.Name) ? Address : Device.Name;
    public bool HasPassword => Device.HasPassword;
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

    // ----- the inline editor: name, kept password ---------------------------------------

    [ObservableProperty] private bool _isEditing;
    [ObservableProperty] private string _editLabel = "";
    [ObservableProperty] private string _editPassword = "";
    [ObservableProperty] private bool _forgetPassword;
    [ObservableProperty] private string? _editError;

    /// <summary>What the name box shows greyed out: the host's own name.</summary>
    public string HostName => string.IsNullOrWhiteSpace(Device.Name) ? Address : Device.Name;

    [RelayCommand]
    private void Edit()
    {
        EditLabel = Device.Label;
        EditPassword = "";
        ForgetPassword = false;
        EditError = null;
        IsEditing = !IsEditing;
    }

    [RelayCommand]
    private void CancelEdit()
    {
        EditPassword = "";
        IsEditing = false;
    }

    [RelayCommand]
    private void SaveEdit()
    {
        var password = ForgetPassword ? null : EditPassword.Length > 0 ? EditPassword : "";
        EditError = owner.UpdateRecent(this, EditLabel.Trim(), password, ForgetPassword);
        if (EditError == null)
        {
            EditPassword = "";
            IsEditing = false;
        }
    }
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
    readonly IViewerPrefs _viewerPrefs;
    readonly ISavedPasswords _passwords;
    /// <summary>The password typed for this attempt, to keep if it gets in and keeping was asked.</summary>
    string? _passwordToKeep;
    /// <summary>The saved connection whose kept password this attempt uses.</summary>
    RecentDevice? _usingKept;
    readonly IWindowService _windows;
    readonly Func<DateTimeOffset> _now;
    readonly Func<string> _signalServer;

    /// <summary>A connection by device ID failed on the signaling server's certificate.</summary>
    public event Action<string>? SignalCertificateProblem;
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
    [NotifyPropertyChangedFor(nameof(CodeHint), nameof(CodeIsPassword))]
    private string _code = "";

    /// <summary>What is typed is a fixed password, not a one-time code: it can be kept.</summary>
    public bool CodeIsPassword => Texts.SignInFor(Code) is ConnectAuth.Password;

    /// <summary>"记住密码": keep the fixed password in the Keychain once it gets in.</summary>
    [ObservableProperty] private bool _rememberPassword;

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
        Code.Length > 0 && Texts.SignInFor(Code) == null ? L.Get("Connect.CodeHint") : null;

    public ConnectViewModel(IClientCore core, IRecentStore store, IWindowService windows, Func<DateTimeOffset>? now = null,
        Func<string>? signalServer = null, TailscaleViewModel? tailscale = null, IViewerPrefs? viewerPrefs = null,
        ISavedPasswords? passwords = null)
    {
        _viewerPrefs = viewerPrefs ?? new MemoryViewerPrefs();
        _passwords = passwords ?? new MemorySavedPasswords();
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

    bool CanConnect() => !IsConnecting && Texts.NormalizeTarget(Address) != null && SignIn() != null;

    /// <summary>The code or password typed; with a device ID and nothing typed, a request
    /// the person at the host answers.</summary>
    Proto.ConnectAuth? SignIn() =>
        Code.Length == 0 && Texts.NormalizeDeviceId(Address) != null
            ? new Proto.ConnectAuth.Request()
            : Texts.SignInFor(Code);

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
        var signIn = SignIn();
        if (address == null || signIn == null) return;
        Start(address, signIn);
        if (IsConnecting && RememberPassword && signIn is ConnectAuth.Password p) _passwordToKeep = p.Value;
    }

    internal void ConnectRecent(RecentDeviceViewModel device)
    {
        if (IsConnecting) return;
        // Its kept password signs in at once. It comes first: it is what the person
        // chose to keep, and the host may have no pairing for this device (it keeps
        // none for its fixed password, and the person there can remove one).
        if (device.HasPassword && _passwords.Load(device.Device.PasswordAccount) is { } kept)
        {
            Start(device.Device.Address, new ConnectAuth.Password(kept));
            if (IsConnecting) _usingKept = device.Device;
            return;
        }
        if (device.IsPaired)
        {
            Start(device.Device.Address, new ConnectAuth.KnownHost(device.HostKey));
            return;
        }
        // An ID through the server: ask the person at the host, as with nothing typed.
        if (Texts.NormalizeDeviceId(device.Device.Address) != null && _signalServer().Length > 0)
        {
            Start(device.Device.Address, new ConnectAuth.Request());
            return;
        }
        Address = Texts.NormalizeDeviceId(device.Device.Address) is { } id ? Texts.GroupDeviceId(id) : device.Device.Address;
        Code = "";
        Error = new ErrorText(L.Get("Connect.CodeNeeded"), L.Format("Connect.NotPairedDetail", device.Name));
        FocusCodeRequested?.Invoke();
    }

    /// <summary>The editor's changes: a name of the person's own (empty: the host's),
    /// a new password to keep (null: none; empty: leave as it is), or the kept one dropped.
    /// Null when done, else why not.</summary>
    internal string? UpdateRecent(RecentDeviceViewModel device, string label, string? password, bool forget)
    {
        if (password is { Length: > 0 } && Texts.SignInFor(password) is not ConnectAuth.Password)
            return L.Get("Connect.SavedPasswordInvalid");
        var at = _recent.FindIndex(r => r.Address == device.Device.Address && r.HostKey == device.HostKey);
        if (at < 0) return null;
        var entry = _recent[at] with { Label = label };
        if (forget)
        {
            _passwords.Delete(entry.PasswordAccount);
            entry = entry with { HasPassword = false };
        }
        else if (password is { Length: > 0 })
        {
            if (_passwords.Save(entry.PasswordAccount, password) is { } refused)
                return L.Format("Connect.SavedPasswordFailed", refused);
            entry = entry with { HasPassword = true };
        }
        _recent[at] = entry;
        _store.Save(_recent);
        RebuildRecent();
        return null;
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
        if (device.HasPassword) _passwords.Delete(device.Device.PasswordAccount);
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
        _passwordToKeep = null;
        _usingKept = null;
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
                var vm = new SessionViewModel(p.Session, c, Shown(p.Address), _now, _viewerPrefs);
                vm.Ended += OnSessionEnded;
                _sessions[p.Tag] = vm;
                _windows.ShowSession(vm);
                break;
            case DisconnectedEvent d:
                _pending = null;
                IsConnecting = false;
                ProgressText = "";
                p.Session.Close();
                _passwordToKeep = null;
                if (d.Reason == DisconnectReason.AuthFailed && _usingKept is { } stale)
                {
                    // The host's password changed: drop the kept one and ask for it again.
                    ForgetKept(stale);
                    Address = Shown(stale.Address);
                    Code = "";
                    Error = new ErrorText(L.Get("Connect.SavedPasswordStale"), L.Format("Connect.SavedPasswordStaleDetail", stale.Label.Length > 0 ? stale.Label : stale.Name));
                    FocusCodeRequested?.Invoke();
                }
                else if (d.Reason != DisconnectReason.ClosedLocally) Error = Failure(d.Reason, p.Address, d.Message);
                // A self-hosted server's certificate: offer to trust it (then connect again).
                if (d.Reason == DisconnectReason.SignalServer && ServerTrustFlow.IsCertificateProblem(d.Message))
                    SignalCertificateProblem?.Invoke(_signalServer());
                _usingKept = null;
                break;
            case WarningEvent w:
                ProgressText = Messages.Warning(w);
                break;
        }
    }

    /// <summary>A failure to get through to an address outside any tailnet points at Tailscale.
    /// The library's own reason follows as the last line (a certificate the server's
    /// CA does not sign, a refused request…): the general text alone cannot tell them apart.</summary>
    internal static ErrorText Failure(DisconnectReason reason, string address, string? message = null)
    {
        var e = Texts.Disconnect(reason);
        var unreachable = reason is DisconnectReason.Network or DisconnectReason.TimedOut or DisconnectReason.NoPath;
        var detail = unreachable && !Texts.IsTailnetTarget(address) ? $"{e.Detail}\n{Texts.TailscaleAdvice}" : e.Detail;
        if (!string.IsNullOrWhiteSpace(message)) detail += "\n" + L.Format("Connect.Reason", message.Trim());
        return new ErrorText(e.Title, detail);
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

    void ForgetKept(RecentDevice device)
    {
        _passwords.Delete(device.PasswordAccount);
        var at = _recent.FindIndex(r => r.Address == device.Address && r.HostKey == device.HostKey);
        if (at >= 0) _recent[at] = _recent[at] with { HasPassword = false };
        _store.Save(_recent);
        RebuildRecent();
    }

    void Remember(string address, ConnectedEvent c)
    {
        if (c.PairedNow && c.HostKey.Length > 0) _paired.Add(c.HostKey);
        // The person's name for it and its kept password stay with it.
        var before = _recent.FirstOrDefault(r => r.Address == address || (c.HostKey.Length > 0 && r.HostKey == c.HostKey));
        _recent.RemoveAll(r => r.Address == address || (c.HostKey.Length > 0 && r.HostKey == c.HostKey));
        var entry = new RecentDevice(address, c.HostName, c.HostKey, _now().ToUnixTimeSeconds(),
            before?.Label ?? "", before?.HasPassword ?? false);
        if (before is { HasPassword: true } && before.PasswordAccount != entry.PasswordAccount
            && _passwords.Load(before.PasswordAccount) is { } moved)
        {
            // Kept under the address before the host's key was known: file it under the key.
            _passwords.Save(entry.PasswordAccount, moved);
            _passwords.Delete(before.PasswordAccount);
        }
        if (_passwordToKeep is { } typed)
        {
            entry = entry with { HasPassword = _passwords.Save(entry.PasswordAccount, typed) == null };
            _passwordToKeep = null;
        }
        _usingKept = null;
        _recent.Insert(0, entry);
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
