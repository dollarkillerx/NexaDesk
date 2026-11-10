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
        HudWindow? hud = null;
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
            if (e.PropertyName == nameof(SessionViewModel.IsHudVisible))
            {
                if (session.IsHudVisible && hud == null)
                {
                    hud = new HudWindow { DataContext = session.Hud };
                    hud.Follow(window);
                    hud.Closed += (_, _) => hud = null;
                    Track(hud);
                    hud.Show(window);
                }
                else if (!session.IsHudVisible) hud?.Close();
                return;
            }
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
            hud?.Close();
            toolbar.Close();
            window.Close();
        };
        window.Closed += (_, _) =>
        {
            diagnostics?.Close();
            hud?.Close();
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

    KeyboardSettingsWindow? _keyboard;

    public void OpenUrl(string url)
    {
        // Only links this app wrote: never anything that came over the network.
        if (!url.StartsWith("https://", StringComparison.Ordinal)) return;
        try
        {
            using var _ = System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("/usr/bin/open", [url])
            {
                UseShellExecute = false,
            });
        }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException)
        {
        }
    }

    public void ShowKeyboardSettings(KeyboardSettingsViewModel settings)
    {
        if (_keyboard != null)
        {
            _keyboard.Activate();
            return;
        }
        _keyboard = new KeyboardSettingsWindow { DataContext = settings };
        _keyboard.Closed += (_, _) => _keyboard = null;
        Track(_keyboard);
        if (Owner is { IsVisible: true } owner) _keyboard.Show(owner);
        else _keyboard.Show();
    }

    public void ShowDiagnostics(DiagnosticsViewModel diagnostics)
    {
        var w = new DiagnosticsWindow { DataContext = diagnostics };
        Track(w);
        w.Show();
    }
}
