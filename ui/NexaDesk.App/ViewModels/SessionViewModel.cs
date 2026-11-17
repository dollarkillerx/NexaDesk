using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
using NexaDesk.Native;
using NexaDesk.Proto;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>The windows the view-models ask for. Tests record the calls instead.</summary>
public interface IWindowService
{
    void ShowSession(SessionViewModel session);
    void ShowConsent(ConsentViewModel consent);
    /// <summary>One banner per person connected, stacked at the top of the screen.</summary>
    void ShowBanner(BannerViewModel banner);
    void HideBanner(BannerViewModel banner);
    void ShowPermissionGuide(PermissionGuideViewModel guide);
    void ShowDiagnostics(DiagnosticsViewModel diagnostics);
    void ShowKeyboardSettings(KeyboardSettingsViewModel settings);
    /// <summary>Opens a fixed https link in the default browser.</summary>
    void OpenUrl(string url);
    /// <summary>Puts text on the clipboard.</summary>
    void CopyText(string text);
    /// <summary>"Trust this signaling server?"; the view model reports the answer.</summary>
    void ShowServerTrust(ServerTrustViewModel trust);
}

/// <summary>
/// One viewing session: the session window, its toolbar, and whether local input is
/// captured. Capture follows the window: on while it is active, off when it is not,
/// off after the host key until the person resumes (§7.4).
/// </summary>
public sealed partial class SessionViewModel : ObservableObject
{
    readonly Func<DateTimeOffset> _now;
    bool _windowActive;
    bool? _inputSent;
    double _wheelX, _wheelY;

    public ISession Session { get; }
    public string Address { get; }
    public string HostName { get; }
    public string HostKey { get; }
    public Permissions Perms { get; }
    public DiagnosticsViewModel Diagnostics { get; }
    /// <summary>Files to and from the host; only with the file permission.</summary>
    public TransferListViewModel Transfers { get; }
    public HudViewModel Hud { get; } = new();
    public bool CanSendFiles => Perms.File;
    /// <summary>The host granted its command line: the toolbar offers a terminal.</summary>
    public bool CanUseTerminal => Perms.Terminal;
    public string Title => $"{HostName} — NexaDesk";
    public string PermsText => Texts.Permissions(Perms);
    public string HostKeyHint => L.Get("Session.HostKeyHint");
    public bool CanControl => Perms.Input;
    /// <summary>The host shared its sound: the toolbar offers mute.</summary>
    public bool CanHearAudio => Perms.Audio;
    /// <summary>The host's displays, for the toolbar's picker.</summary>
    public IReadOnlyList<DisplayChoice> Displays { get; }
    public bool HasManyDisplays => Displays.Count > 1;
    public bool IsEnded { get; private set; }

