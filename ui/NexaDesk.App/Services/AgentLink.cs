using System.Net.Sockets;
using NexaDesk.Localization;
using NexaDesk.Proto;

namespace NexaDesk.Services;

/// <summary>
/// The IPC client for nexa-agent's socket (§5.3). Reads on a background thread and
/// raises everything on the UI thread. Writes are serialized and never block the UI.
/// </summary>
public sealed class AgentLink(string socketPath, IUiDispatcher ui) : IAgentLink
{
    static readonly TimeSpan HelloTimeout = TimeSpan.FromSeconds(3);

    readonly object _gate = new();
    Socket? _socket;
    int _generation;

    public string SocketPath { get; } = socketPath;
    public bool IsConnected { get; private set; }

    public event Action<AgentMessage>? MessageReceived;
    public event Action<string?>? Disconnected;

    public static string DefaultDataDir() =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "Library", "Application Support", "NexaDesk");

    public static string DefaultSocketPath() => Path.Combine(DefaultDataDir(), Ipc.SocketName);

    public async Task<string?> ConnectAsync(CancellationToken ct)
    {
        Close();
        var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        try
        {
            await socket.ConnectAsync(new UnixDomainSocketEndPoint(SocketPath), ct).ConfigureAwait(false);
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
                _socket = socket;
                generation = ++_generation;
            }
            IsConnected = true;
            ui.Post(() => MessageReceived?.Invoke(first));
            _ = Task.Run(() => ReadLoop(socket, decoder, generation));
            return null;
        }
        catch (Exception e) when (e is SocketException or IOException or OperationCanceledException or ProtoException)
        {
            socket.Dispose();
            return e is OperationCanceledException ? L.Get("Agent.Timeout") : e.Message;
        }
    }

    static async Task SendRawAsync(Socket socket, byte[] body, CancellationToken ct)
    {
        var frame = Framing.Encode(body);
        var sent = 0;
        while (sent < frame.Length)
            sent += await socket.SendAsync(frame.AsMemory(sent), SocketFlags.None, ct).ConfigureAwait(false);
    }

    static async Task<AgentMessage> ReadOneAsync(Socket socket, FrameDecoder decoder, CancellationToken ct)
    {
        var buf = new byte[4096];
        while (true)
        {
            var frame = decoder.Next();
            if (frame != null) return AgentMessage.Decode(frame);
            var n = await socket.ReceiveAsync(buf, SocketFlags.None, ct).ConfigureAwait(false);
            if (n == 0) throw new IOException(L.Get("Agent.Closed"));
            decoder.Push(buf.AsSpan(0, n));
        }
    }

    async Task ReadLoop(Socket socket, FrameDecoder decoder, int generation)
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

    void Lost(Socket socket, int generation, string? why)
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
        Socket? socket;
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
            // Small frames on a local socket: this does not block in practice.
            socket.Send(frame);
        }
        catch (Exception e) when (e is SocketException or ObjectDisposedException)
        {
            Lost(socket, generation, e.Message);
        }
    }

    public void Close()
    {
        Socket? socket;
        lock (_gate)
        {
            socket = _socket;
            _socket = null;
            _generation++;
        }
        if (socket == null) return;
        IsConnected = false;
        try { socket.Shutdown(SocketShutdown.Both); } catch (SocketException) { }
        socket.Dispose();
    }
}
