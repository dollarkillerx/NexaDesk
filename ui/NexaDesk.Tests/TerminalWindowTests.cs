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

/// <summary>A session's own windows open where their title bars can be reached, not under the toolbar.</summary>
public class SideWindowPlacementTests
{
    readonly WindowService _windows = new();

    T Open<T>() where T : Avalonia.Controls.Window => _windows.OpenWindows.OfType<T>().Single();

    [AvaloniaFact]
    public void The_terminal_and_diagnostics_open_below_the_toolbar_folded_or_not()
    {
        var vm = new SessionViewModel(new FakeSession(1),
            new ConnectedEvent(1, Samples.HostKey, "工作室 Mac mini", new Permissions(true, true, true, Terminal: true), 2, false),
            "192.168.1.20:47800", () => Samples.T0);
        _windows.ShowSession(vm);
        var window = Open<SessionWindow>();
        var toolbar = Open<ToolbarWindow>();
        HeadlessUiTests.Pump();

        HeadlessUiTests.Click(toolbar, HeadlessUiTests.Find<Avalonia.Controls.Button>(toolbar, "TerminalButton"));
        HeadlessUiTests.Click(toolbar, HeadlessUiTests.Find<Avalonia.Controls.Button>(toolbar, "DiagnosticsButton"));
        var below = toolbar.Position.Y + toolbar.Bounds.Height * window.DesktopScaling;
        foreach (var w in new Avalonia.Controls.Window[] { Open<TerminalWindow>(), Open<DiagnosticsWindow>() })
        {
            Assert.True(w.Position.Y >= below, $"{w.GetType().Name} at {w.Position.Y}, toolbar ends at {below}");
            var centre = w.Position.X + w.Bounds.Width * window.DesktopScaling / 2;
            var sessionCentre = window.Position.X + window.Bounds.Width * window.DesktopScaling / 2;
            Assert.InRange(centre - sessionCentre, -2, 2);
        }

        // Folded: below the tab instead, still clear of it.
        Open<TerminalWindow>().Close();
        vm.CollapseToolbarCommand.Execute(null);
        HeadlessUiTests.Pump();
        var handle = Open<ToolbarHandleWindow>();
        typeof(ToolbarWindow).GetMethod("OnTerminal", System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance)!
            .Invoke(toolbar, [null, null]);
        HeadlessUiTests.Pump();
        Assert.True(Open<TerminalWindow>().Position.Y >= handle.Position.Y + handle.Bounds.Height * window.DesktopScaling);

        vm.DisconnectCommand.Execute(null);
        HeadlessUiTests.Pump();
        Assert.Empty(_windows.OpenWindows);
    }
}
