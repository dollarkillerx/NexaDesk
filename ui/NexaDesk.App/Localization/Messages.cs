using NexaDesk.Proto;

namespace NexaDesk.Localization;

/// <summary>
/// What the Rust core and the agent report, in the app's language. They send a code and the
/// variable part alongside an English text; anything without a known code is shown as sent.
/// </summary>
public static class Messages
{
    public static string Warning(WarningEvent w) => w.Code switch
    {
        WarningCode.GameWssRelay => L.Get("Messages.GameWssRelay"),
        WarningCode.GameDerp => L.Get("Messages.GameDerp"),
        WarningCode.KeymapInvalid => L.Format("Messages.KeymapInvalid", w.Detail),
        WarningCode.AudioOutput => L.Get("Messages.AudioOutput"),
        WarningCode.RememberHost => L.Format("Messages.RememberHost", w.Detail),
        WarningCode.AudioPlayback => L.Format("Messages.AudioPlayback", w.Detail),
        WarningCode.PointerLock => L.Format("Messages.PointerLock", w.Detail),
        _ => w.Text,
    };

    /// <summary>The clipboard status line's text (it is prefixed with "Clipboard:" by the toolbar).</summary>
    public static string Clipboard(ClipboardNoticeEvent n) => n.Code switch
    {
        ClipboardCode.SensitiveKeptLocal => L.Get("Messages.ClipSensitive"),
        ClipboardCode.TooLarge => L.Format("Messages.ClipTooLarge", ClipKind(n.Kind), n.Size / 1048576.0),
        ClipboardCode.PeerRejected => L.Get("Messages.ClipPeerRejected"),
        ClipboardCode.WriteFailed => L.Get("Messages.ClipWriteFailed"),
        ClipboardCode.FetchFailed => L.Format("Messages.ClipFetchFailed", ClipKind(n.Kind)),
        ClipboardCode.Unavailable => L.Format("Messages.ClipUnavailable", n.Detail),
        _ => n.Text,
    };

    static string ClipKind(string kind) => kind switch
    {
        "text" or "html" or "rtf" or "png" => L.Get($"Messages.ClipKind.{kind}"),
        _ => kind,
    };

    public static string Notice(AgentMessage.Notice n) =>
        n.Kind == NoticeKind.ConnectionFailed && n.Remote.Length > 0
            ? L.Format("Messages.ConnectionFailed", n.Remote, n.Reason)
            : n.Text;

    /// <summary>Why the keyboard settings show the built-in rules, or empty.</summary>
    public static string KeymapError(KeymapView v) =>
        v.FileError.Length > 0 ? L.Format("Messages.KeymapUnusable", v.FileError) : v.Error;
}
