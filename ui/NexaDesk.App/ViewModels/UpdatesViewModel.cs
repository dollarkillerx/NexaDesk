using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

public enum UpdateState { NotInstalled, Idle, Checking, UpToDate, Downloading, Ready, Failed }

/// <summary>
/// The version, "Check for updates", download progress and "Restart to update". A quiet check
/// runs after startup and then once a day; it says nothing unless an update is ready. An update
/// never installs by itself: restarting ends remote sessions, so the person chooses when.
/// </summary>
public sealed partial class UpdatesViewModel : ObservableObject
{
    readonly IAppUpdater _updater;
    readonly Action _quit;

    /// <param name="quit">Quits the app the normal way (the update installs after the process exits).</param>
    public UpdatesViewModel(IAppUpdater updater, Action quit)
    {
        _updater = updater;
        _quit = quit;
        if (!updater.IsInstalled) _state = UpdateState.NotInstalled;
        else if (updater.PendingVersion is { } pending)
        {
            _newVersion = pending;
            _state = UpdateState.Ready;
        }
        else _state = UpdateState.Idle;
    }

    public string CurrentVersion => L.Format("Updates.Current", _updater.CurrentVersion);

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(StatusText), nameof(IsReady), nameof(IsBusy), nameof(CanCheckNow))]
    [NotifyCanExecuteChangedFor(nameof(CheckCommand))]
    private UpdateState _state;

    [ObservableProperty] [NotifyPropertyChangedFor(nameof(StatusText))] private string? _newVersion;
    [ObservableProperty] [NotifyPropertyChangedFor(nameof(StatusText))] private int _progress;
    [ObservableProperty] [NotifyPropertyChangedFor(nameof(StatusText))] private string? _error;

    public bool IsReady => State == UpdateState.Ready;
    public bool IsBusy => State is UpdateState.Checking or UpdateState.Downloading;
    public bool CanCheckNow => CanCheck();

    public string StatusText => State switch
    {
        UpdateState.NotInstalled => L.Get("Updates.NotInstalled"),
        UpdateState.Checking => L.Get("Updates.Checking"),
        UpdateState.UpToDate => L.Get("Updates.UpToDate"),
        UpdateState.Downloading => L.Format("Updates.Downloading", NewVersion ?? "", Progress),
        UpdateState.Ready => L.Format("Updates.Ready", NewVersion ?? ""),
        UpdateState.Failed => L.Format("Updates.Failed", Error ?? ""),
        _ => "",
    };

    bool CanCheck() => State is UpdateState.Idle or UpdateState.UpToDate or UpdateState.Failed;

    [RelayCommand(CanExecute = nameof(CanCheck))]
    private Task Check(CancellationToken ct) => RunAsync(background: false, ct);

    /// <summary>The quiet check: failures (offline, rate-limited) are not shown.</summary>
    public Task CheckInBackgroundAsync(CancellationToken ct = default) =>
        State is UpdateState.Idle or UpdateState.UpToDate ? RunAsync(background: true, ct) : Task.CompletedTask;

    /// <summary>After startup, then once a day: a host can run for weeks without a restart.</summary>
    public async Task CheckDailyAsync(CancellationToken ct = default)
    {
        if (State == UpdateState.NotInstalled) return;
        try
        {
            await Task.Delay(TimeSpan.FromSeconds(10), ct);
            while (!ct.IsCancellationRequested && State != UpdateState.Ready)
            {
                await CheckInBackgroundAsync(ct);
                await Task.Delay(TimeSpan.FromDays(1), ct);
            }
        }
        catch (OperationCanceledException)
        {
        }
    }

    async Task RunAsync(bool background, CancellationToken ct)
    {
        State = UpdateState.Checking;
        Error = null;
        try
        {
            var version = await _updater.CheckAsync(ct);
            if (version is null)
            {
                State = background ? UpdateState.Idle : UpdateState.UpToDate;
                return;
            }
            NewVersion = version;
            Progress = 0;
            State = UpdateState.Downloading;
            var ui = SynchronizationContext.Current;
            await _updater.DownloadAsync(p =>
            {
                if (ui is null) Progress = p;
                else ui.Post(_ => Progress = p, null);
            }, ct);
            State = UpdateState.Ready;
        }
        catch (OperationCanceledException)
        {
            State = UpdateState.Idle;
        }
        catch (Exception e) when (e is HttpRequestException or IOException or InvalidOperationException or UnauthorizedAccessException
                                      or System.Text.Json.JsonException or TimeoutException or NotSupportedException or ArgumentException)
        {
            Error = e.Message;
            State = background ? UpdateState.Idle : UpdateState.Failed;
        }
    }

    [RelayCommand]
    private void Restart()
    {
        if (State != UpdateState.Ready) return;
        _updater.ApplyAfterExit();
        _quit();
    }
}
