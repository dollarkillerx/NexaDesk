using System.Diagnostics;
using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using Avalonia.Threading;
using NexaDesk.Localization;

namespace NexaDesk.Services;

public sealed class AvaloniaDispatcher : IUiDispatcher
{
    public void Post(Action action) => Dispatcher.UIThread.Post(action);
}

/// <summary>
/// Starts nexa-agent from next to the app (Contents/MacOS/nexa-agent, or nexa-agent.exe
/// in the install folder on Windows), with consent coming from this UI over the socket.
/// Its stderr is kept for error messages, and written to agent.log (<see cref="AgentLog.DefaultPath"/>)
/// so a session's end can be looked up later.
/// </summary>
public sealed class AgentProcess(IUiDispatcher ui) : IAgentProcess
{
    readonly object _gate = new();
    readonly Queue<string> _lines = new();
    readonly AgentLog _log = new(AgentLog.DefaultPath());
    Process? _process;

    public bool IsRunning
    {
        get
        {
            lock (_gate) return _process is { HasExited: false };
        }
    }

    public event Action<AgentExit>? Exited;

    /// <summary>Next to the app inside the bundle, or a cargo build when running from the repository.</summary>
    public static string? Locate()
    {
        var here = AppContext.BaseDirectory;
        var exe = OperatingSystem.IsWindows() ? "nexa-agent.exe" : "nexa-agent";
        var bundled = Path.Combine(here, exe);
        if (File.Exists(bundled)) return bundled;
        var dir = new DirectoryInfo(here);
        for (var i = 0; i < 8 && dir != null; i++, dir = dir.Parent)
        {
            foreach (var profile in new[] { "release", "debug" })
            {
                var candidate = Path.Combine(dir.FullName, "target", profile, exe);
                if (File.Exists(candidate) && File.Exists(Path.Combine(dir.FullName, "Cargo.toml")))
                    return candidate;
            }
        }
        return null;
    }

    public Task<string?> SetPassword(string? password) =>
        RunSubcommand(password == null ? "clear-password" : "set-password", password);

    public Task<string?> ResetDeviceId() => RunSubcommand("reset-device-id", null);

    /// <summary>
    /// How the agent is run: without a console window, and with its pipes in UTF-8, which
    /// is what it reads and writes. Windows would otherwise use the console code page (GBK
    /// on Chinese Windows): agent.log came out garbled, and a password with letters outside
    /// ASCII reached the agent as bytes that are not UTF-8. No byte order mark on stdin.
    /// </summary>
    /// <summary>Where the agent finds the signaling server's access key.</summary>
    internal const string SignalSecretVariable = "NEXA_SIGNAL_SECRET";

    internal static ProcessStartInfo StartInfo(string path)
    {
        var utf8 = new UTF8Encoding(encoderShouldEmitUTF8Identifier: false);
        return new ProcessStartInfo(path)
        {
            UseShellExecute = false,
            RedirectStandardError = true,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            StandardErrorEncoding = utf8,
            StandardInputEncoding = utf8,
            StandardOutputEncoding = utf8,
            // A console program: without this Windows opens a console window for it.
            CreateNoWindow = true,
        };
    }

    /// <summary>Runs one of the agent's subcommands; null on success, else its error.</summary>
    async Task<string?> RunSubcommand(string command, string? stdin)
    {
        var path = Locate();
        if (path == null) return L.Get("Platform.AgentMissing");
        var info = StartInfo(path);
        info.ArgumentList.Add(command);
        try
        {
            using var p = Process.Start(info);
            if (p == null) return L.Get("Platform.AgentMissing");
            // On stdin, never on the command line where other processes could read it.
            if (stdin != null) await p.StandardInput.WriteLineAsync(stdin);
            p.StandardInput.Close();
            var errors = p.StandardError.ReadToEndAsync();
            using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
            await p.WaitForExitAsync(timeout.Token);
            if (p.ExitCode == 0) return null;
            var message = (await errors).Trim();
            return message.StartsWith("nexa-agent: ") ? message["nexa-agent: ".Length..] : message;
        }
        catch (Exception e)
        {
            return e.Message;
        }
    }

