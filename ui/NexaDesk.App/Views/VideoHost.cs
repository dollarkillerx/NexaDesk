using Avalonia;
using Avalonia.Controls;
using Avalonia.Platform;
using NexaDesk.Services;

namespace NexaDesk.Views;

/// <summary>
/// The video (§10.2): Avalonia's default native child view, handed to Rust, which
/// presents into it from its own thread. Sizes are in pixels rounded the same way
/// layout rounds, at the window's current scaling.
/// </summary>
public sealed class VideoHost : NativeControlHost
{
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
        var handle = base.CreateNativeControlCore(parent);
        if (Session is { IsClosed: false } session && handle.HandleDescriptor == "NSView")
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

    protected override void DestroyNativeControlCore(IPlatformHandle control)
    {
        if (_attached)
        {
            Session?.DetachView();
            _attached = false;
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
