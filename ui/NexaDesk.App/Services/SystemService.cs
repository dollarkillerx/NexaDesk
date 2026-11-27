using System.ComponentModel;
using System.Diagnostics;
using System.IO.Pipes;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using NexaDesk.Localization;

namespace NexaDesk.Services;

/// <summary>Where the NexaDesk service stands on this computer.</summary>
public enum SystemServiceState
{
    NotInstalled,
    /// <summary>Installed and answering: the host is started through it.</summary>
    Running,
    /// <summary>Installed, but not answering on its pipe.</summary>
    Stopped,
    /// <summary>Installed from another version of NexaDesk: not used until updated.</summary>
    NeedsUpdate,
}

/// <summary>
/// The NexaDesk service (Windows, docs/WINDOWS_SERVICE.md §6): installed, it starts the
/// host as SYSTEM in this session, so whoever controls this computer also sees and uses
/// UAC prompts and the lock screen. Installing and removing it ask Windows for
/// administrator rights (the UAC prompt).
/// </summary>
public interface ISystemService
{
    /// <summary>Windows, with nexa-service.exe beside the app.</summary>
    bool IsSupported { get; }
    /// <summary>Looks at the installed files and asks the service; takes a moment.</summary>
    SystemServiceState Check();
    /// <summary>Installs or updates it (the UAC prompt). Null once done, else why not.</summary>
    Task<string?> Install();
    /// <summary>Removes it (the UAC prompt). Null once done, else why not.</summary>
    Task<string?> Uninstall();
    /// <summary>A connection to the service's pipe, or null when it does not answer.</summary>
    Stream? Connect(int timeoutMs);
}

public sealed class NoSystemService : ISystemService
{
    public bool IsSupported => false;
    public SystemServiceState Check() => SystemServiceState.NotInstalled;
    public Task<string?> Install() => Task.FromResult<string?>(null);
    public Task<string?> Uninstall() => Task.FromResult<string?>(null);
    public Stream? Connect(int timeoutMs) => null;
}

/// <summary>An event from the service (crates/nexa-service/src/proto.rs).</summary>
public sealed record ServiceEvent(string Kind, string? Text = null, long Number = 0);

/// <summary>
/// What the app and the service say on the pipe: one JSON value per line
/// (crates/nexa-service/src/proto.rs). Written and read by hand: the app is compiled ahead
/// of time, where reflection-based JSON is not available.
/// </summary>
public static class ServiceProtocol
{
    public const string PipeName = "nexadesk-service";

    public static string Hello() => "\"hello\"\n";

    public static string Start(IReadOnlyList<string> args, string? signalSecret)
    {
        using var buffer = new MemoryStream();
        using (var w = new Utf8JsonWriter(buffer))
        {
            w.WriteStartObject();
            w.WriteStartObject("start");
            w.WriteStartArray("args");
            foreach (var a in args) w.WriteStringValue(a);
            w.WriteEndArray();
            if (!string.IsNullOrEmpty(signalSecret)) w.WriteString("signal_secret", signalSecret);
            w.WriteEndObject();
            w.WriteEndObject();
        }
        return Encoding.UTF8.GetString(buffer.ToArray()) + "\n";
    }

    /// <summary>"started" (Number: pid), "line" (Text), "exited" (Number: code), "hello"
    /// (Number: protocol), "done", "error" (Text); "unknown" for anything else.</summary>
    public static ServiceEvent Parse(string line)
    {
        try
        {
            using var doc = JsonDocument.Parse(line);
            var root = doc.RootElement;
            if (root.ValueKind == JsonValueKind.String) return new ServiceEvent(root.GetString() ?? "unknown");
            if (root.ValueKind != JsonValueKind.Object) return new ServiceEvent("unknown");
            foreach (var p in root.EnumerateObject())
            {
                switch (p.Name)
                {
                    case "line" or "error" when p.Value.ValueKind == JsonValueKind.String:
                        return new ServiceEvent(p.Name, p.Value.GetString());
                    case "started" when p.Value.TryGetProperty("pid", out var pid):
                        return new ServiceEvent("started", Number: pid.GetInt64());
                    case "exited" when p.Value.TryGetProperty("code", out var code):
                        return new ServiceEvent("exited", Number: code.GetInt64());
                    case "hello" when p.Value.TryGetProperty("protocol", out var protocol):
                        return new ServiceEvent("hello", Number: protocol.GetInt64());
                }
            }
        }
        catch (Exception e) when (e is JsonException or InvalidOperationException or FormatException)
        {
        }
        return new ServiceEvent("unknown");
    }
}

