using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
using NexaDesk.Proto;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

public enum HostState { Stopped, Starting, Hosting }

public sealed partial class PairedDeviceViewModel(HostViewModel owner, PairedDevice device, string lastSeen) : ObservableObject
{
    public PairedDevice Device { get; } = device;
    public string Name => string.IsNullOrWhiteSpace(Device.Name) ? L.Get("Host.UnnamedDevice") : Device.Name;
    public string Fingerprint => Texts.ShortFingerprint(Device.DeviceKey);
    public string PermsText => Texts.Permissions(Device.Perms);
    public string LastSeen { get; } = lastSeen;
    public bool Unattended => Device.Unattended;
    public string ConsentText => Device.Unattended ? L.Get("Host.ConsentNever") : L.Get("Host.ConsentAlways");

    [RelayCommand]
    private void Revoke() => owner.Revoke(this);

    [RelayCommand]
    private void RequireConsent() => owner.RequireConsent(this);
}

/// <summary>
/// Everyone connected, in the one banner at the top of the screen: a row each, oldest
/// first. Folded, it is a small tab with how many and for how long; never hidden.
/// </summary>
public sealed partial class BannerStackViewModel : ObservableObject
{
    public ObservableCollection<BannerViewModel> Sessions { get; } = new();

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsExpanded))]
    private bool _isCollapsed;

    public bool IsExpanded => !IsCollapsed;

    /// <summary>The folded tab: the oldest session's time, and how many when more than one.</summary>
    public string FoldedText => Sessions.Count switch
    {
        0 => "",
        1 => Sessions[0].DurationText,
        var n => L.Format("Banner.Folded", n, Sessions[0].DurationText),
    };

    public BannerStackViewModel() =>
        Sessions.CollectionChanged += (_, _) => OnPropertyChanged(nameof(FoldedText));

    [RelayCommand]
    private void ToggleCollapsed() => IsCollapsed = !IsCollapsed;

    public void Tick(DateTimeOffset now)
    {
        foreach (var s in Sessions) s.Tick(now);
        OnPropertyChanged(nameof(FoldedText));
    }
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
    [NotifyPropertyChangedFor(nameof(Headline), nameof(PermsText), nameof(Detail), nameof(HasInput), nameof(CanSendFiles), nameof(DropHint), nameof(IsTerminalOnly), nameof(IsAgent))]
    private HostSession _session;

    /// <summary>An AI agent, not a person: said in the headline and marked apart (docs/MCP.md §4.2).</summary>
    public bool IsAgent => Session.Agent;

    /// <summary>Files dropped here go to the viewer; files from it are listed here too.</summary>
    public TransferListViewModel Transfers { get; }
    public bool CanSendFiles => Session.Perms.File;
    public string DropHint => CanSendFiles
        ? L.Format("Host.DropHint", Texts.DownloadsFolder)
        : "";

    [ObservableProperty] private string _durationText = "00:00";
    [ObservableProperty] private bool _isDisconnecting;


    public ulong SessionId => Session.Id;
    public bool HasInput => Session.Perms.Input;
    /// <summary>The command line alone: nothing of the screen is shown.</summary>
    public bool IsTerminalOnly => !Session.Perms.View && Session.Perms.Terminal;
    public string Headline =>
        IsTerminalOnly ? L.Format("Host.UsingTerminal", Session.DeviceName)
        : IsAgent ? L.Format(HasInput ? "Host.AgentControlling" : "Host.AgentViewing", Session.DeviceName)
        : HasInput ? L.Format("Host.Controlling", Session.DeviceName)
        : L.Format("Host.Viewing", Session.DeviceName);
    public string PermsText => L.Format("Host.Perms", Texts.Permissions(Session.Perms));
    public string Detail => L.Format("Host.Detail", Texts.Remote(Session.Remote), Texts.ShortFingerprint(Session.DeviceKey));

    public void Tick(DateTimeOffset now) => DurationText = Texts.Duration(now - _startedAt);

    [RelayCommand]
    private void Disconnect()
    {
        IsDisconnecting = true;
        _link.Send(new UiRequest.Disconnect(SessionId));
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

/// <summary>
/// The addresses of one way in (the NexaDesk app, a browser) as shown: first the few most people
/// need, then the rest folded away. A Mac with virtual machines or containers has a bridge for
/// each, and listing them all side by side left people not knowing which one to give.
/// </summary>
public sealed partial class AddressGroup : ObservableObject
{
    public ObservableCollection<ConnectOption> Main { get; } = new();
    public ObservableCollection<ConnectOption> More { get; } = new();

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(MoreText))]
    private bool _expanded;

    /// <summary>Said once for the whole group (a browser's certificate warning), not on every row.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasNote))]
    private string _note = "";

    public bool HasNote => Note.Length > 0;
    public bool HasMore => More.Count > 0;
    public bool IsEmpty => Main.Count == 0;
    public string MoreText => Expanded ? L.Get("Host.FewerAddresses") : L.Format("Host.MoreAddresses", More.Count);

    [RelayCommand]
    private void ToggleMore() => Expanded = !Expanded;

    internal void Set(IEnumerable<ConnectOption> main, IEnumerable<ConnectOption> more, string note = "")
    {
        Main.Clear();
        foreach (var o in main) Main.Add(o);
        More.Clear();
        foreach (var o in more) More.Add(o);
        Note = note;
        OnPropertyChanged(nameof(HasMore));
        OnPropertyChanged(nameof(IsEmpty));
        OnPropertyChanged(nameof(MoreText));
    }
}

