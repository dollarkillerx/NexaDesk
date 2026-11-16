using System.Diagnostics;
using System.Runtime.InteropServices;
using NexaDesk.Localization;

namespace NexaDesk.Services;

/// <summary>
/// SMAppService.mainApp (macOS 13+) through the Objective-C runtime. Only the real
/// app creates one, and it only registers when the person switches the toggle on.
/// </summary>
public sealed partial class SmAppServiceLoginItem : ILoginItem
{
    const string ObjC = "/usr/lib/libobjc.A.dylib";
    const string Framework = "/System/Library/Frameworks/ServiceManagement.framework/ServiceManagement";

    [LibraryImport(ObjC, EntryPoint = "objc_getClass", StringMarshalling = StringMarshalling.Utf8)]
    private static partial nint GetClass(string name);

    [LibraryImport(ObjC, EntryPoint = "sel_registerName", StringMarshalling = StringMarshalling.Utf8)]
    private static partial nint Selector(string name);

    [LibraryImport(ObjC, EntryPoint = "objc_msgSend")]
    private static partial nint Send(nint receiver, nint selector);

    [LibraryImport(ObjC, EntryPoint = "objc_msgSend")]
    [return: MarshalAs(UnmanagedType.U1)]
    private static partial bool SendWithError(nint receiver, nint selector, out nint error);

    static nint MainApp()
    {
        if (!OperatingSystem.IsMacOSVersionAtLeast(13)) return 0;
        if (!NativeLibrary.TryLoad(Framework, out _)) return 0;
        var cls = GetClass("SMAppService");
        return cls == 0 ? 0 : Send(cls, Selector("mainAppService"));
    }

    static string Describe(nint error)
    {
        if (error == 0) return L.Get("LoginItem.UnknownError");
        var text = Send(error, Selector("localizedDescription"));
        var utf8 = text == 0 ? 0 : Send(text, Selector("UTF8String"));
        return utf8 == 0 ? L.Get("LoginItem.UnknownError") : Marshal.PtrToStringUTF8(utf8) ?? L.Get("LoginItem.UnknownError");
    }

    public LoginItemStatus Status
    {
        get
        {
            var app = MainApp();
            if (app == 0) return LoginItemStatus.Unsupported;
            var status = (long)Send(app, Selector("status"));
            return status is >= 0 and <= 3 ? (LoginItemStatus)status : LoginItemStatus.NotFound;
        }
    }

    string? Call(string selector)
    {
        var app = MainApp();
        if (app == 0) return L.Get("LoginItem.NeedsMacOS13");
        return SendWithError(app, Selector(selector), out var error) ? null : Describe(error);
    }

    public string? Register() => Call("registerAndReturnError:");

    public string? Unregister() => Call("unregisterAndReturnError:");

    public void OpenSettings()
    {
        try
        {
            using var _ = Process.Start(new ProcessStartInfo("/usr/bin/open",
                ["x-apple.systempreferences:com.apple.LoginItems-Settings.extension"]) { UseShellExecute = false });
        }
        catch (Exception)
        {
        }
    }
}

/// <summary>
/// "Open at login" on Windows: a value under HKCU\Software\Microsoft\Windows\CurrentVersion\Run
/// that starts this NexaDesk.exe with <see cref="BackgroundArg"/> (tray only, remote control on).
/// Settings → Apps → Startup can switch it off without removing it; that shows as needing approval.
/// </summary>
[System.Runtime.Versioning.SupportedOSPlatform("windows")]
public sealed class RunKeyLoginItem : ILoginItem
{
    public const string BackgroundArg = "--background";
    const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    const string ApprovedKey = @"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    const string ValueName = "NexaDesk";

    static string? Command() =>
        Environment.ProcessPath is { } exe ? $"\"{exe}\" {BackgroundArg}" : null;

    public LoginItemStatus Status
    {
        get
        {
            try
            {
                using var run = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(RunKey);
                if (run?.GetValue(ValueName) is not string value
                    || !string.Equals(value, Command(), StringComparison.OrdinalIgnoreCase))
                    return LoginItemStatus.NotRegistered;
                // StartupApproved: an odd first byte means switched off in Settings or Task Manager.
                using var approved = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(ApprovedKey);
                if (approved?.GetValue(ValueName) is byte[] { Length: > 0 } flags && (flags[0] & 1) == 1)
                    return LoginItemStatus.RequiresApproval;
                return LoginItemStatus.Enabled;
            }
            catch (Exception e) when (e is System.Security.SecurityException or UnauthorizedAccessException or IOException)
            {
                return LoginItemStatus.NotRegistered;
            }
        }
    }

    public string? Register()
    {
        var command = Command();
        if (command == null) return L.Get("LoginItem.UnknownError");
        try
        {
            using var run = Microsoft.Win32.Registry.CurrentUser.CreateSubKey(RunKey);
            run.SetValue(ValueName, command);
            // Switched on here means on: forget an earlier "off" from Settings.
            using var approved = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(ApprovedKey, writable: true);
            approved?.DeleteValue(ValueName, throwOnMissingValue: false);
            return null;
        }
        catch (Exception e) when (e is System.Security.SecurityException or UnauthorizedAccessException or IOException)
        {
            return e.Message;
        }
    }

    public string? Unregister()
    {
        try
        {
            Remove();
            return null;
        }
        catch (Exception e) when (e is System.Security.SecurityException or UnauthorizedAccessException or IOException)
        {
            return e.Message;
        }
    }

    /// <summary>Also run by the uninstaller (Program.Main), so nothing is left starting a removed app.</summary>
    public static void Remove()
    {
        using var run = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(RunKey, writable: true);
        run?.DeleteValue(ValueName, throwOnMissingValue: false);
        using var approved = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(ApprovedKey, writable: true);
        approved?.DeleteValue(ValueName, throwOnMissingValue: false);
    }

    public void OpenSettings()
    {
        try
        {
            using var _ = Process.Start(new ProcessStartInfo("ms-settings:startupapps") { UseShellExecute = true });
        }
        catch (Exception)
        {
        }
    }
}
