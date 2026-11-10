using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Styling;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// Reconnecting (§4.4), the WSS relay and DERP paths (§4.6, §4.7) and the tailnet
/// device list, as the app shows them.
/// </summary>
public class ReliabilityTests
{
    static byte[] Bytes(string hex) => Convert.FromHexString(hex.Replace(" ", ""));

    static (SessionViewModel vm, FakeSession s) Session(bool input = true)
    {
        var s = new FakeSession(1);
        return (new SessionViewModel(s, Samples.Connected(1, input: input), "ID 314 159 265", () => Samples.T0), s);
    }

    [Fact]
    public void Reconnecting_and_tailnet_events_decode()
    {
        // Event{reconnecting(40): {attempt 2, reason "x", remaining 25000}, tag 3}
        var r = Assert.IsType<ReconnectingEvent>(ClientCodec.DecodeEvent(Bytes("c20209 0802 120178 18a8c301 a00103")));
        Assert.Equal(new ReconnectingEvent(3, 2, "x", 25000), r);
        // Event{tailnet(41): {devices: [{name "a", address "b", direct true}]}}
        var t = Assert.IsType<TailnetDevicesEvent>(ClientCodec.DecodeEvent(Bytes("ca020a 0a08 0a0161 120162 1801")));
        Assert.Equal(0ul, t.Tag);
        Assert.Equal(new TailnetDevice("a", "b", true, "", ""), Assert.Single(t.Devices));
        // Stats: the WSS relay (5) and DERP (6) path kinds.
        Assert.Equal(PathKind.RelayWss, Assert.IsType<StatsEvent>(ClientCodec.DecodeEvent(Bytes("4202 6005"))).Stats.Path);
        Assert.Equal(PathKind.TailscaleDerp, Assert.IsType<StatsEvent>(ClientCodec.DecodeEvent(Bytes("4202 6006"))).Stats.Path);
        Assert.Equal(PathKind.Unknown, Assert.IsType<StatsEvent>(ClientCodec.DecodeEvent(Bytes("4202 6007"))).Stats.Path);
    }

    [Fact]
    public void A_dropped_transport_keeps_the_window_releases_the_keyboard_and_comes_back()
    {
        var (vm, s) = Session();
        var ended = false;
        vm.Ended += (_, _, _) => ended = true;
        vm.OnWindowActivated();
        Assert.True(vm.InputActive);
        vm.IsToolbarVisible = false;

        vm.OnEvent(new ReconnectingEvent(1, 1, "timed out", 29_500));
        Assert.True(vm.IsReconnecting);
        Assert.False(ended);
        Assert.False(vm.IsEnded);
        Assert.True(vm.IsToolbarVisible, "the state is shown");
        Assert.False(vm.InputActive, "the keyboard is the user's while nothing is connected");
        Assert.Equal("连接中断，正在重新连接（第 1 次，最多再等 30 秒）…", vm.ReconnectText);
        Assert.Contains("重新连上后", vm.CaptureText);
        Assert.Equal("连接中断，正在重新连接…", vm.Diagnostics.PathText);

        vm.OnEvent(new ReconnectingEvent(1, 3, "timed out", 20_100));
        Assert.Contains("第 3 次", vm.ReconnectText);

        vm.OnEvent(Samples.Connected(1));
        Assert.False(vm.IsReconnecting);
        Assert.Null(vm.ReconnectText);
        Assert.True(vm.InputActive, "capture comes back with the window still active");
        Assert.Equal([true, false, true], s.InputActive);
        Assert.False(s.IsClosed);
    }

    [Fact]
    public void Giving_up_ends_the_session_once()
    {
        var (vm, s) = Session();
        var reasons = new List<DisconnectReason>();
        vm.Ended += (_, r, _) => reasons.Add(r);
        vm.OnEvent(new ReconnectingEvent(1, 6, "timed out", 900));
        vm.OnEvent(new DisconnectedEvent(1, DisconnectReason.Network, "could not reconnect within 30 s"));
        Assert.Equal([DisconnectReason.Network], reasons);
        Assert.True(s.IsClosed);
    }

    [Fact]
    public void Game_mode_warnings_show_but_not_over_the_reconnect_line()
    {
        var (vm, _) = Session();
        vm.OnEvent(new WarningEvent(1, "游戏模式：UDP 被阻断，当前经 WSS（TCP）中继传输，延迟和卡顿会明显增加"));
        Assert.True(vm.HasWarning);
        vm.OnEvent(new ReconnectingEvent(1, 1, "", 30_000));
        Assert.False(vm.HasWarning);
        vm.OnEvent(Samples.Connected(1));
        Assert.False(vm.HasWarning, "the old path's warning is gone");
    }

    [Fact]
    public void New_path_kinds_are_named()
    {
        var d = new DiagnosticsViewModel("ID 314 159 265");
        d.Update(new StreamStats(1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 1, PathKind.RelayWss, "relay (WSS)"), Samples.T0);
        Assert.Equal("中继（WSS，UDP 被阻断） · relay (WSS)", d.PathText);
        d.Update(new StreamStats(2, 0, 0, 2, 0, 2, 0, 0, 0, 0, 2, PathKind.TailscaleDerp, "100.64.0.2:47800 (DERP)"), Samples.T0.AddSeconds(1));
        Assert.StartsWith("Tailscale（DERP 中继）", d.PathText);
    }

