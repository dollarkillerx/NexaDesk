using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>
/// The folded toolbar: a small tab over the session window (an owned window like the
/// toolbar, since nothing can be drawn over the native video). A click unfolds it; a drag
/// moves it, and it stays where it was put relative to the session window, kept inside it.
/// The drag is done by hand, the same on macOS and Windows (as the control banner's tab).
/// </summary>
public partial class ToolbarHandleWindow : Window
{
    /// <summary>Movement (pixels) that turns a press into a drag.</summary>
    const int DragThreshold = 4;

    Window? _session;
    /// <summary>Where the person put it: its centre and top, as an offset in screen pixels from
    /// the top centre of the session window's video. Null until moved: then it sits at the top centre.</summary>
    PixelPoint? _offset;
    bool _placing;
    (PixelPoint Pointer, PixelPoint Window)? _press;
    bool _dragged;

    public ToolbarHandleWindow()
    {
        InitializeComponent();
        Opened += (_, _) => Place();
        // Its size is known only once laid out, and changes with the host name.
        SizeChanged += (_, _) => Place();
        PositionChanged += (_, _) => Remember();
        PointerPressed += (_, e) =>
        {
            if (!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed) return;
            _press = (ScreenPoint(e), Position);
            _dragged = false;
            e.Pointer.Capture(this);
            e.Handled = true;
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
            if (_dragged) Place(); // let go: kept over the video
            else (DataContext as SessionViewModel)?.ShowToolbarCommand.Execute(null);
            _dragged = false;
        };
        PointerCaptureLost += (_, _) => _press = null;
    }

    public void Follow(Window session)
    {
        _session = session;
        session.PositionChanged += (_, _) => Place();
        session.Resized += (_, _) => Place();
        session.PropertyChanged += (_, e) =>
        {
            if (e.Property == WindowStateProperty) Place();
        };
    }

    PixelPoint ScreenPoint(PointerEventArgs e) => this.PointToScreen(e.GetPosition(this));

    /// <summary>The top centre of the session window's video, below any title bar, in screen pixels.</summary>
    PixelPoint Anchor(Window session) => session.PointToScreen(new Point(session.Bounds.Width / 2, 0));

    double Scaling => _session?.DesktopScaling ?? DesktopScaling;

    /// <summary>A move by the person, not by <see cref="Place"/>.</summary>
    void Remember()
    {
        if (_placing || !IsVisible || _session == null) return;
        var anchor = Anchor(_session);
        _offset = new PixelPoint(Position.X + (int)(Bounds.Width * Scaling / 2) - anchor.X, Position.Y - anchor.Y);
    }

    public void Place()
    {
        if (_session == null) return;
        var s = Scaling;
        var anchor = Anchor(_session);
        var (w, h) = ((int)(Bounds.Width * s), (int)(Bounds.Height * s));
        var (cx, top) = _offset is { } o ? (anchor.X + o.X, anchor.Y + o.Y) : (anchor.X, anchor.Y);
        // Kept over the session window's video, so it can never be lost off its edges.
        var left = anchor.X - (int)(_session.Bounds.Width * s / 2);
        var right = left + (int)(_session.Bounds.Width * s);
        var bottom = anchor.Y + (int)(_session.Bounds.Height * s);
        var x = Math.Clamp(cx - w / 2, left, Math.Max(left, right - w));
        var y = Math.Clamp(top, anchor.Y, Math.Max(anchor.Y, bottom - h));
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
