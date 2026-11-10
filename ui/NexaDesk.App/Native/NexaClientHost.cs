using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using NexaDesk.Proto;
using NexaDesk.Services;
using static NexaDesk.Native.NativeMethods;

namespace NexaDesk.Native;

/// <summary>Turns callback bytes into events on the UI thread: copy, decode, post (§10.1).</summary>
public sealed class EventPump(IUiDispatcher ui, Action<ClientEvent> sink)
{
    public long Dropped { get; private set; }

    /// <summary>Runs on a Rust thread. The bytes are only valid during the call, so they are copied first.</summary>
    public void Receive(ReadOnlySpan<byte> bytes)
    {
        var copy = bytes.ToArray();
        ClientEvent ev;
        try
        {
            ev = ClientCodec.DecodeEvent(copy);
        }
        catch (ProtoException)
        {
            Dropped++;
            return;
        }
        ui.Post(() => sink(ev));
    }
}

public sealed class NexaException(int code, string message) : Exception($"{message} ({code})")
{
    public int Code { get; } = code;
}

internal static unsafe class NativeError
{
    /// <summary>The last error on this thread; call right after the failing call.</summary>
    public static string Last()
    {
        Span<byte> buf = stackalloc byte[512];
        nuint written = 0;
        fixed (byte* p = buf)
        {
            nexa_last_error(p, (nuint)buf.Length, &written);
        }
        var n = (int)Math.Min((ulong)written, (ulong)buf.Length);
        return Encoding.UTF8.GetString(buf[..n]);
    }
}

/// <summary>The real libnexa_client. Only <see cref="App"/> creates one; tests never do.</summary>
public sealed unsafe class NexaClientHost : IClientCore
{
    NexaClient* _client;
    GCHandle _self;
    readonly EventPump _pump;

    public event Action<ClientEvent>? EventReceived;

    NexaClientHost(IUiDispatcher ui)
    {
        _pump = new EventPump(ui, ev => EventReceived?.Invoke(ev));
    }

    public static NexaClientHost Create(ClientConfig config, IUiDispatcher ui)
    {
        var host = new NexaClientHost(ui);
        host._self = GCHandle.Alloc(host);
        var cfg = config.Encode();
        NexaClient* client = null;
        int rc;
        fixed (byte* p = cfg)
        {
            rc = nexa_init(p, (nuint)cfg.Length, &OnEvent, (void*)GCHandle.ToIntPtr(host._self), &client);
        }
        if (rc != NEXA_OK)
        {
            var message = NativeError.Last();
            host._self.Free();
            throw new NexaException(rc, message);
        }
        host._client = client;
        return host;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    static void OnEvent(void* user, byte* pb, nuint len)
    {
        // Nothing may unwind into Rust.
        try
        {
            if (user == null || pb == null || len > int.MaxValue) return;
            if (GCHandle.FromIntPtr((nint)user).Target is NexaClientHost host)
                host._pump.Receive(new ReadOnlySpan<byte>(pb, (int)len));
        }
        catch
        {
        }
    }

    public ISession Connect(ConnectRequest request)
    {
        if (_client == null) throw new ObjectDisposedException(nameof(NexaClientHost));
        var bytes = request.Encode();
        NexaSession* session = null;
        int rc;
        fixed (byte* p = bytes)
        {
            rc = nexa_connect(_client, p, (nuint)bytes.Length, &session);
        }
        if (rc != NEXA_OK) throw new NexaException(rc, NativeError.Last());
        return new NativeSession(session, request.Tag);
    }

    public void Dispose()
    {
        if (_client == null) return;
        // Returns only once no callback is running, so the handle can be freed after.
        nexa_shutdown(_client);
        _client = null;
        if (_self.IsAllocated) _self.Free();
    }
}

internal sealed unsafe class NativeSession(NexaSession* handle, ulong tag) : ISession
{
    NexaSession* _handle = handle;

    public ulong Tag { get; } = tag;
    public bool IsClosed => _handle == null;
    public string LastError { get; private set; } = "";

    int Check(int rc)
    {
        if (rc != NEXA_OK) LastError = NativeError.Last();
        return rc;
    }

    public int AttachView(nint nsView, uint widthPx, uint heightPx, float scale) =>
        _handle == null ? NEXA_E_BAD_HANDLE
            : Check(nexa_session_attach_view(_handle, (void*)nsView, NEXA_VIEW_NSVIEW, widthPx, heightPx, scale));

    public int ResizeView(uint widthPx, uint heightPx, float scale) =>
        _handle == null ? NEXA_E_BAD_HANDLE : Check(nexa_session_resize_view(_handle, widthPx, heightPx, scale));

    public int DetachView() => _handle == null ? NEXA_E_BAD_HANDLE : Check(nexa_session_detach_view(_handle));

    public int SetInputActive(bool active) =>
        _handle == null ? NEXA_E_BAD_HANDLE : Check(nexa_session_set_input_active(_handle, active));

    public int Send(SessionCommand command)
    {
        if (_handle == null) return NEXA_E_BAD_HANDLE;
        var bytes = command.Encode();
        fixed (byte* p = bytes)
        {
            return Check(nexa_session_command(_handle, p, (nuint)bytes.Length));
        }
    }

    public void Close()
    {
        if (_handle == null) return;
        nexa_session_close(_handle);
        _handle = null;
    }
}