[System.Runtime.Versioning.SupportedOSPlatform("windows")]
public sealed class WindowsSystemService : ISystemService
{
    static readonly string[] Files = ["nexa-service.exe", "nexa-agent.exe"];

    /// <summary>Where install puts it: only administrators can change files there.</summary>
    public static string InstallDir => Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "NexaDesk Service");

    static string Mine(string file) => Path.Combine(AppContext.BaseDirectory, file);

    public bool IsSupported => File.Exists(Mine("nexa-service.exe"));

    readonly object _gate = new();
    bool? _sameFiles;

    public SystemServiceState Check()
    {
        if (!File.Exists(Path.Combine(InstallDir, "nexa-service.exe"))) return SystemServiceState.NotInstalled;
        bool same;
        lock (_gate) same = _sameFiles ??= Files.All(f => SameFile(Mine(f), Path.Combine(InstallDir, f)));
        if (!same) return SystemServiceState.NeedsUpdate;
        return Hello() ? SystemServiceState.Running : SystemServiceState.Stopped;
    }

    static bool SameFile(string a, string b)
    {
        try
        {
            if (new FileInfo(a).Length != new FileInfo(b).Length) return false;
            using var fa = File.OpenRead(a);
            using var fb = File.OpenRead(b);
            return SHA256.HashData(fa).AsSpan().SequenceEqual(SHA256.HashData(fb));
        }
        catch (IOException) { return false; }
        catch (UnauthorizedAccessException) { return false; }
    }

    bool Hello()
    {
        using var pipe = Connect(300);
        if (pipe == null) return false;
        try
        {
            var hello = Encoding.UTF8.GetBytes(ServiceProtocol.Hello());
            pipe.Write(hello);
            pipe.Flush();
            using var reader = new StreamReader(pipe, new UTF8Encoding(false));
            var read = reader.ReadLineAsync();
            return read.Wait(TimeSpan.FromSeconds(2)) && read.Result is { } line
                && ServiceProtocol.Parse(line).Kind == "hello";
        }
        catch (Exception e) when (e is IOException or AggregateException or ObjectDisposedException)
        {
            return false;
        }
    }

    public Stream? Connect(int timeoutMs)
    {
        var pipe = new NamedPipeClientStream(".", ServiceProtocol.PipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
        try
        {
            pipe.Connect(timeoutMs);
            return pipe;
        }
        catch (Exception e) when (e is TimeoutException or IOException or UnauthorizedAccessException)
        {
            pipe.Dispose();
            return null;
        }
    }

    public Task<string?> Install() => RunElevated("install");

    public Task<string?> Uninstall() => RunElevated("uninstall");

    /// <summary>Runs the app's own nexa-service.exe as administrator, hidden, and waits.</summary>
    async Task<string?> RunElevated(string command)
    {
        lock (_gate) _sameFiles = null;
        var info = new ProcessStartInfo(Mine("nexa-service.exe"))
        {
            UseShellExecute = true,
            Verb = "runas",
            WindowStyle = ProcessWindowStyle.Hidden,
        };
        info.ArgumentList.Add(command);
        try
        {
            using var p = Process.Start(info);
            if (p == null) return L.Format("Main.SystemServiceFailed", -1);
            await p.WaitForExitAsync().ConfigureAwait(false);
            return p.ExitCode == 0 ? null : L.Format("Main.SystemServiceFailed", p.ExitCode);
        }
        // ERROR_CANCELLED: the person chose No on the UAC prompt.
        catch (Win32Exception e) when (e.NativeErrorCode == 1223)
        {
            return L.Get("Main.RunAsAdminDeclined");
        }
        catch (Win32Exception e)
        {
            return e.Message;
        }
        finally
        {
            lock (_gate) _sameFiles = null;
        }
    }

    /// <summary>
    /// Uninstalling NexaDesk: remove the service too, which asks Windows (the UAC prompt).
    /// Waits a little only; a no leaves it installed, and it can be removed later with
    /// "nexa-service uninstall" in its folder.
    /// </summary>
    public static void RemoveOnUninstall()
    {
        if (!File.Exists(Path.Combine(InstallDir, "nexa-service.exe")) || !File.Exists(Mine("nexa-service.exe"))) return;
        try
        {
            var info = new ProcessStartInfo(Mine("nexa-service.exe"))
            {
                UseShellExecute = true,
                Verb = "runas",
                WindowStyle = ProcessWindowStyle.Hidden,
            };
            info.ArgumentList.Add("uninstall");
            using var p = Process.Start(info);
            p?.WaitForExit(TimeSpan.FromSeconds(25));
        }
        catch (Win32Exception)
        {
        }
    }
}