/// <summary>"允许别人控制本机": the agent, its code, and the devices paired with it.</summary>
public sealed partial class HostViewModel : ObservableObject
{
    static readonly TimeSpan StartTimeout = TimeSpan.FromSeconds(60);
    /// <summary>The longest start waits for the Keychain to give the server's key back.</summary>
    static readonly TimeSpan SecretWait = TimeSpan.FromSeconds(10);
    static readonly TimeSpan RetryEvery = TimeSpan.FromMilliseconds(250);

    readonly IAgentLink _link;
    readonly IAgentProcess _process;
    readonly IPermissionProbe _permissions;
    readonly IWindowService _windows;
    readonly Func<DateTimeOffset> _now;
    readonly string _deviceName;
    readonly Func<string> _signalServer;
    readonly Func<string> _signalSecret;
    readonly Func<Task> _secretReady;
    readonly Func<bool> _announceOnLan;
    readonly Func<bool> _allowBrowsers;
    readonly Func<bool> _allowAgents;
    readonly ILocalAddresses _addresses;
    /// <summary>Keeps whether hosting should come back at the next start (AppSettings.ResumeHosting).</summary>
    readonly Action<bool> _rememberHosting;
    /// <summary>Everyone connected now, oldest first: one banner each.</summary>
    readonly List<BannerViewModel> _banners = new();
    ConsentViewModel? _consent;
    CancellationTokenSource? _starting;
    bool _stopping;

    public ObservableCollection<PairedDeviceViewModel> Paired { get; } = new();

    /// <summary>Hosting with nothing saved: the list explains how a device gets there, since
    /// signing in with the fixed password never saves one.</summary>
    public bool NoPairedDevices => IsHosting && Paired.Count == 0;
    public IReadOnlyList<string> AddressSuggestions { get; }

    /// <summary>This Mac's Tailscale: how people outside the LAN reach it, with no server of ours.</summary>
    public TailscaleViewModel Tailscale { get; }
    public bool ShowTailscaleAdvice => Tailscale.IsAbsent;

    /// <summary>While hosting: every address the viewer may type, Tailscale first.</summary>
    public ObservableCollection<ConnectOption> ConnectOptions { get; } = new();

