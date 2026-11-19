using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>"AI agents (MCP)": the command and the JSON that hand nexa-mcp to an AI app.</summary>
public class McpTests
{
    const string MacPath = "/Applications/NexaDesk.app/Contents/MacOS/nexa-mcp";

    [Fact]
    public void The_command_and_the_json_name_the_server_next_to_the_app()
    {
        var vm = new McpViewModel(new RecordingWindows(), MacPath, true);
        Assert.True(vm.IsAvailable);
        Assert.Equal($"claude mcp add nexadesk -- \"{MacPath}\"", vm.ClaudeCommand.Replace("\\\"", "\""));
        Assert.Equal("{ \"mcpServers\": { \"nexadesk\": { \"command\": \"" + MacPath + "\" } } }", vm.JsonConfig);
        // Codex: its own table, and the line that lets it act without asking left for the person to choose.
        var codex = vm.CodexConfig.Split('\n');
        Assert.Equal("[mcp_servers.nexadesk]", codex[0]);
        Assert.Equal($"command = \"{MacPath}\"", codex[1]);
        Assert.Equal("# default_tools_approval_mode = \"approve\"", codex[^1]);
    }

    [Fact]
    public void Odd_paths_stay_one_word_and_valid_json()
    {
        Assert.Equal("\"/Users/a b/$x \\\"q\\\"/nexa-mcp\"", McpViewModel.Quote("/Users/a b/$x \"q\"/nexa-mcp", windows: false)
            .Replace("\\$", "$"));
        Assert.Equal("\"C:\\Users\\me\\AppData\\Local\\NexaDesk\\current\\nexa-mcp.exe\"",
            McpViewModel.Quote(@"C:\Users\me\AppData\Local\NexaDesk\current\nexa-mcp.exe", windows: true));
        var json = McpViewModel.JsonString("C:\\a \"b\"\n");
        Assert.Equal("C:\\a \"b\"\n", System.Text.Json.JsonDocument.Parse(json).RootElement.GetString());
    }

    [Fact]
    public async Task Copying_puts_it_on_the_clipboard_and_says_so()
    {
        var windows = new RecordingWindows();
        var vm = new McpViewModel(windows, MacPath, true);
        var copying = vm.CopyCommand.ExecuteAsync(vm.ClaudeCommand);
        Assert.Equal(vm.ClaudeCommand, windows.Copied[^1]);
        Assert.True(vm.ClaudeCopied);
        Assert.False(vm.JsonCopied);
        await copying;
        Assert.False(vm.ClaudeCopied);
    }

    [Fact]
    public void The_switch_serves_ai_apps_and_is_kept()
    {
        var (server, settings) = (new FakeAgentServer(), new MemorySettings());
        var vm = new McpViewModel(new RecordingWindows(), MacPath, true, server, settings, "Studio Mac");
        Assert.False(vm.Serving);
        Assert.False(server.IsServing);

        vm.Serving = true;
        Assert.True(server.IsServing);
        Assert.Equal("Studio Mac", server.StartedAs);
        Assert.True(settings.Current.AiApps);

        // Opened again: it serves at once, as it was left.
        var again = new FakeAgentServer();
        var reopened = new McpViewModel(new RecordingWindows(), MacPath, true, again, settings, "Studio Mac");
        Assert.True(reopened.Serving);
        Assert.True(again.IsServing);

        vm.Serving = false;
        Assert.False(server.IsServing);
        Assert.False(settings.Current.AiApps);
    }

    [Fact]
    public void A_server_that_cannot_start_says_why_and_stays_off()
    {
        var server = new FakeAgentServer { Refuse = "another NexaDesk already serves AI agents" };
        var settings = new MemorySettings();
        var vm = new McpViewModel(new RecordingWindows(), MacPath, true, server, settings);
        vm.Serving = true;
        Assert.False(vm.Serving);
        Assert.Contains("another NexaDesk already serves AI agents", vm.StartError);
        Assert.False(settings.Current.AiApps);
    }

    [Fact]
    public async Task Refused_at_launch_it_tries_again_and_serves_once_the_old_app_has_quit()
    {
        // "Restart to update": the old app still serves for a moment, so the new one is refused at first.
        var server = new FakeAgentServer { Refuse = "another NexaDesk already serves AI agents" };
        var settings = new MemorySettings(new AppSettings(AiApps: true));
        var vm = new McpViewModel(new RecordingWindows(), MacPath, true, server, settings, "Studio Mac",
            retryAfter: TimeSpan.FromMilliseconds(20));
        Assert.False(vm.Serving);
        Assert.NotNull(vm.StartError);
        server.Refuse = null;
        await Until(() => vm.Serving);
        Assert.True(server.IsServing);
        Assert.Null(vm.StartError);
        Assert.True(settings.Current.AiApps);
    }

    [Fact]
    public async Task At_launch_it_gives_up_after_a_few_tries_and_the_switch_stays_on()
    {
        var server = new FakeAgentServer { Refuse = "the socket path is too long" };
        var settings = new MemorySettings(new AppSettings(AiApps: true));
        var vm = new McpViewModel(new RecordingWindows(), MacPath, true, server, settings,
            retryAfter: TimeSpan.FromMilliseconds(5));
        await Until(() => server.Starts == 4);
        await Task.Delay(200);
        Assert.Equal(4, server.Starts);
        Assert.False(vm.Serving);
        Assert.Contains("the socket path is too long", vm.StartError);
        // Kept on, so it serves at the next launch once the cause is gone.
        Assert.True(settings.Current.AiApps);
    }

    [Fact]
    public async Task The_persons_own_flip_ends_the_retries()
    {
        var server = new FakeAgentServer { Refuse = "another NexaDesk already serves AI agents" };
        var settings = new MemorySettings(new AppSettings(AiApps: true));
        var vm = new McpViewModel(new RecordingWindows(), MacPath, true, server, settings,
            retryAfter: TimeSpan.FromMilliseconds(50));
        // They try it themselves (still refused), then the cause goes away: no retry starts it behind them.
        vm.Serving = true;
        Assert.False(vm.Serving);
        server.Refuse = null;
        await Task.Delay(500);
        Assert.False(vm.Serving);
        Assert.Equal(2, server.Starts);
    }

    static async Task Until(Func<bool> done)
    {
        for (var i = 0; i < 200 && !done(); i++) await Task.Delay(10);
        Assert.True(done());
    }

    [Fact]
    public void What_ai_agents_operate_is_shown_and_can_be_stopped()
    {
        var server = new FakeAgentServer();
        var vm = new McpViewModel(new RecordingWindows(), MacPath, true, server, new MemorySettings());
        vm.Serving = true;
        Assert.False(vm.HasSessions);
        server.Show(new AgentSessionInfo("claude-code", "MacBook"), new AgentSessionInfo("", "PC"));
        Assert.True(vm.HasSessions);
        Assert.Equal(2, vm.Sessions.Count);
        Assert.Contains("claude-code", vm.Sessions[0]);
        Assert.Contains("MacBook", vm.Sessions[0]);
        vm.StopSessionsCommand.Execute(null);
        Assert.Equal(1, server.SessionsStopped);
        Assert.False(vm.HasSessions);
    }
}
