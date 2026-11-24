using NexaDesk.Localization;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>"Run as administrator" on Windows: the host can then use programs that run elevated.</summary>
public class RunAsAdministratorTests
{
    sealed class FakeElevation(bool supported = true, bool elevated = false, string? refusal = null) : IElevation
    {
        public int Started;
        public bool IsSupported => supported;
        public bool IsElevated => elevated;
        public string? StartElevatedCopy()
        {
            Started++;
            return refusal;
        }
    }

    [Fact]
    public void Only_windows_offers_it_and_it_starts_off()
    {
        var mac = new SettingsViewModel(new MemorySettings(), elevation: new NoElevation());
        Assert.False(mac.ShowRunAsAdministrator);
        Assert.Null(mac.RunAsAdministratorStatus);
        var windows = new SettingsViewModel(new MemorySettings(), elevation: new FakeElevation());
        Assert.True(windows.ShowRunAsAdministrator);
        Assert.False(windows.RunAsAdministrator);
        Assert.Null(windows.RunAsAdministratorStatus);
    }

    [Fact]
    public void Switching_it_on_saves_it_starts_an_elevated_copy_and_quits_for_it()
    {
        var store = new MemorySettings();
        var elevation = new FakeElevation();
        var vm = new SettingsViewModel(store, elevation: elevation);
        var quit = 0;
        vm.QuitForElevatedCopy += () => quit++;
        vm.RunAsAdministrator = true;
        Assert.True(store.Current.RunAsAdministrator);
        Assert.Equal(1, elevation.Started);
        Assert.Equal(1, quit);
        Assert.Null(vm.RunAsAdministratorError);
    }

    [Fact]
    public void A_no_on_the_uac_prompt_leaves_it_off_and_says_so()
    {
        var store = new MemorySettings();
        var vm = new SettingsViewModel(store, elevation: new FakeElevation(refusal: "declined"));
        var quit = 0;
        vm.QuitForElevatedCopy += () => quit++;
        vm.RunAsAdministrator = true;
        Assert.False(vm.RunAsAdministrator);
        Assert.False(store.Current.RunAsAdministrator);
        Assert.Equal("declined", vm.RunAsAdministratorError);
        Assert.Equal(0, quit);
    }

    [Fact]
    public void Already_elevated_nothing_restarts_and_the_status_says_where_it_stands()
    {
        var store = new MemorySettings(new AppSettings(RunAsAdministrator: true));
        var elevation = new FakeElevation(elevated: true);
        var vm = new SettingsViewModel(store, elevation: elevation);
        Assert.Equal(L.Get("Main.RunAsAdminActive"), vm.RunAsAdministratorStatus);
        vm.RunAsAdministrator = false;
        Assert.Equal(L.Get("Main.RunAsAdminOffLater"), vm.RunAsAdministratorStatus);
        vm.RunAsAdministrator = true;
        Assert.Equal(0, elevation.Started);

        // On, but this copy started at login without asking.
        var notYet = new SettingsViewModel(new MemorySettings(new AppSettings(RunAsAdministrator: true)),
            elevation: new FakeElevation());
        Assert.Equal(L.Get("Main.RunAsAdminNotYet"), notYet.RunAsAdministratorStatus);
    }

    [Fact]
    public void The_main_window_quits_for_the_elevated_copy()
    {
        var main = new MainViewModel(new FakeClient(), new MemoryRecent(), new FakeLink(), new FakeProcess(), new FakePermissions(),
            new RecordingWindows(), new FakeAddresses(), "PC", () => Samples.T0, new MemorySettings(),
            elevation: new FakeElevation());
        var quit = 0;
        main.Tray.QuitRequested += () => quit++;
        main.Settings.RunAsAdministrator = true;
        Assert.Equal(1, quit);
    }
}
