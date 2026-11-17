using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using NexaDesk.Terminal;
using static NexaDesk.Native.NativeMethods;

namespace NexaDesk.Native;

/// <summary>
/// The host's command line over one terminal stream of a session (nexa_terminal_*), for
/// MySSH's <see cref="TerminalView"/>. Output arrives on the session's threads, which the
/// view allows. Disposing closes the terminal: its shell ends, and once
/// nexa_terminal_close returns no callback reaches this object again.
/// </summary>
public sealed unsafe class NativeTerminal : ITerminalBackend
{
    readonly ulong _id;
    GCHandle _self;
    int _closed;

    public event Action<ReadOnlyMemory<byte>>? DataReceived;
    public event Action<TerminalExit>? Exited;

    NativeTerminal(ulong id) => _id = id;

    /// <summary>Opens a terminal of <paramref name="cols"/> × <paramref name="rows"/> cells on the session.</summary>
    internal static (NativeTerminal? Terminal, int Status) Start(NexaSession* session, int cols, int rows)
    {
        // The handle goes to Rust before the id is known; the object is filled in after.
        var box = new StrongBox<NativeTerminal?>();
        var handle = GCHandle.Alloc(box);
        ulong id = 0;
        var rc = nexa_terminal_start(session, (uint)Math.Max(cols, 2), (uint)Math.Max(rows, 1), &OnOutput,
            (void*)GCHandle.ToIntPtr(handle), &id);
        if (rc != NEXA_OK)
        {
            handle.Free();
            return (null, rc);
        }
        var terminal = new NativeTerminal(id) { _self = handle };
        box.Value = terminal;
        return (terminal, NEXA_OK);
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    static void OnOutput(void* user, uint kind, byte* data, nuint len)
    {
        try
        {
            if (GCHandle.FromIntPtr((nint)user).Target is not StrongBox<NativeTerminal?> { Value: { } t }) return;
            var bytes = new ReadOnlySpan<byte>(data, (int)len);
            if (kind == NEXA_TERMINAL_DATA) t.DataReceived?.Invoke(bytes.ToArray());
            else t.Exited?.Invoke(new TerminalExit(null, false, Encoding.UTF8.GetString(bytes)));
        }
        catch (Exception)
        {
            // Never let an exception cross into Rust.
        }
    }

    public ValueTask WriteAsync(ReadOnlyMemory<byte> data, CancellationToken ct = default)
    {
        if (Volatile.Read(ref _closed) == 0 && !data.IsEmpty)
        {
            fixed (byte* p = data.Span)
            {
                nexa_terminal_write(_id, p, (nuint)data.Length);
            }
        }
        return ValueTask.CompletedTask;
    }

    public void Resize(int columns, int rows)
    {
        if (Volatile.Read(ref _closed) == 0) nexa_terminal_resize(_id, (uint)Math.Max(columns, 2), (uint)Math.Max(rows, 1));
    }

    public ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _closed, 1) == 0)
        {
            nexa_terminal_close(_id);
            if (_self.IsAllocated) _self.Free();
        }
        return ValueTask.CompletedTask;
    }
}
