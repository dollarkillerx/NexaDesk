using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>
/// "AI agents (MCP)": AI apps on this computer (Claude Code, Codex, Claude Desktop…) operating other computers
/// through NexaDesk (docs/MCP.md §16). The switch starts NexaDesk's MCP server; the AI app runs nexa-mcp, which
/// ships next to the app and only relays to it (the command and configs name its full path, ready to copy). The AI
/// may operate only the saved devices the person allowed, and NexaDesk signs in to them itself. Letting agents
/// control this computer is the separate switch beside it.
/// </summary>
public sealed partial class McpViewModel : ObservableObject
{
    readonly IWindowService _windows;
    readonly IAgentServer _server;
    readonly ISettingsStore? _settings;
    readonly string _deviceName;

    /// <param name="path">Where nexa-mcp is: next to the app's own executable.</param>
    /// <param name="exists">Whether a file is there (a development build may not have it).</param>
    /// <param name="server">The MCP server in the core.</param>
    /// <param name="settings">Where the switch is kept (<see cref="AppSettings.AiApps"/>); it starts the server at once.</param>
    /// <param name="deviceName">What hosts are told the AI's device is called.</param>
    public McpViewModel(IWindowService windows, string path, bool exists, IAgentServer? server = null,
        ISettingsStore? settings = null, string deviceName = "")
    {
        _windows = windows;
        _server = server ?? new NoAgentServer();
        _settings = settings;
        _deviceName = deviceName;
        Path = path;
        IsAvailable = exists;
        _server.Changed += OnSessionsChanged;
        if (settings?.Load().AiApps == true) Turn(true);
    }

    /// <summary>Next to the running app: Contents/MacOS/nexa-mcp in the bundle, nexa-mcp.exe on Windows.</summary>
    public static McpViewModel ForThisApp(IWindowService windows, IAgentServer? server = null,
        ISettingsStore? settings = null, string deviceName = "")
    {
        var path = System.IO.Path.Combine(AppContext.BaseDirectory,
            OperatingSystem.IsWindows() ? "nexa-mcp.exe" : "nexa-mcp");
        return new McpViewModel(windows, path, File.Exists(path), server, settings, deviceName);
    }

    /// <summary>"Let AI apps on this computer use NexaDesk": the MCP server runs while on. Kept at once.</summary>
    [ObservableProperty]
    private bool _serving;

    /// <summary>Why the server could not start; null when it could (or is off).</summary>
    [ObservableProperty]
    private string? _startError;

    bool _turning;

    partial void OnServingChanged(bool value)
    {
        if (_turning) return;
        Turn(value);
        if (_settings != null) _settings.Save(_settings.Load() with { AiApps = Serving });
    }

    void Turn(bool on)
    {
        _turning = true;
        try
        {
            if (on)
            {
                var refused = _server.Start(_deviceName);
                StartError = refused == null ? null : L.Format("Main.AiAppsFailed", refused);
                Serving = refused == null;
            }
            else
            {
                _server.Stop();
                StartError = null;
                Serving = false;
            }
        }
        finally
        {
            _turning = false;
        }
    }

    /// <summary>What AI agents operate now, as "Claude Code → MacBook".</summary>
    public ObservableCollection<string> Sessions { get; } = [];
    public bool HasSessions => Sessions.Count > 0;

    void OnSessionsChanged()
    {
        Sessions.Clear();
        foreach (var s in _server.Sessions)
            Sessions.Add(L.Format("Main.AiAppsSession", s.Client.Length > 0 ? s.Client : L.Get("Main.AiAppsUnnamed"), s.Device));
        OnPropertyChanged(nameof(HasSessions));
    }

    /// <summary>Ends every AI agent's session at once; AI apps can connect again.</summary>
    [RelayCommand]
    private void StopSessions() => _server.StopSessions();

    public string Path { get; }
    public bool IsAvailable { get; }
    public bool IsMissing => !IsAvailable;

    /// <summary>For Claude Code: <c>claude mcp add nexadesk -- "path"</c>.</summary>
    public string ClaudeCommand => $"claude mcp add nexadesk -- {Quote(Path)}";

    /// <summary>For apps configured with a JSON file (Claude Desktop and others).</summary>
    public string JsonConfig =>
        "{ \"mcpServers\": { \"nexadesk\": { \"command\": " + JsonString(Path) + " } } }";

    /// <summary>For Codex, in its config.toml (a TOML basic string takes a JSON string's escapes). Codex asks
    /// before every tool call unless told otherwise; the line that stops it asking is there, commented out, for
    /// the person to choose.</summary>
    public string CodexConfig =>
        "[mcp_servers.nexadesk]\n" +
        "command = " + JsonString(Path) + "\n" +
        "# To let Codex act without asking you each time:\n" +
        "# default_tools_approval_mode = \"approve\"";

    /// <summary>The text just copied, for two seconds, so its button can say so.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ClaudeCopied), nameof(JsonCopied), nameof(CodexCopied))]
    private string? _copied;

    public bool ClaudeCopied => Copied == ClaudeCommand;
    public bool JsonCopied => Copied == JsonConfig;
    public bool CodexCopied => Copied == CodexConfig;
    int _copies;

    [RelayCommand]
    private async Task Copy(string? text)
    {
        if (string.IsNullOrEmpty(text)) return;
        _windows.CopyText(text);
        Copied = text;
        var mine = ++_copies;
        await Task.Delay(TimeSpan.FromSeconds(2));
        if (mine == _copies) Copied = null;
    }

    /// <summary>A path as one shell word, in double quotes. On a Mac the characters a shell still reads inside
    /// them are escaped; Windows paths cannot hold a double quote, and backslashes there are separators.</summary>
    internal static string Quote(string s, bool windows) => windows
        ? "\"" + s + "\""
        : "\"" + s.Replace("\\", "\\\\").Replace("\"", "\\\"").Replace("$", "\\$").Replace("`", "\\`") + "\"";

    string Quote(string s) => Quote(s, OperatingSystem.IsWindows());

    /// <summary>A JSON string (no reflection, for NativeAOT).</summary>
    internal static string JsonString(string s)
    {
        var b = new System.Text.StringBuilder("\"");
        foreach (var c in s)
        {
            switch (c)
            {
                case '"': b.Append("\\\""); break;
                case '\\': b.Append("\\\\"); break;
                case < ' ': b.Append($"\\u{(int)c:x4}"); break;
                default: b.Append(c); break;
            }
        }
        return b.Append('"').ToString();
    }
}
