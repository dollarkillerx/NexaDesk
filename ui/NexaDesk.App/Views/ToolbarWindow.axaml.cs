using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Platform.Storage;
using NexaDesk.Localization;
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