    /// <summary>The same ways in, grouped and with the less likely addresses folded, as the window shows them.</summary>
    public AddressGroup AppAddresses { get; } = new();
    public AddressGroup WebAddresses { get; } = new();

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
        ? L.Format("Host.ListenEverywhereSummary", Texts.DefaultPort)
        : L.Format("Host.ListenOnSummary", ListenAddress);
    public ConsentViewModel? Consent => _consent;
    /// <summary>The oldest session's banner.</summary>
    public BannerViewModel? Banner => _banners.FirstOrDefault();
    public IReadOnlyList<BannerViewModel> Banners => _banners;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsStopped), nameof(IsStarting), nameof(IsHosting), nameof(StateText), nameof(ShortStateText), nameof(RestartNeeded),
        nameof(NoPairedDevices))]
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

    /// <summary>The access key the running agent was started with; null as for the server.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(RestartNeeded))]
    private string? _startedWithSecret;

    /// <summary>The LAN announcement setting the running agent was started with; null as for the server.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(RestartNeeded))]
    private bool? _startedWithAnnounce;

    /// <summary>The browser setting the running agent was started with; null as for the server.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(RestartNeeded))]
    private bool? _startedWithBrowsers;

    public bool RestartNeeded => IsHosting &&
        ((StartedWithServer != null && StartedWithServer != _signalServer()) ||
         (StartedWithSecret != null && StartedWithSecret != _signalSecret()) ||
         (StartedWithAnnounce != null && StartedWithAnnounce != _announceOnLan()) ||
         (StartedWithBrowsers != null && StartedWithBrowsers != _allowBrowsers()));

    /// <summary>The port browsers connect to, next to the native one.</summary>
    public const int WebPort = 47801;

    /// <summary>The <c>--web</c> address for a native listen address: same host, <see cref="WebPort"/>.</summary>
    public static string WebListen(string listen)
    {
        var (host, _) = SplitListen(listen);
        return host.Contains(':') ? $"[{host}]:{WebPort}" : $"{host}:{WebPort}";
    }

    public void OnServerChanged() => OnPropertyChanged(nameof(RestartNeeded));

    /// <summary>The AI agent setting changed: the running agent takes it at once.</summary>
    public void OnAgentsChanged()
    {
        if (IsHosting) _link.Send(new UiRequest.SetAllowAgents(_allowAgents()));
    }
    [ObservableProperty] private string _hostFingerprint = "—";
    [ObservableProperty] private bool _inputAvailable = true;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasError))]
    private string? _error;

    [ObservableProperty] private string? _notice;

    bool _certificateAsked;

    /// <summary>The agent could not reach the signaling server because of its certificate.</summary>
    public event Action? SignalCertificateProblem;

    // ---- Fixed password ----

    /// <summary>The running agent has a fixed password, from its status.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(PasswordStateText), nameof(PasswordEditLabel))]
    private bool _passwordSet;
    [ObservableProperty] private bool _isEditingPassword;
    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(SavePasswordCommand), nameof(ClearPasswordCommand))]
    private bool _isSavingPassword;
    [ObservableProperty] private string _newPassword = "";
    [ObservableProperty] private string _confirmPassword = "";
    [ObservableProperty] private string? _passwordError;

    public string PasswordStateText => L.Get(PasswordSet ? "Host.PasswordOn" : "Host.PasswordOff");
    public string PasswordEditLabel => L.Get(PasswordSet ? "Host.ChangePassword" : "Host.SetPassword");

    [RelayCommand]
    private void EditPassword()
    {
        NewPassword = ConfirmPassword = "";
        PasswordError = null;
        IsEditingPassword = true;
    }

    [RelayCommand]
    private void CancelPassword()
    {
        NewPassword = ConfirmPassword = "";
        PasswordError = null;
        IsEditingPassword = false;
    }

    bool CanChangePassword() => !IsSavingPassword;

    static readonly TimeSpan AgentExitGrace = TimeSpan.FromSeconds(2);
    static readonly TimeSpan AgentExitPoll = TimeSpan.FromMilliseconds(100);

    [RelayCommand(CanExecute = nameof(CanChangePassword))]
    private async Task SavePassword()
    {
        if (Texts.PasswordProblem(NewPassword, ConfirmPassword) is { } problem)
        {
            PasswordError = L.Get(problem);
            return;
        }
        await ApplyPassword(NewPassword, "Host.PasswordSaved");
    }

    [RelayCommand(CanExecute = nameof(CanChangePassword))]
    private Task ClearPassword() => ApplyPassword(null, "Host.PasswordCleared");

    async Task ApplyPassword(string? password, string doneKey)
    {
        IsSavingPassword = true;
        PasswordError = null;
        var error = await _process.SetPassword(password);
        IsSavingPassword = false;
        if (error != null)
        {
            PasswordError = L.Format("Host.PasswordFailed", error);
            return;
        }
        NewPassword = ConfirmPassword = "";
        IsEditingPassword = false;
        await RestartHosting();
        Notice = L.Get(doneKey);
    }

    /// <summary>The running agent read its settings when it started: start a new one. Waits
    /// for the old one to be gone first, or the new start would attach to it as it shuts down.</summary>
    async Task RestartHosting()
    {
        if (!IsHosting) return;
        StopAgent();
        for (var waited = TimeSpan.Zero; _process.IsRunning && waited < AgentExitGrace; waited += AgentExitPoll)
            await Task.Delay(AgentExitPoll);
        if (_process.IsRunning) _process.Kill();
        await Start();
    }

    // ---- Device ID reset ----

    /// <summary>"Reset" was clicked once: the person confirms before the old ID goes.</summary>
    [ObservableProperty] private bool _isConfirmingIdReset;
    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ResetDeviceIdCommand))]
    private bool _isResettingId;

    [RelayCommand]
    private void AskResetDeviceId() => IsConfirmingIdReset = true;

    [RelayCommand]
    private void CancelResetDeviceId() => IsConfirmingIdReset = false;

    bool CanResetDeviceId() => !IsResettingId;

    [RelayCommand(CanExecute = nameof(CanResetDeviceId))]
    private async Task ResetDeviceId()
    {
        IsResettingId = true;
        Error = null;
        var error = await _process.ResetDeviceId();
        IsResettingId = false;
        IsConfirmingIdReset = false;
        if (error != null)
        {
            Notice = L.Format("Host.ResetIdFailed", error);
            return;
        }
        DeviceId = "";
        await RestartHosting();
        Notice = L.Get("Host.ResetIdDone");
    }

    public HostViewModel(IAgentLink link, IAgentProcess process, IPermissionProbe permissions, IWindowService windows,
        ILocalAddresses addresses, string deviceName, Func<DateTimeOffset>? now = null, Func<string>? signalServer = null,
        Func<bool>? announceOnLan = null, TailscaleViewModel? tailscale = null, Func<bool>? allowBrowsers = null,
        Action<bool>? rememberHosting = null, Func<string>? signalSecret = null, Func<Task>? secretReady = null,
        Func<bool>? allowAgents = null)
    {
        _allowAgents = allowAgents ?? (() => false);
        _secretReady = secretReady ?? (() => Task.CompletedTask);
        _rememberHosting = rememberHosting ?? (_ => { });
        _signalSecret = signalSecret ?? (() => "");
        Paired.CollectionChanged += (_, _) => OnPropertyChanged(nameof(NoPairedDevices));
        Tailscale = tailscale ?? new TailscaleViewModel(windows);
        Tailscale.Changed += () =>
        {
            OnPropertyChanged(nameof(ShowTailscaleAdvice));
            RebuildOptions();
        };
        _addresses = addresses;
        _signalServer = signalServer ?? (() => "");
        _announceOnLan = announceOnLan ?? (() => true);
        _allowBrowsers = allowBrowsers ?? (() => false);
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

    static readonly TimeSpan CopiedFor = TimeSpan.FromSeconds(2);
    int _copies;

    /// <summary>The value just copied, for two seconds, so its row can say so.</summary>
    [ObservableProperty] private string? _copied;

    [RelayCommand]
    private async Task Copy(string? text)
    {
        if (string.IsNullOrEmpty(text)) return;
        _windows.CopyText(text);
        Copied = text;
        var mine = ++_copies;
        await Task.Delay(CopiedFor);
        if (mine == _copies) Copied = null;
    }

    /// <summary>The sidebar's line, seen from every page: whether this Mac can be reached now.</summary>
    public string ShortStateText => State switch
    {
        HostState.Starting => L.Get("Host.SidebarStarting"),
        HostState.Hosting => L.Get("Host.SidebarOn"),
        _ => L.Get("Host.SidebarOff"),
    };

    public string StateText => State switch
    {
        HostState.Starting => L.Get("Host.Starting"),
        HostState.Hosting => L.Get("Host.HostingState"),
        _ => L.Get("Host.StoppedState"),
    };

    public string? ListenHint => !ListenEverywhere && Texts.NormalizeAddress(ListenAddress) == null ? L.Get("Host.ListenHint") : null;

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
        if (!IsHosting || ActiveListen.Length == 0)
        {
            AppAddresses.Set([], []);
            WebAddresses.Set([], []);
            return;
        }
        var (host, port) = SplitListen(ActiveListen);
        var suffix = port == Texts.DefaultPort ? "" : $":{port}";
        var everywhere = host is "0.0.0.0" or "::";
        var ips = everywhere ? _addresses.Candidates().Where(ip => ip != "127.0.0.1").ToList() : [host];
        var tailnet = ips.Where(IsTailnet).ToList();
        var lan = ips.Where(ip => !IsTailnet(ip)).ToList();
        // The LAN addresses to show first: those on a network with a router. The others are mostly
        // bridges to virtual machines and containers, which nobody on the LAN can reach.
        var routed = everywhere ? _addresses.Routed() : [];
        var mainLan = lan.Where(routed.Contains).ToList();
        if (mainLan.Count == 0) mainLan = [.. lan.Take(1)];
        var otherLan = lan.Where(ip => !mainLan.Contains(ip)).ToList();
        string LanKind(string ip) => ip is "127.0.0.1" or "::1" ? L.Get("Host.KindLocalOnly") : L.Get("Host.KindLan");
        // Folded ones: known not to be on a routed LAN, or simply not the first when that is unknown.
        string OtherKind(string ip) => routed.Count > 0 ? L.Get("Host.KindOtherNic") : LanKind(ip);

        var app = new List<ConnectOption>();
        if (tailnet.Count > 0 && Tailscale.DnsName.Length > 0)
            app.Add(new ConnectOption("Tailscale", Tailscale.DnsName + suffix, L.Format("Host.OrAddress", tailnet[0], suffix)));
        else
            app.AddRange(tailnet.Select(ip => new ConnectOption("Tailscale", Bracket(ip) + suffix)));
        foreach (var o in app) ConnectOptions.Add(o);
        foreach (var ip in lan)
            ConnectOptions.Add(new ConnectOption(LanKind(ip), Bracket(ip) + suffix));
        AppAddresses.Set(
            [.. app, .. mainLan.Select(ip => new ConnectOption(LanKind(ip), Bracket(ip) + suffix))],
            otherLan.Select(ip => new ConnectOption(OtherKind(ip), Bracket(ip) + suffix)));

        // Browser access: the page is served on the same addresses, MagicDNS name first.
        if (StartedWithBrowsers != true)
        {
            WebAddresses.Set([], []);
            return;
        }
        var web = tailnet.Count > 0 && Tailscale.DnsName.Length > 0
            ? [Tailscale.DnsName, .. lan]
            : ips;
        // HTTPS only. With our own certificate the browser warns once: the person checks
        // the start of the fingerprint it shows against this one.
        var selfSigned = _webFingerprint.Length > 0 && !_webCertCustom;
        var detail = selfSigned
            ? L.Format("Host.BrowserDetailSelfSigned", ShortFingerprint(_webFingerprint))
            : L.Get("Host.BrowserDetail");
        string Url(string h) => $"https://{Bracket(h)}:{WebPort}";
        foreach (var h in web.Where(h => h != "127.0.0.1" && h != "::1"))
            ConnectOptions.Add(new ConnectOption(L.Get("Host.KindBrowser"), Url(h), detail));
        // From anywhere: the signaling server serves the page; there they type the ID.
        ConnectOption? anywhere = null;
        if (HasDeviceId && ServerPage(StartedWithServer ?? _signalServer()) is { } page)
        {
            ConnectOptions.Add(new ConnectOption(L.Get("Host.KindBrowserServer"), page, L.Format("Host.BrowserServerDetail", DeviceIdDisplay)));
            anywhere = new ConnectOption(L.Get("Host.KindAnywhere"), page, L.Format("Host.BrowserServerDetail", DeviceIdDisplay));
        }
        bool Shown(string h) => h != "127.0.0.1" && h != "::1";
        var webTail = tailnet.Count > 0 && Tailscale.DnsName.Length > 0 ? [Tailscale.DnsName] : tailnet;
        WebAddresses.Set(
            [
                .. webTail.Select(h => new ConnectOption("Tailscale", Url(h))),
                .. mainLan.Where(Shown).Select(ip => new ConnectOption(L.Get("Host.KindLan"), Url(ip))),
                .. anywhere is null ? [] : new[] { anywhere },
            ],
            otherLan.Where(Shown).Select(ip => new ConnectOption(OtherKind(ip), Url(ip))),
            selfSigned ? L.Format("Host.WebCertNote", ShortFingerprint(_webFingerprint)) : "");
    }

    partial void OnStateChanged(HostState value) => RebuildOptions();

    /// <summary>The page on a signaling server, from its wss:// address; null for none.</summary>
    public static string? ServerPage(string? server)
    {
        if (string.IsNullOrWhiteSpace(server)) return null;
        var s = server.Trim();
        if (s.StartsWith("wss://", StringComparison.OrdinalIgnoreCase)) s = s[6..];
        else if (s.Contains("://")) return null;
        var authority = s.Split('/', '?', '#')[0];
        return authority.Length == 0 ? null : $"https://{authority}/";
    }

    string _webFingerprint = "";
    bool _webCertCustom;

    /// <summary>The first eight bytes of a certificate fingerprint, as browsers print them.</summary>
    public static string ShortFingerprint(string fp) => string.Join(":", fp.Split(':').Take(8));

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
            Error = L.Get("Host.BadListen");
            return;
        }
        if (!_permissions.Check().ScreenRecording)
        {
            Error = L.Get("Host.NeedScreenRecording");
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
            // The key comes out of the Keychain at start; do not run ahead of it (but not for ever).
            await Task.WhenAny(_secretReady(), Task.Delay(SecretWait));
            var server = _signalServer();
            var secret = _signalSecret();
            var announce = _announceOnLan();
            var browsers = _allowBrowsers();
            StartedWithServer = server;
            StartedWithSecret = secret;
            StartedWithAnnounce = announce;
            StartedWithBrowsers = browsers;
            var why = _process.Start(listen, _deviceName, server, announce, browsers ? WebListen(listen) : null, secret);
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
            Error ??= L.Get("Host.StartTimeout");
        }
    }

    /// <summary>
    /// At startup, when remote control should be on (Windows login, or AppSettings.ResumeHosting):
    /// an agent still running is picked up, else a new one is started. Start checks Screen
    /// Recording first and, without it, starts nothing and shows the usual notice and guide.
    /// </summary>
    public async Task AttachOrStartAsync()
    {
        await TryAttachExistingAsync();
        if (StartCommand.CanExecute(null)) await StartCommand.ExecuteAsync(null);
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
        _rememberHosting(true);
        _link.Send(new UiRequest.ListPairings());
        // The agent starts with agents refused; this is the person's setting.
        _link.Send(new UiRequest.SetAllowAgents(_allowAgents()));
    }

    /// <summary>The person stops remote control (here or in the menu bar): it stays off at the next
    /// start too. Quitting, an update restart and the agent dying leave the choice as it was.</summary>
    [RelayCommand(CanExecute = nameof(IsHosting))]
    private void Stop()
    {
        _rememberHosting(false);
        StopAgent();
    }

    void StopAgent()
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
                _webFingerprint = s.WebFingerprint;
                PasswordSet = s.PasswordSet;
                _webCertCustom = s.WebCertCustom;
                RebuildOptions();
                SignalProblem = s.Signal == SignalState.Error;
                // Once per start: the agent cannot trust a self-hosted server's certificate.
                if (SignalProblem && !_certificateAsked && ServerTrustFlow.IsCertificateProblem(s.SignalDetail))
                {
                    _certificateAsked = true;
                    SignalCertificateProblem?.Invoke();
                }
                if (s.Session != null) ShowBanner(s.Session);
                break;
            case AgentMessage.IncomingRequest r:
                // The agent asks one question at a time; a new one replaces anything stale.
                _consent?.Expire(L.Get("Host.Superseded"));
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
                HideBanner(e.Id);
                Notice = L.Format("Host.SessionEnded", e.DeviceName);
                _link.Send(new UiRequest.ListPairings());
                break;
            case AgentMessage.Pairings p:
                Paired.Clear();
                var now = _now();
                foreach (var d in p.Devices)
                    Paired.Add(new PairedDeviceViewModel(this, d, Texts.Ago(DateTimeOffset.FromUnixTimeSeconds((long)d.LastSeen), now)));
                break;
            case AgentMessage.Notice n:
                Notice = Messages.Notice(n);
                break;
            case AgentMessage.Transfer t:
                _banners.FirstOrDefault(b => b.SessionId == t.SessionId)?.Transfers.OnUpdate(t.Info);
                break;
            case AgentMessage.Refused { Command: "files" } r when _banners.LastOrDefault(b => b.CanSendFiles) is { } sender:
                sender.Transfers.Error = L.Format("Host.CannotSend", r.Reason);
                break;
            case AgentMessage.Refused r:
                Error = r.Command == "policy"
                    ? L.Get("Host.PolicyRefused")
                    : L.Format("Host.Refused", r.Reason);
                break;
        }
    }

    void ShowBanner(HostSession session)
    {
        if (_banners.FirstOrDefault(b => b.SessionId == session.Id) is { } shown)
        {
            shown.Session = session;
            return;
        }
        var banner = new BannerViewModel(session, _link, _now());
        _banners.Add(banner);
        OnPropertyChanged(nameof(Banner));
        _windows.ShowBanner(banner);
    }

    void HideBanner(ulong id)
    {
        if (_banners.FirstOrDefault(b => b.SessionId == id) is not { } banner) return;
        _banners.Remove(banner);
        OnPropertyChanged(nameof(Banner));
        _windows.HideBanner(banner);
    }

    void HideBanners()
    {
        foreach (var b in _banners.ToList()) HideBanner(b.SessionId);
    }

    void Stopped(string? error)
    {
        _starting?.Cancel();
        State = HostState.Stopped;
        Code = "";
        ActiveListen = "";
        StartedWithServer = null;
        StartedWithSecret = null;
        StartedWithAnnounce = null;
        DeviceId = "";
        SignalText = "";
        ConnectOptions.Clear();
        SignalProblem = false;
        _certificateAsked = false;
        Paired.Clear();
        // With no UI the agent itself ends the session and denies the open question.
        _consent?.Expire(L.Get("Host.AgentStopped"));
        HideBanners();
        if (error != null) Error = error;
    }

    void OnLinkLost(string? why)
    {
        if (State == HostState.Stopped) return;
        Stopped(_stopping ? null : L.Get("Host.LinkLost"));
    }

    void OnProcessExited(AgentExit exit)
    {
        if (_stopping && State == HostState.Stopped) return;
        var text = exit.LastOutput;
        string message;
        if (text.Contains("Screen Recording", StringComparison.OrdinalIgnoreCase))
        {
            message = L.Get("Host.ExitScreenRecording");
            ShowPermissionGuide();
        }
        else if (text.Contains("cannot listen", StringComparison.OrdinalIgnoreCase))
            message = L.Format("Host.ExitCannotListen", ListenAddress);
        else if (text.Contains("already running", StringComparison.OrdinalIgnoreCase))
            message = L.Get("Host.ExitAlreadyRunning");
        else if (text.Contains("device key", StringComparison.OrdinalIgnoreCase))
            message = L.Get("Host.ExitDeviceKey");
        else
            message = L.Format("Host.ExitUnexpected", exit.Code);
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
    public string? Register() => L.Get("LoginItem.NeedsMacOS13");
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

    public string Title => L.Get("Host.PermTitle");

    public string Intro => Context == PermissionContext.Hosting
        ? L.Get("Host.PermIntroHosting")
        : L.Get("Host.PermIntroViewing");

    public bool ShowScreenRecording => Context == PermissionContext.Hosting;
    public string ScreenRecordingStatus => ScreenRecording ? L.Get("Host.PermGranted") : L.Get("Host.PermNotGranted");
    public string AccessibilityStatus => Accessibility ? L.Get("Host.PermGranted") : L.Get("Host.PermNotGranted");
    public string ScreenRecordingUse => L.Get("Host.PermScreenRecordingUse");
    public string AccessibilityUse => Context == PermissionContext.Hosting
        ? L.Get("Host.PermAccessibilityUseHosting")
        : L.Get("Host.PermAccessibilityUseViewing");
    public bool AllGranted => Accessibility && (ScreenRecording || Context != PermissionContext.Hosting);
    public string RestartNote => L.Get("Host.PermRestartNote");

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
    private void RequestAccessibility() => _probe.Request(PermissionKind.Accessibility);

    [RelayCommand]
    private void OpenAccessibility() => _probe.OpenSettings(PermissionKind.Accessibility);

    [RelayCommand]
    private void Close() => CloseRequested?.Invoke();
}

