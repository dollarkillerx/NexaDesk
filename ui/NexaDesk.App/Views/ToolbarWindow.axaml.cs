using Avalonia;
using Avalonia.Controls;
using Avalonia.Interactivity;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>
/// The session toolbar (§10.3): an owned, borderless, topmost window kept at the top
/// centre of the session window, because nothing can be drawn over the native video.
/// </summary>
public partial class ToolbarWindow : Window
{
    Window? _session;

    public event Action? DiagnosticsRequested;

    public ToolbarWindow()
    {
        InitializeComponent();
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
        Opened += (_, _) => Place();
    }

    public void Place()
    {
        if (_session == null) return;
        var s = _session.DesktopScaling;
        var x = _session.Position.X + (int)((_session.Bounds.Width - Width) / 2 * s);
        Position = new PixelPoint(x, _session.Position.Y + (int)(8 * s));
    }

    void OnDiagnostics(object? sender, RoutedEventArgs e) => DiagnosticsRequested?.Invoke();

    void OnFullScreen(object? sender, RoutedEventArgs e)
    {
        if (_session == null) return;
        _session.WindowState = _session.WindowState == WindowState.FullScreen ? WindowState.Normal : WindowState.FullScreen;
    }
}
