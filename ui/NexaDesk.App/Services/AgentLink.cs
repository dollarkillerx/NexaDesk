using System.IO.Pipes;
using System.Net.Sockets;
using System.Text;
using NexaDesk.Localization;
using NexaDesk.Proto;

namespace NexaDesk.Services;

/// <summary>
/// The IPC client for nexa-agent's socket (§5.3): a Unix socket, or on Windows the
/// named pipe the agent derives from the same data directory. Reads on a background
/// thread and raises everything on the UI thread. Writes are serialized and never block the UI.
/// </summary>
public sealed class AgentLink(string socketPath, IUiDispatcher ui) : IAgentLink
{
    static readonly TimeSpan HelloTimeout = TimeSpan.FromSeconds(3);

    readonly object _gate = new();
    /// <summary>One connect at a time: two at once (the attach at startup and the person
    /// clicking Start) would each install a socket, and the first would be left open with
    /// nobody reading it while the agent sends it consent prompts.</summary>
    readonly SemaphoreSlim _connecting = new(1, 1);
    Stream? _socket;
    int _generation;

    public string SocketPath { get; } = socketPath;
    public bool IsConnected { get; private set; }

    public event Action<AgentMessage>? MessageReceived;
    public event Action<string?>? Disconnected;

    /// <summary>Where the agent keeps its keys and the app its settings; the same directory
    /// the agent picks itself (crates/nexa-agent/src/os_*.rs).</summary>
    public static string DefaultDataDir() => OperatingSystem.IsWindows()
        ? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "NexaDesk")
        : Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "Library", "Application Support", "NexaDesk");

    /// <summary>The agent's pipe for a data directory: "nexadesk-" and the 64-bit FNV-1a of the
    /// directory's lower-cased UTF-8 path (ipc/transport.rs, <c>pipe_name</c>).</summary>
    public static string PipeName(string dataDir)
    {
        var h = 0xcbf29ce484222325UL;
        foreach (var b in Encoding.UTF8.GetBytes(dataDir.ToLowerInvariant()))
        {
            h ^= b;
            h *= 0x100000001b3UL;
        }
        return $"nexadesk-{h:x16}";
    }

    /// <summary>A connected stream to the agent: the socket, or the pipe on Windows.</summary>
    async Task<Stream> OpenAsync(CancellationToken ct)
    {
        if (OperatingSystem.IsWindows())
        {
            var dir = Path.GetDirectoryName(SocketPath) ?? ".";
            var pipe = new NamedPipeClientStream(".", PipeName(dir), PipeDirection.InOut, PipeOptions.Asynchronous);
            try
            {
                // No agent: no pipe, and this gives up quickly, like a missing socket does.
                await pipe.ConnectAsync(250, ct).ConfigureAwait(false);
                return pipe;
            }
            catch
            {
                pipe.Dispose();
                throw;
            }
        }
        var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        try
        {
            await socket.ConnectAsync(new UnixDomainSocketEndPoint(SocketPath), ct).ConfigureAwait(false);
            return new NetworkStream(socket, ownsSocket: true);
        }
        catch
        {
            socket.Dispose();
            throw;
        }
    }

    public static string DefaultSocketPath() => Path.Combine(DefaultDataDir(), Ipc.SocketName);

    public async Task<string?> ConnectAsync(CancellationToken ct)
    {
        try
        {
            await _connecting.WaitAsync(ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            return L.Get("Agent.Timeout");
        }
        try
        {
            return await ConnectOnceAsync(ct).ConfigureAwait(false);
        }
        finally
        {
            _connecting.Release();
        }
    }

    async Task<string?> ConnectOnceAsync(CancellationToken ct)
    {
        Close();
        int expected;
        lock (_gate) expected = _generation;
        Stream? socket = null;
        try
        {
            socket = await OpenAsync(ct).ConfigureAwait(false);
            await SendRawAsync(socket, new UiRequest.Hello(Ipc.ProtocolVersion, "NexaDesk").Encode(), ct).ConfigureAwait(false);

            var decoder = new FrameDecoder(Ipc.MaxFrame);
            using var helloCt = CancellationTokenSource.CreateLinkedTokenSource(ct);
            helloCt.CancelAfter(HelloTimeout);
            var first = await ReadOneAsync(socket, decoder, helloCt.Token).ConfigureAwait(false);
            switch (first)
            {
                case AgentMessage.Status:
                    break;
                case AgentMessage.Refused r:
                    socket.Dispose();
                    return r.Reason;
                default:
                    socket.Dispose();
                    return L.Get("Agent.NoAnswer");
            }

            int generation;
            lock (_gate)
            {
                // Closed while this was connecting: the link stays closed.
                if (_generation != expected)
                {
                    socket.Dispose();
                    return L.Get("Agent.Closed");
                }
                _socket = socket;
                generation = ++_generation;
                IsConnected = true;
            }
            ui.Post(() =>
            {
                if (generation == _generation) MessageReceived?.Invoke(first);
            });
            _ = Task.Run(() => ReadLoop(socket, decoder, generation));
            return null;
        }
        catch (Exception e) when (e is SocketException or IOException or OperationCanceledException or ProtoException
                                       or TimeoutException or UnauthorizedAccessException
                                       // A socket path longer than macOS allows (104 bytes).
                                       or ArgumentException)
        {
            socket?.Dispose();
            return e is OperationCanceledException ? L.Get("Agent.Timeout") : e.Message;
        }
    }

    static async Task SendRawAsync(Stream socket, byte[] body, CancellationToken ct)
    {
        await socket.WriteAsync(Framing.Encode(body), ct).ConfigureAwait(false);
        await socket.FlushAsync(ct).ConfigureAwait(false);
    }

    static async Task<AgentMessage> ReadOneAsync(Stream socket, FrameDecoder decoder, CancellationToken ct)
    {
        var buf = new byte[4096];
        while (true)
        {
            var frame = decoder.Next();
            if (frame != null) return AgentMessage.Decode(frame);
            var n = await socket.ReadAsync(buf, ct).ConfigureAwait(false);
            if (n == 0) throw new IOException(L.Get("Agent.Closed"));
            decoder.Push(buf.AsSpan(0, n));
        }
    }

    async Task ReadLoop(Stream socket, FrameDecoder decoder, int generation)
    {
        string? why = null;
        try
        {
            while (true)
            {
                var msg = await ReadOneAsync(socket, decoder, CancellationToken.None).ConfigureAwait(false);
                ui.Post(() =>
                {
                    if (generation == _generation) MessageReceived?.Invoke(msg);
                });
            }
        }
        catch (Exception e)
        {
            why = e is ObjectDisposedException ? null : e.Message;
        }
        Lost(socket, generation, why);
    }

    void Lost(Stream socket, int generation, string? why)
    {
        lock (_gate)
        {
            if (generation != _generation || _socket != socket) return;
            _socket = null;
        }
        socket.Dispose();
        ui.Post(() =>
        {
            IsConnected = false;
            Disconnected?.Invoke(why);
        });
    }

    public void Send(UiRequest request)
    {
        Stream? socket;
        int generation;
        lock (_gate)
        {
            socket = _socket;
            generation = _generation;
        }
        if (socket == null) return;
        var frame = Framing.Encode(request.Encode());
        try
        {
            // Small frames on a local socket: this does not block in practice. One writer at a time.
            lock (socket)
            {
                socket.Write(frame);
                socket.Flush();
            }
        }
        catch (Exception e) when (e is SocketException or IOException or ObjectDisposedException)
        {
            Lost(socket, generation, e.Message);
        }
    }

    public void Close()
    {
        Stream? socket;
        lock (_gate)
        {
            socket = _socket;
            _socket = null;
            _generation++;
            IsConnected = false;
        }
        if (socket == null) return;
        if (socket is NetworkStream ns)
        {
            try { ns.Socket.Shutdown(SocketShutdown.Both); } catch (Exception e) when (e is SocketException or ObjectDisposedException) { }
        }
        socket.Dispose();
    }
}