    public string? Start(string listen, string deviceName, string signalServer, bool announceOnLan, string? web = null,
        string? signalSecret = null)
    {
        var path = Locate();
        if (path == null) return L.Get("Platform.AgentMissing");
        var info = StartInfo(path);
        // Not on the command line, where any process could read it; and never one left over
        // from the environment this app itself was started in.
        info.Environment.Remove(SignalSecretVariable);
        if (!string.IsNullOrEmpty(signalServer) && !string.IsNullOrEmpty(signalSecret))
            info.Environment[SignalSecretVariable] = signalSecret;
        info.ArgumentList.Add("--listen");
        info.ArgumentList.Add(listen);
        info.ArgumentList.Add("--name");
        info.ArgumentList.Add(deviceName);
        info.ArgumentList.Add("--consent");
        info.ArgumentList.Add("ui");
        if (!string.IsNullOrEmpty(signalServer))
        {
            info.ArgumentList.Add("--signal");
            info.ArgumentList.Add(signalServer);
        }
        if (!announceOnLan) info.ArgumentList.Add("--no-lan-discovery");
        if (!string.IsNullOrEmpty(web))
        {
            info.ArgumentList.Add("--web");
            info.ArgumentList.Add(web);
        }
        Process process;
        try
        {
            process = new Process { StartInfo = info, EnableRaisingEvents = true };
            process.ErrorDataReceived += (_, e) => Keep(e.Data);
            process.OutputDataReceived += (_, e) => Keep(e.Data);
            process.Exited += (_, _) => OnExited(process);
            process.Start();
            process.BeginErrorReadLine();
            process.BeginOutputReadLine();
        }
        catch (Exception e)
        {
            return L.Format("Platform.AgentStartFailed", e.Message);
        }
        lock (_gate)
        {
            _lines.Clear();
            _process = process;
        }
        _log.Write($"nexa-agent started (pid {process.Id})");
        return null;
    }

    void Keep(string? line)
    {
        if (string.IsNullOrWhiteSpace(line)) return;
        _log.Write(line.Trim());
        lock (_gate)
        {
            _lines.Enqueue(line.Trim());
            while (_lines.Count > 20) _lines.Dequeue();
        }
    }

    void OnExited(Process process)
    {
        string output;
        int code;
        lock (_gate)
        {
            if (_process != process) return;
            _process = null;
            output = string.Join("\n", _lines);
        }
        try { code = process.ExitCode; } catch (InvalidOperationException) { code = -1; }
        // Let the last stderr lines arrive before reporting.
        process.WaitForExit(500);
        lock (_gate) output = string.Join("\n", _lines);
        _log.Write($"nexa-agent exited with code {code}");
        process.Dispose();
        ui.Post(() => Exited?.Invoke(new AgentExit(code, output)));
    }

    public void Kill()
    {
        Process? p;
        lock (_gate) p = _process;
        try { p?.Kill(); } catch (InvalidOperationException) { }
    }
}

/// <summary>
/// The agent's output with the time it arrived. One file, moved to agent.log.1 once it
/// passes a megabyte. Never throws: a log that cannot be written is simply not written.
/// </summary>
public sealed class AgentLog(string path)
{
    const long MostBytes = 1 << 20;
    readonly object _gate = new();

    public string Path { get; } = path;

    /// <summary>~/Library/Logs/NexaDesk on macOS, %LOCALAPPDATA%\NexaDesk\Logs on Windows.</summary>
    public static string DefaultPath() => OperatingSystem.IsWindows()
        ? System.IO.Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "NexaDesk", "Logs", "agent.log")
        : System.IO.Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
            "Library", "Logs", "NexaDesk", "agent.log");

    public void Write(string line)
    {
        lock (_gate)
        {
            try
            {
                Directory.CreateDirectory(System.IO.Path.GetDirectoryName(Path)!);
                var file = new FileInfo(Path);
                if (file.Exists && file.Length > MostBytes) File.Move(Path, Path + ".1", overwrite: true);
                File.AppendAllText(Path, $"{DateTime.Now:yyyy-MM-dd HH:mm:ss.fff} {line}\n");
            }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
        }
    }
}