/// <summary>The main window's pages, one per thing a person comes to do; the sidebar picks one.</summary>
public enum MainPage { Connect, Host, Paired, Settings }

/// <summary>The main window: a sidebar and its pages, and what they share.</summary>
public sealed partial class MainViewModel : ObservableObject
{
    readonly IClientCore _core;

    /// <summary>The page shown. Everything that is set once and left (language, the server,
    /// browsers, the fixed password…) is on Settings, so the other pages keep to their task.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsConnectPage), nameof(IsHostPage), nameof(IsPairedPage), nameof(IsSettingsPage))]
    private MainPage _page;

    public bool IsConnectPage => Page == MainPage.Connect;
    public bool IsHostPage => Page == MainPage.Host;
    public bool IsPairedPage => Page == MainPage.Paired;
    public bool IsSettingsPage => Page == MainPage.Settings;

    [RelayCommand]
    private void ShowPage(string page)
    {
        if (Enum.TryParse<MainPage>(page, out var p)) Page = p;
    }

    public ConnectViewModel Connect { get; }
    public HostViewModel Host { get; }
    public SettingsViewModel Settings { get; }
    /// <summary>Shared by both halves.</summary>
    public TailscaleViewModel Tailscale { get; }

    public MainViewModel(IClientCore core, IRecentStore recent, IAgentLink link, IAgentProcess process,
        IPermissionProbe permissions, IWindowService windows, ILocalAddresses addresses, string deviceName,
        Func<DateTimeOffset>? now = null, ISettingsStore? settings = null, ILoginItem? loginItem = null,
        IKeymapService? keymap = null, IViewerPrefs? viewerPrefs = null, ISavedPasswords? passwords = null,
        IAppUpdater? updater = null, IServerTrust? serverTrust = null, IServerDirectory? serverDirectory = null, IServerProbe? serverProbe = null)
    {
        _keymap = keymap;
        _core = core;
        DeviceName = deviceName;
        settings ??= new MemorySettings();
        _settings = settings;
        Settings = new SettingsViewModel(settings, passwords, serverDirectory, serverProbe);
        Tailscale = new TailscaleViewModel(windows);
        Connect = new ConnectViewModel(core, recent, windows, now, () => Settings.SavedServer, Tailscale, viewerPrefs, passwords,
            () => Settings.SavedSecret);
        Host = new HostViewModel(link, process, permissions, windows, addresses, deviceName, now, () => Settings.SavedServer,
            () => Settings.AnnounceOnLan, Tailscale, () => Settings.AllowBrowsers, RememberHosting, () => Settings.SavedSecret,
            () => Settings.SecretLoaded, () => Settings.AllowAgents);
        Settings.ServerChanged += _ =>
        {
            Connect.OnServerChanged();
            Host.OnServerChanged();
        };
        Settings.AnnounceChanged += _ => Host.OnServerChanged();
        Settings.BrowsersChanged += _ => Host.OnServerChanged();
        Settings.AgentsChanged += _ => Host.OnAgentsChanged();
        _permissions = permissions;
        _windows = windows;
        LoginItem = new LoginItemViewModel(loginItem ?? new NoLoginItem());
        Tray = new TrayViewModel(Host);
        Updates = new UpdatesViewModel(updater ?? NullAppUpdater.Instance, () => Tray.QuitCommand.Execute(null));
        About = new AboutViewModel(windows);
        Mcp = McpViewModel.ForThisApp(windows);
        ServerTrust = new ServerTrustFlow(serverTrust ?? new NoServerTrust(), windows);
        // A server saved in settings is checked at once; a failure on its certificate,
        // connecting or hosting, asks too.
        Settings.ServerChanged += server => _ = CheckServerAsync(server, retryConnect: false);
        Connect.SignalCertificateProblem += server => _ = CheckServerAsync(server, retryConnect: true);
        Host.SignalCertificateProblem += () => _ = CheckServerAsync(Settings.SavedServer, retryConnect: false);
    }

