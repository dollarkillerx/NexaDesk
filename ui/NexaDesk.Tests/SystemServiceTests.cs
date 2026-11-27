using NexaDesk.Localization;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>The NexaDesk service on Windows: UAC prompts and the lock screen can be used too.</summary>
public class SystemServiceTests
{
    sealed class FakeService(SystemServiceState state = SystemServiceState.NotInstalled, string? refusal = null)
        : ISystemService
    {
        public SystemServiceState State = state;
        public int Installs, Uninstalls;
        public bool IsSupported => true;
        public SystemServiceState Check() => State;

        public Task<string?> Install()
        {
            Installs++;
            if (refusal == null) State = SystemServiceState.Running;
            return Task.FromResult(refusal);
        }

        public Task<string?> Uninstall()
        {
            Uninstalls++;
            if (refusal == null) State = SystemServiceState.NotInstalled;
            return Task.FromResult(refusal);
        }

        public Stream? Connect(int timeoutMs) => null;
    }

    sealed class Elevation(bool elevated) : IElevation
    {
        public bool IsSupported => true;
        public bool IsElevated => elevated;
        public string? StartElevatedCopy() => null;
    }

    sealed class Elevated() : IElevation
    {
        public bool IsSupported => true;
        public bool IsElevated => true;
        public string? StartElevatedCopy() => null;
    }

    static async Task Settled(SettingsViewModel vm)
    {
        for (var i = 0; i < 200 && vm.IsChangingSystemService; i++) await Task.Delay(10);
        await Task.Delay(20);
        Assert.False(vm.IsChangingSystemService);
    }

    [Fact]
    public void Only_windows_with_the_service_beside_the_app_offers_it()
    {
        Assert.False(new SettingsViewModel(new MemorySettings()).ShowSystemService);
        Assert.True(new SettingsViewModel(new MemorySettings(), systemService: new FakeService()).ShowSystemService);
    }

    [Fact]
    public async Task The_box_shows_what_is_installed()
    {
        var vm = new SettingsViewModel(new MemorySettings(), systemService: new FakeService(SystemServiceState.Running));
        var changed = 0;
        vm.SystemServiceChanged += () => changed++;
        await vm.RefreshSystemService();
        Assert.True(vm.UseSystemService);
        Assert.Equal(L.Get("Main.SystemServiceRunning"), vm.SystemServiceStatus);
        Assert.False(vm.SystemServiceNeedsUpdate);
        // Showing what is there installs nothing and restarts nothing.
        Assert.Equal(0, changed);
    }

    [Fact]
    public async Task Ticking_it_installs_the_service_and_restarts_the_host()
    {
        var service = new FakeService();
        var vm = new SettingsViewModel(new MemorySettings(), systemService: service);
        await vm.RefreshSystemService();
        Assert.False(vm.UseSystemService);
        var changed = 0;
        vm.SystemServiceChanged += () => changed++;
        vm.UseSystemService = true;
        await Settled(vm);
        Assert.Equal(1, service.Installs);
        Assert.True(vm.UseSystemService);
        Assert.Equal(SystemServiceState.Running, vm.SystemServiceState);
        Assert.Null(vm.SystemServiceError);
        Assert.Equal(1, changed);
    }

    [Fact]
    public async Task A_no_on_the_uac_prompt_puts_the_box_back_and_says_so()
    {
        var service = new FakeService(refusal: "declined");
        var vm = new SettingsViewModel(new MemorySettings(), systemService: service);
        var changed = 0;
        vm.SystemServiceChanged += () => changed++;
        vm.UseSystemService = true;
        await Settled(vm);
        Assert.False(vm.UseSystemService);
        Assert.Equal("declined", vm.SystemServiceError);
        Assert.Equal(0, changed);
    }

    [Fact]
    public async Task Unticking_it_removes_the_service()
    {
        var service = new FakeService(SystemServiceState.Running);
        var vm = new SettingsViewModel(new MemorySettings(), systemService: service);
        await vm.RefreshSystemService();
        vm.UseSystemService = false;
        await Settled(vm);
        Assert.Equal(1, service.Uninstalls);
        Assert.False(vm.UseSystemService);
        Assert.Null(vm.SystemServiceStatus);
    }

