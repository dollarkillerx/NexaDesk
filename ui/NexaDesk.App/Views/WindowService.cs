using Avalonia;
using Avalonia.Controls;
using Avalonia.Input.Platform;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>Opens and closes the app's windows for the view-models.</summary>
public sealed class WindowService : IWindowService
{
    /// <summary>One banner window for everyone connected.</summary>
    ControlBanner? _banner;
    readonly BannerStackViewModel _stack = new();
    PermissionGuideWindow? _guide;
    readonly List<Window> _open = new();

    public Window? Owner { get; set; }

    /// <summary>Every window this service has open, for tests.</summary>
    public IReadOnlyList<Window> OpenWindows => _open;
    /// <summary>The banner window, for tests.</summary>
    public ControlBanner? Banner => _banner;
    public BannerStackViewModel BannerStack => _stack;

    void Track(Window w)
    {
        _open.Add(w);
        w.Closed += (_, _) => _open.Remove(w);
    }

    public void ShowSession(SessionViewModel session)
    {
        var window = new SessionWindow { DataContext = session };
        var toolbar = new ToolbarWindow { DataContext = session };
        var handle = new ToolbarHandleWindow { DataContext = session };
        HudWindow? hud = null;
        DiagnosticsWindow? diagnostics = null;
        TerminalWindow? terminal = null;
        toolbar.Follow(window);
        handle.Follow(window);
        toolbar.TerminalRequested += () =>
        {
            if (terminal != null)
            {
                terminal.Activate();
                return;
            }
            terminal = new TerminalWindow(session);
            terminal.Closed += (_, _) => terminal = null;
            Track(terminal);
            PlaceBelow(terminal, window, toolbar.IsVisible ? toolbar : handle);
            terminal.Show();
        };
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
            PlaceBelow(diagnostics, window, toolbar.IsVisible ? toolbar : handle);
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
                handle.Hide();
                toolbar.Show(window);
                toolbar.Place();
            }
            else if (!session.IsToolbarVisible && toolbar.IsVisible)
            {
                toolbar.Hide();
                // Folded, not gone: the tab stays in sight to unfold it.
                handle.Show(window);
                handle.Place();
            }
        };
        session.ActivateRequested += () => window.Activate();
        session.Ended += (_, _, _) =>
        {
            diagnostics?.Close();
            terminal?.Close();
            hud?.Close();
            handle.Close();
            toolbar.Close();
            window.Close();
        };
        window.Closed += (_, _) =>
        {
            diagnostics?.Close();
            terminal?.Close();
            hud?.Close();
            handle.Close();
            toolbar.Close();
        };
        Track(window);
        Track(toolbar);
        Track(handle);
        window.Show();
        if (session.IsToolbarVisible) toolbar.Show(window);
        else handle.Show(window);
    }

    /// <summary>
    /// Puts a session's own window (its terminal, its diagnostics) just below the toolbar,
    /// centred on the session window. The toolbar floats above every window, so a title
    /// bar opened under it could be neither seen nor dragged until the toolbar was folded.
    /// </summary>
    internal static void PlaceBelow(Window w, Window session, Window bar)
    {
        var s = session.DesktopScaling;
        var top = (bar.IsVisible ? bar.Position.Y + (int)Math.Ceiling(bar.Bounds.Height * s) : session.Position.Y)
            + (int)(12 * s);
        var x = session.Position.X + (int)((session.Bounds.Width - w.Width) / 2 * s);
        if (session.Screens.ScreenFromWindow(session)?.WorkingArea is { } area)
        {
            // Shorter rather than off the bottom of the screen, when it can be.
            if (w.CanResize && !double.IsNaN(w.Height))
            {
                var room = (area.Bottom - top) / s - 12;
                if (w.Height > room) w.Height = Math.Max(w.MinHeight, room);
            }
            x = Math.Clamp(x, area.X, Math.Max(area.X, area.Right - (int)(w.Width * s)));
        }
        w.WindowStartupLocation = WindowStartupLocation.Manual;
        w.Position = new PixelPoint(x, top);
    }

    public void ShowServerTrust(ServerTrustViewModel trust)
    {
        var dialog = new ServerTrustDialog { DataContext = trust };
        trust.Finished += _ => dialog.Close();
        Track(dialog);
        dialog.Show();
        dialog.Activate();
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
        if (_stack.Sessions.Contains(banner)) return;
        _stack.Sessions.Add(banner);
        if (_banner != null) return;
        _banner = new ControlBanner { DataContext = _stack };
        Track(_banner);
        _banner.Show();
    }

    public void HideBanner(BannerViewModel banner)
    {
        _stack.Sessions.Remove(banner);
        if (_stack.Sessions.Count > 0 || _banner == null) return;
        _banner.MayClose = true;
        _banner.Close();
        _banner = null;
        // The next first session starts unfolded.
        _stack.IsCollapsed = false;
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
        if (OpenUrlCommand(url, OperatingSystem.IsWindows()) is not { } command) return;
        try
        {
            using var _ = System.Diagnostics.Process.Start(command);
        }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException)
        {
        }
    }

    /// <summary>How to open <paramref name="url"/> in the default browser: <c>open</c> on macOS,
    /// the shell on Windows (which has no <c>/usr/bin/open</c>, so the link did nothing there).
    /// Only https links this app wrote, never anything that came over the network.</summary>
    internal static System.Diagnostics.ProcessStartInfo? OpenUrlCommand(string url, bool windows)
    {
        if (!url.StartsWith("https://", StringComparison.Ordinal)) return null;
        return windows
            ? new System.Diagnostics.ProcessStartInfo(url) { UseShellExecute = true }
            : new System.Diagnostics.ProcessStartInfo("/usr/bin/open", [url]) { UseShellExecute = false };
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

    public void CopyText(string text)
    {
        if (Owner?.Clipboard is { } clipboard) _ = clipboard.SetTextAsync(text);
    }
}
