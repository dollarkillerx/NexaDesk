using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Proto;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>
/// "Someone wants to control this Mac." Every way out except the Allow button denies:
/// Deny, Enter or Escape (Deny has focus and is the default), closing the window, the
/// countdown, and the agent closing the request. Allow is disabled for the first
/// second so a click or key meant for something else cannot hit it.
/// </summary>
public sealed partial class ConsentViewModel : ObservableObject
{
    public static readonly TimeSpan ArmDelay = TimeSpan.FromSeconds(1);

    readonly IAgentLink _link;
    readonly AgentMessage.IncomingRequest _req;
    DateTimeOffset _shownAt;
    DateTimeOffset _deadline;

    public ulong ReqId => _req.ReqId;
    public string DeviceName => _req.DeviceName;
    public string DeviceId => _req.DeviceId;
    public string Fingerprint { get; }
    public string Remote => _req.Remote;
    public string AuthText => Texts.Auth(_req.Auth);
    public bool IsTakeover => _req.Takeover;
    public bool OfferSave => _req.OfferSave;
    public bool OfferSkipNextTime => _req.OfferSkipNextTime;
    public bool InputAvailable => _req.InputAvailable;
    public string Headline => $"「{DeviceName}」请求连接本机";
    public string InputNote => InputAvailable ? "可以移动鼠标、点击和输入" : "本机未授予 NexaDesk“辅助功能”权限，无法允许键盘鼠标";

    /// <summary>Raised once, when the dialog should close.</summary>
    public event Action<ConsentViewModel>? Finished;

    [ObservableProperty] private bool _allowInput;
    [ObservableProperty] private bool _allowClipboard;
    [ObservableProperty] private bool _saveDevice;
    [ObservableProperty] private bool _skipNextTime;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(AllowCommand))]
    private bool _isArmed;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CountdownText))]
    private int _secondsLeft;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(AllowCommand))]
    private bool _isFinished;

    [ObservableProperty] private string? _outcome;

    public string CountdownText => $"{SecondsLeft} 秒内不回应将自动拒绝";

    public ConsentViewModel(AgentMessage.IncomingRequest req, IAgentLink link, DateTimeOffset now)
    {
        _req = req;
        _link = link;
        Fingerprint = Texts.Fingerprint(req.DeviceKey);
        _allowInput = req.Perms.Input && req.InputAvailable;
        _allowClipboard = req.Perms.Clipboard;
        _saveDevice = req.OfferSave && req.SaveDefault;
        _skipNextTime = false;
        MarkShown(now);
    }

    /// <summary>Starts the arming delay and the countdown from when the dialog appeared.</summary>
    public void MarkShown(DateTimeOffset now)
    {
        _shownAt = now;
        var timeout = TimeSpan.FromMilliseconds(_req.TimeoutMs == 0 ? 60_000 : _req.TimeoutMs);
        // A little early, so the dialog never says yes after the agent has given up.
        _deadline = now + timeout - TimeSpan.FromSeconds(1);
        Tick(now);
    }

    public void Tick(DateTimeOffset now)
    {
        if (IsFinished) return;
        IsArmed = now - _shownAt >= ArmDelay;
        SecondsLeft = Math.Max(0, (int)Math.Ceiling((_deadline - now).TotalSeconds));
        if (now >= _deadline) Finish(UiRequest.Decide.Deny(ReqId), "已超时，自动拒绝");
    }

    bool CanAllow() => IsArmed && !IsFinished;

    [RelayCommand(CanExecute = nameof(CanAllow))]
    private void Allow()
    {
        if (!CanAllow()) return;
        var perms = new Permissions(View: true, Input: AllowInput && InputAvailable, Clipboard: AllowClipboard);
        Finish(new UiRequest.Decide(ReqId, true, perms, OfferSave && SaveDevice, OfferSkipNextTime && SkipNextTime), "已允许");
    }

    [RelayCommand]
    private void Deny() => Finish(UiRequest.Decide.Deny(ReqId), "已拒绝");

    /// <summary>The window went away by any route: that is a denial.</summary>
    public void OnWindowClosed() => Deny();

    /// <summary>The agent closed the request (timeout, the requester left, the link dropped).</summary>
    public void Expire(string reason)
    {
        if (IsFinished) return;
        IsFinished = true;
        Outcome = reason switch
        {
            "timed out" => "已超时，自动拒绝",
            "cancelled" => "对方取消了请求",
            _ => reason,
        };
        Finished?.Invoke(this);
    }

    void Finish(UiRequest.Decide decision, string outcome)
    {
        if (IsFinished) return;
        IsFinished = true;
        Outcome = outcome;
        _link.Send(decision);
        Finished?.Invoke(this);
    }
}