    [Fact]
    public async Task An_outdated_service_offers_an_update()
    {
        var service = new FakeService(SystemServiceState.NeedsUpdate);
        var vm = new SettingsViewModel(new MemorySettings(), systemService: service);
        await vm.RefreshSystemService();
        Assert.True(vm.UseSystemService);
        Assert.True(vm.SystemServiceNeedsUpdate);
        Assert.Equal(L.Get("Main.SystemServiceNeedsUpdate"), vm.SystemServiceStatus);
        Assert.Equal(0, service.Installs);
        await vm.UpdateSystemServiceCommand.ExecuteAsync(null);
        Assert.Equal(1, service.Installs);
        Assert.False(vm.SystemServiceNeedsUpdate);
    }

    [Fact]
    public async Task A_copy_running_as_administrator_updates_it_without_asking()
    {
        var service = new FakeService(SystemServiceState.NeedsUpdate);
        var vm = new SettingsViewModel(new MemorySettings(), elevation: new Elevated(), systemService: service);
        await vm.RefreshSystemService();
        Assert.Equal(1, service.Installs);
        Assert.Equal(SystemServiceState.Running, vm.SystemServiceState);
    }

    [Fact]
    public void With_the_service_there_run_as_administrator_is_not_offered_any_more()
    {
        var vm = new SettingsViewModel(new MemorySettings(), elevation: new Elevation(false), systemService: new FakeService());
        Assert.True(vm.ShowSystemService);
        Assert.False(vm.ShowRunAsAdministrator);
        // Without the service (a development build) it still is.
        Assert.True(new SettingsViewModel(new MemorySettings(), elevation: new Elevation(false)).ShowRunAsAdministrator);
    }

    [Fact]
    public async Task Run_as_administrator_from_before_moves_over_to_the_service_once_elevated()
    {
        var store = new MemorySettings();
        store.Save(store.Load() with { RunAsAdministrator = true });
        // Started at login: not elevated, so nothing can be installed without asking.
        var atLogin = new SettingsViewModel(store, elevation: new Elevation(false), systemService: new FakeService());
        await atLogin.RefreshSystemService();
        Assert.True(atLogin.RunAsAdministrator);
        Assert.True(atLogin.ShowRunAsAdministrator);

        // Opened by hand, elevated: the service goes in without a prompt and the option goes off.
        var service = new FakeService();
        var opened = new SettingsViewModel(store, elevation: new Elevation(true), systemService: service);
        await opened.RefreshSystemService();
        Assert.Equal(1, service.Installs);
        Assert.Equal(SystemServiceState.Running, opened.SystemServiceState);
        Assert.False(opened.RunAsAdministrator);
        Assert.False(store.Current.RunAsAdministrator);
        Assert.False(opened.ShowRunAsAdministrator);
    }

    [Fact]
    public async Task A_service_that_will_not_install_leaves_run_as_administrator_on()
    {
        var store = new MemorySettings();
        store.Save(store.Load() with { RunAsAdministrator = true });
        var vm = new SettingsViewModel(store, elevation: new Elevation(true), systemService: new FakeService(refusal: "failed"));
        await vm.RefreshSystemService();
        Assert.True(vm.RunAsAdministrator);
        Assert.True(store.Current.RunAsAdministrator);
    }

    [Fact]
    public void The_start_request_is_what_the_service_reads()
    {
        // crates/nexa-service/src/proto.rs: requests_and_events_on_the_wire
        Assert.Equal("{\"start\":{\"args\":[\"--name\",\"x\"]}}\n", ServiceProtocol.Start(["--name", "x"], null));
        Assert.Equal("{\"start\":{\"args\":[],\"signal_secret\":\"k\"}}\n", ServiceProtocol.Start([], "k"));
        Assert.Equal("\"hello\"\n", ServiceProtocol.Hello());
    }

    [Fact]
    public void The_services_events_are_understood()
    {
        Assert.Equal(new ServiceEvent("started", Number: 42), ServiceProtocol.Parse("{\"started\":{\"pid\":42}}"));
        Assert.Equal(new ServiceEvent("line", "nexa-agent: hi"), ServiceProtocol.Parse("{\"line\":\"nexa-agent: hi\"}"));
        Assert.Equal(new ServiceEvent("exited", Number: -1), ServiceProtocol.Parse("{\"exited\":{\"code\":-1}}"));
        Assert.Equal(new ServiceEvent("hello", Number: 1), ServiceProtocol.Parse("{\"hello\":{\"protocol\":1}}"));
        Assert.Equal(new ServiceEvent("error", "no"), ServiceProtocol.Parse("{\"error\":\"no\"}"));
        Assert.Equal(new ServiceEvent("done"), ServiceProtocol.Parse("\"done\""));
        Assert.Equal("unknown", ServiceProtocol.Parse("not json").Kind);
        Assert.Equal("unknown", ServiceProtocol.Parse("{\"later\":{}}").Kind);
    }
}