    /// <summary>The session ended; the windows close. Not raised twice.</summary>
    public event Action<SessionViewModel, DisconnectReason, string>? Ended;
    /// <summary>The person wants control back: bring the session window to the front.</summary>
    public event Action? ActivateRequested;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsOffice), nameof(IsBalanced), nameof(IsGame), nameof(ModeText))]
    private StreamMode _mode = StreamMode.Balanced;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CaptureText), nameof(CaptureNeedsPermission))]
    private CaptureState _capture = CaptureState.Inactive;

    [ObservableProperty] private string _clipboardText;
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsToolbarCollapsed))]
    private bool _isToolbarVisible = true;

    /// <summary>The toolbar is folded into a small tab (host name and ▾) that unfolds it.</summary>
    public bool IsToolbarCollapsed => !IsToolbarVisible;
    /// <summary>The key that releases the keyboard and shows the toolbar, as this platform writes it.</summary>
    public static string ShowToolbarKeys => OperatingSystem.IsWindows() ? "Ctrl+Alt+G" : "⌃⌥⌘G";
    /// <summary>The tab's tooltip, and the collapse button's: how the toolbar comes back.</summary>
    public string ShowToolbarTip => L.Format("Toolbar.ExpandOrMove", ShowToolbarKeys);
    public string CollapseToolbarTip => L.Format("Toolbar.CollapseTip", ShowToolbarKeys);

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CaptureText))]
    private bool _isSuspended;

    [ObservableProperty] private string _streamText = L.Get("Session.WaitingForVideo");
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasWarning))]
    private string? _warning;

    public bool HasWarning => Warning != null && !IsReconnecting;

    /// <summary>§4.4: the transport dropped; the window stays while the library reconnects.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CaptureText), nameof(HasWarning))]
    private bool _isReconnecting;

    [ObservableProperty] private string? _reconnectText;

    /// <summary>The library starts sound playing when the host shares it.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(AudioText), nameof(MuteLabel))]
    private bool _isMuted;

    [ObservableProperty] private DisplayChoice? _selectedDisplay;

    /// <summary>The latency overlay over the video (toolbar → HUD).</summary>
    [ObservableProperty] private bool _isHudVisible;

    [RelayCommand]
    private void ToggleHud() => IsHudVisible = !IsHudVisible;

    public SessionViewModel(ISession session, ConnectedEvent connected, string address, Func<DateTimeOffset>? now = null,
        IViewerPrefs? prefs = null)
    {
        _prefs = prefs ?? new MemoryViewerPrefs();
        Session = session;
        Address = address;
        HostName = string.IsNullOrWhiteSpace(connected.HostName) ? address : connected.HostName;
        HostKey = connected.HostKey;
        Perms = connected.Perms;
        _now = now ?? (() => DateTimeOffset.Now);
        Diagnostics = new DiagnosticsViewModel(address);
        Transfers = new TransferListViewModel(new SessionTransfers(session));
        _clipboardText = Perms.Clipboard ? L.Get("Session.ClipboardSync") : L.Get("Session.ClipboardDenied");
        Displays = (connected.Displays ?? []).Select(DisplayChoice.Of).ToList();
        _displayFromHost = true;
        SelectedDisplay = Displays.FirstOrDefault();
        _displayFromHost = false;
        // The last session's choices; the host starts on Auto, so only others are said.
        var saved = _prefs.Load();
        _selectedQuality = Qualities.FirstOrDefault(q => q.Key == saved.Quality) ?? Qualities[0];
        _selectedResolution = Resolutions.FirstOrDefault(r => r.Key == saved.Resolution) ?? Resolutions[0];
        if (_selectedQuality != Qualities[0]) Session.Send(new SessionCommand.SetQuality(QualityOf(_selectedQuality.Key)));
        if (_selectedResolution != Resolutions[0]) Session.Send(new SessionCommand.SetResolution(LinesOf(_selectedResolution.Key)));
    }

    // ----- picture: quality and resolution ---------------------------------------------

    readonly IViewerPrefs _prefs;

    public IReadOnlyList<StreamChoice> Qualities { get; } =
    [
        new("auto", L.Get("Session.Quality.Auto")),
        new("clear", L.Get("Session.Quality.Clear")),
        new("balanced", L.Get("Session.Quality.Balanced")),
        new("weak", L.Get("Session.Quality.Weak")),
    ];

    public IReadOnlyList<StreamChoice> Resolutions { get; } =
    [
        new("auto", L.Get("Session.Resolution.Auto")),
        new("native", L.Get("Session.Resolution.Native")),
        new("1080", "1080p"),
        new("720", "720p"),
    ];

    [ObservableProperty] private StreamChoice _selectedQuality;
    [ObservableProperty] private StreamChoice _selectedResolution;

    public static StreamQuality QualityOf(string key) => key switch
    {
        "clear" => StreamQuality.Clear,
        "balanced" => StreamQuality.Balanced,
        "weak" => StreamQuality.Weak,
        _ => StreamQuality.Auto,
    };

    public static uint LinesOf(string key) => key switch
    {
        "native" => SessionCommand.SetResolution.Native,
        "1080" => 1080,
        "720" => 720,
        _ => SessionCommand.SetResolution.FitView,
    };

    partial void OnSelectedQualityChanged(StreamChoice value)
    {
        if (IsEnded || value == null) return;
        Session.Send(new SessionCommand.SetQuality(QualityOf(value.Key)));
        _prefs.Save(_prefs.Load() with { Quality = value.Key });
    }

    partial void OnSelectedResolutionChanged(StreamChoice value)
    {
        if (IsEnded || value == null) return;
        Session.Send(new SessionCommand.SetResolution(LinesOf(value.Key)));
        _prefs.Save(_prefs.Load() with { Resolution = value.Key });
    }

    public string AudioText => !Perms.Audio ? L.Get("Session.AudioNotShared") : IsMuted ? L.Get("Session.AudioMuted") : L.Get("Session.AudioPlaying");
    public string MuteLabel => IsMuted ? L.Get("Session.Unmute") : L.Get("Session.Mute");

    [RelayCommand]
    private void ToggleMute() => IsMuted = !IsMuted;

    partial void OnIsMutedChanged(bool value)
    {
        if (IsEnded || !Perms.Audio) return;
        Session.Send(new SessionCommand.SetAudio(!value));
    }

    bool _displayFromHost;

    partial void OnSelectedDisplayChanged(DisplayChoice? value)
    {
        if (_displayFromHost || IsEnded || value == null) return;
        Session.Send(new SessionCommand.SelectDisplay(value.Id));
    }

    public bool IsOffice { get => Mode == StreamMode.Office; set { if (value) Mode = StreamMode.Office; } }
    public bool IsBalanced { get => Mode == StreamMode.Balanced; set { if (value) Mode = StreamMode.Balanced; } }
    public bool IsGame { get => Mode == StreamMode.Game; set { if (value) Mode = StreamMode.Game; } }
    public string ModeText => Texts.Mode(Mode);

    public string CaptureText => IsReconnecting
        ? L.Get("Session.CaptureReconnecting")
        : IsSuspended && Perms.Input
            ? L.Get("Session.CaptureSuspended")
            : Texts.Capture(Capture, Perms.Input);

    public bool CaptureNeedsPermission => Perms.Input && Capture is CaptureState.Degraded or CaptureState.Unavailable;

    bool _modeFromHost;

    partial void OnModeChanged(StreamMode value)
    {
        if (_modeFromHost || IsEnded) return;
        Session.Send(new SessionCommand.SetMode(value));
    }

    // ----- capture follows the window --------------------------------------------------

    public void OnWindowActivated()
    {
        _windowActive = true;
        UpdateCapture();
    }

    public void OnWindowDeactivated()
    {
        _windowActive = false;
        UpdateCapture();
    }

    /// <summary>The host key was pressed: give the keyboard back and show the toolbar.</summary>
    public void OnHostKey()
    {
        IsSuspended = true;
        IsToolbarVisible = true;
        UpdateCapture();
    }

    [RelayCommand]
    private void Resume()
    {
        if (IsEnded) return;
        IsSuspended = false;
        ActivateRequested?.Invoke();
        UpdateCapture();
    }

    /// <summary>The tab, the menu item or the shortcut: the toolbar unfolds, capture is untouched.</summary>
    [RelayCommand]
    private void ShowToolbar() => IsToolbarVisible = true;

    /// <summary>Folds the toolbar into its tab.</summary>
    [RelayCommand]
    private void CollapseToolbar() => IsToolbarVisible = false;

    /// <summary>Whether input is captured right now, as last asked of the library.</summary>
    public bool InputActive => _inputSent == true;

    void UpdateCapture()
    {
        var want = Perms.Input && _windowActive && !IsSuspended && !IsEnded && !IsReconnecting;
        if (_inputSent == want) return;
        _inputSent = want;
        var rc = Session.SetInputActive(want);
        if (want && rc == NativeMethods.NEXA_E_NOT_PERMITTED)
        {
            _inputSent = false;
            Capture = CaptureState.Unavailable;
        }
    }

    // ----- pointer, in view pixels ----------------------------------------------------

    /// <summary>A click in the video while released resumes control instead of being sent.</summary>
    public bool OnVideoPressed()
    {
        if (!IsSuspended || !Perms.Input) return false;
        Resume();
        return true;
    }

    public void PointerMoved(double x, double y)
    {
        if (InputActive) Session.Send(new SessionCommand.PointerMove(x, y));
    }

    public void PointerButton(uint button, bool down)
    {
        if (InputActive) Session.Send(new SessionCommand.PointerButton(button, down));
    }

    /// <summary>Wheel deltas in notches (Avalonia: +y is up, +x is left). Fractions accumulate.</summary>
    public void Wheel(double dx, double dy)
    {
        if (!InputActive) return;
        _wheelX += -dx * 120;
        _wheelY += dy * 120;
        var ix = (int)Math.Truncate(_wheelX);
        var iy = (int)Math.Truncate(_wheelY);
        if (ix == 0 && iy == 0) return;
        _wheelX -= ix;
        _wheelY -= iy;
        Session.Send(new SessionCommand.PointerWheel(ix, iy));
    }

    // ----- lifetime -------------------------------------------------------------------

    [RelayCommand]
    private void Disconnect() => Finish(DisconnectReason.ClosedLocally, "");

    public void OnWindowClosing()
    {
        if (!IsEnded) Disconnect();
    }

    public void OnEvent(ClientEvent ev)
    {
        switch (ev)
        {
            case StreamChangedEvent s:
                _modeFromHost = true;
                Mode = s.Mode;
                _modeFromHost = false;
                // What the host streams is the truth, whoever asked for it.
                _displayFromHost = true;
                SelectedDisplay = Displays.FirstOrDefault(d => d.Id == s.Display) ?? SelectedDisplay;
                _displayFromHost = false;
                StreamText = $"{Texts.Codec(s.Codec)} · {s.Width}×{s.Height} · {s.Fps} fps";
                Diagnostics.OnStream(s);
                break;
            case InputCaptureEvent c:
                Capture = c.State;
                break;
            case PasteGateEvent g:
                ClipboardText = g.TimedOut
                    ? L.Format("Session.PasteTimedOut", g.WaitedMs)
                    : L.Format("Session.PasteWaited", g.WaitedMs);
                break;
            case ClipboardNoticeEvent n:
                ClipboardText = L.Format("Session.ClipboardNotice", Messages.Clipboard(n));
                break;
            case StatsEvent s:
                var at = _now();
                Diagnostics.Update(s.Stats, at);
                Hud.Update(s.Stats, at);
                break;
            case TransferEvent t:
                Transfers.OnUpdate(t.Info);
                break;
            case HostKeyEvent:
                OnHostKey();
                break;
            case WarningEvent w:
                Warning = Messages.Warning(w);
                break;
            case ReconnectingEvent r:
                IsReconnecting = true;
                ReconnectText = Texts.Reconnecting(r.Attempt, r.RemainingMs);
                IsToolbarVisible = true;
                Diagnostics.OnReconnecting();
                UpdateCapture();
                break;
            case ConnectedEvent when IsReconnecting:
                IsReconnecting = false;
                ReconnectText = null;
                // The old path's warning may no longer hold; a new one comes with the stats.
                Warning = null;
                UpdateCapture();
                break;
            case DisconnectedEvent d:
                Finish(d.Reason, d.Message);
                break;
        }
    }

    void Finish(DisconnectReason reason, string message)
    {
        if (IsEnded) return;
        IsEnded = true;
        // Closing stops local capture before anything else (nexa_session_close).
        Session.Close();
        _inputSent = false;
        Ended?.Invoke(this, reason, message);
    }
}

