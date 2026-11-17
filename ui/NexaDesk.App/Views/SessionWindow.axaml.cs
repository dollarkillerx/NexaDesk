using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using NexaDesk.Localization;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>
/// The remote screen. Capture follows activation; pointer events over the video are
/// sent in view pixels, the same space as the size given to attach_view.
/// </summary>
public partial class SessionWindow : Window
{
    public SessionWindow()
    {
        InitializeComponent();
        Activated += (_, _) => Vm?.OnWindowActivated();
        Deactivated += (_, _) => Vm?.OnWindowDeactivated();
        Closing += (_, _) => Vm?.OnWindowClosing();
        AddHandler(PointerMovedEvent, OnPointerMoved, handledEventsToo: true);
        AddHandler(PointerPressedEvent, OnPointerPressed, handledEventsToo: true);
        AddHandler(PointerReleasedEvent, OnPointerReleased, handledEventsToo: true);
        AddHandler(PointerWheelChangedEvent, OnWheel, handledEventsToo: true);
        // Files dropped on the video go to the host. AppKit delivers a drag over the
        // native video view to this window's view, which registers for file drags.
        DragDrop.SetAllowDrop(this, true);
        AddHandler(DragDrop.DragOverEvent, (_, e) => FileDrop.Over(this, e));
        AddHandler(DragDrop.DropEvent, (_, e) => FileDrop.Drop(this, e));
    }

    SessionViewModel? Vm => DataContext as SessionViewModel;

    public VideoHost VideoView => Video;

    protected override void OnDataContextChanged(EventArgs e)
    {
        base.OnDataContextChanged(e);
        if (Vm is not { } vm) return;
        Video.Session = vm.Session;
        UseMenu(vm);
    }

    /// <summary>
    /// The way back to a folded toolbar besides its tab: a 会话 menu in the
    /// menu bar (macOS) and the same key as the host key. While input is captured the
    /// library sees that key first (the host key: keyboard released, toolbar shown);
    /// otherwise the menu's key equivalent or this window's key binding does it.
    /// </summary>
    void UseMenu(SessionViewModel vm)
    {
        var showKey = OperatingSystem.IsWindows()
            ? new KeyGesture(Key.G, KeyModifiers.Control | KeyModifiers.Alt)
            : new KeyGesture(Key.G, KeyModifiers.Control | KeyModifiers.Alt | KeyModifiers.Meta);
        KeyBindings.Clear();
        KeyBindings.Add(new KeyBinding { Gesture = showKey, Command = vm.ShowToolbarCommand });

        var show = new NativeMenuItem { Header = L.Get("Toolbar.MenuExpand"), Command = vm.ShowToolbarCommand, Gesture = showKey };
        var hide = new NativeMenuItem { Header = L.Get("Toolbar.MenuCollapse"), Command = vm.CollapseToolbarCommand };
        var fullScreen = new NativeMenuItem { Header = L.Get("Toolbar.FullScreen") };
        fullScreen.Click += (_, _) => ToggleFullScreen();
        var items = new List<NativeMenuItemBase> { show, hide, new NativeMenuItemSeparator(), fullScreen };
        var session = new NativeMenu();
        foreach (var item in items) session.Items.Add(item);
        var bar = new NativeMenu();
        bar.Items.Add(new NativeMenuItem { Header = L.Get("Toolbar.MenuSession"), Menu = session });
        NativeMenu.SetMenu(this, bar);
    }

    public void ToggleFullScreen() =>
        WindowState = WindowState == WindowState.FullScreen ? WindowState.Normal : WindowState.FullScreen;

    bool InVideo(PointerEventArgs e, out Point px)
    {
        var p = e.GetPosition(Video);
        var s = RenderScaling;
        px = new Point(p.X * s, p.Y * s);
        return p.X >= 0 && p.Y >= 0 && p.X < Video.Bounds.Width && p.Y < Video.Bounds.Height;
    }

    void OnPointerMoved(object? sender, PointerEventArgs e)
    {
        if (InVideo(e, out var px)) Vm?.PointerMoved(px.X, px.Y);
    }

    static uint? ButtonOf(PointerUpdateKind kind) => kind switch
    {
        PointerUpdateKind.LeftButtonPressed or PointerUpdateKind.LeftButtonReleased => 0,
        PointerUpdateKind.RightButtonPressed or PointerUpdateKind.RightButtonReleased => 1,
        PointerUpdateKind.MiddleButtonPressed or PointerUpdateKind.MiddleButtonReleased => 2,
        PointerUpdateKind.XButton1Pressed or PointerUpdateKind.XButton1Released => 3,
        PointerUpdateKind.XButton2Pressed or PointerUpdateKind.XButton2Released => 4,
        _ => null,
    };

    void OnPointerPressed(object? sender, PointerPressedEventArgs e)
    {
        if (Vm is not { } vm || !InVideo(e, out var px)) return;
        if (vm.OnVideoPressed()) return; // that click resumed control
        if (ButtonOf(e.GetCurrentPoint(Video).Properties.PointerUpdateKind) is { } b)
        {
            vm.PointerMoved(px.X, px.Y);
            vm.PointerButton(b, true);
        }
    }

    void OnPointerReleased(object? sender, PointerReleasedEventArgs e)
    {
        if (Vm is { } vm && ButtonOf(e.GetCurrentPoint(Video).Properties.PointerUpdateKind) is { } b)
            vm.PointerButton(b, false);
    }

    void OnWheel(object? sender, PointerWheelEventArgs e)
    {
        if (InVideo(e, out _)) Vm?.Wheel(e.Delta.X, e.Delta.Y);
    }
}
