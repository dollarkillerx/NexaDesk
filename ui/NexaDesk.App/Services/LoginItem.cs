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
