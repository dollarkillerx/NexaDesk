using Avalonia;
using Avalonia.Controls;

namespace NexaDesk.Views;

/// <summary>The latency HUD: kept at the session window's top left, passing clicks through to the video.</summary>
public partial class HudWindow : Window
{
    Window? _session;

    public HudWindow()
    {
        InitializeComponent();
        Opened += (_, _) =>
        {
            MacWindows.IgnoreMouse(this);
            Place();
        };
    }

    public void Follow(Window session)
    {
        _session = session;
        session.PositionChanged += (_, _) => Place();
        session.Resized += (_, _) => Place();
    }

    public void Place()
    {
        if (_session == null) return;
        var s = _session.DesktopScaling;
        // Below the toolbar, which sits at the top centre.
        Position = new PixelPoint(_session.Position.X + (int)(12 * s), _session.Position.Y + (int)(96 * s));
    }
}