    public ServerTrustFlow ServerTrust { get; }

    readonly ISettingsStore _settings;

    void RememberHosting(bool on)
    {
        var s = _settings.Load();
        if (s.ResumeHosting != on) _settings.Save(s with { ResumeHosting = on });
    }

    /// <summary>
    /// Whether this start turns remote control on, so that someone who restarts NexaDesk from
    /// afar (an update, or quit and reopen) is not locked out. On by default, a fresh install
    /// included. Security: the person's own Stop switches it off for every later start until
    /// they start hosting again; nothing else does. Being on only opens the door as Start
    /// always has: consent, pairings, the one-time code and the password still apply, and on
    /// macOS nothing starts without Screen Recording.
    /// </summary>
    public bool ResumesHosting => _settings.Load().ResumeHosting;

    async Task CheckServerAsync(string server, bool retryConnect)
    {
        if (string.IsNullOrWhiteSpace(server)) return;
        switch (await ServerTrust.EnsureAsync(server))
        {
            case TrustOutcome.NowTrusted when retryConnect:
                Connect.Error = null;
                if (Connect.ConnectCommand.CanExecute(null)) Connect.ConnectCommand.Execute(null);
                if (Host.IsHosting) Host.Notice = L.Get("Trust.SavedHost");
                break;
            case TrustOutcome.NowTrusted:
                Host.Notice = L.Get(Host.IsHosting ? "Trust.SavedHost" : "Trust.Saved");
                break;
            case TrustOutcome.Failed when !retryConnect && ServerTrust.LastError is { } why:
                Host.Notice = L.Format("Trust.Failed", why);
                break;
        }
    }

    /// <summary>The version and updates from GitHub releases.</summary>
    public UpdatesViewModel Updates { get; }
    public AboutViewModel About { get; }
    /// <summary>How to give an AI on this computer NexaDesk's MCP server.</summary>
    public McpViewModel Mcp { get; }

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
