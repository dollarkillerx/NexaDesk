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

    const string Fp = "AB:CD:EF:01:23:45:67:89:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:10:20:30:40:50:60:70:80";

    static async Task<(MainViewModel, FakeProcess)> Hosting(bool allow, string fingerprint = Fp, bool custom = false)
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var main = Main(link, process, allow);
        await main.Host.StartCommand.ExecuteAsync(null);
        link.Raise(Samples.Status() with { Listen = "[::]:47800", WebFingerprint = allow ? fingerprint : "", WebCertCustom = custom });
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
        var browser = main.Host.ConnectOptions.Where(o => o.Address.StartsWith("https://")).Select(o => o.Address).ToList();
        Assert.Contains("https://192.168.1.35:47801", browser);
        Assert.Contains("https://100.101.22.7:47801", browser);
        Assert.DoesNotContain("https://127.0.0.1:47801", browser);
        Assert.DoesNotContain(main.Host.ConnectOptions, o => o.Address.StartsWith("http://"));
    }

    [AvaloniaFact]
    public async Task A_self_signed_certificate_comes_with_the_start_of_its_fingerprint()
    {
        var (main, _) = await Hosting(allow: true);
        var option = main.Host.ConnectOptions.First(o => o.Address.StartsWith("https://"));
        Assert.Contains("AB:CD:EF:01:23:45:67:89", option.Detail);
        Assert.DoesNotContain("AA:BB", option.Detail);

        var (own, _) = await Hosting(allow: true, custom: true);
        Assert.DoesNotContain("AB:CD", own.Host.ConnectOptions.First(o => o.Address.StartsWith("https://")).Detail);
    }

    [Fact]
    public void A_web_fingerprint_is_read_from_the_status()
    {
        var bytes = Convert.FromHexString("0a08" + "0801" + "62024142" + "6801");
        var s = Assert.IsType<AgentMessage.Status>(AgentMessage.Decode(bytes));
        Assert.Equal("AB", s.WebFingerprint);
        Assert.True(s.WebCertCustom);
    }

    [AvaloniaFact]
    public async Task Switched_off_nothing_is_served_and_changing_it_asks_for_a_restart()
    {
        var (main, process) = await Hosting(allow: false);
        Assert.Null(process.Webs.Single());
        Assert.DoesNotContain(main.Host.ConnectOptions, o => o.Address.StartsWith("http"));
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
    public void A_browser_is_offered_view_input_and_clipboard_only_and_is_never_saved()
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
        Assert.True(decided.Perms.Clipboard);
        Assert.False(decided.Perms.File);
        Assert.False(decided.Perms.Audio);
        Assert.False(decided.Save);
    }

    [Fact]
    public void A_browser_gets_no_clipboard_unless_it_is_ticked()
    {
        var req = Samples.Request() with { Browser = true, OfferSave = false, Perms = new Permissions(true, true, false) };
        var link = new FakeLink();
        var vm = new ConsentViewModel(req, link, Samples.T0);
        Assert.False(vm.AllowClipboard);
        vm.Tick(Samples.T0.AddSeconds(2));
        vm.AllowCommand.Execute(null);
        Assert.False(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Perms.Clipboard);
    }

    [Fact]
    public void The_server_page_comes_from_its_signaling_address()
    {
        Assert.Equal("https://signal.example.com/", HostViewModel.ServerPage("wss://signal.example.com"));
        Assert.Equal("https://signal.example.com:8443/", HostViewModel.ServerPage("wss://signal.example.com:8443/path"));
        Assert.Equal("https://signal.example.com/", HostViewModel.ServerPage("signal.example.com"));
        Assert.Null(HostViewModel.ServerPage(""));
        Assert.Null(HostViewModel.ServerPage("ws://127.0.0.1:9000"));
    }
}
