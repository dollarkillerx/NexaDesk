using System.Runtime.InteropServices;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Platform;
using NexaDesk.Services;

namespace NexaDesk.Views;

/// <summary>
/// The video (§10.2): Avalonia's default native child view, handed to Rust, which
/// presents into it from its own thread. Sizes are in pixels rounded the same way
/// layout rounds, at the window's current scaling.
/// On Windows the view is a STATIC child window: static controls let the mouse through
/// to the window beneath (HTTRANSPARENT), so the session window keeps its pointer events.
/// </summary>
public sealed partial class VideoHost : NativeControlHost
{
    [LibraryImport("user32.dll", EntryPoint = "CreateWindowExW", StringMarshalling = StringMarshalling.Utf16, SetLastError = true)]
    private static partial nint CreateWindowEx(uint exStyle, string className, string? windowName, uint style,
        int x, int y, int width, int height, nint parent, nint menu, nint instance, nint param);

    [LibraryImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool DestroyWindow(nint hwnd);

    const uint WsChild = 0x40000000, WsVisible = 0x10000000, WsClipSiblings = 0x04000000;

    TopLevel? _top;
    bool _attached;

    public ISession? Session { get; set; }

    /// <summary>The attach status code, or null if no native view was created (headless).</summary>
    public int? AttachResult { get; private set; }

    public event Action<int>? AttachFailed;

    public (uint Width, uint Height, float Scale) PixelSize()
    {
        var s = (float)(TopLevel.GetTopLevel(this)?.RenderScaling ?? 1.0);
        return ((uint)Math.Round(Bounds.Width * s), (uint)Math.Round(Bounds.Height * s), s);
    }

    protected override IPlatformHandle CreateNativeControlCore(IPlatformHandle parent)
    {
        var handle = OperatingSystem.IsWindows() ? CreateWindowsChild(parent) : base.CreateNativeControlCore(parent);
        if (Session is { IsClosed: false } session && handle.HandleDescriptor is "NSView" or "HWND")
        {
            var (w, h, s) = PixelSize();
            // nexa_session_attach_view must run on the main thread, which this is.
            var rc = session.AttachView(handle.Handle, w, h, s);
            AttachResult = rc;
            _attached = rc == 0;
            if (!_attached) AttachFailed?.Invoke(rc);
        }
        return handle;
    }

    IPlatformHandle CreateWindowsChild(IPlatformHandle parent)
    {
        var (w, h, _) = PixelSize();
        var hwnd = CreateWindowEx(0, "STATIC", null, WsChild | WsVisible | WsClipSiblings,
            0, 0, Math.Max(1, (int)w), Math.Max(1, (int)h), parent.Handle, 0, 0, 0);
        return hwnd == 0 ? base.CreateNativeControlCore(parent) : new PlatformHandle(hwnd, "HWND");
    }

    protected override void DestroyNativeControlCore(IPlatformHandle control)
    {
        if (_attached)
        {
            Session?.DetachView();
            _attached = false;
        }
        if (OperatingSystem.IsWindows() && control.HandleDescriptor == "HWND")
        {
            DestroyWindow(control.Handle);
            return;
        }
        base.DestroyNativeControlCore(control);
    }

    protected override void OnSizeChanged(SizeChangedEventArgs e)
    {
        base.OnSizeChanged(e);
        Resize();
    }

    protected override void OnAttachedToVisualTree(VisualTreeAttachmentEventArgs e)
    {
        base.OnAttachedToVisualTree(e);
        _top = TopLevel.GetTopLevel(this);
        if (_top != null) _top.ScalingChanged += OnScalingChanged;
    }

    protected override void OnDetachedFromVisualTree(VisualTreeAttachmentEventArgs e)
    {
        if (_top != null) _top.ScalingChanged -= OnScalingChanged;
        _top = null;
        base.OnDetachedFromVisualTree(e);
    }

    void OnScalingChanged(object? sender, EventArgs e) => Resize();

    void Resize()
    {
        if (!_attached || Session == null) return;
        var (w, h, s) = PixelSize();
        if (w > 0 && h > 0) Session.ResizeView(w, h, s);
    }
}