/// <summary>The viewer's transfers go through the session handle.</summary>
sealed class SessionTransfers(ISession session) : ITransferControl
{
    public void Send(IReadOnlyList<string> paths) => session.Send(new SessionCommand.SendFiles(paths));
    public void Cancel(ulong id) => session.Send(new SessionCommand.CancelTransfer(id));
    public void Answer(ulong id, bool accept) => session.Send(new SessionCommand.AnswerTransfer(id, accept));
}

/// <summary>Connection diagnostics from the once-a-second Stats event.</summary>
public sealed partial class DiagnosticsViewModel(string address) : ObservableObject
{
    StreamStats? _last;
    DateTimeOffset _lastAt;

    /// <summary>
    /// From the address until the first Stats says how packets really travel:
    /// 直连 / 打洞 / 中继 / Tailscale (§11).
    /// </summary>
    [ObservableProperty]
    private string _pathText = Texts.NormalizeDeviceId(address) != null || address.StartsWith("ID ")
        ? L.Get("Diagnostics.Establishing")
        : Texts.PathOf(address);

    [ObservableProperty] private string _rttText = "—";
    [ObservableProperty] private string _lossText = "—";
    [ObservableProperty] private string _fpsText = "—";
    [ObservableProperty] private string _bitrateText = "—";
    [ObservableProperty] private string _streamText = "—";
    [ObservableProperty] private string _recoveryText = "—";
    [ObservableProperty] private string _pasteText = "—";
    [ObservableProperty] private string _audioText = "—";

