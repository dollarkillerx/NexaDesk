using Avalonia.Controls;
using Avalonia.Media;
using NexaDesk.Localization;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>The host's command line, beside its screen. Closing it ends the shell.</summary>
public sealed class TerminalWindow : Window
{
    public TerminalHost Host { get; }

    public TerminalWindow(SessionViewModel session)
    {
        Title = L.Format("Terminal.Title", session.HostName);
        Width = 860;
        Height = 540;
        MinWidth = 360;
        MinHeight = 200;
        Background = new SolidColorBrush(Color.Parse("#161616"));
        Host = new TerminalHost { Session = session.Session };
        Content = Host;
    }
}
