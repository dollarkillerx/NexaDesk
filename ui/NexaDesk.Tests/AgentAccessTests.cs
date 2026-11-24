using Avalonia.Headless.XUnit;
using NexaDesk.Localization;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// AI agents (nexa-mcp, docs/MCP.md §4): off by default, a switch the running agent takes at
/// once, a dialog that says an AI is asking and proposes view and control only, and a banner
/// that never lets an AI's session pass for a person's.
/// </summary>
public class AgentAccessTests
{
    static MainViewModel Main(FakeLink link, bool allow) =>
        new(new FakeClient(), new MemoryRecent(), link, new FakeProcess(), new FakePermissions(), new RecordingWindows(),
            new FakeAddresses(["192.168.1.35"]), "Mac", () => Samples.T0,
            new MemorySettings(new AppSettings(AllowAgents: allow)));

    static async Task<(MainViewModel, FakeLink)> Hosting(bool allow)
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var main = Main(link, allow);
        await main.Host.StartCommand.ExecuteAsync(null);
        link.Raise(Samples.Status());
        return (main, link);
    }

    [Fact]
    public void Agents_are_off_until_switched_on_and_the_choice_is_saved()
    {
        var store = new MemorySettings();
        var settings = new SettingsViewModel(store);
        Assert.False(settings.AllowAgents);
        settings.AllowAgents = true;
        Assert.True(store.Current.AllowAgents);
        Assert.True(new SettingsViewModel(store).AllowAgents);
    }

    [AvaloniaFact]
    public async Task The_agent_is_told_the_setting_when_attached_and_whenever_it_changes()
    {
        var (main, link) = await Hosting(allow: false);
        Assert.Equal(new UiRequest.SetAllowAgents(false), link.Sent.OfType<UiRequest.SetAllowAgents>().Last());
        main.Settings.AllowAgents = true;
        Assert.Equal(new UiRequest.SetAllowAgents(true), link.Sent.OfType<UiRequest.SetAllowAgents>().Last());
        Assert.False(main.Host.RestartNeeded, "no restart: the agent takes it at once");
    }

    [Fact]
    public void An_agents_request_says_so()
    {
        var req = Samples.Request(input: true) with { Agent = true, DeviceName = "Mac mini (AI agent)" };
        var link = new FakeLink();
        var vm = new ConsentViewModel(req, link, Samples.T0);
        Assert.True(vm.IsAgent);
        Assert.True(vm.IsDevice, "an agent is a NexaDesk device: its fingerprint is shown");
        Assert.Equal(L.Format("Consent.HeadlineAgent", "Mac mini (AI agent)"), vm.Headline);
        Assert.NotEqual(L.Format("Consent.Headline", "Mac mini (AI agent)"), vm.Headline);
        Assert.False(new ConsentViewModel(Samples.Request(), link, Samples.T0).IsAgent);
        // A browser never counts as an agent, whatever the flag says.
        Assert.False(new ConsentViewModel(req with { Browser = true }, link, Samples.T0).IsAgent);
    }

    [Fact]
    public void An_agent_that_proved_itself_is_allowed_with_everything_unless_denied_in_time()
    {
        var all = new Permissions(View: true, Input: true, Clipboard: true, File: true, Audio: true, Terminal: true);
        var req = Samples.Request(input: true, timeoutMs: 10_000) with
        {
            Agent = true, Auth = AuthKind.Password, Perms = all, AllowUnlessDenied = true,
        };
        var link = new FakeLink();
        var vm = new ConsentViewModel(req, link, Samples.T0);
        Assert.Equal(L.Format("Consent.CountdownAllow", 9), vm.CountdownText);
        vm.Tick(Samples.T0.AddSeconds(5));
        Assert.Empty(link.Sent);
        // Nobody denied it: allowed, with everything, and nothing saved for later.
        vm.Tick(Samples.T0.AddSeconds(9));
        var decided = Assert.IsType<UiRequest.Decide>(Assert.Single(link.Sent));
        Assert.True(decided.Allow);
        Assert.Equal(all, decided.Perms);
        Assert.False(decided.Save);
        Assert.Equal(L.Get("Consent.AllowedByDefault"), vm.Outcome);

        // Deny in time, or close the window: denied.
        var denied = new ConsentViewModel(req, link, Samples.T0);
        denied.DenyCommand.Execute(null);
        Assert.False(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Allow);
        var closed = new ConsentViewModel(req, link, Samples.T0);
        closed.OnWindowClosed();
        Assert.False(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Allow);
        // Unticking something before the time is up is kept.
        var fewer = new ConsentViewModel(req, link, Samples.T0) { AllowTerminal = false };
        fewer.Tick(Samples.T0.AddSeconds(9));
        Assert.False(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Perms.Terminal);
        // Any other request still says no when the time is up.
        var person = new ConsentViewModel(Samples.Request(timeoutMs: 10_000), link, Samples.T0);
        person.Tick(Samples.T0.AddSeconds(9));
        Assert.False(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Allow);
    }

    [Fact]
    public void The_banner_names_an_agent_as_an_ai()
    {
        var link = new FakeLink();
        var agent = new BannerViewModel(Samples.Session() with { Agent = true, DeviceName = "Claude" }, link, Samples.T0);
        Assert.True(agent.IsAgent);
        Assert.Equal(L.Format("Host.AgentControlling", "Claude"), agent.Headline);
        var viewing = new BannerViewModel(Samples.Session() with
        {
            Agent = true,
            DeviceName = "Claude",
            Perms = new Permissions(true, false, false),
        }, link, Samples.T0);
        Assert.Equal(L.Format("Host.AgentViewing", "Claude"), viewing.Headline);
        var person = new BannerViewModel(Samples.Session(), link, Samples.T0);
        Assert.False(person.IsAgent);
        Assert.DoesNotContain("AI", person.Headline);
    }


    [Fact]
    public void An_agent_is_offered_the_command_line_unticked_and_gets_it_only_when_ticked()
    {
        var req = Samples.Request(input: true) with { Agent = true, Perms = new Permissions(true, true, false) };
        var link = new FakeLink();
        var vm = new ConsentViewModel(req, link, Samples.T0);
        Assert.True(vm.OfferTerminal);
        Assert.False(vm.AllowTerminal);
        vm.Tick(Samples.T0.AddSeconds(2));
        vm.AllowCommand.Execute(null);
        Assert.False(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Perms.Terminal);

        var ticked = new ConsentViewModel(req, link, Samples.T0) { AllowTerminal = true };
        ticked.Tick(Samples.T0.AddSeconds(2));
        ticked.AllowCommand.Execute(null);
        Assert.True(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Perms.Terminal);

        // A person's app is not offered it, and never gets it from the dialog.
        var app = new ConsentViewModel(Samples.Request(input: true), link, Samples.T0) { AllowTerminal = true };
        Assert.False(app.OfferTerminal);
        app.Tick(Samples.T0.AddSeconds(2));
        app.AllowCommand.Execute(null);
        Assert.False(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Perms.Terminal);
    }

}
