using Avalonia.Headless.XUnit;
using NexaDesk.Proto;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>The terminal in the app: offered only when granted, and only where a native view exists.</summary>
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
    public void Headless_there_is_no_native_view_so_no_shell_is_started()
    {
        var vm = Session(terminal: true);
        var w = new TerminalWindow(vm);
        w.Show();
        Assert.Null(w.Host.OpenResult);
        Assert.Empty(((FakeSession)vm.Session).OpenTerminals);
        Assert.Contains("工作室 Mac mini", w.Title);
        w.Close();
    }
}
