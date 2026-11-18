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
    public void An_agents_request_says_so_and_proposes_view_and_control_only()
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

}
