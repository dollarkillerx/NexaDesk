using Avalonia.Controls;
using Avalonia.Platform;
using NexaDesk.Services;

namespace NexaDesk.Views;

/// <summary>
/// The host's command line: a native view handed to Rust, which puts xterm.js in it
/// (a WKWebView) and wires it to a terminal stream of the session. Closing the view
/// ends the shell.
/// </summary>
public sealed class TerminalHost : NativeControlHost
{
    ulong? _id;

    public ISession? Session { get; set; }

    /// <summary>The open status code, or null if no native view was made (headless).</summary>
    public int? OpenResult { get; private set; }

    protected override IPlatformHandle CreateNativeControlCore(IPlatformHandle parent)
    {
        var handle = base.CreateNativeControlCore(parent);
        if (Session is { IsClosed: false } session && handle.HandleDescriptor == "NSView" && _id == null)
        {
            // nexa_terminal_open runs on the main thread, which this is.
            OpenResult = session.OpenTerminal(handle.Handle, out var id);
            if (OpenResult == 0) _id = id;
        }
        return handle;
    }

    protected override void DestroyNativeControlCore(IPlatformHandle control)
    {
        if (_id is { } id)
        {
            Session?.CloseTerminal(id);
            _id = null;
        }
        base.DestroyNativeControlCore(control);
    }
}
