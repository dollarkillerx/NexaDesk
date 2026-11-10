using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Native;
using NexaDesk.Proto;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>The windows the view-models ask for. Tests record the calls instead.</summary>
public interface IWindowService
{
    void ShowSession(SessionViewModel session);
    void ShowConsent(ConsentViewModel consent);
    void ShowBanner(BannerViewModel banner);
    void HideBanner();
    void ShowPermissionGuide(PermissionGuideViewModel guide);
    void ShowDiagnostics(DiagnosticsViewModel diagnostics);
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
    public string Title => $"{HostName} — NexaDesk";
    public string PermsText => Texts.Permissions(Perms);
    public string HostKeyHint => "按 ⌃⌥⌘G 释放键盘并显示工具条";
    public bool CanControl => Perms.Input;
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
    [ObservableProperty] private bool _isToolbarVisible = true;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CaptureText))]
    private bool _isSuspended;

    [ObservableProperty] private string _streamText = "等待画面…";
    [ObservableProperty] private string? _warning;

    public SessionViewModel(ISession session, ConnectedEvent connected, string address, Func<DateTimeOffset>? now = null)
    {
        Session = session;
        Address = address;
        HostName = string.IsNullOrWhiteSpace(connected.HostName) ? address : connected.HostName;
        HostKey = connected.HostKey;
        Perms = connected.Perms;
        _now = now ?? (() => DateTimeOffset.Now);
        Diagnostics = new DiagnosticsViewModel(address);
        _clipboardText = Perms.Clipboard ? "剪贴板：双向同步" : "剪贴板：对方未允许";
    }

    public bool IsOffice { get => Mode == StreamMode.Office; set { if (value) Mode = StreamMode.Office; } }
    public bool IsBalanced { get => Mode == StreamMode.Balanced; set { if (value) Mode = StreamMode.Balanced; } }
    public bool IsGame { get => Mode == StreamMode.Game; set { if (value) Mode = StreamMode.Game; } }
    public string ModeText => Texts.Mode(Mode);

    public string CaptureText => IsSuspended && Perms.Input
        ? "已释放键盘：点击画面或“继续控制”恢复"
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

    [RelayCommand]
    private void ToggleToolbar() => IsToolbarVisible = !IsToolbarVisible;

    /// <summary>Whether input is captured right now, as last asked of the library.</summary>
    public bool InputActive => _inputSent == true;

    void UpdateCapture()
    {
        var want = Perms.Input && _windowActive && !IsSuspended && !IsEnded;
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
                StreamText = $"{Texts.Codec(s.Codec)} · {s.Width}×{s.Height} · {s.Fps} fps";
                Diagnostics.OnStream(s);
                break;
            case InputCaptureEvent c:
                Capture = c.State;
                break;
            case PasteGateEvent g:
                ClipboardText = g.TimedOut
                    ? $"剪贴板：粘贴等待 {g.WaitedMs} ms 后超时，可能粘贴了旧内容"
                    : $"剪贴板：粘贴前等待了 {g.WaitedMs} ms";
                break;
            case ClipboardNoticeEvent n:
                ClipboardText = $"剪贴板：{n.Text}";
                break;
            case StatsEvent s:
                Diagnostics.Update(s.Stats, _now());
                break;
            case HostKeyEvent:
                OnHostKey();
                break;
            case WarningEvent w:
                Warning = w.Text;
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
        ? "正在建立…"
        : Texts.PathOf(address);

    [ObservableProperty] private string _rttText = "—";
    [ObservableProperty] private string _lossText = "—";
    [ObservableProperty] private string _fpsText = "—";
    [ObservableProperty] private string _bitrateText = "—";
    [ObservableProperty] private string _streamText = "—";
    [ObservableProperty] private string _recoveryText = "—";
    [ObservableProperty] private string _pasteText = "—";

    public void OnStream(StreamChangedEvent s) =>
        StreamText = $"{Texts.Codec(s.Codec)} · {s.Width}×{s.Height} · 目标 {s.Fps} fps · {Texts.Mode(s.Mode)}模式";

    public void Update(StreamStats s, DateTimeOffset now)
    {
        RttText = s.RttUs == 0 ? "—" : $"{s.RttUs / 1000.0:F1} ms";
        if (s.Path != PathKind.Unknown)
        {
            var path = Texts.Path(s.Path, s.PathDetail);
            PathText = s.PathRttUs == 0 ? path : $"{path} · 探测 {s.PathRttUs / 1000.0:F0} ms";
        }
        RecoveryText = $"FEC 恢复 {s.FecRecovered} 帧 · 解码错误 {s.DecodeErrors}";
        PasteText = $"{s.GatePastes} 次粘贴 · {s.GateHeld} 次等待剪贴板 · {s.GateTimeouts} 次超时";
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
    }
}
