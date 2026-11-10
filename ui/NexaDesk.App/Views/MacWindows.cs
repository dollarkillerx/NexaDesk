using System.Runtime.InteropServices;
using Avalonia.Controls;
using Avalonia.Platform;

namespace NexaDesk.Views;

/// <summary>NSWindow settings Avalonia has no property for. No-ops off macOS and in headless runs.</summary>
internal static partial class MacWindows
{
    const string ObjC = "/usr/lib/libobjc.A.dylib";

    [LibraryImport(ObjC, EntryPoint = "sel_registerName", StringMarshalling = StringMarshalling.Utf8)]
    private static partial nint Selector(string name);

    [LibraryImport(ObjC, EntryPoint = "objc_msgSend")]
    private static partial void SendNUInt(nint receiver, nint selector, nuint value);

    [LibraryImport(ObjC, EntryPoint = "objc_msgSend")]
    private static partial void SendNInt(nint receiver, nint selector, nint value);

    [LibraryImport(ObjC, EntryPoint = "objc_msgSend")]
    private static partial void SendBool(nint receiver, nint selector, byte value);

    const nuint CanJoinAllSpaces = 1 << 0;
    const nuint Stationary = 1 << 4;
    const nuint IgnoresCycle = 1 << 6;
    const nuint FullScreenAuxiliary = 1 << 8;
    const nint StatusWindowLevel = 25;

    static nint NsWindow(TopLevel window)
    {
        if (!OperatingSystem.IsMacOS()) return 0;
        var handle = window.TryGetPlatformHandle();
        return handle switch
        {
            IMacOSTopLevelPlatformHandle mac => mac.NSWindow,
            { HandleDescriptor: "NSWindow" } h => h.Handle,
            _ => 0,
        };
    }

    /// <summary>The HUD: clicks and scrolls go to the window below (the video).</summary>
    public static void IgnoreMouse(Window window)
    {
        var ns = NsWindow(window);
        if (ns == 0) return;
        SendBool(ns, Selector("setIgnoresMouseEvents:"), 1);
    }

    /// <summary>
    /// The banner: on every Space and over full-screen apps, above ordinary floating
    /// windows, out of the Cmd+` cycle, and not hidden by Cmd+H.
    /// </summary>
    public static void FloatOnAllSpaces(Window window)
    {
        var ns = NsWindow(window);
        if (ns == 0) return;
        SendNUInt(ns, Selector("setCollectionBehavior:"), CanJoinAllSpaces | Stationary | IgnoresCycle | FullScreenAuxiliary);
        SendNInt(ns, Selector("setLevel:"), StatusWindowLevel);
        SendBool(ns, Selector("setCanHide:"), 0);
    }
}
