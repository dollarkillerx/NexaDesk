using Avalonia.Headless.XUnit;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// Browser access (crates/nexa-web): off by default, a switch that starts the agent with
/// --web, the browser addresses among the ways in, and a consent dialog that offers a
/// browser only what it can have.
/// </summary>
public class BrowserAccessTests
{
    static MainViewModel Main(FakeLink link, FakeProcess process, bool allow) =>
        new(new FakeClient(), new MemoryRecent(), link, process, new FakePermissions(), new RecordingWindows(),
            new FakeAddresses(["192.168.1.35", "100.101.22.7", "127.0.0.1"]), "Mac", () => Samples.T0,
            new MemorySettings(new AppSettings(AllowBrowsers: allow)));

    static async Task<(MainViewModel, FakeProcess)> Hosting(bool allow)
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var main = Main(link, process, allow);
        await main.Host.StartCommand.ExecuteAsync(null);
        link.Raise(Samples.Status() with { Listen = "[::]:47800" });
        return (main, process);
    }

    [Fact]
    public void Browser_access_is_off_until_switched_on_and_is_saved()
    {
        var store = new MemorySettings();
        var settings = new SettingsViewModel(store);
        Assert.False(settings.AllowBrowsers);
        settings.AllowBrowsers = true;
        Assert.True(store.Current.AllowBrowsers);
        Assert.True(new SettingsViewModel(store).AllowBrowsers);
    }

    [Fact]
    public void The_web_address_is_the_listen_host_on_port_47801()
    {
        Assert.Equal("[::]:47801", HostViewModel.WebListen("[::]:47800"));
        Assert.Equal("192.168.1.35:47801", HostViewModel.WebListen("192.168.1.35:47800"));
        Assert.Equal("[fd7a::1]:47801", HostViewModel.WebListen("[fd7a::1]:47800"));
    }

    [AvaloniaFact]
    public async Task Switched_on_the_agent_serves_browsers_and_the_addresses_are_listed()
    {
        var (main, process) = await Hosting(allow: true);
        Assert.Equal("[::]:47801", process.Webs.Single());
        var browser = main.Host.ConnectOptions.Where(o => o.Address.StartsWith("http://")).Select(o => o.Address).ToList();
        Assert.Contains("http://192.168.1.35:47801", browser);
        Assert.Contains("http://100.101.22.7:47801", browser);
        Assert.DoesNotContain("http://127.0.0.1:47801", browser);
    }

    [AvaloniaFact]
    public async Task Switched_off_nothing_is_served_and_changing_it_asks_for_a_restart()
    {
        var (main, process) = await Hosting(allow: false);
        Assert.Null(process.Webs.Single());
        Assert.DoesNotContain(main.Host.ConnectOptions, o => o.Address.StartsWith("http://"));
        Assert.False(main.Host.RestartNeeded);
        main.Settings.AllowBrowsers = true;
        Assert.True(main.Host.RestartNeeded);
    }

    [Fact]
    public void The_browser_flag_is_read_from_the_request()
    {
        // IncomingRequest{req_id 7, browser(14) true}
        var withFlag = AgentMessage.Decode(Convert.FromHexString("120408077001"));
        var req = Assert.IsType<AgentMessage.IncomingRequest>(withFlag);
        Assert.Equal(7ul, req.ReqId);
        Assert.True(req.Browser);
        var without = Assert.IsType<AgentMessage.IncomingRequest>(AgentMessage.Decode(Convert.FromHexString("12020807")));
        Assert.False(without.Browser);
    }

    [Fact]
    public void A_browser_is_offered_view_and_input_only_and_is_never_saved()
    {
        var req = Samples.Request() with
        {
            Browser = true,
            OfferSave = false,
            Perms = new Permissions(true, true, false),
        };
        var link = new FakeLink();
        var vm = new ConsentViewModel(req, link, Samples.T0) { AllowClipboard = true, AllowFile = true, AllowAudio = true };
        Assert.True(vm.IsBrowser);
        Assert.False(vm.IsDevice);
        vm.Tick(Samples.T0.AddSeconds(2));
        vm.AllowCommand.Execute(null);
        var decided = Assert.IsType<UiRequest.Decide>(link.Sent[^1]);
        Assert.True(decided.Perms.View);
        Assert.False(decided.Perms.Clipboard);
        Assert.False(decided.Perms.File);
        Assert.False(decided.Perms.Audio);
        Assert.False(decided.Save);
    }
}
