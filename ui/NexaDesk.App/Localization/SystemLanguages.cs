using System.Runtime.InteropServices;

namespace NexaDesk.Localization;

/// <summary>
/// macOS's preferred languages (System Settings → General → Language &amp; Region), most
/// preferred first. The app is built with invariant globalization, so .NET's culture says
/// nothing; CoreFoundation does. Empty on any failure or off macOS.
/// </summary>
public static partial class SystemLanguages
{
    const string CoreFoundation = "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
    const uint Utf8 = 0x08000100; // kCFStringEncodingUTF8

    [LibraryImport(CoreFoundation)]
    private static partial nint CFLocaleCopyPreferredLanguages();

    [LibraryImport(CoreFoundation)]
    private static partial nint CFArrayGetCount(nint array);

    [LibraryImport(CoreFoundation)]
    private static partial nint CFArrayGetValueAtIndex(nint array, nint index);

    [LibraryImport(CoreFoundation)]
    [return: MarshalAs(UnmanagedType.U1)]
    private static unsafe partial bool CFStringGetCString(nint str, byte* buffer, nint size, uint encoding);

    [LibraryImport(CoreFoundation)]
    private static partial void CFRelease(nint obj);

    public static unsafe IReadOnlyList<string> Preferred()
    {
        if (!OperatingSystem.IsMacOS()) return [];
        try
        {
            var array = CFLocaleCopyPreferredLanguages();
            if (array == 0) return [];
            try
            {
                var list = new List<string>();
                var count = Math.Min(CFArrayGetCount(array), 32);
                var buffer = stackalloc byte[128];
                for (nint i = 0; i < count; i++)
                {
                    var s = CFArrayGetValueAtIndex(array, i);
                    if (s != 0 && CFStringGetCString(s, buffer, 128, Utf8))
                        list.Add(Marshal.PtrToStringUTF8((nint)buffer) ?? "");
                }
                return list;
            }
            finally
            {
                CFRelease(array);
            }
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
            return [];
        }
    }
}
