using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using NexaDesk.Localization;
using NexaDesk.Proto;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;
using static NexaDesk.Tests.HeadlessUiTests;

namespace NexaDesk.Tests;

/// <summary>A folded toolbar always has a way back: its tab (click; drag to move), the 会话 menu, the host key.</summary>
public class ToolbarWayBackTests
{
    readonly WindowService _windows = new();

    static SessionViewModel Session(FakeSession session, bool input = true) =>
        new(session, Samples.Connected(session.Tag, input), "192.168.1.20:47800", () => Samples.T0);

    T Open<T>() where T : Window => _windows.OpenWindows.OfType<T>().Single();

    static KeyModifiers ShowKeyModifiers => OperatingSystem.IsWindows()
        ? KeyModifiers.Control | KeyModifiers.Alt
        : KeyModifiers.Control | KeyModifiers.Alt | KeyModifiers.Meta;

    static RawInputModifiers ShowKeyRaw => OperatingSystem.IsWindows()
        ? RawInputModifiers.Control | RawInputModifiers.Alt
        : RawInputModifiers.Control | RawInputModifiers.Alt | RawInputModifiers.Meta;

    // ----- 隐藏 and the way back -------------------------------------------------------

    [Fact]
    public void Folding_and_unfolding_the_toolbar_leaves_capture_alone()
    {
        var fake = new FakeSession(3);
        var vm = Session(fake);
        vm.OnWindowActivated();
        var before = fake.InputActive.Count;
        vm.CollapseToolbarCommand.Execute(null);
        Assert.False(vm.IsToolbarVisible);
        Assert.True(vm.IsToolbarCollapsed);
        vm.ShowToolbarCommand.Execute(null);
        Assert.True(vm.IsToolbarVisible);
        Assert.False(vm.IsToolbarCollapsed);
        Assert.Equal(before, fake.InputActive.Count);
        Assert.True(vm.InputActive);
    }

    [Fact]
    public void The_fold_button_and_the_tab_say_how_to_get_the_toolbar_back()
    {
        var vm = Session(new FakeSession(3));
        Assert.Contains(SessionViewModel.ShowToolbarKeys, vm.CollapseToolbarTip);
        Assert.Contains(SessionViewModel.ShowToolbarKeys, vm.ShowToolbarTip);
    }

    [AvaloniaFact]
    public void Folding_leaves_a_tab_that_a_click_unfolds()
    {
        var fake = new FakeSession(4);
        var vm = Session(fake);
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var toolbar = Open<ToolbarWindow>();
        var handle = Open<ToolbarHandleWindow>();
        window.Activate();
        Pump();
        Assert.True(toolbar.IsVisible);
        Assert.False(handle.IsVisible, "no tab while the toolbar is there");

        Click(toolbar, Find<Button>(toolbar, "CollapseToolbarButton"));
        Assert.False(toolbar.IsVisible);
        Assert.True(handle.IsVisible, "folded, not gone");
        var tab = Find<Border>(handle, "ExpandToolbarTab");
        Assert.Equal(vm.ShowToolbarTip, ToolTip.GetTip(tab));
        Assert.Equal("工作室 Mac mini", Find<TextBlock>(handle, "FoldedHostName").Text);
        // At the top centre of the video.
        var top = window.PointToScreen(new Point(window.Bounds.Width / 2, 0));
        Assert.Equal(top.Y, handle.Position.Y);
        Assert.InRange(handle.Position.X + handle.Bounds.Width / 2 - top.X, -1, 1);

        Click(handle, tab);
        Assert.True(toolbar.IsVisible);
        Assert.False(handle.IsVisible);

        vm.DisconnectCommand.Execute(null);
        Pump();
        Assert.Empty(_windows.OpenWindows);
    }

    /// <summary>The tab covered the main window and other apps; now its × hides it, and the
    /// shortcut brings the toolbar back. Neither window is topmost any more.</summary>
    [AvaloniaFact]
    public void The_tab_can_be_hidden_and_neither_toolbar_window_covers_other_apps()
    {
        var vm = Session(new FakeSession(13));
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var toolbar = Open<ToolbarWindow>();
        var handle = Open<ToolbarHandleWindow>();
        window.Activate();
        Pump();
        Assert.False(toolbar.Topmost);
        Assert.False(handle.Topmost);
        Assert.Same(window, handle.Owner ?? window);

        vm.CollapseToolbarCommand.Execute(null);
        Pump();
        Assert.True(handle.IsVisible);
        RenderTestsShim.Save(handle, "toolbar-tab", Avalonia.Styling.ThemeVariant.Dark);
        var hide = Find<Button>(handle, "HideTabButton");
        Assert.Equal(vm.HideTabTip, ToolTip.GetTip(hide));
        Assert.Contains(SessionViewModel.ShowToolbarKeys, vm.HideTabTip);
        Click(handle, hide);
        Assert.True(vm.IsTabHidden);
        Assert.False(handle.IsVisible, "hidden");
        Assert.False(toolbar.IsVisible);

        // The shortcut (or the menu) brings the toolbar back; folded again, the tab shows.
        vm.ShowToolbarCommand.Execute(null);
        Pump();
        Assert.True(toolbar.IsVisible);
        Assert.False(vm.IsTabHidden);
        vm.CollapseToolbarCommand.Execute(null);
        Pump();
        Assert.True(handle.IsVisible);
    }

