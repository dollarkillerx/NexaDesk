using Avalonia.Controls;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>Opens and closes the app's windows for the view-models.</summary>
public sealed class WindowService : IWindowService
{
    ControlBanner? _banner;
    PermissionGuideWindow? _guide;
    readonly List<Window> _open = new();

    public Window? Owner { get; set; }

    /// <summary>Every window this service has open, for tests.</summary>
    public IReadOnlyList<Window> OpenWindows => _open;
    public ControlBanner? Banner => _banner;

    void Track(Window w)
    {
        _open.Add(w);
        w.Closed += (_, _) => _open.Remove(w);
    }

    public void ShowSession(SessionViewModel session)
    {
        var window = new SessionWindow { DataContext = session };
        var toolbar = new ToolbarWindow { DataContext = session };
        DiagnosticsWindow? diagnostics = null;
        toolbar.Follow(window);
        toolbar.DiagnosticsRequested += () =>
        {
            if (diagnostics != null)
            {
                diagnostics.Activate();
                return;
            }
            diagnostics = new DiagnosticsWindow { DataContext = session.Diagnostics };
            diagnostics.Closed += (_, _) => diagnostics = null;
            Track(diagnostics);
            diagnostics.Show(window);
        };
        session.PropertyChanged += (_, e) =>
        {
            if (e.PropertyName != nameof(SessionViewModel.IsToolbarVisible)) return;
            if (session.IsToolbarVisible && !toolbar.IsVisible)
            {
                toolbar.Show(window);
                toolbar.Place();
            }
            else if (!session.IsToolbarVisible && toolbar.IsVisible)
                toolbar.Hide();
        };
        session.ActivateRequested += () => window.Activate();
        session.Ended += (_, _, _) =>
        {
            diagnostics?.Close();
            toolbar.Close();
            window.Close();
        };
        window.Closed += (_, _) =>
        {
            diagnostics?.Close();
            toolbar.Close();
        };
        Track(window);
        Track(toolbar);
        window.Show();
        if (session.IsToolbarVisible) toolbar.Show(window);
    }

    public void ShowConsent(ConsentViewModel consent)
    {
        var dialog = new ConsentDialog { DataContext = consent };
        consent.Finished += _ => dialog.Close();
        Track(dialog);
        dialog.Show();
        dialog.Activate();
    }

    public void ShowBanner(BannerViewModel banner)
    {
        HideBanner();
        _banner = new ControlBanner { DataContext = banner };
        Track(_banner);
        _banner.Show();
    }

    public void HideBanner()
    {
        if (_banner == null) return;
        _banner.MayClose = true;
        _banner.Close();
        _banner = null;
    }

    public void ShowPermissionGuide(PermissionGuideViewModel guide)
    {
        _guide?.Close();
        _guide = new PermissionGuideWindow { DataContext = guide };
        _guide.Closed += (_, _) => _guide = null;
        Track(_guide);
        if (Owner is { IsVisible: true } owner) _guide.Show(owner);
        else _guide.Show();
    }

    public void ShowDiagnostics(DiagnosticsViewModel diagnostics)
    {
        var w = new DiagnosticsWindow { DataContext = diagnostics };
        Track(w);
        w.Show();
    }
}
