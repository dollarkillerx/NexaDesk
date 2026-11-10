using System.Diagnostics;
using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Serialization;
using Avalonia.Threading;

namespace NexaDesk.Services;

public sealed class AvaloniaDispatcher : IUiDispatcher
{
    public void Post(Action action) => Dispatcher.UIThread.Post(action);
}

/// <summary>
/// Starts nexa-agent from the app bundle (Contents/MacOS/nexa-agent), with consent
/// coming from this UI over the socket. Its stderr is kept for error messages.
/// </summary>
public sealed class AgentProcess(IUiDispatcher ui) : IAgentProcess
{
    readonly object _gate = new();
    readonly Queue<string> _lines = new();
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
        var bundled = Path.Combine(here, "nexa-agent");
        if (File.Exists(bundled)) return bundled;
        var dir = new DirectoryInfo(here);
        for (var i = 0; i < 8 && dir != null; i++, dir = dir.Parent)
        {
            foreach (var profile in new[] { "release", "debug" })
            {
                var candidate = Path.Combine(dir.FullName, "target", profile, "nexa-agent");
                if (File.Exists(candidate) && File.Exists(Path.Combine(dir.FullName, "Cargo.toml")))
                    return candidate;
            }
        }
        return null;
    }

    public string? Start(string listen, string deviceName, string signalServer)
    {
        var path = Locate();
        if (path == null) return "找不到 nexa-agent。请使用打包好的 NexaDesk.app。";
        var info = new ProcessStartInfo(path)
        {
            UseShellExecute = false,
            RedirectStandardError = true,
            RedirectStandardOutput = true,
            RedirectStandardInput = true,
        };
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
            return $"无法启动 nexa-agent：{e.Message}";
        }
        lock (_gate)
        {
            _lines.Clear();
            _process = process;
        }
        return null;
    }

    void Keep(string? line)
    {
        if (string.IsNullOrWhiteSpace(line)) return;
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
        // Accessibility has no request API that does not need CoreFoundation dictionaries;
        // opening the right pane is what the prompt would do anyway.
        OpenSettings(kind);
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
[JsonSourceGenerationOptions(WriteIndented = true, PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase)]
internal sealed partial class SettingsJson : JsonSerializerContext;

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
            return new AppSettings(ViewModels.Texts.NormalizeServer(s.SignalServer) ?? "");
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
