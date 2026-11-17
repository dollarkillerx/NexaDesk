using Avalonia.Controls;
using Avalonia.Media;
using NexaDesk.Localization;
using NexaDesk.Terminal;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>
/// The host's command line, beside its screen: MySSH's native terminal control
/// (<see cref="TerminalView"/>, the same on macOS and Windows) on one terminal stream of
/// the session. Closing the window ends the shell.
/// </summary>
public sealed class TerminalWindow : Window
{
    public TerminalView View { get; }

    /// <summary>The open status code: 0 when the terminal opened.</summary>
    public int OpenResult { get; }

    ITerminalBackend? _backend;

    public TerminalWindow(SessionViewModel session)
    {
        var title = L.Format("Terminal.Title", session.HostName);
        Title = title;
        Width = 860;
        Height = 540;
        MinWidth = 360;
        MinHeight = 200;
        Background = new SolidColorBrush(Color.Parse("#161616"));
        View = new TerminalView();
        if (this.TryFindResource("MonoFont", out var mono) && mono is FontFamily font) View.FontFamily = font;
        Content = View;

        // 80 × 24 to start: the view sends its real size as soon as it is laid out.
        var (backend, status) = session.Session.IsClosed ? (null, -2) : session.Session.StartTerminal(80, 24);
        OpenResult = status;
        _backend = backend;
        if (backend is NexaDesk.Native.NativeTerminal native) native.Backlog = () => View.PendingBytes;
        if (backend != null) View.Attach(backend);
        View.SessionEnded += (_, exit) =>
            Title = string.IsNullOrWhiteSpace(exit.Reason) ? title : $"{title} — {exit.Reason}";
        Opened += (_, _) => View.Focus();
        Closed += async (_, _) =>
        {
            View.Detach();
            if (_backend is { } b)
            {
                _backend = null;
                await b.DisposeAsync();
            }
        };
    }
}
