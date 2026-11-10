using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Styling;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// No server by default: the LAN directly, Tailscale across networks (it relays when it cannot
/// punch through). The app listens everywhere, shows every way in, and recommends Tailscale.
/// </summary>
public class TailscaleFirstTests
{
    static byte[] Bytes(string hex) => Convert.FromHexString(hex.Replace(" ", ""));

    static TailnetDevicesEvent Running(string dns = "studio.tail1234.ts.net", string ip = "100.101.22.7") =>
        new(0, [], TailscaleState.Running, "studio", dns, [ip]);

    static TailnetDevicesEvent Absent() => new(0, [], TailscaleState.Absent);

    static MainViewModel Main(FakeClient client, FakeLink link, FakeProcess process, RecordingWindows windows,
        params string[] ips) =>
        new(client, new MemoryRecent(), link, process, new FakePermissions(), windows,
            new FakeAddresses(ips.Length == 0 ? ["192.168.1.35", "100.101.22.7", "127.0.0.1"] : ips),
            "王小明的 MacBook Pro", () => Samples.T0);

    static async Task<MainViewModel> Hosting(FakeClient client, RecordingWindows windows, string listen = "[::]:47800",
        string shortId = "", SignalState signal = SignalState.Off)
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue(null);
        var main = Main(client, link, new FakeProcess(), windows);
        await main.Host.StartCommand.ExecuteAsync(null);
        link.Raise(Samples.Status(shortId: shortId, signal: signal) with { Listen = listen });
        return main;
    }

    [Fact]
    public void The_tailnet_event_carries_this_machines_tailscale()
    {
        // Event{tailnet(41): {state RUNNING, self_name "m", self_dns_name "m.ts.net", self_ips ["100.64.0.1"]}}
        var e = Assert.IsType<TailnetDevicesEvent>(ClientCodec.DecodeEvent(
            Bytes("ca021b 1002 1a016d 22086d2e74732e6e6574 2a0a3130302e36342e302e31")));
        Assert.Equal(TailscaleState.Running, e.State);
        Assert.Equal("m", e.SelfName);
        Assert.Equal("m.ts.net", e.SelfDnsName);
        Assert.Equal(["100.64.0.1"], e.Ips);
        Assert.Empty(e.Devices);
        // No state field: absent, as the core sends when there is no tailnet address.
        Assert.Equal(TailscaleState.Absent, Assert.IsType<TailnetDevicesEvent>(ClientCodec.DecodeEvent(Bytes("ca0200"))).State);
    }

    [Fact]
    public async Task Hosting_lists_the_tailscale_name_first_then_the_lan()
    {
        var client = new FakeClient();
        var main = await Hosting(client, new RecordingWindows());
        client.Raise(Running());
        Assert.Equal(
            [new ConnectOption("Tailscale", "studio.tail1234.ts.net", "或 100.101.22.7"), new ConnectOption("局域网", "192.168.1.35")],
            main.Host.ConnectOptions);
        Assert.Equal("允许控制中 · studio.tail1234.ts.net", main.Tray.StatusText);
        Assert.False(main.Host.HasSignalText, "no server, nothing to say about one");
    }

    [Fact]
    public async Task Without_magicdns_the_tailnet_ip_is_listed_and_a_registered_id_comes_last()
    {
        var client = new FakeClient();
        var main = await Hosting(client, new RecordingWindows(), shortId: "271828182", signal: SignalState.Online);
        client.Raise(Running(dns: ""));
        Assert.Equal(
            [
                new ConnectOption("Tailscale", "100.101.22.7"),
                new ConnectOption("局域网", "192.168.1.35"),
                new ConnectOption("设备 ID", "271 828 182", "经自建信令服务器"),
            ],
            main.Host.ConnectOptions);
        Assert.True(main.Host.HasSignalText);
    }

    [Fact]
    public async Task A_single_address_and_another_port_are_shown_as_such()
    {
        var main = await Hosting(new FakeClient(), new RecordingWindows(), listen: "192.168.1.35:5000");
        Assert.Equal([new ConnectOption("局域网", "192.168.1.35:5000")], main.Host.ConnectOptions);
    }

    [Fact]
    public async Task Choosing_one_address_listens_only_there()
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var main = Main(new FakeClient(), link, process, new RecordingWindows());
        var host = main.Host;
        Assert.StartsWith("所有网络", host.ListenSummary);
        host.UseSuggestionCommand.Execute("100.101.22.7:47800");
        Assert.True(host.ListenOnOne);
        Assert.Equal("只在 100.101.22.7:47800", host.ListenSummary);
        await host.StartCommand.ExecuteAsync(null);
        Assert.Equal("100.101.22.7:47800", Assert.Single(process.Starts).Listen);
        host.ListenEverywhere = true;
        Assert.False(host.ListenOnOne);
        Assert.Null(host.ListenHint);
    }

    [Fact]
    public void Without_tailscale_both_halves_recommend_it()
    {
        var client = new FakeClient();
        var windows = new RecordingWindows();
        var main = Main(client, new FakeLink(), new FakeProcess(), windows, "192.168.1.35", "127.0.0.1");
        Assert.False(main.Connect.ShowTailscaleAdvice, "nothing until the core has looked");
        Assert.False(main.Host.ShowTailscaleAdvice);
        client.Raise(Absent());
        Assert.True(main.Connect.ShowTailscaleAdvice);
        Assert.True(main.Host.ShowTailscaleAdvice);
        Assert.Equal("未检测到 Tailscale", main.Tailscale.StatusText);
        main.Tailscale.DownloadCommand.Execute(null);
        Assert.Equal(["https://tailscale.com/download"], windows.OpenedUrls);

        client.Raise(new TailnetDevicesEvent(0, [], TailscaleState.AddressOnly, "", "", ["100.101.22.7"]));
        Assert.False(main.Connect.ShowTailscaleAdvice);
        Assert.Equal("Tailscale 已连接：100.101.22.7", main.Tailscale.StatusText);
    }

    [Fact]
    public void A_failure_outside_any_tailnet_points_at_tailscale()
    {
        var client = new FakeClient();
        var vm = Main(client, new FakeLink(), new FakeProcess(), new RecordingWindows()).Connect;
        void Fail(string address, DisconnectReason reason)
        {
            vm.Address = address;
            vm.Code = "ABCD2345";
            vm.ConnectCommand.Execute(null);
            client.Raise(new DisconnectedEvent(client.Requests.Last().Tag, reason, ""));
        }
        Fail("203.0.113.9", DisconnectReason.TimedOut);
        Assert.Contains("Tailscale", vm.Error!.Detail);
        Fail("100.64.0.2", DisconnectReason.TimedOut);
        Assert.DoesNotContain("Tailscale", vm.Error!.Detail);
        Fail("studio.tail1234.ts.net", DisconnectReason.Network);
        Assert.DoesNotContain("Tailscale", vm.Error!.Detail);
        Fail("192.168.1.20", DisconnectReason.AuthFailed);
        Assert.DoesNotContain("Tailscale", vm.Error!.Detail);
    }

    [Fact]
    public void A_device_id_without_a_server_suggests_tailscale_instead()
    {
        var vm = Main(new FakeClient(), new FakeLink(), new FakeProcess(), new RecordingWindows()).Connect;
        Assert.Equal("例如 192.168.1.20 或 studio.tail1234.ts.net", vm.AddressPlaceholder);
        vm.Address = "314 159 265";
        Assert.Contains("Tailscale", vm.AddressHint);
    }

    [Theory]
    [InlineData("100.64.0.2", true)]
    [InlineData("100.127.255.1:5000", true)]
    [InlineData("[fd7a:115c:a1e0::2]:47800", true)]
    [InlineData("studio.tail1234.ts.net", true)]
    [InlineData("studio.tail1234.ts.net:47800", true)]
    [InlineData("100.128.0.1", false)]
    [InlineData("192.168.1.20", false)]
    [InlineData("example.com", false)]
    public void Tailnet_targets_are_recognised(string address, bool tailnet) =>
        Assert.Equal(tailnet, Texts.IsTailnetTarget(address));

    [AvaloniaFact]
    public async Task Serverless_main_window_renders()
    {
        foreach (var theme in new[] { ThemeVariant.Light, ThemeVariant.Dark })
        {
            var app = Avalonia.Application.Current!;
            var before = app.RequestedThemeVariant;
            app.RequestedThemeVariant = theme;
            try
            {
                // Hosting with Tailscale running, LAN and tailnet peers listed.
                var client = new FakeClient();
                var main = await Hosting(client, new RecordingWindows());
                client.Raise(new ReadyEvent(0, Samples.ViewerKey, [], ""));
                client.Raise(new LanDevicesEvent(0, [
                    new LanDevice("工作室 Mac mini", "192.168.1.20:47800", "macos", "0123456789abcdef0123456789abcdef"),
                ]));
                client.Raise(new TailnetDevicesEvent(0, [
                    new TailnetDevice("家里的 iMac", "100.64.0.2:47800", false, "macOS", "imac.tail1234.ts.net"),
                ], TailscaleState.Running, "studio", "studio.tail1234.ts.net", ["100.101.22.7"]));
                var w = new MainWindow { DataContext = main };
                w.Show();
                RenderTestsShim.Save(w, "main-tailscale-hosting", theme);
                Assert.True(w.FindControl<Border>("HostTailscaleStatus")!.IsVisible);
                Assert.False(w.FindControl<Border>("HostTailscaleAdvice")!.IsVisible);
                Assert.False(w.FindControl<Expander>("AdvancedExpander")!.IsExpanded, "the server is tucked away");
                w.Close();

                // No Tailscale, not hosting: both halves recommend it.
                var c2 = new FakeClient();
                var idle = Main(c2, new FakeLink(), new FakeProcess(), new RecordingWindows(), "192.168.1.35", "127.0.0.1");
                c2.Raise(new ReadyEvent(0, Samples.ViewerKey, [], ""));
                c2.Raise(Absent());
                var w2 = new MainWindow { DataContext = idle };
                w2.Show();
                RenderTestsShim.Save(w2, "main-tailscale-absent", theme);
                Assert.True(w2.FindControl<Border>("HostTailscaleAdvice")!.IsVisible);
                Assert.True(w2.FindControl<Border>("ConnectTailscaleAdvice")!.IsVisible);
                w2.Close();
            }
            finally
            {
                app.RequestedThemeVariant = before;
            }
        }
    }
}
