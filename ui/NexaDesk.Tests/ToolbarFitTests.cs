using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Styling;
using Avalonia.Threading;
using NexaDesk.Localization;
using NexaDesk.Proto;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// The toolbar fits the session window: with everything a Windows host offers (two displays,
/// files, terminal, sound) and longer translations, its buttons wrap instead of running off
/// the edge, where collapse and disconnect were out of reach. And the lock button.
/// </summary>
public class ToolbarFitTests
{
    static ConnectedEvent Everything(ulong tag) => new(tag, Samples.HostKey, "DESKTOP-F4AR44K-TEST-SERVICE",
        new Permissions(true, true, true, File: true, Audio: true, Terminal: true), 2, true,
        [new DisplayInfo(0, "Main display", 1920, 1080), new DisplayInfo(1, "Display 2", 1920, 1080)]);

    static void Inside(Window w, string name)
    {
        var b = w.FindControl<Button>(name)!;
        Assert.True(b.IsVisible, name);
        var right = b.TranslatePoint(new Point(b.Bounds.Width, b.Bounds.Height), w)!.Value;
        Assert.True(right.X <= w.Bounds.Width + 0.5, $"{name} ends at {right.X}, the toolbar at {w.Bounds.Width}");
        Assert.True(right.Y <= w.Bounds.Height + 0.5, $"{name} below the toolbar");
    }

    [AvaloniaTheory]
    [InlineData(980)]
    [InlineData(700)]
    public void Collapse_and_disconnect_stay_inside_a_narrow_toolbar(double width)
    {
        {
            var vm = new SessionViewModel(new FakeSession(1), Everything(1), "192.168.0.13:47810", () => Samples.T0);
            var t = new ToolbarWindow { DataContext = vm, MinWidth = width, MaxWidth = width };
            t.Show();
            Dispatcher.UIThread.RunJobs();
            Assert.True(t.Bounds.Width <= width + 0.5);
            Inside(t, "CollapseToolbarButton");
            Inside(t, "ToolbarDisconnectButton");
            Inside(t, "FullScreenButton");
            Inside(t, "LockScreenButton");
            RenderTestsShim.Save(t, $"toolbar-fit-{width}", ThemeVariant.Dark);
            t.Close();
        }
    }

    [Fact]
    public void The_lock_button_asks_the_host_to_lock_its_screen()
    {
        var fake = new FakeSession(1);
        var vm = new SessionViewModel(fake, Everything(1), "192.168.0.13:47810", () => Samples.T0);
        vm.LockScreenCommand.Execute(null);
        Assert.IsType<SessionCommand.LockScreen>(fake.Commands.Last());
    }

    [Fact]
    public void Without_the_keyboard_there_is_no_lock()
    {
        var fake = new FakeSession(1);
        var vm = new SessionViewModel(fake, Samples.Connected(1, input: false), "192.168.0.13:47810", () => Samples.T0);
        Assert.False(vm.CanControl);
        vm.LockScreenCommand.Execute(null);
        Assert.DoesNotContain(fake.Commands, c => c is SessionCommand.LockScreen);
    }
}
