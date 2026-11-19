using Avalonia.Headless.XUnit;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// Remote control is on at start by default, and stays on across a quit or an update restart,
/// so whoever restarted NexaDesk from afar is not locked out. Only the person stopping it
/// themselves keeps it off at later starts.
/// </summary>
public class ResumeHostingTests
{
    static MainViewModel App(ISettingsStore settings, FakeLink link, FakeProcess process, FakePermissions? permissions = null,
        RecordingWindows? windows = null) =>
        new(new FakeClient(), new MemoryRecent(), link, process, permissions ?? new FakePermissions(),
            windows ?? new RecordingWindows(), new FakeAddresses(["192.168.1.35"]), "Mac", () => Samples.T0, settings);

    /// <summary>A first run that turns hosting on; the agent answers on the second try.</summary>
    static async Task<(MainViewModel, FakeLink, FakeProcess)> Hosting(ISettingsStore settings)
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var main = App(settings, link, process);
        await main.Host.StartCommand.ExecuteAsync(null);
        Assert.True(main.Host.IsHosting);
        return (main, link, process);
    }

    /// <summary>The next start: no agent left running, so a new one is started.</summary>
    static async Task<(MainViewModel, FakeProcess)> NextStart(ISettingsStore settings, FakePermissions? permissions = null,
        RecordingWindows? windows = null)
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket"); // the attach
        link.ConnectResults.Enqueue("no socket"); // Start's own look for one
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var main = App(settings, link, process, permissions, windows);
        // As App.OnFrameworkInitializationCompleted does.
        if (main.ResumesHosting) await main.Host.AttachOrStartAsync();
        return (main, process);
    }

    [AvaloniaFact]
    public async Task Hosting_comes_back_after_a_restart_that_happened_while_hosting()
    {
        var settings = new MemorySettings();
        var (first, _, _) = await Hosting(settings);
        Assert.True(settings.Current.ResumeHosting);

        // Quit (or "Restart to update", which quits the same way) while hosting.
        first.OnExit();
        Assert.True(settings.Current.ResumeHosting);

        var (next, process) = await NextStart(settings);
        Assert.True(next.ResumesHosting);
        Assert.Single(process.Starts);
        Assert.True(next.Host.IsHosting);
    }

    [AvaloniaFact]
    public async Task An_agent_still_running_is_picked_up_rather_than_started_again()
    {
        var settings = new MemorySettings(new AppSettings(ResumeHosting: true));
        var link = new FakeLink();
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var main = App(settings, link, process);
        await main.Host.AttachOrStartAsync();
        Assert.True(main.Host.IsHosting);
        Assert.Empty(process.Starts);
    }

    [AvaloniaFact]
    public async Task Hosting_stays_off_after_the_person_stopped_it()
    {
        var settings = new MemorySettings();
        var (first, _, _) = await Hosting(settings);
        first.Host.StopCommand.Execute(null);
        Assert.False(settings.Current.ResumeHosting);
        first.OnExit();

        var (next, process) = await NextStart(settings);
        Assert.False(next.ResumesHosting);
        Assert.Empty(process.Starts);
        Assert.True(next.Host.IsStopped);
    }

    [AvaloniaFact]
    public async Task Stopping_from_the_menu_bar_counts_as_the_person_stopping_it()
    {
        var settings = new MemorySettings();
        var (first, _, _) = await Hosting(settings);
        await first.Tray.ToggleHostingCommand.ExecuteAsync(null);
        Assert.True(first.Host.IsStopped);
        Assert.False(settings.Current.ResumeHosting);
    }

    [AvaloniaFact]
    public async Task A_fresh_install_starts_hosting()
    {
        var settings = new MemorySettings();
        var (next, process) = await NextStart(settings);
        Assert.True(next.ResumesHosting);
        Assert.Single(process.Starts);
        Assert.True(next.Host.IsHosting);
    }

    [Fact]
    public void No_settings_json_or_one_from_before_the_choice_means_on()
    {
        var dir = Directory.CreateTempSubdirectory("nexa-resume-");
        try
        {
            var path = Path.Combine(dir.FullName, "settings.json");
            Assert.True(new SettingsStore(path).Load().ResumeHosting);
            // As 0.1.3 wrote it.
            File.WriteAllText(path, """{ "signalServer": "", "announceOnLan": true, "language": "", "allowBrowsers": false }""");
            Assert.True(new SettingsStore(path).Load().ResumeHosting);
        }
        finally
        {
            dir.Delete(recursive: true);
        }
    }

    [AvaloniaFact]
    public async Task Without_screen_recording_nothing_starts_and_the_usual_notice_shows()
    {
        var settings = new MemorySettings(new AppSettings(ResumeHosting: true));
        var windows = new RecordingWindows();
        var (next, process) = await NextStart(settings, new FakePermissions { State = new(false, true) }, windows);
        Assert.Empty(process.Starts);
        Assert.True(next.Host.IsStopped);
        Assert.NotNull(next.Host.Error);
        Assert.Single(windows.Guides);
        // Still wanted: once the permission is on, the next start brings it back.
        Assert.True(settings.Current.ResumeHosting);
    }

    [AvaloniaFact]
    public async Task The_agent_ending_or_hosting_restarting_for_a_new_password_keeps_the_choice()
    {
        var settings = new MemorySettings();
        var (first, link, _) = await Hosting(settings);
        link.Raise(Samples.Status());
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        first.Host.EditPasswordCommand.Execute(null);
        first.Host.NewPassword = first.Host.ConfirmPassword = "correct horse";
        await first.Host.SavePasswordCommand.ExecuteAsync(null);
        Assert.True(first.Host.IsHosting);
        Assert.True(settings.Current.ResumeHosting);

        link.Drop();
        Assert.True(first.Host.IsStopped);
        Assert.True(settings.Current.ResumeHosting);
    }

    [AvaloniaFact]
    public async Task Changing_a_setting_keeps_the_choice()
    {
        var settings = new MemorySettings();
        var (first, _, _) = await Hosting(settings);
        first.Settings.AnnounceOnLan = !first.Settings.AnnounceOnLan;
        Assert.True(settings.Current.ResumeHosting);
    }

    [Fact]
    public void The_choice_is_kept_in_settings_json()
    {
        var dir = Directory.CreateTempSubdirectory("nexa-resume-");
        try
        {
            var store = new SettingsStore(Path.Combine(dir.FullName, "settings.json"));
            store.Save(new AppSettings(ResumeHosting: false));
            Assert.False(store.Load().ResumeHosting);
            store.Save(store.Load() with { ResumeHosting = true });
            Assert.True(store.Load().ResumeHosting);
        }
        finally
        {
            dir.Delete(recursive: true);
        }
    }

    /// <summary>
    /// An update restart: the new app reaches the old app's agent while it is still shutting down, and
    /// loses it a moment later. Remote control must come back by itself: whoever restarted from afar
    /// cannot press Start.
    /// </summary>
    [AvaloniaFact]
    public async Task An_agent_lost_right_after_an_update_restart_is_started_again()
    {
        var settings = new MemorySettings(new AppSettings(ResumeHosting: true));
        var link = new FakeLink();
        link.ConnectResults.Enqueue(null); // the old agent, still answering
        var process = new FakeProcess();
        var main = App(settings, link, process);
        main.Host.RestartAfter = TimeSpan.FromMilliseconds(20);
        await main.Host.AttachOrStartAsync();
        Assert.True(main.Host.IsHosting);
        Assert.Empty(process.Starts);

        // It finishes shutting down.
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        link.Drop();
        await Until(() => main.Host.IsHosting);
        Assert.Single(process.Starts);
        Assert.Null(main.Host.Error);
    }

    [AvaloniaFact]
    public async Task An_agent_that_keeps_ending_is_restarted_a_few_times_then_left_stopped()
    {
        var settings = new MemorySettings(new AppSettings(ResumeHosting: true));
        var link = new FakeLink { DefaultConnectResult = "no socket" };
        var process = new FakeProcess();
        var main = App(settings, link, process);
        main.Host.RestartAfter = TimeSpan.FromMilliseconds(10);
        link.ConnectResults.Enqueue(null);
        await main.Host.AttachOrStartAsync();
        Assert.True(main.Host.IsHosting);
        link.Drop();
        // Each restart's agent ends at once, as one whose port is not free would.
        for (var i = 1; i <= 3; i++)
        {
            await Until(() => process.Starts.Count == i);
            process.Exit(1, "nexa-agent: cannot listen on [::]:47800");
        }
        await Task.Delay(300);
        Assert.Equal(3, process.Starts.Count);
        Assert.False(main.Host.IsHosting);
        Assert.NotNull(main.Host.Error);
    }

    [AvaloniaFact]
    public async Task An_agent_the_person_stopped_is_not_started_again()
    {
        var settings = new MemorySettings();
        var (main, link, process) = await Hosting(settings);
        main.Host.RestartAfter = TimeSpan.FromMilliseconds(10);
        main.Host.StopCommand.Execute(null);
        link.Drop();
        await Task.Delay(200);
        Assert.Single(process.Starts);
        Assert.False(main.Host.IsHosting);
    }

    static async Task Until(Func<bool> done)
    {
        var end = DateTime.UtcNow + TimeSpan.FromSeconds(5);
        while (!done())
        {
            Assert.True(DateTime.UtcNow < end, "timed out");
            await Task.Delay(10);
        }
    }
}
