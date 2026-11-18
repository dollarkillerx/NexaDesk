using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

namespace NexaDesk.ViewModels;

/// <summary>
/// "AI agents (MCP)": how to give an AI on this computer (Claude Code, Claude Desktop…) NexaDesk's MCP
/// server, nexa-mcp, which ships next to the app (docs/MCP.md). The command and the JSON name its full
/// path, ready to copy. Letting agents control this computer is the separate switch beside it.
/// </summary>
public sealed partial class McpViewModel : ObservableObject
{
    readonly IWindowService _windows;

    /// <param name="path">Where nexa-mcp is: next to the app's own executable.</param>
    /// <param name="exists">Whether a file is there (a development build may not have it).</param>
    public McpViewModel(IWindowService windows, string path, bool exists)
    {
        _windows = windows;
        Path = path;
        IsAvailable = exists;
    }

    /// <summary>Next to the running app: Contents/MacOS/nexa-mcp in the bundle, nexa-mcp.exe on Windows.</summary>
    public static McpViewModel ForThisApp(IWindowService windows)
    {
        var path = System.IO.Path.Combine(AppContext.BaseDirectory,
            OperatingSystem.IsWindows() ? "nexa-mcp.exe" : "nexa-mcp");
        return new McpViewModel(windows, path, File.Exists(path));
    }

    public string Path { get; }
    public bool IsAvailable { get; }
    public bool IsMissing => !IsAvailable;

    /// <summary>For Claude Code: <c>claude mcp add nexadesk -- "path"</c>.</summary>
    public string ClaudeCommand => $"claude mcp add nexadesk -- {Quote(Path)}";

    /// <summary>For apps configured with a JSON file (Claude Desktop and others).</summary>
    public string JsonConfig =>
        "{ \"mcpServers\": { \"nexadesk\": { \"command\": " + JsonString(Path) + " } } }";

    /// <summary>The text just copied, for two seconds, so its button can say so.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ClaudeCopied), nameof(JsonCopied))]
    private string? _copied;

    public bool ClaudeCopied => Copied == ClaudeCommand;
    public bool JsonCopied => Copied == JsonConfig;
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
