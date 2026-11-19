using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.VisualTree;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;
using static NexaDesk.Tests.HeadlessUiTests;

namespace NexaDesk.Tests;

/// <summary>
/// Everything was on one page: connecting, hosting, every setting and the about line. Now a
/// sidebar picks one page per task, and the settings that are set once and left are together
/// on their own page.
/// </summary>
public class MainNavigationTests
{
    static MainViewModel Main(FakeLink? link = null) =>
        new(new FakeClient(), new MemoryRecent(), link ?? new FakeLink(), new FakeProcess(), new FakePermissions(),
            new RecordingWindows(), new FakeAddresses(), "工作室 Mac mini", () => Samples.T0);

    static bool Shows(Window w, string page, string control)
    {
        var p = Find<ScrollViewer>(w, page);
        return p.IsVisible && p.GetVisualDescendants().OfType<Control>().Any(c => c.Name == control);
    }

    [AvaloniaFact]
    public void It_opens_on_connecting_and_the_sidebar_switches_pages()
    {
        var vm = Main();
        var w = new MainWindow { DataContext = vm };
        w.Show();
        Pump();
        Assert.Equal(MainPage.Connect, vm.Page);
        Assert.True(Shows(w, "ConnectPage", "AddressBox"));
        Assert.True(Find<Button>(w, "NavConnectButton").Classes.Contains("selected"));

        foreach (var (button, page, shown) in new[]
        {
            ("NavHostButton", "HostPage", "StartHostingButton"),
            ("NavPairedButton", "PairedPage", "PairedNeedsHostingText"),
            ("NavSettingsButton", "SettingsPage", "LanguageBox"),
            ("NavConnectButton", "ConnectPage", "ConnectButton"),
        })
        {
            Click(w, Find<Button>(w, button));
            Assert.True(Shows(w, page, shown), page);
            Assert.True(Find<Button>(w, button).Classes.Contains("selected"));
            Assert.Single(new[] { "ConnectPage", "HostPage", "PairedPage", "SettingsPage" }, n => Find<ScrollViewer>(w, n).IsVisible);
        }
        w.Close();
    }

    [AvaloniaFact]
    public void The_settings_set_once_are_all_on_the_settings_page()
    {
        var vm = Main();
        var w = new MainWindow { DataContext = vm };
        w.Show();
        vm.Page = MainPage.Settings;
        Pump();
        foreach (var name in new[]
        {
            "LanguageBox", "LoginItemBox", "EditPasswordButton", "AnnounceOnLanBox", "AllowBrowsersBox", "AllowAgentsBox",
            "ListenExpander", "AdvancedExpander", "McpExpander", "ViewerPermissionsButton", "VersionText",
        })
            Assert.True(Shows(w, "SettingsPage", name), name);
        // …and none of them crowd the page for letting others in.
        vm.Page = MainPage.Host;
        Pump();
        Assert.False(Find<ScrollViewer>(w, "HostPage").GetVisualDescendants().OfType<Control>()
            .Any(c => c.Name is "EditPasswordButton" or "AllowBrowsersBox" or "ListenExpander"));
        w.Close();
    }

    [AvaloniaFact]
    public async Task The_sidebar_says_whether_this_computer_can_be_reached_and_leads_there()
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue(null);
        var vm = Main(link);
        var w = new MainWindow { DataContext = vm };
        w.Show();
        Pump();
        Assert.Equal("允许控制：未开启", Find<TextBlock>(w, "SidebarStateText").Text);
        await vm.Host.StartCommand.ExecuteAsync(null);
        link.Raise(Samples.Status());
        Pump();
        Assert.Equal("允许控制：已开启", Find<TextBlock>(w, "SidebarStateText").Text);
        Click(w, Find<Button>(w, "HostStatusButton"));
        Assert.Equal(MainPage.Host, vm.Page);
        Assert.Equal("K7QM-2XPA", Find<TextBlock>(w, "CodeText").Text);
        w.Close();
    }
}
