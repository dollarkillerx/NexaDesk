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
        // Re-centred whenever it folds, unfolds, or someone joins or leaves.
        SizeChanged += (_, _) => Place();
        PointerPressed += (_, e) =>
        {
            if (e.GetCurrentPoint(this).Properties.IsLeftButtonPressed && e.Source is not Button)
                BeginMoveDrag(e);
        };
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

    public void Place()
    {
        var screen = Screens.ScreenFromWindow(this) ?? Screens.Primary;
        if (screen == null) return;
        var area = screen.WorkingArea;
        var w = (int)(Bounds.Width * screen.Scaling);
        Position = new PixelPoint(area.X + (area.Width - w) / 2, area.Y + (int)(8 * screen.Scaling));
    }
}
