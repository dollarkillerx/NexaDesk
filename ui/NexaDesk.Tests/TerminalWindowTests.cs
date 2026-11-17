using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using NexaDesk.Proto;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>The terminal in the app: offered only when granted, and run in MySSH's terminal control.</summary>
public class TerminalWindowTests
{
    static SessionViewModel Session(bool terminal) =>
        new(new FakeSession(1),
            new ConnectedEvent(1, Samples.HostKey, "工作室 Mac mini", new Permissions(true, true, true, Terminal: terminal), 2, false),
            "192.168.1.20:47800", () => Samples.T0);

    [Fact]
    public void The_toolbar_offers_a_terminal_only_when_the_host_granted_it()
    {
        Assert.True(Session(terminal: true).CanUseTerminal);
        Assert.False(Session(terminal: false).CanUseTerminal);
    }

    [AvaloniaFact]
    public void The_window_runs_the_shell_in_the_terminal_control_and_closing_ends_it()
    {
        var vm = Session(terminal: true);
        var w = new TerminalWindow(vm);
        w.Show();
        Assert.Equal(0, w.OpenResult);
        var t = Assert.Single(((FakeSession)vm.Session).Terminals);
        Assert.Contains("工作室 Mac mini", w.Title);

        // The control sizes the shell, shows what it prints, and sends what is typed.
        Assert.True(t.Size.Cols >= 2 && t.Size.Rows >= 1);
        t.Feed("hello from the host\r\n");
        w.View.Focus();
        w.KeyTextInput("ls");
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
        Assert.Contains(t.Written, b => System.Text.Encoding.UTF8.GetString(b).Contains("ls"));

        // The shell ending shows in the title; closing the window ends the stream.
        t.End("exited");
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
        Assert.Contains("exited", w.Title);
        w.Close();
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
        Assert.True(t.Disposed);
    }
}
