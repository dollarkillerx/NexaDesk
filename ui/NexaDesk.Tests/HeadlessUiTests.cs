using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Threading;
using Avalonia.VisualTree;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>The real windows, headless, driven with simulated keyboard and mouse input.</summary>
public class HeadlessUiTests
{
    readonly FakeClient _client = new();
    readonly FakeLink _link = new();
    readonly FakeProcess _process = new();
    readonly FakePermissions _perms = new();
    readonly WindowService _windows = new();

    MainViewModel Main(params RecentDevice[] recent) =>
        new(_client, new MemoryRecent(recent), _link, _process, _perms, _windows, new FakeAddresses(), "工作室 Mac mini",
            () => Samples.T0);

    internal static void Pump()
    {
        Dispatcher.UIThread.RunJobs();
        AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        Dispatcher.UIThread.RunJobs();
    }

    internal static T Find<T>(Window w, string name) where T : Control =>
        w.FindControl<T>(name) ?? throw new Xunit.Sdk.XunitException($"no control named {name}");

    internal static void Click(Window w, Control c)
    {
        Pump();
        var p = c.TranslatePoint(new Point(c.Bounds.Width / 2, c.Bounds.Height / 2), w)
                ?? throw new Xunit.Sdk.XunitException("control is not in the window");
        w.MouseMove(p);
        w.MouseDown(p, MouseButton.Left);
        w.MouseUp(p, MouseButton.Left);
        Pump();
    }

    static void Type(Window w, TextBox box, string text)
    {
        box.Focus();
        Pump();
        w.KeyTextInput(text);
        Pump();
    }

    T Open<T>() where T : Window => _windows.OpenWindows.OfType<T>().Single();

    [AvaloniaFact]
    public void Connect_flow_from_typing_to_the_session_window_and_back()
    {
        var main = new MainWindow { DataContext = Main() };
        main.Show();
        Pump();
        var connect = Find<Button>(main, "ConnectButton");
        Assert.False(connect.IsEffectivelyEnabled);

        Type(main, Find<TextBox>(main, "AddressBox"), "192.168.1.20:47800");
        Type(main, Find<TextBox>(main, "CodeBox"), "abcd-2345");
        Assert.True(connect.IsEffectivelyEnabled);
        Click(main, connect);

        var req = Assert.Single(_client.Requests);
        Assert.Equal(new ConnectAuth.Code("ABCD2345"), req.Auth);
        Assert.True(Find<Button>(main, "CancelButton").IsVisible);

        _client.Raise(Samples.Connected(req.Tag));
        Pump();
        var session = Open<SessionWindow>();
        var toolbar = Open<ToolbarWindow>();
        Assert.True(session.IsVisible && toolbar.IsVisible);
        Assert.Equal("工作室 Mac mini — NexaDesk", session.Title);
        Assert.Same(_client.Sessions[0], session.VideoView.Session);
        Assert.Null(session.VideoView.AttachResult); // headless: no native view, nothing attached

        _client.Raise(new DisconnectedEvent(req.Tag, DisconnectReason.HostEnded, "ended"));
        Pump();
        Assert.Empty(_windows.OpenWindows.OfType<SessionWindow>());
        Assert.Empty(_windows.OpenWindows.OfType<ToolbarWindow>());
        Assert.True(Find<Border>(main, "ConnectError").IsVisible);
        Assert.Contains("对方结束了会话", main.GetVisualDescendants().OfType<TextBlock>().Select(t => t.Text ?? "").First(t => t.Contains("会话已结束")));
        main.Close();
    }

    [AvaloniaFact]
    public void A_wrong_code_is_shown_next_to_the_form()
    {
        var main = new MainWindow { DataContext = Main() };
        main.Show();
        Type(main, Find<TextBox>(main, "AddressBox"), "10.0.0.2");
        Type(main, Find<TextBox>(main, "CodeBox"), "ABCD2345");
        Click(main, Find<Button>(main, "ConnectButton"));
        _client.Raise(new DisconnectedEvent(_client.Requests[0].Tag, DisconnectReason.AuthFailed, "authentication failed"));
        Pump();
        Assert.True(Find<Border>(main, "ConnectError").IsVisible);
        Assert.Equal("10.0.0.2:47800", _client.Requests[0].Address);
        main.Close();
    }