    [AvaloniaFact]
    public void Dragging_the_tab_moves_it_keeps_it_folded_and_it_follows_the_window()
    {
        var vm = Session(new FakeSession(12));
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var handle = Open<ToolbarHandleWindow>();
        vm.CollapseToolbarCommand.Execute(null);
        Pump();
        var tab = Find<Border>(handle, "ExpandToolbarTab");
        var at = tab.TranslatePoint(new Point(tab.Bounds.Width / 2, tab.Bounds.Height / 2), handle)!.Value;
        var before = handle.Position;

        handle.MouseMove(at);
        handle.MouseDown(at, MouseButton.Left);
        handle.MouseMove(at + new Point(40, 30));
        handle.MouseUp(at + new Point(40, 30), MouseButton.Left);
        Pump();
        Assert.True(vm.IsToolbarCollapsed, "a drag is not a click");
        Assert.Equal(before + new PixelPoint(40, 30), handle.Position);

        // A tiny wobble is still a click.
        var moved = handle.Position;
        handle.MouseMove(at);
        handle.MouseDown(at, MouseButton.Left);
        handle.MouseMove(at + new Point(1, 1));
        handle.MouseUp(at + new Point(1, 1), MouseButton.Left);
        Pump();
        Assert.True(vm.IsToolbarVisible);
        Assert.Equal(moved, handle.Position);

        // Folded again, it comes back where it was put, and moves with the window.
        vm.CollapseToolbarCommand.Execute(null);
        Pump();
        Assert.Equal(moved, handle.Position);
        window.Position += new PixelPoint(100, 50);
        Pump();
        Assert.Equal(moved + new PixelPoint(100, 50), handle.Position);
        window.Close();
    }

    [AvaloniaFact]
    public void A_tab_dragged_off_the_window_is_kept_over_the_video()
    {
        var vm = Session(new FakeSession(13));
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var handle = Open<ToolbarHandleWindow>();
        vm.CollapseToolbarCommand.Execute(null);
        Pump();
        var tab = Find<Border>(handle, "ExpandToolbarTab");
        var at = tab.TranslatePoint(new Point(tab.Bounds.Width / 2, tab.Bounds.Height / 2), handle)!.Value;
        handle.MouseMove(at);
        handle.MouseDown(at, MouseButton.Left);
        handle.MouseMove(at + new Point(-5000, -5000));
        handle.MouseUp(at + new Point(-5000, -5000), MouseButton.Left);
        Pump();
        Assert.True(vm.IsToolbarCollapsed);
        Assert.Equal(window.PointToScreen(new Point(0, 0)), handle.Position);
        window.Close();
    }

    [AvaloniaFact]
    public void The_tab_stays_in_full_screen()
    {
        var vm = Session(new FakeSession(4));
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var handle = Open<ToolbarHandleWindow>();
        window.WindowState = WindowState.FullScreen;
        Pump();
        vm.CollapseToolbarCommand.Execute(null);
        Pump();
        Assert.True(handle.IsVisible);
        window.ToggleFullScreen();
        Pump();
        Assert.True(handle.IsVisible, "leaving full screen keeps the tab");
        window.Close();
        Pump();
        Assert.Empty(_windows.OpenWindows);
    }

    [AvaloniaFact]
    public void The_show_key_brings_the_toolbar_back_when_input_is_not_captured()
    {
        // A view-only session never captures, so the library's host key is never seen:
        // the window's own key binding is the way back.
        var fake = new FakeSession(6);
        var vm = Session(fake, input: false);
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var toolbar = Open<ToolbarWindow>();
        window.Activate();
        Pump();
        vm.CollapseToolbarCommand.Execute(null);
        Pump();
        Assert.False(toolbar.IsVisible);

        window.KeyPress(Key.G, ShowKeyRaw, PhysicalKey.G, null);
        Pump();
        Assert.True(toolbar.IsVisible);
        Assert.Empty(fake.InputActive.Where(a => a));
        window.Close();
    }

    [AvaloniaFact]
    public void While_captured_the_host_key_brings_the_toolbar_back_and_hides_the_tab()
    {
        var fake = new FakeSession(7);
        var vm = Session(fake);
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var toolbar = Open<ToolbarWindow>();
        var handle = Open<ToolbarHandleWindow>();
        window.Activate();
        Pump();
        vm.CollapseToolbarCommand.Execute(null);
        Pump();
        Assert.True(handle.IsVisible);

        vm.OnEvent(new HostKeyEvent(7));
        Pump();
        Assert.True(toolbar.IsVisible);
        Assert.False(handle.IsVisible);
        window.Close();
    }

    [AvaloniaFact]
    public void The_session_menu_folds_and_unfolds_the_toolbar_with_the_same_key()
    {
        var vm = Session(new FakeSession(8));
        var window = new SessionWindow { DataContext = vm };
        var bar = NativeMenu.GetMenu(window);
        Assert.NotNull(bar);
        var top = Assert.IsType<NativeMenuItem>(Assert.Single(bar!.Items));
        Assert.Equal(L.Get("Toolbar.MenuSession"), top.Header);
        var items = top.Menu!.Items.OfType<NativeMenuItem>().ToList();
        var show = items.Single(i => i.Header == L.Get("Toolbar.MenuExpand"));
        var hide = items.Single(i => i.Header == L.Get("Toolbar.MenuCollapse"));
        Assert.Equal(new KeyGesture(Key.G, ShowKeyModifiers), show.Gesture);

        hide.Command!.Execute(null);
        Assert.False(vm.IsToolbarVisible);
        show.Command!.Execute(null);
        Assert.True(vm.IsToolbarVisible);
    }

    [AvaloniaFact]
    public void Renders_the_folded_tab()
    {
        var handle = new ToolbarHandleWindow { DataContext = Session(new FakeSession(11)) };
        handle.Show();
        RenderTestsShim.Save(handle, "toolbar-handle", Avalonia.Styling.ThemeVariant.Dark);
        handle.Close();
    }
}