    [Fact]
    public void Tailnet_devices_fill_the_address_or_connect_a_paired_one()
    {
        var client = new FakeClient();
        var recent = new MemoryRecent(
            new RecentDevice("100.64.0.2:47800", "studio", Samples.HostKey, Samples.T0.ToUnixTimeSeconds() - 60));
        var vm = new ConnectViewModel(client, recent, new RecordingWindows(), () => Samples.T0);
        Assert.False(vm.HasTailnet);
        client.Raise(new ReadyEvent(0, Samples.ViewerKey, [new KnownHost(Samples.HostKey, "studio", 0)], ""));
        client.Raise(new TailnetDevicesEvent(0, [
            new TailnetDevice("studio", "100.64.0.2:47800", true, "macOS", "studio.tail1234.ts.net"),
            new TailnetDevice("laptop", "100.64.0.3:47800", false, "windows", ""),
        ]));
        Assert.True(vm.HasTailnet);
        Assert.Equal(["直连", "经 DERP 中继"], vm.Tailnet.Select(t => t.PathText));
        Assert.Equal("100.64.0.3:47800 · windows", vm.Tailnet[1].Detail);

        var focused = false;
        vm.FocusCodeRequested += () => focused = true;
        vm.Tailnet[1].ConnectCommand.Execute(null);
        Assert.Empty(client.Requests);
        Assert.Equal("100.64.0.3:47800", vm.Address);
        Assert.True(focused);

        vm.Tailnet[0].ConnectCommand.Execute(null);
        var req = Assert.Single(client.Requests);
        Assert.Equal("100.64.0.2:47800", req.Address);
        Assert.Equal(new ConnectAuth.KnownHost(Samples.HostKey), req.Auth);

        client.Raise(new TailnetDevicesEvent(0, []));
        Assert.False(vm.HasTailnet);
    }

    [AvaloniaFact]
    public void Reconnecting_session_toolbar_and_tailnet_list_render()
    {
        foreach (var theme in new[] { ThemeVariant.Light, ThemeVariant.Dark })
        {
            var app = Avalonia.Application.Current!;
            var before = app.RequestedThemeVariant;
            app.RequestedThemeVariant = theme;
            try
            {
                var (vm, _) = Session();
                vm.OnEvent(new StreamChangedEvent(1, 2, StreamMode.Game, 2560, 1440, 120));
                vm.OnEvent(new WarningEvent(1, "游戏模式：UDP 被阻断，当前经 WSS（TCP）中继传输，延迟和卡顿会明显增加"));
                var t = new ToolbarWindow { DataContext = vm };
                t.Show();
                RenderTestsShim.Save(t, "toolbar-wss-warning", theme);
                vm.OnEvent(new ReconnectingEvent(1, 2, "timed out", 26_400));
                RenderTestsShim.Save(t, "toolbar-reconnecting", theme);
                Assert.True(t.FindControl<TextBlock>("ReconnectText")!.IsVisible);
                Assert.False(t.FindControl<TextBlock>("CaptureText")!.IsVisible);
                var s = new SessionWindow { DataContext = vm, Width = 960, Height = 600 };
                s.Show();
                RenderTestsShim.Save(s, "session-reconnecting", theme);
                s.Close();
                t.Close();

                var client = new FakeClient();
                var main = new MainViewModel(client, new MemoryRecent(), new FakeLink(), new FakeProcess(), new FakePermissions(),
                    new RecordingWindows(), new FakeAddresses("192.168.1.35", "100.101.22.7"), "王小明的 MacBook Pro", () => Samples.T0);
                client.Raise(new ReadyEvent(0, Samples.ViewerKey, [], ""));
                client.Raise(new TailnetDevicesEvent(0, [
                    new TailnetDevice("studio", "100.64.0.2:47800", true, "macOS", "studio.tail1234.ts.net"),
                    new TailnetDevice("laptop", "100.64.0.3:47800", false, "windows", ""),
                ]));
                var w = new MainWindow { DataContext = main };
                w.Show();
                RenderTestsShim.Save(w, "main-tailnet", theme);
                Assert.True(w.FindControl<StackPanel>("TailnetSection")!.IsVisible);
                w.Close();
            }
            finally
            {
                app.RequestedThemeVariant = before;
            }
        }
    }
}

/// <summary>The same output as <see cref="RenderTests"/>: ui/out/name-theme.png.</summary>
static class RenderTestsShim
{
    public static void Save(Window w, string name, ThemeVariant theme)
    {
        for (var i = 0; i < 3; i++)
        {
            Avalonia.Threading.Dispatcher.UIThread.RunJobs();
            Avalonia.Headless.AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        }
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
        var frame = w.CaptureRenderedFrame()
                    ?? throw new Xunit.Sdk.XunitException("no frame");
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !Directory.Exists(Path.Combine(dir.FullName, "NexaDesk.App"))) dir = dir.Parent;
        var outDir = Path.Combine(dir?.FullName ?? Path.GetTempPath(), "out");
        Directory.CreateDirectory(outDir);
        var file = Path.Combine(outDir, $"{name}-{(theme == ThemeVariant.Dark ? "dark" : "light")}.png");
#pragma warning disable CS0618
        frame.Save(file);
#pragma warning restore CS0618
        Assert.True(new FileInfo(file).Length > 1000);
    }
}