    [AvaloniaFact]
    public void Consent_dialog_focuses_deny_and_enter_denies()
    {
        var vm = new ConsentViewModel(Samples.Request(), _link, DateTimeOffset.Now);
        _windows.ShowConsent(vm);
        var dialog = Open<ConsentDialog>();
        Pump();
        var deny = Find<Button>(dialog, "DenyButton");
        var allow = Find<Button>(dialog, "AllowButton");
        Assert.True(deny.IsFocused, "deny has focus");
        Assert.True(deny.IsDefault && deny.IsCancel);
        Assert.False(allow.IsDefault);
        Assert.False(allow.IsEffectivelyEnabled, "allow is not armed yet");
        Assert.Equal("王小明的 MacBook Pro", Find<TextBlock>(dialog, "DeviceNameText").Text);
        Assert.StartsWith("3F2A 9C1B", Find<TextBlock>(dialog, "FingerprintText").Text);

        dialog.KeyPress(Key.Enter, RawInputModifiers.None, PhysicalKey.Enter, null);
        Pump();
        Assert.Equal(UiRequest.Decide.Deny(41), Assert.Single(_link.Sent));
        Assert.Empty(_windows.OpenWindows.OfType<ConsentDialog>());
    }

    [AvaloniaFact]
    public void Consent_dialog_allows_with_the_boxes_as_ticked_once_armed()
    {
        var vm = new ConsentViewModel(Samples.Request(input: true), _link, DateTimeOffset.Now);
        _windows.ShowConsent(vm);
        var dialog = Open<ConsentDialog>();
        Pump();
        Click(dialog, Find<Button>(dialog, "AllowButton"));
        Assert.Empty(_link.Sent);

        vm.Tick(DateTimeOffset.Now.AddSeconds(2));
        Click(dialog, Find<CheckBox>(dialog, "InputBox"));   // untick input
        Click(dialog, Find<CheckBox>(dialog, "ClipboardBox")); // tick clipboard
        Click(dialog, Find<Button>(dialog, "AllowButton"));
        var d = Assert.IsType<UiRequest.Decide>(Assert.Single(_link.Sent));
        Assert.True(d.Allow);
        Assert.Equal(new Permissions(true, false, true), d.Perms);
        Assert.Empty(_windows.OpenWindows.OfType<ConsentDialog>());
    }

    [AvaloniaFact]
    public void Closing_the_consent_window_or_pressing_escape_denies()
    {
        var a = new ConsentViewModel(Samples.Request(1), _link, DateTimeOffset.Now);
        _windows.ShowConsent(a);
        Open<ConsentDialog>().Close();
        Pump();
        var b = new ConsentViewModel(Samples.Request(2), _link, DateTimeOffset.Now);
        _windows.ShowConsent(b);
        var dialog = Open<ConsentDialog>();
        Pump();
        dialog.KeyPress(Key.Escape, RawInputModifiers.None, PhysicalKey.Escape, null);
        Pump();
        Assert.Equal([UiRequest.Decide.Deny(1), UiRequest.Decide.Deny(2)], _link.Sent);
    }

    [AvaloniaFact]
    public void The_banner_refuses_to_close_and_disconnects_in_one_click()
    {
        var vm = new BannerViewModel(Samples.Session(), _link, DateTimeOffset.Now);
        _windows.ShowBanner(vm);
        var banner = Open<ControlBanner>();
        Pump();
        Assert.True(banner.Topmost);
        banner.Close();
        Pump();
        Assert.True(banner.IsVisible, "only the session ending removes it");
        banner.WindowState = WindowState.Minimized;
        Pump();
        Assert.Equal(WindowState.Normal, banner.WindowState);

        Assert.Equal("「王小明的 MacBook Pro」正在控制本机", Deep<TextBlock>(banner, "HeadlineText").Single().Text);

        // Folded into a small tab, never gone; unfolded again in one click.
        Click(banner, Find<Button>(banner, "CollapseBannerButton"));
        Pump();
        Assert.True(_windows.BannerStack.IsCollapsed);
        Assert.False(Deep<Button>(banner, "BannerDisconnectButton").Single().IsEffectivelyVisible);
        var tab = Find<Border>(banner, "ExpandBannerButton");
        Assert.True(tab.IsEffectivelyVisible);
        Assert.True(banner.IsVisible);

        // Dragging the tab moves it and leaves it folded.
        var before = banner.Position;
        var at = tab.TranslatePoint(new Point(tab.Bounds.Width / 2, tab.Bounds.Height / 2), banner)!.Value;
        banner.MouseMove(at);
        banner.MouseDown(at, MouseButton.Left);
        banner.MouseMove(at + new Point(40, 30));
        banner.MouseUp(at + new Point(40, 30), MouseButton.Left);
        Pump();
        Assert.True(_windows.BannerStack.IsCollapsed, "a drag is not a click");
        Assert.Equal(before + new PixelPoint(40, 30), banner.Position);

        // A click without moving unfolds it.
        Click(banner, tab);
        Pump();
        Assert.False(_windows.BannerStack.IsCollapsed);

        Click(banner, Deep<Button>(banner, "BannerDisconnectButton").Single());
        Assert.Equal(new UiRequest.Disconnect(7), Assert.Single(_link.Sent));
        _windows.HideBanner(vm);
        Pump();
        Assert.Empty(_windows.OpenWindows.OfType<ControlBanner>());
    }

