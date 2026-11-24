using System.ComponentModel;
using System.Diagnostics;
using System.Security.Principal;
using NexaDesk.Localization;

namespace NexaDesk.Services;

/// <summary>
/// "Run as administrator" (Windows only). Windows drops input from a program that is not
/// elevated when it would reach one that is (Task Manager, installers, an elevated console;
/// docs/WINDOWS.md), so a host that is not elevated cannot click or type into those windows.
/// Run elevated, NexaDesk, its host (nexa-agent) and so a remote command line all have
/// administrator rights. The secure desktop (the UAC prompt itself, the lock screen) is
/// still out of reach: only a service running as SYSTEM can see it.
/// </summary>
public interface IElevation
{
    /// <summary>Whether this system has the option at all.</summary>
    bool IsSupported { get; }
    /// <summary>This copy runs with administrator rights now.</summary>
    bool IsElevated { get; }
    /// <summary>
    /// Starts another copy as administrator, which asks Windows (the UAC prompt); this copy
    /// should then quit. Null once it started, else why not (the person said no, say).
    /// </summary>
    string? StartElevatedCopy();
}

public sealed class NoElevation : IElevation
{
    public bool IsSupported => false;
    public bool IsElevated => false;
    public string? StartElevatedCopy() => null;
}

[System.Runtime.Versioning.SupportedOSPlatform("windows")]
public sealed class WindowsElevation : IElevation
{
    /// <summary>Given to the elevated copy: wait for this one to quit instead of handing over to it.</summary>
    public const string RestartArg = "--elevated-restart";

    public bool IsSupported => true;

    /// <summary>A filtered (UAC) token is not in the Administrators role; an elevated one is.</summary>
    public bool IsElevated => IsProcessElevated();

    public static bool IsProcessElevated()
    {
        using var identity = WindowsIdentity.GetCurrent();
        return new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator);
    }

    public string? StartElevatedCopy() => Start([RestartArg]);

    /// <summary>Starts this program as administrator with these arguments; null once started.</summary>
    public static string? Start(IEnumerable<string> args)
    {
        var path = Environment.ProcessPath;
        if (path == null) return L.Get("Main.RunAsAdminNoPath");
        var info = new ProcessStartInfo(path) { UseShellExecute = true, Verb = "runas" };
        foreach (var a in args) info.ArgumentList.Add(a);
        try
        {
            using var started = Process.Start(info);
            return started == null ? L.Get("Main.RunAsAdminNoPath") : null;
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
    }
}
