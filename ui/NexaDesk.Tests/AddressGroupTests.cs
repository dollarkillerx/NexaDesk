using Avalonia.Headless.XUnit;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// A Mac with virtual machines and containers has a bridge for each, and every one of them was
/// listed twice (app and browser) with the certificate note on every browser row. Now each way in
/// shows the addresses on a routed network, the rest folded, and the note once.
/// </summary>
public class AddressGroupTests
{
    const string Fp = "8E:5D:5A:C1:34:8C:A1:78:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:10:20:30:40:50:60:70:80";

    static readonly string[] ManyIps = ["192.168.0.70", "192.168.0.127", "192.168.64.1", "192.168.65.1", "100.104.200.119", "127.0.0.1"];

    static async Task<MainViewModel> Hosting(FakeAddresses addresses, bool browsers = true, string shortId = "271828182")
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var main = new MainViewModel(new FakeClient(), new MemoryRecent(), link, new FakeProcess(), new FakePermissions(),
            new RecordingWindows(), addresses, "Mac", () => Samples.T0,
            new MemorySettings(new AppSettings(SignalServer: "wss://150.251.158.58:8443", AllowBrowsers: browsers)));
        await main.Host.StartCommand.ExecuteAsync(null);
        link.Raise(Samples.Status(shortId: shortId, signal: SignalState.Online) with
        {
            Listen = "[::]:47800",
            WebFingerprint = browsers ? Fp : "",
        });
        return main;
    }

    static string[] Addresses(IEnumerable<ConnectOption> options) => options.Select(o => o.Address).ToArray();

    [AvaloniaFact]
    public async Task Routed_lan_addresses_come_first_and_bridges_are_folded()
    {
        var main = await Hosting(new FakeAddresses(ManyIps) { RoutedIps = ["192.168.0.70", "192.168.0.127"] });
        var app = main.Host.AppAddresses;
        Assert.Equal(["100.104.200.119", "192.168.0.70", "192.168.0.127"], Addresses(app.Main));
        Assert.Equal(["192.168.64.1", "192.168.65.1"], Addresses(app.More));
        Assert.All(app.More, o => Assert.Equal("其他网卡", o.Kind));
        Assert.False(app.Expanded, "folded until asked");
        Assert.Contains("2", app.MoreText);
        app.ToggleMoreCommand.Execute(null);
        Assert.True(app.Expanded);
        // The flat list (what the tray and the older views read) still has every address.
        Assert.Equal(
            ["100.104.200.119", "192.168.0.70", "192.168.0.127", "192.168.64.1", "192.168.65.1"],
            Addresses(main.Host.ConnectOptions.Where(o => !o.Address.StartsWith("https://"))));
    }

    [AvaloniaFact]
    public async Task Browsers_get_the_same_split_the_server_page_and_the_certificate_note_once()
    {
        var main = await Hosting(new FakeAddresses(ManyIps) { RoutedIps = ["192.168.0.70"] });
        var web = main.Host.WebAddresses;
        Assert.Equal(
            ["https://100.104.200.119:47801", "https://192.168.0.70:47801", "https://150.251.158.58:8443/"],
            Addresses(web.Main));
        Assert.Equal(["任何网络", "打开后输入设备 ID 271 828 182：经服务器中转，不需要安装"], new[] { web.Main[2].Kind, web.Main[2].Detail });
        Assert.Equal(["https://192.168.0.127:47801", "https://192.168.64.1:47801", "https://192.168.65.1:47801"], Addresses(web.More));
        Assert.Contains("8E:5D:5A:C1:34:8C:A1:78", web.Note);
        Assert.DoesNotContain("AA:BB", web.Note);
        Assert.All(web.Main.Concat(web.More).Where(o => o.Kind != "任何网络"), o => Assert.False(o.HasDetail));
    }

    [AvaloniaFact]
    public async Task Without_router_information_the_first_lan_address_stands_in()
    {
        var main = await Hosting(new FakeAddresses(ManyIps));
        Assert.Equal(["100.104.200.119", "192.168.0.70"], Addresses(main.Host.AppAddresses.Main));
        Assert.Equal(["192.168.0.127", "192.168.64.1", "192.168.65.1"], Addresses(main.Host.AppAddresses.More));
        Assert.All(main.Host.AppAddresses.More, o => Assert.Equal("局域网", o.Kind));
    }

    [AvaloniaFact]
    public async Task One_lan_address_and_no_browsers_leave_nothing_folded_and_no_browser_group()
    {
        var main = await Hosting(new FakeAddresses("192.168.1.35", "127.0.0.1"), browsers: false);
        Assert.Equal(["192.168.1.35"], Addresses(main.Host.AppAddresses.Main));
        Assert.False(main.Host.AppAddresses.HasMore);
        Assert.True(main.Host.WebAddresses.IsEmpty);
    }

    [AvaloniaFact]
    public async Task Stopping_empties_both_groups()
    {
        var main = await Hosting(new FakeAddresses(ManyIps) { RoutedIps = ["192.168.0.70"] });
        main.Host.StopCommand.Execute(null);
        Assert.True(main.Host.AppAddresses.IsEmpty);
        Assert.True(main.Host.WebAddresses.IsEmpty);
        Assert.False(main.Host.AppAddresses.HasMore);
    }
}