    /// <summary>The folded tab covered the top of the screen a remote viewer looks at: its ×
    /// hides it until someone new connects, and the menu bar offers it back.</summary>
    [AvaloniaFact]
    public void The_folded_banner_can_be_hidden_until_someone_new_connects()
    {
        var first = new BannerViewModel(Samples.Session(7), _link, DateTimeOffset.Now);
        _windows.ShowBanner(first);
        var banner = Open<ControlBanner>();
        var tray = new TrayViewModel(new HostViewModel(_link, new FakeProcess(), new FakePermissions(), _windows,
            new FakeAddresses(), "Mac"), _windows);
        _windows.BannerStack.IsCollapsed = true;
        Pump();
        RenderTestsShim.Save(banner, "banner-tab", Avalonia.Styling.ThemeVariant.Dark);
        Assert.False(tray.ShowBannerCommand.CanExecute(null));

        Click(banner, Find<Button>(banner, "HideBannerButton"));
        Assert.False(banner.IsVisible, "hidden");
        Assert.True(_windows.BannersHidden);
        Assert.True(tray.BannerHidden);
        Assert.True(tray.ShowBannerCommand.CanExecute(null));

        // The menu bar shows it again.
        tray.ShowBannerCommand.Execute(null);
        Pump();
        Assert.True(banner.IsVisible);
        Assert.False(tray.BannerHidden);

        // Hidden again, someone new connecting brings it back: nobody is controlled unseen.
        Click(banner, Find<Button>(banner, "HideBannerButton"));
        Assert.False(banner.IsVisible);
        _windows.ShowBanner(new BannerViewModel(Samples.Session(8), _link, DateTimeOffset.Now));
        Pump();
        Assert.True(banner.IsVisible);
        Assert.False(_windows.BannersHidden);
    }

    [AvaloniaFact]
    public void Several_people_share_one_banner_and_one_fold()
    {
        var first = new BannerViewModel(Samples.Session(7), _link, DateTimeOffset.Now);
        var second = new BannerViewModel(Samples.Session(8) with { DeviceName = "李雷的 iPad" }, _link, DateTimeOffset.Now);
        _windows.ShowBanner(first);
        _windows.ShowBanner(second);
        var banner = Assert.Single(_windows.OpenWindows.OfType<ControlBanner>());
        Pump();
        Assert.Equal(2, Deep<TextBlock>(banner, "HeadlineText").Count());

        Click(banner, Find<Button>(banner, "CollapseBannerButton"));
        Pump();
        Assert.StartsWith("2 人 · ", Find<TextBlock>(banner, "FoldedText").Text);

        // One leaves: the same window, one row, back to just the time when folded.
        _windows.HideBanner(second);
        Pump();
        Assert.Same(banner, Assert.Single(_windows.OpenWindows.OfType<ControlBanner>()));
        Assert.DoesNotContain("人", Find<TextBlock>(banner, "FoldedText").Text);
        _windows.HideBanner(first);
        Pump();
        Assert.Empty(_windows.OpenWindows.OfType<ControlBanner>());
    }

