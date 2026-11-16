using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>Updates from GitHub releases: found, downloaded, and installed only when asked.</summary>
public class UpdatesTests
{
    sealed class FakeUpdater(bool installed, string? available, string? pending = null, Exception? fail = null) : IAppUpdater
    {
        public bool IsInstalled => installed;
        public string CurrentVersion => "0.1.0";
        public string? PendingVersion => pending;
        public int Checks;
        public bool Downloaded;
        public bool Applied;

        public Task<string?> CheckAsync(CancellationToken ct)
        {
            Checks++;
            return fail != null ? Task.FromException<string?>(fail) : Task.FromResult(available);
        }

        public Task DownloadAsync(Action<int> progress, CancellationToken ct)
        {
            progress(100);
            Downloaded = true;
            return Task.CompletedTask;
        }

        public void ApplyAfterExit() => Applied = true;
    }

    [Fact]
    public void A_build_that_was_not_packaged_never_checks()
    {
        var updater = new FakeUpdater(installed: false, available: "0.2.0");
        var vm = new UpdatesViewModel(updater, () => { });
        Assert.Equal(UpdateState.NotInstalled, vm.State);
        Assert.False(vm.CheckCommand.CanExecute(null));
        vm.CheckInBackgroundAsync().Wait();
        Assert.Equal(0, updater.Checks);
    }

    [Fact]
    public async Task A_newer_release_is_downloaded_and_waits_for_a_restart()
    {
        var updater = new FakeUpdater(installed: true, available: "0.2.0");
        var quit = 0;
        var vm = new UpdatesViewModel(updater, () => quit++);
        await vm.CheckCommand.ExecuteAsync(null);
        Assert.Equal(UpdateState.Ready, vm.State);
        Assert.True(updater.Downloaded);
        Assert.False(updater.Applied);
        Assert.Equal(0, quit);
        vm.RestartCommand.Execute(null);
        Assert.True(updater.Applied);
        Assert.Equal(1, quit);
    }

    [Fact]
    public async Task Up_to_date_is_said_only_when_asked()
    {
        var updater = new FakeUpdater(installed: true, available: null);
        var vm = new UpdatesViewModel(updater, () => { });
        await vm.CheckInBackgroundAsync();
        Assert.Equal(UpdateState.Idle, vm.State);
        await vm.CheckCommand.ExecuteAsync(null);
        Assert.Equal(UpdateState.UpToDate, vm.State);
    }

    [Fact]
    public async Task A_failed_quiet_check_shows_nothing_but_a_check_by_hand_does()
    {
        var updater = new FakeUpdater(installed: true, available: null, fail: new HttpRequestException("offline"));
        var vm = new UpdatesViewModel(updater, () => { });
        await vm.CheckInBackgroundAsync();
        Assert.Equal(UpdateState.Idle, vm.State);
        await vm.CheckCommand.ExecuteAsync(null);
        Assert.Equal(UpdateState.Failed, vm.State);
        Assert.Equal("offline", vm.Error);
    }

    [Fact]
    public void An_update_downloaded_earlier_is_ready_at_once()
    {
        var vm = new UpdatesViewModel(new FakeUpdater(installed: true, available: null, pending: "0.2.0"), () => { });
        Assert.True(vm.IsReady);
        Assert.Equal("0.2.0", vm.NewVersion);
    }
}