/// <summary>Windows asks for nothing: any program in the session may capture the screen
/// and send input (except into elevated windows, which the agent cannot reach).</summary>
public sealed class NoPermissionsNeeded : IPermissionProbe
{
    public PermissionState Check() => new(true, true);

    public void Request(PermissionKind kind)
    {
    }

    public void OpenSettings(PermissionKind kind)
    {
    }
}

/// <summary>TCC checks that never prompt, and requests that do, only when the person asks.</summary>
public sealed partial class MacPermissions : IPermissionProbe
{
    [LibraryImport("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")]
    [return: MarshalAs(UnmanagedType.U1)]
    private static partial bool CGPreflightScreenCaptureAccess();

    [LibraryImport("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")]
    [return: MarshalAs(UnmanagedType.U1)]
    private static partial bool CGRequestScreenCaptureAccess();

    [LibraryImport("/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices")]
    [return: MarshalAs(UnmanagedType.U1)]
    private static partial bool AXIsProcessTrusted();

    public PermissionState Check() => new(CGPreflightScreenCaptureAccess(), AXIsProcessTrusted());

    public void Request(PermissionKind kind)
    {
        if (kind == PermissionKind.ScreenRecording)
        {
            // Adds NexaDesk to the list in System Settings and shows the system prompt once.
            CGRequestScreenCaptureAccess();
            return;
        }
        // The same for Accessibility: adds NexaDesk to its list and shows the system prompt.
        if (!PromptAccessibility()) OpenSettings(kind);
    }

    const string CoreFoundation = "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
    const string ApplicationServices = "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices";

    [LibraryImport(ApplicationServices)]
    [return: MarshalAs(UnmanagedType.U1)]
    private static partial bool AXIsProcessTrustedWithOptions(nint options);

    [LibraryImport(CoreFoundation)]
    private static partial nint CFDictionaryCreate(nint allocator, nint[] keys, nint[] values, nint count,
        nint keyCallBacks, nint valueCallBacks);

    [LibraryImport(CoreFoundation)]
    private static partial void CFRelease(nint cf);

    /// <summary>AXIsProcessTrustedWithOptions({kAXTrustedCheckOptionPrompt: true}): macOS lists
    /// NexaDesk under Accessibility and offers to open the setting. False if it could not ask.</summary>
    static bool PromptAccessibility()
    {
        try
        {
            var cf = NativeLibrary.Load(CoreFoundation);
            var ax = NativeLibrary.Load(ApplicationServices);
            // Exported globals: the symbol is the variable's address; the key and true are
            // pointers stored there, the callback tables are passed by their own address.
            var key = Marshal.ReadIntPtr(NativeLibrary.GetExport(ax, "kAXTrustedCheckOptionPrompt"));
            var yes = Marshal.ReadIntPtr(NativeLibrary.GetExport(cf, "kCFBooleanTrue"));
            var dict = CFDictionaryCreate(0, [key], [yes], 1,
                NativeLibrary.GetExport(cf, "kCFTypeDictionaryKeyCallBacks"),
                NativeLibrary.GetExport(cf, "kCFTypeDictionaryValueCallBacks"));
            if (dict == 0) return false;
            try
            {
                AXIsProcessTrustedWithOptions(dict);
            }
            finally
            {
                CFRelease(dict);
            }
            return true;
        }
        catch (Exception)
        {
            return false;
        }
    }

    public void OpenSettings(PermissionKind kind)
    {
        var anchor = kind == PermissionKind.ScreenRecording ? "Privacy_ScreenCapture" : "Privacy_Accessibility";
        try
        {
            using var _ = Process.Start(new ProcessStartInfo("/usr/bin/open",
                [$"x-apple.systempreferences:com.apple.preference.security?{anchor}"]) { UseShellExecute = false });
        }
        catch (Exception)
        {
        }
    }
}

[JsonSerializable(typeof(AppSettings))]
[JsonSerializable(typeof(ViewerPrefs))]
[JsonSourceGenerationOptions(WriteIndented = true, PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase)]
internal sealed partial class SettingsJson : JsonSerializerContext;

/// <summary>viewer-prefs.json in the data directory. A damaged file reads as defaults.</summary>
public sealed class ViewerPrefsStore(string path) : IViewerPrefs
{
    static readonly string[] Qualities = ["auto", "clear", "balanced", "weak"];
    static readonly string[] Resolutions = ["auto", "native", "1080", "720"];