    /// <summary>Named controls inside item templates, which FindControl cannot see.</summary>
    static IEnumerable<T> Deep<T>(Window w, string name) where T : Control =>
        w.GetVisualDescendants().OfType<T>().Where(c => c.Name == name);

    [AvaloniaFact]
    public void Session_capture_follows_activation_and_the_host_key_brings_the_toolbar_back()
    {
        var session = new FakeSession(5);
        var vm = new SessionViewModel(session, Samples.Connected(5), "192.168.1.20:47800");
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var toolbar = Open<ToolbarWindow>();
        window.Activate();
        Pump();
        Assert.True(session.InputActive.LastOrDefault());

        // Pointer over the video goes out in view pixels while captured.
        window.MouseMove(new Point(320, 200));
        Pump();
        Assert.Contains(new SessionCommand.PointerMove(320, 200), session.Commands);

        Click(toolbar, Find<Button>(toolbar, "CollapseToolbarButton"));
        Assert.False(toolbar.IsVisible);

        vm.OnEvent(new HostKeyEvent(5));
        Pump();
        Assert.False(session.InputActive.Last());
        Assert.True(toolbar.IsVisible);
        var resume = Find<Button>(toolbar, "ResumeButton");
        Assert.True(resume.IsVisible);
        Click(toolbar, resume);
        Assert.True(session.InputActive.Last());

        Click(toolbar, Find<RadioButton>(toolbar, "ModeGame"));
        Assert.Contains(new SessionCommand.SetMode(StreamMode.Game), session.Commands);

        Click(toolbar, Find<Button>(toolbar, "ToolbarDisconnectButton"));
        Assert.True(session.IsClosed);
        Assert.Empty(_windows.OpenWindows);
    }

    [AvaloniaFact]
    public void Hosting_flow_shows_the_code_the_request_and_the_banner()
    {
        _link.ConnectResults.Enqueue("no agent yet");
        _link.ConnectResults.Enqueue(null);
        var main = new MainWindow { DataContext = Main() };
        main.Show();
        Pump();
        Click(main, Find<Button>(main, "NavHostButton"));
        Click(main, Find<Button>(main, "StartHostingButton"));
        for (var i = 0; i < 50 && _link.Sent.Count == 0; i++)
        {
            Thread.Sleep(10);
            Pump();
        }
        Assert.Single(_process.Starts);
        _link.Raise(Samples.Status());
        Pump();
        Assert.Equal("K7QM-2XPA", Find<TextBlock>(main, "CodeText").Text);
        Assert.True(Find<Button>(main, "StopHostingButton").IsVisible);

        _link.Raise(Samples.Request(3));
        Pump();
        Assert.Single(_windows.OpenWindows.OfType<ConsentDialog>());
        _link.Raise(new AgentMessage.RequestClosed(3, "cancelled"));
        Pump();
        Assert.Empty(_windows.OpenWindows.OfType<ConsentDialog>());

        _link.Raise(new AgentMessage.SessionStarted(Samples.Session(8)));
        Pump();
        Assert.Single(_windows.OpenWindows.OfType<ControlBanner>());
        _link.Raise(new AgentMessage.SessionEnded(8, "x", "done"));
        Pump();
        Assert.Empty(_windows.OpenWindows.OfType<ControlBanner>());

        Click(main, Find<Button>(main, "StopHostingButton"));
        Assert.IsType<UiRequest.Shutdown>(_link.Sent.Last());
        Assert.True(Find<Button>(main, "StartHostingButton").IsVisible);
        main.Close();
    }

    [AvaloniaFact]
    public void Missing_screen_recording_opens_the_guide()
    {
        _perms.State = new PermissionState(false, false);
        var main = new MainWindow { DataContext = Main() };
        _windows.Owner = main;
        main.Show();
        Click(main, Find<Button>(main, "NavHostButton"));
        Click(main, Find<Button>(main, "StartHostingButton"));
        var guide = Open<PermissionGuideWindow>();
        Pump();
        Assert.Equal("未授予", Find<TextBlock>(guide, "ScreenRecordingState").Text);
        _perms.State = new PermissionState(true, true);
        Click(guide, Find<Button>(guide, "RecheckButton"));
        Assert.Equal("已授予", Find<TextBlock>(guide, "ScreenRecordingState").Text);
        Assert.Empty(_process.Starts);
        main.Close();
    }
}
