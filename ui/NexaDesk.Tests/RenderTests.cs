using Avalonia;
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
/// Renders every view with sample data, light and dark, to ui/out/*.png so they can be
/// looked at without opening a real window.
/// </summary>
public class RenderTests
{
    static string OutDir()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !Directory.Exists(Path.Combine(dir.FullName, "NexaDesk.App"))) dir = dir.Parent;
        var path = Path.Combine(dir?.FullName ?? Path.GetTempPath(), "out");
        Directory.CreateDirectory(path);
        return path;
    }

    static void Save(Window w, string name, ThemeVariant theme)
    {
        HeadlessUiTests_Pump();
        var frame = w.CaptureRenderedFrame() ?? throw new Xunit.Sdk.XunitException("no frame");
        var file = Path.Combine(OutDir(), $"{name}-{(theme == ThemeVariant.Dark ? "dark" : "light")}.png");
#pragma warning disable CS0618 // the replacement takes encoder options these tests do not need
        frame.Save(file);
#pragma warning restore CS0618
        Assert.True(new FileInfo(file).Length > 1000);
    }

    static void HeadlessUiTests_Pump()
    {
        for (var i = 0; i < 3; i++)
        {
            Avalonia.Threading.Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        }
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
    }

    static IEnumerable<ThemeVariant> Themes() => [ThemeVariant.Light, ThemeVariant.Dark];

    static void WithTheme(ThemeVariant theme, Action a)
    {
        var app = Application.Current!;
        var before = app.RequestedThemeVariant;
        app.RequestedThemeVariant = theme;
        try { a(); }
        finally { app.RequestedThemeVariant = before; }
    }

    static MainViewModel SampleMain(FakeClient client, FakeLink link, IWindowService windows)
    {
        var now = Samples.T0;
        var recent = new MemoryRecent(
            new RecentDevice("192.168.1.20:47800", "工作室 Mac mini", Samples.HostKey, now.AddHours(-2).ToUnixTimeSeconds()),
            new RecentDevice("100.88.12.4:47800", "家里的 iMac", "bb", now.AddDays(-3).ToUnixTimeSeconds()));
        var vm = new MainViewModel(client, recent, link, new FakeProcess(), new FakePermissions(), windows,
            new FakeAddresses("192.168.1.35", "100.101.22.7", "127.0.0.1"), "王小明的 MacBook Pro", () => now);
        client.Raise(new ReadyEvent(0, Samples.ViewerKey, [new KnownHost(Samples.HostKey, "工作室 Mac mini", 0)], ""));
        return vm;
    }

    [AvaloniaFact]
    public void Main_window_idle()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var vm = SampleMain(new FakeClient(), new FakeLink(), new RecordingWindows());
                vm.Connect.Address = "192.168.1.20:47800";
                var w = new MainWindow { DataContext = vm };
                w.Show();
                Save(w, "main-idle", theme);
                w.Close();
            });
    }

    [AvaloniaFact]
    public async Task Main_window_hosting_with_an_error()
    {
        foreach (var theme in Themes())
        {
            var client = new FakeClient();
            var link = new FakeLink();
            link.ConnectResults.Enqueue(null);
            var vm = SampleMain(client, link, new RecordingWindows());
            await vm.Host.StartCommand.ExecuteAsync(null);
            link.Raise(Samples.Status(input: false));
            link.Raise(new AgentMessage.Pairings([
                new PairedDevice(Samples.ViewerKey, "id", "王小明的 MacBook Pro", new Permissions(true, true, true), 1,
                    (ulong)Samples.T0.AddMinutes(-20).ToUnixTimeSeconds(), false),
                new PairedDevice("cc00dd11ee22ff3344556677889900aabbccddeeff00112233445566778899aa", "id2", "会议室 Windows",
                    new Permissions(true, false, false), 1, (ulong)Samples.T0.AddDays(-5).ToUnixTimeSeconds(), true),
            ]));
            vm.Connect.Address = "10.0.0.2:47800";
            vm.Connect.Code = "ABCD2345";
            vm.Connect.ConnectCommand.Execute(null);
            client.Raise(new DisconnectedEvent(client.Requests[0].Tag, DisconnectReason.AuthFailed, ""));
            WithTheme(theme, () =>
            {
                var w = new MainWindow { DataContext = vm };
                w.Show();
                Save(w, "main-hosting", theme);
                w.Close();
            });
        }
    }

    [AvaloniaFact]
    public void Consent_dialog()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var vm = new ConsentViewModel(Samples.Request(input: true, takeover: true), new FakeLink(), DateTimeOffset.Now);
                var w = new ConsentDialog { DataContext = vm };
                w.Show();
                Save(w, "consent", theme);
                w.Close();
            });
    }

    [AvaloniaFact]
    public void Consent_dialog_paired_device_without_accessibility()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var req = Samples.Request(input: true, offerSave: false, inputAvailable: false) with
                {
                    Auth = AuthKind.Paired,
                    OfferSkipNextTime = true,
                };
                var vm = new ConsentViewModel(req, new FakeLink(), DateTimeOffset.Now);
                var w = new ConsentDialog { DataContext = vm };
                w.Show();
                Save(w, "consent-paired", theme);
                w.Close();
            });
    }

    [AvaloniaFact]
    public void Control_banner()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var vm = new BannerViewModel(Samples.Session(), new FakeLink(), Samples.T0);
                vm.Tick(Samples.T0.AddMinutes(3).AddSeconds(12));
                var w = new ControlBanner { DataContext = vm };
                w.Show();
                Save(w, "banner", theme);
                w.MayClose = true;
                w.Close();
            });
    }

    static SessionViewModel Session()
    {
        var t = Samples.T0;
        var vm = new SessionViewModel(new FakeSession(1), Samples.Connected(1), "192.168.1.20:47800", () => t = t.AddSeconds(1));
        vm.OnEvent(new StreamChangedEvent(1, 2, StreamMode.Balanced, 3024, 1964, 60));
        vm.OnEvent(new InputCaptureEvent(1, CaptureState.Degraded));
        vm.OnEvent(new PasteGateEvent(1, 18, false));
        vm.OnEvent(new StatsEvent(1, new StreamStats(1000, 0, 2, 1000, 0, 990, 3400, 2, 1, 0, 50_000_000)));
        vm.OnEvent(new StatsEvent(1, new StreamStats(1060, 1, 3, 1060, 0, 1050, 3100, 3, 1, 0, 52_400_000)));
        return vm;
    }

    [AvaloniaFact]
    public void Session_window_toolbar_and_diagnostics()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var vm = Session();
                var s = new SessionWindow { DataContext = vm, Width = 960, Height = 600 };
                s.Show();
                Save(s, "session", theme);
                var t = new ToolbarWindow { DataContext = vm };
                t.Show();
                Save(t, "toolbar", theme);
                vm.OnHostKey();
                Save(t, "toolbar-released", theme);
                var d = new DiagnosticsWindow { DataContext = vm.Diagnostics };
                d.Show();
                Save(d, "diagnostics", theme);
                d.Close();
                t.Close();
                s.Close();
            });
    }

    [AvaloniaFact]
    public void Permission_guides()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var perms = new FakePermissions { State = new PermissionState(false, true) };
                var w = new PermissionGuideWindow { DataContext = new PermissionGuideViewModel(perms, PermissionContext.Hosting) };
                w.Show();
                Save(w, "permissions-hosting", theme);
                w.Close();
                perms.State = new PermissionState(true, false);
                var v = new PermissionGuideWindow { DataContext = new PermissionGuideViewModel(perms, PermissionContext.Viewing) };
                v.Show();
                Save(v, "permissions-viewing", theme);
                v.Close();
            });
    }

    /// <summary>Reachable by ID: the hosting card shows the ID, the connect box takes one.</summary>
    [AvaloniaFact]
    public async Task Main_window_hosting_by_device_id()
    {
        foreach (var theme in Themes())
        {
            var client = new FakeClient();
            var link = new FakeLink();
            link.ConnectResults.Enqueue(null);
            var now = Samples.T0;
            var recent = new MemoryRecent(
                new RecentDevice("314159265", "家里的 iMac", Samples.HostKey, now.AddHours(-1).ToUnixTimeSeconds()),
                new RecentDevice("192.168.1.20:47800", "工作室 Mac mini", "bb", now.AddDays(-2).ToUnixTimeSeconds()));
            var vm = new MainViewModel(client, recent, link, new FakeProcess(), new FakePermissions(), new RecordingWindows(),
                new FakeAddresses("192.168.1.35", "127.0.0.1"), "王小明的 MacBook Pro", () => now,
                new MemorySettings(new AppSettings("wss://signal.example.com")));
            client.Raise(new ReadyEvent(0, Samples.ViewerKey, [new KnownHost(Samples.HostKey, "家里的 iMac", 0)], ""));
            await vm.Host.StartCommand.ExecuteAsync(null);
            link.Raise(Samples.Status(shortId: "271828182", signal: SignalState.Online));
            vm.Connect.Address = "314 159 265";
            vm.Connect.Code = "ABCD2345";
            vm.Connect.ConnectCommand.Execute(null);
            client.Raise(new ConnectingEvent(client.Requests[0].Tag, ConnectStage.Checking));
            WithTheme(theme, () =>
            {
                var w = new MainWindow { DataContext = vm };
                w.Show();
                Save(w, "main-hosting-id", theme);
                w.Close();
            });
        }
    }

    [AvaloniaFact]
    public async Task Main_window_signal_server_unreachable()
    {
        foreach (var theme in Themes())
        {
            var link = new FakeLink();
            link.ConnectResults.Enqueue(null);
            var vm = SampleMain(new FakeClient(), link, new RecordingWindows());
            vm.Settings.ServerInput = "http://bad";
            await vm.Host.StartCommand.ExecuteAsync(null);
            link.Raise(Samples.Status(signal: SignalState.Error, detail: "cannot reach the server: Connection refused"));
            WithTheme(theme, () =>
            {
                var w = new MainWindow { DataContext = vm };
                w.Show();
                Save(w, "main-signal-error", theme);
                w.Close();
            });
        }
    }

    [AvaloniaFact]
    public void Diagnostics_show_the_path_type()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var d = new DiagnosticsViewModel("ID 314 159 265");
                StreamStats S(ulong n, PathKind kind, string detail, ulong rtt) =>
                    new(n, 0, 0, n, 0, n, 21_500, 0, 0, 0, n * 20_000, kind, detail, rtt);
                d.Update(S(100, PathKind.Relay, "relay 203.0.113.10:3480", 48_000), Samples.T0);
                d.Update(S(160, PathKind.Relay, "relay 203.0.113.10:3480", 48_000), Samples.T0.AddSeconds(1));
                var w = new DiagnosticsWindow { DataContext = d };
                w.Show();
                Save(w, "diagnostics-relay", theme);
                d.Update(S(220, PathKind.Punched, "198.51.100.7:40211", 21_000), Samples.T0.AddSeconds(2));
                Save(w, "diagnostics-punched", theme);
                w.Close();
            });
    }

    // ----- file transfer, HUD, keyboard settings, background --------------------------

    static SessionViewModel SessionWithFiles()
    {
        var t = Samples.T0;
        var connected = new ConnectedEvent(1, Samples.HostKey, "工作室 Mac mini", new Permissions(true, true, true, File: true), 2, true);
        var vm = new SessionViewModel(new FakeSession(1), connected, "192.168.1.20:47800", () => t = t.AddSeconds(1));
        vm.OnEvent(new StreamChangedEvent(1, 2, StreamMode.Balanced, 3024, 1964, 60));
        vm.OnEvent(new InputCaptureEvent(1, CaptureState.Active));
        vm.OnEvent(new TransferEvent(1, new TransferInfo(1, true, "季度报告 Q3.pdf", 4_200_000, 4_200_000, TransferState.Done)));
        vm.OnEvent(new TransferEvent(1, new TransferInfo(2, true, "设计稿-最终版.sketch", 86_000_000, 31_500_000, TransferState.Running)));
        vm.OnEvent(new TransferEvent(1, new TransferInfo(3, false, "虚拟机镜像.iso", 3_400_000_000, 0, TransferState.NeedsConfirm)));
        vm.OnEvent(new TransferEvent(1, new TransferInfo(4, false, "notes.txt", 2_048, 1_024, TransferState.Failed, TransferFail.HashMismatch)));
        SegmentLatency[] segs =
        [
            new(LatencySegment.HostAndNetwork, 14_800, 23_600), new(LatencySegment.Receive, 1_900, 4_200),
            new(LatencySegment.DecodeQueue, 300, 1_100), new(LatencySegment.Decode, 3_100, 6_800),
            new(LatencySegment.PresentQueue, 4_200, 8_300), new(LatencySegment.Present, 600, 1_400),
            new(LatencySegment.Total, 25_400, 41_900),
        ];
        vm.OnEvent(new StatsEvent(1, new StreamStats(1000, 0, 2, 1000, 0, 990, 3400, 0, 0, 0, 50_000_000, PathKind.Punched,
            "198.51.100.7:40211", 21_000, Latency: segs)));
        vm.OnEvent(new StatsEvent(1, new StreamStats(1060, 0, 2, 1060, 0, 1050, 3400, 0, 0, 0, 52_400_000, PathKind.Punched,
            "198.51.100.7:40211", 21_000, Latency: segs)));
        return vm;
    }

    [AvaloniaFact]
    public void Toolbar_with_files_and_the_hud()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var vm = SessionWithFiles();
                vm.ToggleHudCommand.Execute(null);
                var t = new ToolbarWindow { DataContext = vm };
                t.Show();
                Save(t, "toolbar-files", theme);
                t.Close();
                var h = new HudWindow { DataContext = vm.Hud };
                h.Show();
                Save(h, "hud", theme);
                h.Close();
            });
    }

    [AvaloniaFact]
    public void Banner_with_files()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var session = Samples.Session() with { Perms = new Permissions(true, true, true, File: true) };
                var vm = new BannerViewModel(session, new FakeLink(), Samples.T0);
                vm.Tick(Samples.T0.AddMinutes(12).AddSeconds(5));
                vm.Transfers.OnUpdate(new TransferInfo(1, true, "合同扫描件.pdf", 12_600_000, 5_100_000, TransferState.Running));
                vm.Transfers.OnUpdate(new TransferInfo(2, false, "截图 2026-11-10.png", 840_000, 840_000, TransferState.Done,
                    Path: "/Users/me/Downloads/NexaDesk/截图 2026-11-10.png"));
                vm.Transfers.OnUpdate(new TransferInfo(3, false, "备份.zip", 2_100_000_000, 0, TransferState.NeedsConfirm));
                var w = new ControlBanner { DataContext = vm };
                w.Show();
                Save(w, "banner-files", theme);
                w.MayClose = true;
                w.Close();
            });
    }

    [AvaloniaFact]
    public void Consent_dialog_with_the_file_box()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var vm = new ConsentViewModel(Samples.Request(input: true), new FakeLink(), DateTimeOffset.Now) { AllowFile = true };
                var w = new ConsentDialog { DataContext = vm };
                w.Show();
                Save(w, "consent-file", theme);
                w.Close();
            });
    }

    [AvaloniaFact]
    public void Keyboard_settings_with_a_preview()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var keymap = new FakeKeymap();
                keymap.Saved[RemoteOs.Windows].InsertRange(3,
                [
                    new KeymapRule(true, "", "Cmd+Tab", ["Alt+Tab"], "Alt", "", true),
                    new KeymapRule(true, "", "Cmd+Q", ["Alt+F4"], "", "", true),
                    new KeymapRule(true, "", "Cmd+Backspace", ["Shift+Home", "Delete"], "", "", true),
                ]);
                var vm = new KeyboardSettingsViewModel(keymap);
                vm.Rules.First(r => r.When == "Cmd+Q").IsEnabled = false;
                vm.NewWhen = "Cmd+K";
                vm.NewThen = "Ctrl+Shift+K";
                vm.AddRuleCommand.Execute(null);
                vm.OnChord("Cmd+Shift+Z");
                var w = new KeyboardSettingsWindow { DataContext = vm, Height = 820 };
                w.Show();
                Save(w, "keyboard-settings", theme);
                w.Close();
            });
    }

    [AvaloniaFact]
    public void Main_window_with_login_and_menu_bar_settings()
    {
        foreach (var theme in Themes())
            WithTheme(theme, () =>
            {
                var login = new FakeLoginItem { AfterRegister = LoginItemStatus.RequiresApproval };
                var vm = new MainViewModel(new FakeClient(), new MemoryRecent(), new FakeLink(), new FakeProcess(), new FakePermissions(),
                    new RecordingWindows(), new FakeAddresses("192.168.1.35", "127.0.0.1"), "王小明的 MacBook Pro", () => Samples.T0,
                    loginItem: login, keymap: new FakeKeymap());
                vm.LoginItem.IsEnabled = true;
                var w = new MainWindow { DataContext = vm };
                w.Show();
                Save(w, "main-background", theme);
                w.Close();
            });
    }
}
