using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Threading;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>
/// Always on screen while this Mac is being viewed or controlled (§9.3). There is no
/// setting to hide it and it refuses to close or minimize; only the session ending
/// takes it away. It floats above other windows on every Space.
/// </summary>
public partial class ControlBanner : Window
{
    readonly DispatcherTimer _timer = new() { Interval = TimeSpan.FromSeconds(1) };

    /// <summary>Set by the window service once the session has ended.</summary>
    public bool MayClose { get; set; }

    /// <summary>Where the person put it: its centre and top, in screen pixels. Null until it is
    /// first moved; until then it sits centred at the top of the screen.</summary>
    PixelPoint? _anchor;
    bool _placing;
    /// <summary>The folded tab's press: where it started, and whether it became a drag.</summary>
    (PixelPoint Pointer, PixelPoint Window)? _press;
    bool _dragged;

    /// <summary>Movement (pixels) that turns a press on the folded tab into a drag.</summary>
    const int DragThreshold = 4;


    public ControlBanner()
    {
        InitializeComponent();
        _timer.Tick += (_, _) => Stack?.Tick(DateTimeOffset.Now);
        Opened += (_, _) =>
        {
            _timer.Start();
            Place();
            MacWindows.FloatOnAllSpaces(this);
        };
        Closed += (_, _) => _timer.Stop();
        // Placed again whenever it folds, unfolds, or someone joins or leaves: centred at the
        // top, or where the person moved it.
        SizeChanged += (_, _) => Place();
        PositionChanged += (_, _) => Remember();
        PointerPressed += (_, e) =>
        {
            if (!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed || e.Source is Button) return;
            if (Stack is { IsCollapsed: true })
            {
                // The folded tab: a click unfolds, a drag moves (see the pointer handlers below).
                _press = (ScreenPoint(e), Position);
                _dragged = false;
                e.Pointer.Capture(this);
                e.Handled = true;
                return;
            }
            BeginMoveDrag(e);
        };
        PointerMoved += (_, e) =>
        {
            if (_press is not { } press) return;
            var now = ScreenPoint(e);
            var (dx, dy) = (now.X - press.Pointer.X, now.Y - press.Pointer.Y);
            if (!_dragged && Math.Abs(dx) + Math.Abs(dy) < DragThreshold) return;
            _dragged = true;
            Position = new PixelPoint(press.Window.X + dx, press.Window.Y + dy);
        };
        PointerReleased += (_, e) =>
        {
            if (_press is null) return;
            _press = null;
            e.Pointer.Capture(null);
            if (!_dragged) Stack?.ToggleCollapsedCommand.Execute(null);
            _dragged = false;
        };
        PointerCaptureLost += (_, _) => _press = null;
        // Files the person at this Mac drops on someone's row go to that viewer, and only
        // if that session was granted files; the agent checks that again.
        DragDrop.SetAllowDrop(this, true);
        AddHandler(DragDrop.DragOverEvent, (_, e) =>
        {
            e.DragEffects = Target(e) is { CanSendFiles: true } && DroppedFiles.HasFiles(e) ? DragDropEffects.Copy : DragDropEffects.None;
            e.Handled = true;
        });
        AddHandler(DragDrop.DropEvent, (_, e) =>
        {
            e.Handled = true;
            if (Target(e) is not { CanSendFiles: true } vm) return;
            var paths = DroppedFiles.Of(e);
            if (paths.Count > 0) vm.Transfers.SendFiles(paths);
        });
    }

    BannerStackViewModel? Stack => DataContext as BannerStackViewModel;

    /// <summary>The session whose row the files are over, else the newest that takes files.</summary>
    BannerViewModel? Target(DragEventArgs e) =>
        (e.Source as StyledElement)?.DataContext as BannerViewModel
        ?? Stack?.Sessions.LastOrDefault(s => s.CanSendFiles);

    protected override void OnClosing(WindowClosingEventArgs e)
    {
        if (!MayClose) e.Cancel = true;
        base.OnClosing(e);
    }

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == WindowStateProperty && WindowState != WindowState.Normal)
            Dispatcher.UIThread.Post(() => WindowState = WindowState.Normal);
    }

    PixelPoint ScreenPoint(PointerEventArgs e) => this.PointToScreen(e.GetPosition(this));

    /// <summary>A move by the person (a drag of either form), not by <see cref="Place"/>.</summary>
    void Remember()
    {
        if (_placing || !IsVisible) return;
        var scaling = Screens.ScreenFromWindow(this)?.Scaling ?? 1.0;
        _anchor = new PixelPoint(Position.X + (int)(Bounds.Width * scaling / 2), Position.Y);
    }

    public void Place()
    {
        var screen = Screens.ScreenFromWindow(this) ?? Screens.Primary;
        if (screen == null) return;
        var area = screen.WorkingArea;
        var (w, h) = ((int)(Bounds.Width * screen.Scaling), (int)(Bounds.Height * screen.Scaling));
        // Centred on where it was put (folded and unfolded share that centre), kept on screen.
        var (cx, top) = _anchor is { } a ? (a.X, a.Y) : (area.X + area.Width / 2, area.Y + (int)(8 * screen.Scaling));
        var x = Math.Clamp(cx - w / 2, area.X, Math.Max(area.X, area.Right - w));
        var y = Math.Clamp(top, area.Y, Math.Max(area.Y, area.Bottom - h));
        _placing = true;
        try
        {
            Position = new PixelPoint(x, y);
        }
        finally
        {
            _placing = false;
        }
    }
}