    public ViewerPrefs Load()
    {
        try
        {
            if (!File.Exists(path)) return new ViewerPrefs();
            using var f = File.OpenRead(path);
            if (f.Length > 1 << 12) return new ViewerPrefs();
            var p = JsonSerializer.Deserialize(f, SettingsJson.Default.ViewerPrefs) ?? new ViewerPrefs();
            // Only what the app itself would have written is honoured.
            return new ViewerPrefs(
                Qualities.Contains(p.Quality) ? p.Quality : "auto",
                Resolutions.Contains(p.Resolution) ? p.Resolution : "auto");
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return new ViewerPrefs();
        }
    }

    public void Save(ViewerPrefs prefs)
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            var tmp = path + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(prefs, SettingsJson.Default.ViewerPrefs));
            File.Move(tmp, path, overwrite: true);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
        }
    }
}

/// <summary>settings.json in the data directory. A damaged file reads as defaults.</summary>
public sealed class SettingsStore(string path) : ISettingsStore
{
    public AppSettings Load()
    {
        try
        {
            if (!File.Exists(path)) return new AppSettings();
            using var f = File.OpenRead(path);
            if (f.Length > 1 << 16) return new AppSettings();
            var s = JsonSerializer.Deserialize(f, SettingsJson.Default.AppSettings) ?? new AppSettings();
            // Only what the app itself would have written is honoured.
            return new AppSettings(ViewModels.Texts.NormalizeServer(s.SignalServer) ?? "", s.AnnounceOnLan,
                Localization.L.IsSupported(s.Language ?? "") ? s.Language! : "", s.AllowBrowsers, s.ResumeHosting,
                s.UseServerList, s.AllowAgents);
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return new AppSettings();
        }
    }

    public void Save(AppSettings settings)
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            var tmp = path + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(settings, SettingsJson.Default.AppSettings));
            File.Move(tmp, path, overwrite: true);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
        }
    }
}

[JsonSerializable(typeof(List<RecentDevice>))]
[JsonSourceGenerationOptions(WriteIndented = true, PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase)]
internal sealed partial class RecentJson : JsonSerializerContext;

/// <summary>recent.json in the data directory: addresses this viewer connected to.</summary>
public sealed class RecentStore(string path) : IRecentStore
{
    public IReadOnlyList<RecentDevice> Load()
    {
        try
        {
            if (!File.Exists(path)) return [];
            using var f = File.OpenRead(path);
            if (f.Length > 1 << 20) return [];
            return JsonSerializer.Deserialize(f, RecentJson.Default.ListRecentDevice) ?? [];
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return [];
        }
    }

    public void Save(IReadOnlyList<RecentDevice> devices)
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            var tmp = path + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(devices.ToList(), RecentJson.Default.ListRecentDevice));
            File.Move(tmp, path, overwrite: true);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
        }
    }
}

public sealed class LocalAddresses : ILocalAddresses
{
    public IReadOnlyList<string> Candidates()
    {
        var list = new List<(int rank, string ip)>();
        try
        {
            foreach (var nic in NetworkInterface.GetAllNetworkInterfaces())
            {
                if (nic.OperationalStatus != OperationalStatus.Up) continue;
                foreach (var a in nic.GetIPProperties().UnicastAddresses)
                {
                    if (a.Address.AddressFamily != AddressFamily.InterNetwork || IPAddress.IsLoopback(a.Address)) continue;
                    var b = a.Address.GetAddressBytes();
                    if (b[0] == 169 && b[1] == 254) continue;
                    var tailscale = b[0] == 100 && b[1] >= 64 && b[1] < 128;
                    var lan = b[0] == 10 || (b[0] == 172 && b[1] >= 16 && b[1] < 32) || (b[0] == 192 && b[1] == 168);
                    list.Add((lan ? 0 : tailscale ? 1 : 2, a.Address.ToString()));
                }
            }
        }
        catch (NetworkInformationException)
        {
        }
        var result = list.OrderBy(x => x.rank).Select(x => x.ip).Distinct().ToList();
        result.Add("127.0.0.1");
        return result;
    }
}
