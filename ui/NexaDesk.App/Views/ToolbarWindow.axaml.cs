using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Platform.Storage;
using NexaDesk.Localization;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>
/// The session toolbar (§10.3): an owned, borderless window (not topmost: it covered the main
/// window and every other app) kept at the top
/// centre of the session window, because nothing can be drawn over the native video.
/// </summary>
public partial class ToolbarWindow : Window
{
    Window? _session;

    public event Action? DiagnosticsRequested;

    public ToolbarWindow()
    {
        InitializeComponent();
        DragDrop.SetAllowDrop(this, true);
        AddHandler(DragDrop.DragOverEvent, (_, e) => FileDrop.Over(this, e));
        AddHandler(DragDrop.DropEvent, (_, e) => FileDrop.Drop(this, e));
    }

    SessionViewModel? Vm => DataContext as SessionViewModel;

    async void OnSendFiles(object? sender, RoutedEventArgs e)
    {
        if (Vm is not { CanSendFiles: true } vm) return;
        var picked = await StorageProvider.OpenFilePickerAsync(new FilePickerOpenOptions
        {
            Title = L.Format("Toolbar.SendFilesTo", vm.HostName),
            AllowMultiple = true,
        });
        var paths = picked.Select(f => f.TryGetLocalPath()).OfType<string>().ToList();
        if (paths.Count > 0) vm.Transfers.SendFiles(paths);
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
        // It sizes itself to what it shows: centred again whenever that changes.
        SizeChanged += (_, _) => Place();
    }

    /// <summary>Space left at the sides of the session window.</summary>
    const double SideMargin = 24;

    /// <summary>Centred near the top of the session window, and never wider than it: what
    /// does not fit wraps (the buttons in the first row) rather than going off the edge.</summary>
    public void Place()
    {
        if (_session == null) return;
        var room = Math.Max(360, _session.Bounds.Width - SideMargin);
        if (Math.Abs(MaxWidth - room) > 0.5)
        {
            MaxWidth = room;
            MinWidth = Math.Min(980, room);
        }
        var s = _session.DesktopScaling;
        var x = _session.Position.X + (int)((_session.Bounds.Width - Bounds.Width) / 2 * s);
        Position = new PixelPoint(x, _session.Position.Y + (int)(8 * s));
    }

    void OnDiagnostics(object? sender, RoutedEventArgs e) => DiagnosticsRequested?.Invoke();

    /// <summary>The host's command line, asked for from the toolbar.</summary>
    public event Action? TerminalRequested;

    void OnTerminal(object? sender, RoutedEventArgs e) => TerminalRequested?.Invoke();

    void OnFullScreen(object? sender, RoutedEventArgs e)
    {
        if (_session == null) return;
        _session.WindowState = _session.WindowState == WindowState.FullScreen ? WindowState.Normal : WindowState.FullScreen;
    }
}
