using System.Runtime.InteropServices;

namespace NexaDesk.Localization;

/// <summary>
/// The system's preferred languages, most preferred first: macOS's (System Settings →
/// General → Language &amp; Region) from CoreFoundation, Windows' display languages from
/// GetUserPreferredUILanguages. The app is built with invariant globalization, so .NET's
/// culture says nothing. Empty on any failure or on other systems.
/// </summary>
public static partial class SystemLanguages
{
    const string CoreFoundation = "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
    const uint Utf8 = 0x08000100; // kCFStringEncodingUTF8

    [LibraryImport(CoreFoundation)]
    private static partial nint CFLocaleCopyPreferredLanguages();

    const uint MuiLanguageName = 0x8; // MUI_LANGUAGE_NAME: "zh-CN", "ja-JP", …

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static unsafe partial bool GetUserPreferredUILanguages(uint flags, out uint count, char* buffer, ref uint length);

    /// <summary>A double-NUL-terminated list of language names.</summary>
    static unsafe IReadOnlyList<string> WindowsPreferred()
    {
        try
        {
            uint length = 0;
            if (!GetUserPreferredUILanguages(MuiLanguageName, out _, null, ref length) || length == 0 || length > 4096) return [];
            var buffer = new char[length];
            fixed (char* p = buffer)
            {
                if (!GetUserPreferredUILanguages(MuiLanguageName, out _, p, ref length)) return [];
            }
            return new string(buffer, 0, (int)Math.Min(length, (uint)buffer.Length))
                .Split('\0', StringSplitOptions.RemoveEmptyEntries);
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
            return [];
        }
    }

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
        if (OperatingSystem.IsWindows()) return WindowsPreferred();
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