    public void OnReconnecting() => PathText = L.Get("Diagnostics.Reconnecting");

    public void OnStream(StreamChangedEvent s) =>
        StreamText = L.Format("Diagnostics.StreamInfo", Texts.Codec(s.Codec), s.Width, s.Height, s.Fps, Texts.Mode(s.Mode));

    public void Update(StreamStats s, DateTimeOffset now)
    {
        RttText = s.RttUs == 0 ? "—" : $"{s.RttUs / 1000.0:F1} ms";
        if (s.Path != PathKind.Unknown)
        {
            var path = Texts.Path(s.Path, s.PathDetail);
            PathText = s.PathRttUs == 0 ? path : L.Format("Diagnostics.PathProbe", path, s.PathRttUs / 1000.0);
        }
        RecoveryText = L.Format("Diagnostics.RecoveryInfo", s.FecRecovered, s.DecodeErrors);
        PasteText = L.Format("Diagnostics.PasteInfo", s.GatePastes, s.GateHeld, s.GateTimeouts);
        if (s.AudioPlayed > 0 || s.AudioConcealed > 0)
            AudioText = L.Format("Diagnostics.AudioInfo", s.AudioBufferUs / 1000.0, s.AudioConcealed, s.AudioPlayed + s.AudioConcealed);
        var last = _last;
        var lastAt = _lastAt;
        _last = s;
        _lastAt = now;
        if (last == null) return;
        var dt = (now - lastAt).TotalSeconds;
        // A new pipeline restarts the counters; skip that interval.
        if (dt <= 0 || s.Complete < last.Complete || s.Lost < last.Lost || s.Presented < last.Presented || s.RxBytes < last.RxBytes)
            return;
        var complete = s.Complete - last.Complete;
        var lost = s.Lost - last.Lost;
        LossText = complete + lost == 0 ? "0.0 %" : $"{100.0 * lost / (complete + lost):F1} %";
        FpsText = $"{(s.Presented - last.Presented) / dt:F0} fps";
        var mbps = (s.RxBytes - last.RxBytes) * 8 / dt / 1e6;
        BitrateText = mbps >= 1 ? $"{mbps:F1} Mbps" : $"{mbps * 1000:F0} kbps";
        var lossPct = complete + lost == 0 ? 0 : 100.0 * lost / (complete + lost);
        var rttMs = s.RttUs / 1000.0;
        // One line for the toolbar: how it travels, how quickly, how smoothly.
        var how = s.Path == PathKind.Unknown ? L.Get("Texts.Path.Unknown") : Texts.Path(s.Path, "");
        Summary = s.RttUs == 0 ? $"{how} · {FpsText}" : $"{how} · {rttMs:F0} ms · {FpsText}";
        Level = s.RttUs == 0 ? NetLevel.Unknown
            : rttMs > 150 || lossPct > 3 ? NetLevel.Poor
            : rttMs > 60 || lossPct > 0.5 ? NetLevel.Fair
            : NetLevel.Good;
    }

    /// <summary>The toolbar's one line about the network.</summary>
    [ObservableProperty] private string _summary = L.Get("Diagnostics.Establishing");

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsGood), nameof(IsFair), nameof(IsPoor))]
    private NetLevel _level = NetLevel.Unknown;

    public bool IsGood => Level is NetLevel.Good or NetLevel.Unknown;
    public bool IsFair => Level == NetLevel.Fair;
    public bool IsPoor => Level == NetLevel.Poor;
}

public enum NetLevel { Unknown, Good, Fair, Poor }

/// <summary>A quality or resolution in the toolbar's pickers.</summary>
public sealed record StreamChoice(string Key, string Label)
{
    public override string ToString() => Label;
}

/// <summary>A display in the toolbar's picker.</summary>
public sealed record DisplayChoice(uint Id, string Label)
{
    public static DisplayChoice Of(DisplayInfo d) =>
        new(d.Id, string.IsNullOrWhiteSpace(d.Name) ? L.Format("Session.Display", d.Id + 1) : $"{d.Name} · {d.Width}×{d.Height}");

    public override string ToString() => Label;
}
